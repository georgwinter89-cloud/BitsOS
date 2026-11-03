//! The recovery ladder: which steps in which order, and who is told.

use std::vec;
use std::vec::Vec;

use super::watch::client;
use crate::errno;
use crate::recover::{
    Action, IpVersion, MODE2_MOVED_PICTURE, MODE2_UNMEASURED, MODE2_WRONG_CHIP, Mode2Gate, Next, Outcome, Policy, REINIT_LOOP,
    REINIT_MOVED_PICTURE, RecoverError, Reply, ResetDomain, Scope, SubmitError, linux_uses_mode2,
};
use crate::told::{Fate, query2};
use crate::watch::{JobMeta, Owner, QueueReset, RingCfg, RingClass, RingId, RingState, Sample};

const MP1: IpVersion = IpVersion(13, 0, 4);
const T: u64 = 1_000;
const KERNEL: JobMeta = JobMeta { owner: Owner::Kernel, vmid: 0, pasid: 0 };
const PROC: RingClass = RingClass::ComputeProcess { doorbell_dw: 0x40, gang_ctx: 0x9000 };
const SDMA: RingClass = RingClass::Sdma { burst_nop: true };

fn cfg(class: RingClass) -> RingCfg {
    let mut c = RingCfg::for_780m("r", class, 1024, 0, 16);
    c.timeout_us = T;
    c
}

fn domain() -> ResetDomain {
    ResetDomain::new(Policy::for_mp1(MP1))
}

fn s(fence: u64) -> Sample {
    Sample { rptr_hw: 0, fence }
}

/// Hangs the oldest job of `ring` at `now + T` and starts its recovery.
fn hang(d: &mut ResetDomain, ring: RingId, now: u64, fence: u64) -> Next {
    assert!(matches!(d.poll(ring, now + T, s(fence)).unwrap().state, RingState::Hung(_)));
    let rep = d.report(ring, now + T, &[], &[]).unwrap();
    d.begin(&rep, now + T).unwrap()
}

/// Answers every step with what `answer` says; returns the steps and the end.
fn run(d: &mut ResetDomain, mut next: Next, now: u64, mut answer: impl FnMut(&Action) -> Reply) -> (Vec<Action>, crate::recover::Finish) {
    let mut acts = Vec::new();
    for _ in 0..1000 {
        match next {
            Next::Done(f) => return (acts, f),
            Next::Do(step) => {
                acts.push(step.action);
                next = d.reply(answer(&step.action), now).unwrap();
            }
        }
    }
    panic!("no end: {acts:?}");
}

/// The chip does what it is asked; `Reset` keeps VRAM and picture.
fn good(a: &Action) -> Reply {
    match a {
        Action::TestRing { .. } => Reply::Ring { ptr: 0 },
        Action::ReinitGc | Action::Mode2Reset => Reply::Reset { vram_intact: true, display_equal: true },
        _ => Reply::Ok,
    }
}

fn fails_on(pred: impl Fn(&Action) -> bool) -> impl FnMut(&Action) -> Reply {
    move |a| if pred(a) { Reply::Failed } else { good(a) }
}

fn no_mode2(acts: &[Action]) {
    assert!(!acts.iter().any(|a| matches!(a, Action::Mode2Reset | Action::SuspendGc)), "{acts:?}");
}

#[test]
fn mode2_gate_is_closed_by_default_and_opens_only_on_mode2_chips() {
    assert!(linux_uses_mode2(MP1) && linux_uses_mode2(IpVersion(13, 0, 11)));
    assert!(!linux_uses_mode2(IpVersion(13, 0, 7)), "mode 1 on the big chips");
    let mut p = Policy::for_mp1(MP1);
    assert_eq!(p.mode2, Mode2Gate::Refused(MODE2_UNMEASURED));
    assert_eq!(Policy::for_mp1(IpVersion(13, 0, 7)).mode2, Mode2Gate::Refused(MODE2_WRONG_CHIP));
    assert_eq!(p.open_mode2(IpVersion(13, 0, 7), "x"), Err(MODE2_WRONG_CHIP));
    assert_eq!(p.mode2, Mode2Gate::Refused(MODE2_UNMEASURED));
    p.open_mode2(MP1, "gx9a").unwrap();
    assert_eq!(p.mode2, Mode2Gate::Measured { proof: "gx9a" });
}

