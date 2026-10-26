//! ktests of the TLB shootdown (MM-6). Threads run only on the BSP until SC-3, so the other CPUs take part
//! through `hal::smp::call_on` (one step at a time, the AP idles with interrupts on in between: real IPIs) and
//! through storm workers started on a dynamic vector of each AP (interrupts off, serving requests with
//! `tlb::poll` between accesses). Every access goes through the CPU's own TLB (`probe::user_read` and friends
//! catch the faults), so a missed shootdown shows up as a stale read, a write that should fault, or a fault
//! with the present bit set where the page is gone. Writes are made only where a stale entry could not harm:
//! a page goes RW -> RO -> unmapped, so a stale entry left by a broken unmap is read-only.

use crate::linux::{MAP_ANONYMOUS, MAP_FIXED, MAP_PRIVATE, PROT_READ};
use crate::{AddressSpace, Fill, Prot, VmaKind};
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed, Ordering::SeqCst};
use hal::apic::{IpiDest, IpiKind};
use hal::paging::{self, probe, tlb, PageFlags};
use ktest::{kassert, ktest, TestResult};

const P: u64 = 4096;
const RW: u64 = 3;
const AP: u64 = MAP_ANONYMOUS | MAP_PRIVATE;
pub(crate) const FAULT: u64 = 1 << 63;
const REFUSED: u64 = 1 << 62;
pub(crate) const PF_P: u64 = 1;
pub(crate) const PF_W: u64 = 2;
const SEC: u64 = 1_000_000_000;

fn e<T: core::fmt::Debug>(x: T) -> String {
    format!("{x:?}")
}

fn lazy() -> Result<AddressSpace, String> {
    AddressSpace::with_fill(Fill::OnFault).map_err(|x| format!("address space: {x:?}"))
}

/// The online APs.
pub(crate) fn aps() -> Vec<usize> {
    (1..hal::percpu::cpu_count()).filter(|&c| hal::smp::is_online(c)).collect()
}

pub(crate) fn on(cpu: usize, f: fn(u64) -> u64, arg: u64) -> Result<u64, String> {
    hal::smp::call_on(cpu, f, arg, time::now_ns, SEC).map_err(|x| format!("call_on CPU {cpu}: {x:?}"))
}

fn val(i: u64) -> u64 {
    0x0123_4500_0000 | i
}

fn user_frames() -> u64 {
    mm_core::frames().map_or(0, |f| f.stats().used_by_kind[mm_core::Kind::User as usize])
}

// What the other CPUs run (hard-IRQ context: atomics and the probes only).

pub(crate) fn ap_activate(root: u64) -> u64 {
    paging::activate(root);
    paging::current_root()
}

pub(crate) fn ap_root(_: u64) -> u64 {
    paging::current_root()
}

fn ap_lazy(_: u64) -> u64 {
    tlb::activate_lazy_kernel();
    tlb::lazy_here() as u64
}

fn ap_is_lazy(_: u64) -> u64 {
    tlb::lazy_here() as u64
}

fn ap_read(va: u64) -> u64 {
    match probe::user_read(va) {
        Ok(Ok(v)) => v,
        Ok(Err(code)) => FAULT | code,
        Err(_) => REFUSED,
    }
}

pub(crate) static WRITE_VAL: AtomicU64 = AtomicU64::new(0);
/// The space `stale_user` probes through: since SC-3 an AP may run another thread between two calls and leave the
/// root it was given, so these probes load it again first (no-op while it is still loaded).
pub(crate) static PROBE_ROOT: AtomicU64 = AtomicU64::new(0);

pub(crate) fn ap_read_in(va: u64) -> u64 {
    paging::activate(PROBE_ROOT.load(SeqCst));
    ap_read(va)
}

/// Set in `ap_read_lazy`'s answer when the CPU held `PROBE_ROOT` lazily right before the read.
pub(crate) const LAZY_BIT: u64 = 1 << 61;

/// Whether this CPU holds `PROBE_ROOT` lazily, and the read after loading it again (same-root path: catches up),
/// in one call so no shootdown request comes in between.
pub(crate) fn ap_read_lazy(va: u64) -> u64 {
    let was = tlb::lazy_here() && paging::current_root() == PROBE_ROOT.load(SeqCst);
    ap_read_in(va) | if was { LAZY_BIT } else { 0 }
}

pub(crate) fn ap_write_in(va: u64) -> u64 {
    paging::activate(PROBE_ROOT.load(SeqCst));
    ap_write(va)
}

fn ap_write(va: u64) -> u64 {
    match probe::user_write(va, WRITE_VAL.load(SeqCst)) {
        Ok(Ok(())) => 0,
        Ok(Err(code)) => FAULT | code,
        Err(_) => REFUSED,
    }
}

fn ap_vmap_read(va: u64) -> u64 {
    match probe::vmap_read(va) {
        Ok(Ok(v)) => v & !(FAULT | REFUSED),
        Ok(Err(code)) => FAULT | code,
        Err(_) => REFUSED,
    }
}

fn ap_read_unmapped(va: u64) -> u64 {
    match probe::read_unmapped(va) {
        Ok(probe::Probe::Faulted { error, .. }) => FAULT | error,
        Ok(_) => 0,
        Err(probe::Refused::Mapped) => REFUSED | 1,
        Err(_) => REFUSED,
    }
}

fn gone(r: u64) -> bool {
    r & FAULT != 0 && r & PF_P == 0
}

// ---------------------------------------------------------------------------------------------------------

fn ready() -> TestResult {
    let s = tlb::stats();
    kassert!(s.ready, "the shootdown vector is not registered");
    kassert!(hal::vectors::register_system(tlb::VECTOR, |_| {}) == Err(hal::vectors::VectorError::Taken), "vector {:#x} not taken", tlb::VECTOR);
    let kroot = paging::home_root();
    let a = lazy()?;
    kassert!(tlb::has_context(a.root()) && !tlb::has_context(kroot), "user roots carry a context, the kernel root none");
    a.activate();
    let pcid = tlb::pcid_here();
    let mine = a.loaded_on();
    paging::activate(kroot);
    kassert!(mine == alloc::vec![hal::percpu::this_cpu()], "mask while loaded here: {mine:?}");
    kassert!(a.loaded_on().is_empty(), "mask after leaving: {:?}", a.loaded_on());
    kassert!(pcid == s.pcid, "PCID on this CPU {pcid}, CPU offers it {}", s.pcid);
    crate::tlb::stats_event(&tlb::stats());
    Ok(())
}
ktest!("mm.tlb.ready", Safe, ready);

