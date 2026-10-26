//! Realtek RTL8852BE (rtw89, PCI 10EC:B852; the Mini-PC's 04:00.0 behind root port 00:02.2).
//! Owner: WL. Item WL-4: the kernel side of `rtw89-core` (spec/wlan.md):
//! - attaches only with `wlan=on` in CONFIG.TXT (R8 d: the PCI core leaves the function and its
//!   bridge untouched otherwise, `opt_in_key`);
//! - `kbus::KBus`: rtw89-core's `Bus` on BAR2 (the whole 1 MB, uncached), config space and DMA
//!   memory below 4 GB (`dma::DmaPool`), wrapped in Linux's CMAC read rule;
//! - `card`: the driver thread (class KernelRt, its own: the chip is served whatever programs do),
//!   which runs rtw89-core's `drv::Runner` (bring-up, supervisor, health, track work), the queues
//!   to the network stack, the MCP commands and Quiesce (chip off before Reload/reboot);
//! - `netdev::WlanNet`: the card in the net stack (`wlan0`), link up only with the group key in;
//! - `config`: `wlan_ssid`/`wlan_psk` (a secret: PMK at once, never shown) and the scan's channels;
//! - `tool`: MCP `wlan` (status, scan, join, leave).
//!
//! Interrupts: one MSI vector (capability 0x50) wakes the thread, with a 20 ms safety poll while
//! linked; without MSI (or `net_irq=off`) the thread polls RX every 2 ms. Bus master on the
//! function only after the MAC is known off (`card::run`).
#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

pub mod card;
pub mod config;
pub mod dma;
pub mod kbus;
mod ktests;
#[cfg(test)]
mod lock_order;
pub mod netdev;
pub mod tool;

use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::Ordering;
use ksync::SpinLock;
use rtw89_core::deadread::CmacRead;

use card::{Card, Core};

pub const VENDOR_REALTEK: u16 = 0x10EC;
pub const DEV_RTL8852BE: u16 = 0xB852;
/// BAR2 is the register window (spec/wlan.md §2.1); it must be mapped whole
pub const BAR: u8 = 2;
pub const BAR_SIZE: u64 = 0x10_0000;
/// The driver thread's KernelRt priority: below MCP's and the stack's default (50), above programs
pub const RT_PRIO: u8 = 40;

static MATCHES: [pci::Match; 1] = [pci::Match::Id { vendor: VENDOR_REALTEK, device: DEV_RTL8852BE }];

pci::pci_driver!(pci::DriverEntry { name: "rtw89", matches: &MATCHES, opt_in_key: Some("wlan"), probe });

static CARDS: SpinLock<Vec<Arc<Card>>> = SpinLock::named("rtw89.cards", Vec::new());

/// Every card this driver attached
pub fn cards() -> Vec<Arc<Card>> {
    CARDS.lock().clone()
}

fn probe(f: &pci::PciFunction) -> Result<(), String> {
    let function = format!("{}", f.addr);
    // MSI and bus master off before anything (PC-2): a chip a killed run left on must DMA nowhere.
    // The thread switches memory decoding on and powers a MAC that is still on off, only then bus
    // master on the bridges (and MSI); on the function itself only rtw89-core's safety step sets it.
    let _ = f.disable_device();
    klog::event!(Info, "wlan", "probe", function = function.as_str(), device = klog::Hex(f.device as u64));
    let fail = |e: String| {
        klog::event!(Error, "wlan", "probe_failed", function = function.as_str(), error = e.as_str());
        e
    };
    let Some(pci::Bar::Mem { phys, size, .. }) = f.bar(BAR) else {
        return Err(fail(String::from("BAR2 is not a memory BAR")));
    };
    if size < BAR_SIZE {
        return Err(fail(format!("BAR2 is {size:#x} bytes, the register window needs {BAR_SIZE:#x}")));
    }
    let io = hal::mmio::map_device(phys, BAR_SIZE as usize, hal::paging::CacheType::Uncached)
        .map_err(|e| fail(format!("map BAR2 {phys:#x}+{BAR_SIZE:#x}: {e:?}")))?;
    let index = CARDS.lock().len();
    let bus = CmacRead::new(kbus::KBus::new(io, f.clone()));
    let card = Arc::new(Card::new(
        format!("wlan{index}"),
        function.clone(),
        Some(f.clone()),
        Some(Core { bus, runner: None }),
    ));
    devmodel::register_quiesce(card.clone());
    CARDS.lock().push(card.clone());
    let opts = sched::SpawnOpts { rt_prio: RT_PRIO, ..sched::SpawnOpts::default() };
    let c = card.clone();
    match sched::spawn_with("rtw89", sched::Class::KernelRt, opts, move || card::run(c)) {
        Ok(_) => {
            klog::event!(Info, "wlan", "thread", ifname = card.name.as_str(), class = "kernel_rt", rt_prio = RT_PRIO);
            Ok(())
        }
        Err(e) => Err(fail(format!("driver thread: {e:?}"))),
    }
}

/// The card joins the stack's device list once its MAC is known (after its first bring-up)
pub fn register_netdev(card: &Card) {
    if card.registered.swap(true, Ordering::AcqRel) {
        return;
    }
    let Some(c) = cards().into_iter().find(|c| core::ptr::eq(c.as_ref(), card)) else { return };
    net::netdev::register(Arc::new(netdev::WlanNet(c)));
    // receive comes by the thread's wake (net::netdev::irq_wake), never by the stack polling us
    net::netdev::set_irq_driven(&card.name, true);
    klog::event!(Info, "wlan", "netdev.registered", ifname = card.name.as_str());
}