#[test]
fn soft_kill_tells_everyone_of_a_reset_blames_nobody_and_keeps_later_jobs() {
    let mut d = domain();
    let ring = d.add_ring(cfg(RingClass::GfxDirect)).unwrap();
    let old = d.counters().new_context();
    d.submit(ring, client(1), 0, 256, 0).unwrap();
    d.submit(ring, client(2), 256, 512, 0).unwrap();
    let next = hang(&mut d, ring, 0, 0);
    let Next::Do(step) = next else { panic!() };
    assert_eq!(step.action, Action::KillWaves { ring, vmid: 3, seq: 1, errno: errno::ENODATA });
    assert_eq!(step.deadline_us, T + 10_000, "Linux's 10 ms");
    assert_eq!(d.counters().reset, 1, "moved before the kill");
    assert!(!d.counters().in_reset, "Linux takes the reset lock only for the full reset");
    assert_eq!(d.submit(ring, client(2), 512, 768, T).unwrap_err(), SubmitError::Recovering);
    // The fence shows success.
    let p = d.poll(ring, T + 5, Sample { rptr_hw: 256, fence: 1 }).unwrap();
    assert_eq!((p.state, p.advanced), (RingState::Recovering, Some((0, 1))));
    let Next::Done(f) = d.reply(Reply::Ok, T + 5).unwrap() else { panic!() };
    assert_eq!(f.outcome, Outcome::Recovered(Scope::Soft));
    assert!(f.fates.is_empty() && f.guilty.is_empty());
    assert_eq!(d.counters().query2(&old), query2::RESET, "amdgpu_ctx.c:591");
    // The second job stays, with a new timer.
    assert_eq!(d.ring(ring).unwrap().jobs().next().unwrap().seq, 2);
    assert_eq!(d.poll(ring, T + 5 + T - 1, Sample { rptr_hw: 256, fence: 1 }).unwrap().state, RingState::Waiting { remaining_us: 1 });
}

#[test]
fn soft_kill_is_judged_by_the_fence_and_escalates_to_a_full_reset() {
    let mut d = domain();
    let ring = d.add_ring(cfg(RingClass::GfxDirect)).unwrap();
    let sdma = d.add_ring(cfg(SDMA)).unwrap();
    let innocent = d.counters().new_context();
    d.submit(ring, client(1), 0, 256, 0).unwrap();
    d.submit(ring, client(1), 256, 512, 0).unwrap();
    d.submit(sdma, client(2), 0, 64, 0).unwrap();
    d.submit(sdma, KERNEL, 64, 128, 0).unwrap();
    let next = hang(&mut d, ring, 0, 0);
    // "Ok" does not count: the fence did not move.
    let Next::Do(blame) = d.reply(Reply::Ok, T + 1).unwrap() else { panic!() };
    assert!(matches!(next, Next::Do(_)));
    assert_eq!(blame.action, Action::Blame { ctx: 1, entity: 10 });
    let c = d.counters();
    assert!(c.in_reset, "from here on");
    assert_eq!(c.reset, 2, "soft and full each count (amdgpu_ring.c:447, amdgpu_device.c:5277)");
    assert_eq!(d.poll(sdma, T + 1, s(1)).unwrap(), crate::watch::Polled::only(RingState::InReset));
    assert_eq!(d.submit(sdma, client(2), 128, 192, T + 1).unwrap_err().errno(), errno::EBUSY);
    assert_eq!(d.counters().query2(&innocent) & query2::RESET_IN_PROGRESS, query2::RESET_IN_PROGRESS);

    let (acts, f) = run(&mut d, Next::Do(blame), T + 1, good);
    assert_eq!(acts[1..], [
        Action::ReinitGc,
        Action::TestRing { ring, fence_value: 2 },
        Action::TestRing { ring: sdma, fence_value: 2 },
    ]);
    assert_eq!(f.outcome, Outcome::Recovered(Scope::Gc));
    assert_eq!(f.guilty, vec![1]);
    let fates: Vec<_> = f.fates.iter().map(|j| (j.ring, j.job.seq, j.fate, j.fate.errno())).collect();
    assert_eq!(fates, [
        (ring, 1, Fate::Hung, errno::ETIME),
        (ring, 2, Fate::Canceled, errno::ECANCELED),
        (sdma, 1, Fate::Canceled, errno::ECANCELED),
        (sdma, 2, Fate::Canceled, errno::ECANCELED),
    ]);
    assert!(!d.counters().in_reset);
    assert_eq!(d.counters().query2(&innocent), query2::RESET, "innocent: reset, not guilty, VRAM kept");
    // Running again: numbers go on, the fence slot was written to the last one.
    assert_eq!(d.submit(ring, client(3), 0, 256, 2 * T).unwrap(), 3);
    assert_eq!(d.poll(ring, 2 * T, s(2)).unwrap().state, RingState::Waiting { remaining_us: T });
}

#[test]
fn a_kernel_job_is_never_blamed_and_gets_no_kill() {
    let mut d = domain();
    let ring = d.add_ring(cfg(RingClass::GfxDirect)).unwrap();
    d.submit(ring, KERNEL, 0, 256, 0).unwrap();
    let next = hang(&mut d, ring, 0, 0);
    let (acts, f) = run(&mut d, next, T, good);
    assert_eq!(acts, [Action::ReinitGc, Action::TestRing { ring, fence_value: 1 }]);
    assert!(f.guilty.is_empty());
    assert_eq!(f.fates[0].fate, Fate::Hung);
}

#[test]
fn a_gfx_client_job_in_vmid_0_gets_no_kill() {
    // The kill names a VMID: 0 would hit the kernel's waves.
    let mut d = domain();
    let ring = d.add_ring(cfg(RingClass::GfxDirect)).unwrap();
    d.submit(ring, JobMeta { vmid: 0, ..client(1) }, 0, 256, 0).unwrap();
    let next = hang(&mut d, ring, 0, 0);
    let (acts, _) = run(&mut d, next, T, good);
    assert!(!acts.iter().any(|a| matches!(a, Action::KillWaves { .. })));
}

