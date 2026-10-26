//! US-3 host tests: bulk and interrupt transfers against the fake controller (`fake_bulk.rs`) in both shapes:
//! a BOT read through TDs cut at 64 KiB, deep queues that grow the ring, buffers made of pieces, short packets in
//! the middle of a TD (also as AMD's Success-with-residual and with a spurious success after it), timeouts that
//! stop and skip, cancels in the middle of a queue (No-Ops), STALL with Reset Endpoint and the device's halt
//! cleared (data toggles checked on both sides), the toggle reset of a running endpoint (Configure Endpoint
//! drop/add), abort, deep interrupt pipes and their repair, device removal and foreign buffers.

use crate::buf::XhciBuf;
use crate::fake::{Cfg, FakeEnv};
use crate::fake_usb::FakeDev;
use crate::hc::{Source, MS};
use crate::mem::DmaMem;
use crate::tests_usb::{devs, errors, up, C};
use crate::trb::ty;
use alloc::string::String;
use alloc::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::vec;
use std::vec::Vec;
use usb_core::{registry, Buffer, Device, PipeEvent, PipeOpts, Request, Speed, UsbError, XferMem, XferResult};

const IN: u8 = 0x81;
const OUT: u8 = 0x02;
const T: u64 = 1000 * MS;

fn stick(c: &C) -> Arc<Device> {
    devs(c).into_iter().find(|d| d.config.as_ref().is_some_and(|x| x.defaults().any(|f| f.class == 8))).expect("a stick")
}

/// Lets the fake run its endpoints and the driver take the events.
fn pump(c: &C, env: &FakeEnv) {
    env.model().pump();
    c.process_events(Source::Waiter);
}

fn cbw(tag: u32, len: u32, dir_in: bool, cb: &[u8]) -> Vec<u8> {
    let mut v = b"USBC".to_vec();
    v.extend_from_slice(&tag.to_le_bytes());
    v.extend_from_slice(&len.to_le_bytes());
    v.push(if dir_in { 0x80 } else { 0 });
    v.push(0);
    v.push(cb.len() as u8);
    let mut c = [0u8; 16];
    c[..cb.len()].copy_from_slice(cb);
    v.extend_from_slice(&c);
    v
}

fn read10(lba: u32, n: u16) -> [u8; 10] {
    let l = lba.to_be_bytes();
    let k = n.to_be_bytes();
    [0x28, 0, l[0], l[1], l[2], l[3], 0, k[0], k[1], 0]
}

/// One BOT READ(10): CBW, the data into `buf` (one TD), CSW. Returns the bytes read.
fn bot_read(d: &Arc<Device>, buf: &Buffer, lba: u32, blocks: u16) -> usize {
    let n = blocks as usize * 512;
    assert_eq!(d.out_bytes(OUT, &cbw(7, n as u32, true, &read10(lba, blocks)), T), Ok(31));
    let r = d.transfer(IN, Request::new(buf.clone(), 0, n), T);
    let csw = d.in_bytes(IN, 13, T).expect("CSW");
    assert_eq!((&csw[0..4], csw[12]), (&b"USBS"[..], 0), "CSW {csw:?}");
    r.status().expect("data phase")
}

fn disk_byte(i: usize) -> u8 {
    (i ^ (i >> 9) ^ (i >> 17)) as u8
}

#[test]
fn bot_read_cut_at_64k_in_both_shapes() {
    for (name, cfg, port, speed) in [("t-x-bot-mini", Cfg::minipc(), 2, Speed::High), ("t-x-bot-qemu", Cfg::qemu(), 5, Speed::Super)] {
        let (c, env, _hp) = up(name, cfg, |m| m.attach_root(port, FakeDev::bot_storage(speed, 2048)));
        let d = stick(&c);
        // 256 KiB starting 0x3000 below a 64 KiB boundary (fake `alloc_data`): TRBs 12 KiB + 3 x 64 KiB + 52 KiB.
        let buf = d.alloc_buffer(256 * 1024).unwrap();
        let x = buf.as_any().downcast_ref::<XhciBuf<crate::fake::FakeMem>>().unwrap();
        assert_eq!(x.pieces(), 1);
        assert_eq!(bot_read(&d, &buf, 100, 512), 256 * 1024);
        let mut got = vec![0u8; 256 * 1024];
        assert!(buf.read(0, &mut got));
        assert!(got.iter().enumerate().all(|(i, &b)| b == disk_byte(100 * 512 + i)), "{name}: data differs");
        // INQUIRY into a larger TD: short, the CSW reports the residue.
        assert_eq!(d.out_bytes(OUT, &cbw(8, 96, true, &[0x12, 0, 0, 0, 96, 0]), T), Ok(31));
        let inq = d.in_bytes(IN, 96, T).unwrap();
        assert_eq!((inq.len(), &inq[8..16]), (36, &b"BitsFake"[..]));
        let csw = d.in_bytes(IN, 13, T).unwrap();
        assert_eq!(u32::from_le_bytes([csw[8], csw[9], csw[10], csw[11]]), 60);
        assert_eq!(errors(&env), Vec::<String>::new(), "{name}: TRB rules (64 KiB, TD Size, CH/IOC/ISP, toggles)");
        let m = env.model();
        assert!(m.usb.root[&port].bot.as_ref().unwrap().commands == vec![0x28, 0x12]);
    }
}

