//! ktest `mm.oom.kill` (QemuOnly: the killer chooses among every program of the machine): three sleeping
//! programs with 8, 48 and 96 MiB of written memory, the largest with oom_score_adj -1000. The killer must choose
//! the 48 MiB one (Linux `oom_badness`: the largest that may be killed), end it with SIGKILL, and its memory must
//! come back (reaped or freed by its end) while the other two live on.
//!
//! ktest `mm.oom.pressure` (its own boot, host/tests/mm/83-oom.sh: 1 GiB, 4 CPUs): a program holds nearly all
//! free memory, then a kernel thread keeps taking 28 KiB heap blocks (order 3, the Mini-PC's crash size) until it
//! holds more than was left. No block may fail: the heap's never-failing loop has to end in the OOM killer, which
//! ends the program, and the blocks go on.

use crate::ktests::{ended, image, need, start, SEC};
use crate::{ExitStatus, Process};
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use hal::entry::test_programs;
use ktest::{kassert, ktest, TestResult};
use mm::linux::{MAP_ANONYMOUS, MAP_PRIVATE};
use mm_core::oom::Verdict;
use uapi::ab::signal::SIGKILL;

const MIB: u64 = 1 << 20;

/// `bytes` of anonymous memory in `p`, every page written from the kernel.
fn fill(p: &Process, bytes: u64) -> Result<(), String> {
    let at = p.mm.sys_mmap(0, bytes, 3, MAP_ANONYMOUS | MAP_PRIVATE, 0).map_err(|e| format!("mmap in {}: {e:?}", p.pid))?;
    for off in (0..bytes).step_by(4096) {
        p.mm.write_u64(at + off, off).map_err(|e| format!("write in {}: {e:?}", p.pid))?;
    }
    Ok(())
}

fn oom_kill() -> TestResult {
    let sleeper = image(need(test_programs::sleep_exit())?, false);
    // argc >= 100: sleeps for good.
    let forever: Vec<&str> = (0..100).map(|_| "s").collect();
    let mut progs: Vec<Arc<Process>> = Vec::new();
    for (name, mib) in [("/ab-ktest/oom-small", 8), ("/ab-ktest/oom-big", 48), ("/ab-ktest/oom-shielded", 96)] {
        let (p, _) = start(name, &sleeper, &forever, None, false)?;
        fill(&p, mib * MIB)?;
        progs.push(p);
    }
    let (small, big, shielded) = (progs[0].clone(), progs[1].clone(), progs[2].clone());
    // A child of a program, with memory of its own, is no vfork child: nobody here is passed over.
    let (child, _) = start("/ab-ktest/oom-child", &sleeper, &forever, Some(&small), false)?;
    for p in [&small, &big, &shielded, &child] {
        kassert!(!crate::oom::passed_over_for_test(p), "{} ({}) would be passed over", p.pid, p.name);
    }
    shielded.set_oom_score_adj(mm_core::oom::SCORE_ADJ_MIN, true).map_err(|e| format!("oom_score_adj: {e:?}"))?;
    let choice = crate::oom::choice_for_test();
    kassert!(choice.map(|c| c.0) == Some(big.pid), "the killer would end {choice:?}, want the 48 MiB program {}", big.pid);
    let rss = big.mm.rss_pages();
    kassert!(rss >= 48 * MIB / 4096, "the big program holds {rss} pages");
    let v = mm_core::oom::out_of_memory(0);
    kassert!(v == Verdict::Killed, "out_of_memory answered {v:?}");
    let st = ended(&big, 10)?;
    kassert!(st == ExitStatus::Signaled(SIGKILL), "the victim ended with {}", st.describe());
    // The reaper marks the victim done (reaped, or ended first) before another may be chosen.
    let t0 = time::now_ns();
    while !crate::oom::victim_for_test().is_some_and(|v| v.0 == big.pid && v.1 && v.2) && time::now_ns() < t0 + 5 * SEC {
        sched::sleep_ms(10);
    }
    let v = crate::oom::victim_for_test();
    kassert!(v.is_some_and(|v| v.0 == big.pid && v.1 && v.2), "the reaper did not finish the victim: {v:?}");
    kassert!(big.mm.rss_pages() == 0, "the victim still holds {} pages", big.mm.rss_pages());
    kassert!(!small.is_finished() && !shielded.is_finished(), "a program that was not chosen ended");
    for p in [&small, &shielded, &child] {
        p.send_signal(SIGKILL);
        let st = ended(p, 10)?;
        kassert!(st == ExitStatus::Signaled(SIGKILL), "{} ended with {}", p.pid, st.describe());
    }
    Ok(())
}
ktest!("mm.oom.kill", QemuOnly, oom_kill);