/// Kernel-half leaves are global: the shootdown's `invlpg` must drop them in every PCID.
fn kernel_global() -> TestResult {
    let root = paging::kernel_root().ok_or("kernel still on the loader's tables")?;
    let (mut n, mut bad) = (0u64, Vec::new());
    paging::for_each_leaf(root, paging::KERNEL_HALF, 0xFFFF_FFFF_FFFF_F000, &mut |u| {
        n += 1;
        if !u.flags.contains(PageFlags::GLOBAL) && bad.len() < 4 {
            bad.push(u.virt);
        }
    });
    kassert!(n > 0 && bad.is_empty(), "{n} kernel leaves, not global: {bad:x?}");
    let sp = hal::switch::current_stack();
    if sp.bottom != 0 {
        let (_, fl) = paging::translate(root, sp.bottom).ok_or("own stack not mapped")?;
        kassert!(fl.contains(PageFlags::GLOBAL), "kernel stack page not global");
    }
    Ok(())
}
ktest!("mm.tlb.kernel_global", Safe, kernel_global);

/// munmap and mprotect reach every CPU that has the root loaded (APs idling with it: real IPIs); a lazy CPU is
/// not asked while no tables are freed and catches up when it loads the root again, and leaves the root when
/// tables are freed (MM-tlb3); the drop makes every CPU leave before anything is freed.
fn stale_user() -> TestResult {
    let aps = aps();
    if aps.is_empty() {
        return ktest::skip("one CPU");
    }
    // The kernel root itself: this thread may sit lazily on some program's root right now.
    let kroot = paging::home_root();
    let frames_before = user_frames();
    let leaked_before = crate::aspace::AS_LEAKED.get();
    let s0 = tlb::stats();
    let dropped;
    {
        let a = lazy()?;
        dropped = a.root();
        let base = a.sys_mmap(0, 4 * P, RW, AP, 0).map_err(e)?;
        for i in 0..4 {
            a.write_u64(base + i * P, val(i)).map_err(e)?;
        }
        let _s = probe::user_probes().map_err(e)?;
        PROBE_ROOT.store(a.root(), SeqCst);
        for &c in &aps {
            kassert!(on(c, ap_activate, a.root())? == a.root(), "CPU {c} did not load the root");
            for i in 0..4 {
                let r = on(c, ap_read_in, base + i * P)?;
                kassert!(r == val(i), "CPU {c} page {i}: {r:#x}");
            }
        }
        let mask = a.loaded_on();
        kassert!(aps.iter().all(|c| mask.contains(c)), "mask {mask:?}, APs {aps:?}");
        let irqs: Vec<u64> = aps.iter().map(|&c| tlb::cpu_irqs(c)).collect();
        // Page 1 read-only: a write faults on every CPU (present + write), reads still see the content.
        a.sys_mprotect(base + P, P, PROT_READ).map_err(e)?;
        for (k, &c) in aps.iter().enumerate() {
            kassert!(tlb::cpu_irqs(c) > irqs[k], "CPU {c} took no shootdown IPI");
            WRITE_VAL.store(val(1), SeqCst);
            let w = on(c, ap_write_in, base + P)?;
            kassert!(w & FAULT != 0 && w & (PF_P | PF_W) == PF_P | PF_W, "CPU {c} wrote a read-only page: {w:#x}");
            kassert!(on(c, ap_read_in, base + P)? == val(1), "CPU {c} lost page 1's content");
        }
        // Page 0: read-only first (a stale entry after the unmap could only be read-only), then unmapped: every
        // CPU's read and write fault with the present bit clear.
        a.sys_mprotect(base, P, PROT_READ).map_err(e)?;
        a.sys_munmap(base, P).map_err(e)?;
        for &c in &aps {
            let r = on(c, ap_read_in, base)?;
            kassert!(gone(r), "CPU {c} read {r:#x} after munmap");
            WRITE_VAL.store(val(0), SeqCst);
            let w = on(c, ap_write_in, base)?;
            kassert!(gone(w), "CPU {c} write after munmap: {w:#x}");
        }
        // Page 3 stays RW: the APs write it (same value) and the BSP sees it.
        for &c in &aps {
            WRITE_VAL.store(val(3), SeqCst);
            kassert!(on(c, ap_write_in, base + 3 * P)? == 0, "CPU {c} could not write page 3");
        }
        kassert!(a.read_u64(base + 3 * P).map_err(e)? == val(3));
        // Lazy TLB (MM-tlb3): each AP keeps the root for a "kernel thread". A change that frees no tables does
        // not ask them; an AP still lazy on the root afterwards took no request (one would have made it leave)
        // and must catch up when it comes back (`ap_read_in` loads the root again: same-root path).
        let far = 1u64 << 44;
        kassert!(a.sys_mmap(far, P, RW, AP | MAP_FIXED, 0).map_err(e)? == far, "fixed mapping refused");
        a.write_u64(far, val(7)).map_err(e)?;
        for &c in &aps {
            kassert!(on(c, ap_read_in, base + 2 * P)? == val(2), "CPU {c} page 2 before the lazy change");
            kassert!(on(c, ap_lazy, 0)? == 1, "CPU {c} did not keep the root lazily");
        }
        let s_skip = tlb::stats();
        a.sys_mprotect(base + 2 * P, P, PROT_READ).map_err(e)?;
        a.sys_munmap(base + 2 * P, P).map_err(e)?;
        // An AP still lazy on the root when it comes back was skipped by both changes (a request would have made
        // it leave; one that ran another thread meanwhile left through the switch, desk-INT-A1 flake): look and
        // read in one call, so no request can come in between.
        let mut kept = 0u64;
        for &c in &aps {
            let r = on(c, ap_read_lazy, base + 2 * P)?;
            kept += (r & LAZY_BIT != 0) as u64;
            kassert!(gone(r), "CPU {c} read {r:#x} after munmap");
            kassert!(on(c, ap_read_in, base + 3 * P)? == val(3), "CPU {c} page 3 after catching up");
            kassert!(on(c, ap_read_in, far)? == val(7), "CPU {c} far page");
        }
        let s_kept = tlb::stats();
        kassert!(s_kept.lazy_skips >= s_skip.lazy_skips + kept, "{kept} CPUs kept the root lazily, skips {} -> {}", s_skip.lazy_skips, s_kept.lazy_skips);
        kassert!(s_kept.lazy_catchups >= s_skip.lazy_catchups + kept, "{kept} lazy CPUs, catch-ups {} -> {}", s_skip.lazy_catchups, s_kept.lazy_catchups);
        klog::event!(Info, "mm", "ktest.tlb_stale_user_lazy", aps = aps.len(), kept = kept, skips = s_kept.lazy_skips - s_skip.lazy_skips,
            catchups = s_kept.lazy_catchups - s_skip.lazy_catchups, ipis = s_kept.ipis - s_skip.ipis, lazy_leaves = s_kept.lazy_leaves - s_skip.lazy_leaves);
        // Freed tables: the far page has a page table (and more) of its own; its unmap asks the lazy CPUs too, and
        // they leave the root (Linux shoots lazy CPUs when tables are freed).
        for &c in &aps {
            kassert!(on(c, ap_lazy, 0)? == 1, "CPU {c} did not keep the root lazily");
        }
        let still: u64 = aps.iter().map(|&c| on(c, ap_is_lazy, 0).unwrap_or(0)).sum();
        let lazy0 = tlb::stats().lazy_leaves;
        let tables0 = paging::count_user_tables(a.root());
        a.sys_munmap(far, P).map_err(e)?;
        let tables1 = paging::count_user_tables(a.root());
        kassert!(tables1 < tables0, "the far unmap freed no tables ({tables0} -> {tables1})");
        let left = tlb::stats().lazy_leaves - lazy0;
        kassert!(still == 0 || left >= 1, "{still} lazy CPUs, none left the root lazily ({lazy0} -> {})", tlb::stats().lazy_leaves);
        kassert!(a.loaded_on().is_empty(), "still in the mask after leaving: {:?}", a.loaded_on());
        for &c in &aps {
            // INT-A2a2: the invariant is "not on a's root any more". The CPU may have run another program's
            // thread meanwhile and sit on that root (gate flake "CPU 1 not on the kernel root after leaving").
            let r = on(c, ap_root, 0)?;
            kassert!(r != a.root() && (r == kroot || tlb::has_context(r)), "CPU {c} on {r:#x} after leaving (a {:#x}, kernel {kroot:#x})", a.root());
            kassert!(on(c, ap_activate, a.root())? == a.root());
            kassert!(gone(on(c, ap_read, base + 2 * P)?), "CPU {c} reads page 2 after munmap");
            kassert!(on(c, ap_read, base + 3 * P)? == val(3), "CPU {c} page 3 after reload");
        }
        // The drop: every AP still has the root loaded and must leave it before the tables go.
    }
    for &c in &aps {
        let r = on(c, ap_root, 0)?;
        kassert!(r != dropped && (r == kroot || tlb::has_context(r)), "CPU {c} kept a dropped root loaded ({r:#x}, dropped {dropped:#x})");
    }
    let s1 = tlb::stats();
    kassert!(crate::aspace::AS_LEAKED.get() == leaked_before, "the address space leaked");
    kassert!(user_frames() == frames_before, "{} bytes of user frames not back", user_frames().saturating_sub(frames_before));
    kassert!(s1.give_ups == s0.give_ups, "a shootdown gave up");
    kassert!(s1.dying_leaves >= s0.dying_leaves + aps.len() as u64, "dying leaves {} -> {}", s0.dying_leaves, s1.dying_leaves);
    klog::event!(Info, "mm", "ktest.tlb_stale_user", aps = aps.len(), user = s1.user - s0.user, ipis = s1.ipis - s0.ipis,
        irqs = s1.irqs - s0.irqs, page_flushes = s1.page_flushes - s0.page_flushes, full_flushes = s1.full_flushes - s0.full_flushes);
    Ok(())
}
ktest!("mm.tlb.stale_user", Safe, stale_user);

