//! Host tests of the controller logic against the fake model (`fake.rs`): the Mini-PC shape (64-byte contexts,
//! two scratchpad pages, two event ring segments) and QEMU's (32-byte, none, one), BIOS handoff, the failure
//! paths (CNR stuck, HSE, never halts, gone), commands with wrap-around, the abort path, events, Quiesce.

use crate::fake::{Cfg, FakeEnv, FakeMem, Legacy, CAPLEN, DB, RTS, XECP};
use crate::hc::{CmdError, Controller, Env, State, BIOS_NS};
use crate::mem::DmaMem;
use crate::regs::{self, cmd, crcr, legacy, sts, walk_ext_caps, Caps, ContextSize};
use crate::ring::{EventRing, ProducerRing, RingError};
use crate::trb::{cc, ty, Trb, CYCLE, LINK_TC};
use alloc::string::String;
use core::sync::atomic::Ordering;
use std::vec::Vec;

fn ctrl(cfg: Cfg) -> (Controller<FakeEnv>, FakeEnv) {
    let env = FakeEnv::new(cfg);
    let caps = Caps::parse(|o| env.read32(o), env.bar_len()).expect("caps");
    let ext = walk_ext_caps(|o| env.read32(o), caps.xecp, env.bar_len(), caps.max_ports);
    (Controller::new(String::from("xhci-test"), env.clone(), caps, ext), env)
}

fn up(cfg: Cfg) -> (Controller<FakeEnv>, FakeEnv) {
    let (c, env) = ctrl(cfg);
    c.start().expect("start");
    (c, env)
}

fn write_index(env: &FakeEnv, off: usize) -> Option<usize> {
    env.model().writes.iter().rposition(|w| w.0 == off)
}

#[test]
fn minipc_shape_comes_up() {
    let (c, env) = up(Cfg::minipc());
    assert_eq!(c.state(), State::Running);
    assert_eq!(c.caps().csz, ContextSize::B64);
    assert_eq!(c.caps().scratchpads, 2);
    assert_eq!(c.ext().protocols.len(), 2);
    assert!(c.ext().legacy.is_some());
    let mut m = env.model();
    assert!(!m.early_read, "a register was read within 1 ms of HCRST");
    assert!(m.bus_master);
    assert_eq!(m.config & 0xFF, 64, "MaxSlotsEn = all slots");
    // DCBAA[0] = scratchpad array with two distinct page-aligned pages.
    let dcbaa = m.dcbaap;
    assert!(dcbaa != 0 && dcbaa % 64 == 0);
    let array = m.rd_mem64(dcbaa).unwrap();
    let (p0, p1) = (m.rd_mem64(array).unwrap(), m.rd_mem64(array + 8).unwrap());
    assert!(p0 != 0 && p1 != 0 && p0 != p1 && p0 % 4096 == 0 && p1 % 4096 == 0);
    // Command ring pointer with RCS 1, two ERST entries, interrupter 0 prepared.
    assert!(m.ccs && m.crcr_ptr != 0);
    assert_eq!(m.erstsz & 0xFFFF, 2);
    let erstba = m.erstba;
    assert_eq!(m.rd_mem32(erstba + 8), Some(256));
    assert_eq!(m.rd_mem32(erstba + 24), Some(256));
    assert_eq!(m.imod, regs::IMOD_DEFAULT);
    assert_eq!(m.iman & regs::iman::IE, 0, "polled: IE off");
    assert!(m.portsc.iter().all(|p| p & regs::portsc::PP != 0), "PPC: every port powered");
    assert!(!m.ped_written, "never write 1 to PED");
    assert!(m.usbcmd & cmd::RS != 0 && m.usbsts & sts::HCH == 0);
    drop(m);
    // ERSTBA (high dword latches) after ERSTSZ, ERDP, CRCR, DCBAAP; RS after all of them.
    let erstba = write_index(&env, RTS + 0x34).unwrap();
    for off in [RTS + 0x28, RTS + 0x3C, CAPLEN + regs::CRCR + 4, CAPLEN + regs::DCBAAP + 4] {
        assert!(write_index(&env, off).unwrap() < erstba, "{off:#x} written after ERSTBA");
    }
    let rs = env.model().writes.iter().rposition(|w| w.0 == CAPLEN && w.1 & cmd::RS != 0).unwrap();
    assert!(rs > erstba);
    assert_eq!(c.ports_at_takeover(), std::vec![1]);
}

