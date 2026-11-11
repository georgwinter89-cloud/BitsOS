//! ktests of the kernel's own page tables and the vmap allocator (MM-2): the switch, W^X and NX through
//! deliberate faults, the direct map against RAM and the MTRRs, WC/UC windows with their PAT bits, guard
//! pages, unmapping that gives page-table frames back, and the framebuffer's WC write speed (judged on the
//! Mini-PC only).

use crate::kernel_tables;
use crate::mtrr::Mtrrs;
use crate::vmap;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use bootinfo::layout::{DIRECT_MAP_BASE, VMAP_BASE};
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, Ordering};
use hal::paging::probe::{self, Probe, PF_FETCH, PF_PRESENT, PF_WRITE};
use hal::paging::{self, CacheType, PageFlags, PageSize, VmapError};
use ktest::{kassert, ktest, TestResult};

/// The kernel root. A kernel thread may still hold a process's root lazily (MM-6): the MCP ktest thread on the
/// Mini-PC after init ran on its CPU. The kernel half is shared, so that counts as running on the kernel root.
fn root() -> Result<u64, String> {
    let r = paging::kernel_root().ok_or("the kernel still runs on the loader's tables")?;
    let on_kernel = paging::current_root() == r || (paging::tlb::lazy_here() && paging::tlb::thread_root() == r);
    kassert!(on_kernel, "loaded root {:#x} is not the kernel root {r:#x}", paging::current_root());
    Ok(r)
}

fn switched() -> TestResult {
    root()?;
    let rep = kernel_tables::report().ok_or("no switch report")?;
    kassert!(rep.wx_pages == 0, "{} image pages writable and executable", rep.wx_pages);
    kassert!(rep.ram_bytes > 0 && rep.image_pages > 0);
    kassert!(rep.ram_bytes == kernel_tables::ram().iter().map(|r| r.1 - r.0).sum::<u64>(), "direct map != RAM");
    kassert!(vmap::stats().is_some(), "vmap allocator not installed");
    kassert!(paging::device_mapper(), "hal device mapper not installed");
    if hal::cpu::detect().pdpe1gb && rep.ram_bytes >= 3 << 30 {
        kassert!(rep.pages_1g > 0, "no 1 GiB page in {} MiB of RAM", rep.ram_bytes >> 20);
    }
    Ok(())
}
ktest!("mm.paging.switched", Safe, switched);

#[inline(never)]
fn text_target() -> u64 {
    0x5EED
}
/// A `ret` in .rodata and one in .data: executing either must fault (NX); were NX broken, it would return.
static RET_RO: [u8; 1] = [0xC3];
static RET_RW: AtomicU8 = AtomicU8::new(0xC3);

fn faulted(p: Probe, want: u64, what: &str) -> TestResult {
    match p {
        Probe::Faulted { error, .. } if error & want == want => Ok(()),
        other => Err(alloc::format!("{what}: {other:?} (wanted a fault with error bits {want:#x})")),
    }
}

fn wx() -> TestResult {
    let r = root()?;
    let text = text_target as fn() -> u64 as usize as u64;
    kassert!(text_target() == 0x5EED);
    let ro = RET_RO.as_ptr() as u64;
    let rw = &RET_RW as *const AtomicU8 as u64;
    kassert!(RET_RW.load(Ordering::Relaxed) == 0xC3);
    let fl = |va: u64| paging::translate(r, va).map(|t| t.1).ok_or_else(|| alloc::format!("{va:#x} unmapped"));
    let alias = |va: u64| paging::translate(r, va).map(|t| DIRECT_MAP_BASE + t.0).ok_or_else(|| alloc::format!("{va:#x} unmapped"));
    let (t, o, w) = (fl(text)?, fl(ro)?, fl(rw)?);
    kassert!(!t.contains(PageFlags::WRITABLE) && !t.contains(PageFlags::NX) && t.contains(PageFlags::GLOBAL), "text flags {:#x}", t.0);
    kassert!(!o.contains(PageFlags::WRITABLE) && o.contains(PageFlags::NX), "rodata flags {:#x}", o.0);
    kassert!(w.contains(PageFlags::WRITABLE | PageFlags::NX), "data flags {:#x}", w.0);
    let p = |r: Result<Probe, probe::Refused>| r.map_err(|e| alloc::format!("probe refused: {e:?}"));
    faulted(p(probe::write_same(text))?, PF_PRESENT | PF_WRITE, "write to text")?;
    faulted(p(probe::write_same(ro))?, PF_PRESENT | PF_WRITE, "write to rodata")?;
    faulted(p(probe::exec(ro))?, PF_PRESENT | PF_FETCH, "execute rodata")?;
    faulted(p(probe::exec(rw))?, PF_PRESENT | PF_FETCH, "execute data")?;
    kassert!(p(probe::write_same(rw))? == Probe::WritableNotProbed);
    // The direct-map aliases: text and rodata read-only there too, nothing executable.
    let (ta, oa, wa) = (alias(text)?, alias(ro)?, alias(rw)?);
    kassert!(!fl(ta)?.contains(PageFlags::WRITABLE) && fl(ta)?.contains(PageFlags::NX), "text alias flags {:#x}", fl(ta)?.0);
    faulted(p(probe::write_same(ta))?, PF_PRESENT | PF_WRITE, "write to the text alias")?;
    faulted(p(probe::exec(oa))?, PF_PRESENT | PF_FETCH, "execute the rodata alias")?;
    faulted(p(probe::exec(wa))?, PF_PRESENT | PF_FETCH, "execute the data alias")?;
    // No image page both writable and executable.
    let (s, e) = paging::kernel_image();
    let mut va = s & !0xFFF;
    let (mut x, mut ronly) = (0, 0);
    while va < e {
        let f = fl(va)?;
        kassert!(!(f.contains(PageFlags::WRITABLE) && !f.contains(PageFlags::NX)), "image page {va:#x} is W+X");
        kassert!(f.contains(PageFlags::GLOBAL), "image page {va:#x} not global");
        x += !f.contains(PageFlags::NX) as u32;
        ronly += !f.contains(PageFlags::WRITABLE) as u32;
        va += 0x1000;
    }
    klog::event!(Info, "mm", "ktest.wx", exec_pages = x, read_only_pages = ronly, image_kb = (e - s) >> 10);
    Ok(())
}
ktest!("mm.paging.wx", Safe, wx);

