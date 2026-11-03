//! VMID manager and PASID allocator against amdgpu_ids.c behaviour.

use crate::errno;
use crate::ids::{Grab, IdError, Owner, PasidAlloc, VmidMgr};

#[test]
fn vmid_range_is_an_argument() {
    assert_eq!(VmidMgr::new(0, 7).err(), Some(IdError::BadRange));
    assert_eq!(VmidMgr::new(1, 0).err(), Some(IdError::BadRange));
    assert_eq!(VmidMgr::new(u32::MAX, 2).err(), Some(IdError::BadRange));
    let mut m = VmidMgr::new(1, 7).unwrap();
    let mut got = std::vec::Vec::new();
    for vm in 0..7 {
        got.push(m.grab(Owner(vm), 0x1000 * (vm + 1), 0, false).unwrap().unwrap().vmid);
    }
    got.sort();
    assert_eq!(got, [1, 2, 3, 4, 5, 6, 7]);
    // An eighth VM waits until a job ends, then takes the least recently used idle VMID.
    assert_eq!(m.grab(Owner(7), 0x8000, 0, false), Ok(None));
    m.job_done(3).unwrap();
    assert_eq!(m.grab(Owner(7), 0x8000, 0, false), Ok(Some(Grab { vmid: 3, needs_flush: true })));
    assert_eq!(m.job_done(9), Err(IdError::NotOurs(9)));
    assert_eq!(m.job_done(0), Err(IdError::NotOurs(0)));
}

#[test]
fn reuse_flushes_only_when_tables_changed() {
    let mut m = VmidMgr::new(1, 2).unwrap();
    let a = Owner(10);
    let g = m.grab(a, 0x5000, 4, false).unwrap().unwrap();
    assert!(g.needs_flush);
    m.job_done(g.vmid).unwrap();
    assert_eq!(m.grab(a, 0x5000, 4, false).unwrap().unwrap(), Grab { vmid: g.vmid, needs_flush: false });
    // Busy is fine for the same VM (concurrent flush), a newer sequence needs a flush.
    assert_eq!(m.grab(a, 0x5000, 5, false).unwrap().unwrap(), Grab { vmid: g.vmid, needs_flush: true });
    assert_eq!(m.active(g.vmid), Ok(2));
    // Same owner, other root (VM recreated): not reused.
    let other = m.grab(a, 0x9000, 5, false).unwrap().unwrap();
    assert_ne!(other.vmid, g.vmid);
    assert!(other.needs_flush);
}

#[test]
fn forget_and_reset_force_flushes() {
    let mut m = VmidMgr::new(1, 1).unwrap();
    let g = m.grab(Owner(1), 0x5000, 1, false).unwrap().unwrap();
    m.job_done(g.vmid).unwrap();
    m.forget(Owner(1));
    assert!(m.grab(Owner(1), 0x5000, 1, false).unwrap().unwrap().needs_flush);
    m.job_done(1).unwrap();
    m.reset_all();
    assert!(m.grab(Owner(1), 0x5000, 1, false).unwrap().unwrap().needs_flush);
    m.job_done(1).unwrap();
    assert_eq!(m.job_done(1), Err(IdError::NotBusy(1)));
}

#[test]
fn reserved_vmid_is_shared_and_counted() {
    let mut m = VmidMgr::new(1, 3).unwrap();
    assert_eq!(m.grab(Owner(1), 1, 0, true), Err(IdError::NoReserved));
    let r = m.reserve().unwrap();
    assert_eq!(m.reserve().unwrap(), r);
    // Round robin no longer hands it out.
    for vm in 0..2 {
        assert_ne!(m.grab(Owner(100 + vm), 1, 0, false).unwrap().unwrap().vmid, r);
    }
    assert_eq!(m.grab(Owner(200), 1, 0, false), Ok(None));
    let g = m.grab(Owner(1), 0x1000, 0, true).unwrap().unwrap();
    assert_eq!(g.vmid, r);
    // Another VM on the busy reserved VMID waits; the same one does not.
    assert_eq!(m.grab(Owner(2), 0x2000, 0, true), Ok(None));
    assert!(!m.grab(Owner(1), 0x1000, 0, true).unwrap().unwrap().needs_flush);
    m.unreserve().unwrap();
    m.unreserve().unwrap();
    assert_eq!(m.unreserve(), Err(IdError::NoReserved));
    assert_eq!(IdError::NoReserved.errno(), errno::EBUSY);
}

#[test]
fn pasids_like_amdgpu_pasid_alloc() {
    let mut p = PasidAlloc::new();
    assert_eq!(p.alloc(0), Err(IdError::BadBits));
    assert_eq!(p.alloc(16), Ok(0x8000));
    assert_eq!(p.alloc(16), Ok(0x8001));
    p.free(0x8000).unwrap();
    assert_eq!(p.alloc(16), Ok(0x8000), "lowest free first");
    assert_eq!(p.free(5), Err(IdError::NotAllocated(5)));
    // Width 2: [2, 4) then [1, 2), then nothing.
    let mut q = PasidAlloc::new();
    assert_eq!((q.alloc(2), q.alloc(2), q.alloc(2)), (Ok(2), Ok(3), Ok(1)));
    let e = q.alloc(2).unwrap_err();
    assert_eq!((e, e.errno()), (IdError::NoPasid, errno::ENOSPC));
    assert_eq!(PasidAlloc::new().alloc(64), Ok(1 << 30), "capped at 31 bits");
}