#[test]
fn buffer_of_pieces_and_offsets() {
    let (c, env, _hp) = up("t-x-pieces", Cfg::minipc(), |m| m.attach_root(2, FakeDev::bot_storage(Speed::High, 2048)));
    let d = stick(&c);
    let buf = c.alloc_xfer_buffer(200 * 1024, true).unwrap();
    let x = buf.as_any().downcast_ref::<XhciBuf<crate::fake::FakeMem>>().unwrap();
    assert_eq!(x.pieces(), 4, "64 KiB pieces");
    // 120 KiB into the middle of the buffer (offset 20 KiB): crosses two piece borders.
    let n = 240 * 512;
    assert_eq!(d.out_bytes(OUT, &cbw(9, n as u32, true, &read10(3, 240)), T), Ok(31));
    let r = d.transfer(IN, Request::new(buf.clone(), 20 * 1024, n), T);
    assert_eq!(r, XferResult::ok(n));
    assert!(d.in_bytes(IN, 13, T).is_ok());
    let mut got = vec![0u8; n];
    assert!(buf.read(20 * 1024, &mut got));
    assert!(got.iter().enumerate().all(|(i, &b)| b == disk_byte(3 * 512 + i)));
    let mut head = vec![1u8; 20 * 1024];
    assert!(buf.read(0, &mut head) && head.iter().all(|&b| b == 0), "nothing before the offset touched");
    assert!(!buf.read(200 * 1024 - 4, &mut [0u8; 8]), "read past the end refused");
    assert_eq!(errors(&env), Vec::<String>::new());
}

#[test]
fn deep_queues_grow_the_ring_and_complete_in_order() {
    let (c, env, _hp) = up("t-x-deep", Cfg::qemu(), |m| m.attach_root(1, FakeDev::storage(Speed::High)));
    let d = stick(&c);
    let path = vec![1u8];
    env.model().dev(&path).unwrap().nak_out.push(OUT);
    // 600 OUT transfers queued while the device NAKs: far more than one 255-TRB segment.
    let mut ts = Vec::new();
    for i in 0..600u32 {
        let b = d.alloc_buffer(8).unwrap();
        assert!(b.write(0, &i.to_le_bytes()));
        ts.push(d.submit(OUT, Request::new(b, 0, 4)).unwrap());
    }
    assert!(ts.iter().all(|t| !t.is_done()));
    env.model().dev(&path).unwrap().nak_out.clear();
    // The controller retries a NAKed endpoint by itself; the fake needs a push.
    pump(&c, &env);
    for t in &ts {
        assert_eq!(t.wait(T), XferResult::ok(4));
    }
    let seen: Vec<u32> = env.model().dev(&path).unwrap().out_seen.iter().map(|(_, v)| u32::from_le_bytes([v[0], v[1], v[2], v[3]])).collect();
    assert_eq!(seen, (0..600).collect::<Vec<_>>(), "in submission order");
    // 700 IN transfers queued, then the device answers each with its own data.
    let bufs: Vec<Buffer> = (0..700).map(|_| d.alloc_buffer(16).unwrap()).collect();
    let ts: Vec<_> = bufs.iter().map(|b| d.submit(IN, Request::new(b.clone(), 0, 16)).unwrap()).collect();
    {
        let mut m = env.model();
        let q = m.dev(&path).unwrap().in_q.entry(IN).or_default();
        for i in 0..700u32 {
            q.push_back(i.to_le_bytes().to_vec());
        }
    }
    pump(&c, &env);
    for (i, (t, b)) in ts.iter().zip(&bufs).enumerate() {
        assert_eq!(t.wait(T), XferResult::ok(4), "short: 4 of 16");
        let mut v = [0u8; 4];
        assert!(b.read(0, &mut v));
        assert_eq!(u32::from_le_bytes(v), i as u32);
    }
    assert_eq!(errors(&env), Vec::<String>::new());
}