fn mtrrs() -> Mtrrs {
    hal::pat::read_mtrrs().as_ref().map_or_else(Mtrrs::none, Mtrrs::from_regs)
}

fn direct_map() -> TestResult {
    let r = root()?;
    let ram = kernel_tables::ram();
    kassert!(!ram.is_empty());
    let m = mtrrs();
    let inside = |s: u64, e: u64| {
        let i = ram.partition_point(|x| x.1 <= s);
        i < ram.len() && ram[i].0 <= s && e <= ram[i].1
    };
    let mut leaves = Vec::new();
    paging::for_each_leaf(r, DIRECT_MAP_BASE, VMAP_BASE, &mut |u| leaves.push(u));
    let mut total = 0;
    let mut sizes = [0u64; 3];
    for u in &leaves {
        let (p, n) = (u.virt - DIRECT_MAP_BASE, u.size.bytes());
        kassert!(u.phys == p, "direct map {:#x} -> {:#x}", u.virt, u.phys);
        kassert!(inside(p, p + n), "leaf {p:#x}+{n:#x} not all RAM");
        kassert!(m.block_type(p, n).is_some(), "leaf {p:#x}+{n:#x} crosses an MTRR type boundary");
        kassert!(u.flags.contains(PageFlags::NX | PageFlags::GLOBAL) && !u.flags.contains(PageFlags::USER));
        kassert!(u.flags.cache() == Some(CacheType::WriteBack));
        total += n;
        sizes[u.size as usize] += 1;
    }
    kassert!(total == ram.iter().map(|x| x.1 - x.0).sum::<u64>(), "direct map covers {total:#x} bytes");
    // The highest RAM page and the edges of every range translate to themselves.
    for &(s, e) in &ram {
        for p in [s, e - 0x1000, s + (((e - s) / 2) & !0xFFF)] {
            let l = paging::translate_leaf(r, DIRECT_MAP_BASE + p).ok_or_else(|| alloc::format!("RAM {p:#x} not in the direct map"))?;
            kassert!(l.phys == p, "RAM {p:#x} -> {:#x}", l.phys);
        }
    }
    let top = ram.last().map_or(0, |x| x.1);
    let past = paging::translate(r, DIRECT_MAP_BASE + top);
    kassert!(past.is_none() || ram.iter().any(|x| x.0 <= top && top < x.1), "the direct map runs past RAM");
    klog::event!(Info, "mm", "ktest.direct_map", ram_mb = total >> 20, top = klog::Hex(top), above_4g_mb = ram.iter().map(|x| x.1.saturating_sub(x.0.max(1 << 32))).sum::<u64>() >> 20,
        pages_4k = sizes[PageSize::Size4K as usize], pages_2m = sizes[PageSize::Size2M as usize], pages_1g = sizes[PageSize::Size1G as usize]);
    Ok(())
}
ktest!("mm.paging.direct_map", Safe, direct_map);

/// HPET's register page: device memory on q35 and on the Mini-PC's FCH; mapped UC but never touched here.
const HPET: u64 = 0xFED0_0000;

fn wc_and_uc_windows() -> TestResult {
    let r = root()?;
    let pat = hal::pat::read_pat();
    kassert!(hal::pat::pat_entry(pat, 1) == hal::pat::MT_WC, "PAT entry 1 is {} (PAT {pat:#x})", hal::pat::pat_entry(pat, 1));
    for (c, bits) in [(CacheType::Uncached, PageFlags::PCD | PageFlags::PWT), (CacheType::UncachedMinus, PageFlags::PCD)] {
        let w = vmap::map_device(HPET, 0x400, c).map_err(|e| alloc::format!("{c:?} window: {e:?}"))?;
        let (pa, f) = paging::translate(r, w.virt()).ok_or("window unmapped")?;
        kassert!(pa == HPET && f.0 & (PageFlags::PCD | PageFlags::PWT | PageFlags::PAT_4K) == bits, "{c:?}: flags {:#x}", f.0);
        kassert!(f.contains(PageFlags::GLOBAL | PageFlags::NX | PageFlags::WRITABLE));
    }
    let Some((fb_virt, fb_phys, fb_size)) = kernel_tables::framebuffer() else {
        return ktest::fail("no GOP framebuffer: the WC part needs one");
    };
    // The framebuffer at the loader's address, mapped anew by MM: WC, global, 2 MiB pages where aligned.
    let fl = paging::translate_leaf(r, fb_virt).ok_or("framebuffer mapping lost in the switch")?;
    kassert!(fl.phys == fb_phys && fl.flags.cache() == Some(CacheType::WriteCombining), "framebuffer flags {:#x}", fl.flags.0);
    kassert!(fl.flags.contains(PageFlags::GLOBAL | PageFlags::NX | PageFlags::WRITABLE));
    let aligned = |p: u64| p.is_multiple_of(2 << 20) && mtrrs().block_type(p, 2 << 20).is_some();
    kassert!(!(aligned(fb_phys) && fb_virt % (2 << 20) == 0 && fb_size >= 2 << 20) || fl.size == PageSize::Size2M, "framebuffer not in 2 MiB pages");
    kassert!(!vmap::is_free(fb_virt), "the framebuffer's range is free in the allocator");
    // A new WC window on it (same type, so the alias is allowed), through hal's device-window path.
    let len = fb_size.min(8 << 20);
    let w = paging::map_device_window(fb_phys, len, CacheType::WriteCombining).map_err(|e| alloc::format!("WC window: {e:?}"))?;
    let l = paging::translate_leaf(r, w.virt()).ok_or("WC window unmapped")?;
    kassert!(l.flags.0 & (PageFlags::PCD | PageFlags::PWT | PageFlags::PAT_4K) == PageFlags::PWT, "WC flags {:#x}", l.flags.0);
    kassert!(!(aligned(fb_phys) && len >= 2 << 20) || l.size == PageSize::Size2M, "an aligned WC framebuffer window should use 2 MiB pages");
    klog::event!(Info, "mm", "ktest.wc_window", phys = klog::Hex(fb_phys), virt = klog::Hex(w.virt()), len = len, page = alloc::format!("{:?}", l.size));
    Ok(())
}
ktest!("mm.vmap.wc_uc_pat", Safe, wc_and_uc_windows);