#[test]
fn pasid_exhaustion_then_reuse_after_free() {
    let mut p = PasidAlloc::new();
    let all: std::vec::Vec<u32> = (0..7).map(|_| p.alloc(3).unwrap()).collect();
    assert_eq!(all, [4, 5, 6, 7, 2, 3, 1]);
    assert_eq!(p.alloc(3), Err(IdError::NoPasid));
    p.free(6).unwrap();
    p.free(2).unwrap();
    assert_eq!(p.alloc(3), Ok(6), "the widest range first");
    assert_eq!(p.alloc(3), Ok(2));
    assert_eq!(p.alloc(3), Err(IdError::NoPasid));
    assert_eq!(p.in_use(), 7);
}

/// Random grabs, job ends, forgets, resets and reserved use against a model: a busy VMID never serves two
/// VMs, and a grab without flush only happens when this VM and root were flushed at a sequence at least as new
/// on that VMID since it last changed hands.
#[test]
fn vmids_against_a_model() {
    use std::collections::BTreeMap;
    let mut rng = super::Rng::new(7);
    let (mut waits, mut flushes, mut reuses) = (0, 0, 0);
    for round in 0..200 {
        let count = 1 + rng.below(7) as u32;
        let mut m = VmidMgr::new(1, count).unwrap();
        // vmid -> (owner, pd, flushed seq) as the model knows it; running jobs per vmid with their owner.
        let mut known: BTreeMap<u32, (u64, u64, u64)> = BTreeMap::new();
        let mut jobs: std::vec::Vec<(u32, u64)> = std::vec::Vec::new();
        let mut seqs = [0u64; 10];
        let mut reserved_users = 0u32;
        let mut reserved_vmid = None;
        for _ in 0..300 {
            match rng.below(10) {
                0..=4 => {
                    let vm = rng.below(10);
                    if rng.chance(30) {
                        seqs[vm as usize] += 1;
                    }
                    let (pd, seq) = (0x1000 * (vm + 1), seqs[vm as usize]);
                    let want_res = reserved_users > 0 && rng.chance(20);
                    match m.grab(Owner(vm), pd, seq, want_res).unwrap() {
                        None => {
                            waits += 1;
                            // Waiting is only right when no usable VMID is idle.
                            if !want_res {
                                let idle = (1..=count).any(|v| {
                                    Some(v) != reserved_vmid && !jobs.iter().any(|j| j.0 == v)
                                });
                                assert!(!idle, "round {round}: waited with an idle VMID");
                            }
                        }
                        Some(g) => {
                            assert!((1..=count).contains(&g.vmid));
                            assert_eq!(want_res, Some(g.vmid) == reserved_vmid);
                            for &(v, o) in &jobs {
                                assert!(v != g.vmid || o == vm, "round {round}: VMID {v} shared while busy");
                            }
                            if g.needs_flush {
                                flushes += 1;
                                known.insert(g.vmid, (vm, pd, seq));
                            } else {
                                reuses += 1;
                                let k = known.get(&g.vmid).copied();
                                assert!(
                                    matches!(k, Some((o, p, f)) if o == vm && p == pd && f >= seq),
                                    "round {round}: no flush on VMID {} for {vm}/{seq}, model {k:?}",
                                    g.vmid
                                );
                            }
                            jobs.push((g.vmid, vm));
                        }
                    }
                }
                5..=7 if !jobs.is_empty() => {
                    let (v, _) = jobs.swap_remove(rng.below(jobs.len() as u64) as usize);
                    m.job_done(v).unwrap();
                }
                8 => {
                    let vm = rng.below(10);
                    m.forget(Owner(vm));
                    known.retain(|_, k| k.0 != vm);
                }
                9 if rng.chance(20) => {
                    m.reset_all();
                    known.clear();
                }
                _ if reserved_users == 0 || rng.chance(50) => {
                    if let Ok(v) = m.reserve() {
                        reserved_users += 1;
                        reserved_vmid = Some(v);
                    }
                }
                _ => {
                    m.unreserve().unwrap();
                    reserved_users -= 1;
                    if reserved_users == 0 {
                        reserved_vmid = None;
                    }
                }
            }
        }
        for (v, _) in jobs {
            m.job_done(v).unwrap();
        }
        for v in 1..=count {
            assert_eq!(m.active(v), Ok(0));
        }
    }
    assert!(waits > 0 && flushes > 0 && reuses > 0, "{waits} {flushes} {reuses}");
}
