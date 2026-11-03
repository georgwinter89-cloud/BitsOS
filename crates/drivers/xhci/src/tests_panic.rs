//! RB-13: `Controller::panic_stop` against the fake model: halt before HCRST, the 1 ms quiet time, CNR waited
//! for, bus master off, and every wait bounded (a controller that never halts, a CNR that never clears, a gone
//! controller).

use crate::fake::{Cfg, FakeEnv};
use crate::hc::{Controller, Env, PanicList, State, HALT_NS, HCRST_QUIET_NS, MS, PANIC_RESET_NS};
use crate::regs::{self, cmd, sts, walk_ext_caps, Caps};
use alloc::string::String;

fn up(cfg: Cfg) -> (Controller<FakeEnv>, FakeEnv) {
    let env = FakeEnv::new(cfg);
    let caps = Caps::parse(|o| env.read32(o), env.bar_len()).expect("caps");
    let ext = walk_ext_caps(|o| env.read32(o), caps.xecp, env.bar_len(), caps.max_ports);
    let c = Controller::new(String::from("xhci-panic"), env.clone(), caps, ext);
    c.start().expect("start");
    env.model().writes.clear();
    (c, env)
}

#[test]
fn halts_then_resets_then_bus_master_off() {
    for cfg in [Cfg::minipc(), Cfg::qemu()] {
        let (c, env) = up(cfg);
        let usbcmd = c.caps().op(regs::USBCMD);
        let r = c.panic_stop(&|| env.now_ns());
        assert!(r.halted && r.reset && r.ready && !r.gone, "{r:?}");
        assert_eq!(c.state(), State::Quiesced);
        let m = env.model();
        let cmds: std::vec::Vec<u32> = m.writes.iter().filter(|w| w.0 == usbcmd).map(|w| w.1).collect();
        assert_eq!(cmds.len(), 2, "one halt write, one HCRST write: {cmds:x?}");
        assert_eq!(cmds[0] & (cmd::RS | cmd::INTE | cmd::HSEE | cmd::HCRST), 0, "first RS, INTE, HSEE off");
        assert_eq!(cmds[1], cmd::HCRST, "then HCRST alone");
        assert!(!m.early_read, "a register was read within 1 ms of HCRST");
        assert_eq!(m.usbcmd & cmd::HCRST, 0);
        assert_eq!(m.usbsts & (sts::CNR | sts::HCH), sts::HCH, "reset done: halted, controller ready");
        assert!(!m.bus_master);
        assert!(r.ns >= HCRST_QUIET_NS && r.ns < HALT_NS + HCRST_QUIET_NS + 10 * MS, "{} ns", r.ns);
    }
}

#[test]
fn a_controller_that_never_halts_is_not_reset() {
    let (c, env) = up(Cfg::minipc());
    env.model().cfg.never_halts = true;
    let usbcmd = c.caps().op(regs::USBCMD);
    let r = c.panic_stop(&|| env.now_ns());
    assert!(!r.halted && !r.reset && !r.ready && !r.gone, "{r:?}");
    let m = env.model();
    assert!(m.writes.iter().all(|w| w.0 != usbcmd || w.1 & cmd::HCRST == 0), "HCRST on a running controller (Linux xhci_reset refuses)");
    assert!(!m.bus_master, "bus master off anyway");
    assert!(r.ns >= HALT_NS && r.ns < HALT_NS + 5 * MS, "the halt wait is bounded: {} ns", r.ns);
}

#[test]
fn a_cnr_that_never_clears_is_bounded() {
    let (c, env) = up(Cfg::minipc());
    env.model().cfg.cnr_stuck = true;
    let r = c.panic_stop(&|| env.now_ns());
    assert!(r.halted && r.reset && !r.ready && !r.gone, "{r:?}");
    assert!(!env.model().bus_master);
    let most = HALT_NS + HCRST_QUIET_NS + PANIC_RESET_NS + 5 * MS;
    assert!(r.ns >= PANIC_RESET_NS && r.ns < most, "the reset wait is bounded: {} ns", r.ns);
}

#[test]
fn a_gone_controller_gets_no_register_writes() {
    let (c, env) = up(Cfg::minipc());
    env.model().gone = true;
    let r = c.panic_stop(&|| env.now_ns());
    assert!(r.gone && !r.halted && !r.reset && !r.ready, "{r:?}");
    let m = env.model();
    assert!(m.writes.is_empty(), "{:x?}", m.writes);
    assert!(!m.bus_master);
}

#[test]
fn nothing_restarts_it_afterwards() {
    let (c, env) = up(Cfg::minipc());
    let _ = c.panic_stop(&|| env.now_ns());
    c.check_health();
    assert_eq!(c.state(), State::Quiesced, "the health watch leaves it alone");
}

// RB-13 fixer: the panic path walks `PanicList`, never the `CONTROLLERS` lock (a CPU the panic stopped may hold it
// for good). These check the list itself: adds racing from many threads all land once, the walk keeps probe
// order, and the walk and the stops run while the driver's list lock is held by someone else.

#[test]
fn the_panic_list_takes_every_add_once_even_racing() {
    static LIST: PanicList<usize> = PanicList::new();
    let threads: std::vec::Vec<_> = (0..8)
        .map(|t| std::thread::spawn(move || (0..200).for_each(|i| LIST.add(t * 1000 + i))))
        .collect();
    threads.into_iter().for_each(|h| h.join().unwrap());
    let mut seen = std::vec::Vec::new();
    let n = LIST.for_each(&mut |v| seen.push(*v));
    assert_eq!(n, 1600);
    seen.sort_unstable();
    seen.dedup();
    assert_eq!(seen.len(), 1600, "every add exactly once");
}

#[test]
fn panic_stops_walk_the_list_while_the_driver_lock_is_held() {
    let list: &'static PanicList<(Controller<FakeEnv>, FakeEnv)> = std::boxed::Box::leak(std::boxed::Box::new(PanicList::new()));
    for cfg in [Cfg::minipc(), Cfg::qemu(), Cfg::minipc()] {
        list.add(up(cfg));
    }
    // A stand-in for `CONTROLLERS` held by a CPU the panic stopped: nothing below may wait for it.
    let held = ksync::SpinLock::new(());
    let _g = held.lock();
    let mut order = std::vec::Vec::new();
    let n = list.for_each(&mut |(c, env)| {
        let r = c.panic_stop(&|| env.now_ns());
        assert!(r.halted && r.reset && r.ready, "{r:?}");
        assert!(!env.model().bus_master);
        order.push(c.caps().max_ports);
    });
    assert_eq!(n, 3, "every controller stopped although the lock is held");
    assert_eq!(order[0], order[2], "probe order");
}

#[test]
fn an_empty_panic_list_stops_nothing() {
    let list: PanicList<u8> = PanicList::new();
    assert_eq!(list.for_each(&mut |_| panic!("no item")), 0);
}
