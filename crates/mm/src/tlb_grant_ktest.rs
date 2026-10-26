//! ktests of MM-tlb2 (YT-4a CR 7: the TLB shootdown IPI storm during Cobalt's 1080p playback): a write fault
//! that only widens rights (fbdev deferred io's dirty mark, copy-on-write reuse) takes no shootdown round, a
//! CPU with the old read-only translation recovers through one spurious fault; one munmap or madvise of a
//! Cobalt-sized buffer is one round; and `mm.tlb.cobalt` runs Cobalt's pattern (decoder threads that mmap,
//! fill, madvise and munmap 2-8 MB buffers at 60 Hz on their own CPUs, a renderer writing a deferred-io frame
//! buffer that is write-protected every frame) and reports rounds and IPIs per second. Rounds are counted per
//! space (`tlb::generation`), so other programs' shootdowns do not disturb the counts.

use crate::device::{Aperture, DeviceMap, DeviceOps, DevicePage};
use crate::dma::{DmaBuffer, DmaCache, DmaConstraints};
use crate::linux::{madv, MAP_ANONYMOUS, MAP_PRIVATE, MAP_SHARED};
use crate::tlb_ktest::{ap_activate, aps, on, ap_read_in, ap_write_in, FAULT, PF_P, PF_W, PROBE_ROOT, WRITE_VAL};
use crate::{AddressSpace, Fill};
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering::SeqCst};
use hal::paging::{self, tlb, PageFlags, GATHER_MAX};
use ktest::{kassert, ktest, TestResult};
use vfs::pagecache::PageFault;

const P: u64 = 4096;
const MB: u64 = 1 << 20;
const RW: u64 = 3;
const AP: u64 = MAP_ANONYMOUS | MAP_PRIVATE;
const PF_U: u64 = 4;
const SEC: u64 = 1_000_000_000;

fn e<T: core::fmt::Debug>(x: T) -> String {
    format!("{x:?}")
}

fn lazy() -> Result<Arc<AddressSpace>, String> {
    AddressSpace::with_fill(Fill::OnFault).map(Arc::new).map_err(|x| format!("address space: {x:?}"))
}

fn generation(a: &AddressSpace) -> u64 {
    tlb::generation(a.root()).unwrap_or(0)
}

fn user_frames() -> u64 {
    mm_core::frames().map_or(0, |f| f.stats().used_by_kind[mm_core::Kind::User as usize])
}

/// A device whose pages stay read-only until written (dirty bit per page), as fbdev's deferred io.
struct DirtyDev {
    pages: u64,
    dirty: Vec<AtomicU64>,
}

impl DirtyDev {
    fn new(pages: u64) -> Self {
        DirtyDev { pages, dirty: (0..pages.div_ceil(64)).map(|_| AtomicU64::new(0)).collect() }
    }
    fn is_dirty(&self, pg: u64) -> bool {
        self.dirty[(pg / 64) as usize].load(SeqCst) & (1 << (pg % 64)) != 0
    }
    fn clean(&self) {
        self.dirty.iter().for_each(|d| d.store(0, SeqCst));
    }
}

impl DeviceOps for DirtyDev {
    fn check(&self, pgoff: u64, pages: u64, _prot: u64, shared: bool) -> Result<(), i64> {
        if !shared || pgoff + pages > self.pages { Err(uapi::errno::EINVAL) } else { Ok(()) }
    }
    fn fault(&self, pgoff: u64, write: bool) -> Result<DevicePage, PageFault> {
        if pgoff >= self.pages {
            return Err(PageFault::Bus);
        }
        if write {
            self.dirty[(pgoff / 64) as usize].fetch_or(1 << (pgoff % 64), SeqCst);
        }
        Ok(DevicePage { aperture: 0, page: pgoff, writable: self.is_dirty(pgoff) })
    }
}

/// A WC "frame buffer" of up to `pages` pages (fewer when the DMA allocator has no such run) behind a dirty-tracking
/// device, mapped shared into `a`.
struct Fb {
    dev: Arc<DirtyDev>,
    map: Arc<DeviceMap>,
    base: u64,
    pages: u64,
    _buf: Arc<DmaBuffer>,
}

