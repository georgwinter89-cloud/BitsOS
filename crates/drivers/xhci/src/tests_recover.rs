//! US-R host tests against the fake controller: a device reset in place (same `Device`, same slot, its transfers
//! ended, its configuration back, the same device checked), both resets of a SuperSpeed port, what ends in a detach
//! (another device, nobody there, a logical disconnect), a port given up on and tried again, the clean stop before a
//! machine reset with the port's power, the serial string, a stronger ask, and what the controller answers to "is this endpoint moving?".

use crate::fake::{Cfg, FakeEnv};
use crate::fake_dev::{FakeDev, Wedge};
use crate::hc::{Source, MS};
use crate::hotplug::Hotplug;
use crate::regs::portsc;
use crate::slot::slot_state;
use crate::tests_usb::{devs, errors, settle, up, C};
use crate::tests_xfer::{bot_read, cbw};
use crate::trb::ty;
use alloc::string::String;
use alloc::sync::Arc;
use std::vec;
use std::vec::Vec;
use usb_core::ports::RETRY_FIRST_NS;
use usb_core::{registry, Device, Host, Interface, Request, ResetJob, ResetKind, Setup, Speed, UsbDriver, UsbError, UsbMatch};

const OUT: u8 = 0x02;
const IN: u8 = 0x81;
const S: u64 = 1000 * MS;

/// Hotplug rounds until the job has its result.
fn run_job(c: &C, env: &FakeEnv, hp: &mut Hotplug, job: &Arc<ResetJob>) -> Result<(), UsbError> {
    for _ in 0..100 {
        c.process_events(Source::Waiter);
        let _ = hp.round(c);
        c.process_events(Source::Waiter);
        if let Some(r) = job.result() {
            return r;
        }
        env.model().time += MS;
    }
    panic!("the worker never ran the reset");
}

fn reset(c: &C, env: &FakeEnv, hp: &mut Hotplug, d: &Arc<Device>, kind: ResetKind) -> Result<(), UsbError> {
    let job = d.start_reset(kind).expect("asked");
    run_job(c, env, hp, &job)
}

fn commands(env: &FakeEnv, t: u8) -> usize {
    env.model().usb.commands.iter().filter(|&&x| x == t).count()
}

fn descriptor_works(d: &Device) {
    let mut b = [0u8; 18];
    assert_eq!(d.control_in(Setup::get_descriptor(1, 0, 18), &mut b), Ok(18));
}

#[test]
fn a_reset_in_place_keeps_device_slot_and_ends_its_transfers() {
    for (name, cfg, port, speed) in [("t-r-mini", Cfg::minipc(), 2, Speed::High), ("t-r-qemu", Cfg::qemu(), 5, Speed::Super)] {
        let (c, env, mut hp) = up(name, cfg, |m| m.attach_root(port, FakeDev::bot_storage(speed, 2048)));
        let d = devs(&c)[0].clone();
        let (id, slot, configures) = (d.id, d.slot, env.model().usb.configures.len());
        env.model().usb.port_resets.clear();
        env.model().dev(&[port]).unwrap().nak_out.push(OUT);
        let buf = d.alloc_buffer(512).unwrap();
        let t = d.submit(OUT, Request::new(buf.clone(), 0, 512)).unwrap();
        assert!(!t.is_done());

        reset(&c, &env, &mut hp, &d, ResetKind::Recover).unwrap();

        assert_eq!(t.result().map(|r| r.error), Some(Some(UsbError::Cancelled)), "{name}: the transfer under way ended");
        assert!(!d.is_gone());
        assert_eq!(devs(&c).iter().map(|x| x.id).collect::<Vec<_>>(), [id], "{name}: the same device, nothing new");
        assert_eq!(d.slot, slot);
        let i = c.slot_info(slot).expect("the slot lives");
        assert_eq!(i.state, slot_state::CONFIGURED);
        assert_eq!(i.address, d.address());
        assert_ne!(i.address, 0);
        assert_eq!(commands(&env, ty::RESET_DEVICE), 1);
        assert_eq!(commands(&env, ty::DISABLE_SLOT), 0, "{name}: the slot was never given up");
        let m = env.model();
        assert_eq!(m.usb.port_resets, [(port, false)], "{name}: a real reset, the hot one on SuperSpeed");
        assert_eq!(m.usb.addresses.last(), Some(&(slot.0 as u8, false)), "{name}: Address Device with BSR 0");
        assert_eq!(m.usb.configures.len(), configures + 1);
        drop(m);
        // Its endpoints work again, the old TD is not run (it would be a broken CBW), its buffer is free.
        env.model().dev(&[port]).unwrap().nak_out.clear();
        assert_eq!(bot_read(&d, &buf, 3, 1), 512, "{name}");
        assert_eq!(errors(&env), Vec::<String>::new(), "{name}");
        assert_eq!(c.state(), crate::hc::State::Running);
    }
}

