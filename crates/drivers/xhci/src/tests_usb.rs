//! US-2 host tests: root ports, enumeration, hubs, hotplug and detach against the fake controller with fake
//! devices (`fake_usb.rs`). The registry is global and tests run in parallel, so every test uses its own
//! controller name and looks only at its own devices; test drivers match test-only vendor IDs.

use crate::fake::{Cfg, FakeEnv};
use crate::fake_usb::FakeDev;
use crate::hc::{Controller, Env, Source, State, MS};
use crate::hotplug::Hotplug;
use crate::regs::{walk_ext_caps, Caps};
use crate::slot::slot_state;
use crate::trb::ty;
use alloc::string::String;
use alloc::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::vec;
use std::vec::Vec;
use usb_core::{hub, registry, Data, Device, Interface, PipeEvent, Setup, Speed, UsbDriver, UsbError, UsbMatch};

pub(crate) type C = Arc<Controller<FakeEnv>>;

pub(crate) fn up(name: &str, cfg: Cfg, setup: impl FnOnce(&mut crate::fake::Model)) -> (C, FakeEnv, Hotplug) {
    let env = FakeEnv::new(cfg.bare());
    setup(&mut env.model());
    let caps = Caps::parse(|o| env.read32(o), env.bar_len()).unwrap();
    let ext = walk_ext_caps(|o| env.read32(o), caps.xecp, env.bar_len(), caps.max_ports);
    let c = Arc::new(Controller::new(String::from(name), env.clone(), caps, ext));
    c.start().expect("start");
    let mut hp = Hotplug::new();
    settle(&c, &env, &mut hp);
    (c, env, hp)
}

/// Runs hotplug rounds, advancing the fake clock to each deadline, until nothing is pending.
pub(crate) fn settle(c: &C, env: &FakeEnv, hp: &mut Hotplug) {
    for _ in 0..2000 {
        c.process_events(Source::Waiter);
        let next = hp.round(c);
        c.process_events(Source::Waiter);
        if c.settled() && next.is_none() && !c.take_kick() {
            return;
        }
        let now = env.model().time;
        env.model().time = next.unwrap_or(now + MS).max(now + MS);
    }
    panic!("hotplug did not settle");
}

pub(crate) fn devs(c: &C) -> Vec<Arc<Device>> {
    let mut v: Vec<_> = registry::devices().into_iter().filter(|d| d.host_name() == c.name()).collect();
    v.sort_by_key(|d| (d.place.root_port, d.place.route));
    v
}

pub(crate) fn errors(env: &FakeEnv) -> Vec<String> {
    env.model().usb.errors.clone()
}

fn with_id(mut d: FakeDev, vid: u16) -> FakeDev {
    d.device[8..10].copy_from_slice(&vid.to_le_bytes());
    d
}

#[test]
fn minipc_root_devices_enumerate() {
    // 64-byte contexts; ports 1-3 USB 2, 4-7 USB 3.
    let (c, env, _hp) = up("t-minipc", Cfg::minipc(), |m| {
        m.attach_root(1, FakeDev::keyboard(Speed::Full));
        m.attach_root(2, FakeDev::storage(Speed::High));
        m.attach_root(5, FakeDev::storage(Speed::Super));
    });
    let d = devs(&c);
    assert_eq!(d.len(), 3, "{d:?}");
    assert_eq!(errors(&env), Vec::<String>::new());
    assert_eq!((d[0].speed(), d[1].speed(), d[2].speed()), (Speed::Full, Speed::High, Speed::Super));
    for x in &d {
        assert!(x.config.is_some(), "{x:?} configured");
        let i = c.slot_info(x.slot).unwrap();
        assert_eq!(i.state, slot_state::CONFIGURED, "{x:?}");
        assert!(i.dcbaa_ok);
        assert_eq!((i.root_port, i.route, i.address), (x.place.root_port, 0, x.address));
    }
    let m = env.model();
    // The keyboard's endpoint 0 is 8 bytes: Evaluate Context with the whole output context copied (dequeue
    // pointer included) and the new size.
    assert_eq!(m.usb.evaluated.len(), 1, "only the full speed keyboard needs the fix");
    let (inp, out) = m.usb.evaluated[0];
    assert_eq!(inp[1] >> 16, 8);
    assert_eq!((inp[2], inp[3], inp[4]), (out[2], out[3], out[4]), "whole EP0 context copied");
    assert_ne!(inp[2] & !0xF, 0);
    assert!(!m.ped_written, "PED never written");
    assert!(m.usb.root.values().all(|dv| dv.configured == 1));
    drop(m);
    let i = c.slot_info(d[0].slot).unwrap();
    assert_eq!(i.mps0, 8);
    assert_eq!(i.entries, 3, "interrupt IN 0x81 = DCI 3");
    let s = c.slot_info(d[2].slot).unwrap();
    assert_eq!(s.entries, 4, "bulk 0x81 (DCI 3) and 0x02 (DCI 4)");
    assert_eq!(s.speed, 4);
}

