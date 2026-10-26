//! MM-9 ktests: page-cache reclaim. `setup` and `private` are Safe (rerunnable, they free only their own
//! file's pages); `pressure` and `mapped_exec` are QemuOnly: they fill the machine's memory with a file's
//! pages and then ask for more program memory than is free (host/tests/mm/60-reclaim.sh runs them with a small
//! RAM; on a machine above 2.5 GiB they skip).

use crate::{AddressSpace, Fill, Prot};
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use core::sync::atomic::{AtomicU64, Ordering};
use ktest::{kassert, ktest, TestResult};
use vfs::pagecache::{Backend, CachePage, PageCache};
use vfs::reclaim::{self as vr, Mode, Reclaimer};

const P: u64 = 4096;
const MIB: u64 = 1 << 20;
const MAGIC: u64 = 0x5eed_0000_0000;

/// A device that is not there: every page holds its number at both ends.
struct Pat {
    reads: AtomicU64,
    /// Pages written back (their content is the pattern again: the tests dirty pages by writing it).
    writes: AtomicU64,
}

impl Backend for Pat {
    fn read_page(&self, pgoff: u64, page: &Arc<CachePage>) -> Result<(), i64> {
        self.read_pages(pgoff, core::slice::from_ref(page))
    }
    fn read_pages(&self, first: u64, pages: &[Arc<CachePage>]) -> Result<(), i64> {
        self.reads.fetch_add(pages.len() as u64, Ordering::Relaxed);
        for (i, p) in pages.iter().enumerate() {
            let v = (first + i as u64).to_le_bytes();
            p.write(0, &v);
            p.write(P as usize - 8, &v);
        }
        Ok(())
    }
    fn write_run(&self, _first: u64, pages: &[Arc<CachePage>], _size: u64) -> Result<(), i64> {
        self.writes.fetch_add(pages.len() as u64, Ordering::Relaxed);
        Ok(())
    }
    fn read_window(&self) -> u64 {
        32
    }
}

fn pattern_file(pages: u64) -> (PageCache, Arc<Pat>) {
    let b = Arc::new(Pat { reads: AtomicU64::new(0), writes: AtomicU64::new(0) });
    (PageCache::with_backend(b.clone(), pages * P), b)
}

/// Reads every page of a `pattern_file` and checks both ends.
fn check_file(c: &PageCache, pages: u64) -> Result<(), String> {
    for i in 0..pages {
        let (mut a, mut z) = ([0u8; 8], [0u8; 8]);
        c.read_at(i * P, &mut a).map_err(|e| format!("read page {i}: {e}"))?;
        c.read_at(i * P + P - 8, &mut z).map_err(|e| format!("read page {i}: {e}"))?;
        kassert!(u64::from_le_bytes(a) == i && u64::from_le_bytes(z) == i, "page {i} reads {:#x}/{:#x}", u64::from_le_bytes(a), u64::from_le_bytes(z));
    }
    Ok(())
}

fn stats() -> mm_core::FrameStats {
    mm_core::frames().map(|f| f.stats()).unwrap_or_default()
}

fn free_pages() -> u64 {
    stats().free_bytes / P
}

