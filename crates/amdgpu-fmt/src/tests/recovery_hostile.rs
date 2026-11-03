//! Random chips: garbage samples, wild replies, a clock that jumps. Whatever they do, the ladder ends, mode 2 stays
//! out on the default policy, and every job ends exactly once.

use std::collections::BTreeMap;
use std::vec::Vec;

use super::Rng;
use super::watch::client;
use crate::recover::{Action, IpVersion, Next, Outcome, Policy, Reply, ResetDomain};
use crate::told::Fate;
use crate::watch::{JobMeta, Owner, RingCfg, RingClass, RingId, RingState, Sample};

const CLASSES: [RingClass; 4] = [
    RingClass::GfxDirect,
    RingClass::ComputeProcess { doorbell_dw: 4, gang_ctx: 8 },
    RingClass::GfxProcess { doorbell_dw: 5, gang_ctx: 9 },
    RingClass::Sdma { burst_nop: true },
];

fn wild_reply(r: &mut Rng) -> Reply {
    match r.below(5) {
        0 => Reply::Ok,
        1 => Reply::Failed,
        2 => Reply::Ring { ptr: r.next() },
        3 => Reply::Reset { vram_intact: r.chance(70), display_equal: r.chance(90) },
        _ => Reply::Ok,
    }
}

fn wild_sample(r: &mut Rng, sane_fence: u64, sane_rptr: u64) -> Sample {
    Sample {
        rptr_hw: if r.chance(30) { r.next() } else { sane_rptr.wrapping_add(r.below(64)) },
        fence: if r.chance(30) { r.next() } else { sane_fence.wrapping_add(r.below(3)) },
    }
}

fn round(seed: u64, policy: Policy, may_mode2: bool) {
    let mut r = Rng::new(seed);
    let mut d = ResetDomain::new(policy);
    let mut rings: Vec<RingId> = Vec::new();
    let n = 1 + r.below(4);
    for _ in 0..n {
        let mut c = RingCfg::for_780m("h", CLASSES[r.below(4) as usize], 256 << r.below(4), 0, 1 + r.below(8));
        c.timeout_us = 1 + r.below(2000);
        c.fence_start = if r.chance(20) { u64::MAX - r.below(4) } else { 0 };
        rings.push(d.add_ring(c).unwrap());
    }
    let mut now = r.below(1 << 40);
    // Every job still open: (ring, seq).
    let mut open: BTreeMap<(RingId, u64), JobMeta> = BTreeMap::new();
    let mut ptr: BTreeMap<RingId, u64> = rings.iter().map(|&id| (id, 0)).collect();

    for _ in 0..400 {
        now = match r.below(20) {
            0 => now.saturating_add(r.below(1 << 30)),
            1 => now.saturating_sub(r.below(1000)),
            _ => now.saturating_add(r.below(500)),
        };
        let id = rings[r.below(rings.len() as u64) as usize];
        if r.chance(40) {
            let meta = if r.chance(20) { JobMeta { owner: Owner::Kernel, vmid: 0, pasid: 0 } } else { client(1 + r.below(4)) };
            let start = ptr[&id];
            let end = start.wrapping_add(16 * (1 + r.below(4)));
            if let Ok(seq) = d.submit(id, meta, start, end, now) {
                assert!(open.insert((id, seq), meta).is_none(), "a number given twice");
                ptr.insert(id, end);
            }
            continue;
        }
        let sane = d.ring(id).unwrap().timeline().signalled();
        let rptr = d.ring(id).unwrap().rptr();
        let p = d.poll(id, now, wild_sample(&mut r, sane, rptr)).unwrap();
        if let Some((from, to)) = p.advanced {
            let mut s = from;
            while s != to {
                s = s.wrapping_add(1);
                assert!(open.remove(&(id, s)).is_some(), "job {s} ended twice or never was");
            }
        }
        if d.is_dead() {
            assert_eq!(p.state, RingState::Dead);
            continue;
        }
        let RingState::Hung(_) = p.state else { continue };
        let garbage: Vec<u32> = (0..64).map(|_| r.next() as u32).collect();
        let snapshot = if r.chance(50) { &garbage[..] } else { &[] };
        let rep = d.report(id, now, snapshot, &[]).unwrap();
        assert!(!std::format!("{rep}").is_empty());
        let mut next = d.begin(&rep, now).unwrap();
        let mut steps = 0;
        let f = loop {
            match next {
                Next::Done(f) => break f,
                Next::Do(step) => {
                    if !may_mode2 {
                        assert!(!matches!(step.action, Action::Mode2Reset | Action::SuspendGc), "{step:?}");
                    }
                    if let Action::Blame { ctx, .. } = step.action {
                        assert_eq!(rep.job.meta.owner, open[&(id, rep.job.seq)].owner);
                        assert!(matches!(rep.job.meta.owner, Owner::Client { ctx: c, .. } if c == ctx));
                    }
                    steps += 1;
                    assert!(steps < 64 + 4 * rings.len(), "the ladder only moves up");
                    if r.chance(10) {
                        let rp = d.ring(id).unwrap().rptr();
                        let p = d.poll(id, now, wild_sample(&mut r, rep.signalled, rp)).unwrap();
                        if let Some((from, to)) = p.advanced {
                            let mut s = from;
                            while s != to {
                                s = s.wrapping_add(1);
                                assert!(open.remove(&(id, s)).is_some());
                            }
                        }
                    }
                    assert_eq!(d.waiting(), Some(step));
                    if r.chance(15) {
                        // Nobody answers.
                        assert!(d.expire(step.deadline_us + d.policy().expire_slack_us).is_none());
                        now = now.max(step.deadline_us + d.policy().expire_slack_us + 1);
                        next = d.expire(now).unwrap();
                    } else {
                        now = now.saturating_add(r.below(1 << 20));
                        next = d.reply(wild_reply(&mut r), now).unwrap();
                    }
                }
            }
        };
        assert!(!f.counters.in_reset);
        let hung = f.fates.iter().filter(|j| j.fate == Fate::Hung).count();
        assert!(hung <= 1, "one job hung");
        for j in &f.fates {
            assert!(open.remove(&(j.ring, j.job.seq)).is_some(), "fate of a job that had ended: {j:?}");
        }
        if let Outcome::NeedsReboot { mode2 } = f.outcome {
            assert!(d.is_dead());
            assert!(may_mode2 || mode2.is_some());
            assert!(open.is_empty(), "a dead device owes no job its end");
        }
        for &rid in &rings {
            ptr.insert(rid, d.ring(rid).unwrap().wptr());
        }
    }
    // Nothing fell through: what is open is what the rings still hold.
    let held: usize = rings.iter().map(|&id| d.ring(id).unwrap().jobs().count()).sum();
    assert_eq!(held, open.len());
}

#[test]
fn hostile_chips_default_policy() {
    for seed in 0..400 {
        round(seed, Policy::for_mp1(IpVersion(13, 0, 4)), false);
    }
}

#[test]
fn hostile_chips_recovery_off() {
    for seed in 0..200 {
        let mut p = Policy::for_mp1(IpVersion(13, 0, 4));
        p.recovery = false;
        round(2000 + seed, p, false);
    }
}

#[test]
fn hostile_chips_mode2_open() {
    for seed in 0..200 {
        let mut p = Policy::for_mp1(IpVersion(13, 0, 4));
        p.open_mode2(IpVersion(13, 0, 4), "test").unwrap();
        p.reinit_limit.count = 1 + (seed as usize % 4);
        round(1000 + seed, p, true);
    }
}