#[test]
fn qemu_hub_tree_routes_and_hub_slots() {
    let (c, env, _hp) = up("t-qtree", Cfg::qemu(), |m| {
        let mut h = FakeDev::hub(Speed::Full, 8, false);
        h.plug(1, FakeDev::keyboard(Speed::Full));
        h.plug(3, FakeDev::storage(Speed::Full));
        let mut inner = FakeDev::hub(Speed::Full, 4, false);
        inner.plug(2, FakeDev::keyboard(Speed::Full));
        h.plug(4, inner);
        m.attach_root(1, h);
        m.attach_root(2, FakeDev::keyboard(Speed::Full));
    });
    assert_eq!(errors(&env), Vec::<String>::new());
    let d = devs(&c);
    let routes: Vec<(u8, u32, u8)> = d.iter().map(|x| (x.place.root_port, x.place.route, x.place.depth)).collect();
    assert_eq!(routes, vec![(1, 0, 0), (1, 0x1, 1), (1, 0x3, 1), (1, 0x4, 1), (1, 0x24, 2), (2, 0, 0)]);
    assert!(d.iter().all(|x| x.place.tt.is_none()), "full speed below full speed hubs on a root port: no TT");
    let h = c.slot_info(d[0].slot).unwrap();
    assert!(h.hub && h.ports == 8 && !h.mtt);
    let inner = c.slot_info(d[3].slot).unwrap();
    assert!(inner.hub && inner.ports == 4);
    assert_eq!(c.slot_info(d[4].slot).unwrap().route, 0x24);
    assert_eq!(hub::hubs().iter().filter(|h| h.dev.host_name() == c.name()).count(), 2);
    assert_eq!(d[4].path(), "t-qtree-1.4.2");
}

#[test]
fn high_speed_hub_tt_and_multi_tt() {
    let (c, env, _hp) = up("t-tt", Cfg::minipc(), |m| {
        let mut h = FakeDev::hub(Speed::High, 4, true);
        h.plug(2, FakeDev::keyboard(Speed::Low));
        let mut fs = FakeDev::hub(Speed::Full, 4, false);
        fs.plug(1, FakeDev::keyboard(Speed::Full));
        h.plug(3, fs);
        h.plug(4, FakeDev::storage(Speed::High));
        m.attach_root(2, h);
    });
    // The model checks every TT against the nearest high speed hub (`World::errors`).
    assert_eq!(errors(&env), Vec::<String>::new());
    let d = devs(&c);
    assert_eq!(d.len(), 5, "{d:?}");
    let hub_slot = (d[0].slot.0 & 0xFF) as u8;
    let ls = d.iter().find(|x| x.speed() == Speed::Low).unwrap();
    let li = c.slot_info(ls.slot).unwrap();
    assert_eq!((li.tt_slot, li.tt_port, li.mtt), (hub_slot, 2, true));
    let deep = d.iter().find(|x| x.place.depth == 2).unwrap();
    let di = c.slot_info(deep.slot).unwrap();
    assert_eq!((di.tt_slot, di.tt_port), (hub_slot, 3), "inherited from the high speed hub");
    let hs = d.iter().find(|x| x.speed() == Speed::High && x.place.depth == 1).unwrap();
    assert_eq!(c.slot_info(hs.slot).unwrap().tt_slot, 0, "high speed needs no TT");
    let hi = c.slot_info(d[0].slot).unwrap();
    assert!(hi.hub && hi.mtt, "multi-TT hub");
    let mut m = env.model();
    assert_eq!(m.dev(&[2]).unwrap().alt, 1, "alternate setting 1 selects one TT per port");
    let out = m.usb.slots[&hub_slot].out;
    assert_eq!((m.rd_mem32(out + 8).unwrap() >> 16) & 3, 2, "TT think time from wHubCharacteristics");
}

