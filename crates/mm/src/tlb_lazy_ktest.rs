//! MM-tlb3 ktests: a CPU that holds a user root only lazily is not asked by flushes that free no page tables,
//! and catches up when it loads the root again (Linux `tlb_is_not_lazy`). Run them with
//! `cargo xtask ktest mm.tlb.lazy -- -smp 16` (with fewer CPUs they run too, on what is there).
//!
//! - `mm.tlb.lazy_skip`: rounds of "every AP reads a page, goes lazy; this CPU unmaps (or write-protects) it;
//!   every AP loads the root again and must fault". An AP still lazy right before its read was skipped and must
//!   have caught up; the frames come back. Stale entries would read the old value (or a reused frame's).
//!   No shootdown IPI reaches an AP that stays lazy through the unmap.
//! - `mm.tlb.lazy_tables`: an unmap that frees page tables still makes every lazy AP leave the root.
//! - `mm.tlb.lazy_copy`: a kernel thread on a CPU that holds the root lazily (with a stale entry after a skipped
//!   unmap and a remap) copies the root's memory and must see the new contents (`tlb::without_lazy`).
//!
//! The counts in `lazy_skip` are exact: nothing else loads the test root, so an AP that is lazy on it at its
//! read was lazy on it since its `ap_lazy`, through the flush (counted as a skip in the flush's first look) and
//! until its catch-up; a request reaching a lazy AP that is behind makes it leave, so it would not count as kept.

use crate::linux::{MAP_ANONYMOUS, MAP_FIXED, MAP_PRIVATE, PROT_READ};
use crate::tlb_ktest::{ap_read_in, ap_read_lazy, ap_write_in, aps, on, FAULT, LAZY_BIT, PF_P, PF_W, PROBE_ROOT, WRITE_VAL};
use crate::{AddressSpace, Fill};
use alloc::format;
use alloc::sync::Arc;
use alloc::vec::Vec;
use alloc::string::String;
use core::sync::atomic::{AtomicU64, Ordering::SeqCst};
use hal::paging::{self, probe, tlb};
use ktest::{kassert, ktest, TestResult};

const P: u64 = 4096;
const RW: u64 = 3;
const AP: u64 = MAP_ANONYMOUS | MAP_PRIVATE;
const PAGES: u64 = 8;
const ROUNDS: u64 = 48;
const SEC: u64 = 1_000_000_000;
/// Pages `lazy_copy` fills after its unmap, so the freed frame is handed out to one of them, not to the remap.
const OTHERS: u64 = 64;

fn e<T: core::fmt::Debug>(x: T) -> String {
    format!("{x:?}")
}

fn val(i: u64) -> u64 {
    0x0BAD_1A2E_0000 | i
}

fn user_frames() -> u64 {
    mm_core::frames().map_or(0, |f| f.stats().used_by_kind[mm_core::Kind::User as usize])
}

// What the APs run (hard-IRQ context). The look at the lazy state and the access are one call, so no shootdown
// request can come in between.

fn lazy_here_on_probe_root() -> bool {
    tlb::lazy_here() && paging::current_root() == PROBE_ROOT.load(SeqCst)
}

fn ap_lazy(_: u64) -> u64 {
    tlb::activate_lazy_kernel();
    lazy_here_on_probe_root() as u64
}

fn ap_write_lazy(va: u64) -> u64 {
    let was = lazy_here_on_probe_root();
    ap_write_in(va) | if was { LAZY_BIT } else { 0 }
}

fn ap_root(_: u64) -> u64 {
    paging::current_root()
}

fn gone(r: u64) -> bool {
    r & FAULT != 0 && r & PF_P == 0
}

fn read_only(r: u64) -> bool {
    r & FAULT != 0 && r & (PF_P | PF_W) == PF_P | PF_W
}