#[test]
fn nothing_gets_through_to_the_device_during_its_reset() {
    let (c, env, mut hp) = up("t-r-busy", Cfg::qemu(), |m| m.attach_root(5, FakeDev::bot_storage(Speed::Super, 2048)));
    let d = devs(&c)[0].clone();
    // Mid-reset (the Reset Device command is about to run) a control transfer of someone else is refused.
    let d2 = d.clone();
    let buf = d.alloc_buffer(8).unwrap();
    env.model().usb.before_cmd = Some((ty::RESET_DEVICE, std::boxed::Box::new(move |_m| {
        assert_eq!(d2.control_out(Setup::set_configuration(1), &[]), Err(UsbError::Busy));
        assert_eq!(d2.submit(OUT, Request::new(buf, 0, 8)).err(), Some(UsbError::Busy));
    })));
    reset(&c, &env, &mut hp, &d, ResetKind::Recover).unwrap();
    assert_eq!(commands(&env, ty::RESET_DEVICE), 1);
    descriptor_works(&d);
}

#[test]
fn a_superspeed_port_gets_its_warm_reset_as_the_second_try() {
    let (c, env, mut hp) = up("t-r-warm", Cfg::qemu(), |m| m.attach_root(5, FakeDev::bot_storage(Speed::Super, 2048)));
    let d = devs(&c)[0].clone();
    env.model().dev(&[5]).unwrap().wedged = Some(Wedge::UntilWarm);
    reset(&c, &env, &mut hp, &d, ResetKind::Recover).unwrap();
    assert_eq!(env.model().usb.port_resets, [(5, false), (5, true)]);
    assert!(env.model().usb.commands.iter().filter(|&&t| t == ty::RESET_DEVICE).count() >= 1);
    assert!(!d.is_gone());
    descriptor_works(&d);
    assert_eq!(errors(&env), Vec::<String>::new());
}

#[test]
fn a_reset_that_fails_keeps_the_device_for_the_next_one() {
    let (c, env, mut hp) = up("t-r-fail", Cfg::qemu(), |m| m.attach_root(5, FakeDev::bot_storage(Speed::Super, 2048)));
    let d = devs(&c)[0].clone();
    env.model().dev(&[5]).unwrap().fail_addresses = 2;
    assert_eq!(reset(&c, &env, &mut hp, &d, ResetKind::Recover), Err(UsbError::Command(4)));
    assert!(!d.is_gone(), "the asker decides what comes next");
    assert_eq!(devs(&c).len(), 1);
    reset(&c, &env, &mut hp, &d, ResetKind::Recover).unwrap();
    assert_eq!(c.slot_info(d.slot).unwrap().state, slot_state::CONFIGURED);
    descriptor_works(&d);
    assert_eq!(errors(&env), Vec::<String>::new());
}

#[test]
fn another_device_on_the_port_ends_in_a_detach_and_a_fresh_enumeration() {
    let (c, env, mut hp) = up("t-r-other", Cfg::qemu(), |m| m.attach_root(5, FakeDev::bot_storage(Speed::Super, 2048)));
    let d = devs(&c)[0].clone();
    env.model().dev(&[5]).unwrap().device[10] ^= 0xFF;
    assert_eq!(reset(&c, &env, &mut hp, &d, ResetKind::Recover), Err(UsbError::Gone));
    settle(&c, &env, &mut hp);
    assert!(d.is_gone());
    let now = devs(&c);
    assert_eq!(now.len(), 1);
    assert_ne!(now[0].id, d.id);
}