/// A kernel page every AP has read is gone on all of them after `unmap_4k` (the KernelStack path), and the
/// range goes back (no quarantine).
fn stale_kernel() -> TestResult {
    let aps = aps();
    if aps.is_empty() {
        return ktest::skip("one CPU");
    }
    let root = paging::kernel_root().ok_or("kernel still on the loader's tables")?;
    let fa = mm_core::frames().ok_or("no frame allocator")?;
    let base = paging::reserve_kernel_va(P).ok_or("no reservation")?;
    let f = mm_core::alloc_zeroed(mm_core::Kind::Other).ok_or("no frame")?;
    paging::map_4k(root, base, f, PageFlags::kernel_data(), &mut mm_core::alloc_table_frame).map_err(e)?;
    let s0 = tlb::stats();
    {
        let _s = probe::user_probes().map_err(e)?;
        for &c in &aps {
            let r = on(c, ap_vmap_read, base)?;
            kassert!(r == 0, "CPU {c} read {r:#x} from a zeroed page");
        }
    }
    let back = paging::unmap_4k(root, base);
    kassert!(back == Some(f), "unmap_4k gave {back:?}");
    fa.free(f, 0, mm_core::Kind::Other);
    {
        let _s = probe::user_probes().map_err(e)?;
        for &c in &aps {
            let r = on(c, ap_vmap_read, base)?;
            kassert!(gone(r), "CPU {c} still reads the unmapped kernel page: {r:#x}");
        }
    }
    kassert!(paging::release_kernel_va(base, P), "release refused");
    // The batched form (`unmap_kernel_pages`, for kernel stacks): three pages, one shootdown.
    let base = paging::reserve_kernel_va(3 * P).ok_or("no reservation")?;
    let mut fs = Vec::new();
    for i in 0..3 {
        let f = mm_core::alloc_zeroed(mm_core::Kind::Other).ok_or("no frame")?;
        paging::map_4k(root, base + i * P, f, PageFlags::kernel_data(), &mut mm_core::alloc_table_frame).map_err(e)?;
        fs.push(f);
    }
    {
        let _s = probe::user_probes().map_err(e)?;
        for &c in &aps {
            for i in 0..3 {
                kassert!(on(c, ap_vmap_read, base + i * P)? == 0, "CPU {c} page {i}");
            }
        }
    }
    let k0 = tlb::stats().kernel;
    let mut back = Vec::new();
    let n = paging::unmap_kernel_pages(root, base, 3 * P, &mut |f| back.push(f));
    kassert!(n == 3 && back == fs, "unmapped {n}, frames {back:x?}");
    kassert!(tlb::stats().kernel == k0 + 1, "{} shootdowns for three pages", tlb::stats().kernel - k0);
    for f in back {
        fa.free(f, 0, mm_core::Kind::Other);
    }
    {
        let _s = probe::user_probes().map_err(e)?;
        for &c in &aps {
            for i in 0..3 {
                kassert!(gone(on(c, ap_vmap_read, base + i * P)?), "CPU {c} still reads page {i}");
            }
        }
    }
    kassert!(paging::release_kernel_va(base, 3 * P), "release refused");
    let s1 = tlb::stats();
    kassert!(s1.kernel > s0.kernel && s1.give_ups == s0.give_ups);
    Ok(())
}
ktest!("mm.tlb.stale_kernel", Safe, stale_kernel);