fn lazy_skip() -> TestResult {
    let aps = aps();
    if aps.is_empty() {
        return ktest::skip("one CPU");
    }
    let frames_before = user_frames();
    let s0 = tlb::stats();
    let (mut kept, mut asked, mut lazy_before, mut shot) = (0u64, 0u64, 0u64, 0u64);
    {
        let a = AddressSpace::with_fill(Fill::OnFault).map_err(e)?;
        let base = a.sys_mmap(0, PAGES * P, RW, AP, 0).map_err(e)?;
        for i in 0..PAGES {
            a.write_u64(base + i * P, val(i)).map_err(e)?;
        }
        let _s = probe::user_probes().map_err(e)?;
        PROBE_ROOT.store(a.root(), SeqCst);
        // The last page keeps the page table alive: no round of this test frees a table.
        let tables = paging::count_user_tables(a.root());
        for r in 0..ROUNDS {
            let va = base + (r % (PAGES - 1)) * P;
            let v = val(0x100 + r);
            a.write_u64(va, v).map_err(e)?;
            for &c in &aps {
                kassert!(on(c, ap_read_in, va)? == v, "round {r}: CPU {c} before the change");
                lazy_before += on(c, ap_lazy, 0)?;
            }
            // Odd rounds write-protect first: a lazy AP must not write through its old writable entry.
            if r % 2 == 1 {
                a.sys_mprotect(va, P, PROT_READ).map_err(e)?;
                for &c in &aps {
                    WRITE_VAL.store(v, SeqCst);
                    let w = on(c, ap_write_lazy, va)?;
                    kept += (w & LAZY_BIT != 0) as u64;
                    kassert!(read_only(w), "round {r}: CPU {c} wrote a write-protected page: {w:#x}");
                    on(c, ap_lazy, 0)?;
                }
            }
            let irqs: Vec<u64> = aps.iter().map(|&c| tlb::cpu_irqs(c)).collect();
            let (ipis, kflush) = (tlb::stats().ipis, tlb::stats().kernel);
            a.sys_munmap(va, P).map_err(e)?;
            asked += tlb::stats().ipis - ipis;
            // A kernel-half flush from elsewhere in this window would interrupt every AP: such a round is not
            // judged for IPIs.
            let quiet = tlb::stats().kernel == kflush;
            for (k, &c) in aps.iter().enumerate() {
                let x = on(c, ap_read_lazy, va)?;
                kept += (x & LAZY_BIT != 0) as u64;
                kassert!(gone(x), "round {r}: CPU {c} read {x:#x} after munmap");
                if quiet && x & LAZY_BIT != 0 && tlb::cpu_irqs(c) != irqs[k] {
                    shot += 1;
                }
            }
            kassert!(a.sys_mmap(va, P, RW, AP | MAP_FIXED, 0).map_err(e)? == va, "round {r}: remap refused");
        }
        kassert!(paging::count_user_tables(a.root()) == tables, "a round changed the page tables");
        let s1 = tlb::stats();
        kassert!(s1.give_ups == s0.give_ups, "a shootdown gave up");
        kassert!(shot == 0, "{shot} times a shootdown IPI reached an AP that held the root lazily through the unmap");
        kassert!(s1.lazy_skips - s0.lazy_skips >= kept, "{kept} lazy reads, {} skips", s1.lazy_skips - s0.lazy_skips);
        kassert!(s1.lazy_catchups - s0.lazy_catchups >= kept, "{kept} lazy reads, {} catch-ups", s1.lazy_catchups - s0.lazy_catchups);
        klog::event!(Info, "mm", "ktest.tlb_lazy_skip", aps = aps.len(), rounds = ROUNDS, kept = kept, ipis_in_unmaps = asked,
            skips = s1.lazy_skips - s0.lazy_skips, catchups = s1.lazy_catchups - s0.lazy_catchups, lazy_leaves = s1.lazy_leaves - s0.lazy_leaves);
    }
    kassert!(user_frames() == frames_before, "{} bytes of user frames not back", user_frames().saturating_sub(frames_before));
    if kept == 0 {
        // The APs were lazy right before most unmaps but none was after it: they were asked and left (no skip).
        kassert!(lazy_before <= ROUNDS, "{lazy_before} lazy APs before the unmaps, none lazy after them");
        // Every AP ran another thread between going lazy and its read, every time (busy machine).
        return ktest::skip("no AP stayed lazy on the test root");
    }
    Ok(())
}
ktest!("mm.tlb.lazy_skip", Safe, lazy_skip);

