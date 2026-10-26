//! Host tests of SN-8a: alternate settings and isochronous pipes against the fake controller (`fake_iso.rs`) with a
//! JBL Quantum 800 shaped headset (`FakeDev::audio`, full speed on a root port).

use crate::fake::{Cfg, FakeEnv};
use crate::fake_iso::IsoAct;
use crate::fake_usb::FakeDev;
use crate::hc::{Source, MS};
use crate::tests_usb::{devs, errors, settle, up, C};
use crate::trb::{cc, ty};
use alloc::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::vec;
use std::vec::Vec;
use usb_core::{Device, IsoOpts, IsoPacket, IsoPipe, IsoRequest, IsoResult, Speed, UsbError, XferResult};

const OUT: u8 = 0x02;
const IN: u8 = 0x81;

fn headset(name: &str) -> (C, FakeEnv, crate::hotplug::Hotplug, Arc<Device>) {
    let (c, env, hp) = up(name, Cfg::minipc(), |m| m.attach_root(2, FakeDev::audio(Speed::Full)));
    let d = devs(&c).into_iter().find(|d| d.vendor() == 0x0ECB).expect("the headset");
    (c, env, hp, d)
}

fn pump(c: &C, env: &FakeEnv) {
    env.model().pump();
    c.process_events(Source::Waiter);
}

type Log = Arc<StdMutex<Vec<IsoResult>>>;

fn queue(p: &IsoPipe, req: IsoRequest, log: &Log) -> Result<(), UsbError> {
    let l = log.clone();
    p.queue(req, alloc::boxed::Box::new(move |r| l.lock().unwrap().push(r)))
}

fn pattern(d: &Device, lens: &[usize], seed: u8) -> IsoRequest {
    let total: usize = lens.iter().sum();
    let buf = d.alloc_buffer(total.max(1)).unwrap();
    let bytes: Vec<u8> = (0..total).map(|i| seed.wrapping_add(i as u8)).collect();
    assert!(buf.write(0, &bytes));
    let mut off = 0;
    let packets = lens.iter().map(|&len| {
        let p = IsoPacket { offset: off, len };
        off += len;
        p
    }).collect();
    IsoRequest { buf, packets, frame: None }
}

#[test]
fn alternate_settings_add_and_drop_isochronous_endpoints() {
    let (c, env, _hp, d) = headset("t-iso-alt");
    assert_eq!(d.endpoint(OUT), None, "alternate setting 0 has no endpoint");
    assert_eq!(d.current_alt(1), Some(0));
    d.set_interface(1, 1).unwrap();
    assert_eq!(d.endpoint(OUT).map(|e| e.max_packet()), Some(288));
    assert_eq!(env.model().usb.configures.last(), Some(&(0, 1 | 1 << 4)));
    assert_eq!(env.model().dev(&[2]).unwrap().alt, 1);
    // Alternate setting 2 has the same endpoint at another size: dropped and added in one command.
    d.set_interface(1, 2).unwrap();
    assert_eq!(env.model().usb.configures.last(), Some(&(1 << 4, 1 | 1 << 4)));
    assert_eq!(d.endpoint(OUT).map(|e| e.max_packet()), Some(144));
    d.set_interface(3, 1).unwrap();
    assert_eq!(env.model().usb.configures.last(), Some(&(0, 1 | 1 << 3)));
    assert_eq!((d.current_alt(1), d.current_alt(3)), (Some(2), Some(1)));
    d.set_interface(1, 0).unwrap();
    assert_eq!(env.model().usb.configures.last(), Some(&(1 << 4, 1)));
    assert_eq!(d.endpoint(OUT), None);
    assert!(d.endpoint(IN).is_some());
    assert_eq!(d.set_interface(1, 7).err(), Some(UsbError::Invalid("no such alternate setting")));
    assert!(d.iso_pipe(OUT, IsoOpts::ms(4)).is_err(), "no pipe on an endpoint not set up");
    // A device that refuses SET_INTERFACE: the host goes back to the old endpoints.
    env.model().dev(&[2]).unwrap().stall.push(11);
    assert_eq!(d.set_interface(1, 1).err(), Some(UsbError::Stall));
    assert_eq!(env.model().usb.configures.last(), Some(&(1 << 4, 1)), "added, then dropped again");
    assert_eq!(d.current_alt(1), Some(0));
    assert!(errors(&env).is_empty(), "{:?}", errors(&env));
    let _ = c;
}