/// SC-1's kernel stacks: a finished thread's stack pages are unmapped on every CPU before their frames go back.
fn stacks() -> TestResult {
    let aps = aps();
    if aps.is_empty() {
        return ktest::skip("one CPU");
    }
    let s0 = tlb::stats();
    let seen = Arc::new(ksync::SpinLock::new(Vec::new()));
    let mut hs = Vec::new();
    for k in 0..6 {
        let seen = seen.clone();
        hs.push(
            sched::spawn("mm.tlb.stack", sched::Class::Normal, move || {
                let b = hal::switch::current_stack();
                seen.lock().push((k, b.bottom));
            })
            .map_err(e)?,
        );
    }
    for h in &hs {
        kassert!(h.join_timeout(5 * SEC), "a thread did not finish");
    }
    // The last stack is freed by the switch after its thread's end: give the scheduler a few rounds. Since SC-3
    // finished threads' stacks are unmapped by the reaper; `sched::reap` runs that pass now.
    for _ in 0..8 {
        sched::yield_now();
    }
    let end = time::now_ns() + 5 * SEC;
    while !sched::reap() && time::now_ns() < end {
        sched::sleep_ns(1_000_000);
    }
    let s1 = tlb::stats();
    kassert!(s1.kernel >= s0.kernel + 6, "kernel shootdowns {} -> {}", s0.kernel, s1.kernel);
    let mut checked = 0;
    let seen: Vec<(i32, u64)> = seen.lock().clone();
    for (_, bottom) in seen {
        if bottom == 0 {
            continue;
        }
        for &c in &aps {
            let r = on(c, ap_read_unmapped, bottom)?;
            if r == REFUSED | 1 {
                continue; // the slot holds a newer thread's stack now
            }
            kassert!(gone(r), "CPU {c} reads a freed stack page {bottom:#x}: {r:#x}");
            checked += 1;
        }
    }
    kassert!(s1.give_ups == s0.give_ups && s1.kept_pages == s0.kept_pages, "stack pages kept out of use");
    klog::event!(Info, "mm", "ktest.tlb_stacks", kernel_shootdowns = s1.kernel - s0.kernel, probes = checked);
    Ok(())
}
ktest!("mm.tlb.stacks", Safe, stacks);

/// PCID: contexts keep their own entries across switches (same address, different content), a slot that
/// went stale while the context was not loaded is flushed when it comes back, and more contexts than slots
/// evict without mixing anything up.
fn pcid() -> TestResult {
    let s = tlb::stats();
    if !s.pcid {
        // CPUID leaf 1 ECX bit 17 is clear (`tlb.ready pcid=false`): hidden by the hypervisor (`-cpu ...,-pcid`) or
        // the CPU has none. Zen 4 (the Mini-PC) has it.
        return ktest::skip("CPUID shows no PCID (leaf 1 ECX bit 17): every switch flushes");
    }
    if s.pcid_cpus == 0 && s.pcid_no_pge + s.pcid_cr3_bits > 0 {
        return Err(format!("CPUID offers PCID, but no CPU turned it on: {} without CR4.PGE, {} with CR3 low bits set", s.pcid_no_pge, s.pcid_cr3_bits));
    }
    const VA: u64 = 0x5000_0000;
    let kroot = paging::home_root();
    let r = (|| -> TestResult {
        let mut v = Vec::new();
        for i in 0..(tlb::PCID_SLOTS as u64 + 3) {
            let a = lazy()?;
            a.map_fixed(VA, P, Prot::rw(), VmaKind::Anonymous).map_err(e)?;
            a.write_u64(VA, val(i)).map_err(e)?;
            v.push(a);
        }
        let s0 = tlb::stats();
        let _s = probe::user_probes().map_err(e)?;
        for round in 0..3 {
            for (i, a) in v.iter().enumerate() {
                a.activate();
                let r = probe::user_read(VA).map_err(e)?;
                kassert!(r == Ok(val(i as u64)), "round {round} context {i}: {r:x?}");
            }
        }
        // Two contexts alternating: the PCID entries are kept.
        let kept0 = tlb::stats().pcid_kept;
        for _ in 0..4 {
            v[0].activate();
            kassert!(probe::user_read(VA).map_err(e)? == Ok(val(0)));
            v[1].activate();
            kassert!(probe::user_read(VA).map_err(e)? == Ok(val(1)));
        }
        kassert!(tlb::stats().pcid_kept > kept0, "no switch kept its PCID");
        // v[0] changes while v[1] is loaded here: v[0]'s slot is stale and must be flushed on the way back.
        v[0].activate();
        kassert!(probe::user_read(VA).map_err(e)? == Ok(val(0)));
        v[1].activate();
        v[0].sys_mprotect(VA, P, PROT_READ).map_err(e)?;
        v[0].sys_munmap(VA, P).map_err(e)?;
        v[0].activate();
        let r = probe::user_read(VA).map_err(e)?;
        kassert!(matches!(r, Err(code) if code & PF_P == 0), "stale PCID entry after munmap: {r:x?}");
        let s1 = tlb::stats();
        kassert!(s1.pcid_evictions > s0.pcid_evictions, "{} contexts for {} slots and no eviction", v.len(), tlb::PCID_SLOTS);
        kassert!(s1.pcid_cpus >= 1);
        Ok(())
    })();
    paging::activate(kroot);
    r
}
ktest!("mm.tlb.pcid", Safe, pcid);

