//! US-3 host tests for the races and edge cases found in review: an abort with a transfer queued while its
//! commands run, a cancel whose transfer's result is still on its way, results delivered in order by one
//! context, a stale pipe handle, errors that leave the endpoint running, a toggle reset that catches a transfer
//! under way, huge buffer lengths, and a pipe deeper than a ring segment.

use crate::fake::{Cfg, FakeEnv, Model};
use crate::fake_usb::FakeDev;
use crate::hc::{Source, MS};
use crate::tests_usb::{devs, errors, up, C};
use crate::trb::{cc, ty};
use alloc::boxed::Box;
use alloc::string::String;
use alloc::sync::Arc;
use core::sync::atomic::Ordering;
use std::sync::Mutex as StdMutex;
use std::vec;
use std::vec::Vec;
use usb_core::{Device, PipeEvent, PipeOpts, Request, Speed, Transfer, UsbError, XferResult};

const IN: u8 = 0x81;
const OUT: u8 = 0x02;
const T: u64 = 1000 * MS;

fn stick(c: &C) -> Arc<Device> {
    devs(c).into_iter().find(|d| d.config.as_ref().is_some_and(|x| x.defaults().any(|f| f.class == 8))).expect("a stick")
}

fn pump(c: &C, env: &FakeEnv) {
    env.model().pump();
    c.process_events(Source::Waiter);
}

type Got = Arc<StdMutex<Vec<Result<u8, UsbError>>>>;

fn sink(g: &Got) -> usb_core::Sink {
    let g = g.clone();
    Arc::new(move |e: PipeEvent<'_>| match e {
        PipeEvent::Data(r) => g.lock().unwrap().push(Ok(r[0])),
        PipeEvent::Error(e) | PipeEvent::Stopped(e) => g.lock().unwrap().push(Err(e)),
    })
}

#[test]
fn abort_keeps_a_transfer_queued_while_its_commands_run() {
    let (c, env, _hp) = up("t-f-abort-late", Cfg::qemu(), |m| m.attach_root(1, FakeDev::storage(Speed::High)));
    let d = stick(&c);
    let path = vec![1u8];
    // Two IN transfers wait (no data: NAK); an OUT transfer waits too, and its callback queues a third IN one.
    let ins: Vec<Transfer> = (0..2).map(|_| d.submit(IN, Request::whole(d.alloc_buffer(64).unwrap())).unwrap()).collect();
    env.model().dev(&path).unwrap().nak_out.push(OUT);
    let late: Arc<StdMutex<Option<Transfer>>> = Arc::new(StdMutex::new(None));
    let lb = d.alloc_buffer(64).unwrap();
    let (d2, l2, lb2) = (d.clone(), late.clone(), lb.clone());
    let out = d.submit_then(OUT, Request::new(d.alloc_buffer(8).unwrap(), 0, 4), Box::new(move |_r: &XferResult| {
        *l2.lock().unwrap() = Some(d2.submit(IN, Request::whole(lb2.clone())).expect("submit from the callback"));
    })).unwrap();
    // The OUT endpoint answers while the abort's Set TR Dequeue runs: the callback queues behind the skip target.
    env.model().usb.before_cmd = Some((ty::SET_TR_DEQUEUE, Box::new(|m: &mut Model| m.dev(&[1]).unwrap().nak_out.clear())));
    assert_eq!(d.abort(IN), Ok(2));
    assert!(env.model().usb.before_cmd.is_none(), "the device answered during the abort");
    assert!(ins.iter().all(|t| t.result() == Some(XferResult::failed(UsbError::Cancelled, 0))));
    assert_eq!(out.result(), Some(XferResult::ok(4)));
    let late = late.lock().unwrap().take().expect("queued from the callback during the abort");
    assert!(!late.is_done(), "not ended with the transfers queued before the abort: {:?}", late.result());
    env.model().dev(&path).unwrap().in_q.entry(IN).or_default().push_back(vec![7; 9]);
    pump(&c, &env);
    assert_eq!(late.wait(T), XferResult::ok(9), "it runs from where the skip left the ring");
    let mut v = [0u8; 9];
    assert!(lb.read(0, &mut v) && v == [7; 9]);
    // The ring's count is right: a deep queue afterwards neither overruns nor stalls.
    let bufs: Vec<_> = (0..300).map(|_| d.alloc_buffer(8).unwrap()).collect();
    let ts: Vec<_> = bufs.iter().map(|b| d.submit(IN, Request::whole(b.clone())).unwrap()).collect();
    for i in 0..300u32 {
        env.model().dev(&path).unwrap().in_q.entry(IN).or_default().push_back(i.to_le_bytes().to_vec());
    }
    pump(&c, &env);
    assert!(ts.iter().all(|t| t.wait(T) == XferResult::ok(4)));
    assert_eq!(errors(&env), Vec::<String>::new());
}