#[test]
fn out_requests_become_isoch_tds_with_per_packet_results() {
    let (c, env, _hp, d) = headset("t-iso-out");
    d.set_interface(1, 1).unwrap();
    let p = d.iso_pipe(OUT, IsoOpts::ms(8)).unwrap();
    assert_eq!((p.window(), p.max_packet(), p.interval_ns()), (8, 288, MS));
    let log: Log = Default::default();
    env.model().usb.iso.hold = true;
    queue(&p, pattern(&d, &[288, 192, 0, 100], 1), &log).unwrap();
    queue(&p, pattern(&d, &[192, 192, 192, 192], 50), &log).unwrap();
    assert_eq!((p.queued(), p.room()), (8, 0));
    assert_eq!(queue(&p, pattern(&d, &[192], 9), &log).err(), Some(UsbError::Busy), "the window is full");
    assert_eq!(queue(&p, pattern(&d, &[289], 9), &log).err().map(|e| e.as_str()),
        Some("isochronous packet longer than the endpoint's service interval payload"));
    env.model().usb.iso.hold = false;
    pump(&c, &env);
    let got = log.lock().unwrap().clone();
    assert_eq!(got.len(), 2);
    assert_eq!(got[0].packets, [288, 192, 0, 100].map(XferResult::ok));
    assert_eq!((got[1].bytes(), got[1].failed(), got[1].error), (768, 0, None));
    let m = env.model();
    let seen = &m.usb.iso.seen;
    assert_eq!(seen.iter().map(|s| s.len).collect::<Vec<_>>(), [288, 192, 0, 100, 192, 192, 192, 192]);
    assert!(seen.iter().all(|s| s.sia && s.tlbpc == 0));
    assert_eq!(seen.iter().map(|s| s.bei).collect::<Vec<_>>(), [true, true, true, false, true, true, true, false],
        "an interrupt only for a request's last TD");
    let want: Vec<u8> = (0..580).map(|i| 1u8.wrapping_add(i as u8)).collect();
    assert_eq!(seen[..4].iter().flat_map(|s| s.data.clone()).collect::<Vec<_>>(), want);
    assert!(m.usb.errors.is_empty(), "{:?}", m.usb.errors);
    drop(m);
    assert_eq!(p.room(), 8);
    let st = c.iso_stats(d.slot, OUT).unwrap();
    assert_eq!((st.packets, st.missed, st.ring_empty), (8, 0, 1), "one Ring Underrun once the ring ran empty");
}

#[test]
fn in_packets_may_be_short() {
    let (c, env, _hp, d) = headset("t-iso-in");
    d.set_interface(3, 1).unwrap();
    let p = d.iso_pipe(IN, IsoOpts::ms(4)).unwrap();
    let log: Log = Default::default();
    {
        let mut m = env.model();
        m.usb.iso.script.extend([IsoAct::Done, IsoAct::Short(50), IsoAct::Done]);
        m.usb.iso.in_byte = 0x40;
        m.usb.iso.hold = true;
    }
    let req = IsoRequest::even(d.alloc_buffer(3 * 144).unwrap(), 3, 144);
    let buf = req.buf.clone();
    queue(&p, req, &log).unwrap();
    env.model().usb.iso.hold = false;
    pump(&c, &env);
    let got = log.lock().unwrap().clone();
    assert_eq!(got[0].packets, [144, 50, 144].map(XferResult::ok));
    let mut b = vec![0u8; 3 * 144];
    assert!(buf.read(0, &mut b));
    assert!(b[..144].iter().all(|&x| x == 0x40) && b[144..194].iter().all(|&x| x == 0x41) && b[194..288].iter().all(|&x| x == 0));
    assert!(errors(&env).is_empty(), "{:?}", errors(&env));
    assert_eq!(c.iso_stats(d.slot, IN).unwrap().ring_empty, 1, "Ring Overrun once empty");
}