fn fb(a: &AddressSpace, want: u64) -> Result<Option<Fb>, String> {
    let Ok(f) = vfs::root_ns().open_flags(b"/dev/zero", vfs::abi::O_RDWR, 0) else { return Ok(None) };
    let c = DmaConstraints::dma64().with_cache(DmaCache::WriteCombining);
    let mut pages = want;
    let buf = loop {
        match DmaBuffer::alloc((pages * P) as usize, c) {
            Ok(b) => break Arc::new(b),
            Err(_) if pages > 64 => pages /= 2,
            Err(x) => return Err(format!("WC DMA buffer: {x:?}")),
        }
    };
    let dev = Arc::new(DirtyDev::new(pages));
    let map = DeviceMap::new(dev.clone(), alloc::vec![Aperture::Ram(buf.clone())]).map_err(e)?;
    let base = a.sys_mmap_device(0, pages * P, RW, MAP_SHARED, 0, &f, &map).map_err(e)?;
    Ok(Some(Fb { dev, map, base, pages, _buf: buf }))
}

/// A store into page `va` of `a` as the CPU would report it: present or not.
fn store_fault(a: &AddressSpace, va: u64) -> Result<(), String> {
    let present = paging::translate(a.root(), va).is_some();
    a.handle_user_fault(va, PF_U | PF_W | if present { PF_P } else { 0 }).map_err(|x| format!("store fault at {va:#x}: {}", x.text()))
}

/// An AP's store through its own TLB: a read-only translation cached before a grant may fault once (spurious,
/// the fault drops it); the retry must go through.
fn ap_store(c: usize, va: u64, v: u64) -> Result<bool, String> {
    WRITE_VAL.store(v, SeqCst);
    let w = on(c, ap_write_in, va)?;
    if w == 0 {
        return Ok(false);
    }
    kassert_str(w & FAULT != 0 && w & (PF_P | PF_W) == PF_P | PF_W, || format!("CPU {c} store at {va:#x}: {w:#x}"))?;
    let w = on(c, ap_write_in, va)?;
    kassert_str(w == 0, || format!("CPU {c} store at {va:#x} faults again after the spurious fault: {w:#x}"))?;
    Ok(true)
}

fn kassert_str(ok: bool, why: impl FnOnce() -> String) -> Result<(), String> {
    if ok { Ok(()) } else { Err(why()) }
}

fn leave_all(cpus: &[usize]) {
    let k = paging::home_root();
    for &c in cpus {
        let _ = on(c, ap_activate, k);
    }
}

/// Checker finding 1 (fixer): until the kernel's copy fault path retries spurious faults
/// (`tlb::set_spurious_copy_retry`), a space that did not opt in keeps the round for a copy-on-write reuse, so
/// no CPU is left with the read-only entry a `read()` into the page would fail on (EFAULT); with the retry on,
/// the reuse takes no round. Expects the probes taken.
fn kept_round(aps: &[usize]) -> TestResult {
    let b = lazy()?;
    PROBE_ROOT.store(b.root(), SeqCst);
    let r = (|| -> TestResult {
        for &c in aps {
            kassert!(on(c, ap_activate, b.root())? == b.root(), "CPU {c} did not load the space");
        }
        let x = b.sys_mmap(0, P, RW, AP, 0).map_err(e)?;
        b.write_u64(x, 0x4242).map_err(e)?;
        drop(b.fork().map_err(e)?);
        for &c in aps {
            kassert!(on(c, ap_read_in, x)? == 0x4242, "CPU {c} reads the page wrong");
        }
        let (g, kept0, retry) = (generation(&b), tlb::stats().grant_rounds, tlb::stats().spurious_copy_retry);
        store_fault(&b, x)?;
        let rounds = generation(&b) - g;
        if retry {
            kassert!(rounds == 0, "{rounds} rounds for a copy-on-write reuse with the copy retry on");
            return Ok(());
        }
        kassert!(rounds >= 1 && tlb::stats().grant_rounds > kept0, "the reuse took {rounds} rounds without the copy retry: a read() on another CPU would fail");
        for (k, &c) in aps.iter().enumerate() {
            WRITE_VAL.store(0x6600 + k as u64, SeqCst);
            let w = on(c, ap_write_in, x)?;
            kassert!(w == 0, "CPU {c} still had the read-only entry after the round: {w:#x}");
        }
        Ok(())
    })();
    leave_all(aps);
    r
}