fn guard_pages() -> TestResult {
    root()?;
    let a = vmap::map_device(HPET, 0x1000, CacheType::Uncached).map_err(|e| alloc::format!("{e:?}"))?;
    let b = vmap::map_device(HPET, 0x1000, CacheType::Uncached).map_err(|e| alloc::format!("{e:?}"))?;
    kassert!(a.virt() != b.virt());
    for (what, addr) in [("guard below a", a.guard_below()), ("page after a", a.page_after()), ("guard below b", b.guard_below())] {
        match probe::read_unmapped(addr) {
            Ok(Probe::Faulted { addr: cr2, error }) => {
                kassert!(cr2 == addr && error & PF_PRESENT == 0, "{what}: fault at {cr2:#x}, error {error:#x}");
            }
            other => return Err(alloc::format!("{what} {addr:#x}: {other:?}")),
        }
    }
    let gone = a.virt();
    let quarantined = vmap::stats().ok_or("no vmap")?.quarantined_windows;
    drop(a);
    // MM-6: unmapped on every CPU (shootdown), not quarantined, also with the APs online.
    let now = vmap::stats().ok_or("no vmap")?.quarantined_windows;
    kassert!(now == quarantined, "dropped window quarantined: {quarantined} -> {now}");
    kassert!(paging::translate(paging::current_root(), gone).is_none(), "dropped window still mapped");
    kassert!(matches!(probe::read_unmapped(gone), Ok(Probe::Faulted { .. })), "access to a dropped window must fault");
    Ok(())
}
ktest!("mm.vmap.guard_fault", Safe, guard_pages);

/// What the walkers of `tables_smp` share.
struct TablesRun {
    root: u64,
    /// 2 MiB-aligned: `CHUNKS` regions no table covers yet, then one for the second phase.
    base: u64,
    walkers: u32,
    arrived: AtomicU32,
    failed: AtomicBool,
}

const TABLE_CHUNKS: u64 = 64;

impl TablesRun {
    /// Waits for every walker's `n`th arrival, so they enter a new region together. False once another failed.
    fn meet(&self, n: u32) -> bool {
        self.arrived.fetch_add(1, Ordering::AcqRel);
        while self.arrived.load(Ordering::Acquire) < n * self.walkers {
            if self.failed.load(Ordering::Acquire) {
                return false;
            }
            core::hint::spin_loop();
        }
        !self.failed.load(Ordering::Acquire)
    }

    /// One leaf, by `map_4k` (what `hal::mmio`, stacks and the CPU area use) or `map_kernel` (MM's windows).
    fn map(&self, me: u32, virt: u64) -> Result<(), String> {
        let flags = PageFlags::kernel_data().with_cache(CacheType::Uncached);
        if me % 2 == 0 {
            paging::map_4k(self.root, virt, HPET, flags, &mut mm_core::alloc_table_frame)
        } else {
            paging::map_kernel(self.root, virt, HPET, PageSize::Size4K, flags)
        }
        .map_err(|e| alloc::format!("{virt:#x}: {e:?}"))
    }

    fn walk(&self, me: u32) -> Result<u64, String> {
        // Everyone enters a region no table covers at the same moment: all see the entry empty.
        for round in 0..TABLE_CHUNKS as u32 {
            if !self.meet(2 * round + 1) {
                return Ok(0);
            }
            let virt = self.base + round as u64 * (2 << 20) + me as u64 * 0x1000;
            self.map(me, virt)?;
            if !self.meet(2 * round + 2) {
                return Ok(0);
            }
            kassert!(paging::translate(self.root, virt).is_some_and(|(p, _)| p & !0xFFF == HPET), "round {round}: walker {me} lost its page at {virt:#x}");
        }
        // One region again and again: whoever clears the last leaf takes the table out while the others map into it.
        let virt = self.base + TABLE_CHUNKS * (2 << 20) + me as u64 * 0x1000;
        let until = time::now_ns() + 100_000_000;
        let mut rounds = 0;
        while time::now_ns() < until && !self.failed.load(Ordering::Acquire) {
            self.map(me, virt)?;
            kassert!(paging::translate(self.root, virt).is_some_and(|(p, _)| p & !0xFFF == HPET), "walker {me} lost its page at {virt:#x} in a table another emptied");
            paging::unmap_kernel(virt, 0x1000, &mut |_| {}).map_err(|e| alloc::format!("unmap {virt:#x}: {e:?}"))?;
            rounds += 1;
        }
        Ok(rounds)
    }
}