#[test]
fn missed_service_intervals_end_as_missed() {
    let (c, env, _hp, d) = headset("t-iso-miss");
    d.set_interface(1, 1).unwrap();
    let p = d.iso_pipe(OUT, IsoOpts::ms(8)).unwrap();
    let log: Log = Default::default();
    {
        let mut m = env.model();
        m.usb.iso.script.extend([IsoAct::Done, IsoAct::Missed { event: false }, IsoAct::Missed { event: true }, IsoAct::Done, IsoAct::Done]);
        m.usb.iso.hold = true;
    }
    queue(&p, pattern(&d, &[192; 5], 0), &log).unwrap();
    env.model().usb.iso.hold = false;
    pump(&c, &env);
    let got = log.lock().unwrap().clone();
    let missed = XferResult::failed(UsbError::Transfer(cc::MISSED_SERVICE), 0);
    assert_eq!(got[0].packets, [XferResult::ok(192), missed, missed, XferResult::ok(192), XferResult::ok(192)]);
    assert_eq!(got[0].error, None, "missed packets do not end the request as a whole");
    assert_eq!(c.iso_stats(d.slot, OUT).unwrap().missed, 2);
    assert_eq!(p.queued(), 0);
}

#[test]
fn a_halting_error_is_repaired_by_the_worker() {
    let (c, env, mut hp, d) = headset("t-iso-halt");
    d.set_interface(1, 1).unwrap();
    let p = d.iso_pipe(OUT, IsoOpts::ms(8)).unwrap();
    let log: Log = Default::default();
    {
        let mut m = env.model();
        m.usb.iso.script.push_back(IsoAct::Error { code: cc::USB_TRANSACTION, halt: true });
        m.usb.iso.hold = true;
    }
    queue(&p, pattern(&d, &[192; 3], 0), &log).unwrap();
    env.model().usb.iso.hold = false;
    pump(&c, &env);
    assert!(log.lock().unwrap().is_empty(), "two packets wait behind the halt");
    settle(&c, &env, &mut hp);
    pump(&c, &env);
    let got = log.lock().unwrap().clone();
    assert_eq!(got[0].packets, [XferResult::failed(UsbError::Transaction, 0), XferResult::ok(192), XferResult::ok(192)]);
    let m = env.model();
    assert!(m.usb.commands.contains(&ty::RESET_ENDPOINT) && m.usb.commands.contains(&ty::SET_TR_DEQUEUE));
    assert!(m.usb.errors.is_empty(), "{:?}", m.usb.errors);
}

#[test]
fn stop_ends_everything_queued_and_the_pipe_goes_on() {
    let (c, env, _hp, d) = headset("t-iso-stop");
    d.set_interface(1, 1).unwrap();
    let p = d.iso_pipe(OUT, IsoOpts::ms(16)).unwrap();
    let log: Log = Default::default();
    env.model().usb.iso.hold = true;
    queue(&p, pattern(&d, &[192; 4], 0), &log).unwrap();
    queue(&p, pattern(&d, &[192; 4], 0), &log).unwrap();
    assert_eq!(p.stop(), Ok(8));
    let got = log.lock().unwrap().clone();
    assert_eq!(got.len(), 2);
    assert!(got.iter().all(|r| r.error == Some(UsbError::Cancelled) && r.packets.iter().all(|x| x.error == Some(UsbError::Cancelled))));
    assert_eq!((p.queued(), c.queued_tds(d.slot, OUT)), (0, Some(0)));
    assert!(env.model().usb.commands.contains(&ty::STOP_ENDPOINT));
    env.model().usb.iso.hold = false;
    queue(&p, pattern(&d, &[192; 2], 7), &log).unwrap();
    pump(&c, &env);
    assert_eq!(log.lock().unwrap()[2].packets, [XferResult::ok(192); 2]);
    assert!(errors(&env).is_empty(), "{:?}", errors(&env));
}