// ---------------------------------------------------------------------------------------------------------
// A CPU that does not answer: resend, and give up.

static BUSY_NS: AtomicU64 = AtomicU64::new(0);
/// The root `busy_irq` holds (not lazily) while it spins.
static BUSY_ROOT: AtomicU64 = AtomicU64::new(0);
static BUSY_STARTED: AtomicBool = AtomicBool::new(false);
static BUSY_DONE: AtomicBool = AtomicBool::new(false);

/// Spins with interrupts off and without serving requests (a CPU stuck in a long critical section).
fn busy_irq(_v: u8) {
    // MM-tlb3: the CPU may have switched to a kernel thread since it loaded the root and hold it only lazily,
    // which a flush no longer waits for: load it again (no-op while loaded), so the round must wait for it.
    let root = BUSY_ROOT.load(SeqCst);
    if root != 0 {
        paging::activate(root);
    }
    BUSY_STARTED.store(true, SeqCst);
    let until = time::now_ns().saturating_add(BUSY_NS.load(SeqCst));
    while time::now_ns() < until {
        core::hint::spin_loop();
    }
    BUSY_DONE.store(true, SeqCst);
}

fn wait_flag(f: &AtomicBool, what: &str) -> TestResult {
    let until = time::now_ns() + 5 * SEC;
    while !f.load(SeqCst) {
        if time::now_ns() > until {
            return Err(format!("{what}: timed out"));
        }
        sched::yield_now();
    }
    Ok(())
}

/// Starts `busy_irq` on `cpu` for `ns` with `root` loaded and waits until it runs.
fn make_busy(cpu: usize, ns: u64, root: u64) -> Result<hal::vectors::Vector, String> {
    let v = hal::vectors::alloc(busy_irq, Some(cpu as u32)).map_err(e)?;
    BUSY_NS.store(ns, SeqCst);
    BUSY_ROOT.store(root, SeqCst);
    BUSY_STARTED.store(false, SeqCst);
    BUSY_DONE.store(false, SeqCst);
    let Some(apic) = hal::smp::apic_id(cpu) else {
        hal::vectors::free(v);
        return Err(String::from("AP not online"));
    };
    if let Err(x) = hal::apic::send_ipi(IpiDest::Physical(apic), IpiKind::Fixed(v.vector)) {
        hal::vectors::free(v);
        return Err(e(x));
    }
    if let Err(x) = wait_flag(&BUSY_STARTED, "busy handler start") {
        hal::vectors::free(v);
        return Err(x);
    }
    Ok(v)
}

fn resend() -> TestResult {
    let Some(&c) = aps().first() else { return ktest::skip("one CPU") };
    let a = lazy()?;
    let base = a.sys_mmap(0, P, RW, AP, 0).map_err(e)?;
    a.write_u64(base, val(7)).map_err(e)?;
    kassert!(on(c, ap_activate, a.root())? == a.root());
    let s0 = tlb::stats();
    let v = make_busy(c, 5_000_000, a.root())?;
    let here = hal::percpu::this_cpu();
    let t0 = time::now_ns();
    let m = a.sys_mprotect(base, P, PROT_READ);
    let waited = time::now_ns() - t0;
    let on_busy = here == c || hal::percpu::this_cpu() == c;
    let r = wait_flag(&BUSY_DONE, "busy handler end");
    hal::vectors::free(v);
    r?;
    m.map_err(e)?;
    if on_busy {
        // This thread ran on the busy CPU itself (it may migrate since SC-3): the change came after the busy end.
        return ktest::skip("the test thread ran on the busy CPU");
    }
    let s1 = tlb::stats();
    kassert!(s1.resends > s0.resends, "no resend while CPU {c} was busy ({waited} ns)");
    kassert!(s1.give_ups == s0.give_ups, "gave up on a CPU busy for 5 ms");
    kassert!(waited >= tlb::RESEND_NS, "mprotect returned after {waited} ns while CPU {c} could not answer");
    {
        let _s = probe::user_probes().map_err(e)?;
        WRITE_VAL.store(val(7), SeqCst);
        let w = on(c, ap_write, base)?;
        kassert!(w & FAULT != 0 && w & PF_W != 0, "CPU {c} writes the read-only page: {w:#x}");
    }
    drop(a);
    kassert!(on(c, ap_root, 0)? == paging::home_root());
    klog::event!(Info, "mm", "ktest.tlb_resend", waited_us = waited / 1000, resends = s1.resends - s0.resends);
    Ok(())
}
ktest!("mm.tlb.resend", Safe, resend);

/// A CPU stuck longer than `GIVE_UP_NS`: the unmap returns, its frames stay out of use until the drop, and
/// the stuck CPU is back in service once it answers.
fn give_up() -> TestResult {
    let Some(&c) = aps().first() else { return ktest::skip("one CPU") };
    let frames_before = user_frames();
    let unconfirmed0 = crate::aspace::UNCONFIRMED.get();
    let s0 = tlb::stats();
    {
        let a = lazy()?;
        let base = a.sys_mmap(0, 4 * P, RW, AP, 0).map_err(e)?;
        for i in 0..4 {
            a.write_u64(base + i * P, val(i)).map_err(e)?;
        }
        kassert!(on(c, ap_activate, a.root())? == a.root());
        let v = make_busy(c, tlb::GIVE_UP_NS + 300_000_000, a.root())?;
        let here = hal::percpu::this_cpu();
        let m = a.sys_munmap(base, 2 * P);
        let s1 = tlb::stats();
        let on_busy = here == c || hal::percpu::this_cpu() == c;
        let r = wait_flag(&BUSY_DONE, "busy handler end");
        hal::vectors::free(v);
        r?;
        m.map_err(e)?;
        if on_busy {
            // This thread ran on the busy CPU itself (it may migrate since SC-3): the unmap came after the busy end.
            return ktest::skip("the test thread ran on the busy CPU");
        }
        // At least this munmap's give-up: a kernel shootdown of the same 800 ms (a thread's stack reaped meanwhile)
        // waits for the busy CPU too and may give up as well.
        kassert!(s1.give_ups > s0.give_ups && s1.unanswered > s0.unanswered, "no give-up: {s0:?} -> {s1:?}");
        kassert!(crate::aspace::UNCONFIRMED.get() == unconfirmed0 + 1, "the unmap did not report the missing answer");
        kassert!(s1.stuck_cpus >= 1, "CPU {c} not marked stuck");
        kassert!(user_frames() > frames_before, "the unconfirmed frames went back");
        // The CPU answers again (it served the pending IPI): no longer stuck, and the next change is confirmed.
        a.sys_munmap(base + 2 * P, P).map_err(e)?;
        let s2 = tlb::stats();
        kassert!(crate::aspace::UNCONFIRMED.get() == unconfirmed0 + 1 && s2.stuck_cpus == 0, "CPU {c} still stuck: {s2:?}");
    }
    kassert!(user_frames() == frames_before, "frames kept after the drop: {} bytes", user_frames().saturating_sub(frames_before));
    Ok(())
}
ktest!("mm.tlb.give_up", QemuOnly, give_up);

