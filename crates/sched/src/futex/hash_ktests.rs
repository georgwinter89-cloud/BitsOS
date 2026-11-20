//! L7-SYNC row 49: what a per-process futex hash (Linux 7.3.1's `futex_private_hash`, kernel/futex/core.c:239,
//! sized `clamp(roundup_pow_of_two(4 * threads), 16, global)`) would cost or save against V2's one table of
//! `256 x CPUs` buckets. Many threads, each on its own private futex word of one address space, hash, lock and
//! scan their bucket as `wake` does, once through the global table and once through a table of the private
//! size reached through a per-address-space pointer (the lookup a private hash adds to every operation; one
//! load here, the cheapest it can be: Linux takes an RCU pointer and a per-CPU reference). The time per visit is
//! logged for both (`sc futex.hash_bench`, ps); the decision is in plan/spec/core.md §9.3.

use super::table::{self, Key, KeyKind};
use super::{FEntry, Table, buckets};
use crate::Class;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use ktest::{TestResult, kassert, ktest};

const THREADS: usize = 64;
const ROUNDS: u64 = 20_000;

/// The address space's private table as a private hash would find it.
static PRIVATE: ksync::OnceRef<Table> = ksync::OnceRef::new();

/// One run: every thread does `ROUNDS` bucket visits on its own key; returns the run's wall time (from the start
/// signal until the last thread is done) per visit in ps, so threads sharing a CPU do not count their waits.
fn run(global: &'static Table, private: bool) -> Result<u64, String> {
    let go = Arc::new(AtomicBool::new(false));
    let ready = Arc::new(AtomicU64::new(0));
    let mut hs = Vec::new();
    for i in 0..THREADS {
        let (go, ready) = (go.clone(), ready.clone());
        let h = crate::spawn("sc-fxhash", Class::Normal, move || {
            // One address space (object), word i on its own cache line.
            let a = 0x40_0000 + i as u64 * 64;
            let key = Key { kind: KeyKind::Private, object: 0x5c_0000, page: a & !0xfff, offset: (a & 0xfff) as u32 };
            ready.fetch_add(1, Ordering::AcqRel);
            while !go.load(Ordering::Acquire) {
                crate::yield_now();
            }
            for _ in 0..ROUNDS {
                let t: &Table = if private { PRIVATE.get().unwrap_or(global) } else { global };
                let mut q = t.lock(t.index(&key));
                let _ = table::wake(&mut q, &key, 1, super::BITSET_MATCH_ANY, &mut |_e: FEntry| {});
            }
        })
        .map_err(|e| alloc::format!("spawn: {e:?}"))?;
        hs.push(h);
    }
    while ready.load(Ordering::Acquire) < THREADS as u64 {
        crate::yield_now();
    }
    let t0 = time::now_ns();
    go.store(true, Ordering::Release);
    for h in hs {
        if !h.join_timeout(60 * super::ktests::SEC) {
            return Err("a bench thread did not finish".into());
        }
    }
    Ok((time::now_ns() - t0) * 1000 / (THREADS as u64 * ROUNDS))
}

ktest!("sc.futex.hash.private_bench", Safe, private_bench);
fn private_bench() -> TestResult {
    let global = buckets().map_err(|e| alloc::format!("table: {e}"))?;
    let cpus = crate::online_cpus().max(1);
    let size = (4 * THREADS.min(cpus)).next_power_of_two().clamp(16, global.buckets.len());
    if PRIVATE.get().is_none() {
        let t = Table::with_buckets(size).map_err(|e| alloc::format!("table: {e}"))?;
        PRIVATE.set(alloc::boxed::Box::leak(t));
    }
    let private_buckets = PRIVATE.get().map_or(0, |t| t.buckets.len());
    let (mut g, mut p) = (u64::MAX, u64::MAX);
    // Interleaved, best of three each: the host's load moves both alike.
    for _ in 0..3 {
        g = g.min(run(global, false)?);
        p = p.min(run(global, true)?);
    }
    kassert!(g > 0 && p > 0 && private_buckets == size, "no time measured: global {g} private {p} ({private_buckets} buckets)");
    klog::event!(Info, "sc", "futex.hash_bench", threads = THREADS, cpus = cpus, global_buckets = global.buckets.len(), private_buckets = size, global_ps = g, private_ps = p);
    Ok(())
}