#[test]
fn unplug_and_alternate_setting_change_end_requests_whole() {
    let (c, env, mut hp, d) = headset("t-iso-gone");
    d.set_interface(1, 1).unwrap();
    let p = d.iso_pipe(OUT, IsoOpts::ms(16)).unwrap();
    let log: Log = Default::default();
    env.model().usb.iso.hold = true;
    queue(&p, pattern(&d, &[192; 4], 0), &log).unwrap();
    d.set_interface(1, 0).unwrap();
    let got = log.lock().unwrap().clone();
    assert!(matches!(got[0].error, Some(UsbError::Invalid(_))), "{:?}", got[0]);
    d.set_interface(1, 1).unwrap();
    let p = d.iso_pipe(OUT, IsoOpts::ms(16)).unwrap();
    queue(&p, pattern(&d, &[192; 4], 0), &log).unwrap();
    env.model().detach_root(2);
    settle(&c, &env, &mut hp);
    let got = log.lock().unwrap().clone();
    assert_eq!(got[1].error, Some(UsbError::Gone));
    assert_eq!(got[1].packets, [XferResult::failed(UsbError::Gone, 0); 4]);
    assert_eq!(queue(&p, pattern(&d, &[192], 0), &log).err(), Some(UsbError::Gone));
}

#[test]
fn frame_ids_when_a_frame_is_asked_for() {
    let (c, env, _hp, d) = headset("t-iso-frame");
    d.set_interface(1, 1).unwrap();
    let p = d.iso_pipe(OUT, IsoOpts::ms(16)).unwrap();
    let log: Log = Default::default();
    let now = ((env.model().time / MS) & 0x7FF) as u16;
    let mut req = pattern(&d, &[192; 3], 0);
    req.frame = Some((now + 10) & 0x7FF);
    queue(&p, req, &log).unwrap();
    let mut late = pattern(&d, &[192], 0);
    late.frame = Some(now.wrapping_sub(5) & 0x7FF);
    queue(&p, late, &log).unwrap();
    pump(&c, &env);
    let m = env.model();
    let seen: Vec<(bool, u16)> = m.usb.iso.seen.iter().map(|s| (s.sia, s.frame)).collect();
    let f = (now + 10) & 0x7FF;
    assert_eq!(seen, [(false, f), (false, (f + 1) & 0x7FF), (false, (f + 2) & 0x7FF), (true, 0)], "a frame already past: ASAP");
}

#[test]
fn a_deep_window_grows_the_ring_and_crosses_its_links() {
    let (c, env, _hp, d) = headset("t-iso-deep");
    d.set_interface(1, 1).unwrap();
    let p = d.iso_pipe(OUT, IsoOpts::ms(1000)).unwrap();
    let log: Log = Default::default();
    env.model().usb.iso.hold = true;
    for k in 0..100u8 {
        queue(&p, pattern(&d, &[192; 8], k), &log).unwrap();
    }
    env.model().usb.iso.hold = false;
    pump(&c, &env);
    for k in 100..200u8 {
        queue(&p, pattern(&d, &[192; 8], k), &log).unwrap();
        pump(&c, &env);
    }
    let got = log.lock().unwrap().clone();
    assert_eq!(got.len(), 200);
    assert!(got.iter().all(|r| r.error.is_none() && r.bytes() == 8 * 192));
    let m = env.model();
    assert_eq!(m.usb.iso.seen.len(), 1600);
    assert!(m.usb.iso.seen.chunks(8).enumerate().all(|(k, s)| s[0].data[0] == k as u8), "in order");
    assert!(m.usb.errors.is_empty(), "{:?}", m.usb.errors);
}