#[test]
fn short_packet_in_the_middle_amd_and_spurious_success() {
    for (name, amd, spurious) in [("t-x-short", false, false), ("t-x-short-amd", true, false), ("t-x-short-sp", false, true)] {
        let (c, env, _hp) = up(name, Cfg::minipc(), |m| {
            m.attach_root(2, FakeDev::storage(Speed::High));
            m.usb.short_as_success = amd;
            m.usb.spurious_success = spurious;
        });
        let d = stick(&c);
        let path = vec![2u8];
        // 150 KiB TD (4 TRBs from 0x3000 below a boundary); the device sends 70 000 bytes: short in TRB 2.
        let buf = d.alloc_buffer(150 * 1024).unwrap();
        let data: Vec<u8> = (0..70_000u32).map(|i| (i * 7) as u8).collect();
        env.model().dev(&path).unwrap().in_q.entry(IN).or_default().push_back(data.clone());
        let r = d.transfer(IN, Request::whole(buf.clone()), T);
        assert_eq!(r, XferResult::ok(70_000), "{name}");
        let mut got = vec![0u8; 70_000];
        assert!(buf.read(0, &mut got) && got == data);
        // The next TD is not confused by the extra event.
        env.model().dev(&path).unwrap().in_q.entry(IN).or_default().push_back(vec![9; 100]);
        assert_eq!(d.transfer(IN, Request::new(buf, 0, 512), T), XferResult::ok(100), "{name}");
        assert_eq!(errors(&env), Vec::<String>::new(), "{name}");
    }
}

#[test]
fn timeout_stops_and_skips() {
    let (c, env, _hp) = up("t-x-timeout", Cfg::minipc(), |m| m.attach_root(2, FakeDev::storage(Speed::High)));
    let d = stick(&c);
    let path = vec![2u8];
    let b1 = d.alloc_buffer(512).unwrap();
    let t0 = env.model().time;
    let r = d.transfer(IN, Request::whole(b1.clone()), 100 * MS);
    assert_eq!(r, XferResult::failed(UsbError::Timeout, 0));
    assert!(env.model().time >= t0 + 100 * MS);
    let cmds = env.model().usb.commands.clone();
    let n = cmds.len();
    assert_eq!(&cmds[n - 2..], &[ty::STOP_ENDPOINT, ty::SET_TR_DEQUEUE], "stop, then skip");
    // The data that comes later goes to the next transfer, not to the timed-out one's buffer.
    env.model().dev(&path).unwrap().in_q.entry(IN).or_default().push_back(vec![0xAB; 64]);
    let b2 = d.alloc_buffer(512).unwrap();
    assert_eq!(d.transfer(IN, Request::whole(b2.clone()), T), XferResult::ok(64));
    let mut v = [0u8; 64];
    assert!(b1.read(0, &mut v) && v.iter().all(|&x| x == 0), "the skipped TD's buffer untouched");
    assert!(b2.read(0, &mut v) && v.iter().all(|&x| x == 0xAB));
    assert_eq!(errors(&env), Vec::<String>::new());
}

#[test]
fn cancel_in_the_middle_turns_the_td_into_noops() {
    let (c, env, _hp) = up("t-x-cancel", Cfg::qemu(), |m| m.attach_root(1, FakeDev::storage(Speed::High)));
    let d = stick(&c);
    let path = vec![1u8];
    let bufs: Vec<Buffer> = (0..3).map(|_| d.alloc_buffer(64).unwrap()).collect();
    let ts: Vec<_> = bufs.iter().map(|b| d.submit(IN, Request::whole(b.clone())).unwrap()).collect();
    assert_eq!(ts[1].cancel(), XferResult::failed(UsbError::Cancelled, 0));
    assert!(!ts[0].is_done() && !ts[2].is_done(), "the others keep their place");
    {
        let mut m = env.model();
        let q = m.dev(&path).unwrap().in_q.entry(IN).or_default();
        q.push_back(vec![1; 10]);
        q.push_back(vec![3; 10]);
    }
    pump(&c, &env);
    assert_eq!(ts[0].wait(T), XferResult::ok(10));
    assert_eq!(ts[2].wait(T), XferResult::ok(10));
    let mut v = [0u8; 10];
    assert!(bufs[0].read(0, &mut v) && v == [1; 10]);
    assert!(bufs[2].read(0, &mut v) && v == [3; 10]);
    assert!(bufs[1].read(0, &mut v) && v == [0; 10], "the cancelled TD got nothing");
    assert_eq!(env.model().usb.noops, 1, "its TRB passed as a No-Op");
    // Cancelling the head: stop and skip; the transfer behind it runs.
    let a = d.submit(IN, Request::whole(bufs[0].clone())).unwrap();
    let b = d.submit(IN, Request::whole(bufs[1].clone())).unwrap();
    assert_eq!(a.cancel().error, Some(UsbError::Cancelled));
    env.model().dev(&path).unwrap().in_q.entry(IN).or_default().push_back(vec![5; 3]);
    pump(&c, &env);
    assert_eq!(b.wait(T), XferResult::ok(3));
    assert_eq!(errors(&env), Vec::<String>::new());
}

