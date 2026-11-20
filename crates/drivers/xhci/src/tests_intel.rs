//! GP-BUS (plan/spec/gaming-pc.md §5): the Gaming-PC's Intel controllers (8086:7ec0 at 00:0d.0, 8086:7f6e at 80:14.0)
//! against the fake. What Linux does for exactly these ids (linux-7.3.1 drivers/usb/host/xhci-pci.c:346-350:
//! LPM_SUPPORT, INTEL_HOST, AVOID_BEI; none of the id lists at :351-417 names them) and what the bring-up needs of it:
//! - INTEL_HOST: 1 ms of silence after HCRST (xhci.c:205-214). BitsOS keeps it for every vendor.
//! - AVOID_BEI: `quirks::for_id`, isochronous only.
//! - BIOS handoff (pci-quirks.c:1165-1262): ask, wait 1 s, take it anyway, SMIs off.
//! - Port routing (`usb_enable_intel_xhci_ports`, pci-quirks.c): only with an Intel EHCI in the machine; this one
//!   has none, so there is nothing to route.
//! - Time bounds: Linux's for Intel (`quirks::Timing::LINUX`), V1's for everything else, unchanged.

use crate::fake::{Cfg, FakeEnv, Legacy};
use crate::hc::{Controller, Env, State, BIOS_NS, HALT_NS, HCRST_QUIET_NS, MS, RESET_NS};
use crate::quirks::{self, Quirks, Timing, GAMING_PC_IDS, MINIPC_IDS};
use crate::regs::{cmd, legacy, sts, walk_ext_caps, Caps, ContextSize};
use std::string::String;

fn ctrl(cfg: Cfg, t: Timing) -> (Controller<FakeEnv>, FakeEnv) {
    let env = FakeEnv::new(cfg);
    let caps = Caps::parse(|o| env.read32(o), env.bar_len()).expect("caps");
    let ext = walk_ext_caps(|o| env.read32(o), caps.xecp, env.bar_len(), caps.max_ports);
    (Controller::new(String::from("xhci-intel"), env.clone(), caps, ext).with_quirks(quirks::for_id(0x8086, 0x7F6E)).with_timing(t), env)
}

#[test]
fn both_controllers_get_intels_quirks_and_linuxs_bounds() {
    for (v, d) in GAMING_PC_IDS {
        assert_eq!(quirks::for_id(v, d), Quirks { trust_tx_length: false, avoid_bei: true }, "{v:04x}:{d:04x}");
        assert_eq!(quirks::timing_for(v), Timing::LINUX);
    }
    assert_eq!(Timing::LINUX, Timing { halt_ns: 32 * MS, hcrst_ns: 10_000 * MS, cnr_ns: Some(10_000 * MS) });
}

#[test]
fn every_other_vendor_keeps_the_bounds_it_had() {
    for (v, _) in MINIPC_IDS {
        assert_eq!(quirks::timing_for(v), Timing::V1);
    }
    assert_eq!(quirks::timing_for(0x1B36), Timing::V1, "QEMU");
    assert_eq!(quirks::timing_for(0x1033), Timing::V1, "NEC");
    // The numbers themselves: 20 ms to halt, 1 s for HCRST and CNR together.
    assert_eq!(Timing::V1, Timing { halt_ns: HALT_NS, hcrst_ns: RESET_NS, cnr_ns: None });
    assert_eq!((HALT_NS, RESET_NS), (20 * MS, 1000 * MS));
    // A controller made without `with_timing` runs with them.
    let env = FakeEnv::new(Cfg::minipc());
    let caps = Caps::parse(|o| env.read32(o), env.bar_len()).unwrap();
    let ext = walk_ext_caps(|o| env.read32(o), caps.xecp, env.bar_len(), caps.max_ports);
    assert_eq!(Controller::new(String::from("x"), env, caps, ext).timing(), Timing::V1);
}

