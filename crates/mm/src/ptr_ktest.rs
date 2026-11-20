//! L7-VFS row #52, decision in plan/spec/storage.md §3.4: what freeing the page table a whole-2 MiB MADV_DONTNEED
//! empties would cost (Linux 7.3.1's CONFIG_PT_RECLAIM, mm/madvise.c:860, memory.c:2113). Numbers for the desk,
//! never judged by time (only the page-table counts and the data are checked). Run it with
//! `cargo xtask ktest mm.as.ptr_measure -- -smp 8` and read the `ktest.ptr_measure` events.
//!
//! A 2 MiB anonymous VMA with MADV_NOHUGEPAGE (4 KiB pages: its own page table). Per round every AP reads the
//! first page and goes lazy on the space, then this CPU times MADV_DONTNEED of the 2 MiB, then the refill (one
//! write, which faults the page and, after a reclaim, a new table in). Modes keep / reclaim / keep / reclaim,
//! `ROUNDS` rounds each; "keep" is V2's MADV_DONTNEED, "reclaim" `test_dontneed_reclaim`. Freeing a table makes
//! the round a full flush that asks the lazy CPUs too (IPIs); keeping it lets them catch up later.

use crate::linux::{madv, MAP_ANONYMOUS, MAP_FIXED, MAP_PRIVATE};
use crate::tlb_ktest::{ap_read_in, aps, on, PROBE_ROOT};
use crate::{AddressSpace, Fill};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::Ordering::SeqCst;
use hal::paging::{self, probe, tlb};
use ktest::{kassert, ktest, TestResult};

const P: u64 = 4096;
const RW: u64 = 3;
const AP: u64 = MAP_ANONYMOUS | MAP_PRIVATE;
const HUGE: u64 = 2 << 20;
const ROUNDS: usize = 41;
/// 2 MiB aligned, apart from the other mm ktests' places.
const BASE: u64 = 0x34_0000_0000;

fn e<T: core::fmt::Debug>(x: T) -> String {
    format!("{x:?}")
}

fn ap_lazy(_: u64) -> u64 {
    tlb::activate_lazy_kernel();
    (tlb::lazy_here() && paging::current_root() == PROBE_ROOT.load(SeqCst)) as u64
}

/// (median, 90th percentile) in ns.
fn spread(mut v: Vec<u64>) -> (u64, u64) {
    v.sort_unstable();
    (v[v.len() / 2], v[v.len() * 9 / 10])
}

fn measure() -> TestResult {
    let aps = aps();
    if aps.is_empty() {
        return ktest::fail("one CPU");
    }
    let a = AddressSpace::with_fill(Fill::OnFault).map_err(e)?;
    let m = a.sys_mmap(BASE, HUGE, RW, AP | MAP_FIXED, 0).map_err(e)?;
    a.sys_madvise(m, HUGE, madv::NOHUGEPAGE).map_err(e)?;
    // A page in the next 2 MiB keeps the directory above the measured table.
    let far = a.sys_mmap(BASE + HUGE, P, RW, AP | MAP_FIXED, 0).map_err(e)?;
    a.write_u64(far, 7).map_err(e)?;
    a.write_u64(m, 1).map_err(e)?;
    let _s = probe::user_probes().map_err(e)?;
    PROBE_ROOT.store(a.root(), SeqCst);
    let tables = paging::count_user_tables(a.root());
    for (k, reclaim) in [false, true, false, true].into_iter().enumerate() {
        let (mut zap, mut refill, mut lazy) = (Vec::new(), Vec::new(), 0u64);
        let ipis = tlb::stats().ipis;
        for r in 0..ROUNDS {
            let v = 0x5A00 + r as u64;
            for &c in &aps {
                kassert!(on(c, ap_read_in, m)? == v - 1 || r == 0, "mode {k} round {r}: CPU {c} read a stale value");
                lazy += on(c, ap_lazy, 0)?;
            }
            let t0 = time::now_ns();
            if reclaim {
                a.test_dontneed_reclaim(m, m + HUGE).map_err(e)?;
            } else {
                a.sys_madvise(m, HUGE, madv::DONTNEED).map_err(e)?;
            }
            let t1 = time::now_ns();
            let want = if reclaim { tables - 1 } else { tables };
            kassert!(paging::count_user_tables(a.root()) == want, "mode {k} round {r}: page tables {} (want {want})", paging::count_user_tables(a.root()));
            a.write_u64(m, v).map_err(e)?;
            refill.push(time::now_ns() - t1);
            zap.push(t1 - t0);
            kassert!(paging::count_user_tables(a.root()) == tables, "mode {k} round {r}: the refill made no table");
        }
        let ((zm, z90), (rm, _)) = (spread(zap), spread(refill));
        klog::event!(Info, "mm", "ktest.ptr_measure", mode = if reclaim { "reclaim" } else { "keep" }, rounds = ROUNDS, aps = aps.len(),
            lazy_reads = lazy, dontneed_median_ns = zm, dontneed_p90_ns = z90, refill_median_ns = rm, ipis = tlb::stats().ipis - ipis);
    }
    kassert!(a.read_u64(far).map_err(e)? == 7, "the far page lost its data");
    Ok(())
}
ktest!("mm.as.ptr_measure", Safe, measure);