#[test]
fn stall_then_clear_halt_resets_both_toggles() {
    let (c, env, _hp) = up("t-x-stall", Cfg::minipc(), |m| m.attach_root(2, FakeDev::storage(Speed::High)));
    let d = stick(&c);
    let path = vec![2u8];
    // An odd number of packets first, so the toggles stand at 1 on both sides.
    env.model().dev(&path).unwrap().in_q.entry(IN).or_default().push_back(vec![1; 10]);
    assert_eq!(d.in_bytes(IN, 64, T).unwrap().len(), 10);
    env.model().dev(&path).unwrap().stall_next.push(IN);
    let buf = d.alloc_buffer(64).unwrap();
    let failed = d.submit(IN, Request::whole(buf.clone())).unwrap();
    let behind = d.submit(IN, Request::whole(buf.clone())).unwrap();
    assert_eq!(failed.wait(T), XferResult::failed(UsbError::Stall, 0));
    env.model().dev(&path).unwrap().in_q.entry(IN).or_default().push_back(vec![2; 20]);
    pump(&c, &env);
    assert!(!behind.is_done(), "the endpoint stays halted until the driver clears it");
    let resets = env.model().usb.toggle_resets.len();
    d.clear_halt(IN).unwrap();
    let m = env.model();
    let n = m.usb.commands.len();
    assert_eq!(&m.usb.commands[n - 2..], &[ty::RESET_ENDPOINT, ty::SET_TR_DEQUEUE]);
    assert_eq!(m.usb.toggle_resets.len(), resets + 1, "host toggle back to 0");
    assert_eq!(m.usb.root[&2].clear_halts, vec![IN], "CLEAR_FEATURE(ENDPOINT_HALT) to the device, before the host restarts");
    drop(m);
    assert_eq!(behind.wait(T), XferResult::ok(20), "the transfer behind the stall runs");
    assert_eq!(errors(&env), Vec::<String>::new(), "toggles in step after the reset");
}

#[test]
fn reset_of_a_running_endpoint_uses_configure_drop_add() {
    let (c, env, _hp) = up("t-x-reset", Cfg::qemu(), |m| m.attach_root(1, FakeDev::storage(Speed::High)));
    let d = stick(&c);
    let path = vec![1u8];
    env.model().dev(&path).unwrap().in_q.entry(IN).or_default().push_back(vec![1; 10]);
    assert_eq!(d.in_bytes(IN, 64, T).unwrap().len(), 10);
    let queued = d.submit(IN, Request::whole(d.alloc_buffer(64).unwrap())).unwrap();
    d.clear_halt(IN).unwrap();
    let m = env.model();
    let (dropped, added) = *m.usb.configures.last().unwrap();
    assert_eq!((dropped, added), (1 << 3, 1 | 1 << 3), "DCI 3 dropped and added in one Configure Endpoint");
    let n = m.usb.commands.len();
    assert_eq!(&m.usb.commands[n - 2..], &[ty::STOP_ENDPOINT, ty::CONFIGURE_ENDPOINT]);
    drop(m);
    env.model().dev(&path).unwrap().in_q.entry(IN).or_default().push_back(vec![4; 4]);
    pump(&c, &env);
    assert_eq!(queued.wait(T), XferResult::ok(4), "the queued TD survived the reset");
    assert_eq!(errors(&env), Vec::<String>::new(), "toggles 0 on both sides afterwards");
}

#[test]
fn abort_cancels_everything() {
    let (c, env, _hp) = up("t-x-abort", Cfg::minipc(), |m| m.attach_root(2, FakeDev::storage(Speed::High)));
    let d = stick(&c);
    let path = vec![2u8];
    let done = Arc::new(StdMutex::new(Vec::new()));
    let ts: Vec<_> = (0..5).map(|i| {
        let dn = done.clone();
        d.submit_then(IN, Request::whole(d.alloc_buffer(32).unwrap()), alloc::boxed::Box::new(move |r: &XferResult| dn.lock().unwrap().push((i, *r)))).unwrap()
    }).collect();
    assert_eq!(d.abort(IN), Ok(5));
    assert!(ts.iter().all(|t| t.result() == Some(XferResult::failed(UsbError::Cancelled, 0))));
    assert_eq!(done.lock().unwrap().len(), 5, "every callback ran once");
    env.model().dev(&path).unwrap().in_q.entry(IN).or_default().push_back(vec![6; 6]);
    assert_eq!(d.in_bytes(IN, 32, T).unwrap(), vec![6; 6], "the endpoint works afterwards");
    assert_eq!(errors(&env), Vec::<String>::new());
}

