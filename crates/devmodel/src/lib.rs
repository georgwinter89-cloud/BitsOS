//! The hardware abstraction (V2-PLAN §3): driver traits every back end implements, so a new card is a new crate
//! behind the same trait (the Gaming-PC later). Owner of the traits: desk (changes by change request); each
//! back end belongs to its lane. Rules (CONTRACTS.md): every driver implements `Quiesce` from its first item;
//! long waits sleep (R5); state changes are `klog::event!`s (B6); counts of things are dynamic (R1).
#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

pub mod audio;
pub mod block;
pub mod display;
pub mod net;

use alloc::sync::Arc;
use alloc::vec::Vec;
use ksync::SpinLock;

/// Stops a device's DMA and interrupts and clears the bus master it set, before Reload, reboot or a panic
/// (RB-5a/5b). Must work with interrupts off and must not sleep.
pub trait Quiesce: Send + Sync {
    fn name(&self) -> &str;
    fn quiesce(&self);
}

static QUIESCE: SpinLock<Vec<Arc<dyn Quiesce>>> = SpinLock::named("devmodel.quiesce", Vec::new());

/// Every driver registers itself here when it starts a device.
pub fn register_quiesce(d: Arc<dyn Quiesce>) {
    QUIESCE.lock().push(d);
}

/// RB-16: where a device's Quiesce runs in `quiesce_all`. `Normal`: newest registered first (as before).
/// `Transport`: after every normal one, in the order `set_place` named them: the console mirror and the card
/// it sends on, so every other device's breadcrumb reaches the listener first. `Final`: last of all (the
/// watchdog: its grace must cover everything before the reset).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Place {
    Normal,
    Transport,
    Final,
}

/// Places by Quiesce name (thread context; a name set again keeps its first position, a new place replaces).
static PLACES: SpinLock<Vec<(alloc::string::String, Place)>> = SpinLock::named("devmodel.quiesce_places", Vec::new());
/// The places as `stop_all_before_park` found them (taken with the waiting lock, before any CPU is parked):
/// `quiesce_all`'s order when a parked CPU holds `PLACES`. Only the shutdown's own thread takes it.
static PLACES_BEFORE_PARK: SpinLock<Option<Vec<(alloc::string::String, Place)>>> =
    SpinLock::named("devmodel.quiesce_places_before_park", None);
klog::counter!(ORDER_FALLBACK, "devmodel.quiesce_order_fallback");

pub fn set_place(name: &str, place: Place) {
    let mut g = PLACES.lock();
    match g.iter_mut().find(|(n, _)| n == name) {
        Some(e) => e.1 = place,
        None => g.push((alloc::string::String::from(name), place)),
    }
}

/// The place a name has (`Normal` when none was set).
pub fn place_of(name: &str) -> Place {
    PLACES.lock().iter().find(|(n, _)| n == name).map_or(Place::Normal, |e| e.1)
}

/// The order `quiesce_all` uses: `newest_first` (the registry reversed) sorted stably by place, and within a
/// place other than `Normal` by the order `places` lists the names. Pure (host-tested).
pub fn quiesce_order<'a>(newest_first: &[&'a str], places: &[(alloc::string::String, Place)]) -> Vec<&'a str> {
    let rank = |n: &str| match places.iter().position(|(p, _)| p == n) {
        Some(i) if places[i].1 != Place::Normal => (places[i].1, i),
        _ => (Place::Normal, 0),
    };
    let mut v: Vec<(usize, &str)> = newest_first.iter().copied().enumerate().collect();
    v.sort_by_key(|&(i, n)| (rank(n), i));
    v.into_iter().map(|(_, n)| n).collect()
}