#[test]
fn superspeed_hubs_get_their_depth() {
    let (c, env, _hp) = up("t-ss", Cfg::qemu(), |m| {
        let mut h = FakeDev::hub(Speed::Super, 4, false);
        let mut h2 = FakeDev::hub(Speed::Super, 4, false);
        h2.plug(1, FakeDev::storage(Speed::Super));
        h.plug(2, h2);
        h.plug(1, FakeDev::storage(Speed::Super));
        m.attach_root(6, h);
    });
    assert_eq!(errors(&env), Vec::<String>::new());
    assert_eq!(devs(&c).len(), 4);
    let mut m = env.model();
    assert_eq!(m.dev(&[6]).unwrap().hub_depth, Some(0));
    assert_eq!(m.dev(&[6, 2]).unwrap().hub_depth, Some(1));
    assert!(m.dev(&[6, 2, 1]).unwrap().configured == 1);
}

macro_rules! recorder {
    ($name:ident, $vid:expr) => {
        mod $name {
            use super::*;
            pub static LOG: StdMutex<Vec<String>> = StdMutex::new(Vec::new());
            static M: [UsbMatch; 1] = [UsbMatch::Device { vendor: $vid, product: 0x0001 }];
            fn probe(d: &Arc<Device>, f: &Interface) -> Result<(), String> {
                LOG.lock().unwrap().push(alloc::format!("probe {}", d.id));
                let ep = f.endpoints[0].address;
                let sink = Arc::new(|e: PipeEvent<'_>| {
                    let s = match e {
                        PipeEvent::Data(b) => alloc::format!("data {b:?}"),
                        PipeEvent::Error(x) => alloc::format!("error {x:?}"),
                        PipeEvent::Stopped(x) => alloc::format!("stopped {x:?}"),
                    };
                    LOG.lock().unwrap().push(s);
                });
                d.interrupt_in(ep, 8, sink).map_err(|e| String::from(e.as_str()))
            }
            fn disconnect(d: &Arc<Device>, f: u8) {
                LOG.lock().unwrap().push(alloc::format!("disconnect {} {f}", d.id));
            }
            pub static DRIVER: UsbDriver = UsbDriver { name: stringify!($name), matches: &M, probe, disconnect };
            pub fn log() -> Vec<String> {
                LOG.lock().unwrap().clone()
            }
        }
    };
}

recorder!(drv_root, 0x7A01);
recorder!(drv_hub, 0x7A02);
recorder!(drv_restart, 0x7A03);

/// A driver whose probe sees its device detached under it (as when the hotplug worker detaches while a driver
/// registered at run time probes): it must still hear `disconnect`, and no binding may stay behind.
mod drv_race {
    use super::*;
    pub static LOG: StdMutex<Vec<String>> = StdMutex::new(Vec::new());
    static M: [UsbMatch; 1] = [UsbMatch::Device { vendor: 0x7A04, product: 0x0001 }];
    fn probe(d: &Arc<Device>, _: &Interface) -> Result<(), String> {
        LOG.lock().unwrap().push(alloc::format!("probe {}", d.id));
        registry::detach(d);
        Ok(())
    }
    fn disconnect(d: &Arc<Device>, f: u8) {
        LOG.lock().unwrap().push(alloc::format!("disconnect {} {f}", d.id));
    }
    pub static DRIVER: UsbDriver = UsbDriver { name: "drv_race", matches: &M, probe, disconnect };
}