#[test]
fn a_stick_of_the_same_model_with_another_serial_ends_in_a_detach() {
    let serial = |text: u8| vec![4, 3, text, 0];
    let (c, env, mut hp) = up("t-r-serial", Cfg::qemu(), |m| {
        let mut d = FakeDev::bot_storage(Speed::Super, 2048);
        d.device[16] = 3;
        d.strings.insert(0, vec![4, 3, 0x09, 0x04]);
        d.strings.insert(3, serial(b'A'));
        m.attach_root(5, d)
    });
    let d = devs(&c)[0].clone();
    d.remember_serial();
    assert_eq!(reset(&c, &env, &mut hp, &d, ResetKind::Recover), Ok(()), "the same stick");
    assert_eq!(devs(&c)[0].id, d.id);
    env.model().dev(&[5]).unwrap().strings.insert(3, serial(b'B'));
    assert_eq!(reset(&c, &env, &mut hp, &d, ResetKind::Recover), Err(UsbError::Gone));
    settle(&c, &env, &mut hp);
    assert!(d.is_gone());
}

#[test]
fn a_stick_without_a_serial_string_is_compared_by_its_descriptors() {
    let (c, env, mut hp) = up("t-r-noserial", Cfg::qemu(), |m| {
        let mut d = FakeDev::bot_storage(Speed::Super, 2048);
        d.device[16] = 3;
        m.attach_root(5, d)
    });
    let d = devs(&c)[0].clone();
    d.remember_serial();
    assert_eq!(reset(&c, &env, &mut hp, &d, ResetKind::Recover), Ok(()));
}

#[test]
fn a_stronger_ask_is_not_lost_in_a_job_still_queued() {
    let (c, env, mut hp) = up("t-r-kinds", Cfg::qemu(), |m| m.attach_root(5, FakeDev::bot_storage(Speed::Super, 2048)));
    let d = devs(&c)[0].clone();
    let weak = d.start_reset(ResetKind::Recover).unwrap();
    let stop = d.start_reset(ResetKind::ForFirmware).unwrap();
    assert!(Arc::ptr_eq(&weak, &stop), "one job at a time");
    assert_eq!(weak.kind(), ResetKind::ForFirmware);
    assert_eq!(d.start_reset(ResetKind::Recover).unwrap().kind(), ResetKind::ForFirmware, "a weaker ask does not lower it");
    assert_eq!(run_job(&c, &env, &mut hp, &weak), Ok(()));
    assert_eq!(commands(&env, ty::RESET_DEVICE), 0, "no Reset Device for the firmware's reset");

    let weak = d.start_reset(ResetKind::Recover).unwrap();
    let gone = d.start_reset(ResetKind::Redetect).unwrap();
    assert!(Arc::ptr_eq(&weak, &gone));
    assert_eq!(run_job(&c, &env, &mut hp, &weak), Err(UsbError::Gone), "the logical disconnect happens");
    settle(&c, &env, &mut hp);
    assert!(d.is_gone());
    assert_ne!(devs(&c)[0].id, d.id);
}

#[test]
fn an_empty_port_ends_in_a_detach() {
    let (c, env, mut hp) = up("t-r-empty", Cfg::qemu(), |m| m.attach_root(5, FakeDev::bot_storage(Speed::Super, 2048)));
    let d = devs(&c)[0].clone();
    env.model().detach_root(5);
    // The port change may be looked at first or the job may find the port empty: either way the device goes.
    let job = d.start_reset(ResetKind::Recover).unwrap();
    assert_eq!(run_job(&c, &env, &mut hp, &job), Err(UsbError::Gone));
    settle(&c, &env, &mut hp);
    assert!(d.is_gone());
    assert!(devs(&c).is_empty());
}

#[test]
fn a_logical_disconnect_detaches_and_enumerates_the_port_afresh() {
    let (c, env, mut hp) = up("t-r-redetect", Cfg::qemu(), |m| m.attach_root(5, FakeDev::bot_storage(Speed::Super, 2048)));
    let d = devs(&c)[0].clone();
    assert_eq!(reset(&c, &env, &mut hp, &d, ResetKind::Redetect), Err(UsbError::Gone));
    settle(&c, &env, &mut hp);
    assert!(d.is_gone());
    let now = devs(&c);
    assert_eq!(now.len(), 1);
    assert_ne!(now[0].id, d.id, "a new device");
    assert!(matches!(d.start_reset(ResetKind::Recover), Err(UsbError::Gone)));
}