#[test]
fn a_cancel_keeps_a_result_that_is_on_its_way() {
    let (c, env, _hp) = up("t-f-late-result", Cfg::minipc(), |m| m.attach_root(2, FakeDev::storage(Speed::High)));
    let d = stick(&c);
    let t = d.submit(IN, Request::whole(d.alloc_buffer(64).unwrap())).unwrap();
    env.model().dev(&[2]).unwrap().in_q.entry(IN).or_default().push_back(vec![3; 10]);
    // Another context is handing results on: the completion is queued but not yet delivered.
    c.completions.busy.store(true, Ordering::SeqCst);
    pump(&c, &env);
    assert!(!t.is_done());
    // The cancel finds the TD gone; the other context finishes while the cancel waits for it.
    let c2 = c.clone();
    *env.on_wait.lock().unwrap() = Some(Box::new(move || {
        c2.completions.busy.store(false, Ordering::SeqCst);
        c2.deliver_pending();
    }));
    assert_eq!(t.cancel(), XferResult::ok(10), "the real result, not Cancelled");
    // Nobody delivering any more and nothing queued: a cancel of a finished transfer returns its result at once.
    assert_eq!(t.cancel(), XferResult::ok(10));
}

#[test]
fn results_are_delivered_in_order_by_one_context() {
    let (c, env, _hp) = up("t-f-order", Cfg::minipc(), |m| m.attach_root(1, FakeDev::keyboard(Speed::Full)));
    let kbd = devs(&c).remove(0);
    let got: Got = Arc::new(StdMutex::new(Vec::new()));
    let pipe = kbd.interrupt_pipe(IN, PipeOpts::new(8, 4), sink(&got)).unwrap();
    c.reports.busy.store(true, Ordering::SeqCst);
    for i in 0..3u8 {
        env.model().dev(&[1]).unwrap().reports.push_back(vec![i; 8]);
    }
    pump(&c, &env);
    pipe.stop().unwrap();
    assert!(got.lock().unwrap().is_empty(), "another context holds the turn: everything waits for it");
    c.reports.busy.store(false, Ordering::SeqCst);
    c.deliver_pending();
    assert_eq!(*got.lock().unwrap(), vec![Ok(0), Ok(1), Ok(2), Err(UsbError::Cancelled)], "the reports, then Stopped last");
}

#[test]
fn a_stale_pipe_handle_stops_nothing() {
    let (c, env, _hp) = up("t-f-stale-pipe", Cfg::qemu(), |m| m.attach_root(1, FakeDev::keyboard(Speed::Full)));
    let kbd = devs(&c).remove(0);
    let got: Got = Arc::new(StdMutex::new(Vec::new()));
    let pipe = kbd.interrupt_pipe(IN, PipeOpts::new(8, 2), sink(&got)).unwrap();
    pipe.stop().unwrap();
    let t = kbd.submit(IN, Request::whole(kbd.alloc_buffer(8).unwrap())).unwrap();
    pipe.stop().unwrap();
    assert!(!t.is_done(), "the transfer queued after the pipe ended stays: {:?}", t.result());
    env.model().dev(&[1]).unwrap().reports.push_back(vec![5; 8]);
    pump(&c, &env);
    assert_eq!(t.wait(T), XferResult::ok(8));
    assert_eq!(*got.lock().unwrap(), vec![Err(UsbError::Cancelled)], "the sink heard Stopped once");
    assert_eq!(errors(&env), Vec::<String>::new());
}