/// Quiesces every registered device in `quiesce_order` (newest first, then the transport, the watchdog last),
/// leaving a breadcrumb before each (RB-16, `klog::crumb`) and one after the last. Returns the names in the
/// order handled. The list and the names are taken before the first Quiesce runs.
pub fn quiesce_all() -> Vec<alloc::string::String> {
    let list: Vec<Arc<dyn Quiesce>> = QUIESCE.lock().iter().rev().cloned().collect();
    let names: Vec<&str> = list.iter().map(|d| d.name()).collect();
    // The places as they are; when a CPU parked inside `set_place` holds them, the copy the stop before the park
    // took (the same shutdown, thread context); without either the plain newest-first order, counted and with a
    // crumb, so the listener and the counter show that the transport may not have come last.
    let order = match PLACES.try_lock() {
        Some(p) => quiesce_order(&names, &p),
        None => match PLACES_BEFORE_PARK.try_lock() {
            Some(g) if g.is_some() => quiesce_order(&names, g.as_deref().unwrap_or(&[])),
            _ => {
                ORDER_FALLBACK.inc();
                klog::crumb::leave(klog::crumb::Stage::Quiesce, "order-fallback");
                names.clone()
            }
        },
    };
    let out: Vec<alloc::string::String> = order.iter().map(|n| alloc::string::String::from(*n)).collect();
    let mut done = alloc::vec![false; list.len()];
    for name in &order {
        let Some(i) = (0..list.len()).find(|&i| !done[i] && list[i].name() == *name) else { continue };
        done[i] = true;
        klog::crumb::leave(klog::crumb::Stage::Device, name);
        list[i].quiesce();
    }
    klog::crumb::leave(klog::crumb::Stage::Quiesced, "");
    out
}

/// Quiesces only the registered devices with this exact name (tests, a single driver's restart).
pub fn quiesce_matching(name: &str) -> usize {
    let list: Vec<Arc<dyn Quiesce>> = QUIESCE.lock().iter().filter(|d| d.name() == name).cloned().collect();
    list.iter().for_each(|d| d.quiesce());
    list.len()
}

/// RB-16: the thread-context half of a device's stop, before the reboot, power-off or Reload parks the other
/// CPUs. Linux runs `device_shutdown` with every CPU and thread alive (linux-6.6/kernel/reboot.c:76
/// `kernel_restart_prepare`, before `machine_restart` stops the CPUs); here the park comes first (RB-quiesce),
/// so a driver whose own thread holds the device's lock across a sleep would find it held in its Quiesce and
/// could only switch bus master off beside a running engine. `stop` may sleep, at most `budget_ns`; it
/// returns whether the device is stopped (its Quiesce then only finishes). It must leave the device so that
/// its Quiesce still works.
pub trait StopBeforePark: Send + Sync {
    fn name(&self) -> &str;
    fn stop(&self, budget_ns: u64) -> bool;
}

static BEFORE_PARK: SpinLock<Vec<Arc<dyn StopBeforePark>>> = SpinLock::named("devmodel.before_park", Vec::new());

pub fn register_stop_before_park(d: Arc<dyn StopBeforePark>) {
    BEFORE_PARK.lock().push(d);
}