#[test]
fn a_bandwidth_refusal_keeps_the_old_alternate_setting() {
    let (c, env, _hp, d) = headset("t-iso-bw");
    d.set_interface(1, 1).unwrap();
    let p = d.iso_pipe(OUT, IsoOpts::ms(8)).unwrap();
    env.model().usb.configure_codes.push_back(cc::BANDWIDTH);
    assert_eq!(d.set_interface(1, 2).err(), Some(UsbError::Command(cc::BANDWIDTH)));
    assert_eq!(env.model().dev(&[2]).unwrap().alt, 1, "the device never heard of alternate setting 2");
    assert_eq!((d.current_alt(1), d.endpoint(OUT).map(|e| e.max_packet())), (Some(1), Some(288)));
    let log: Log = Default::default();
    queue(&p, pattern(&d, &[288, 288], 3), &log).unwrap();
    pump(&c, &env);
    assert_eq!(log.lock().unwrap()[0].packets, [XferResult::ok(288); 2], "the old pipe still runs");
    assert!(errors(&env).is_empty(), "{:?}", errors(&env));
}

#[test]
fn a_failed_restore_leaves_the_interface_unusable_until_set_again() {
    let (_c, env, _hp, d) = headset("t-iso-restore");
    d.set_interface(1, 1).unwrap();
    {
        let mut m = env.model();
        m.dev(&[2]).unwrap().stall.push(11);
        // The change to alternate setting 2 goes through on the host, going back is refused.
        m.usb.configure_codes.extend([cc::SUCCESS, cc::BANDWIDTH]);
    }
    assert_eq!(d.set_interface(1, 2).err(), Some(UsbError::Stall));
    assert_eq!(d.current_alt(1), Some(1), "the device still runs alternate setting 1");
    assert_eq!(d.endpoint(OUT), None, "host and device disagree: no endpoint");
    assert!(matches!(d.iso_pipe(OUT, IsoOpts::ms(4)).err(), Some(UsbError::Invalid(_))));
    // Setting it again starts from the endpoints the host has (alternate setting 2's).
    env.model().dev(&[2]).unwrap().stall.clear();
    d.set_interface(1, 2).unwrap();
    assert_eq!(env.model().usb.configures.last(), Some(&(1 << 4, 1 | 1 << 4)));
    assert_eq!((d.current_alt(1), d.endpoint(OUT).map(|e| e.max_packet())), (Some(2), Some(144)));
    assert!(d.iso_pipe(OUT, IsoOpts::ms(4)).is_ok());
    assert!(errors(&env).is_empty(), "{:?}", errors(&env));
}

#[test]
fn hostile_requests_are_refused_without_a_callback() {
    let (c, env, mut hp, d) = headset("t-iso-hostile");
    d.set_interface(1, 1).unwrap();
    let p = d.iso_pipe(OUT, IsoOpts::ms(4)).unwrap();
    let log: Log = Default::default();
    let req = |packets: Vec<IsoPacket>| IsoRequest { buf: d.alloc_buffer(1024).unwrap(), packets, frame: None };
    let pk = |offset, len| IsoPacket { offset, len };
    let cases: Vec<(&str, IsoRequest)> = vec![
        ("no packets", req(vec![])),
        ("past the buffer end", req(vec![pk(1000, 100)])),
        ("offset + len wraps", req(vec![pk(usize::MAX, 2)])),
        ("longer than a service interval", req(vec![pk(0, 289)])),
        ("more packets than the window", req(vec![pk(0, 10); 5])),
    ];
    for (why, r) in cases {
        assert!(matches!(queue(&p, r, &log), Err(UsbError::Invalid(_))), "{why}");
        assert_eq!(p.queued(), 0, "{why}");
    }
    assert_eq!(IsoOpts::ms(u64::MAX).window_ns, u64::MAX, "no overflow");
    env.model().detach_root(2);
    settle(&c, &env, &mut hp);
    assert_eq!(queue(&p, req(vec![pk(0, 10)]), &log).err(), Some(UsbError::Gone));
    assert_eq!(p.queued(), 0);
    assert!(log.lock().unwrap().is_empty(), "no callback for a refused request");
}