#[test]
fn a_process_queue_recovers_alone_and_only_its_context_is_told() {
    let mut d = domain();
    let ring = d.add_ring(cfg(PROC)).unwrap();
    let other = d.add_ring(cfg(RingClass::GfxDirect)).unwrap();
    let queue = QueueReset::MesProcess { doorbell_dw: 0x40, gang_ctx: 0x9000 };
    let mine = d.counters().new_context();
    let game = d.counters().new_context();
    d.submit(ring, client(1), 0, 256, 0).unwrap();
    d.submit(ring, JobMeta { owner: Owner::Client { ctx: 1, entity: 11 }, ..client(1) }, 256, 512, 0).unwrap();
    d.submit(other, client(2), 0, 256, T).unwrap();
    let next = hang(&mut d, ring, 0, 0);
    let (acts, f) = run(&mut d, next, T, good);
    assert_eq!(acts, [
        Action::Blame { ctx: 1, entity: 10 },
        Action::Unmap { ring, queue },
        Action::ResetQueue { ring, queue },
        Action::Map { ring, queue },
        Action::TestRing { ring, fence_value: 2 },
    ]);
    assert_eq!(f.outcome, Outcome::Recovered(Scope::Queue));
    assert_eq!(f.guilty, vec![1]);
    assert_eq!(f.fates.len(), 2);
    assert_eq!(f.fates[0].fate, Fate::Hung);
    assert_eq!(f.fates[1].fate, Fate::Canceled);
    // The game on the other ring saw nothing.
    assert_eq!(d.counters().reset, 0);
    assert_eq!(d.counters().query2(&game), 0);
    assert_eq!(d.ring(other).unwrap().jobs().count(), 1);
    let mut mine = mine;
    d.counters().blame(&mut mine);
    assert_eq!(d.counters().query2(&mine), query2::RESET | query2::GUILTY, "as Mesa expects a guilty context");
}

#[test]
fn a_shared_ring_tells_everyone_when_other_contexts_lost_jobs() {
    let mut d = domain();
    let ring = d.add_ring(cfg(SDMA)).unwrap();
    let innocent = d.counters().new_context();
    d.submit(ring, client(1), 0, 64, 0).unwrap();
    d.submit(ring, client(2), 64, 128, 0).unwrap();
    let next = hang(&mut d, ring, 0, 0);
    let (acts, f) = run(&mut d, next, T, |a| match a {
        // The chip keeps its pointers through a restart.
        Action::TestRing { .. } => Reply::Ring { ptr: 0x40 },
        a => good(a),
    });
    assert_eq!(acts, [
        Action::Blame { ctx: 1, entity: 10 },
        Action::RestartSdma { ring },
        Action::TestRing { ring, fence_value: 2 },
    ]);
    assert_eq!(f.outcome, Outcome::Recovered(Scope::Queue));
    assert_eq!(d.counters().reset, 1);
    assert_eq!(d.counters().query2(&innocent), query2::RESET);
    let w = d.ring(ring).unwrap();
    assert_eq!((w.rptr(), w.wptr()), (0x40, 0x40));
}

#[test]
fn a_silent_mes_escalates_to_gc_and_the_process_queue_is_mapped_again() {
    let mut d = domain();
    let ring = d.add_ring(cfg(PROC)).unwrap();
    let direct = d.add_ring(cfg(RingClass::GfxDirect)).unwrap();
    let queue = QueueReset::MesProcess { doorbell_dw: 0x40, gang_ctx: 0x9000 };
    d.submit(ring, client(1), 0, 256, 0).unwrap();
    let next = hang(&mut d, ring, 0, 0);
    let (acts, f) = run(&mut d, next, T, fails_on(|a| matches!(a, Action::Unmap { .. })));
    assert_eq!(acts, [
        Action::Blame { ctx: 1, entity: 10 },
        Action::Unmap { ring, queue },
        Action::ReinitGc,
        Action::Map { ring, queue },
        Action::TestRing { ring, fence_value: 1 },
        Action::TestRing { ring: direct, fence_value: 0 },
    ]);
    assert_eq!(f.outcome, Outcome::Recovered(Scope::Gc));
    assert_eq!(f.fates.len(), 1, "the job has one end, though two rungs ran");
    assert_eq!(f.guilty, vec![1]);
    assert_eq!(d.counters().reset, 1);
}

#[test]
fn compute_on_the_780m_has_no_kill_and_gfx_process_queues_have_one() {
    let c = RingCfg::for_780m("c", RingClass::ComputeMesLegacy { queue_type: 1, pipe: 0, queue: 0, doorbell_dw: 8 }, 1024, 0, 8);
    assert!(!c.kill_waves, "gfx_v11_0_ring_funcs_compute has no soft_recovery");
    assert_eq!(c.timeout_us, 60_000_000);
    let g = RingCfg::for_780m("g", RingClass::GfxProcess { doorbell_dw: 8, gang_ctx: 1 }, 1024, 0, 8);
    assert!(g.kill_waves);
    assert_eq!(g.timeout_us, 10_000_000);
    let s = RingCfg::for_780m("s", SDMA, 1024, 0, 8);
    assert!(!s.kill_waves);
    assert_eq!(s.queue, QueueReset::SdmaRestart);
    let l = RingCfg::for_780m("g", RingClass::GfxMesLegacy { queue_type: 0, pipe: 1, queue: 2, doorbell_dw: 3 }, 1024, 0, 8);
    assert_eq!(l.queue, QueueReset::MesLegacy { queue_type: 0, pipe: 1, queue: 2, doorbell_dw: 3 });
    assert_eq!(RingCfg::for_780m("d", RingClass::GfxDirect, 1024, 0, 8).queue, QueueReset::None);
}

