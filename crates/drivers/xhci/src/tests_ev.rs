//! US-ev: pipe events carry the interrupt's time; input and transfer ends go to their own threads.

use crate::fake::Cfg;
use crate::fake_usb::FakeDev;
use crate::hc::{Source, MS};
use crate::tests_usb::{devs, up, C};
use alloc::boxed::Box;
use alloc::sync::Arc;
use core::sync::atomic::Ordering;
use std::sync::Mutex as StdMutex;
use std::vec;
use std::vec::Vec;
use usb_core::{Device, PipeEvent, PipeOpts, Request, Speed, TimedSink, XferResult};

const IN: u8 = 0x81;

type Stamps = Arc<StdMutex<Vec<(u8, u64)>>>;

fn class(c: &C, class: u8) -> Arc<Device> {
    devs(c).into_iter().find(|d| d.config.as_ref().is_some_and(|x| x.defaults().any(|f| f.class == class))).expect("device")
}

fn timed(s: &Stamps) -> TimedSink {
    let s = s.clone();
    Arc::new(move |e: PipeEvent<'_>, at: u64| {
        if let PipeEvent::Data(r) = e {
            s.lock().unwrap().push((r[0], at));
        }
    })
}

fn time(env: &crate::fake::FakeEnv) -> u64 {
    env.model().time
}

#[test]
fn reports_carry_the_time_the_interrupt_was_noticed() {
    let (c, env, _hp) = up("t-ev-stamp", Cfg::minipc(), |m| m.attach_root(1, FakeDev::keyboard(Speed::Full)));
    let kbd = class(&c, 3);
    let got: Stamps = Arc::new(StdMutex::new(Vec::new()));
    let _p = kbd.interrupt_pipe_at(IN, PipeOpts::new(8, 2), timed(&got)).unwrap();
    // Noticed (poller, later MSI-X), then the event thread runs 3 ms later: the stamp is the notice.
    env.model().dev(&[1]).unwrap().reports.push_back(vec![1; 8]);
    env.model().pump();
    let noticed = time(&env);
    c.interrupt();
    env.model().time += 3 * MS;
    c.process_events(Source::Thread);
    assert_eq!(*got.lock().unwrap(), vec![(1, noticed)]);
    // Nothing noticed it (the thread's tick): the drain's start.
    env.model().dev(&[1]).unwrap().reports.push_back(vec![2; 8]);
    env.model().pump();
    env.model().time += MS;
    let drain = time(&env);
    c.process_events(Source::Thread);
    assert_eq!(got.lock().unwrap()[1], (2, drain));
    // A notice older than the last drain (it raced with that drain) never makes a stamp go back.
    c.irq_at.store(noticed, Ordering::Release);
    env.model().dev(&[1]).unwrap().reports.push_back(vec![3; 8]);
    env.model().pump();
    c.process_events(Source::Thread);
    let g = got.lock().unwrap();
    assert_eq!(g.len(), 3);
    assert!(g.windows(2).all(|w| w[0].1 <= w[1].1), "monotonic: {g:?}");
    assert!(g.iter().all(|&(_, t)| t <= time(&env)));
}

#[test]
fn an_untimed_pipe_still_gets_its_reports() {
    let (c, env, _hp) = up("t-ev-untimed", Cfg::minipc(), |m| m.attach_root(1, FakeDev::keyboard(Speed::Full)));
    let kbd = class(&c, 3);
    let got = Arc::new(StdMutex::new(Vec::new()));
    let g = got.clone();
    let _p = kbd.interrupt_pipe(IN, PipeOpts::new(8, 2), Arc::new(move |e: PipeEvent<'_>| {
        if let PipeEvent::Data(r) = e {
            g.lock().unwrap().push(r[0]);
        }
    })).unwrap();
    env.model().dev(&[1]).unwrap().reports.push_back(vec![7; 8]);
    env.model().pump();
    c.process_events(Source::Waiter);
    assert_eq!(*got.lock().unwrap(), vec![7]);
}