#[test]
fn unplug_while_a_key_is_held_and_replug() {
    registry::register_driver(&drv_root::DRIVER);
    let (c, env, mut hp) = up("t-unplug", Cfg::qemu(), |m| m.attach_root(2, with_id(FakeDev::keyboard(Speed::Full), 0x7A01)));
    let d = devs(&c);
    assert_eq!(d.len(), 1);
    assert_eq!(registry::bindings_of(d[0].id), vec![(0, "drv_root")]);
    // A key goes down: the report arrives through the pipe, and the pipe is armed again.
    env.model().dev(&[2]).unwrap().reports.push_back(vec![0, 0, 4, 0, 0, 0, 0, 0]);
    env.model().pump();
    c.process_events(Source::Waiter);
    assert!(drv_root::log().contains(&String::from("data [0, 0, 4, 0, 0, 0, 0, 0]")), "{:?}", drv_root::log());
    // Pulled while held: disconnect once, the pipe stopped, slot disabled, DCBAA cleared, nothing left.
    let slot = (d[0].slot.0 & 0xFF) as u8;
    env.model().detach_root(2);
    settle(&c, &env, &mut hp);
    let log = drv_root::log();
    assert_eq!(log.iter().filter(|l| l.starts_with("disconnect")).count(), 1, "{log:?}");
    assert!(log.contains(&String::from("stopped Gone")), "{log:?}");
    assert!(devs(&c).is_empty());
    assert!(d[0].is_gone());
    assert!(c.live_slots().is_empty());
    assert!(env.model().usb.slots.is_empty(), "Disable Slot reached the controller");
    assert_eq!(c.dcbaa(slot), Some(0));
    assert_eq!(d[0].control(Setup::get_descriptor(1, 0, 8), Data::In(&mut [0; 8])), Err(UsbError::Gone));
    // Plugged in again: a new device, bound again.
    env.model().attach_root(2, with_id(FakeDev::keyboard(Speed::Full), 0x7A01));
    settle(&c, &env, &mut hp);
    let d2 = devs(&c);
    assert_eq!(d2.len(), 1);
    assert_ne!(d2[0].id, d[0].id);
    assert_eq!(drv_root::log().iter().filter(|l| l.starts_with("probe")).count(), 2);
    assert_eq!(errors(&env), Vec::<String>::new());
    assert_eq!(c.state(), State::Running);
}

#[test]
fn unplug_behind_a_hub_and_the_hub_itself() {
    registry::register_driver(&drv_hub::DRIVER);
    let (c, env, mut hp) = up("t-hubplug", Cfg::qemu(), |m| {
        let mut h = FakeDev::hub(Speed::Full, 4, false);
        h.plug(2, with_id(FakeDev::keyboard(Speed::Full), 0x7A02));
        h.plug(3, FakeDev::storage(Speed::Full));
        m.attach_root(1, h);
    });
    assert_eq!(devs(&c).len(), 3);
    env.model().hub_port(&[1], 2, None);
    settle(&c, &env, &mut hp);
    assert_eq!(devs(&c).len(), 2, "keyboard gone");
    assert_eq!(drv_hub::log().iter().filter(|l| l.starts_with("disconnect")).count(), 1);
    // Plugged into another port of the hub.
    env.model().hub_port(&[1], 4, Some(with_id(FakeDev::keyboard(Speed::Full), 0x7A02)));
    settle(&c, &env, &mut hp);
    let d = devs(&c);
    assert_eq!(d.len(), 3);
    assert!(d.iter().any(|x| x.place.route == 0x4));
    // The hub goes: children first, then the hub; every slot disabled.
    env.model().detach_root(1);
    settle(&c, &env, &mut hp);
    assert!(devs(&c).is_empty());
    assert!(c.live_slots().is_empty());
    assert!(env.model().usb.slots.is_empty());
    assert!(hub::hubs().iter().all(|h| h.dev.host_name() != c.name()));
    assert_eq!(errors(&env), Vec::<String>::new());
}

#[test]
fn a_device_failing_once_gets_its_second_try() {
    let (c, env, _hp) = up("t-retry", Cfg::qemu(), |m| {
        let mut k = FakeDev::keyboard(Speed::Full);
        k.fail_addresses = 1;
        m.attach_root(1, k);
    });
    assert_eq!(devs(&c).len(), 1);
    let m = env.model();
    assert_eq!(m.usb.commands.iter().filter(|&&t| t == ty::ENABLE_SLOT).count(), 2);
    assert_eq!(m.usb.commands.iter().filter(|&&t| t == ty::DISABLE_SLOT).count(), 1, "the failed slot freed");
    assert!(m.usb.slots.len() == 1);
}