/// Write faults that only widen rights take no round; the other CPUs see the new rights (after at most one
/// spurious fault each), and the next write-protect is a real round again.
fn grant() -> TestResult {
    let aps = aps();
    if aps.is_empty() {
        return ktest::skip("one CPU");
    }
    let frames_before = user_frames();
    let grants0 = tlb::stats().grants;
    let r = (|| -> TestResult {
        let _probes = paging::probe::user_probes().map_err(e)?;
        kept_round(&aps)?;
        let a = lazy()?;
        // No fast kernel copy writes into this space: its grants may skip the round whatever the copy fault path
        // does (`tlb::allow_grants`).
        tlb::allow_grants(a.root());
        let Some(f) = fb(&a, 4)? else { return ktest::skip("no /dev/zero to stand in for a device file") };
        let m = f.base;
        PROBE_ROOT.store(a.root(), SeqCst);
        for &c in &aps {
            kassert!(on(c, ap_activate, a.root())? == a.root(), "CPU {c} did not load the space");
        }
        for i in 0..4 {
            a.handle_user_fault(m + i * P, PF_U).map_err(|x| x.text())?;
            for &c in &aps {
                kassert!(on(c, ap_read_in, m + i * P)? & FAULT == 0, "CPU {c} cannot read device page {i}");
            }
        }
        // The dirty marks: four write faults, no round, the leaves writable with their memory type.
        let g0 = generation(&a);
        for i in 0..4 {
            store_fault(&a, m + i * P)?;
        }
        kassert!(generation(&a) == g0, "{} rounds for four dirty marks", generation(&a) - g0);
        for i in 0..4 {
            let (_, fl) = paging::translate(a.root(), m + i * P).ok_or("device page gone")?;
            kassert!(fl.contains(PageFlags::WRITABLE) && fl.cache() == Some(hal::paging::CacheType::WriteCombining) && f.dev.is_dirty(i), "page {i}: {fl:?}");
        }
        let mut spurious = 0;
        for (k, &c) in aps.iter().enumerate() {
            for i in 0..4 {
                spurious += ap_store(c, m + i * P, 0x7700 + k as u64)? as u64;
            }
        }
        // fbdev copied the picture: write-protect is a real round, and every CPU's store faults again.
        f.dev.clean();
        kassert!(f.map.write_protect(0, 3) == 4, "four entries write-protected");
        kassert!(generation(&a) > g0, "the write-protect raised no generation");
        for &c in &aps {
            WRITE_VAL.store(1, SeqCst);
            let w = on(c, ap_write_in, m)?;
            kassert!(w & FAULT != 0 && w & (PF_P | PF_W) == PF_P | PF_W, "CPU {c} stored into a write-protected page: {w:#x}");
        }
        // Copy-on-write reuse: after the child is gone the parent's page is its own again; the write that makes
        // it writable takes no round either.
        let x = a.sys_mmap(0, P, RW, AP, 0).map_err(e)?;
        a.write_u64(x, 0x1234).map_err(e)?;
        drop(a.fork().map_err(e)?);
        let (_, fl) = paging::translate(a.root(), x).ok_or("anonymous page gone")?;
        kassert!(!fl.contains(PageFlags::WRITABLE), "the fork left the parent's page writable");
        for &c in &aps {
            kassert!(on(c, ap_read_in, x)? == 0x1234, "CPU {c} reads the page wrong");
        }
        let g1 = generation(&a);
        store_fault(&a, x)?;
        kassert!(generation(&a) == g1, "the copy-on-write reuse took a round");
        for (k, &c) in aps.iter().enumerate() {
            spurious += ap_store(c, x, 0x5500 + k as u64)? as u64;
        }
        kassert!(a.read_u64(x).map_err(e)? == 0x5500 + aps.len() as u64 - 1, "the last AP's store is not there");
        leave_all(&aps);
        klog::event!(Info, "mm", "ktest.tlb_grant", aps = aps.len(), spurious = spurious, grants = tlb::stats().grants - grants0);
        Ok(())
    })();
    leave_all(&aps);
    r?;
    kassert!(tlb::stats().grants >= grants0 + 5, "grants {} -> {}", grants0, tlb::stats().grants);
    kassert!(user_frames() == frames_before, "{} bytes of user frames not back", user_frames().saturating_sub(frames_before));
    Ok(())
}
ktest!("mm.tlb.grant", Safe, grant);