/// The reclaimer is installed, the watermarks and the reserve follow Linux's rules for this machine's RAM, and
/// MemAvailable counts the file cache.
fn setup() -> TestResult {
    struct Never;
    impl mm_core::reclaim::Reclaim for Never {
        fn reclaim(&self, _: u64, _: bool) -> mm_core::reclaim::Reclaimed {
            mm_core::reclaim::Reclaimed::default()
        }
    }
    static NEVER: Never = Never;
    kassert!(!mm_core::reclaim::set_reclaimer(&NEVER), "no reclaimer was installed at boot (mm.reclaim initcall)");
    let s = stats();
    let total = s.total_bytes / P;
    let w = crate::reclaim::watermarks();
    kassert!(w == mm_core::reclaim::watermarks(total), "{w:?}");
    kassert!(w.min >= 32 && w.min < w.low && w.low < w.high && w.high < total / 8, "{w:?} for {total} frames");
    let b = mm_core::boot_buddy().ok_or("no boot allocator")?;
    let reserve: u64 = b.reserve_frames().iter().sum();
    // Set from the frames managed at the core initcall (released boot modules come later).
    kassert!(reserve <= w.min && reserve >= w.min * 9 / 10, "reserve {reserve} vs min {}", w.min);
    let file = vr::file_pages() * P;
    let avail = mm_core::reclaim::mem_available(s.total_bytes, s.free_bytes, file);
    kassert!(avail + w.high * P >= s.free_bytes, "MemAvailable {avail} lost free memory {}", s.free_bytes);
    kassert!(avail <= s.free_bytes + file, "MemAvailable {avail} above free + file");
    let d = mm_core::reclaim::direct_stats();
    let st = vr::stats();
    klog::event!(Info, "mm", "ktest.reclaim_setup", min_kb = w.min * 4, low_kb = w.low * 4, high_kb = w.high * 4, reserve_kb = reserve * 4,
        avail_mb = avail / MIB, free_mb = s.free_bytes / MIB, file_mb = file / MIB, caches = st.listed.0 as u64, preload_caches = st.listed.1 as u64,
        reclaimed = st.pages, direct_runs = d.runs, direct_failed = d.failed, background_pages = st.background_pages);
    Ok(())
}
ktest!("mm.reclaim.setup", Safe, setup);

/// A private reclaimer over one file of real frames: the clock takes its clean pages (frames go back), a read
/// brings them in again with the same content; a pinned page stays.
fn private() -> TestResult {
    let (c, b) = pattern_file(256);
    check_file(&c, 256)?;
    kassert!(c.pages() == 256);
    let pin = c.page_for_map(7, false).map_err(|e| format!("page_for_map: {e:?}"))?;
    let r = Reclaimer::of(&[&c]);
    let o = r.shrink(64, Mode::Background, false);
    kassert!(o.freed == 64, "{o:?}");
    kassert!(c.pages() == 192);
    let o = r.shrink(1000, Mode::Direct, true);
    kassert!(o.freed == 191 && o.held >= 1, "all but the pinned page: {o:?}");
    kassert!(c.cached(7).is_some_and(|p| Arc::ptr_eq(&p, &pin)), "the pinned page left the cache");
    let before = b.reads.load(Ordering::Relaxed);
    check_file(&c, 256)?;
    kassert!(b.reads.load(Ordering::Relaxed) - before == 255, "{} pages read again", b.reads.load(Ordering::Relaxed) - before);
    Ok(())
}
ktest!("mm.reclaim.private", Safe, private);

/// The start-up preload's pages nobody used go before a read file's pages (FS-13 marks them at insert).
fn preloaded_first() -> TestResult {
    let (read, _) = pattern_file(64);
    check_file(&read, 64)?;
    let (pre, _) = pattern_file(64);
    let (got, _) = pre.prefetch(0, 64, &mut |_| true).map_err(|e| format!("prefetch: {e}"))?;
    kassert!(got == 64, "prefetch brought {got} pages");
    let mut v = [0u8; 8];
    pre.read_at(0, &mut v).map_err(|e| format!("{e}"))?;
    let r = Reclaimer::of(&[&read, &pre]);
    kassert!(r.listed() == (2, 1), "{:?}", r.listed());
    let o = r.shrink(63, Mode::Direct, false);
    kassert!(o.freed == 63 && o.preloaded == 63, "{o:?}");
    kassert!(read.pages() == 64 && pre.cached(0).is_some(), "the read file or the used preloaded page went first");
    Ok(())
}
ktest!("mm.reclaim.preloaded_first", Safe, preloaded_first);

/// Program memory of `pages` pages, each written with its number (Err names the page that failed).
fn anon(pages: u64) -> Result<(AddressSpace, u64), String> {
    let a = AddressSpace::with_fill(Fill::OnFault).map_err(|e| format!("address space: {e:?}"))?;
    let m = a.map_anywhere(pages * P, Prot::rw()).map_err(|e| format!("mmap of {} MiB: {e:?}", pages * P / MIB))?;
    for i in 0..pages {
        if let Err(e) = a.write_u64(m + i * P, MAGIC | i) {
            return Err(format!("page {i} of {pages}: {e:?} (free {} pages, direct {:?}, cache {:?})", free_pages(), mm_core::reclaim::direct_stats(), vr::stats()));
        }
    }
    Ok((a, m))
}