#[test]
fn a_refused_repair_is_tried_again_then_given_up() {
    let (c, env, mut hp, d) = headset("t-iso-repair");
    d.set_interface(1, 1).unwrap();
    let p = d.iso_pipe(OUT, IsoOpts::ms(8)).unwrap();
    let log: Log = Default::default();
    let halt = |n: usize| {
        let mut m = env.model();
        m.usb.iso.script.push_back(IsoAct::Error { code: cc::USB_TRANSACTION, halt: true });
        m.usb.fail_ep_commands = n;
        m.usb.iso.hold = true;
    };
    // Two refused resets, then it works: the packets behind the halt go out.
    halt(2);
    queue(&p, pattern(&d, &[192; 3], 0), &log).unwrap();
    env.model().usb.iso.hold = false;
    pump(&c, &env);
    settle(&c, &env, &mut hp);
    pump(&c, &env);
    assert_eq!(log.lock().unwrap()[0].packets, [XferResult::failed(UsbError::Transaction, 0), XferResult::ok(192), XferResult::ok(192)]);
    // Never resets: given up after the limit, the queued packets end with the error, new requests are refused.
    halt(usize::MAX);
    queue(&p, pattern(&d, &[192; 3], 0), &log).unwrap();
    env.model().usb.iso.hold = false;
    pump(&c, &env);
    settle(&c, &env, &mut hp);
    let got = log.lock().unwrap().clone();
    assert_eq!(got.len(), 2);
    assert_eq!(got[1].error, Some(UsbError::Transaction));
    assert_eq!(p.queued(), 0);
    assert_eq!(queue(&p, pattern(&d, &[192], 0), &log).err(), Some(UsbError::Transaction));
    // Setting the alternate setting again gives a fresh endpoint.
    env.model().usb.fail_ep_commands = 0;
    d.set_interface(1, 0).unwrap();
    d.set_interface(1, 1).unwrap();
    let p = d.iso_pipe(OUT, IsoOpts::ms(8)).unwrap();
    queue(&p, pattern(&d, &[192; 2], 5), &log).unwrap();
    pump(&c, &env);
    assert_eq!(log.lock().unwrap()[2].packets, [XferResult::ok(192); 2]);
}

#[test]
fn the_event_after_an_early_short_packet_is_no_stray() {
    let (c, env, _hp, d) = headset("t-iso-tail");
    d.set_interface(3, 1).unwrap();
    let p = d.iso_pipe(IN, IsoOpts::ms(4)).unwrap();
    let log: Log = Default::default();
    {
        let mut m = env.model();
        m.usb.spurious_success = true;
        m.usb.iso.script.extend([IsoAct::Short(30), IsoAct::Done]);
        m.usb.iso.hold = true;
    }
    // The fake's buffers start 0x3000 below a 64 KiB boundary: the first packet is two TRBs (50 + 94 bytes).
    let buf = d.alloc_buffer(0x3000 + 144).unwrap();
    let req = IsoRequest { buf, packets: vec![IsoPacket { offset: 0x3000 - 50, len: 144 }, IsoPacket { offset: 0x3000 + 94, len: 50 }], frame: None };
    queue(&p, req, &log).unwrap();
    env.model().usb.iso.hold = false;
    pump(&c, &env);
    assert_eq!(log.lock().unwrap()[0].packets, [XferResult::ok(30), XferResult::ok(50)]);
    assert_eq!(c.iso_stats(d.slot, IN).unwrap().stray, 0);
    assert!(errors(&env).is_empty(), "{:?}", errors(&env));
}