#[test]
fn nothing_left_means_reboot_never_an_smu_message_and_programs_get_errors() {
    let mut d = domain();
    let ring = d.add_ring(cfg(RingClass::GfxDirect)).unwrap();
    let other = d.add_ring(cfg(SDMA)).unwrap();
    let innocent = d.counters().new_context();
    d.submit(ring, client(1), 0, 256, 0).unwrap();
    d.submit(other, client(2), 0, 64, 0).unwrap();
    let next = hang(&mut d, ring, 0, 0);
    let (acts, f) = run(&mut d, next, T, |_| Reply::Failed);
    no_mode2(&acts);
    assert_eq!(f.outcome, Outcome::NeedsReboot { mode2: Some(MODE2_UNMEASURED) });
    assert_eq!(f.fates.len(), 2);
    assert_eq!(f.guilty, vec![1]);
    assert!(!f.counters.in_reset, "programs must not wait for a reset that will not end");
    assert_eq!(d.counters().query2(&innocent), query2::RESET);
    assert!(d.is_dead());
    assert_eq!(d.submit(ring, client(3), 0, 256, 2 * T).unwrap_err().errno(), errno::ENODEV);
    assert_eq!(d.poll(ring, 2 * T, s(0)).unwrap().state, RingState::Dead);
    assert!(d.undo_submit(ring).is_none());
    assert_eq!(d.reply(Reply::Ok, 2 * T).unwrap_err(), RecoverError::Idle);
    // The report still works for a log; a recovery does not.
    assert!(d.report(ring, 2 * T, &[], &[]).is_none());
}

#[test]
fn mode2_runs_only_after_its_measurement_and_after_the_suspend() {
    let mut p = Policy::for_mp1(MP1);
    p.open_mode2(MP1, "gx9a").unwrap();
    let mut d = ResetDomain::new(p);
    let ring = d.add_ring(cfg(RingClass::GfxDirect)).unwrap();
    d.submit(ring, KERNEL, 0, 256, 0).unwrap();
    let next = hang(&mut d, ring, 0, 0);
    let mut first = true;
    let (acts, f) = run(&mut d, next, T, |a| {
        // The first re-init fails, the one after mode 2 works.
        if *a == Action::ReinitGc && first {
            first = false;
            return Reply::Failed;
        }
        good(a)
    });
    assert_eq!(acts, [
        Action::ReinitGc,
        Action::SuspendGc,
        Action::Mode2Reset,
        Action::ReinitGc,
        Action::TestRing { ring, fence_value: 1 },
    ]);
    assert_eq!(f.outcome, Outcome::Recovered(Scope::Mode2));
    assert_eq!(d.counters().reset, 1, "one reset, tried twice (amdgpu_device.c:5277 before the retry label)");
}

#[test]
fn lost_vram_moves_the_generation_once_and_new_contexts_start_clean() {
    let mut d = domain();
    let ring = d.add_ring(cfg(RingClass::GfxDirect)).unwrap();
    let old = d.counters().new_context();
    d.submit(ring, KERNEL, 0, 256, 0).unwrap();
    let next = hang(&mut d, ring, 0, 0);
    let (_, f) = run(&mut d, next, T, |a| match a {
        Action::ReinitGc => Reply::Reset { vram_intact: false, display_equal: true },
        a => good(a),
    });
    assert!(f.vram_lost);
    assert_eq!(d.counters().vram_lost, 1);
    assert_eq!(d.counters().query2(&old), query2::RESET | query2::VRAMLOST);
    assert_eq!(d.counters().query2(&d.counters().new_context()), 0);
}

#[test]
fn a_reset_that_moved_the_picture_closes_every_reset_for_this_boot() {
    let mut p = Policy::for_mp1(MP1);
    p.open_mode2(MP1, "gx9a").unwrap();
    let mut d = ResetDomain::new(p);
    let ring = d.add_ring(cfg(RingClass::GfxDirect)).unwrap();
    d.submit(ring, KERNEL, 0, 256, 0).unwrap();
    let next = hang(&mut d, ring, 0, 0);
    let (_, f) = run(&mut d, next, T, |a| match a {
        Action::ReinitGc => Reply::Reset { vram_intact: true, display_equal: false },
        a => good(a),
    });
    assert!(f.display_changed);
    assert_eq!(f.outcome, Outcome::Recovered(Scope::Gc), "this one is done, the harm too");
    assert_eq!(d.policy().mode2, Mode2Gate::Refused(MODE2_MOVED_PICTURE));
    d.submit(ring, KERNEL, 0, 256, 2 * T).unwrap();
    let next = hang(&mut d, ring, 2 * T, 1);
    let (acts, f) = run(&mut d, next, 2 * T, good);
    assert!(acts.is_empty(), "{acts:?}");
    assert_eq!(f.outcome, Outcome::NeedsReboot { mode2: Some(MODE2_MOVED_PICTURE) });
    assert_eq!(f.reinit_refused, Some(REINIT_MOVED_PICTURE));
}