#[test]
fn the_chipset_controllers_shape_comes_up_from_the_bios() {
    let (c, env) = ctrl(Cfg::gaming_pc(), Timing::LINUX);
    c.start().expect("start");
    assert_eq!(c.state(), State::Running);
    assert_eq!((c.caps().csz, c.caps().max_ports, c.caps().ac64, c.caps().ppc), (ContextSize::B32, 22, true, false));
    let m = env.model();
    // Taken from the BIOS, its SMIs off and their events cleared, before anything else was written.
    assert!(m.legsup & legacy::OS_OWNED != 0 && m.legsup & legacy::BIOS_OWNED == 0);
    assert_eq!(m.legctl, 0xE001_E011 & legacy::CTL_KEEP);
    assert!(!m.early_read, "a register was read within 1 ms of HCRST");
    assert_eq!(m.bus_master_ons, 1);
    assert!(m.usbcmd & cmd::RS != 0 && m.usbsts & (sts::HCH | sts::HSE | sts::CNR) == 0);
    let hcrst = m.writes.iter().position(|w| w.0 == crate::fake::CAPLEN && w.1 & cmd::HCRST != 0).unwrap();
    let handoff = m.writes.iter().position(|w| w.0 == crate::fake::XECP).unwrap();
    assert!(handoff < hcrst, "the BIOS was asked before the reset");
    // No write went to a port routing register: there is none in the model and none on this machine.
    assert!(m.writes.iter().all(|w| w.0 < crate::fake::BAR));
}

#[test]
fn a_bios_that_never_lets_go_loses_the_controller_after_a_second() {
    let (c, env) = ctrl(Cfg { legacy: Legacy::Never, ..Cfg::gaming_pc() }, Timing::LINUX);
    c.start().expect("start");
    let m = env.model();
    assert!(m.time >= BIOS_NS && m.time < BIOS_NS + 200 * MS, "waited {} ms", m.time / MS);
    assert!(m.legsup & legacy::BIOS_OWNED == 0 && m.legsup & legacy::OS_OWNED != 0);
}

#[test]
fn a_slow_halt_passes_under_linuxs_bound_and_not_under_v1s() {
    let slow = Cfg { halt_takes: 25 * MS, ..Cfg::gaming_pc() };
    let (c, _) = ctrl(slow.clone(), Timing::LINUX);
    c.start().expect("25 ms to halt is inside Linux's 32 ms");
    let (c, env) = ctrl(slow, Timing::V1);
    let e = c.start().unwrap_err();
    assert!(e.contains("halt (HCH)"), "{e}");
    assert!(!env.model().bus_master);
}

#[test]
fn a_halt_slower_than_linux_allows_parks_the_controller_and_says_why() {
    let (c, env) = ctrl(Cfg { halt_takes: 40 * MS, legacy: Legacy::NotOwned, ..Cfg::gaming_pc() }, Timing::LINUX);
    let e = c.start().unwrap_err();
    assert!(e.contains("halt (HCH)"), "{e}");
    assert_eq!(c.state(), State::Failed);
    assert!(c.reason().contains("halt"));
    let m = env.model();
    assert!(m.time >= 32 * MS && m.time < 40 * MS, "gave up after {} ms", m.time / MS);
    assert!(!m.bus_master && m.writes.iter().all(|w| w.1 & cmd::HCRST == 0 || w.0 != crate::fake::CAPLEN), "no reset of a controller that did not halt");
}

#[test]
fn a_slow_reset_passes_under_linuxs_bounds_and_not_under_v1s() {
    let slow = Cfg { hcrst_takes: 3000 * MS, cnr_takes: 2000 * MS, legacy: Legacy::NotOwned, ..Cfg::gaming_pc() };
    let (c, env) = ctrl(slow.clone(), Timing::LINUX);
    c.start().expect("3 s and 2 s are inside Linux's 10 s each");
    assert!(!env.model().early_read);
    let (c, _) = ctrl(slow, Timing::V1);
    let e = c.start().unwrap_err();
    assert!(e.contains("reset (HCRST)"), "{e}");
}

#[test]
fn hcrst_and_cnr_each_get_their_own_ten_seconds() {
    // 9 s for each: more than one shared budget of 10 s, inside two.
    let (c, _) = ctrl(Cfg { hcrst_takes: 9000 * MS, cnr_takes: 9000 * MS, legacy: Legacy::NotOwned, ..Cfg::gaming_pc() }, Timing::LINUX);
    c.start().expect("9 s each");
    let (c, env) = ctrl(Cfg { hcrst_takes: 11_000 * MS, legacy: Legacy::NotOwned, ..Cfg::gaming_pc() }, Timing::LINUX);
    let e = c.start().unwrap_err();
    assert!(e.contains("reset (HCRST)"), "{e}");
    let t = env.model().time;
    assert!((10_000 * MS..10_200 * MS).contains(&t), "gave up after {} ms", t / MS);
}

