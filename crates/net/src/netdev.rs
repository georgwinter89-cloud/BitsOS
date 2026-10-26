//! The list of network cards the drivers brought up (virtio-net, igc, later rtw89). Drivers register a card
//! once its `NetDevice` works; NW-3 puts a smoltcp interface on each, RB-4's mirror picks one for `poll_tx`.
//! Dynamic (R1): any number of cards, names unique per kind (`virtio-net0`, `virtio-net1`, `igc0`, ...).
//!
//! NW-4: a driver whose receive and transmit completions arrive by MSI/MSI-X says so (`set_irq_driven`) and wakes
//! the stack from its hard-IRQ hook (`irq_wake`); the stack then sleeps until a wake instead of looking at the
//! cards every 50/500 µs. While any registered card is polled (no MSI to be had, a stormed line, `net_irq=off`),
//! the stack keeps polling (`all_irq_driven` false).

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};
use devmodel::net::NetDevice;
use ksync::SpinLock;

static DEVICES: SpinLock<Vec<Arc<dyn NetDevice>>> = SpinLock::named("net.devices", Vec::new());
/// Names of the cards whose completions arrive by interrupt.
static IRQ_CARDS: SpinLock<Vec<String>> = SpinLock::named("net.irq_cards", Vec::new());
/// Every registered card is in `IRQ_CARDS` (and there is at least one): recomputed on each change.
static ALL_IRQ: AtomicBool = AtomicBool::new(false);

klog::counter!(pub IRQ_WAKES, "net.irq_wakes");

/// A driver's card is (true) or is no longer (false) interrupt-driven: its hard-IRQ hook calls `irq_wake` for
/// every receive/transmit completion. Logged on change. Thread context (takes spin locks).
pub fn set_irq_driven(name: &str, on: bool) {
    let changed = {
        let mut l = IRQ_CARDS.lock();
        match (on, l.iter().position(|n| n == name)) {
            (true, None) => {
                l.push(String::from(name));
                true
            }
            (false, Some(i)) => {
                l.swap_remove(i);
                true
            }
            _ => false,
        }
    };
    if changed {
        recompute();
        klog::event!(Info, "net", "card.irq_mode", ifname = name, irq = on, all_cards_irq = all_irq_driven());
    }
}

/// Whether card `name` said it is interrupt-driven.
pub fn is_irq_driven(name: &str) -> bool {
    IRQ_CARDS.lock().iter().any(|n| n == name)
}

/// True when every registered card is interrupt-driven: the stack may sleep until a wake (with a slow safety
/// look). False with no card or any polled card.
pub fn all_irq_driven() -> bool {
    ALL_IRQ.load(Ordering::Acquire)
}

/// Stored while both lists are locked (DEVICES, then IRQ_CARDS, the order everywhere): every list change is
/// followed by its own recompute, so the last value stored matches the lists as they are. Stored after the
/// unlock, an older "all interrupt-driven" could land last and leave a polled card unlooked-at for the 100 ms
/// safety sleep (review INT-A1d drivers 2).
fn recompute() {
    let d = DEVICES.lock();
    let irq = IRQ_CARDS.lock();
    ALL_IRQ.store(!d.is_empty() && d.iter().all(|x| irq.iter().any(|n| n == x.name())), Ordering::Release);
}

/// Called from a driver's hard-IRQ hook: wakes the stack's poll thread (lock-free, no heap, no klog; the wait
/// queue's wake is IRQ-safe since SC-3). Without the kernel's stack (host tests) it only counts.
pub fn irq_wake() {
    IRQ_WAKES.inc();
    #[cfg(target_os = "none")]
    if let Some(s) = crate::stack::kernel() {
        s.wake_poll();
    }
}

/// Adds a working card. Returns its index in the list.
pub fn register(d: Arc<dyn NetDevice>) -> usize {
    let mac = d.mac().0;
    let name = String::from(d.name());
    let n = {
        let mut l = DEVICES.lock();
        l.push(d);
        l.len() - 1
    };
    recompute();
    klog::event!(Info, "net", "device.added", ifname = name.as_str(), index = n, mac = mac_str(mac));
    n
}

pub fn mac_str(m: [u8; 6]) -> String {
    alloc::format!("{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}", m[0], m[1], m[2], m[3], m[4], m[5])
}

/// Every registered card, in registration order.
pub fn devices() -> Vec<Arc<dyn NetDevice>> {
    DEVICES.lock().clone()
}

/// How many cards are registered (cheap: no copy of the list).
pub fn count() -> usize {
    DEVICES.lock().len()
}

pub fn by_name(name: &str) -> Option<Arc<dyn NetDevice>> {
    DEVICES.lock().iter().find(|d| d.name() == name).cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The device list and the IRQ flags are global: these tests take turns.
    static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn register_and_find() {
        let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let d = Arc::new(crate::loopback::LoopDevice::new("lo-netdev-test"));
        let before = devices().len();
        let i = register(d);
        assert_eq!(i, before);
        assert_eq!(devices().len(), before + 1);
        assert!(by_name("lo-netdev-test").is_some());
        assert!(by_name("nothing").is_none());
    }

    /// NW-4: the stack polls until every registered card is interrupt-driven.
    #[test]
    fn all_irq_only_when_every_card_says_so() {
        let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let a = Arc::new(crate::loopback::LoopDevice::new("lo-irq-a"));
        register(a);
        assert!(!all_irq_driven(), "a polled card keeps the stack polling");
        set_irq_driven("lo-irq-a", true);
        // Other tests' cards in the same list are polled: mark them all to see the flag go up.
        let names: Vec<String> = devices().iter().map(|d| String::from(d.name())).collect();
        for n in &names {
            set_irq_driven(n, true);
        }
        assert!(all_irq_driven() && is_irq_driven("lo-irq-a"));
        set_irq_driven("lo-irq-a", false);
        assert!(!all_irq_driven() && !is_irq_driven("lo-irq-a"));
        for n in &names {
            set_irq_driven(n, false);
        }
        irq_wake();
        assert!(IRQ_WAKES.get() >= 1);
    }
}