#[test]
fn qemu_shape_comes_up() {
    let (c, env) = up(Cfg::qemu());
    assert_eq!(c.caps().csz, ContextSize::B32);
    let mut m = env.model();
    assert_eq!(m.erstsz & 0xFFFF, 1, "ERST Max 0: one segment");
    let d = m.dcbaap;
    assert_eq!(m.rd_mem64(d), Some(0), "no scratchpads: DCBAA[0] stays 0");
    assert!(m.portsc.iter().all(|p| p & regs::portsc::PP != 0), "PP untouched without PPC (QEMU ports are powered)");
    drop(m);
    assert_eq!(c.ring_info().2, 1);
}

#[test]
fn bios_handoff_released() {
    let (c, env) = up(Cfg { legacy: Legacy::Releases(5 * crate::hc::MS), ..Cfg::minipc() });
    assert_eq!(c.state(), State::Running);
    let m = env.model();
    assert!(m.legsup & legacy::OS_OWNED != 0 && m.legsup & legacy::BIOS_OWNED == 0);
    assert!(m.time < BIOS_NS, "released within the wait, no forced takeover");
    assert_eq!(m.legctl, 0xE001_E011 & legacy::CTL_KEEP, "SMI enables off, SMI events cleared");
}

#[test]
fn bios_handoff_forced_after_timeout() {
    let (c, env) = up(Cfg { legacy: Legacy::Never, ..Cfg::minipc() });
    assert_eq!(c.state(), State::Running);
    let m = env.model();
    assert!(m.time >= BIOS_NS, "waited the full second first");
    assert!(m.legsup & legacy::BIOS_OWNED == 0 && m.legsup & legacy::OS_OWNED != 0, "taken anyway, OS owned");
}

#[test]
fn cnr_stuck_parks_the_controller() {
    let (c, env) = ctrl(Cfg { cnr_stuck: true, ..Cfg::minipc() });
    let e = c.start().unwrap_err();
    assert!(e.contains("CNR"), "{e}");
    assert_eq!(c.state(), State::Failed);
    assert!(c.reason().contains("CNR"));
    let m = env.model();
    assert!(!m.bus_master && m.usbcmd & cmd::RS == 0);
    assert!(m.time >= crate::hc::RESET_NS, "waited the full second");
}

#[test]
fn hse_after_run_parks_the_controller() {
    let (c, env) = ctrl(Cfg { hse_on_run: true, ..Cfg::minipc() });
    let e = c.start().unwrap_err();
    assert!(e.contains("HSE"), "{e}");
    assert_eq!(c.state(), State::Failed);
    assert!(!env.model().bus_master);
    assert_eq!(c.noop(), Err(CmdError::NotRunning));
}

#[test]
fn controller_that_never_halts() {
    let (c, env) = ctrl(Cfg { never_halts: true, ..Cfg::minipc() });
    env.model().usbsts &= !sts::HCH;
    let e = c.start().unwrap_err();
    assert!(e.contains("halt"), "{e}");
    assert!(!env.model().bus_master);
}

#[test]
fn gone_device() {
    let env = FakeEnv::new(Cfg::minipc());
    env.model().gone = true;
    assert!(Caps::parse(|o| env.read32(o), env.bar_len()).is_err());
    let (c, env) = up(Cfg::minipc());
    env.model().gone = true;
    c.check_health();
    assert_eq!(c.state(), State::Failed);
    assert!(c.reason().contains("gone"));
    assert_eq!(c.noop(), Err(CmdError::NotRunning));
}

#[test]
fn hse_while_running_is_noticed() {
    let (c, env) = up(Cfg::minipc());
    env.model().usbsts |= sts::HSE;
    c.thread_round();
    assert_eq!(c.state(), State::Failed);
    assert!(c.reason().contains("HSE"));
    assert!(!env.model().bus_master);
}