/// The walkers that share the kernel half's tables, each on its own CPU: no page gets lost, whichever of them installs
/// or empties a table (INT-W30: `paging::kernel`'s `TABLES`).
fn tables_smp() -> TestResult {
    let root = root()?;
    sched::wait_cpus_online(5_000_000_000);
    let walkers = hal::percpu::cpu_count().min(8) as u32;
    if walkers < 2 {
        return ktest::fail("needs two CPUs");
    }
    let len = (TABLE_CHUNKS + 2) * (2 << 20);
    let va = paging::reserve_kernel_va(len).ok_or("no kernel VA")?;
    let base = (va + (2 << 20) - 1) & !((2 << 20) - 1);
    let run = Arc::new(TablesRun { root, base, walkers, arrived: AtomicU32::new(0), failed: AtomicBool::new(false) });
    let mut threads = Vec::new();
    for me in 0..walkers {
        let (run, out) = (run.clone(), Arc::new(ksync::SpinLock::named("mm.ktest.tables", None)));
        let o = out.clone();
        let opts = sched::SpawnOpts { cpu: Some(me as usize), pin: true, rt_prio: sched::RT_PRIO_DEFAULT };
        let h = sched::spawn_with("mm-tables", sched::Class::Normal, opts, move || {
            let res = run.walk(me);
            if res.is_err() {
                run.failed.store(true, Ordering::Release);
            }
            *o.lock() = Some(res);
        })
        .map_err(|e| alloc::format!("spawn on cpu {me}: {e:?}"))?;
        threads.push((h, out));
    }
    let mut rounds = 0;
    let mut first = None;
    for (h, out) in threads {
        kassert!(h.join_timeout(30_000_000_000), "a walker did not finish in 30 s");
        match out.lock().take() {
            Some(Ok(n)) => rounds += n,
            Some(Err(e)) => first = first.or(Some(e)),
            None => first = first.or(Some(String::from("a walker gave no result"))),
        }
    }
    sched::reap();
    kassert!(paging::release_kernel_va(va, len), "the range stayed out of use");
    if let Some(e) = first {
        return Err(e);
    }
    klog::event!(Info, "mm", "ktest.tables_smp", walkers = walkers, new_regions = TABLE_CHUNKS, emptied_rounds = rounds);
    Ok(())
}
ktest!("mm.paging.kernel_tables_smp", Safe, tables_smp);

const M2: u64 = 2 << 20;
const G1: u64 = 1 << 30;

fn uncached() -> PageFlags {
    PageFlags::kernel_data().with_cache(CacheType::Uncached)
}

/// The kernel half's other walkers while `unmap_4g_under_load` runs: each takes a page in and out of its CPU's 2 MiB
/// region, so it wants `TABLES` and sends a shootdown round after round.
struct TablesLoad {
    root: u64,
    base: u64,
    started: AtomicU32,
    stop: AtomicBool,
    /// Calls that got through `TABLES` and returned.
    entered: AtomicU64,
}

impl TablesLoad {
    /// Round by round in turn: install and empty a page table; or keep an anchor page, so the table stays and no frame
    /// is needed, and wait for `TABLES` with interrupts off, where only `tlb::poll` in that spin answers the others.
    fn run(&self) -> Result<u64, String> {
        let region = self.base + hal::percpu::this_cpu() as u64 * M2;
        let (anchor, page) = (region, region + 0x1000);
        let err = |w: &str, e: paging::MapError| alloc::format!("{w} {page:#x}: {e:?}");
        self.started.fetch_add(1, Ordering::AcqRel);
        let until = time::now_ns() + 20_000_000_000;
        let mut rounds = 0;
        while !self.stop.load(Ordering::Acquire) {
            kassert!(time::now_ns() < until, "no stop in 20 s");
            let entered = || {
                self.entered.fetch_add(1, Ordering::Relaxed);
            };
            if rounds % 2 == 0 {
                paging::map_4k(self.root, page, HPET, uncached(), &mut mm_core::alloc_table_frame).map_err(|e| err("map", e))?;
                entered();
                paging::unmap_kernel(page, 0x1000, &mut |_| {}).map_err(|e| err("unmap", e))?;
            } else {
                paging::map_kernel(self.root, anchor, HPET, PageSize::Size4K, uncached()).map_err(|e| err("anchor", e))?;
                entered();
                let mapped = {
                    let _irq = hal::cpu::IrqGuard::new();
                    paging::map_kernel(self.root, page, HPET, PageSize::Size4K, uncached())
                };
                mapped.map_err(|e| err("masked map", e))?;
                entered();
                paging::unmap_kernel(page, 0x1000, &mut |_| {}).map_err(|e| err("unmap", e))?;
                paging::unmap_kernel(anchor, 0x1000, &mut |_| {}).map_err(|e| err("anchor", e))?;
            }
            entered();
            rounds += 1;
        }
        Ok(rounds)
    }
}

/// What `unmap_4g` saw of the walk, in TSC ticks. The callback runs under `TABLES`, so from a table's first leaf to its
/// last is a stretch with interrupts off, without what a slice does before and after (a path down, a read of the two
/// tables to see whether they are empty: 1024 reads at most, about a microsecond).
#[derive(Default)]
struct SliceTimes {
    start: u64,
    prev_end: u64,
    slices: u64,
    longest: u64,
    sum: u64,
    longest_gap: u64,
    irqs_on: u64,
    over_100us: u64,
    /// `TablesLoad::entered` at the first and the last leaf.
    entered: [u64; 2],
}