/// One munmap or madvise(DONTNEED) of a decoder-sized buffer is one round (Linux's mmu_gather); a range beyond
/// `GATHER_MAX` frames takes one more.
fn gather() -> TestResult {
    let aps = aps();
    let frames_before = user_frames();
    let r = (|| -> TestResult {
        let a = lazy()?;
        for &c in &aps {
            on(c, ap_activate, a.root())?;
        }
        let fill = |base: u64, len: u64| -> TestResult {
            for va in (base..base + len).step_by(P as usize) {
                a.write_u64(va, va).map_err(e)?;
            }
            Ok(())
        };
        for (len, rounds) in [(8 * MB, 1), (48 * MB, 2)] {
            let base = a.sys_mmap(0, len, RW, AP, 0).map_err(e)?;
            fill(base, len)?;
            let g = generation(&a);
            a.sys_madvise(base, len, madv::DONTNEED).map_err(e)?;
            let dontneed = generation(&a) - g;
            fill(base, len / 2)?;
            let g = generation(&a);
            a.sys_munmap(base, len).map_err(e)?;
            let munmap = generation(&a) - g;
            klog::event!(Info, "mm", "ktest.tlb_gather", mb = len / MB, dontneed = dontneed, munmap = munmap, aps = aps.len());
            kassert!(dontneed <= rounds && munmap <= rounds, "{} MB: madvise {dontneed} rounds, munmap {munmap} rounds, want at most {rounds}", len / MB);
            // Checker finding 5: every touched page went away, so neither may skip its round; DONTNEED gave back
            // all `len` pages, more than one gather (`GATHER_MAX`) for 48 MB.
            let least = (len / P).div_ceil(GATHER_MAX as u64).max(1);
            kassert!(dontneed >= least && munmap >= 1, "{} MB: madvise {dontneed} rounds (want {least}), munmap {munmap}: a range went without its shootdown", len / MB);
        }
        // mprotect to read-only stays a round (only widening skips it), and no CPU keeps writing through its
        // writable entry.
        let _probes = paging::probe::user_probes().map_err(e)?;
        PROBE_ROOT.store(a.root(), SeqCst);
        let x = a.sys_mmap(0, P, RW, AP, 0).map_err(e)?;
        a.write_u64(x, 1).map_err(e)?;
        for (k, &c) in aps.iter().enumerate() {
            WRITE_VAL.store(0x3300 + k as u64, SeqCst);
            kassert!(on(c, ap_write_in, x)? == 0, "CPU {c} cannot write the fresh page");
        }
        let g = generation(&a);
        a.sys_mprotect(x, P, 1).map_err(e)?;
        kassert!(generation(&a) > g, "mprotect(PROT_READ) raised no generation");
        for &c in &aps {
            WRITE_VAL.store(0x99, SeqCst);
            let w = on(c, ap_write_in, x)?;
            kassert!(w & FAULT != 0 && w & (PF_P | PF_W) == PF_P | PF_W, "CPU {c} wrote through after mprotect(PROT_READ): {w:#x}");
        }
        kassert!(a.read_u64(x).map_err(e)? == 0x3300 + aps.len().saturating_sub(1) as u64 || aps.is_empty(), "the page lost the APs' stores");
        leave_all(&aps);
        Ok(())
    })();
    leave_all(&aps);
    r?;
    kassert!(user_frames() == frames_before, "{} bytes of user frames not back", user_frames().saturating_sub(frames_before));
    Ok(())
}
ktest!("mm.tlb.gather", Safe, gather);