fn probe_ok(_: &Arc<Device>, _: &Interface) -> Result<(), String> {
    Ok(())
}
fn disconnect_none(_: &Arc<Device>, _: u8) {}
static AWARE_MATCH: [UsbMatch; 1] = [UsbMatch::Device { vendor: 0x7B01, product: 0x0001 }];
static AWARE: UsbDriver = UsbDriver { name: "t-reset-aware", matches: &AWARE_MATCH, probe: probe_ok, disconnect: disconnect_none };
static UNAWARE_MATCH: [UsbMatch; 1] = [UsbMatch::Device { vendor: 0x7B02, product: 0x0001 }];
static UNAWARE: UsbDriver = UsbDriver { name: "t-reset-unaware", matches: &UNAWARE_MATCH, probe: probe_ok, disconnect: disconnect_none };

fn ids(mut d: FakeDev, vendor: u16) -> FakeDev {
    d.device[8..10].copy_from_slice(&vendor.to_le_bytes());
    d.device[10..12].copy_from_slice(&1u16.to_le_bytes());
    d
}

fn lose_enable(env: &FakeEnv, port: u8) {
    let mut m = env.model();
    let i = port as usize - 1;
    m.portsc[i] = (m.portsc[i] & !portsc::PED) | portsc::PEC;
    m.port_event(port);
}

#[test]
fn a_port_disabled_under_a_device_that_survives_a_reset_costs_that_reset() {
    registry::register_driver(&AWARE);
    registry::declare_reset_aware(AWARE.name);
    let (c, env, mut hp) = up("t-r-dropped", Cfg::qemu(), |m| m.attach_root(5, ids(FakeDev::bot_storage(Speed::Super, 2048), 0x7B01)));
    let d = devs(&c)[0].clone();
    assert!(!registry::bindings_of(d.id).is_empty());
    lose_enable(&env, 5);
    settle(&c, &env, &mut hp);
    assert!(!d.is_gone(), "the same device, not a new one");
    assert_eq!(devs(&c).iter().map(|x| x.id).collect::<Vec<_>>(), [d.id]);
    assert_eq!(commands(&env, ty::RESET_DEVICE), 1);
    descriptor_works(&d);
    registry::unregister_driver(AWARE.name);
}

#[test]
fn a_driver_that_does_not_survive_a_reset_keeps_the_old_way() {
    registry::register_driver(&UNAWARE);
    let (c, env, mut hp) = up("t-r-unaware", Cfg::qemu(), |m| m.attach_root(5, ids(FakeDev::bot_storage(Speed::Super, 2048), 0x7B02)));
    let d = devs(&c)[0].clone();
    assert!(matches!(d.start_reset(ResetKind::Recover), Err(UsbError::Invalid(_))), "the asker goes for Redetect");
    lose_enable(&env, 5);
    settle(&c, &env, &mut hp);
    assert!(d.is_gone(), "enumerated afresh");
    assert_eq!(commands(&env, ty::RESET_DEVICE), 0);
    assert_eq!(devs(&c).len(), 1);
    registry::unregister_driver(UNAWARE.name);
}

/// Hotplug rounds with the fake clock at `until`.
fn rounds_until(c: &C, env: &FakeEnv, hp: &mut Hotplug, until: u64) {
    {
        let mut m = env.model();
        m.time = m.time.max(until);
    }
    settle(c, env, hp);
}

#[test]
fn a_root_port_given_up_on_is_tried_again_after_a_pause_that_grows() {
    let (c, env, mut hp) = up("t-r-retry", Cfg::qemu(), |m| {
        let mut k = FakeDev::keyboard(Speed::Full);
        k.fail_addresses = 4;
        m.attach_root(1, k);
    });
    assert!(devs(&c).is_empty(), "both tries failed");
    let enables = commands(&env, ty::ENABLE_SLOT);
    let t0 = env.model().time;
    rounds_until(&c, &env, &mut hp, t0 + RETRY_FIRST_NS - S);
    assert_eq!(commands(&env, ty::ENABLE_SLOT), enables, "not before the pause is over");
    rounds_until(&c, &env, &mut hp, t0 + RETRY_FIRST_NS + S);
    assert!(devs(&c).is_empty(), "the two failures left");
    assert!(commands(&env, ty::ENABLE_SLOT) > enables, "it was tried");
    let t1 = env.model().time;
    rounds_until(&c, &env, &mut hp, t1 + RETRY_FIRST_NS + S);
    assert!(devs(&c).is_empty(), "the second pause is twice as long");
    rounds_until(&c, &env, &mut hp, t1 + 2 * RETRY_FIRST_NS + S);
    assert_eq!(devs(&c).len(), 1, "and now it answers");
    assert_eq!(errors(&env), Vec::<String>::new());
}