impl SliceTimes {
    fn leaf(&mut self, virt: u64, ticks_100us: u64, entered: &AtomicU64) {
        let off = virt & (M2 - 1);
        if off == 0 {
            if self.slices == 0 {
                self.entered[0] = entered.load(Ordering::Relaxed);
            }
            self.start = hal::cpu::rdtsc_ordered();
            self.irqs_on += hal::cpu::interrupts_enabled() as u64;
            if self.prev_end != 0 {
                self.longest_gap = self.longest_gap.max(self.start - self.prev_end);
            }
        } else if off == M2 - 0x1000 {
            self.prev_end = hal::cpu::rdtsc_ordered();
            let held = self.prev_end - self.start;
            self.longest = self.longest.max(held);
            self.sum += held;
            self.slices += 1;
            self.over_100us += (held > ticks_100us) as u64;
            self.entered[1] = entered.load(Ordering::Relaxed);
        }
    }
}

/// PC-W31: 4 GiB of 4 KiB leaves (the worst case: 2048 page tables to clear) taken out while other CPUs want the same
/// lock and send shootdowns. Every shootdown must be answered, no table kept out of use, and the load must get through
/// `TABLES` during the walk: one lock over the walk lets each thread finish at most the one call it was in. A vCPU the
/// host has taken away can sit out one walk, not five. The interrupts-off time of a slice goes into the log (judged on the
/// Mini-PC, not here). What maps it is short pieces too, and the load lasts only as long as the walk: nothing here holds a
/// CPU for long. A shootdown answered by a third CPU that waits for the lock takes three CPUs: in `-smp 8` and on the
/// Mini-PC; `waiter_answers_shootdown` proves the mechanism with two.
fn unmap_4g_under_load() -> TestResult {
    let root = root()?;
    sched::wait_cpus_online(5_000_000_000);
    let load = hal::percpu::cpu_count().saturating_sub(1).min(7) as u32;
    if load == 0 {
        return ktest::fail("needs two CPUs: one unmaps, one waits for the lock and sends shootdowns");
    }
    let len = 5 * G1;
    let va = paging::reserve_kernel_va(len).ok_or("no kernel VA")?;
    let load_len = (hal::percpu::cpu_count() as u64 + 1) * M2;
    let Some(load_va) = paging::reserve_kernel_va(load_len) else {
        kassert!(paging::release_kernel_va(va, len), "the range stayed out of use");
        return ktest::fail("no kernel VA for the load");
    };
    let mut out = Ok(0);
    for _ in 0..5 {
        out = unmap_4g(root, (va + G1 - 1) & !(G1 - 1), (load_va + M2 - 1) & !(M2 - 1), load);
        if out.as_ref().map_or(true, |&got_in| got_in > load as u64) {
            break;
        }
    }
    let back = paging::release_kernel_va(va, len) && paging::release_kernel_va(load_va, load_len);
    let got_in = out?;
    kassert!(got_in > load as u64, "{got_in} calls of {load} load threads got through TABLES during the walk, in five walks");
    kassert!(back, "a range stayed out of use");
    Ok(())
}
ktest!("mm.paging.unmap_4g_under_load", Safe, unmap_4g_under_load);

/// Returns how many calls of the load got through `TABLES` during the walk.
fn unmap_4g(root: u64, base: u64, load_base: u64, load: u32) -> Result<u64, String> {
    const LEN: u64 = 4 << 30;
    let t0 = time::now_ns();
    for i in 0..LEN / 0x1000 {
        paging::map_kernel(root, base + i * 0x1000, HPET, PageSize::Size4K, uncached()).map_err(|e| alloc::format!("map {i}: {e:?}"))?;
    }
    let map_ns = time::now_ns() - t0;
    let here = hal::percpu::this_cpu();
    let run = Arc::new(TablesLoad { root, base: load_base, started: AtomicU32::new(0), stop: AtomicBool::new(false), entered: AtomicU64::new(0) });
    let mut threads = Vec::new();
    let mut spawn_error = None;
    for cpu in (0..hal::percpu::cpu_count()).filter(|&c| c != here).take(load as usize) {
        let (r, out) = (run.clone(), Arc::new(ksync::SpinLock::named("mm.ktest.tables", None)));
        let o = out.clone();
        let opts = sched::SpawnOpts { cpu: Some(cpu), pin: true, rt_prio: sched::RT_PRIO_DEFAULT };
        match sched::spawn_with("mm-tables", sched::Class::Normal, opts, move || {
            let res = r.run();
            *o.lock() = Some(res);
        }) {
            Ok(h) => threads.push((h, out)),
            Err(e) => {
                spawn_error = Some(alloc::format!("spawn on cpu {cpu}: {e:?}"));
                break;
            }
        }
    }
    let until = time::now_ns() + 5_000_000_000;
    while spawn_error.is_none() && run.started.load(Ordering::Acquire) < threads.len() as u32 && time::now_ns() < until {
        sched::yield_now();
    }
    let started = run.started.load(Ordering::Acquire);
    let before = paging::tlb::stats();
    let mut times = SliceTimes::default();
    let hz = time::tsc_hz();
    let ticks_100us = hz / 10_000;
    let t0 = time::now_ns();
    let st = paging::unmap_kernel(base, LEN, &mut |u| times.leaf(u.virt, ticks_100us, &run.entered));
    let total_ns = time::now_ns() - t0;
    run.stop.store(true, Ordering::Release);
    let mut rounds = 0;
    let mut first = spawn_error;
    for (h, out) in threads {
        if !h.join_timeout(30_000_000_000) {
            first = first.or(Some(String::from("a load thread did not finish in 30 s")));
            continue;
        }
        match out.lock().take() {
            Some(Ok(n)) => rounds += n,
            Some(Err(e)) => first = first.or(Some(e)),
            None => first = first.or(Some(String::from("a load thread gave no result"))),
        }
    }
    sched::reap();
    let after = paging::tlb::stats();
    let st = st.map_err(|e| alloc::format!("unmap: {e:?}"))?;
    if let Some(e) = first {
        return Err(e);
    }
    kassert!(started == load, "{started} of {load} load threads started");
    kassert!(st.leaves == LEN / 0x1000 && st.tables_freed == (LEN >> 21) + (LEN >> 30), "{st:?}");
    kassert!(times.slices == LEN >> 21, "{} slices", times.slices);
    kassert!(times.irqs_on == 0, "interrupts on under the lock in {} slices", times.irqs_on);
    kassert!(rounds > 0, "the load never ran");
    kassert!(after.kept_tables == before.kept_tables, "{} tables kept out of use", after.kept_tables - before.kept_tables);
    kassert!(after.unanswered == before.unanswered, "{} CPUs did not answer a shootdown", after.unanswered - before.unanswered);
    kassert!(after.give_ups == before.give_ups, "{} shootdowns given up", after.give_ups - before.give_ups);
    let ns = |t| paging::tlb::ticks_to_ns(t, hz);
    let got_in = times.entered[1] - times.entered[0];
    klog::event!(Info, "mm", "ktest.unmap_4g", slices = times.slices, longest_slice_ns = ns(times.longest), all_slices_ns = ns(times.sum),
        slices_over_100us = times.over_100us, longest_gap_ns = ns(times.longest_gap), unmap_ns = total_ns, map_ns = map_ns, load_threads = load,
        load_rounds = rounds, got_in_during_walk = got_in, resends = after.resends - before.resends;
        "longest_slice is the longest time with interrupts off; all_slices is what one lock over the whole walk would have held");
    Ok(got_in)
}

