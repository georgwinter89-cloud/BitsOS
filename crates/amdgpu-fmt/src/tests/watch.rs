//! The job timeout and the hung-ring verdict, on a fake clock.

use std::string::ToString;
use std::vec;
use std::vec::Vec;

use crate::bus::SimMem;
use crate::errno;
use crate::pm4;
use crate::ring::{Kind, Ring};
use crate::sim::{Engine, Hang, Isa, Stop};
use crate::watch::{
    Cause, IbWords, JobMeta, Owner, RingCfg, RingClass, RingId, RingState, RingWatch, Sample, TimeoutClass,
};

const T: u64 = 1_000;

pub fn client(ctx: u64) -> JobMeta {
    JobMeta { owner: Owner::Client { ctx, entity: ctx * 10 }, vmid: 3, pasid: 77 }
}

fn cfg(kind: Kind, size_dw: u64) -> RingCfg {
    let mut c = RingCfg::for_780m("t", RingClass::GfxDirect, size_dw, 0, 16);
    c.kind = kind;
    c.timeout_us = T;
    c
}

fn watch() -> RingWatch {
    RingWatch::new(RingId(0), cfg(Kind::Gfx, 1024)).unwrap()
}

fn hung(s: RingState) -> bool {
    matches!(s, RingState::Hung(_))
}

#[test]
fn linux_timeouts_and_bad_configurations() {
    assert_eq!(TimeoutClass::Gfx.linux_us(), 10_000_000);
    assert_eq!(TimeoutClass::Sdma.linux_us(), 10_000_000);
    assert_eq!(TimeoutClass::Video.linux_us(), 10_000_000);
    assert_eq!(TimeoutClass::Compute.linux_us(), 60_000_000);
    assert!(RingWatch::new(RingId(0), cfg(Kind::Gfx, 1000)).is_none(), "ring size");
    let mut c = cfg(Kind::Gfx, 1024);
    c.timeout_us = 0;
    assert!(RingWatch::new(RingId(0), c).is_none(), "timeout");
}

#[test]
fn verdict_comes_exactly_at_the_timeout_and_later_jobs_do_not_postpone_it() {
    let mut w = watch();
    let s = Sample { rptr_hw: 0, fence: 0 };
    assert_eq!(w.poll(0, s).state, RingState::Idle);
    assert_eq!(w.submit(client(1), 0, 256, 100).unwrap(), 1);
    assert_eq!(w.poll(100, s).state, RingState::Waiting { remaining_us: T });
    assert_eq!(w.submit(client(1), 256, 512, 900).unwrap(), 2);
    assert_eq!(w.poll(100 + T - 1, s).state, RingState::Waiting { remaining_us: 1 });
    assert_eq!(w.poll(100 + T, s).state, RingState::Hung(Cause::Timeout { stalled_us: T, timeout_us: T }));
    // Stays hung while nothing moves, and says for how long.
    assert_eq!(w.poll(100 + 5 * T, s).state, RingState::Hung(Cause::Timeout { stalled_us: 5 * T, timeout_us: T }));
}

#[test]
fn fence_or_read_pointer_progress_restarts_the_timer() {
    let mut w = watch();
    w.submit(client(1), 0, 256, 0).unwrap();
    w.submit(client(1), 256, 512, 0).unwrap();
    // The read pointer moves inside the window.
    let p = w.poll(T - 1, Sample { rptr_hw: 16, fence: 0 });
    assert_eq!(p.state, RingState::Waiting { remaining_us: T });
    assert_eq!(w.rptr(), 16);
    // The fence moves.
    let p = w.poll(2 * T - 2, Sample { rptr_hw: 16, fence: 1 });
    assert_eq!(p.advanced, Some((0, 1)));
    assert_eq!(p.state, RingState::Waiting { remaining_us: T });
    assert_eq!(w.jobs().count(), 1);
    // Then nothing: hung, the blamed job is the one left.
    let s = Sample { rptr_hw: 16, fence: 1 };
    assert!(!hung(w.poll(3 * T - 3, s).state));
    assert!(hung(w.poll(3 * T - 2, s).state));
    assert_eq!(w.jobs().next().unwrap().seq, 2);
}