#[test]
fn completion_callback_submits_the_next_transfer() {
    let (c, env, _hp) = up("t-x-chain", Cfg::qemu(), |m| m.attach_root(1, FakeDev::storage(Speed::High)));
    let d = stick(&c);
    let path = vec![1u8];
    let second: Arc<StdMutex<Option<usb_core::Transfer>>> = Arc::new(StdMutex::new(None));
    let (d2, s2) = (d.clone(), second.clone());
    let b = d.alloc_buffer(64).unwrap();
    let b2 = b.clone();
    let first = d.submit_then(OUT, Request::new(b, 0, 31), alloc::boxed::Box::new(move |r: &XferResult| {
        assert!(r.error.is_none());
        *s2.lock().unwrap() = Some(d2.submit(IN, Request::whole(b2.clone())).expect("submit from the callback"));
    })).unwrap();
    env.model().dev(&path).unwrap().in_q.entry(IN).or_default().push_back(vec![8; 13]);
    assert_eq!(first.wait(T), XferResult::ok(31));
    let t = second.lock().unwrap().take().expect("the callback queued the IN transfer");
    pump(&c, &env);
    assert_eq!(t.wait(T), XferResult::ok(13));
}

#[test]
fn interrupt_pipe_keeps_depth_queued_and_repairs_a_stall() {
    let (c, env, mut hp) = up("t-x-pipe", Cfg::minipc(), |m| m.attach_root(1, FakeDev::keyboard(Speed::Full)));
    let kbd = devs(&c).remove(0);
    let path = vec![1u8];
    let got = Arc::new(StdMutex::new(Vec::new()));
    let g = got.clone();
    let sink: usb_core::Sink = Arc::new(move |e: PipeEvent<'_>| match e {
        PipeEvent::Data(r) => g.lock().unwrap().push(Ok(r[0])),
        PipeEvent::Error(e) => g.lock().unwrap().push(Err(e)),
        PipeEvent::Stopped(e) => g.lock().unwrap().push(Err(e)),
    });
    let pipe = kbd.interrupt_pipe(IN, PipeOpts::new(8, 4), sink).unwrap();
    let live = |c: &C| c.slot_info(kbd.slot).map(|_| ()).is_some();
    assert!(live(&c));
    for i in 0..40u8 {
        env.model().dev(&path).unwrap().reports.push_back(vec![i; 8]);
    }
    pump(&c, &env);
    assert_eq!(*got.lock().unwrap(), (0..40).map(Ok).collect::<Vec<_>>(), "every report, in order");
    // A STALL: the worker resets the endpoint, clears the device's halt and arms again (4 TDs).
    got.lock().unwrap().clear();
    env.model().dev(&path).unwrap().stall_next.push(IN);
    env.model().dev(&path).unwrap().reports.push_back(vec![99; 8]);
    pump(&c, &env);
    hp.round(&c);
    pump(&c, &env);
    assert_eq!(*got.lock().unwrap(), vec![Err(UsbError::Stall), Ok(99)]);
    assert_eq!(env.model().dev(&path).unwrap().clear_halts, vec![IN]);
    // Four TDs queued again: four reports go through without any further doorbell from the driver.
    got.lock().unwrap().clear();
    for i in 0..4u8 {
        env.model().dev(&path).unwrap().reports.push_back(vec![i; 8]);
    }
    env.model().run_ep(kbd.slot.0 as u8, 3);
    c.process_events(Source::Waiter);
    assert_eq!(got.lock().unwrap().len(), 4);
    // Stopping: Stopped(Cancelled) once, and a new pipe can start.
    got.lock().unwrap().clear();
    pipe.stop().unwrap();
    assert_eq!(*got.lock().unwrap(), vec![Err(UsbError::Cancelled)]);
    let _again = kbd.interrupt_pipe(IN, PipeOpts::new(8, 2), Arc::new(|_e: PipeEvent<'_>| {})).unwrap();
    assert_eq!(errors(&env), Vec::<String>::new());
}

#[test]
fn device_gone_ends_queued_transfers() {
    let (c, env, mut hp) = up("t-x-gone", Cfg::minipc(), |m| m.attach_root(2, FakeDev::storage(Speed::High)));
    let d = stick(&c);
    let buf = d.alloc_buffer(64).unwrap();
    let t = d.submit(IN, Request::whole(buf.clone())).unwrap();
    env.model().detach_root(2);
    crate::tests_usb::settle(&c, &env, &mut hp);
    assert_eq!(t.result(), Some(XferResult::failed(UsbError::Gone, 0)));
    assert!(d.is_gone());
    assert_eq!(d.submit(IN, Request::whole(buf)).err(), Some(UsbError::Gone));
    assert!(registry::find(d.id).is_none());
}

struct Foreign;
impl XferMem for Foreign {
    fn len(&self) -> usize {
        64
    }
    fn read(&self, _: usize, _: &mut [u8]) -> bool {
        true
    }
    fn write(&self, _: usize, _: &[u8]) -> bool {
        true
    }
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

#[test]
fn foreign_buffers_and_bad_requests_are_refused() {
    let (c, _env, _hp) = up("t-x-refuse", Cfg::qemu(), |m| {
        m.attach_root(1, FakeDev::storage(Speed::High));
        m.attach_root(2, FakeDev::keyboard(Speed::Full));
    });
    let d = stick(&c);
    let f: Buffer = Arc::new(Foreign);
    assert!(matches!(d.submit(IN, Request::whole(f)).err(), Some(UsbError::Invalid(_))));
    let b = d.alloc_buffer(64).unwrap();
    assert!(matches!(d.submit(IN, Request::new(b.clone(), 60, 8)).err(), Some(UsbError::Invalid(_))), "range past the end");
    assert!(matches!(d.submit(0x83, Request::whole(b.clone())).err(), Some(UsbError::Invalid(_))), "no such endpoint");
    assert!(matches!(d.submit(0x00, Request::whole(b.clone())).err(), Some(UsbError::Invalid(_))), "endpoint 0");
    let kbd = devs(&c).into_iter().find(|x| x.vendor() == 0x0627).unwrap();
    assert!(kbd.interrupt_pipe(0x81, PipeOpts::new(8, 0), Arc::new(|_e: PipeEvent<'_>| {})).is_err());
    assert!(kbd.interrupt_pipe(0x81, PipeOpts::new(0x2_0000, 1), Arc::new(|_e: PipeEvent<'_>| {})).is_err());
    let _p = kbd.interrupt_pipe(0x81, PipeOpts::new(8, 2), Arc::new(|_e: PipeEvent<'_>| {})).unwrap();
    let kb = kbd.alloc_buffer(8).unwrap();
    assert_eq!(kbd.submit(0x81, Request::whole(kb)).err(), Some(UsbError::Busy), "a pipe owns the endpoint");
}

#[test]
fn ring_grows_under_a_consumer_in_both_places() {
    use crate::fake::FakeMem;
    use crate::ring::ProducerRing;
    use crate::trb::Trb;
    use std::collections::VecDeque;
    let env = FakeEnv::new(Cfg::qemu());
    // Grow after the first segment and after the last one (where the toggle moves) while the consumer lags.
    for grow_at in [10usize, 300] {
        let segs = vec![FakeMem::new(&env, 4096), FakeMem::new(&env, 4096)];
        let mut r = ProducerRing::new(segs).unwrap();
        let mut ep = crate::fake_usb::FEp { deq: r.base(), cycle: true, ..Default::default() };
        let mut queued: VecDeque<u64> = VecDeque::new();
        let mut next_read = 0u64;
        let mut read = |ep: &mut crate::fake_usb::FEp, upto: u64, r: &mut ProducerRing<FakeMem>, q: &mut VecDeque<u64>| {
            while next_read < upto {
                let (at, t, n) = env.model().peek(*ep).expect("a TRB handed over");
                assert_eq!((t.param, Some(at)), (next_read, q.pop_front()), "TRB order");
                *ep = n;
                next_read += 1;
                r.consumed(1);
            }
        };
        let mut pushed = 0u64;
        while pushed < grow_at as u64 {
            queued.push_back(r.push(Trb { param: pushed, status: 0, control: (ty::NORMAL as u32) << 10 }).unwrap());
            pushed += 1;
        }
        read(&mut ep, pushed - 5, &mut r, &mut queued);
        let extra = vec![FakeMem::new(&env, 4096), FakeMem::new(&env, 4096)];
        r.grow(extra, queued.front().copied()).unwrap();
        assert_eq!(r.segments(), 4);
        while pushed < 2000 {
            if !r.fits(1, queued.front().copied()) {
                read(&mut ep, pushed, &mut r, &mut queued);
            }
            queued.push_back(r.push(Trb { param: pushed, status: 0, control: (ty::NORMAL as u32) << 10 }).unwrap());
            pushed += 1;
        }
        read(&mut ep, pushed, &mut r, &mut queued);
        assert!(env.model().peek(ep).is_none(), "nothing after the last TRB");
        assert!(r.wraps() >= 1);
    }
    let _ = DmaMem::size(&FakeMem::new(&env, 64));
}

#[test]
fn a_ring_never_grows_with_the_controller_ahead_in_the_producers_segment() {
    use crate::fake::FakeMem;
    use crate::ring::{ProducerRing, RingError};
    use crate::trb::{Trb, CYCLE};
    use std::collections::VecDeque;
    let env = FakeEnv::new(Cfg::qemu());
    let normal = |i: u64| Trb { param: i, status: 0, control: (ty::NORMAL as u32) << 10 };
    // The checker's case, reached with plain pushes: 2 segments full (509), 200 consumed, 200 more. The producer
    // is in segment 0 at place 200, the controller there too at place 200 of its previous lap.
    let mut r = ProducerRing::new(vec![FakeMem::new(&env, 4096), FakeMem::new(&env, 4096)]).unwrap();
    let mut q: VecDeque<u64> = (0..509).map(|i| r.push(normal(i)).unwrap()).collect();
    let mut ep = crate::fake_usb::FEp { deq: r.base(), cycle: true, ..Default::default() };
    for i in 0..200 {
        let (at, t, n) = env.model().peek(ep).unwrap();
        assert_eq!((t.param, Some(at)), (i, q.pop_front()));
        ep = n;
        r.consumed(1);
    }
    for i in 509..709 {
        q.push_back(r.push(normal(i)).unwrap());
    }
    assert!(!r.fits(1, q.front().copied()), "the next TRB would land in front of the controller's lap");
    let extra = vec![FakeMem::new(&env, 4096)];
    assert_eq!(r.grow(extra, q.front().copied()), Err(RingError::Wrapped));
    assert_eq!(r.segments(), 2);
    // The controller still runs its lap unharmed: TRB 200 next.
    assert_eq!(env.model().peek(ep).unwrap().1.param, 200);

    // `fits` keeps a ring out of that position: it refuses to cross into the consumer's segment, and growing
    // then is allowed and makes room. 1 segment, 250 queued and 10 consumed: the next 5 cross the only link.
    let mut r = ProducerRing::new(vec![FakeMem::new(&env, 4096)]).unwrap();
    let mut q: VecDeque<u64> = (0..250).map(|i| r.push(normal(i)).unwrap()).collect();
    for _ in 0..10 {
        q.pop_front();
        r.consumed(1);
    }
    assert!(!r.fits(5, q.front().copied()) && r.has_room(5), "room by count, but across into the consumer");
    assert!(r.fits(4, q.front().copied()), "up to the link is fine");
    assert_eq!(r.segments_for(5), 1);
    let seg = FakeMem::new(&env, 4096);
    let new_at = seg.dev_addr();
    r.grow(vec![seg], q.front().copied()).unwrap();
    assert!(r.fits(5, q.front().copied()));
    // New segments are Transfer No-Ops the controller does not own yet (it would pass them harmlessly).
    let t = env.model().rd_trb(new_at + 16).unwrap();
    assert_eq!(t.trb_type(), ty::TRANSFER_NOOP);
    assert_eq!(t.control & CYCLE != 0, !r.cycle(), "not handed over");
}

#[test]
fn a_td_is_handed_over_by_its_first_trb_last() {
    use crate::fake::FakeMem;
    use crate::ring::ProducerRing;
    use crate::trb::{Trb, CYCLE};
    let env = FakeEnv::new(Cfg::qemu());
    let mut r = ProducerRing::new(vec![FakeMem::new(&env, 4096)]).unwrap();
    // Twice: once in the first lap (cycle 1), once across the link into the second (cycle 0).
    for (before, n) in [(0usize, 3usize), (250, 8)] {
        for i in 0..before {
            r.push(Trb { param: i as u64, status: 0, control: (ty::NORMAL as u32) << 10 }).unwrap();
            r.consumed(1);
        }
        let trbs: Vec<Trb> = (0..n).map(|i| Trb::normal(0x1000 * i as u64, 64, 0, i + 1 == n, true)).collect();
        env.model().mem_log = Some(Vec::new());
        let (addrs, cycle) = r.push_td(&trbs).unwrap();
        let log = env.model().mem_log.take().unwrap();
        let first_ctl = addrs[0] + 12;
        let (last_addr, last_v) = *log.last().unwrap();
        assert_eq!(last_addr, first_ctl, "the first TRB's control dword is the last write");
        assert_eq!(last_v & CYCLE != 0, cycle, "handed over with the producer's cycle");
        let early: Vec<_> = log.iter().filter(|(a, _)| *a == first_ctl).collect();
        assert!(early.len() == 2 && (early[0].1 & CYCLE != 0) != cycle, "written first with the cycle bit the controller does not own");
        r.consumed(n);
    }
}

#[test]
fn a_stopped_pipe_leaves_the_endpoint_to_transfers() {
    let (c, env, mut hp) = up("t-x-after-pipe", Cfg::qemu(), |m| m.attach_root(1, FakeDev::keyboard(Speed::Full)));
    let kbd = devs(&c).remove(0);
    let path = vec![1u8];
    let pipe = kbd.interrupt_pipe(IN, PipeOpts::new(8, 3), Arc::new(|_e: PipeEvent<'_>| {})).unwrap();
    pipe.stop().unwrap();
    // One-shot transfers on the same endpoint; the hotplug worker must not take them for the pipe's leftovers.
    let t = kbd.submit(IN, Request::whole(kbd.alloc_buffer(8).unwrap())).unwrap();
    hp.round(&c);
    hp.round(&c);
    assert!(!t.is_done(), "{:?}", t.result());
    env.model().dev(&path).unwrap().reports.push_back(vec![7; 8]);
    pump(&c, &env);
    assert_eq!(t.wait(T), XferResult::ok(8));
    assert_eq!(errors(&env), Vec::<String>::new());
}

#[test]
fn a_td_across_a_link_has_whole_packets_in_front_of_it() {
    let (c, env, _hp) = up("t-x-link", Cfg::minipc(), |m| m.attach_root(2, FakeDev::storage(Speed::High)));
    let d = stick(&c);
    let path = vec![2u8];
    let small = d.alloc_buffer(8).unwrap();
    let feed = |n: usize| {
        for _ in 0..n {
            env.model().dev(&path).unwrap().in_q.entry(IN).or_default().push_back(vec![1]);
            assert_eq!(d.transfer(IN, Request::whole(small.clone()), T), XferResult::ok(1));
        }
    };
    // 254 one-TRB transfers: the next TRB is the last place before the link (255 places per segment).
    feed(254);
    // 20 000 bytes 100 bytes into a buffer that starts 0x3000 below a 64 KiB boundary: 12 188 + 7 812 bytes.
    // 12 188 is no whole number of 512-byte packets: the first TRB is cut to 11 776, 412 go after the link.
    let buf = d.alloc_buffer(32 * 1024).unwrap();
    let data: Vec<u8> = (0..20_000u32).map(|i| (i % 253) as u8).collect();
    env.model().dev(&path).unwrap().in_q.entry(IN).or_default().push_back(data.clone());
    assert_eq!(d.transfer(IN, Request::new(buf.clone(), 100, 20_000), T), XferResult::ok(20_000));
    let mut got = vec![0u8; 20_000];
    assert!(buf.read(100, &mut got) && got == data);
    assert_eq!(errors(&env), Vec::<String>::new(), "whole packets in front of the link");
    // Again at the last place, now with a first piece of 100 bytes (shorter than its odd part): the rest of the
    // segment becomes a No-Op and the TD starts after the link.
    feed(255 - 3);
    let noops = env.model().usb.noops;
    env.model().dev(&path).unwrap().in_q.entry(IN).or_default().push_back(data[..5000].to_vec());
    assert_eq!(d.transfer(IN, Request::new(buf.clone(), 0x3000 - 100, 5000), T), XferResult::ok(5000));
    assert_eq!(env.model().usb.noops, noops + 1, "one padding No-Op");
    assert_eq!(errors(&env), Vec::<String>::new());
}

#[test]
fn a_pipe_with_an_interval_wish_reconfigures_only_its_endpoint() {
    // IN-2b: a low-speed keyboard (bInterval 10 -> Interval 6) asked for 1 ms gets Interval 3 by one Configure
    // Endpoint that drops and adds only DCI 3; reports flow on the new ring.
    let (c, env, _hp) = up("t-x-interval", Cfg::minipc(), |m| m.attach_root(1, FakeDev::keyboard(Speed::Low)));
    let kbd = devs(&c).remove(0);
    let before = env.model().usb.configures.len();
    let got = Arc::new(StdMutex::new(Vec::new()));
    let g = got.clone();
    let sink: usb_core::Sink = Arc::new(move |e: PipeEvent<'_>| {
        if let PipeEvent::Data(r) = e {
            g.lock().unwrap().push(r[0]);
        }
    });
    let _pipe = kbd.interrupt_pipe(IN, PipeOpts::new(8, 2).with_interval(1), sink).unwrap();
    let id = (kbd.slot.0 & 0xFF) as u8;
    {
        let mut m = env.model();
        assert_eq!(m.usb.configures[before..], [(1 << 3, 1 | 1 << 3)], "one Configure Endpoint for DCI 3 only");
        let out = m.usb.slots[&id].out;
        assert_eq!((m.out_get(out, 3, 0) >> 16) & 0xFF, 3, "Interval 2^3 x 125 us = 1 ms");
        for i in 0..5u8 {
            m.dev(&[1]).unwrap().reports.push_back(vec![i; 8]);
        }
    }
    pump(&c, &env);
    assert_eq!(*got.lock().unwrap(), (0..5).collect::<Vec<u8>>());
    assert_eq!(errors(&env), Vec::<String>::new());
}