#[test]
fn input_stays_with_the_event_thread_and_storage_with_the_completion_thread() {
    let (c, env, _hp) = up("t-ev-split", Cfg::minipc(), |m| {
        m.attach_root(1, FakeDev::keyboard(Speed::Full));
        m.attach_root(2, FakeDev::storage(Speed::High));
    });
    let (kbd, stick) = (class(&c, 3), class(&c, 8));
    let got: Stamps = Arc::new(StdMutex::new(Vec::new()));
    let _p = kbd.interrupt_pipe_at(IN, PipeOpts::new(8, 2), timed(&got)).unwrap();
    let done = Arc::new(StdMutex::new(Vec::new()));
    let read = |n: u8| {
        let d = done.clone();
        stick.submit_then(IN, Request::whole(stick.alloc_buffer(64).unwrap()), Box::new(move |r: &XferResult| d.lock().unwrap().push((n, *r)))).unwrap()
    };
    // Both threads run from here on.
    c.set_thread_alive();
    c.set_done_alive();
    let _t1 = read(1);
    env.model().dev(&[2]).unwrap().in_q.entry(IN).or_default().push_back(vec![9; 16]);
    env.model().dev(&[1]).unwrap().reports.push_back(vec![1; 8]);
    env.model().pump();
    // A waiter drains (fallback): it takes the transfer end it may wait for, the report waits for the event thread.
    c.process_events(Source::Waiter);
    assert_eq!(*done.lock().unwrap(), vec![(1, XferResult::ok(16))]);
    assert!(got.lock().unwrap().is_empty(), "a report delivered on a waiter's thread");
    assert!(c.reports.ready(), "the event thread has something to do");
    c.thread_round();
    assert_eq!(got.lock().unwrap().len(), 1);
    // The event thread drains a transfer end: it is left to the completion thread.
    let _t2 = read(2);
    env.model().dev(&[2]).unwrap().in_q.entry(IN).or_default().push_back(vec![9; 32]);
    env.model().dev(&[1]).unwrap().reports.push_back(vec![2; 8]);
    env.model().pump();
    c.thread_round();
    assert_eq!(got.lock().unwrap().len(), 2, "the report at once");
    assert_eq!(done.lock().unwrap().len(), 1, "a storage callback ran on the event thread");
    assert!(c.completions.ready());
    c.completion_round();
    assert_eq!(done.lock().unwrap()[1], (2, XferResult::ok(32)));
    assert!(!c.completions.ready() && !c.reports.ready());
}

#[test]
fn without_the_completion_thread_the_event_thread_delivers_transfer_ends() {
    let (c, env, _hp) = up("t-ev-nodone", Cfg::minipc(), |m| {
        m.attach_root(1, FakeDev::keyboard(Speed::Full));
        m.attach_root(2, FakeDev::storage(Speed::High));
    });
    let (kbd, stick) = (class(&c, 3), class(&c, 8));
    let got: Stamps = Arc::new(StdMutex::new(Vec::new()));
    let _p = kbd.interrupt_pipe_at(IN, PipeOpts::new(8, 2), timed(&got)).unwrap();
    let done = Arc::new(StdMutex::new(Vec::new()));
    let d = done.clone();
    // The completion thread failed to spawn: only the event thread runs (driver.rs then makes it Interactive).
    c.set_thread_alive();
    let _t = stick.submit_then(IN, Request::whole(stick.alloc_buffer(64).unwrap()), Box::new(move |r: &XferResult| d.lock().unwrap().push(*r))).unwrap();
    env.model().dev(&[2]).unwrap().in_q.entry(IN).or_default().push_back(vec![9; 16]);
    env.model().dev(&[1]).unwrap().reports.push_back(vec![1; 8]);
    env.model().pump();
    c.thread_round();
    assert_eq!(got.lock().unwrap().len(), 1, "the report");
    assert_eq!(*done.lock().unwrap(), vec![XferResult::ok(16)], "the transfer end must not stall");
    assert!(!c.completions.ready() && !c.reports.ready());
}

type Events = Arc<StdMutex<Vec<(Result<u8, usb_core::UsbError>, u64)>>>;