/// A pinned thread, with its result to fetch.
fn pinned_thread(cpu: usize, f: impl FnOnce() -> TestResult + Send + 'static) -> Result<impl FnOnce() -> TestResult, String> {
    let out = Arc::new(ksync::SpinLock::named("mm.ktest.tables", None));
    let o = out.clone();
    let opts = sched::SpawnOpts { cpu: Some(cpu), pin: true, rt_prio: sched::RT_PRIO_DEFAULT };
    let h = sched::spawn_with("mm-tables", sched::Class::Normal, opts, move || {
        let res = f();
        *o.lock() = Some(res);
    })
    .map_err(|e| alloc::format!("spawn on cpu {cpu}: {e:?}"))?;
    Ok(move || {
        kassert!(h.join_timeout(30_000_000_000), "a thread did not finish in 30 s");
        sched::reap();
        let res = out.lock().take();
        res.unwrap_or_else(|| ktest::fail("a thread gave no result"))
    })
}

/// PC-W31: a CPU that waits for `TABLES` with interrupts off answers a shootdown, or the holder and every sender would
/// wait for it while it waits for them. Here the holder itself sends one from under the lock, which real code never does
/// (the callback "must not wait") but which puts the waiter in the one place that can answer: it masked its interrupts
/// before the holder published anything and polls nowhere else. The holder's wait is 100 ms at most, on a thread that is
/// spinning anyway. Two CPUs are enough, so this runs in the plain gate.
fn waiter_answers_shootdown() -> TestResult {
    let root = root()?;
    sched::wait_cpus_online(5_000_000_000);
    let cpus: Vec<usize> = (0..hal::percpu::cpu_count()).collect();
    let [holder_cpu, waiter_cpu, ..] = cpus[..] else {
        return ktest::fail("needs two CPUs: one holds the lock, one waits with interrupts off");
    };
    let va = paging::reserve_kernel_va(2 * M2).ok_or("no kernel VA")?;
    let anchor = (va + M2 - 1) & !(M2 - 1);
    let (held, wanted) = (anchor + 0x1000, anchor + 0x2000);
    let out = (|| -> TestResult {
        let e = |e: paging::MapError| alloc::format!("{e:?}");
        // The anchor keeps the page table, so the waiter's walk needs no frame.
        paging::map_kernel(root, anchor, HPET, PageSize::Size4K, uncached()).map_err(e)?;
        paging::map_kernel(root, held, HPET, PageSize::Size4K, uncached()).map_err(e)?;
        // 1: the waiter runs, 2: the holder has the lock, 3: the waiter is masked.
        let step = Arc::new(AtomicU32::new(0));
        let s = step.clone();
        let waiter = pinned_thread(waiter_cpu, move || {
            let until = time::now_ns() + 5_000_000_000;
            s.store(1, Ordering::Release);
            while s.load(Ordering::Acquire) < 2 {
                kassert!(time::now_ns() < until, "the holder did not take the lock in 5 s");
                core::hint::spin_loop();
            }
            let mapped = {
                let _irq = hal::cpu::IrqGuard::new();
                s.store(3, Ordering::Release);
                paging::map_kernel(root, wanted, HPET, PageSize::Size4K, uncached())
            };
            mapped.map(|_| ()).map_err(|e| alloc::format!("masked map: {e:?}"))
        })?;
        let s = step.clone();
        let holder = pinned_thread(holder_cpu, move || {
            let until = time::now_ns() + 5_000_000_000;
            while s.load(Ordering::Acquire) < 1 {
                kassert!(time::now_ns() < until, "the waiter did not start in 5 s");
                sched::yield_now();
            }
            let mut answer = None;
            let st = paging::unmap_kernel(held, 0x1000, &mut |_| {
                s.store(2, Ordering::Release);
                let until = time::now_ns() + 100_000_000;
                while s.load(Ordering::Acquire) < 3 && time::now_ns() < until {
                    paging::tlb::poll();
                    core::hint::spin_loop();
                }
                if s.load(Ordering::Acquire) == 3 {
                    answer = Some(paging::tlb::flush_kernel(held, held + 0x1000, false));
                }
            });
            kassert!(st.is_ok(), "{st:?}");
            let answer = answer.ok_or("the waiter did not mask in 100 ms")?;
            kassert!(answer.confirmed(), "{} CPUs did not answer, the waiter's spin among them: {answer:?}", answer.unanswered);
            Ok(())
        })?;
        let (w, h) = (waiter(), holder());
        h.and(w)?;
        kassert!(paging::translate(root, wanted).is_some(), "the waiter's map did not arrive");
        Ok(())
    })();
    let mut gone = true;
    for page in [wanted, held, anchor] {
        gone &= paging::unmap_kernel(page, 0x1000, &mut |_| {}).is_ok();
    }
    let back = paging::release_kernel_va(va, 2 * M2);
    out?;
    kassert!(gone && back, "the range stayed out of use");
    Ok(())
}
ktest!("mm.paging.waiter_answers_shootdown", Safe, waiter_answers_shootdown);