fn check_anon(a: &AddressSpace, m: u64, pages: u64) -> Result<(), String> {
    for i in 0..pages {
        let v = a.read_u64(m + i * P).map_err(|e| format!("read back page {i}: {e:?}"))?;
        kassert!(v == MAGIC | i, "program page {i} reads {v:#x}");
    }
    Ok(())
}

/// Skips on machines too big for a pressure test (it would fill all their memory).
fn small_machine() -> Option<TestResult> {
    let total = stats().total_bytes;
    (total == 0 || total > 5 * (512 * MIB)).then(|| ktest::skip("pressure test for small RAM (60-reclaim.sh, the 2G gate)"))
}

/// The pages a pressure test may map beyond its file (strict commit: the room left under the limit).
fn commit_room() -> u64 {
    crate::commit::limit().saturating_sub(crate::commit::committed()).saturating_sub(8 * MIB / P)
}

/// Fills memory with a file's clean pages, then writes more program memory than is free: every page arrives
/// (direct and background reclaim take the file's pages back), no allocation fails, the file reads right again.
fn pressure() -> TestResult {
    if let Some(s) = small_machine() {
        return s;
    }
    let w = crate::reclaim::watermarks();
    let free0 = free_pages();
    let margin = w.high + 16 * MIB / P;
    if free0 < margin + 16 * MIB / P {
        return ktest::skip("too little free memory to start");
    }
    let (s0, d0) = (vr::stats(), mm_core::reclaim::direct_stats());
    let fails0 = mm_core::report::GLOBAL.alloc_fails.load(Ordering::Relaxed);
    let target = free0 - margin;
    let (c, b) = pattern_file(target);
    check_file(&c, target)?;
    let cached = c.pages();
    let free1 = free_pages();
    // MemAvailable counts the clean cache (CR1 shows it in /proc/meminfo).
    let avail = vr::mem_available_bytes();
    kassert!(avail > free1 * P && avail <= free1 * P + vr::file_pages() * P, "MemAvailable {avail} vs free {} + file {}", free1 * P, vr::file_pages() * P);
    let n = (free1 + cached / 2).min(commit_room());
    if n < free1 + 4 * MIB / P {
        return ktest::skip("the commit limit leaves no room past the free memory");
    }
    let (a, m) = anon(n)?;
    let after = c.pages();
    let (s1, d1) = (vr::stats(), mm_core::reclaim::direct_stats());
    let reclaimed = s1.pages - s0.pages;
    klog::event!(Info, "mm", "ktest.reclaim_pressure", file_mb = target * P / MIB, cached_mb = cached * P / MIB, free_mb = free1 * P / MIB,
        program_mb = n * P / MIB, cache_after_mb = after * P / MIB, reclaimed = reclaimed, preloaded = s1.preloaded_pages - s0.preloaded_pages,
        direct_runs = d1.runs - d0.runs, direct_pages = d1.pages - d0.pages, background_pages = s1.background_pages - s0.background_pages);
    kassert!(d1.failed == d0.failed, "{} allocations failed although the file's pages could go", d1.failed - d0.failed);
    kassert!(mm_core::report::GLOBAL.alloc_fails.load(Ordering::Relaxed) == fails0, "a frame allocation failed under pressure");
    kassert!(after < cached, "the cache did not shrink: {cached} -> {after}");
    kassert!(reclaimed + free1 + 4096 >= n, "{n} program pages, {free1} free, only {reclaimed} reclaimed");
    check_anon(&a, m, n)?;
    drop(a);
    let reads = b.reads.load(Ordering::Relaxed);
    check_file(&c, target)?;
    kassert!(b.reads.load(Ordering::Relaxed) > reads, "the reclaimed pages were not read again");
    Ok(())
}
ktest!("mm.reclaim.pressure", QemuOnly, pressure);