fn metric(name: &str) -> u64 {
    klog::metrics::snapshot().find(|m| m.0 == name).map_or(0, |m| m.1)
}

fn oom_pressure() -> TestResult {
    let f = mm_core::frames().ok_or("no frame allocator")?;
    let total = f.stats().total_bytes;
    kassert!(total <= 2 << 30, "{} MiB: the test is for a small machine (83-oom.sh boots 1 GiB)", total >> 20);
    let sleeper = image(need(test_programs::sleep_exit())?, false);
    let forever: Vec<&str> = (0..100).map(|_| "s").collect();
    let (hog, _) = start("/ab-ktest/oom-hog", &sleeper, &forever, None, false)?;
    // All but 48 MiB of what is free now: the writes stay clear of the wall (a fault that met it would hold the
    // program's mm.as in the loop, which a reaper needs), the blocks below do not.
    let free = f.stats().usable_free_bytes();
    let hold = free.saturating_sub(48 * MIB) & !(MIB - 1);
    // The strict commit keeps a quarter of a small machine for the kernel (V2-PLAN R2): lifted for the hog, which
    // stands for what R2 cannot see (kernel memory, a machine whose cache cannot go).
    mm::commit::set_test_limit(Some(total / 4096 * 2));
    let filled = fill(&hog, hold);
    mm::commit::set_test_limit(None);
    filled?;
    let (kills0, nofail0) = (metric("mm.oom.kills"), metric("mm.alloc.nofail_allocs"));
    let fails0 = mm_core::HEAP.reports().heap_alloc_fails.load(core::sync::atomic::Ordering::Relaxed);
    // 96 MiB of 28 KiB blocks: twice what was left.
    let mut blocks: Vec<Vec<u8>> = Vec::new();
    blocks.try_reserve_exact(4096).map_err(|_| "no room for the list")?;
    let t0 = time::now_ns();
    let mut refused = 0u64;
    while (blocks.len() as u64) * 28_000 < 96 * MIB {
        let mut b: Vec<u8> = Vec::new();
        if b.try_reserve_exact(28_000).is_err() {
            refused += 1;
            if refused > 100 {
                break;
            }
            continue;
        }
        b.resize(28_000, 1);
        blocks.push(b);
        kassert!(time::now_ns() < t0 + 120 * SEC, "the blocks took more than two minutes ({} held)", blocks.len());
    }
    let ms = (time::now_ns() - t0) / 1_000_000;
    let st = ended(&hog, 30)?;
    let (kills, nofail) = (metric("mm.oom.kills") - kills0, metric("mm.alloc.nofail_allocs") - nofail0);
    let fails = mm_core::HEAP.reports().heap_alloc_fails.load(core::sync::atomic::Ordering::Relaxed) - fails0;
    klog::event!(Info, "proc", "ktest.oom_pressure", hold_mb = hold >> 20, blocks = blocks.len(), ms = ms, refused = refused, kills = kills, nofail = nofail,
        heap_fails = fails, nofail_max_us = metric("mm.alloc.nofail_max_us"), reaped = metric("mm.oom.reaped_pages"));
    drop(blocks);
    kassert!(refused == 0, "{refused} heap blocks of 28 KiB were refused");
    kassert!(st == ExitStatus::Signaled(SIGKILL), "the program ended with {}", st.describe());
    kassert!(kills >= 1 && nofail >= 1, "kills {kills}, never-failing allocations {nofail}");
    Ok(())
}
ktest!("mm.oom.pressure", Run("mm/83-oom"), oom_pressure);