/// PC-W31: a 1 GiB leaf comes off in one piece, with what lies next to it, and part of it is still refused. The leaf maps
/// the GiB that holds the HPET, uncached and never touched: no RAM gets an alias, no access happens.
fn unmap_gib_leaf() -> TestResult {
    let root = root()?;
    let len = 3 * G1;
    let va = paging::reserve_kernel_va(len).ok_or("no kernel VA")?;
    let out = (|| -> TestResult {
        let base = (va + G1 - 1) & !(G1 - 1);
        let e = |e: paging::MapError| alloc::format!("{e:?}");
        paging::map_kernel(root, base, HPET & !(G1 - 1), PageSize::Size1G, uncached()).map_err(e)?;
        paging::map_kernel(root, base + G1, HPET & !(M2 - 1), PageSize::Size2M, uncached()).map_err(e)?;
        paging::map_kernel(root, base + G1 + M2, HPET, PageSize::Size4K, uncached()).map_err(e)?;
        use paging::MapError::PartialHugePage;
        kassert!(paging::unmap_kernel(base + M2, 0x1000, &mut |_| {}) == Err(PartialHugePage), "part of the 1 GiB leaf");
        kassert!(paging::unmap_kernel(base, G1 + 0x1000, &mut |_| {}) == Err(PartialHugePage), "part of the 2 MiB leaf at the end");
        kassert!(paging::translate(root, base + M2).is_some() && paging::translate(root, base + G1 + M2).is_some(), "a refusal changed something");
        // On the stack: the callback runs under `TABLES`, no heap there.
        let mut sizes = [None; 3];
        let mut seen = 0;
        let st = paging::unmap_kernel(base, G1 + 2 * M2, &mut |u| {
            if let Some(s) = sizes.get_mut(seen) {
                *s = Some(u.size);
            }
            seen += 1;
        })
        .map_err(e)?;
        kassert!(seen == 3 && sizes == [Some(PageSize::Size1G), Some(PageSize::Size2M), Some(PageSize::Size4K)], "{seen} leaves: {sizes:?}");
        kassert!(st.leaves == 3 && st.tables_freed == 2, "{st:?}");
        kassert!(paging::translate(root, base).is_none() && paging::translate(root, base + G1 + M2).is_none());
        Ok(())
    })();
    let back = paging::release_kernel_va(va, len);
    out?;
    kassert!(back, "the range stayed out of use");
    Ok(())
}
ktest!("mm.paging.unmap_gib_leaf", Safe, unmap_gib_leaf);

/// PC-W31: once the table hook is in, a kernel-half table comes from it, whatever source the caller brought, so a frame
/// left over or a table freed later goes back as what it is.
fn map_4k_table_source() -> TestResult {
    let root = root()?;
    let len = 2 * M2;
    let va = paging::reserve_kernel_va(len).ok_or("no kernel VA")?;
    let page = (va + M2 - 1) & !(M2 - 1);
    let mut asked = 0;
    let mapped = paging::map_4k(root, page, HPET, uncached(), &mut || {
        asked += 1;
        None
    });
    let unmapped = paging::unmap_kernel(page, 0x1000, &mut |_| {});
    let back = paging::release_kernel_va(va, len);
    kassert!(mapped.is_ok(), "{mapped:?}");
    kassert!(asked == 0, "the caller's source was asked {asked} times");
    kassert!(unmapped.is_ok_and(|s| s.leaves == 1 && s.tables_freed >= 1), "{unmapped:?}");
    kassert!(back, "the range stayed out of use");
    Ok(())
}
ktest!("mm.paging.map_4k_table_source", Safe, map_4k_table_source);

fn table_frames() -> u64 {
    mm_core::frames().map_or(0, |f| f.stats().used_by_kind[mm_core::Kind::PageTable as usize])
}

fn unmap_frees_tables() -> TestResult {
    root()?;
    let before = (table_frames(), vmap::stats().ok_or("no vmap")?);
    let mut v = Vec::new();
    for _ in 0..600 {
        v.push(vmap::map_device(HPET, 0x1000, CacheType::Uncached).map_err(|e| alloc::format!("{e:?}"))?);
    }
    let mid = table_frames();
    kassert!(mid > before.0, "600 windows took no page-table frame");
    let mut virts: Vec<u64> = v.iter().map(|w| w.virt()).collect();
    virts.sort_unstable();
    kassert!(virts.windows(2).all(|w| w[1] - w[0] >= 0x2000), "two windows without a guard page between");
    drop(v);
    let after = (table_frames(), vmap::stats().ok_or("no vmap")?);
    kassert!(after.0 == before.0, "page-table frames {} before, {} after (peak {mid})", before.0 >> 12, after.0 >> 12);
    kassert!(after.1.free_bytes == before.1.free_bytes && after.1.windows == before.1.windows, "vmap space not given back");
    kassert!(after.1.pages_4k == before.1.pages_4k && after.1.pages_2m == before.1.pages_2m,
        "page counters {}/{} before, {}/{} after", before.1.pages_4k, before.1.pages_2m, after.1.pages_4k, after.1.pages_2m);
    kassert!(after.1.quarantined_windows == 0, "{} windows quarantined", after.1.quarantined_windows);
    klog::event!(Info, "mm", "ktest.vmap_tables", peak_tables = (mid - before.0) >> 12, free_ranges = after.1.free_ranges);
    Ok(())
}
ktest!("mm.vmap.unmap_frees_tables", Safe, unmap_frees_tables);