fn lazy_tables() -> TestResult {
    let aps = aps();
    if aps.is_empty() {
        return ktest::skip("one CPU");
    }
    let s0 = tlb::stats();
    let a = AddressSpace::with_fill(Fill::OnFault).map_err(e)?;
    let base = a.sys_mmap(0, P, RW, AP, 0).map_err(e)?;
    a.write_u64(base, val(1)).map_err(e)?;
    // A page far from the other one: its page table (and directory) hold nothing else.
    let far = 1u64 << 44;
    kassert!(a.sys_mmap(far, P, RW, AP | MAP_FIXED, 0).map_err(e)? == far, "fixed mapping refused");
    a.write_u64(far, val(2)).map_err(e)?;
    let _s = probe::user_probes().map_err(e)?;
    PROBE_ROOT.store(a.root(), SeqCst);
    let mut lazy = 0u64;
    for &c in &aps {
        kassert!(on(c, ap_read_in, far)? == val(2), "CPU {c} far page");
        lazy += on(c, ap_lazy, 0)?;
    }
    if lazy == 0 {
        // Every AP ran another thread right after `ap_lazy` (busy machine): nothing to shoot lazily.
        return ktest::skip("no AP lazy on the test root");
    }
    let t0 = paging::count_user_tables(a.root());
    a.sys_munmap(far, P).map_err(e)?;
    let t1 = paging::count_user_tables(a.root());
    kassert!(t1 < t0, "the far unmap freed no tables ({t0} -> {t1})");
    for &c in &aps {
        let r = on(c, ap_root, 0)?;
        kassert!(r != a.root(), "CPU {c} still holds the root after tables were freed");
    }
    kassert!(a.loaded_on().is_empty(), "still in the mask: {:?}", a.loaded_on());
    let s1 = tlb::stats();
    // At least one: an AP that ran another thread between `ap_lazy` and the unmap left the root otherwise.
    kassert!(s1.lazy_leaves > s0.lazy_leaves, "{lazy} lazy APs, no lazy leave");
    for &c in &aps {
        kassert!(gone(on(c, ap_read_in, far)?), "CPU {c} reads the far page after munmap");
        kassert!(on(c, ap_read_in, base)? == val(1), "CPU {c} near page");
    }
    klog::event!(Info, "mm", "ktest.tlb_lazy_tables", aps = aps.len(), lazy = lazy, tables_freed = t0 - t1,
        lazy_leaves = s1.lazy_leaves - s0.lazy_leaves, ipis = s1.ipis - s0.ipis);
    Ok(())
}
ktest!("mm.tlb.lazy_tables", Safe, lazy_tables);

/// `lazy_copy`'s steps: 1 the thread holds the root lazily, 2 the test changed the page, 3 the thread copied.
static COPY_STEP: AtomicU64 = AtomicU64::new(0);
/// What the kernel thread in `lazy_copy` saw: the value before the change, whether it held the root lazily
/// before the change and before the copy (1 + lazy), the value it copied, and its table walks for that copy
/// (1: it walked; 0: it went through this CPU's TLB).
static COPY_FIRST: AtomicU64 = AtomicU64::new(0);
static COPY_LAZY0: AtomicU64 = AtomicU64::new(0);
static COPY_LAZY: AtomicU64 = AtomicU64::new(0);
static COPY_VAL: AtomicU64 = AtomicU64::new(0);
static COPY_WALKS: AtomicU64 = AtomicU64::new(0);

/// Waits (bounded) until `COPY_STEP` reaches `step`.
fn wait_step(step: u64) -> bool {
    let until = time::now_ns() + 5 * SEC;
    while COPY_STEP.load(SeqCst) < step {
        if time::now_ns() > until {
            return false;
        }
        core::hint::spin_loop();
    }
    true
}

fn lazy_here_on(root: u64) -> u64 {
    (tlb::lazy_here() && paging::current_root() == root) as u64 + 1
}

/// The kernel thread on the lazy CPU: loads the root, reads the page through its TLB (fast path), holds the root
/// lazily while it waits (spinning: a halted vCPU may lose its TLB on the host and hide a stale entry), then
/// copies the page again after the test unmapped and remapped it.
fn copier(a: &AddressSpace, base: u64) {
    let root = a.root();
    paging::activate(root);
    COPY_FIRST.store(a.read_u64(base).unwrap_or(u64::MAX), SeqCst);
    tlb::activate_lazy_kernel();
    COPY_LAZY0.store(lazy_here_on(root), SeqCst);
    COPY_STEP.store(1, SeqCst);
    if wait_step(2) {
        COPY_LAZY.store(lazy_here_on(root), SeqCst);
        let w0 = hal::usercopy::stats().walks;
        COPY_VAL.store(a.read_u64(base).unwrap_or(u64::MAX), SeqCst);
        COPY_WALKS.store(hal::usercopy::stats().walks - w0, SeqCst);
    }
    paging::activate(paging::home_root());
    COPY_STEP.store(3, SeqCst);
}