// ---------------------------------------------------------------------------------------------------------
// The storm: kernel threads on the BSP cycle the pages of one address space through RW -> RO -> unmapped ->
// RW (new frame, new value) while the APs read and write them through their own TLBs. Each page's state is
// published (seq, kind) only after the call returned, and "changing" before it starts; an access counts only
// when the state was the same before and after it, and then it must match that state exactly. The APs write
// only the page's second word, so a write delayed into a later incarnation of the page changes nothing checked.

const STORM_PAGES: usize = 64;
const K_TRANS: u64 = 0;
const K_RW: u64 = 1;
const K_RO: u64 = 2;
const K_GONE: u64 = 3;

static PAGES: [AtomicU64; STORM_PAGES] = [const { AtomicU64::new(0) }; STORM_PAGES];
static S_ROOT: AtomicU64 = AtomicU64::new(0);
static S_KROOT: AtomicU64 = AtomicU64::new(0);
static S_BASE: AtomicU64 = AtomicU64::new(0);
static S_STOP: AtomicBool = AtomicBool::new(false);
static S_DEADLINE: AtomicU64 = AtomicU64::new(0);
static S_SAMPLES: AtomicU64 = AtomicU64::new(0);
static S_SKIPPED: AtomicU64 = AtomicU64::new(0);
static S_STALE: AtomicU64 = AtomicU64::new(0);
static S_FIRST: AtomicU64 = AtomicU64::new(0);
static S_DONE: AtomicU64 = AtomicU64::new(0);
/// The first stale access: what was read, what was expected, the page state, the frame the tables name now.
static S_BAD: (AtomicU64, AtomicU64, AtomicU64, AtomicU64) = (AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0));

fn state(seq: u64, kind: u64) -> u64 {
    seq << 2 | kind
}

fn sval(page: u64, seq: u64) -> u64 {
    0x0057_0000_0000 | page << 24 | (seq & 0xFF_FFFF)
}

fn storm_irq(_v: u8) {
    let me = hal::percpu::this_cpu() as u64;
    paging::activate(S_ROOT.load(SeqCst));
    let base = S_BASE.load(SeqCst);
    let deadline = S_DEADLINE.load(SeqCst);
    let mut x = 0x9E37_79B9_7F4A_7C15u64 ^ (me << 32 | me);
    let (mut samples, mut skipped) = (0u64, 0u64);
    let mut n = 0u64;
    while !S_STOP.load(Relaxed) && time::now_ns() < deadline {
        // Requests are served only every 64 accesses: the translations a change made stale stay in this TLB
        // for a while, so an initiator that did not wait for this CPU would be caught (checked by hand with the
        // wait removed: thousands of stale accesses per run).
        if n.is_multiple_of(64) {
            tlb::poll();
        }
        n += 1;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        let i = (x % STORM_PAGES as u64) as usize;
        let va = base + i as u64 * P;
        let s1 = PAGES[i].load(SeqCst);
        let (seq, kind) = (s1 >> 2, s1 & 3);
        if kind == K_TRANS {
            continue;
        }
        let write = (x >> 40) & 1 == 1;
        // An RO page keeps the content it had as RW, published two steps earlier.
        let want = sval(i as u64, if kind == K_RO { seq - 2 } else { seq });
        // Writes go to the page's second word: an AP delayed across several transitions may land its write
        // on a later incarnation of the page, which must not change the value the others check.
        let r: Result<u64, u64> = if write {
            match probe::user_write(va + 8, me) {
                Ok(Ok(())) => Ok(want),
                Ok(Err(code)) => Err(code),
                Err(_) => Err(REFUSED),
            }
        } else {
            probe::user_read(va).unwrap_or(Err(REFUSED))
        };
        core::sync::atomic::fence(SeqCst);
        if PAGES[i].load(SeqCst) != s1 {
            skipped += 1;
            continue;
        }
        samples += 1;
        let ok = match (kind, write, r) {
            (K_RW, _, Ok(v)) => v == want,
            (K_RO, false, Ok(v)) => v == want,
            (K_RO, true, Err(code)) => code & (PF_P | PF_W) == PF_P | PF_W,
            (K_GONE, _, Err(code)) => code & PF_P == 0 && code != REFUSED,
            _ => false,
        };
        if !ok {
            if S_STALE.fetch_add(1, SeqCst) == 0 {
                S_BAD.0.store(match r { Ok(v) => v, Err(c) => FAULT | c }, SeqCst);
                S_BAD.1.store(want, SeqCst);
                S_BAD.2.store(s1, SeqCst);
                S_BAD.3.store(paging::translate(S_ROOT.load(SeqCst), va).map_or(u64::MAX, |t| t.0), SeqCst);
            }
            let code = match r {
                Ok(v) => v & 0xFFFF,
                Err(c) => 0x8000 | (c & 0x7FFF),
            };
            let _ = S_FIRST.compare_exchange(0, 1 << 63 | me << 40 | (i as u64) << 24 | kind << 20 | (write as u64) << 16 | code, SeqCst, SeqCst);
        }
    }
    S_SAMPLES.fetch_add(samples, SeqCst);
    S_SKIPPED.fetch_add(skipped, SeqCst);
    paging::activate(S_KROOT.load(SeqCst));
    S_DONE.fetch_add(1, SeqCst);
}