#[test]
fn a_hub_port_whose_device_always_fails_is_switched_off() {
    let (c, env, _hp) = up("t-giveup", Cfg::qemu(), |m| {
        let mut h = FakeDev::hub(Speed::Full, 4, false);
        let mut k = FakeDev::keyboard(Speed::Full);
        k.fail_addresses = 10;
        h.plug(1, k);
        m.attach_root(1, h);
    });
    assert_eq!(devs(&c).len(), 1, "only the hub");
    let mut m = env.model();
    let p = &m.dev(&[1]).unwrap().hub.as_ref().unwrap().ports[0];
    assert!(!p.enabled, "PORT_ENABLE cleared");
    assert_eq!(m.usb.slots.len(), 1, "no slot left behind");
}

#[test]
fn a_device_that_never_answers_times_out_and_endpoint_0_is_restarted() {
    let (c, env, _hp) = up("t-hang", Cfg::qemu(), |m| m.attach_root(1, FakeDev::keyboard(Speed::Full).with_hang()));
    assert!(devs(&c).is_empty());
    let m = env.model();
    assert!(m.usb.commands.contains(&ty::STOP_ENDPOINT), "running endpoint 0 stopped");
    assert!(m.usb.commands.contains(&ty::SET_TR_DEQUEUE), "dequeue past the stale TD");
    assert!(m.usb.slots.is_empty(), "both tries' slots disabled");
    drop(m);
    assert_eq!(c.state(), State::Running);
    assert!(c.noop().is_ok());
}

#[test]
fn stall_halts_endpoint_0_and_the_next_request_works() {
    let (c, env, _hp) = up("t-stall", Cfg::qemu(), |m| {
        let mut k = FakeDev::keyboard(Speed::Full);
        k.stall.push(0x0A); // GET_INTERFACE
        m.attach_root(1, k);
    });
    let d = devs(&c).pop().unwrap();
    let mut b = [0u8; 1];
    assert_eq!(d.control_in(Setup::new(0x81, 0x0A, 0, 0, 1), &mut b), Err(UsbError::Stall));
    assert!(env.model().usb.commands.contains(&ty::RESET_ENDPOINT));
    let mut dd = [0u8; 18];
    assert_eq!(d.control_in(Setup::get_descriptor(1, 0, 18), &mut dd), Ok(18));
    assert_eq!(&dd[8..10], &[0x27, 0x06]);
}

#[test]
fn endpoint_0_ring_wraps_past_its_link() {
    let (c, env, _hp) = up("t-wrap", Cfg::minipc(), |m| m.attach_root(2, FakeDev::storage(Speed::High)));
    let d = devs(&c).pop().unwrap();
    // 255 TRBs per segment, 3 per transfer: 300 transfers pass the Link TRB three times.
    for i in 0..300 {
        let mut b = [0u8; 18];
        assert_eq!(d.control_in(Setup::get_descriptor(1, 0, 18), &mut b), Ok(18), "transfer {i}");
    }
    let mut b = [0u8; 4];
    assert_eq!(d.control_in(Setup::new(0x80, 0, 0, 0, 2), &mut b[..2]), Err(UsbError::Stall), "unknown request stalls");
    assert_eq!(errors(&env), Vec::<String>::new());
}

#[test]
fn restart_drops_every_device_and_scans_again() {
    registry::register_driver(&drv_restart::DRIVER);
    let (c, env, mut hp) = up("t-restart", Cfg::qemu(), |m| {
        m.attach_root(1, with_id(FakeDev::keyboard(Speed::Full), 0x7A03));
        m.attach_root(6, FakeDev::storage(Speed::Super));
    });
    let before = devs(&c);
    assert_eq!(before.len(), 2);
    c.quiesce();
    settle(&c, &env, &mut hp);
    assert!(devs(&c).is_empty(), "a stopped controller loses its devices");
    c.restart("test").unwrap();
    settle(&c, &env, &mut hp);
    let after = devs(&c);
    assert_eq!(after.len(), 2);
    assert!(after.iter().all(|a| before.iter().all(|b| a.id != b.id)));
    // A handle from before the restart never reaches the slot of the same number now.
    assert_eq!(c.slot_info(before[0].slot), None);
    assert_eq!(drv_restart::log().iter().filter(|l| l.starts_with("disconnect")).count(), 1);
}