#[test]
fn noop_and_both_rings_wrap() {
    for cfg in [Cfg::minipc(), Cfg::qemu()] {
        let (c, env) = up(cfg);
        let n = 1100;
        for i in 0..n {
            let r = c.noop();
            assert!(r.is_ok(), "No-Op {i}: {r:?}");
        }
        let (cw, ew, segs) = c.ring_info();
        assert!(cw >= 4, "command ring wraps {cw}");
        assert!(ew as usize >= n / (256 * segs), "event ring ({segs} segments) wraps {ew}");
        assert_eq!(env.model().lost_events, 0, "event ring never full");
        assert_eq!(env.model().commands_seen, n);
        assert_eq!(c.stats.cmd_completions.load(Ordering::Relaxed), n as u64);
        assert_eq!(c.stats.by_waiter.load(Ordering::Relaxed), n as u64, "no thread here: waiters drained");
        // Interrupt state acknowledged on each batch.
        let m = env.model();
        assert_eq!(m.usbsts & sts::EINT, 0);
        assert_eq!(m.iman & regs::iman::IP, 0);
    }
}

#[test]
fn command_abort_reinits_the_ring() {
    let (c, env) = up(Cfg::minipc());
    assert!(c.noop().is_ok());
    let base = env.model().crcr_ptr;
    env.model().drop_commands = 1;
    let t0 = env.now_ns();
    assert_eq!(c.noop(), Err(CmdError::Timeout));
    assert!(env.now_ns() - t0 >= crate::hc::CMD_NS, "waited the full second");
    assert!(env.model().ca_written);
    assert_eq!(c.state(), State::Running, "only the command failed");
    assert_eq!(c.stats.timeouts.load(Ordering::Relaxed), 1);
    {
        let m = env.model();
        assert!(m.ccs, "fresh ring: RCS 1");
        assert_eq!(m.crcr_ptr, base - 16, "pointer back at the ring start");
    }
    // The ring starts over and works.
    for _ in 0..300 {
        assert!(c.noop().is_ok());
    }
    assert_eq!(c.stats.stray_completions.load(Ordering::Relaxed), 0);
    assert_eq!(c.stats.aborted_completions.load(Ordering::Relaxed), 1, "the abort's own Command Aborted event");
}

#[test]
fn abort_that_does_not_stop_parks_the_controller() {
    let (c, env) = up(Cfg::minipc());
    {
        let mut m = env.model();
        m.drop_commands = 1;
        m.crr_stuck = true;
    }
    assert_eq!(c.noop(), Err(CmdError::ControllerFailed));
    assert_eq!(c.state(), State::Failed);
    assert!(c.reason().contains("did not stop"), "{}", c.reason());
    assert!(!env.model().bus_master);
}

#[test]
fn port_events_and_bad_ports() {
    let (c, env) = up(Cfg::minipc());
    {
        let mut m = env.model();
        m.port_event(3);
        m.port_event(3);
        m.port_event(99);
        m.port_event(0);
        m.port_event(7);
    }
    assert!(c.event_pending());
    assert_eq!(c.process_events(crate::hc::Source::Thread), 5);
    assert_eq!(c.take_port_changes(), std::vec![3, 7]);
    assert!(c.take_port_changes().is_empty());
    assert_eq!(c.stats.bad_port_events.load(Ordering::Relaxed), 2);
    assert_eq!(c.stats.by_thread.load(Ordering::Relaxed), 5);
    assert!(!c.event_pending());
}