/// One BSP thread: every `step`-th page from `first`, round and round, until `until`.
fn storm_thread(a: &AddressSpace, first: usize, step: usize, until: u64) -> Result<u64, String> {
    let base = S_BASE.load(SeqCst);
    let mut ops = 0u64;
    let mut i = first;
    while time::now_ns() < until {
        let va = base + i as u64 * P;
        let s = PAGES[i].load(SeqCst);
        let (seq, kind) = (s >> 2, s & 3);
        PAGES[i].store(state(seq + 1, K_TRANS), SeqCst);
        let next = match kind {
            K_RW => a.sys_mprotect(va, P, PROT_READ).map(|_| K_RO).map_err(e),
            K_RO => a.sys_munmap(va, P).map(|_| K_GONE).map_err(e),
            _ => a
                .sys_mmap(va, P, RW, AP | MAP_FIXED, 0)
                .map_err(e)
                .and_then(|_| a.write_u64(va, sval(i as u64, seq + 2)).map_err(e))
                .map(|_| K_RW),
        };
        let k = next.map_err(|x| format!("page {i} from kind {kind}: {x}"))?;
        PAGES[i].store(state(seq + 2, k), SeqCst);
        ops += 1;
        i = (i + step) % STORM_PAGES;
        if ops.is_multiple_of(4) {
            sched::yield_now();
        }
    }
    Ok(ops)
}

fn storm() -> TestResult {
    let aps = aps();
    if aps.is_empty() {
        return ktest::skip("one CPU");
    }
    const THREADS: usize = 4;
    const RUN_NS: u64 = 1_000_000_000;
    let frames_before = user_frames();
    let s0 = tlb::stats();
    let a = Arc::new(lazy()?);
    let base = a.sys_mmap(0, STORM_PAGES as u64 * P, RW, AP, 0).map_err(e)?;
    for (i, p) in PAGES.iter().enumerate() {
        a.write_u64(base + i as u64 * P, sval(i as u64, 1)).map_err(e)?;
        p.store(state(1, K_RW), SeqCst);
    }
    S_ROOT.store(a.root(), SeqCst);
    S_KROOT.store(paging::home_root(), SeqCst);
    S_BASE.store(base, SeqCst);
    S_STOP.store(false, SeqCst);
    for c in [&S_SAMPLES, &S_SKIPPED, &S_STALE, &S_FIRST, &S_DONE, &S_BAD.0, &S_BAD.1, &S_BAD.2, &S_BAD.3] {
        c.store(0, SeqCst);
    }
    // Half the APs (at least one) run the access loop with interrupts off; the others load the root and idle,
    // so they get the shootdowns as real IPIs.
    let workers: Vec<usize> = aps.iter().copied().take(aps.len().div_ceil(2)).collect();
    for &c in &aps[workers.len()..] {
        on(c, ap_activate, a.root())?;
    }
    let session = probe::user_probes().map_err(e)?;
    let t0 = time::now_ns();
    S_DEADLINE.store(t0 + RUN_NS + 2 * SEC, SeqCst);
    let mut vecs = Vec::new();
    let mut started = Ok(());
    for &c in &workers {
        let r = hal::vectors::alloc(storm_irq, Some(c as u32)).map_err(e).and_then(|v| {
            let Some(apic) = hal::smp::apic_id(c) else {
                hal::vectors::free(v);
                return Err(String::from("AP not online"));
            };
            if let Err(x) = hal::apic::send_ipi(IpiDest::Physical(apic), IpiKind::Fixed(v.vector)) {
                hal::vectors::free(v);
                return Err(e(x));
            }
            Ok(v)
        });
        match r {
            Ok(v) => vecs.push(v),
            Err(x) => {
                started = Err(x);
                break;
            }
        }
    }
    let ops = Arc::new(AtomicU64::new(0));
    let errs = Arc::new(ksync::SpinLock::new(Vec::<String>::new()));
    let mut hs = Vec::new();
    if started.is_ok() {
        for k in 0..THREADS {
            let (a, ops, errs) = (a.clone(), ops.clone(), errs.clone());
            let until = t0 + RUN_NS;
            match sched::spawn("mm.tlb.storm", sched::Class::Normal, move || match storm_thread(&a, k, THREADS, until) {
                Ok(n) => {
                    ops.fetch_add(n, SeqCst);
                }
                Err(x) => errs.lock().push(x),
            }) {
                Ok(h) => hs.push(h),
                Err(x) => {
                    started = Err(e(x));
                    break;
                }
            }
        }
    }
    for h in &hs {
        if !h.join_timeout(30 * SEC) {
            errs.lock().push(String::from("a storm thread did not finish"));
        }
    }
    S_STOP.store(true, SeqCst);
    let until = time::now_ns() + 5 * SEC;
    while S_DONE.load(SeqCst) < vecs.len() as u64 && time::now_ns() < until {
        sched::yield_now();
    }
    let done = S_DONE.load(SeqCst);
    let leaked0 = probe::leaked_sessions();
    drop(session);
    kassert!(probe::leaked_sessions() == leaked0, "the probe session could not be closed: a probe stayed in flight");
    for v in vecs {
        hal::vectors::free(v);
    }
    started?;
    kassert!(done == workers.len() as u64, "{done} of {} AP workers finished", workers.len());
    let first_err = errs.lock().first().cloned();
    if let Some(x) = first_err {
        return Err(x);
    }
    let (samples, skipped, stale, first) = (S_SAMPLES.load(SeqCst), S_SKIPPED.load(SeqCst), S_STALE.load(SeqCst), S_FIRST.load(SeqCst));
    let ops = ops.load(SeqCst);
    let s1 = tlb::stats();
    drop(a);
    klog::event!(Info, "mm", "ktest.tlb_storm", cpus = aps.len() + 1, workers = workers.len(), threads = THREADS, ops = ops, samples = samples,
        skipped = skipped, stale = stale, shootdowns = s1.user - s0.user, ipis = s1.ipis - s0.ipis, irqs = s1.irqs - s0.irqs,
        resends = s1.resends - s0.resends, page_flushes = s1.page_flushes - s0.page_flushes, full_flushes = s1.full_flushes - s0.full_flushes);
    kassert!(stale == 0, "{stale} stale accesses; first: CPU {} page {} kind {} write {} result {:#x}; read {:#x} want {:#x} state {:#x} frame now {:#x}",
        (first >> 40) & 0x7F_FFFF, (first >> 24) & 0xFFFF, (first >> 20) & 0xF, (first >> 16) & 1, first & 0xFFFF,
        S_BAD.0.load(SeqCst), S_BAD.1.load(SeqCst), S_BAD.2.load(SeqCst), S_BAD.3.load(SeqCst));
    kassert!(ops >= 30 && samples >= 1000, "too little happened: {ops} ops, {samples} samples");
    kassert!(s1.give_ups == s0.give_ups, "a shootdown gave up during the storm");
    kassert!(user_frames() == frames_before, "{} bytes of user frames not back", user_frames().saturating_sub(frames_before));
    Ok(())
}
ktest!("mm.tlb.storm", Safe, storm);