#[test]
fn a_moved_picture_leaves_queue_resets_open() {
    // A queue reset touches no display field: a process queue still recovers alone.
    let mut d = domain();
    let gfx = d.add_ring(cfg(RingClass::GfxDirect)).unwrap();
    let proc = d.add_ring(cfg(PROC)).unwrap();
    d.submit(gfx, KERNEL, 0, 256, 0).unwrap();
    let next = hang(&mut d, gfx, 0, 0);
    let (_, f) = run(&mut d, next, T, |a| match a {
        Action::ReinitGc => Reply::Reset { vram_intact: true, display_equal: false },
        a => good(a),
    });
    assert!(f.display_changed && f.reinit_refused.is_none());
    d.submit(proc, client(1), 0, 256, 2 * T).unwrap();
    let next = hang(&mut d, proc, 2 * T, 0);
    let (_, f) = run(&mut d, next, 3 * T, good);
    assert_eq!(f.outcome, Outcome::Recovered(Scope::Queue));
    // When the queue will not come back there is nothing above it any more.
    d.submit(proc, client(1), 0, 256, 4 * T).unwrap();
    let next = hang(&mut d, proc, 4 * T, 1);
    let (acts, f) = run(&mut d, next, 5 * T, fails_on(|a| matches!(a, Action::Unmap { .. })));
    assert!(!acts.contains(&Action::ReinitGc));
    assert_eq!(f.outcome, Outcome::NeedsReboot { mode2: Some(MODE2_MOVED_PICTURE) });
}

#[test]
fn bringup_runs_again_and_again_but_not_in_a_tight_loop() {
    let mut p = Policy::for_mp1(MP1);
    p.reinit_limit.count = 2;
    p.reinit_limit.window_us = 100 * T;
    let mut d = ResetDomain::new(p);
    let ring = d.add_ring(cfg(RingClass::GfxDirect)).unwrap();
    // Spread out: as often as it takes.
    let mut now = 0;
    for i in 0..10u64 {
        d.submit(ring, KERNEL, 0, 256, now).unwrap();
        let next = hang(&mut d, ring, now, i);
        let (_, f) = run(&mut d, next, now + T, good);
        assert_eq!(f.outcome, Outcome::Recovered(Scope::Gc), "round {i}");
        now += 100 * T;
    }
    assert_eq!(d.counters().reset, 10);
    // Close together: the third within the window is refused, and nothing is left.
    now += 100 * T;
    for i in 0..3u64 {
        d.submit(ring, KERNEL, 0, 256, now).unwrap();
        let next = hang(&mut d, ring, now, 10 + i);
        let (acts, f) = run(&mut d, next, now + T, good);
        if i < 2 {
            assert_eq!(f.outcome, Outcome::Recovered(Scope::Gc));
        } else {
            assert!(acts.is_empty(), "{acts:?}");
            assert_eq!(f.outcome, Outcome::NeedsReboot { mode2: Some(MODE2_UNMEASURED) });
            assert_eq!(f.reinit_refused, Some(REINIT_LOOP));
        }
        now += 2 * T;
    }
}

#[test]
fn a_ring_that_moved_after_the_report_is_left_alone() {
    let mut d = domain();
    let ring = d.add_ring(cfg(RingClass::GfxDirect)).unwrap();
    d.submit(ring, client(1), 0, 256, 0).unwrap();
    d.submit(ring, client(1), 256, 512, 0).unwrap();
    d.poll(ring, T, s(0));
    let rep = d.report(ring, T, &[], &[]).unwrap();
    // The job signalled: the report is stale, the second job has a new timer.
    d.poll(ring, T + 1, s(1));
    assert_eq!(d.begin(&rep, T + 1).unwrap_err(), RecoverError::NotHung);
    assert_eq!(d.counters().reset, 0);
    // Hung again, but on another job than the report's.
    d.poll(ring, 2 * T + 1, s(1));
    assert_eq!(d.begin(&rep, 2 * T + 1).unwrap_err(), RecoverError::NotHung);
    let rep2 = d.report(ring, 2 * T + 1, &[], &[]).unwrap();
    assert_eq!(rep2.job.seq, 2);
    assert!(d.begin(&rep2, 2 * T + 1).is_ok());
}

#[test]
fn a_job_signalling_while_the_kill_ran_ends_it_without_a_reset() {
    let mut d = domain();
    let ring = d.add_ring(cfg(RingClass::GfxProcess { doorbell_dw: 1, gang_ctx: 2 })).unwrap();
    d.submit(ring, client(1), 0, 256, 0).unwrap();
    let next = hang(&mut d, ring, 0, 0);
    assert!(matches!(next, Next::Do(s) if matches!(s.action, Action::KillWaves { .. })));
    d.poll(ring, T + 1, s(1));
    let Next::Done(f) = d.reply(Reply::Failed, T + 1).unwrap() else { panic!() };
    assert_eq!(f.outcome, Outcome::Recovered(Scope::Soft), "the fence decides, not the reply");
    assert!(f.fates.is_empty() && f.guilty.is_empty());
}