#[test]
fn a_poller_that_slept_past_the_timeout_does_not_hang_a_ring_that_moved() {
    let mut w = watch();
    w.submit(client(1), 0, 256, 0).unwrap();
    w.submit(client(1), 256, 512, 0).unwrap();
    let p = w.poll(50 * T, Sample { rptr_hw: 300, fence: 1 });
    assert_eq!(p.state, RingState::Waiting { remaining_us: T });
    let p = w.poll(50 * T, Sample { rptr_hw: 512, fence: 2 });
    assert_eq!(p.state, RingState::Idle);
}

#[test]
fn idle_ring_never_hangs_and_the_next_job_gets_a_fresh_timer() {
    let mut w = watch();
    w.submit(client(1), 0, 256, 0).unwrap();
    let done = Sample { rptr_hw: 256, fence: 1 };
    assert_eq!(w.poll(10, done).state, RingState::Idle);
    assert_eq!(w.poll(100 * T, done).state, RingState::Idle);
    assert!(w.cause(1000 * T).is_none());
    w.submit(client(1), 256, 512, 1000 * T).unwrap();
    assert_eq!(w.poll(1000 * T, done).state, RingState::Waiting { remaining_us: T });
}

#[test]
fn a_late_signal_clears_the_verdict() {
    let mut w = watch();
    w.submit(client(1), 0, 256, 0).unwrap();
    assert!(hung(w.poll(T, Sample { rptr_hw: 0, fence: 0 }).state));
    assert_eq!(w.poll(T + 1, Sample { rptr_hw: 256, fence: 1 }).state, RingState::Idle);
}

#[test]
fn garbage_cannot_postpone_the_verdict() {
    let mut w = watch();
    w.submit(client(1), 0, 256, 0).unwrap();
    // All ones (a dead link), an older fence, a read pointer past the write pointer or moving back.
    let p = w.poll(1, Sample { rptr_hw: u64::MAX, fence: u64::MAX });
    assert_eq!(p.rptr_garbage, Some(u64::MAX));
    assert_eq!(p.fence_garbage, None, "all ones is older than 0: stale");
    let p = w.poll(2, Sample { rptr_hw: 700, fence: 50 });
    assert_eq!((p.rptr_garbage, p.fence_garbage), (Some(700), Some(50)));
    assert!(w.poll(3, Sample { rptr_hw: 100, fence: 0 }).rptr_garbage.is_none());
    assert_eq!(w.rptr(), 100);
    assert_eq!(w.poll(T + 3, Sample { rptr_hw: 50, fence: 0 }).rptr_garbage, Some(50), "moving back");
    assert_eq!(w.rptr(), 100, "kept");
    // 100 was the last progress, at time 3.
    assert!(hung(w.poll(T + 3, Sample { rptr_hw: 512, fence: 9 }).state));
    assert_eq!(w.timeline().signalled(), 0);
}

/// A ring of 1024 dwords with all but 8 in flight, the pointers at `at`.
fn nearly_full(at: u64) -> RingWatch {
    let mut c = cfg(Kind::Gfx, 1024);
    c.start = at;
    let mut w = RingWatch::new(RingId(0), c).unwrap();
    w.submit(client(1), at, at + 1016, 0).unwrap();
    w
}