// ---------------------------------------------------------------------------------------------------------

static PC_COUNT: AtomicU64 = AtomicU64::new(0);
/// 0 running, 1 ended on a refused probe (session closed), 2 ended at the deadline.
static PC_END: AtomicU64 = AtomicU64::new(0);
static PC_DEADLINE: AtomicU64 = AtomicU64::new(0);
/// A user address the kernel root does not map: every probe faults and is resumed by the session's hook.
const PC_VA: u64 = 0x10_0000_0000;

fn probe_close_irq(_v: u8) {
    let deadline = PC_DEADLINE.load(SeqCst);
    loop {
        if time::now_ns() >= deadline {
            PC_END.store(2, SeqCst);
            return;
        }
        match probe::user_read(PC_VA) {
            Ok(_) => {
                PC_COUNT.fetch_add(1, SeqCst);
            }
            Err(_) => {
                PC_END.store(1, SeqCst);
                return;
            }
        }
        tlb::poll();
    }
}

/// Closing a probe session while an AP keeps probing (faulting each time) neither leaves a fault unresumed (a
/// kernel #PF on the AP) nor gives up: the close waits for the probe in flight, the next one is refused.
fn probe_close() -> TestResult {
    let aps = aps();
    let Some(&cpu) = aps.first() else { return ktest::skip("one CPU") };
    for round in 0..20u64 {
        on(cpu, ap_activate, paging::home_root())?;
        PC_COUNT.store(0, SeqCst);
        PC_END.store(0, SeqCst);
        PC_DEADLINE.store(time::now_ns() + 5 * SEC, SeqCst);
        let leaked0 = probe::leaked_sessions();
        let session = probe::user_probes().map_err(e)?;
        let v = hal::vectors::alloc(probe_close_irq, Some(cpu as u32)).map_err(e)?;
        let sent = hal::smp::apic_id(cpu)
            .ok_or_else(|| String::from("AP not online"))
            .and_then(|apic| hal::apic::send_ipi(IpiDest::Physical(apic), IpiKind::Fixed(v.vector)).map_err(e));
        if let Err(x) = sent {
            drop(session);
            hal::vectors::free(v);
            return Err(x);
        }
        // Close at a different point of the AP's loop each round.
        let want = 100 + round * 37;
        let until = time::now_ns() + 2 * SEC;
        while PC_COUNT.load(SeqCst) < want && PC_END.load(SeqCst) == 0 && time::now_ns() < until {
            core::hint::spin_loop();
        }
        drop(session);
        let until = time::now_ns() + 6 * SEC;
        while PC_END.load(SeqCst) == 0 && time::now_ns() < until {
            core::hint::spin_loop();
        }
        let end = PC_END.load(SeqCst);
        hal::vectors::free(v);
        kassert!(probe::leaked_sessions() == leaked0, "round {round}: the close gave up waiting");
        kassert!(end == 1, "round {round}: the AP's loop ended with {end} (1: refused after the close) after {} probes", PC_COUNT.load(SeqCst));
        kassert!(PC_COUNT.load(SeqCst) >= want.min(1), "round {round}: no probe ran before the close");
        let r = on(cpu, ap_root, 0)?;
        kassert!(r == paging::home_root(), "round {round}: the AP does not answer normally after the close");
    }
    Ok(())
}
ktest!("mm.tlb.probe_close", Safe, probe_close);

// The test frames are ours (just allocated); DmaRegion has no constructor for a plain frame.
#[allow(deprecated)]
fn raw_write(f: u64, off: usize, d: &[u8]) -> bool {
    hal::dma_buf::write(f, 8192, off, d)
}
#[allow(deprecated)]
fn raw_read(f: u64, off: usize, d: &mut [u8]) -> bool {
    hal::dma_buf::read(f, 8192, off, d)
}

/// The safe unmap and retire calls refuse what is not theirs: kernel-half addresses outside vmap (the kernel
/// image, the direct map) stay mapped, and a frame that is no user root is not written.
fn refuse_foreign() -> TestResult {
    let root = paging::current_root();
    let text = refuse_foreign as *const () as usize as u64 & !(P - 1);
    let before = paging::translate(root, text);
    kassert!(before.is_some(), "kernel text not mapped?");
    kassert!(paging::unmap_4k(root, text).is_none(), "unmap_4k took a kernel-image page");
    let mut got = 0u64;
    kassert!(paging::unmap_kernel_pages(root, text, P, &mut |_| got += 1) == 0 && got == 0, "unmap_kernel_pages took a kernel-image page");
    kassert!(paging::translate(root, text) == before, "kernel text mapping changed");
    // 8 KiB aligned like a user root, so only the missing context magic tells it apart.
    let f = mm_core::alloc_zeroed_block(1, mm_core::Zone::Normal, mm_core::Kind::Other).ok_or("no frames")?;
    let va = hal::phys_to_virt(f);
    let dm = paging::translate(root, va);
    kassert!(paging::unmap_4k(root, va).is_none(), "unmap_4k took a direct-map page");
    kassert!(paging::unmap_kernel_pages(root, va, P, &mut |_| got += 1) == 0 && got == 0, "unmap_kernel_pages took a direct-map page");
    kassert!(paging::translate(root, va) == dm, "direct-map mapping changed");
    // A frame of ours whose entry 256 has the mark bit set but no context: a bogus retire must leave it alone.
    let marker = 1u64 << 52 | 0x5A5A;
    let mut b = [0u8; 8];
    let ok = raw_write(f, 2048, &marker.to_le_bytes()) && {
        paging::retire_user_root(f);
        raw_read(f, 2048, &mut b)
    };
    if let Some(fa) = mm_core::frames() {
        fa.free(f, 1, mm_core::Kind::Other);
    }
    kassert!(ok, "raw access to the test frame refused");
    kassert!(u64::from_le_bytes(b) == marker, "retire_user_root wrote into a frame that is no user root");
    Ok(())
}
ktest!("mm.tlb.refuse_foreign", Safe, refuse_foreign);