#[test]
fn a_controller_that_never_becomes_ready_ends_by_itself() {
    let (c, env) = ctrl(Cfg { cnr_stuck: true, legacy: Legacy::NotOwned, ..Cfg::gaming_pc() }, Timing::LINUX);
    let e = c.start().unwrap_err();
    assert!(e.contains("reset (CNR)"), "{e}");
    assert_eq!(c.state(), State::Failed);
    let m = env.model();
    assert!(m.time < 10_300 * MS, "bounded: {} ms", m.time / MS);
    assert!(!m.bus_master && m.usbcmd & cmd::RS == 0, "parked: no DMA, not running");
}

/// A running controller named `name` whose halt takes 25 ms from now on.
fn running_then_slow(name: &str, t: Timing) -> (Controller<FakeEnv>, FakeEnv) {
    let env = FakeEnv::new(Cfg { legacy: Legacy::NotOwned, ..Cfg::gaming_pc() });
    let caps = Caps::parse(|o| env.read32(o), env.bar_len()).expect("caps");
    let ext = walk_ext_caps(|o| env.read32(o), caps.xecp, env.bar_len(), caps.max_ports);
    let c = Controller::new(String::from(name), env.clone(), caps, ext).with_timing(t);
    c.start().expect("start");
    env.model().cfg.halt_takes = 25 * MS;
    env.model().writes.clear();
    (c, env)
}

/// The bound that lets a slow controller halt at bring-up holds for the panic stop as well. Without it a panic on
/// the Gaming-PC would find the chipset controller "not halted" after 20 ms and skip the reset, and the firmware
/// would not see the root ports as after power-on (RB-13: the stick must boot again after a panic).
#[test]
fn a_slow_halt_is_waited_for_at_the_panic_stop() {
    let (c, env) = running_then_slow("xhci-slow-panic", Timing::LINUX);
    let r = c.panic_stop(&|| env.now_ns());
    assert!(r.halted && r.reset && r.ready && !r.gone, "{r:?}");
    assert!(!env.model().bus_master);
    assert!(r.ns >= 25 * MS && r.ns < 32 * MS + HCRST_QUIET_NS + 10 * MS, "{} ns", r.ns);
    // Every other vendor: 20 ms as before, and no reset of a controller that did not halt in them.
    let (c, env) = running_then_slow("xhci-slow-panic-v1", Timing::V1);
    let r = c.panic_stop(&|| env.now_ns());
    assert!(!r.halted && !r.reset && !r.ready && !r.gone, "{r:?}");
    let m = env.model();
    assert!(m.writes.iter().all(|w| w.1 & cmd::HCRST == 0 || w.0 != crate::fake::CAPLEN));
    assert!(!m.bus_master);
    assert!(r.ns >= HALT_NS && r.ns < HALT_NS + 5 * MS, "{} ns", r.ns);
}

/// `quiesce` (shutdown, Reload) notes whether the controller halted: the same bound there.
#[test]
fn a_slow_halt_is_waited_for_at_quiesce() {
    let note = |name: &str, t: Timing| -> String {
        let (c, env) = running_then_slow(name, t);
        let seq = klog::next_seq();
        let t0 = env.model().time;
        c.quiesce();
        let took = env.model().time - t0;
        assert!(!env.model().bus_master);
        assert!(took < 32 * MS + 5 * MS, "{name}: bounded, {} ms", took / MS);
        c.log_quiesce_note();
        let line = klog::read_since(seq, usize::MAX).into_iter().find(|r| r.name == "xhci.quiesce" && r.text.contains(name)).expect("the note");
        line.text
    };
    let intel = note("xhci-slow-quiesce-intel", Timing::LINUX);
    assert!(intel.contains("halted=true"), "{intel}");
    let v1 = note("xhci-slow-quiesce-v1", Timing::V1);
    assert!(v1.contains("halted=false"), "{v1}");
}