#[test]
fn a_hub_port_given_up_on_comes_back_too() {
    let (c, env, mut hp) = up("t-r-hubretry", Cfg::qemu(), |m| {
        let mut h = FakeDev::hub(Speed::Full, 4, false);
        let mut k = FakeDev::keyboard(Speed::Full);
        k.fail_addresses = 2;
        h.plug(1, k);
        m.attach_root(1, h);
    });
    assert_eq!(devs(&c).len(), 1, "only the hub");
    let t0 = env.model().time;
    rounds_until(&c, &env, &mut hp, t0 + RETRY_FIRST_NS + S);
    assert_eq!(devs(&c).len(), 2, "the keyboard behind it");
}

#[test]
fn a_superspeed_hub_port_given_up_on_is_switched_on_again() {
    let (c, env, mut hp) = up("t-r-ssretry", Cfg::qemu(), |m| {
        let mut h = FakeDev::hub(Speed::Super, 4, false);
        let mut k = FakeDev::storage(Speed::Super);
        k.fail_addresses = 2;
        h.plug(1, k);
        m.attach_root(5, h);
    });
    assert_eq!(devs(&c).len(), 1, "only the hub");
    assert!(env.model().dev(&[5]).unwrap().hub.as_ref().unwrap().ports[0].ss_off, "in SS.Disabled, showing no connection");
    let t0 = env.model().time;
    rounds_until(&c, &env, &mut hp, t0 + RETRY_FIRST_NS + S);
    assert_eq!(devs(&c).len(), 2, "the stick behind it");
    assert!(!env.model().dev(&[5]).unwrap().hub.as_ref().unwrap().ports[0].ss_off);
}

#[test]
fn a_new_device_on_a_port_given_up_on_needs_no_pause() {
    let (c, env, mut hp) = up("t-r-replug", Cfg::qemu(), |m| {
        let mut k = FakeDev::keyboard(Speed::Full);
        k.fail_addresses = 2;
        m.attach_root(1, k);
    });
    assert!(devs(&c).is_empty());
    env.model().detach_root(1);
    settle(&c, &env, &mut hp);
    env.model().attach_root(1, FakeDev::keyboard(Speed::Full));
    settle(&c, &env, &mut hp);
    assert_eq!(devs(&c).len(), 1);
}

#[test]
fn the_stop_for_the_firmware_powers_the_port_off_and_on_and_warm_resets_it() {
    let (c, env, mut hp) = up("t-r-fw", Cfg::minipc(), |m| m.attach_root(2, FakeDev::bot_storage(Speed::High, 2048)));
    let d = devs(&c)[0].clone();
    env.model().dev(&[2]).unwrap().wedged = Some(Wedge::UntilPower);
    let before = env.model().usb.commands.len();
    env.model().usb.port_resets.clear();
    reset(&c, &env, &mut hp, &d, ResetKind::ForFirmware).unwrap();
    let m = env.model();
    assert_eq!(m.usb.power_offs, [2]);
    assert_eq!(m.portsc[1] & portsc::PP, portsc::PP, "the power is back on");
    assert_eq!(m.usb.port_resets, [(2, false)], "USB 2 has one kind of reset");
    assert_eq!(m.usb.commands.len(), before, "no command: the controller halts next");
    assert!(m.usb.root[&2].wedged.is_none());
    drop(m);
    assert!(!d.is_gone());

    // The kernel goes on after all (a Reload that did not jump): the next recovery brings the slot along.
    reset(&c, &env, &mut hp, &d, ResetKind::Recover).unwrap();
    descriptor_works(&d);

    // Without a power switch: the warm reset alone, and the check says so.
    let (c, env, mut hp) = up("t-r-fw2", Cfg::qemu(), |m| m.attach_root(5, FakeDev::bot_storage(Speed::Super, 2048)));
    let d = devs(&c)[0].clone();
    env.model().usb.port_resets.clear();
    reset(&c, &env, &mut hp, &d, ResetKind::ForFirmware).unwrap();
    assert!(env.model().usb.power_offs.is_empty());
    assert_eq!(env.model().usb.port_resets, [(5, true)]);
}