/// busybox from the boot stick (FAT) mapped like a program's text keeps reading the same bytes while memory
/// runs out: its mapped pages stay (pinned), its unmapped ones may go and are read again from the stick.
fn mapped_exec() -> TestResult {
    if let Some(s) = small_machine() {
        return s;
    }
    // A program file on the stick's FAT volume (busybox when a stick carries it, else ninja, which xtask puts on
    // every image). The stick mounts while the ktests start: wait for it a bounded while.
    const PROGS: [&[u8]; 2] = [b"/boot/BITSOS2/bin/busybox", b"/boot/BITSOS2/bin/ninja"];
    let ns = vfs::root_ns();
    let k = vfs::Ctx::kernel(&ns);
    let mut found = None;
    for _ in 0..50 {
        found = PROGS.iter().find_map(|p| k.open(None, p, vfs::abi::O_RDONLY, 0).ok());
        if found.is_some() {
            break;
        }
        sched::sleep_ms(100);
    }
    let Some(f) = found else { return ktest::skip("no program file in /boot/BITSOS2/bin (no boot stick)") };
    let i = f.inode().ok_or("no inode")?.clone();
    let cache = i.page_cache().ok_or("no page cache")?;
    kassert!(cache.reclaimable(), "a FAT file is reclaimable");
    let size = cache.size();
    let pages = size / P;
    kassert!(pages >= 64, "the program is {size} bytes");
    let mapped = pages / 2;
    let a = AddressSpace::with_fill(Fill::OnFault).map_err(|e| format!("{e:?}"))?;
    let (prot, flags) = (5, crate::linux::MAP_PRIVATE);
    let m = a.sys_mmap_file(0, mapped * P, prot, flags, 0, &mut |len| vfs::mmap::check(&f, prot, flags, 0, len)).map_err(|e| format!("mmap: {e}"))?;
    let mut sum = 0u64;
    for pg in 0..mapped {
        sum = sum.wrapping_mul(31).wrapping_add(a.read_u64(m + pg * P).map_err(|e| format!("{e:?}"))?);
    }
    let mut file_sum = 0u64;
    let mut word = [0u8; 8];
    for pg in mapped..pages {
        cache.read_at(pg * P, &mut word).map_err(|e| format!("read: {e}"))?;
        file_sum = file_sum.wrapping_mul(31).wrapping_add(u64::from_le_bytes(word));
    }
    // Pressure: a pattern file over the free memory, then program memory past what is left.
    let w = crate::reclaim::watermarks();
    let free0 = free_pages();
    let target = free0.saturating_sub(w.high + 16 * MIB / P);
    let (c, _b) = pattern_file(target);
    check_file(&c, target)?;
    let s0 = vr::stats();
    let free1 = free_pages();
    let n = (free1 + c.pages() / 2).min(commit_room());
    if n < free1 + 4 * MIB / P {
        return ktest::skip("the commit limit leaves no room past the free memory");
    }
    let (b, bm) = anon(n)?;
    let mut again = 0u64;
    for pg in 0..mapped {
        again = again.wrapping_mul(31).wrapping_add(a.read_u64(m + pg * P).map_err(|e| format!("{e:?}"))?);
    }
    kassert!(again == sum, "the mapped text changed under pressure");
    for pg in 0..mapped {
        kassert!(cache.cached(pg).is_some(), "mapped page {pg} left the cache");
    }
    let tail_left = (mapped..pages).filter(|&pg| cache.cached(pg).is_some()).count() as u64;
    let mut file_again = 0u64;
    for pg in mapped..pages {
        cache.read_at(pg * P, &mut word).map_err(|e| format!("read again: {e}"))?;
        file_again = file_again.wrapping_mul(31).wrapping_add(u64::from_le_bytes(word));
    }
    kassert!(file_again == file_sum, "the unmapped part of busybox reads differently after reclaim");
    check_anon(&b, bm, n)?;
    klog::event!(Info, "mm", "ktest.reclaim_mapped", prog_pages = pages, mapped = mapped, unmapped_left = tail_left,
        reclaimed = vr::stats().pages - s0.pages, program_mb = n * P / MIB);
    Ok(())
}
ktest!("mm.reclaim.mapped_exec", QemuOnly, mapped_exec);