fn lazy_copy() -> TestResult {
    let aps = aps();
    let Some(&c) = aps.first() else { return ktest::skip("one CPU") };
    let s0 = tlb::stats();
    let a = Arc::new(AddressSpace::with_fill(Fill::OnFault).map_err(e)?);
    let base = a.sys_mmap(0, 2 * P, RW, AP, 0).map_err(e)?;
    // The second page keeps the page table: the unmap below frees no table, so the lazy CPU is skipped.
    a.write_u64(base + P, val(9)).map_err(e)?;
    a.write_u64(base, val(1)).map_err(e)?;
    for x in [&COPY_STEP, &COPY_FIRST, &COPY_LAZY0, &COPY_LAZY, &COPY_VAL, &COPY_WALKS] {
        x.store(0, SeqCst);
    }
    let t = a.clone();
    let o = sched::SpawnOpts { cpu: Some(c), pin: true, ..sched::SpawnOpts::default() };
    let h = sched::spawn_with("mm-tlb-lazy-copy", sched::Class::Normal, o, move || copier(&t, base))
        .map_err(|x| format!("spawn on CPU {c}: {x:?}"))?;
    let r = (|| -> Result<bool, String> {
        if !wait_step(1) {
            return Err(format!("the thread on CPU {c} did not get going within 5 s"));
        }
        let old = paging::translate(a.root(), base).map(|(pa, _)| pa);
        a.sys_munmap(base, P).map_err(e)?;
        // The freed frame goes to other pages first (an allocator that hands it out again would otherwise put
        // the new value under the stale entry too), then the address gets a new page.
        let others = a.sys_mmap(0, OTHERS * P, RW, AP, 0).map_err(e)?;
        for i in 0..OTHERS {
            a.write_u64(others + i * P, val(2)).map_err(e)?;
        }
        if a.sys_mmap(base, P, RW, AP | MAP_FIXED, 0).map_err(e)? != base {
            return Err("remap refused".into());
        }
        a.write_u64(base, val(3)).map_err(e)?;
        Ok(old.is_some() && paging::translate(a.root(), base).map(|(pa, _)| pa) != old)
    })();
    // The thread always gets to finish, also when a step above failed.
    COPY_STEP.fetch_max(2, SeqCst);
    kassert!(h.join_timeout(10 * SEC), "the copying thread did not finish within 10 s");
    let moved = r?;
    let (lazy0, was_lazy, got) = (COPY_LAZY0.load(SeqCst) == 2, COPY_LAZY.load(SeqCst) == 2, COPY_VAL.load(SeqCst));
    let s1 = tlb::stats();
    klog::event!(Info, "mm", "ktest.tlb_lazy_copy", cpu = c, lazy_before = lazy0, lazy_at_copy = was_lazy,
        skips = s1.lazy_skips - s0.lazy_skips, drops = s1.lazy_drops - s0.lazy_drops, walks = COPY_WALKS.load(SeqCst), new_frame = moved);
    kassert!(COPY_FIRST.load(SeqCst) == val(1), "the thread read {:#x} before the change", COPY_FIRST.load(SeqCst));
    kassert!(got == val(3), "the kernel thread read {got:#x} (a stale entry: {:#x} or {:#x}), want {:#x}", val(1), val(2), val(3));
    if !(lazy0 && was_lazy) {
        // CPU c ran a user thread between the steps (busy machine).
        return ktest::skip("the copying thread did not hold the root lazily throughout");
    }
    kassert!(s1.lazy_drops > s0.lazy_drops, "the copy through a lazily held root did not leave it first");
    kassert!(COPY_WALKS.load(SeqCst) == 1, "the copy through a lazily held root did not walk the tables");
    Ok(())
}
ktest!("mm.tlb.lazy_copy", Safe, lazy_copy);