/// Posts an error event for the TD at the endpoint's dequeue pointer and moves the fake past it (the endpoint
/// keeps running, as after a Missed Service or Data Buffer error).
fn fail_head(env: &FakeEnv, slot: u8, dci: usize, code: u8) {
    let mut m = env.model();
    let at = m.usb.slots[&slot].eps[dci].deq;
    let len = m.rd_trb(at).unwrap().status & 0x1_FFFF;
    m.usb.slots.get_mut(&slot).unwrap().eps[dci].deq = at + 16;
    m.transfer_event(slot, dci as u8, at, code, len);
}

#[test]
fn an_error_that_does_not_halt_leaves_the_queue_running() {
    let (c, env, _hp) = up("t-f-no-halt", Cfg::minipc(), |m| m.attach_root(2, FakeDev::storage(Speed::High)));
    let d = stick(&c);
    let t = d.submit(IN, Request::whole(d.alloc_buffer(64).unwrap())).unwrap();
    fail_head(&env, d.slot.0 as u8, 3, cc::DATA_BUFFER);
    c.process_events(Source::Waiter);
    assert_eq!(t.result(), Some(XferResult::failed(UsbError::Transfer(cc::DATA_BUFFER), 0)));
    env.model().dev(&[2]).unwrap().in_q.entry(IN).or_default().push_back(vec![5; 5]);
    assert_eq!(d.transfer(IN, Request::whole(d.alloc_buffer(64).unwrap()), T), XferResult::ok(5), "the next one gets its doorbell");
    assert_eq!(errors(&env), Vec::<String>::new());
}

#[test]
fn a_pipe_error_that_does_not_halt_is_rearmed_without_a_reset() {
    let (c, env, mut hp) = up("t-f-pipe-missed", Cfg::minipc(), |m| m.attach_root(1, FakeDev::keyboard(Speed::Full)));
    let kbd = devs(&c).remove(0);
    let got: Got = Arc::new(StdMutex::new(Vec::new()));
    let _pipe = kbd.interrupt_pipe(IN, PipeOpts::new(8, 2), sink(&got)).unwrap();
    let cmds = env.model().usb.commands.len();
    fail_head(&env, kbd.slot.0 as u8, 3, 23); // Missed Service Error
    c.process_events(Source::Waiter);
    hp.round(&c);
    assert_eq!(&env.model().usb.commands[cmds..], &[] as &[u8], "no Stop, Reset or Configure Endpoint");
    for i in 0..5u8 {
        env.model().dev(&[1]).unwrap().reports.push_back(vec![i; 8]);
    }
    pump(&c, &env);
    let g = got.lock().unwrap().clone();
    assert_eq!(g, vec![Err(UsbError::Transfer(23)), Ok(0), Ok(1), Ok(2), Ok(3), Ok(4)]);
    assert_eq!(errors(&env), Vec::<String>::new(), "toggles still in step");
}

#[test]
fn a_toggle_reset_ends_a_transfer_under_way_instead_of_repeating_it() {
    let (c, env, _hp) = up("t-f-reset-partial", Cfg::qemu(), |m| m.attach_root(1, FakeDev::storage(Speed::High)));
    let d = stick(&c);
    let path = vec![1u8];
    env.model().dev(&path).unwrap().nak_out.push(OUT);
    let big = d.alloc_buffer(4096).unwrap();
    assert!(big.write(0, &[0xEE; 4096]));
    let first = d.submit(OUT, Request::whole(big)).unwrap();
    let small = d.alloc_buffer(8).unwrap();
    assert!(small.write(0, &[1, 2, 3, 4]));
    let second = d.submit(OUT, Request::new(small, 0, 4)).unwrap();
    // The stop of the reset catches the first transfer after 1024 of its bytes went out.
    env.model().usb.stop_moved = Some(1024);
    d.clear_halt(OUT).unwrap();
    assert_eq!(first.result(), Some(XferResult::failed(UsbError::Cancelled, 1024)));
    assert!(!second.is_done());
    env.model().dev(&path).unwrap().nak_out.clear();
    pump(&c, &env);
    assert_eq!(second.wait(T), XferResult::ok(4));
    let seen: Vec<Vec<u8>> = env.model().dev(&path).unwrap().out_seen.iter().map(|(_, v)| v.clone()).collect();
    assert_eq!(seen, vec![vec![1, 2, 3, 4]], "the first transfer's data does not go out again from its start");
    // A stop that caught nothing moved: the transfer stays and runs whole.
    env.model().dev(&path).unwrap().nak_out.push(OUT);
    let third = d.submit(OUT, Request::new(d.alloc_buffer(8).unwrap(), 0, 8)).unwrap();
    d.clear_halt(OUT).unwrap();
    assert!(!third.is_done());
    env.model().dev(&path).unwrap().nak_out.clear();
    pump(&c, &env);
    assert_eq!(third.wait(T), XferResult::ok(8));
    assert_eq!(errors(&env), Vec::<String>::new());
}