#[test]
fn a_read_from_behind_in_a_nearly_full_ring_is_no_progress() {
    let at = 4096;
    // Less than the free part behind: outside the window, always garbage.
    let mut w = nearly_full(at);
    assert_eq!(w.poll(1, Sample { rptr_hw: at - 4, fence: 0 }).rptr_garbage, Some(at - 4));
    assert_eq!(w.rptr(), at);
    // More than that: inside the window, it looks like a step ahead. A register bouncing between the two values
    // neither moves the pointer nor the timer.
    let mut w = nearly_full(at);
    for t in 0..T {
        let (ptr, garbage) = if t % 2 == 0 { (at - 16, t >= 2) } else { (at, true) };
        let p = w.poll(t, Sample { rptr_hw: ptr, fence: 0 });
        assert_eq!(p.state, RingState::Waiting { remaining_us: T - t });
        assert_eq!(p.rptr_garbage.is_some(), garbage && t % 2 == 1, "t={t}");
        assert_eq!(w.rptr(), at, "t={t}");
    }
    // The verdict comes at the first sample that is not a new suspect: one poll later at most.
    assert_eq!(w.poll(T, Sample { rptr_hw: at - 16, fence: 0 }).state, RingState::Waiting { remaining_us: 0 });
    assert!(hung(w.poll(T + 1, Sample { rptr_hw: at, fence: 0 }).state));
    assert_eq!(w.report(T + 1, &[], &[]).unwrap().rptr, at, "the dump marks the real place");
}

#[test]
fn reads_walking_backwards_cannot_hold_the_verdict() {
    let at = 4096;
    let mut w = nearly_full(at);
    assert_eq!(w.poll(T, Sample { rptr_hw: at - 16, fence: 0 }).state, RingState::Waiting { remaining_us: 0 });
    let p = w.poll(T + 1, Sample { rptr_hw: at - 17, fence: 0 });
    assert!(hung(p.state), "only the first look holds it");
    assert_eq!(p.rptr_garbage, Some(at + 1008), "the step it replaced");
    assert_eq!(w.rptr(), at);
}

#[test]
fn a_real_big_step_counts_when_the_next_sample_agrees() {
    let at = 4096;
    // The ring really ran three quarters of the way between two polls.
    let mut w = nearly_full(at);
    let p = w.poll(1, Sample { rptr_hw: at + 768, fence: 0 });
    assert_eq!(p.rptr_garbage, None, "not called garbage yet");
    assert_eq!(w.rptr(), at);
    let p = w.poll(T - 1, Sample { rptr_hw: at + 768, fence: 0 });
    assert_eq!((p.rptr_garbage, w.rptr()), (None, at + 768));
    assert_eq!(p.state, RingState::Waiting { remaining_us: T }, "the timer starts over");
    // Going on from there is ordinary progress.
    w.poll(T, Sample { rptr_hw: at + 800, fence: 0 });
    assert_eq!(w.rptr(), at + 800);

    // A poller that slept past the timeout does not hang a ring on the first look at a big step.
    let mut w = nearly_full(at);
    assert_eq!(w.poll(T * 5, Sample { rptr_hw: at + 700, fence: 0 }).state, RingState::Waiting { remaining_us: 0 });
    let p = w.poll(T * 5 + 1, Sample { rptr_hw: at + 720, fence: 0 });
    assert_eq!(p.state, RingState::Waiting { remaining_us: T });
    assert_eq!(w.rptr(), at + 720);

    // With half the ring or less in flight nothing changes: the first sample counts.
    let mut w = watch();
    w.submit(client(1), 0, 512, 0).unwrap();
    w.poll(1, Sample { rptr_hw: 512, fence: 0 });
    assert_eq!(w.rptr(), 512);
}