#[test]
fn a_stick_only_power_helps_comes_back_from_the_power_rung() {
    let (c, env, mut hp) = up("t-r-power", Cfg::minipc(), |m| m.attach_root(2, FakeDev::bot_storage(Speed::High, 2048)));
    let d = devs(&c)[0].clone();
    env.model().dev(&[2]).unwrap().wedged = Some(Wedge::UntilPower);
    assert_eq!(reset(&c, &env, &mut hp, &d, ResetKind::Recover), Err(UsbError::Timeout));
    assert!(env.model().usb.power_offs.is_empty());
    reset(&c, &env, &mut hp, &d, ResetKind::RecoverPower).unwrap();
    assert_eq!(env.model().usb.power_offs, [2]);
    assert!(!d.is_gone());
    descriptor_works(&d);
}

#[test]
fn a_look_at_an_endpoint_sees_its_events_and_its_dequeue_pointer() {
    let (c, env, _hp) = up("t-r-look", Cfg::qemu(), |m| m.attach_root(5, FakeDev::bot_storage(Speed::Super, 2048)));
    let d = devs(&c)[0].clone();
    let host: &Arc<dyn Host> = d.host();
    let a = host.look_endpoint(d.slot, OUT).expect("a configured endpoint");
    assert_eq!(a.events, 0);
    let tur = cbw(1, 0, false, &[0; 6]);
    assert_eq!(d.out_bytes(OUT, &tur, S), Ok(31));
    assert!(d.in_bytes(IN, 13, S).is_ok());
    let b = host.look_endpoint(d.slot, OUT).unwrap();
    assert_eq!(b.events, 1);
    assert!(b.moved_since(&a));
    assert!(!b.moved_since(&b));
    assert!(host.look_endpoint(d.slot, IN).is_some());
    assert!(host.look_endpoint(d.slot, 0x85).is_none(), "no such endpoint");
    // An answer in the event ring that nobody took yet is taken by the look: the transfer's end comes with it.
    env.model().dev(&[5]).unwrap().nak_out.push(OUT);
    let buf = d.alloc_buffer(31).unwrap();
    assert!(buf.write(0, &tur));
    let t = d.submit(OUT, Request::new(buf, 0, 31)).unwrap();
    assert!(!t.is_done());
    env.model().dev(&[5]).unwrap().nak_out.clear();
    env.model().pump();
    assert!(!t.is_done(), "its event waits in the ring");
    assert_eq!(host.look_endpoint(d.slot, OUT).map(|l| l.events), Some(2));
    assert!(t.is_done());
}

#[test]
fn a_transfer_whose_time_ran_out_but_whose_event_is_there_did_not_time_out() {
    let (c, env, _hp) = up("t-r-late", Cfg::qemu(), |m| m.attach_root(5, FakeDev::bot_storage(Speed::Super, 2048)));
    let d = devs(&c)[0].clone();
    env.model().dev(&[5]).unwrap().nak_out.push(OUT);
    let buf = d.alloc_buffer(31).unwrap();
    assert!(buf.write(0, &cbw(1, 0, false, &[0; 6])));
    let t = d.submit(OUT, Request::new(buf, 0, 31)).unwrap();
    env.model().dev(&[5]).unwrap().nak_out.clear();
    env.model().pump();
    // The CPUs stood still for longer than the transfer's time.
    env.model().time += 10 * S;
    assert_eq!(t.wait(MS).status(), Ok(31));
    // One that really stands still times out, with its time and the look on top.
    env.model().dev(&[5]).unwrap().nak_out.push(OUT);
    let t0 = env.model().time;
    let r = d.out_bytes(OUT, &[1u8; 31], S);
    assert_eq!(r, Err(UsbError::Timeout));
    assert!(env.model().time - t0 >= S + usb_core::reset::LOOK_NS);
}