/// Writes the pattern (unchanged bytes, but the pages are dirty) into every `step`-th page of `c`.
fn dirty_every(c: &PageCache, pages: u64, step: u64) -> Result<u64, String> {
    let mut n = 0;
    for i in (0..pages).step_by(step as usize) {
        let v = i.to_le_bytes();
        c.write_at(i * P, &v).map_err(|e| format!("write page {i}: {e}"))?;
        c.write_at(i * P + P - 8, &v).map_err(|e| format!("write page {i}: {e}"))?;
        n += 1;
    }
    Ok(n)
}

/// Dirty pages are skipped (counted as dirty, the call is not `cut`: nothing reclaimable there) until they are
/// written back; then they go and read the same again.
fn dirty() -> TestResult {
    let (c, b) = pattern_file(256);
    check_file(&c, 256)?;
    let d = dirty_every(&c, 256, 2)?;
    kassert!(c.dirty_pages() == d, "{} dirty, {d} written", c.dirty_pages());
    let r = Reclaimer::of(&[&c]);
    let o = r.shrink(1000, Mode::Direct, true);
    kassert!(o.freed == 256 - d && o.dirty >= d && !o.cut, "only the clean half: {o:?}");
    kassert!(c.pages() == d && c.dirty_pages() == d);
    c.sync_all(vfs::pagecache::SyncKind::Fsync).map_err(|e| format!("sync: {e}"))?;
    kassert!(b.writes.load(Ordering::Relaxed) == d && c.dirty_pages() == 0, "{} pages written back", b.writes.load(Ordering::Relaxed));
    let o = r.shrink(1000, Mode::Direct, true);
    kassert!(o.freed == d && c.pages() == 0, "written back: they go: {o:?}");
    check_file(&c, 256)
}
ktest!("mm.reclaim.dirty", Safe, dirty);

/// `pressure` with every other page of the file dirty: program memory up to free plus half the clean pages
/// arrives without a failed allocation (the dirty pages stay, the clean ones go); after write-back the dirty
/// pages can go too.
fn dirty_pressure() -> TestResult {
    if let Some(s) = small_machine() {
        return s;
    }
    let w = crate::reclaim::watermarks();
    let free0 = free_pages();
    let margin = w.high + 32 * MIB / P;
    if free0 < margin + 32 * MIB / P {
        return ktest::skip("too little free memory to start");
    }
    let d0 = mm_core::reclaim::direct_stats();
    let fails0 = mm_core::report::GLOBAL.alloc_fails.load(Ordering::Relaxed);
    let target = free0 - margin;
    let (c, b) = pattern_file(target);
    check_file(&c, target)?;
    let dirty = dirty_every(&c, target, 2)?;
    let clean = c.pages() - dirty;
    let free1 = free_pages();
    let n = (free1 + clean / 2).min(commit_room());
    if n < free1 + 4 * MIB / P {
        return ktest::skip("the commit limit leaves no room past the free memory");
    }
    let (a, m) = anon(n)?;
    let d1 = mm_core::reclaim::direct_stats();
    klog::event!(Info, "mm", "ktest.reclaim_dirty_pressure", file_mb = target * P / MIB, dirty_mb = dirty * P / MIB, program_mb = n * P / MIB,
        cache_after_mb = c.pages() * P / MIB, direct_runs = d1.runs - d0.runs, direct_failed = d1.failed - d0.failed);
    kassert!(d1.failed == d0.failed, "{} allocations failed although clean pages could go", d1.failed - d0.failed);
    kassert!(mm_core::report::GLOBAL.alloc_fails.load(Ordering::Relaxed) == fails0, "a frame allocation failed under pressure");
    kassert!(c.dirty_pages() == dirty, "dirty pages went: {} of {dirty} left", c.dirty_pages());
    check_anon(&a, m, n)?;
    drop(a);
    c.sync_all(vfs::pagecache::SyncKind::Fsync).map_err(|e| format!("sync: {e}"))?;
    kassert!(b.writes.load(Ordering::Relaxed) == dirty, "{} of {dirty} written back", b.writes.load(Ordering::Relaxed));
    let held = c.pages();
    let o = Reclaimer::of(&[&c]).shrink(held, Mode::Background, true);
    kassert!(o.freed == held && c.pages() == 0, "after write-back every page goes: {o:?}");
    check_file(&c, target)
}
ktest!("mm.reclaim.dirty_pressure", QemuOnly, dirty_pressure);