/// What the Cobalt-like threads did.
#[derive(Default)]
struct Work {
    cycles: AtomicU64,
    frames: AtomicU64,
    stores: AtomicU64,
    /// Decoder faults on missing pages.
    fills: AtomicU64,
}

/// A decoder thread: 2-8 MB buffers at 60 Hz, filled, half given back with DONTNEED, unmapped.
fn decoder(a: &AddressSpace, k: u64, until: u64, w: &Work) -> Result<(), String> {
    a.activate();
    let mut seed = 0x9E37_79B9_7F4A_7C15u64 ^ k;
    let mut next = time::now_ns();
    while time::now_ns() < until {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        let len = (2 + seed % 7) * MB;
        let base = a.sys_mmap(0, len, RW, AP, 0).map_err(e)?;
        for va in (base..base + len).step_by(P as usize) {
            store_fault(a, va)?;
        }
        w.fills.fetch_add(len / P, SeqCst);
        a.sys_madvise(base, len / 2, madv::DONTNEED).map_err(e)?;
        a.sys_munmap(base, len).map_err(e)?;
        w.cycles.fetch_add(1, SeqCst);
        next += SEC / 60;
        let now = time::now_ns();
        if next > now {
            sched::sleep_ns(next - now);
        } else {
            next = now;
        }
    }
    Ok(())
}

/// The renderer: every page of the frame buffer stored to once per frame (the first store of a clean page is
/// a write fault), then fbdev's deferred io copies it out and write-protects the buffer again.
fn renderer(a: &AddressSpace, f: &Fb, until: u64, w: &Work) -> Result<(), String> {
    a.activate();
    let mut next = time::now_ns();
    while time::now_ns() < until {
        for i in 0..f.pages {
            let va = f.base + i * P;
            let writable = paging::translate(a.root(), va).is_some_and(|(_, fl)| fl.contains(PageFlags::WRITABLE));
            if !writable {
                store_fault(a, va)?;
                w.stores.fetch_add(1, SeqCst);
            }
        }
        f.dev.clean();
        f.map.write_protect(0, f.pages - 1);
        w.frames.fetch_add(1, SeqCst);
        next += SEC / 60;
        let now = time::now_ns();
        if next > now {
            sched::sleep_ns(next - now);
        } else {
            next = now;
        }
    }
    Ok(())
}

/// Reschedule IPIs sent so far (sched's counter; the other system vector that storms when threads of one space
/// contend for its lock).
fn resched_sent() -> u64 {
    klog::metrics::snapshot().find(|(n, _)| *n == "sched.resched_ipis_sent").map_or(0, |(_, v)| v)
}