#[test]
fn host_controller_and_transfer_events() {
    let (c, env) = up(Cfg::minipc());
    {
        let mut m = env.model();
        m.post(Trb { param: 0, status: (cc::EVENT_RING_FULL as u32) << 24, control: (ty::HOST_CONTROLLER as u32) << 10 });
        m.post(Trb { param: 0x1234_5670, status: (cc::SUCCESS as u32) << 24 | 8, control: (ty::TRANSFER_EVENT as u32) << 10 | 1 << 16 | 2 << 24 });
        m.post(Trb { param: 0, status: 0, control: (ty::MFINDEX_WRAP as u32) << 10 });
    }
    assert_eq!(c.process_events(crate::hc::Source::Thread), 3);
    assert_eq!(c.stats.ring_full.load(Ordering::Relaxed), 1);
    assert_eq!(c.stats.transfer_events.load(Ordering::Relaxed), 1);
    assert_eq!(c.stats.other_events.load(Ordering::Relaxed), 1);
    assert_eq!(c.state(), State::Running);
}

#[test]
fn one_batch_spans_both_segments() {
    // More events than one segment between two drains: one batch takes them all across the segment change.
    let (c, env) = up(Cfg::minipc());
    {
        let mut m = env.model();
        for i in 0..400 {
            m.port_event((i % 7 + 1) as u8);
        }
    }
    let n = c.process_events(crate::hc::Source::Thread);
    assert_eq!(n, 400);
    assert_eq!(env.model().lost_events, 0);
    assert_eq!(c.take_port_changes().len(), 7);
}

#[test]
fn quiesce_and_restart() {
    let (c, env) = up(Cfg::minipc());
    c.quiesce();
    assert_eq!(c.state(), State::Quiesced);
    {
        let m = env.model();
        assert!(m.usbcmd & cmd::RS == 0 && m.usbsts & sts::HCH != 0 && !m.bus_master);
    }
    assert_eq!(c.noop(), Err(CmdError::NotRunning));
    assert!(!c.event_pending());
    c.check_health();
    assert_eq!(c.state(), State::Quiesced, "the health watch leaves a quiesced controller alone");
    c.restart("test").unwrap();
    assert_eq!(c.state(), State::Running);
    assert!(c.noop().is_ok());
    assert!(env.model().bus_master);
}

#[test]
fn quiesce_of_a_failed_controller() {
    let (c, env) = ctrl(Cfg { hse_on_run: true, ..Cfg::minipc() });
    let _ = c.start();
    c.quiesce();
    assert_eq!(c.state(), State::Quiesced);
    assert!(!env.model().bus_master);
}

#[test]
fn quiesce_during_bring_up_wins() {
    // BIOS that never lets go: the bring-up sits in the 1 s handoff wait, and a Quiesce arrives there (e.g. the
    // panic or Reload path). Neither bus master nor RS may be switched on afterwards.
    let (c, env) = ctrl(Cfg { legacy: Legacy::Never, ..Cfg::minipc() });
    let c = std::sync::Arc::new(c);
    let w = std::sync::Arc::downgrade(&c);
    *env.on_relax.lock().unwrap() = Some(std::boxed::Box::new(move || {
        if let Some(c) = w.upgrade() {
            c.quiesce();
        }
    }));
    let writes_before = env.model().writes.len();
    let e = c.start().unwrap_err();
    assert!(e.contains("stopped during bring-up"), "{e}");
    assert_eq!(c.state(), State::Quiesced);
    let m = env.model();
    assert_eq!(m.bus_master_ons, 0, "bus master never switched on after the Quiesce");
    assert!(!m.bus_master && m.usbcmd & cmd::RS == 0);
    assert!(!m.writes[writes_before..].iter().any(|w| w.0 == CAPLEN && w.1 & (cmd::RS | cmd::HCRST) != 0),
        "no reset and no RS after the Quiesce");
    drop(m);
    // A restart afterwards works as usual.
    c.restart("test").unwrap();
    assert!(c.noop().is_ok());
}