#[test]
fn superspeed_link_stuck_gets_a_warm_reset() {
    let (c, env, _hp) = up("t-warm", Cfg::qemu(), |m| {
        m.usb.stuck_link.push(7);
        m.attach_root(7, FakeDev::storage(Speed::Super));
    });
    assert_eq!(devs(&c).len(), 1);
    assert!(env.model().usb.stuck_link.is_empty(), "WPR written");
    assert!(env.model().writes.iter().any(|w| w.1 & crate::regs::portsc::WPR != 0));
}

#[test]
fn hub_power_on_waits_and_debounce_holds_attach() {
    let (c, env, _hp) = up("t-timing", Cfg::qemu(), |m| m.attach_root(1, FakeDev::keyboard(Speed::Full)));
    let d = devs(&c);
    assert_eq!(d.len(), 1);
    // The first request reached the device at least 100 ms (debounce) after the controller came up.
    assert!(env.model().time >= 100 * MS);
}

#[test]
fn a_device_late_after_the_reset_is_waited_for() {
    // QEMU shape (no port power switching): the first look would come right after start; the device that was
    // there at takeover shows CCS again only 60 ms after HCRST, without an event.
    let (c, env, _hp) = up("t-late", Cfg::qemu(), |m| {
        m.usb.late.push((2, 60 * MS));
        m.attach_root(2, FakeDev::keyboard(Speed::Full));
    });
    assert_eq!(c.ports_at_takeover(), vec![2]);
    assert_eq!(devs(&c).len(), 1, "found after the late-device wait");
    assert_eq!(errors(&env), Vec::<String>::new());
}

#[test]
fn a_device_detached_during_probe_is_disconnected_and_not_bound() {
    registry::register_driver(&drv_race::DRIVER);
    let (c, env, _hp) = up("t-race", Cfg::qemu(), |m| m.attach_root(1, with_id(FakeDev::keyboard(Speed::Full), 0x7A04)));
    assert!(devs(&c).is_empty(), "detached");
    let log = drv_race::LOG.lock().unwrap().clone();
    assert_eq!(log.len(), 2, "{log:?}");
    let id: u64 = log[0].strip_prefix("probe ").unwrap().parse().unwrap();
    assert_eq!(log[1], alloc::format!("disconnect {id} 0"), "disconnect after the probe that won the race");
    assert!(registry::bindings_of(id).is_empty(), "no binding for a gone device");
    assert!(c.live_slots().is_empty());
    assert!(env.model().usb.slots.is_empty());
}

#[test]
fn a_device_failing_set_address_after_reset_gets_the_deferred_address() {
    // Fails every SET_ADDRESS that comes straight after the reset; works when the descriptor was read at address 0
    // first (Address Device BSR 1, GET_DESCRIPTOR 8, Evaluate Context, Address Device BSR 0 on the same slot).
    let (c, env, _hp) = up("t-bsr", Cfg::minipc(), |m| {
        let mut k = FakeDev::keyboard(Speed::Full);
        k.fail_direct_addresses = 10;
        m.attach_root(1, k);
    });
    let d = devs(&c);
    assert_eq!(d.len(), 1, "enumerated on the second try");
    assert_eq!(errors(&env), Vec::<String>::new());
    let m = env.model();
    let a = &m.usb.addresses;
    assert_eq!(a.len(), 3, "{a:?}");
    assert!(!a[0].1, "first try: BSR 0");
    assert_eq!((a[1].1, a[2].1, a[1].0), (true, false, a[2].0), "second try: BSR 1, then BSR 0 on the same slot");
    assert_eq!(m.usb.evaluated.len(), 1, "endpoint 0 fixed to 8 bytes between the two");
    drop(m);
    let i = c.slot_info(d[0].slot).unwrap();
    assert_eq!((i.state, i.mps0), (slot_state::CONFIGURED, 8));
    assert_ne!(i.address, 0);
    assert_eq!(d[0].address, i.address);
}