fn timed_all(s: &Events) -> TimedSink {
    let s = s.clone();
    Arc::new(move |e: PipeEvent<'_>, at: u64| {
        let v = match e {
            PipeEvent::Data(r) => Ok(r[0]),
            PipeEvent::Error(e) | PipeEvent::Stopped(e) => Err(e),
        };
        s.lock().unwrap().push((v, at));
    })
}

fn stamps_ok(got: &Events, env: &crate::fake::FakeEnv, after: u64) {
    assert!(after > 0, "the fake clock runs, so a stamp of 0 would show");
    let g = got.lock().unwrap();
    assert!(g.windows(2).all(|w| w[0].1 <= w[1].1), "monotonic: {g:?}");
    assert!(g.iter().all(|&(_, t)| t >= after && t <= time(env)), "inside start..now: {g:?}");
}

#[test]
fn errors_and_stops_are_stamped_and_go_through_the_event_thread() {
    use usb_core::UsbError;
    let (c, env, mut hp) = up("t-ev-errstop", Cfg::minipc(), |m| m.attach_root(1, FakeDev::keyboard(Speed::Full)));
    let kbd = class(&c, 3);
    let got: Events = Arc::new(StdMutex::new(Vec::new()));
    let pipe = kbd.interrupt_pipe_at(IN, PipeOpts::new(8, 2), timed_all(&got)).unwrap();
    c.set_thread_alive();
    c.set_done_alive();
    let start = time(&env);
    // A STALL: Error(Stall) stamped like a report, then the repaired pipe's report.
    env.model().dev(&[1]).unwrap().stall_next.push(IN);
    env.model().dev(&[1]).unwrap().reports.push_back(vec![99; 8]);
    env.model().pump();
    env.model().time += MS;
    c.thread_round();
    hp.round(&c);
    env.model().pump();
    env.model().time += MS;
    c.thread_round();
    let firsts: Vec<_> = got.lock().unwrap().iter().map(|e| e.0).collect();
    assert_eq!(firsts, vec![Err(UsbError::Stall), Ok(99)]);
    // Stopping from another thread: Stopped waits for the event thread, then arrives with a later stamp.
    env.model().time += MS;
    pipe.stop().unwrap();
    assert_eq!(got.lock().unwrap().len(), 2, "Stopped delivered off the event thread");
    assert!(c.reports.ready());
    c.thread_round();
    assert_eq!(got.lock().unwrap().last().unwrap().0, Err(UsbError::Cancelled));
    stamps_ok(&got, &env, start);
    assert!(!c.reports.ready() && !c.completions.ready());
}

#[test]
fn an_unplug_stops_the_pipe_and_flushes_pending_transfer_ends() {
    use usb_core::UsbError;
    let (c, env, mut hp) = up("t-ev-unplug", Cfg::minipc(), |m| {
        m.attach_root(1, FakeDev::keyboard(Speed::Full));
        m.attach_root(2, FakeDev::storage(Speed::High));
    });
    let (kbd, stick) = (class(&c, 3), class(&c, 8));
    let got: Events = Arc::new(StdMutex::new(Vec::new()));
    let _p = kbd.interrupt_pipe_at(IN, PipeOpts::new(8, 2), timed_all(&got)).unwrap();
    let done = Arc::new(StdMutex::new(Vec::new()));
    let d = done.clone();
    c.set_thread_alive();
    c.set_done_alive();
    let start = time(&env);
    let _t = stick.submit_then(IN, Request::whole(stick.alloc_buffer(64).unwrap()), Box::new(move |r: &XferResult| d.lock().unwrap().push(*r))).unwrap();
    env.model().detach_root(2);
    env.model().detach_root(1);
    crate::tests_usb::settle(&c, &env, &mut hp);
    // The hotplug path is not the event thread: it hands the transfer end on itself, the pipe's end waits.
    assert_eq!(*done.lock().unwrap(), vec![XferResult::failed(UsbError::Gone, 0)]);
    assert!(!c.completions.ready(), "a transfer end left behind after the unplug");
    c.thread_round();
    let g: Vec<_> = got.lock().unwrap().iter().map(|e| e.0).collect();
    assert_eq!(g.last(), Some(&Err(UsbError::Gone)), "{g:?}");
    stamps_ok(&got, &env, start);
    assert!(!c.reports.ready());
}