#[test]
fn quiesce_during_reset_wait_wins() {
    // Later in the bring-up: the Quiesce lands while CNR is polled, before bus master and RS.
    let (c, env) = ctrl(Cfg::minipc());
    let c = std::sync::Arc::new(c);
    let w = std::sync::Arc::downgrade(&c);
    let env2 = env.clone();
    // The hook re-arms itself until HCRST was written, so it fires inside the reset wait.
    fn hook(env: FakeEnv, w: std::sync::Weak<Controller<FakeEnv>>) -> crate::fake::Hook {
        std::boxed::Box::new(move || {
            let hcrst = env.model().writes.iter().any(|w| w.0 == CAPLEN && w.1 & cmd::HCRST != 0);
            if hcrst {
                if let Some(c) = w.upgrade() {
                    c.quiesce();
                }
            } else {
                let again = hook(env.clone(), w);
                *env.on_relax.lock().unwrap() = Some(again);
            }
        })
    }
    *env.on_relax.lock().unwrap() = Some(hook(env2, w));
    assert!(c.start().is_err());
    assert_eq!(c.state(), State::Quiesced);
    let m = env.model();
    assert_eq!(m.bus_master_ons, 0);
    assert!(!m.bus_master && m.usbcmd & cmd::RS == 0);
}

#[test]
fn failed_boot_noop_parks_the_controller() {
    // The controller comes up but never answers the boot No-Op (DMA lost somewhere): the abort works, yet the
    // controller must not stay Running with bus master on.
    let (c, env) = ctrl(Cfg::minipc());
    env.model().drop_commands = 1;
    let e = c.start_checked().unwrap_err();
    assert!(e.contains("boot No-Op failed"), "{e}");
    assert_eq!(c.state(), State::Failed);
    assert!(c.reason().contains("boot No-Op failed: Timeout"), "{}", c.reason());
    let m = env.model();
    assert!(!m.bus_master && m.usbcmd & cmd::RS == 0);
}

#[test]
fn boot_noop_ok() {
    let (c, env) = ctrl(Cfg::qemu());
    c.start_checked().unwrap();
    assert_eq!(c.state(), State::Running);
    assert_eq!(env.model().commands_seen, 1);
}

#[test]
fn late_command_aborted_does_not_complete_the_next_command() {
    // After an abort the ring starts over at its base, so the next command sits at the aborted one's address.
    // A Command Aborted event for that address arriving late is stray, a real completion still matches.
    let (c, env) = up(Cfg::minipc());
    let base = env.model().crcr_ptr;
    c.cmd.lock().as_mut().unwrap().pending = Some(base);
    let ev = |code: u8| Trb { param: base, status: (code as u32) << 24, control: (ty::COMMAND_COMPLETION as u32) << 10 };
    env.model().post(ev(cc::COMMAND_ABORTED));
    assert_eq!(c.process_events(crate::hc::Source::Thread), 1);
    assert!(c.cmd.lock().as_ref().unwrap().done.is_none(), "Command Aborted completed a waiting command");
    assert_eq!(c.stats.stray_completions.load(Ordering::Relaxed), 1);
    env.model().post(ev(cc::SUCCESS));
    c.process_events(crate::hc::Source::Thread);
    let g = c.cmd.lock();
    let st = g.as_ref().unwrap();
    assert_eq!(st.done.map(|d| d.code), Some(cc::SUCCESS));
    assert!(st.pending.is_none());
}

// --- rings on their own

fn seg(env: &FakeEnv, trbs: usize) -> FakeMem {
    FakeMem::new(env, trbs * 16)
}