fn refusals() -> TestResult {
    root()?;
    let ram = kernel_tables::ram();
    let p = ram.last().map(|x| x.1 - 0x1000).ok_or("no RAM")?;
    kassert!(matches!(vmap::map_device(p, 0x1000, CacheType::WriteCombining), Err(VmapError::OverlapsRam)), "a WC window over RAM");
    kassert!(matches!(vmap::map_device(HPET, 0, CacheType::Uncached), Err(VmapError::BadRange)));
    kassert!(matches!(vmap::map_device(u64::MAX - 0x10, 0x100, CacheType::Uncached), Err(VmapError::BadRange)));
    Ok(())
}
ktest!("mm.vmap.refusals", Safe, refusals);

/// Rewrites the bottom MiB of the screen with its own pixels through the WC mapping the console uses and
/// reports the time per MiB. QEMU only proves it works; the Mini-PC baseline is 0.10-0.13 ms/MB (25x UC).
fn fb_wc_write_speed() -> TestResult {
    let Some((virt, phys, size)) = kernel_tables::framebuffer() else {
        return ktest::fail("no GOP framebuffer");
    };
    let fb = bootinfo::Framebuffer {
        phys, virt, size, width: 0, height: 0, stride: 0, format: bootinfo::PixelFormat::Bgrx, masks: [0; 4], mode_index: 0,
    };
    let io = hal::mmio::framebuffer(&fb).ok_or("framebuffer window")?;
    let n = (1usize << 20).min(size as usize) & !3;
    let off = (size as usize - n) & !3;
    // Read once (slow on hardware, ~0.1 s), so the timed writes put back what is on the screen.
    let mut buf = Vec::with_capacity(n);
    for i in (0..n).step_by(4) {
        buf.extend_from_slice(&io.read32(off + i).map_err(|e| alloc::format!("{e:?}"))?.to_le_bytes());
    }
    let mut best = u64::MAX;
    for _ in 0..5 {
        let t0 = time::now_ns();
        io.write_bytes(off, &buf).map_err(|e| alloc::format!("{e:?}"))?;
        best = best.min(time::now_ns() - t0);
    }
    let per_mb = best * (1 << 20) / n as u64;
    klog::event!(Info, "mm", "ktest.fb_wc_write", bytes = n, best_ns = best, us_per_mb = per_mb / 1000;
        "Mini-PC baseline 100-130 us/MB with WC (UC ~3200)");
    Ok(())
}
ktest!("mm.vmap.fb_wc_write_speed", Safe, fb_wc_write_speed);

/// hal's kernel-root primitives refuse what would break the live tables.
fn primitive_refusals() -> TestResult {
    let r = root()?;
    use hal::paging::{MapError, SwitchError};
    let text = text_target as fn() -> u64 as usize as u64;
    kassert!(paging::map_kernel(r, DIRECT_MAP_BASE + (1 << 46), 0, PageSize::Size4K, PageFlags::kernel_data()) == Err(MapError::WrongRange),
        "live root mapped outside vmap");
    kassert!(paging::unmap_kernel(text & !0xFFF, 0x1000, &mut |_| {}) == Err(MapError::WrongRange), "kernel text unmapped");
    kassert!(paging::switch_kernel_root(r ^ 0x1000) == Err(SwitchError::NotBuilt));
    kassert!(paging::free_unused_root(r) == Err(MapError::WrongRange), "the live root freed");
    kassert!(paging::translate(r, text).is_some(), "text still mapped");
    // One-shot hooks: a second install changes nothing.
    kassert!(!paging::set_table_frames(mm_core::alloc_table_frame, mm_core::free_table_frame), "table frame source replaced");
    fn dummy(_: u64, _: u64, _: CacheType) -> Result<(u64, alloc::boxed::Box<dyn core::any::Any + Send + Sync>), VmapError> {
        Err(VmapError::NotReady)
    }
    kassert!(!paging::set_device_mapper(dummy), "device mapper replaced");
    kassert!(probe::read_unmapped(0x0000_8000_0000_0000) == Err(probe::Refused::NonCanonical), "non-canonical probe");
    // A root where the first image page (text) lost its execute right: the switch must refuse it.
    let (img, _) = paging::kernel_image();
    let page = img & !0xFFF;
    let (phys, fl) = paging::translate(r, page).ok_or("image start unmapped")?;
    kassert!(!fl.contains(PageFlags::NX), "first image page is not executable");
    let cand = paging::new_kernel_root().map_err(|e| alloc::format!("{e:?}"))?;
    let mapped = paging::map_kernel(cand, page, phys & !0xFFF, PageSize::Size4K, PageFlags(PageFlags::kernel_data().0 | PageFlags::GLOBAL));
    let sw = paging::switch_kernel_root(cand);
    let freed = paging::free_unused_root(cand);
    kassert!(mapped.is_ok() && freed.is_ok(), "candidate root: map {mapped:?}, free {freed:?}");
    kassert!(sw == Err(SwitchError::RightsMismatch(page)), "switch to a root with NX text: {sw:?}");
    kassert!(paging::kernel_root() == Some(r) && root()? == r, "kernel root changed");
    Ok(())
}
ktest!("mm.paging.refusals", Safe, primitive_refusals);