/// Every registered `StopBeforePark`, newest first, each with `budget_ns` (thread context, may sleep).
/// Returns (name, stopped) in that order; a breadcrumb before each. Also copies the quiesce places for
/// `quiesce_all` (the park that follows may stop a CPU inside `set_place`).
pub fn stop_all_before_park(budget_ns: u64) -> Vec<(alloc::string::String, bool)> {
    // RB-16 fixer: the places copied while every CPU still runs (`quiesce_all` falls back on them).
    let places = PLACES.lock().clone();
    *PLACES_BEFORE_PARK.lock() = Some(places);
    let list: Vec<Arc<dyn StopBeforePark>> = BEFORE_PARK.lock().iter().rev().cloned().collect();
    list.iter()
        .map(|d| {
            klog::crumb::leave(klog::crumb::Stage::Prepare, d.name());
            (alloc::string::String::from(d.name()), d.stop(budget_ns))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{Place, quiesce_order};
    use alloc::string::String;

    #[test]
    fn normal_newest_first_then_the_transport_in_its_order_then_the_watchdog() {
        let newest_first = ["amdgpu.psp", "mirror", "xhci0", "wlan0", "igc0", "snd/mix", "fch-wdt"];
        let places = [
            (String::from("fch-wdt"), Place::Final),
            (String::from("mirror"), Place::Transport),
            (String::from("igc0"), Place::Transport),
        ];
        assert_eq!(quiesce_order(&newest_first, &places), ["amdgpu.psp", "xhci0", "wlan0", "snd/mix", "mirror", "igc0", "fch-wdt"]);
    }

    #[test]
    fn without_places_the_order_stays_newest_first() {
        let newest_first = ["b", "a", "a"];
        assert_eq!(quiesce_order(&newest_first, &[]), ["b", "a", "a"]);
        let places = [(String::from("a"), Place::Normal)];
        assert_eq!(quiesce_order(&newest_first, &places), ["b", "a", "a"]);
    }

    /// The registries are global: one test owns them (names with a `t-` prefix, the only ones registered here).
    #[test]
    fn registries_leave_a_crumb_before_each_device_and_keep_their_order() {
        use super::*;
        use alloc::boxed::Box;
        use alloc::format;
        use klog::crumb::{Crumb, Store};
        static LOG: SpinLock<Vec<String>> = SpinLock::named("t.devmodel.log", Vec::new());
        struct Rec;
        impl Store for Rec {
            fn leave(&self, c: &Crumb<'_>) -> bool {
                LOG.lock().push(format!("crumb {} {}", c.stage.as_str(), c.what));
                true
            }
        }
        assert!(klog::crumb::set_wire(Box::leak(Box::new(Rec))), "the test binary's only wire store");
        struct Dev(&'static str);
        impl Quiesce for Dev {
            fn name(&self) -> &str {
                self.0
            }
            fn quiesce(&self) {
                LOG.lock().push(format!("quiesce {}", self.0));
            }
        }
        struct Stop(&'static str, bool);
        impl StopBeforePark for Stop {
            fn name(&self) -> &str {
                self.0
            }
            fn stop(&self, budget_ns: u64) -> bool {
                LOG.lock().push(format!("stop {} {budget_ns}", self.0));
                self.1
            }
        }
        let take = || core::mem::take(&mut *LOG.lock());

        // Stop before the park: newest first, the budget handed on, the crumb before each, the result in order.
        register_stop_before_park(Arc::new(Stop("t-held", false)));
        register_stop_before_park(Arc::new(Stop("t-ok", true)));
        let r = stop_all_before_park(7);
        assert_eq!(r, [(String::from("t-ok"), true), (String::from("t-held"), false)]);
        assert_eq!(take(), ["crumb prepare t-ok", "stop t-ok 7", "crumb prepare t-held", "stop t-held 7"]);

        // Quiesce: normal newest first, then the transport; a crumb before each device, one after the last.
        register_quiesce(Arc::new(Dev("t-old")));
        register_quiesce(Arc::new(Dev("t-wire")));
        register_quiesce(Arc::new(Dev("t-new")));
        set_place("t-wire", Place::Transport);
        let order = ["t-new", "t-old", "t-wire"];
        let want: Vec<String> = order
            .iter()
            .flat_map(|n| [format!("crumb device {n}"), format!("quiesce {n}")])
            .chain([String::from("crumb quiesced ")])
            .collect();
        assert_eq!(quiesce_all(), order);
        assert_eq!(take(), want);

        // A CPU parked inside `set_place` holds the places: the copy taken before the park keeps the order.
        let _ = stop_all_before_park(1);
        take();
        {
            let _held = PLACES.lock();
            assert_eq!(quiesce_all(), order);
        }
        assert_eq!(take(), want);

        // Neither: newest first, counted, with a crumb that says so.
        let before = ORDER_FALLBACK.get();
        *PLACES_BEFORE_PARK.lock() = None;
        {
            let _held = PLACES.lock();
            assert_eq!(quiesce_all(), ["t-new", "t-wire", "t-old"]);
        }
        assert_eq!(ORDER_FALLBACK.get(), before + 1);
        assert_eq!(take().first().map(String::as_str), Some("crumb quiesce order-fallback"));
    }

    #[test]
    fn the_transport_order_follows_set_place_not_registration() {
        let newest_first = ["igc0", "x", "mirror"];
        let places = [(String::from("mirror"), Place::Transport), (String::from("igc0"), Place::Transport)];
        assert_eq!(quiesce_order(&newest_first, &places), ["x", "mirror", "igc0"]);
    }
}

/// Error type shared by driver traits where nothing more specific fits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DevError {
    /// Device gone (unplugged, reads all ones).
    Gone,
    Timeout,
    Io,
    /// Refused by a safety rule (R8: write gate, opt-in key).
    Refused,
    NoMemory,
    Unsupported,
    InvalidArgument,
    /// No space left (a full thin pool, or a write beyond the end of a disk): ENOSPC.
    NoSpace,
}