#[test]
fn a_second_hung_ring_waits_its_turn_unless_a_full_reset_took_it_along() {
    let mut d = domain();
    let a = d.add_ring(cfg(PROC)).unwrap();
    let b = d.add_ring(cfg(PROC)).unwrap();
    d.submit(a, client(1), 0, 256, 0).unwrap();
    d.submit(b, client(2), 0, 256, 0).unwrap();
    assert!(matches!(d.poll(b, T, s(0)).unwrap().state, RingState::Hung(_)));
    let rep_b = d.report(b, T, &[], &[]).unwrap();
    let next = hang(&mut d, a, 0, 0);
    assert_eq!(d.begin(&rep_b, T).unwrap_err(), RecoverError::Busy);
    assert_eq!(d.remove_ring(a).unwrap_err(), RecoverError::Busy);
    let (_, f) = run(&mut d, next, T, good);
    assert_eq!(f.fates.len(), 1, "only ring a");
    let next = d.begin(&rep_b, T).unwrap();
    let (_, f) = run(&mut d, next, T, good);
    assert_eq!((f.ring, f.fates[0].job.seq), (b, 1));
    assert_eq!(d.counters().reset, 0);

    // A full reset takes both.
    let mut d = domain();
    let a = d.add_ring(cfg(RingClass::GfxDirect)).unwrap();
    let b = d.add_ring(cfg(RingClass::GfxDirect)).unwrap();
    d.submit(a, KERNEL, 0, 256, 0).unwrap();
    d.submit(b, KERNEL, 0, 256, 0).unwrap();
    d.poll(b, T, s(0));
    let rep_b = d.report(b, T, &[], &[]).unwrap();
    let next = hang(&mut d, a, 0, 0);
    let (_, f) = run(&mut d, next, T, good);
    assert_eq!(f.fates.len(), 2);
    assert_eq!(d.begin(&rep_b, T).unwrap_err(), RecoverError::NotHung);
    assert_eq!(d.poll(b, T, s(1)).unwrap().state, RingState::Idle);
}

#[test]
fn wrong_replies_count_as_failures_and_overruns_are_counted() {
    let mut d = domain();
    let ring = d.add_ring(cfg(PROC)).unwrap();
    d.submit(ring, client(1), 0, 256, 0).unwrap();
    let next = hang(&mut d, ring, 0, 0);
    let (acts, f) = run(&mut d, next, T, |a| match a {
        // Late by a day, and of the wrong kind.
        Action::Unmap { .. } => Reply::Reset { vram_intact: false, display_equal: false },
        Action::ReinitGc => Reply::Ring { ptr: 5 },
        a => good(a),
    });
    assert_eq!(acts.len(), 3);
    assert_eq!(f.outcome, Outcome::NeedsReboot { mode2: Some(MODE2_UNMEASURED) });
    assert!(!f.vram_lost && !f.display_changed, "a reply of the wrong kind says nothing");

    let mut d = domain();
    let ring = d.add_ring(cfg(PROC)).unwrap();
    d.submit(ring, client(1), 0, 256, 0).unwrap();
    let Next::Do(step) = hang(&mut d, ring, 0, 0) else { panic!() };
    assert_eq!(step.deadline_us, T + d.policy().queue_us);
    let Next::Do(unmap) = d.reply(Reply::Ok, T).unwrap() else { panic!() };
    let late = unmap.deadline_us + 1;
    let next = d.reply(Reply::Ok, late).unwrap();
    let (_, f) = run(&mut d, next, late, good);
    assert_eq!(f.overruns, 1);
    assert_eq!(f.outcome, Outcome::Recovered(Scope::Queue), "late is not failed: the queue did come back");
}

#[test]
fn rings_come_and_go() {
    let mut d = domain();
    let a = d.add_ring(cfg(PROC)).unwrap();
    let b = d.add_ring(cfg(PROC)).unwrap();
    assert_ne!(a, b);
    let mut bad = cfg(PROC);
    bad.size_dw = 1000;
    assert_eq!(d.add_ring(bad).unwrap_err(), RecoverError::Invalid);
    d.submit(a, client(1), 0, 256, 0).unwrap();
    assert_eq!(d.remove_ring(a).unwrap_err(), RecoverError::Busy);
    assert_eq!(d.undo_submit(a).unwrap().seq, 1);
    d.remove_ring(a).unwrap();
    assert_eq!(d.remove_ring(a).unwrap_err(), RecoverError::NoRing);
    assert_eq!(d.submit(a, client(1), 0, 256, 0).unwrap_err().errno(), errno::EINVAL);
    assert!(d.poll(a, 0, s(0)).is_none());
    assert!(d.report(a, 0, &[], &[]).is_none());
    assert!(d.ring(b).is_some());
}