#[test]
fn endpoint_0_recovers_when_its_restart_failed_before() {
    let (c, env, _hp) = up("t-ep0stuck", Cfg::qemu(), |m| {
        let mut k = FakeDev::keyboard(Speed::Full);
        k.stall.push(0x0A);
        m.attach_root(1, k);
    });
    let d = devs(&c).pop().unwrap();
    // The STALL halts endpoint 0 and every restart command fails: the TRBs stay counted until the ring is full.
    env.model().usb.fail_ep_commands = usize::MAX;
    let mut b = [0u8; 1];
    assert_eq!(d.control_in(Setup::new(0x81, 0x0A, 0, 0, 1), &mut b), Err(UsbError::Stall));
    let mut busy = false;
    for _ in 0..200 {
        let mut dd = [0u8; 18];
        match d.control_in(Setup::get_descriptor(1, 0, 18), &mut dd) {
            Err(UsbError::Timeout) => {}
            Err(UsbError::Busy) => {
                busy = true;
                break;
            }
            r => panic!("unexpected {r:?}"),
        }
    }
    assert!(busy, "ring full while the endpoint does not restart");
    // The controller answers again: the next transfer restarts endpoint 0 first and works.
    env.model().usb.fail_ep_commands = 0;
    let mut dd = [0u8; 18];
    assert_eq!(d.control_in(Setup::get_descriptor(1, 0, 18), &mut dd), Ok(18));
    assert_eq!(d.control_in(Setup::get_descriptor(1, 0, 18), &mut dd), Ok(18));
    assert_eq!(errors(&env), Vec::<String>::new());
}

#[test]
fn an_inactive_superspeed_link_gets_its_warm_reset_at_once() {
    let (c1, e1, _h1) = up("t-inactive", Cfg::qemu(), |m| {
        m.usb.inactive_link.push(7);
        m.attach_root(7, FakeDev::storage(Speed::Super));
    });
    let (c2, e2, _h2) = up("t-rxdetect", Cfg::qemu(), |m| {
        m.usb.stuck_link.push(7);
        m.attach_root(7, FakeDev::storage(Speed::Super));
    });
    assert_eq!((devs(&c1).len(), devs(&c2).len()), (1, 1));
    assert!(e1.model().usb.inactive_link.is_empty(), "WPR written");
    let (t1, t2) = (e1.model().time, e2.model().time);
    assert!(t1 + 400 * MS < t2, "SS.Inactive: no 500 ms wait for U0 first ({t1} vs {t2})");
}

#[test]
fn a_superspeed_hub_with_zero_bytes_per_interval_configures() {
    // A companion with wBytesPerInterval 0 on the hub's status endpoint: the context still gets a payload.
    let (c, env, _hp) = up("t-ssbpi0", Cfg::qemu(), |m| {
        let mut h = FakeDev::hub(Speed::Super, 4, false);
        let i = h.config.windows(2).position(|w| w == [6, 0x30]).unwrap();
        h.config[i + 4] = 0;
        h.config[i + 5] = 0;
        h.plug(1, FakeDev::storage(Speed::Super));
        m.attach_root(6, h);
    });
    assert_eq!(errors(&env), Vec::<String>::new());
    assert_eq!(devs(&c).len(), 2, "hub configured, its device found");
}

#[test]
fn a_slot_id_in_use_from_enable_slot_leaves_the_live_device_alone() {
    let (c, env, mut hp) = up("t-badslot", Cfg::qemu(), |m| m.attach_root(1, FakeDev::keyboard(Speed::Full)));
    let first = devs(&c).pop().unwrap();
    let id = (first.slot.0 & 0xFF) as u8;
    // The controller hands out the keyboard's slot again for the next device.
    env.model().usb.bogus_slot = Some(id);
    env.model().attach_root(2, FakeDev::keyboard(Speed::Full));
    settle(&c, &env, &mut hp);
    assert_eq!(devs(&c).len(), 2, "the new device got a real slot on its second try");
    assert!(!env.model().usb.commands.contains(&ty::DISABLE_SLOT), "no Disable Slot for a slot in use");
    let mut dd = [0u8; 18];
    assert_eq!(first.control_in(Setup::get_descriptor(1, 0, 18), &mut dd), Ok(18), "the first device still works");
    assert_eq!(errors(&env), Vec::<String>::new());
}