#[test]
fn producer_ring_links_and_cycle() {
    let env = FakeEnv::new(Cfg::qemu());
    let (a, b) = (seg(&env, 4), seg(&env, 4));
    let (aa, ba) = (a.dev_addr(), b.dev_addr());
    let mut r = ProducerRing::new(std::vec![a, b]).unwrap();
    assert_eq!(r.capacity(), 5);
    let m = || env.model();
    let l0 = m().rd_trb(aa + 48).unwrap();
    let l1 = m().rd_trb(ba + 48).unwrap();
    assert_eq!((l0.trb_type(), l0.param, l0.control & LINK_TC, l0.cycle()), (ty::LINK, ba, 0, false));
    assert_eq!((l1.trb_type(), l1.param, l1.control & LINK_TC, l1.cycle()), (ty::LINK, aa, LINK_TC, false));
    let mut addrs = Vec::new();
    for _ in 0..3 {
        addrs.push(r.push(Trb::noop_command()).unwrap());
    }
    assert_eq!(addrs, std::vec![aa, aa + 16, aa + 32]);
    assert!(m().rd_trb(aa + 48).unwrap().cycle(), "link handed over on the first pass with cycle 1");
    assert!(r.contains(aa + 16) && !r.contains(aa + 48) && !r.contains(aa + 8));
    for _ in 0..2 {
        r.push(Trb::noop_command()).unwrap();
    }
    assert_eq!(r.push(Trb::noop_command()), Err(RingError::Full));
    r.consumed(5);
    r.push(Trb::noop_command()).unwrap();
    assert!(!r.cycle(), "toggled at the last segment's link");
    assert_eq!(r.wraps(), 1);
    let t = r.push(Trb::noop_command()).unwrap();
    assert_eq!(t, aa);
    assert!(!m().rd_trb(aa).unwrap().cycle(), "second pass writes cycle 0");
    // init empties it again.
    r.init().unwrap();
    assert!(r.cycle());
    assert_eq!(m().rd_trb(aa).unwrap(), Trb::default());
    assert!(!m().rd_trb(aa + 48).unwrap().cycle());
}

#[test]
fn producer_ring_refuses_bad_segments() {
    let env = FakeEnv::new(Cfg::qemu());
    assert!(ProducerRing::<FakeMem>::new(Vec::new()).is_err());
    assert!(ProducerRing::new(std::vec![seg(&env, 1)]).is_err());
    assert!(ProducerRing::new(std::vec![seg(&env, 4), seg(&env, 8)]).is_err());
}

#[test]
fn event_ring_two_segments() {
    let env = FakeEnv::new(Cfg::qemu());
    let (a, b) = (seg(&env, 16), seg(&env, 16));
    let (aa, ba) = (a.dev_addr(), b.dev_addr());
    let erst = FakeMem::new(&env, 32);
    let mut r = EventRing::new(std::vec![a, b], erst).unwrap();
    assert_eq!(r.capacity(), 32);
    {
        let mut m = env.model();
        assert_eq!(m.rd_mem64(r.erst_base()), Some(aa));
        assert_eq!(m.rd_mem64(r.erst_base() + 16), Some(ba));
        assert_eq!(m.rd_mem32(r.erst_base() + 24), Some(16));
    }
    let put = |addr: u64, n: u32, cycle: bool| {
        let mut m = env.model();
        assert!(m.wr_mem32(addr, n) && m.wr_mem32(addr + 12, (ty::PORT_STATUS_CHANGE as u32) << 10 | cycle as u32));
    };
    assert_eq!(r.pop(), Ok(None));
    for round in 0..3u32 {
        let cyc = round % 2 == 0;
        for i in 0..32u32 {
            let at = if i < 16 { aa } else { ba } + (i as u64 % 16) * 16;
            put(at, round * 100 + i, cyc);
            assert!(r.pending().unwrap());
            let t = r.pop().unwrap().unwrap();
            assert_eq!(t.param as u32, round * 100 + i);
            assert_eq!(r.pop(), Ok(None), "the next slot still has the old cycle");
        }
        assert_eq!(r.dequeue_ptr(), aa);
        assert_eq!(r.wraps(), round as u64 + 1);
    }
    let _ = CYCLE;
}

#[test]
fn event_ring_refuses_short_erst() {
    let env = FakeEnv::new(Cfg::qemu());
    assert!(EventRing::new(std::vec![seg(&env, 16), seg(&env, 16)], FakeMem::new(&env, 16)).is_err());
    assert!(EventRing::new(std::vec![seg(&env, 8)], FakeMem::new(&env, 16)).is_err());
}

#[test]
fn model_layout_is_what_the_parser_reads() {
    let env = FakeEnv::new(Cfg::minipc());
    let caps = Caps::parse(|o| env.read32(o), env.bar_len()).unwrap();
    assert_eq!((caps.caplength, caps.rtsoff, caps.dboff, caps.xecp), (CAPLEN, RTS, DB, XECP));
    let _ = crcr::CA;
}