#[test]
fn pointer_forms_masked_unmasked_bytes_and_counters_near_the_wrap() {
    // Masked or not, the same position.
    let mut w = watch();
    w.submit(client(1), 0, 256, 0).unwrap();
    w.submit(client(1), 256, 512, 0).unwrap();
    assert_eq!(w.poll(1, Sample { rptr_hw: 1024 + 8, fence: 0 }).rptr_garbage, None);
    assert_eq!(w.rptr(), 8);

    // Across the end of the ring.
    let mut c = cfg(Kind::Gfx, 1024);
    c.start = 768;
    let mut w = RingWatch::new(RingId(0), c).unwrap();
    w.submit(client(1), 768, 1280, 0).unwrap();
    assert_eq!(w.poll(1, Sample { rptr_hw: 16, fence: 0 }).rptr_garbage, None);
    assert_eq!(w.rptr(), 1024 + 16);

    // SDMA reads bytes.
    let mut w = RingWatch::new(RingId(0), cfg(Kind::Sdma { burst_nop: true }, 1024)).unwrap();
    w.submit(client(1), 0, 64, 0).unwrap();
    assert_eq!(w.poll(1, Sample { rptr_hw: 40 << 2, fence: 0 }).rptr_garbage, None);
    assert_eq!(w.rptr(), 40);

    // 64-bit numbers past 2^64.
    let mut c = cfg(Kind::Gfx, 1024);
    c.fence_start = u64::MAX - 1;
    c.start = u64::MAX - 255;
    let mut w = RingWatch::new(RingId(0), c).unwrap();
    let a = w.submit(client(1), u64::MAX - 255, 0, 0).unwrap();
    let b = w.submit(client(1), 0, 256, 0).unwrap();
    assert_eq!((a, b), (u64::MAX, 0));
    assert_eq!(w.poll(1, Sample { rptr_hw: 16, fence: u64::MAX }).advanced, Some((u64::MAX - 1, u64::MAX)));
    assert_eq!(w.rptr(), 16);
    assert_eq!(w.poll(2, Sample { rptr_hw: 256, fence: 0 }).state, RingState::Idle);
}

#[test]
fn undo_takes_the_last_job_back_and_stops_an_idle_timer() {
    let mut w = watch();
    assert!(w.undo_submit().is_none());
    w.submit(client(1), 0, 256, 0).unwrap();
    w.submit(client(2), 256, 512, 0).unwrap();
    assert_eq!(w.undo_submit().unwrap().seq, 2);
    assert_eq!(w.wptr(), 256);
    assert_eq!(w.undo_submit().unwrap().seq, 1);
    assert_eq!(w.timeline().in_flight(), 0);
    assert!(w.cause(100 * T).is_none());
    assert_eq!(w.submit(client(1), 0, 256, 100 * T).unwrap(), 1, "the number is free again");
    assert_eq!(w.poll(100 * T, Sample { rptr_hw: 0, fence: 0 }).state, RingState::Waiting { remaining_us: T });
}

#[test]
fn the_timeline_limit_is_the_callers() {
    let mut c = cfg(Kind::Gfx, 1024);
    c.max_in_flight = 1;
    let mut w = RingWatch::new(RingId(0), c).unwrap();
    w.submit(client(1), 0, 256, 0).unwrap();
    assert_eq!(w.submit(client(1), 256, 512, 0).unwrap_err().errno(), errno::EBUSY);
}

/// A real ring on the interpreter; `words` is the job's IB.
fn engine_with_ib(words: &[u32]) -> (SimMem, Engine, Ring, Vec<u32>, u64, u32) {
    const RING: u64 = 0x10_0000;
    const IB: u64 = 0x20_0000;
    let mut mem = SimMem::new();
    mem.map(0, RING, 1024 * 4).unwrap();
    mem.map(3, IB, 0x1000).unwrap();
    mem.map(0, 0x30_0000, 0x1000).unwrap();
    mem.poke_dwords(3, IB, words).unwrap();
    let mut ring = Ring::new(Kind::Gfx, 1024, 0).unwrap();
    let mut shadow = vec![0u32; 1024];
    ring.begin(32, 0).unwrap();
    ring.emit_ib(3, IB, words.len() as u32, 0, false).unwrap();
    ring.emit_fence(0x30_0000, 1, true, true).unwrap();
    let wptr = ring.commit(&mut shadow).unwrap();
    mem.poke_dwords(0, RING, &shadow).unwrap();
    let eng = Engine::new(Isa::Pm4, RING, 1024, 0, super::limits()).unwrap();
    (mem, eng, ring, shadow, wptr, words.len() as u32)
}

fn wait_forever() -> Vec<u32> {
    let mut v = Vec::new();
    pm4::wait_reg_mem(
        &mut v,
        &pm4::WaitRegMem {
            function: 3,
            mem: true,
            operation: 0,
            engine: 0,
            addr0: 0x20_0800,
            addr1: 0,
            reference: 1,
            mask: 0xffff_ffff,
            interval: 4,
        },
    )
    .unwrap();
    v
}