/// Cobalt's playback pattern for 2 s; rounds, IPIs and shootdown interrupts per second go to `ktest.tlb_cobalt`.
/// Every round is one of: a write-protect per frame, a madvise and a munmap per decoder cycle (tables freed with
/// the munmap ride on its round). The grants of the renderer's write faults must not add any.
fn cobalt() -> TestResult {
    const RUN_NS: u64 = 2 * SEC;
    let cpus: Vec<usize> = (0..hal::percpu::cpu_count()).filter(|&c| hal::smp::is_online(c)).collect();
    if cpus.len() < 2 {
        return ktest::skip("one CPU");
    }
    let frames_before = user_frames();
    let a = lazy()?;
    tlb::allow_grants(a.root());
    let Some(f) = fb(&a, 2048)? else { return ktest::skip("no /dev/zero to stand in for a device file") };
    let f = Arc::new(f);
    let w = Arc::new(Work::default());
    let errs = Arc::new(ksync::SpinLock::new(Vec::<String>::new()));
    let decoders = (cpus.len() - 1).clamp(1, 4);
    let (s0, g0, r0, t0) = (tlb::stats(), generation(&a), resched_sent(), time::now_ns());
    let until = t0 + RUN_NS;
    let mut hs = Vec::new();
    for (k, &c) in cpus.iter().rev().take(decoders + 1).enumerate() {
        let (a, f, w, errs) = (a.clone(), f.clone(), w.clone(), errs.clone());
        let o = sched::SpawnOpts { cpu: Some(c), pin: true, ..sched::SpawnOpts::default() };
        let h = sched::spawn_with("mm-tlb-cobalt", sched::Class::Normal, o, move || {
            let r = if k == 0 { renderer(&a, &f, until, &w) } else { decoder(&a, k as u64, until, &w) };
            paging::activate(paging::home_root());
            if let Err(x) = r {
                errs.lock().push(x);
            }
        })
        .map_err(|x| format!("spawn on CPU {c}: {x:?}"))?;
        hs.push(h);
    }
    let joined = hs.iter().all(|h| h.join_timeout(60 * SEC));
    let (s1, g1, r1, t1) = (tlb::stats(), generation(&a), resched_sent(), time::now_ns());
    kassert!(joined, "a Cobalt-like thread did not finish within 60 s");
    if let Some(x) = errs.lock().first().cloned() {
        return Err(x);
    }
    let (cycles, frames, stores, fills) = (w.cycles.load(SeqCst), w.frames.load(SeqCst), w.stores.load(SeqCst), w.fills.load(SeqCst));
    let ms = ((t1 - t0) / 1_000_000).max(1);
    let per_s = |n: u64| n * 1000 / ms;
    let rounds = g1 - g0;
    klog::event!(Info, "mm", "ktest.tlb_cobalt", cpus = cpus.len(), decoders = decoders, fb_pages = f.pages, ms = ms, cycles = cycles, frames = frames,
        stores = stores, rounds = rounds, rounds_per_s = per_s(rounds), ipis_per_s = per_s(s1.ipis - s0.ipis), irqs_per_s = per_s(s1.irqs - s0.irqs),
        irqs_per_cpu_s = per_s(s1.irqs - s0.irqs) / cpus.len() as u64, grants = s1.grants - s0.grants, full_flushes = s1.full_flushes - s0.full_flushes,
        page_flushes = s1.page_flushes - s0.page_flushes, lazy_leaves = s1.lazy_leaves - s0.lazy_leaves, lazy_skips = s1.lazy_skips - s0.lazy_skips,
        lazy_catchups = s1.lazy_catchups - s0.lazy_catchups, kernel = s1.kernel - s0.kernel,
        resched_ipis_per_s = per_s(r1 - r0), fills = fills, resched = r1 - r0;
        "Cobalt-like playback: decoder buffers 2-8 MB at 60 Hz, a deferred-io frame buffer written every frame");
    drop(f);
    drop(a);
    kassert!(frames >= 2 && cycles >= 2, "too little happened: {frames} frames, {cycles} decoder cycles");
    kassert!(s1.give_ups == s0.give_ups, "a shootdown gave up");
    // Each counted cycle or frame may have a round in flight at the start and at the end: one of each as slack.
    let most = frames + 2 * cycles + 2 + 2 * decoders as u64;
    kassert!(rounds <= most, "{rounds} rounds for {frames} frames and {cycles} decoder cycles (at most {most}): write faults take rounds");
    // Checker finding 3: the decoders' fresh pages must not queue on the space's lock with a wake-up each (frame
    // zeroed before the lock): about 0.9 reschedule IPIs per filled page before, 0.05 after (-smp 4). Judged per
    // page, not per second, and only up to 4 CPUs: above, sched's own hand-overs vary too much (SC CR 1).
    if cpus.len() <= 4 {
        let resched = r1 - r0;
        kassert!(resched <= fills / 4 + 1000, "{resched} reschedule IPIs for {fills} filled pages: the decoders queue on the space's lock");
    }
    kassert!(user_frames() == frames_before, "{} bytes of user frames not back", user_frames().saturating_sub(frames_before));
    Ok(())
}
ktest!("mm.tlb.cobalt", Safe, cobalt);