#[test]
fn query2_flags_are_linuxs() {
    // include/uapi/drm/amdgpu_drm.h:245-254.
    assert_eq!((query2::RESET, query2::VRAMLOST, query2::GUILTY, query2::RESET_IN_PROGRESS), (1, 2, 4, 32));
    let d = domain();
    let c = d.counters().new_context();
    assert_eq!(d.counters().query2(&c), 0);
    let mut counters = d.counters();
    counters.in_reset = true;
    assert_eq!(counters.query2(&c), query2::RESET_IN_PROGRESS);
    counters.reset = u32::MAX;
    counters.vram_lost = 7;
    assert_eq!(counters.query2(&c), query2::RESET | query2::VRAMLOST | query2::RESET_IN_PROGRESS);
}

#[test]
fn query2_takes_each_flag_from_its_own_source_and_a_blame_moves_the_snapshot() {
    // amdgpu_ctx.c:596-606: guilty alone says nothing about RESET.
    let mut counters = domain().counters();
    let c = counters.new_context();
    assert_eq!(counters.query2(&crate::told::ContextState { guilty: true, ..c }), query2::GUILTY);

    let mut blamed = c;
    counters.blame(&mut blamed);
    assert_eq!(counters.query2(&blamed), query2::RESET | query2::GUILTY);
    assert_eq!(counters.query2(&c), 0, "the device's counter did not move");
    // It stays: later resets do not bring the counter back to the snapshot.
    counters.reset = counters.reset.wrapping_add(1);
    assert_eq!(counters.query2(&blamed), query2::RESET | query2::GUILTY);
    // A context that already saw a reset keeps its snapshot, a second blame changes nothing.
    let before = blamed;
    counters.blame(&mut blamed);
    assert_eq!(blamed, before);
}

#[test]
fn a_recovery_nobody_answers_ends_by_itself() {
    let mut d = domain();
    let ring = d.add_ring(cfg(PROC)).unwrap();
    let other = d.add_ring(cfg(SDMA)).unwrap();
    d.submit(ring, client(1), 0, 256, 0).unwrap();
    d.submit(other, client(2), 0, 256, T).unwrap();
    let mut next = hang(&mut d, ring, 0, 0);
    assert_eq!(d.submit(ring, client(1), 256, 512, T).unwrap_err().errno(), errno::EBUSY);
    let mut now = T;
    let mut seen = Vec::new();
    let f = loop {
        match next {
            Next::Done(f) => break f,
            Next::Do(step) => {
                seen.push(step.action);
                assert_eq!(d.waiting(), Some(step));
                let last = step.deadline_us + d.policy().expire_slack_us;
                assert!(d.expire(now).is_none() && d.expire(last).is_none(), "{step:?} is not overdue yet");
                now = last + 1;
                next = d.expire(now).unwrap();
                assert!(seen.len() < 16, "no end: {seen:?}");
            }
        }
    };
    // Blame cannot fail; Unmap is lost, so the queue rung ends. ReinitGc is lost too, and nothing else is allowed.
    assert_eq!(seen, [Action::Blame { ctx: 1, entity: 10 }, Action::Unmap { ring, queue: d.ring(ring).unwrap().cfg().queue }, Action::ReinitGc]);
    assert_eq!(f.outcome, Outcome::NeedsReboot { mode2: Some(MODE2_UNMEASURED) });
    assert_eq!(f.expired, 3);
    assert_eq!(f.overruns, 0, "nobody replied late");
    assert_eq!(f.fates.len(), 2, "every job ended");
    assert!(!d.counters().in_reset, "no program waits for this reset any more");
    assert!(d.is_dead() && d.waiting().is_none());
    assert_eq!(d.expire(now + (1 << 40)), None);
    assert_eq!(d.reply(Reply::Ok, now).unwrap_err(), RecoverError::Idle);
}

#[test]
fn an_answer_after_the_expiry_belongs_to_the_next_step_unless_the_driver_compares() {
    let mut d = domain();
    let ring = d.add_ring(cfg(PROC)).unwrap();
    d.submit(ring, client(1), 0, 256, 0).unwrap();
    let Next::Do(blame) = hang(&mut d, ring, 0, 0) else { panic!() };
    let Next::Do(unmap) = d.reply(Reply::Ok, T).unwrap() else { panic!() };
    assert_ne!(blame, unmap);
    let now = unmap.deadline_us + d.policy().expire_slack_us + 1;
    let Some(Next::Do(gc)) = d.expire(now) else { panic!() };
    assert_eq!(gc.action, Action::ReinitGc);
    assert_ne!(d.waiting(), Some(unmap), "the worker of the unmap sees it lost its step");
    assert!(d.counters().in_reset);
    // A full reset that comes back after all, before its expiry, still recovers.
    let next = d.reply(Reply::Reset { vram_intact: true, display_equal: true }, now).unwrap();
    let (_, f) = run(&mut d, next, now, good);
    assert_eq!(f.outcome, Outcome::Recovered(Scope::Gc));
    assert_eq!(f.expired, 1);
}