#[test]
fn huge_buffer_lengths_are_refused_without_a_panic() {
    let (c, _env, _hp) = up("t-f-huge", Cfg::qemu(), |m| m.attach_root(1, FakeDev::storage(Speed::High)));
    let d = stick(&c);
    assert!(d.alloc_buffer(usize::MAX).is_err());
    assert!(d.alloc_buffer(isize::MAX as usize + 1).is_err());
    assert!(c.alloc_xfer_buffer(usize::MAX, true).is_err());
    assert!(d.in_bytes(IN, usize::MAX, T).is_err());
}

#[test]
fn a_pipe_deeper_than_a_segment_keeps_every_buffer_queued() {
    let (c, env, _hp) = up("t-f-deep-pipe", Cfg::qemu(), |m| m.attach_root(1, FakeDev::keyboard(Speed::Full)));
    let kbd = devs(&c).remove(0);
    let got: Got = Arc::new(StdMutex::new(Vec::new()));
    let _pipe = kbd.interrupt_pipe(IN, PipeOpts::new(8, 300), sink(&got)).unwrap();
    for i in 0..900u32 {
        env.model().dev(&[1]).unwrap().reports.push_back(vec![i as u8; 8]);
    }
    for _ in 0..200 {
        if got.lock().unwrap().len() >= 900 {
            break;
        }
        pump(&c, &env);
    }
    let g = got.lock().unwrap().clone();
    assert_eq!(g, (0..900u32).map(|i| Ok(i as u8)).collect::<Vec<_>>());
    let e = c.entry(kbd.slot).unwrap();
    let mut x = e.xfer.lock();
    let r = x.ep_ring(3).unwrap();
    assert_eq!((r.tds.len(), r.ring.segments()), (300, 3), "all 300 queued again, on 1 + 2 segments");
    drop(x);
    assert_eq!(errors(&env), Vec::<String>::new());
}

#[test]
fn the_worker_arms_a_pipe_buffer_again_that_a_reset_ended() {
    let (c, env, mut hp) = up("t-f-pipe-rearm", Cfg::minipc(), |m| m.attach_root(1, FakeDev::keyboard(Speed::Full)));
    let kbd = devs(&c).remove(0);
    let got: Got = Arc::new(StdMutex::new(Vec::new()));
    let _pipe = kbd.interrupt_pipe(IN, PipeOpts::new(8, 2), sink(&got)).unwrap();
    let live = || {
        let e = c.entry(kbd.slot).unwrap();
        let mut x = e.xfer.lock();
        x.ep_ring(3).unwrap().tds.iter().filter(|t| t.state == crate::queue::TdState::Live).count()
    };
    assert_eq!(live(), 2);
    // A toggle reset catches the head report under way: that TD ends, its buffer is not queued for a moment.
    env.model().usb.stop_moved = Some(4);
    kbd.clear_halt(IN).unwrap();
    assert_eq!(live(), 1);
    hp.round(&c);
    assert_eq!(live(), 2, "the worker queued it again");
    for i in 0..4u8 {
        env.model().dev(&[1]).unwrap().reports.push_back(vec![i; 8]);
    }
    pump(&c, &env);
    assert_eq!(*got.lock().unwrap(), vec![Ok(0), Ok(1), Ok(2), Ok(3)], "a partial report is dropped, not delivered");
    assert_eq!(errors(&env), Vec::<String>::new());
}
