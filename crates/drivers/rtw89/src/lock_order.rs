//! Host test: ksync's order table accepts the WLAN thread's real nesting. The chip mutex `rtw89.core` is held while
//! rtw89-core reads and writes config space through `KBus` (pci.cfg, pci.ecam above 0xFF), as Linux holds
//! rtwdev->mutex around pci_*_config_* (the Mini-PC logged rtw89.core -> pci.cfg/pci.ecam at every boot, W11).

extern crate std;
use ksync::{Completion, Mutex, SpinLock, ViolationKind, lockdep, order};

// The checker exists only with debug_assertions (ksync lockdep.rs:99/104); a release run has no proof to give, so
// the test is left out there instead of passing silently.
#[cfg(debug_assertions)]
#[test]
fn the_order_table_accepts_the_bring_up_nesting() {
    assert!(lockdep::enabled(), "lockdep off in a debug build");
    let class = ksync::stats::class("rtw89.core").expect("rtw89.core named in the order table");
    assert_eq!(class.level, order::DRIVER_OUTER);
    assert_eq!(order::level_name(class.level), "driver");
    // card.rs: `mac_off_first` and the runner loop hold the chip mutex over config space, the driver's queues, a
    // completion and sleeps; `wl.quiesce.lock_held` runs a disable (pci.enable) under it.
    let core = Mutex::named("rtw89.core", ());
    let enable = Mutex::named("pci.enable", ());
    let cfg = SpinLock::named("pci.cfg", ());
    let ecam = SpinLock::named("pci.ecam", ());
    let cmds = SpinLock::named("rtw89.cmds", ());
    let txq = SpinLock::named("rtw89.txq", ());
    let irqs = SpinLock::named("rtw89.irqs", ());
    let status = SpinLock::named("rtw89.status", ());
    let ring = SpinLock::named("klog.ring", ());
    let done = Completion::new();
    let (_, n) = lockdep::expect_violations(|| {
        let _g = core.lock();
        {
            let _c = cfg.lock();
            let _e = ecam.lock();
        }
        drop(ecam.lock());
        {
            let _p = enable.lock();
            let _c = cfg.lock();
        }
        drop(cmds.lock());
        drop(txq.lock());
        drop(irqs.lock());
        drop(status.lock());
        drop(ring.lock());
        done.complete();
        lockdep::might_sleep();
        assert_eq!(lockdep::held_count(), 1);
    });
    assert_eq!(n, 0);
    assert_eq!(lockdep::held_count(), 0);
    // `quiesce` takes the chip mutex with try_lock only (not order-checked).
    let (_, n) = lockdep::expect_violations(|| {
        let _c = cfg.lock();
        let _g = core.try_lock();
    });
    assert_eq!(n, 0);
    // The reverse stays a report.
    let (_, n) = lockdep::expect_violations(|| {
        let _p = enable.lock();
        let _g = core.lock();
    });
    assert_eq!(n, 1);
    let v = lockdep::list();
    assert!(v.iter().any(|x| x.kind == ViolationKind::Order && x.held == "pci.enable" && x.taking == "rtw89.core"), "{v:?}");
}

/// The driver's own sources: every lock it names is found here, so a renamed or a new lock is checked without
/// anyone remembering to add it to this file.
const SOURCES: &[(&str, &str)] = &[
    ("card.rs", include_str!("card.rs")),
    ("config.rs", include_str!("config.rs")),
    ("dma.rs", include_str!("dma.rs")),
    ("kbus.rs", include_str!("kbus.rs")),
    ("ktests.rs", include_str!("ktests.rs")),
    ("lib.rs", include_str!("lib.rs")),
    ("netdev.rs", include_str!("netdev.rs")),
    ("tool.rs", include_str!("tool.rs")),
];

fn named_locks() -> std::vec::Vec<&'static str> {
    let mut v = std::vec::Vec::new();
    for (_, src) in SOURCES {
        let mut rest: &'static str = src;
        while let Some(i) = rest.find("named(\"") {
            rest = &rest[i + 7..];
            let end = rest.find('"').expect("a closed lock name");
            if !v.contains(&&rest[..end]) {
                v.push(&rest[..end]);
            }
        }
    }
    v
}

// The checker's finding (W12): the nesting above is replayed by hand, so this one ties it to the driver's code.
#[cfg(debug_assertions)]
#[test]
fn every_lock_of_the_driver_and_the_pci_chain_sits_inside_rtw89_core() {
    assert!(lockdep::enabled(), "lockdep off in a debug build");
    let card = SOURCES.iter().find(|(f, _)| *f == "card.rs").unwrap().1;
    assert!(card.contains("Mutex::named(\"rtw89.core\""), "card.rs no longer names the chip mutex rtw89.core");
    let kbus = SOURCES.iter().find(|(f, _)| *f == "kbus.rs").unwrap().1;
    for call in ["self.pci.read32(", "self.pci.read32_ext(", "self.pci.write32("] {
        assert!(kbus.contains(call), "kbus.rs config path changed ({call}): recheck the locks it takes");
    }
    let names = named_locks();
    assert!(names.len() >= 9, "{names:?}");
    // pci::PciFunction's config accessors take pci.cfg (and pci.ecam above 0xFF); disable/enable take pci.enable.
    let core = Mutex::named("rtw89.core", ());
    for n in names.iter().copied().chain(["pci.enable", "pci.cfg", "pci.ecam"]) {
        if n == "rtw89.core" {
            continue;
        }
        let l = SpinLock::named(n, ());
        drop(l.lock());
        let level = ksync::stats::class(n).expect("a class once taken").level;
        assert!(level > order::DRIVER_OUTER, "{n} at level {level} cannot be taken under rtw89.core");
        let (_, bad) = lockdep::expect_violations(|| {
            let _g = core.lock();
            drop(l.lock());
        });
        assert_eq!(bad, 0, "rtw89.core > {n}: {:?}", lockdep::list());
    }
}