#[test]
fn an_unanswered_kill_is_still_judged_by_the_fence() {
    let mut d = domain();
    let ring = d.add_ring(cfg(RingClass::GfxDirect)).unwrap();
    d.submit(ring, client(1), 0, 256, 0).unwrap();
    let Next::Do(kill) = hang(&mut d, ring, 0, 0) else { panic!() };
    assert!(matches!(kill.action, Action::KillWaves { .. }));
    d.poll(ring, T + 1, s(1)).unwrap();
    let Some(Next::Done(f)) = d.expire(kill.deadline_us + d.policy().expire_slack_us + 1) else { panic!() };
    assert_eq!(f.outcome, Outcome::Recovered(Scope::Soft));
    assert_eq!(f.expired, 1);
}

#[test]
fn a_ring_added_during_a_recovery_would_miss_its_steps_and_is_refused() {
    let mut d = domain();
    let ring = d.add_ring(cfg(PROC)).unwrap();
    d.submit(ring, client(1), 0, 256, 0).unwrap();
    let mut next = hang(&mut d, ring, 0, 0);
    assert_eq!(d.add_ring(cfg(PROC)).unwrap_err(), RecoverError::Busy, "queue rung");
    // Into the full reset, then out of it.
    loop {
        let Next::Do(step) = next else { panic!() };
        if step.action == Action::ReinitGc {
            break;
        }
        next = d.reply(Reply::Failed, T).unwrap();
    }
    assert!(d.counters().in_reset);
    assert_eq!(d.add_ring(cfg(PROC)).unwrap_err(), RecoverError::Busy, "full reset");
    let next = d.reply(Reply::Failed, T).unwrap();
    let (_, f) = run(&mut d, next, T, good);
    assert_eq!(f.outcome, Outcome::NeedsReboot { mode2: Some(MODE2_UNMEASURED) });
    assert_eq!(d.add_ring(cfg(PROC)).unwrap_err(), RecoverError::Dead);

    let mut d = domain();
    let ring = d.add_ring(cfg(PROC)).unwrap();
    d.submit(ring, client(1), 0, 256, 0).unwrap();
    let next = hang(&mut d, ring, 0, 0);
    let (_, f) = run(&mut d, next, T, good);
    assert_eq!(f.outcome, Outcome::Recovered(Scope::Queue));
    assert!(d.add_ring(cfg(PROC)).is_ok(), "after it, again");
}

#[test]
fn ring_ids_that_wrap_skip_the_rings_still_alive() {
    let mut d = domain();
    let old = d.add_ring(cfg(PROC)).unwrap();
    let next_after = d.add_ring(cfg(PROC)).unwrap();
    d.submit(old, client(1), 0, 256, 0).unwrap();
    d.set_next_id(u32::MAX);
    let last = d.add_ring(cfg(PROC)).unwrap();
    assert_eq!(last, RingId(u32::MAX));
    let wrapped = d.add_ring(cfg(PROC)).unwrap();
    assert_eq!(wrapped, RingId(2), "0 and 1 are alive");
    assert_ne!(wrapped, next_after);
    assert_eq!(d.ring(old).unwrap().jobs().count(), 1, "its job is still there");
}

#[test]
fn recovery_off_reports_ends_the_rings_jobs_and_touches_nothing_else() {
    // amdgpu_job.c:54,70: no soft recovery and no reset; ETIME stays.
    let mut p = Policy::for_mp1(MP1);
    assert!(p.recovery, "Linux's default on this chip");
    p.recovery = false;
    let mut d = ResetDomain::new(p);
    let ring = d.add_ring(cfg(RingClass::GfxDirect)).unwrap();
    let other = d.add_ring(cfg(SDMA)).unwrap();
    let ctx = d.counters().new_context();
    d.submit(ring, client(1), 0, 256, 0).unwrap();
    d.submit(ring, client(2), 256, 512, 0).unwrap();
    d.submit(other, client(3), 0, 256, T).unwrap();
    let Next::Done(f) = hang(&mut d, ring, 0, 0) else { panic!("no step at all") };
    assert_eq!(f.outcome, Outcome::RecoveryOff);
    assert_eq!(f.fates.iter().map(|j| j.fate).collect::<Vec<_>>(), [Fate::Hung, Fate::Canceled]);
    assert!(f.guilty.is_empty());
    assert_eq!(d.counters().query2(&ctx), 0, "no reset happened");
    assert_eq!(d.counters().reset, 0);
    assert!(!d.is_dead(), "the other rings go on");
    assert_eq!(d.ring(other).unwrap().jobs().count(), 1);
    assert_eq!(d.poll(other, T + 1, s(1)).unwrap().advanced, Some((0, 1)));
    d.submit(other, client(3), 256, 512, T + 1).unwrap();
    // The frozen one takes nothing more and stays that way.
    assert_eq!(d.submit(ring, client(4), 512, 768, T).unwrap_err().errno(), errno::ENODEV);
    assert_eq!(d.poll(ring, T + 1, s(0)).unwrap().state, RingState::Dead);
    assert_eq!(d.undo_submit(ring), None);
    d.remove_ring(ring).unwrap();
    let again = d.add_ring(cfg(RingClass::GfxDirect)).unwrap();
    assert_ne!(again, ring);
    assert_eq!(d.poll(again, T + 2, s(0)).unwrap().state, RingState::Idle);
}