#[test]
fn a_wait_times_out_and_only_a_halt_is_a_verdict_at_once() {
    let ib = wait_forever();
    let (mut mem, mut eng, _, _, wptr, _) = engine_with_ib(&ib);
    let mut w = watch();
    w.submit(client(1), 0, wptr, 0).unwrap();
    let r = eng.run(&mut mem, wptr);
    assert!(matches!(r.stop, Stop::Wait(_)));
    // The engine's walk to the poll is progress; from then on nothing moves.
    assert_eq!(w.poll_report(0, &r, 0).state, RingState::Waiting { remaining_us: T });
    let r = eng.run(&mut mem, wptr);
    assert_eq!(w.poll_report(T - 1, &r, 0).state, RingState::Waiting { remaining_us: 1 });
    let r = eng.run(&mut mem, wptr);
    assert!(hung(w.poll_report(T, &r, 0).state));

    // An IB in memory nothing maps: halted, the same microsecond.
    let (mut mem, mut eng, _, _, wptr, _) = engine_with_ib(&ib);
    mem.unmap(3, 0x20_0000);
    let mut w = watch();
    w.submit(client(1), 0, wptr, 5).unwrap();
    let r = eng.run(&mut mem, wptr);
    let RingState::Hung(Cause::Engine(h)) = w.poll_report(5, &r, 0).state else { panic!("{:?}", r.stop) };
    assert!(matches!(h, Hang::Fault(_)));
    assert_eq!(h.errno(), errno::EFAULT);
    assert!(w.cause(5).is_some());
}

#[test]
fn report_decodes_ring_and_ib_with_the_position() {
    let ib = wait_forever();
    let (mut mem, mut eng, _, shadow, wptr, _) = engine_with_ib(&ib);
    let mut w = watch();
    w.submit(client(1), 0, wptr, 0).unwrap();
    assert!(w.report(T - 1, &shadow, &[]).is_none(), "not hung yet");
    let r = eng.run(&mut mem, wptr);
    w.poll_report(0, &r, 0);
    let rep = w.report(T, &shadow, &[IbWords { addr: 0x20_0000, words: &ib }]).unwrap();
    assert_eq!(rep.job.seq, 1);
    assert_eq!((rep.signalled, rep.emitted, rep.wptr), (0, 1, wptr));
    assert_eq!(rep.ring_dump.from, 0, "from the hung job's frame");
    assert!(rep.ring_dump.lines[0].text.starts_with("INDIRECT_BUFFER addr=0x200000"));
    assert!(rep.ibs[0].lines[0].text.starts_with("WAIT_REG_MEM"), "{}", rep.ibs[0].lines[0].text);
    let text = rep.to_string();
    assert!(text.starts_with("ring t timeout: no progress for 1000 us (limit 1000 us), signaled seq=0x0, emitted seq=0x1\n"), "{text}");
    assert!(text.contains("job seq=0x1 Client { ctx: 1, entity: 10 } vmid=3 pasid=77"), "{text}");
    assert!(text.contains("ib addr=0x200000\n"), "{text}");
}

#[test]
fn report_survives_garbage_and_an_unreadable_ring() {
    let mut w = watch();
    w.submit(client(1), 0, 512, 0).unwrap();
    w.poll(0, Sample { rptr_hw: 4, fence: 0 });
    let garbage: Vec<u32> = (0..1024).map(|i| 0xDEAD_0000 | i).collect();
    let rep = w.report(T, &garbage, &[IbWords { addr: 1, words: &[u32::MAX; 8] }]).unwrap();
    assert!(!rep.ring_dump.lines.is_empty());
    assert!(!rep.to_string().is_empty());
    let rep = w.report(T, &[], &[]).unwrap();
    assert!(rep.ring_dump.lines.is_empty() && rep.ring_dump.rptr_outside);
    assert!(rep.ibs.is_empty());
}
