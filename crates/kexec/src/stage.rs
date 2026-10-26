//! Staging a new kernel in the running one: fetch (NW-3's HTTP GET), check, place it in frames the running
//! kernel's allocator hands out, build its page tables and BootInfo. Nothing of the running kernel changes until
//! `Staged::jump`; dropping a `Staged` gives every frame back.

use crate::elf::{ElfError, ElfStream, ImageSink, Layout, PF_W, PF_X};
use crate::handover::{self, NewBoot};
use crate::memmap::{self, Mark};
use crate::tables::{self, Perm, Tables};
use crate::url::Bundle;
use crate::{Fnv, boot};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use bootinfo::layout::{DIRECT_MAP_BASE, KERNEL_BASE, VMAP_BASE};
use bootinfo::{KernelImage, PageTables, PixelFormat, memtype};
use hal::kexec_stub;
use mm_core::{Kind, Zone};
use net::http::{self, HttpError, Options};

const PAGE: u64 = 4096;
/// Memory the running kernel keeps for itself while it stages (it goes on if Reload is refused): a bound on
/// what Reload may take, not a count.
pub const RESERVE: u64 = 8 << 20;
/// The new kernel seeds its heap from its largest free descriptor and needs 4 x 8 MiB there
/// (`kernel/src/main.rs`); Reload asks for twice that so the new kernel has room to start.
pub const MIN_LARGEST_FREE: u64 = 64 << 20;
/// What a Reload needs before it fetches anything: the fetch's socket buffers (~1.4 MiB, NW-3) and the smallest
/// image. Below this it is refused before a connection is opened.
pub const START_NEED: u64 = 4 << 20;
/// Hostile-input bound: the kernel file (mostly debug info, never stored) may be at most this long.
pub const KERNEL_FILE_MAX: u64 = 1 << 30;
const STACK_ORDER: u32 = 2;
const PIECE: usize = 1 << 20;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StageError {
    /// Refused before anything was fetched (the caller's fault or a rule).
    Refused(String),
    /// Fetching failed (network, HTTP status).
    Fetch(String),
    /// The fetched bytes are not a kernel this machine can start, or do not fit.
    Bad(String),
    /// Something of the running kernel is missing (BootInfo, stub page, ...).
    NotReady(String),
}

impl StageError {
    pub fn text(&self) -> &str {
        match self {
            StageError::Refused(s) | StageError::Fetch(s) | StageError::Bad(s) | StageError::NotReady(s) => s,
        }
    }
}

/// Frames taken for the new kernel; given back on drop unless `forget` (the jump).
#[derive(Default)]
pub struct Frames {
    blocks: Vec<(u64, u32)>,
}

impl Frames {
    pub fn alloc_order(&mut self, order: u32) -> Option<u64> {
        let p = mm_core::frames()?.alloc(order, Zone::Normal, Kind::Other)?;
        self.blocks.push((p, order));
        Some(p)
    }
    /// A contiguous block of at least `bytes`.
    pub fn alloc_bytes(&mut self, bytes: u64) -> Option<u64> {
        self.alloc_order(order_for(bytes))
    }
    pub fn total(&self) -> u64 {
        self.blocks.iter().map(|&(_, o)| PAGE << o).sum()
    }
    fn forget(&mut self) {
        self.blocks.clear();
    }
}

impl Drop for Frames {
    fn drop(&mut self) {
        if let Some(f) = mm_core::frames() {
            for &(p, o) in &self.blocks {
                f.free(p, o, Kind::Other);
            }
        }
    }
}

pub fn order_for(bytes: u64) -> u32 {
    let pages = bytes.div_ceil(PAGE).max(1);
    if pages <= 1 { 0 } else { 64 - (pages - 1).leading_zeros() }
}

/// Free bytes the frame allocator has now, minus what the running kernel keeps.
pub fn budget() -> u64 {
    mm_core::frames().map(|f| f.stats().free_bytes).unwrap_or(0).saturating_sub(RESERVE)
}

fn no_memory(what: &str, bytes: u64, budget: u64) -> String {
    format!(
        "does not fit: {what} needs {} KiB in one piece and the running kernel can spare {} KiB (free memory beyond its {} MiB reserve)",
        bytes.div_ceil(1024),
        budget / 1024,
        RESERVE >> 20
    )
}

/// The new kernel's image: one contiguous block, zeroed, segments written as they stream in.
struct KernelSink<'a> {
    frames: &'a mut Frames,
    block: Option<(u64, u64)>,
    budget: u64,
}

impl ImageSink for KernelSink<'_> {
    fn layout(&mut self, l: &Layout) -> Result<(), String> {
        let need = PAGE << order_for(l.span);
        // Live: the fetch's socket buffers came from the same pool after the budget was taken.
        let left = left_now(self.budget, self.frames);
        if need > left {
            return Err(no_memory("the kernel image", l.span, left));
        }
        let p = self.frames.alloc_bytes(l.span).ok_or_else(|| no_memory("the kernel image", l.span, left))?;
        kexec_stub::zero_phys(p, l.span);
        self.block = Some((p, l.span));
        Ok(())
    }
    fn write(&mut self, off: u64, data: &[u8]) -> Result<(), String> {
        let (p, span) = self.block.ok_or("no image block")?;
        if off.checked_add(data.len() as u64).is_none_or(|e| e > span) {
            return Err(String::from("a segment write past the image"));
        }
        kexec_stub::write_phys(p + off, data);
        Ok(())
    }
}

pub(crate) fn http_text(what: &str, url: &str, e: &HttpError) -> String {
    match e {
        HttpError::Status(c) => format!("{what}: {url} answered HTTP {c}"),
        HttpError::TooLarge { limit } => format!("{what}: {url} is too big: more than {limit} bytes (the limit is what the running kernel can spare, or the file bound)"),
        HttpError::Connect(n) => format!("{what}: cannot connect to {url}: {n:?}"),
        HttpError::TimedOut => format!("{what}: {url} timed out"),
        e => format!("{what}: {url}: {e:?}"),
    }
}

pub(crate) fn options(limit: u64, deadline_ns: u64) -> Options {
    let mut o = Options::new(limit);
    let left = deadline_ns.saturating_sub(time::now_ns());
    o.timeout_ns = left.saturating_sub(500_000_000).max(1_000_000_000);
    o.idle_ns = o.idle_ns.min(o.timeout_ns);
    o
}

/// One file of a bundle: a URL (over http from the LAN) or a generation's file on the boot stick (RB-9).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FileRef {
    Url(String),
    Stick { generation: u32, name: &'static str },
}

impl FileRef {
    pub fn label(&self) -> String {
        match self {
            FileRef::Url(u) => u.clone(),
            FileRef::Stick { generation, name } => format!("stick \\BITSOS2\\SYS\\{generation}\\{name}"),
        }
    }
}

enum ReadError {
    /// The sink refused (out of memory, a bad ELF).
    Sink,
    Failed(StageError),
}

/// Streams `f` to `sink`, at most `limit` bytes, until `deadline_ns`. Returns the byte count.
fn read_file(what: &str, f: &FileRef, limit: u64, deadline_ns: u64, sink: &mut dyn FnMut(&[u8]) -> Result<(), ()>) -> Result<u64, ReadError> {
    let mut bytes = 0u64;
    match f {
        FileRef::Url(url) => {
            let sock = net::stack().ok_or_else(|| ReadError::Failed(StageError::NotReady(String::from("no network stack"))))?;
            http::get_to(sock, url, &options(limit, deadline_ns), &mut |d| {
                bytes += d.len() as u64;
                sink(d)
            })
            .map_err(|e| match e {
                HttpError::Sink => ReadError::Sink,
                e => ReadError::Failed(StageError::Fetch(http_text(what, url, &e))),
            })?;
        }
        FileRef::Stick { generation, name } => {
            let label = f.label();
            let failed = |t: String| ReadError::Failed(StageError::Fetch(t));
            // Piece by piece from the stick (never the whole file in RAM: the sink places it); the deadline is
            // checked between pieces, each piece is one bounded USB transfer chain.
            let mut buf = Vec::new();
            buf.try_reserve_exact(PIECE).map_err(|_| ReadError::Failed(StageError::Bad(no_memory("a read buffer", PIECE as u64, budget()))))?;
            buf.resize(PIECE, 0);
            let r = fs_fat::stick::with_sys(|s| -> Result<(), ReadError> {
                loop {
                    if time::now_ns() >= deadline_ns {
                        return Err(failed(format!("{what}: {label}: timed out after {} bytes", bytes)));
                    }
                    let got = s.read_at(*generation, name, bytes, &mut buf).map_err(|e| failed(format!("{what}: {label}: {}", crate::install::errno_name(e))))?;
                    if got == 0 {
                        return Ok(());
                    }
                    if bytes + got as u64 > limit {
                        return Err(ReadError::Failed(StageError::Bad(format!(
                            "{what}: {label} is too big: more than {limit} bytes (the limit is what the running kernel can spare, or the file bound)"
                        ))));
                    }
                    bytes += got as u64;
                    sink(&buf[..got]).map_err(|()| ReadError::Sink)?;
                }
            });
            match r {
                Ok(r) => r?,
                Err(e) => return Err(ReadError::Failed(StageError::NotReady(format!("{what}: {label}: {}", crate::install::errno_name(e))))),
            }
        }
    }
    Ok(bytes)
}

/// Streams the kernel ELF into a new image block.
/// Returns the layout, the image's frames, the file's bytes and its kernel id (FNV-1a 64 of the file).
fn fetch_kernel(f: &FileRef, frames: &mut Frames, deadline_ns: u64, budget: u64) -> Result<(Layout, u64, u64, u64), StageError> {
    let label = f.label();
    let mut stream = ElfStream::new();
    let mut sink = KernelSink { frames, block: None, budget };
    let mut err: Option<ElfError> = None;
    let mut fnv = Fnv::new();
    let r = read_file("kernel", f, KERNEL_FILE_MAX, deadline_ns, &mut |d| {
        fnv.update(d);
        stream.feed(d, &mut sink).map_err(|e| err = Some(e))
    });
    if let Some(e) = err {
        return Err(StageError::Bad(format!("kernel {label}: {}", e.describe())));
    }
    let bytes = match r {
        Ok(b) => b,
        Err(ReadError::Sink) => return Err(StageError::Bad(format!("kernel {label}: refused while it was read"))),
        Err(ReadError::Failed(e)) => return Err(e),
    };
    let l = stream.finish().map_err(|e| StageError::Bad(format!("kernel {label}: {}", e.describe())))?;
    let (phys, _) = sink.block.ok_or_else(|| StageError::Bad(format!("kernel {label}: no image")))?;
    Ok((l, phys, bytes, fnv.finish()))
}

/// What Reload may still take now: `cap` (its total bound) minus what it took already, and never more than the
/// live free memory beyond the reserve. The G0 heap takes its big blocks from the same frame pool, so the live
/// figure already counts the download pieces held on the heap.
fn left_now(cap: u64, frames: &Frames) -> u64 {
    cap.saturating_sub(frames.total()).min(budget())
}

/// Fetches the initramfs into pieces, then into one contiguous block (the MODULE record is one range).
/// Returns `(phys, size, fnv)`.
fn fetch_initrd(f: &FileRef, frames: &mut Frames, deadline_ns: u64, cap: u64) -> Result<(u64, u64, u64), StageError> {
    let label = f.label();
    // Pieces and the block both come from what is left after the kernel image: at most half of it each, so the
    // running kernel's reserve stays untouched while the net, MCP and klog threads go on allocating.
    let avail = left_now(cap, frames);
    let limit = avail / 2;
    let mut pieces: Vec<Vec<u8>> = Vec::new();
    let mut total = 0u64;
    let mut fnv = Fnv::new();
    read_file("initramfs", f, limit, deadline_ns, &mut |mut d: &[u8]| {
        fnv.update(d);
        total += d.len() as u64;
        while !d.is_empty() {
            if pieces.last().is_none_or(|p| p.len() == PIECE) {
                let mut v = Vec::new();
                v.try_reserve_exact(PIECE).map_err(|_| ())?;
                pieces.push(v);
            }
            let p = pieces.last_mut().ok_or(())?;
            let n = (PIECE - p.len()).min(d.len());
            p.extend_from_slice(&d[..n]);
            d = &d[n..];
        }
        Ok(())
    })
    .map_err(|e| match e {
        ReadError::Sink => StageError::Bad(no_memory("the initramfs download", total, avail)),
        ReadError::Failed(e) => e,
    })?;
    if total == 0 {
        return Err(StageError::Bad(format!("initramfs {label} is empty")));
    }
    let size = total.next_multiple_of(PAGE);
    // The block is a power-of-two allocation (that much is taken); checked against what is left with the
    // pieces still alive.
    let left = left_now(cap, frames);
    if PAGE << order_for(size) > left {
        return Err(StageError::Bad(no_memory("the initramfs", size, left)));
    }
    let phys = frames.alloc_bytes(size).ok_or_else(|| StageError::Bad(no_memory("the initramfs", size, left)))?;
    let mut at = phys;
    for p in &pieces {
        kexec_stub::write_phys(at, p);
        at += p.len() as u64;
    }
    kexec_stub::zero_phys(at, phys + size - at);
    Ok((phys, total, fnv.finish()))
}

/// What the answer and the log say about a staged kernel.
#[derive(Clone, Debug, Default)]
pub struct Summary {
    pub counter: u32,
    pub kernel_url: String,
    pub kernel_file_bytes: u64,
    /// RB-11: the kernel id (FNV-1a 64 of the KERNEL file), handed to the new kernel in the crash page's marker.
    pub kernel_fnv: u64,
    pub entry: u64,
    pub span: u64,
    pub image_phys: u64,
    pub initrd: Option<(u64, u64)>,
    pub initrd_fnv: u64,
    pub initrd_kept: bool,
    pub table_pages: usize,
    pub bootinfo_bytes: usize,
    pub bootinfo_capacity: usize,
    pub frames_bytes: u64,
    pub new_free: u64,
    pub new_largest: u64,
    pub tsc_hz: u64,
}

pub struct Staged {
    frames: Frames,
    kernel_root: u64,
    pub summary: Summary,
    stub_phys: u64,
    root: u64,
    entry: u64,
    stack_top: u64,
    bootinfo_phys: u64,
    bootinfo: Vec<u8>,
    /// RB-9: the GENERATION record the new kernel gets (a Reload into another stick generation); None: the
    /// running one's (RB-10 may still hand it on as confirmed).
    pub(crate) generation: Option<bootinfo::Generation>,
}

impl Staged {
    /// The BootInfo the new kernel will get (tests read it back).
    pub fn bootinfo_bytes(&self) -> &[u8] {
        &self.bootinfo
    }

    /// RB-10: swaps in a rebuilt BootInfo (`refresh::apply`); returns the one it replaces.
    pub(crate) fn replace_bootinfo(&mut self, b: Vec<u8>) -> Vec<u8> {
        self.summary.bootinfo_bytes = b.len();
        core::mem::replace(&mut self.bootinfo, b)
    }

    fn handoff(&self) -> hal::kexec_stub::Handoff<'_> {
        hal::kexec_stub::Handoff {
            kernel_root: self.kernel_root,
            stub_phys: self.stub_phys,
            new_root: self.root,
            entry: self.entry,
            stack_top: self.stack_top,
            bootinfo_phys: self.bootinfo_phys,
            bootinfo_capacity: self.summary.bootinfo_capacity as u64,
            bootinfo: &self.bootinfo,
        }
    }

    /// Every check the jump makes, run before the devices are quiesced (a refusal leaves the kernel working).
    pub fn check(&self) -> Result<(), hal::kexec_stub::StubError> {
        hal::kexec_stub::check(&self.handoff())
    }

    /// Into the new kernel. Returns only if the last checks fail (nothing was written then).
    pub fn jump(mut self) -> hal::kexec_stub::StubError {
        // RB-11: the new kernel learns which KERNEL file it is from this boot's crash-page marker.
        let handoff = self.handoff();
        let e = crate::crash::jump_handing_on(self.summary.kernel_fnv, || match hal::kexec_stub::jump(&handoff) {
            Ok(never) => match never {},
            Err(e) => e,
        });
        // The devices are quiesced; keep the frames out of use rather than hand them to a stopped system.
        self.frames.forget();
        e
    }
}

/// The root the jump runs on until the stub loads the new kernel's: a fresh PML4 with the running kernel's half
/// (shared with every root, so code, stacks and the direct map translate as in the live tables) and the `KEEP`
/// stub page identity-mapped, read-only and executable, as bitsboot maps it. MM-2's own kernel tables have no
/// lower half, and bitsboot's tables (recorded by `set_boot_info`) lack the vmap stacks threads run on, so neither
/// can be used as it is. Its frames belong to `frames` (given back unless the jump happens).
fn bridge_root(frames: &mut Frames, stub: u64) -> Result<u64, StageError> {
    let kroot = hal::paging::kernel_root()
        .or_else(boot::kernel_root)
        .ok_or_else(|| StageError::NotReady(String::from("no kernel root recorded")))?;
    // MM-6: `new_user_root` takes an 8 KiB block (PML4 + TLB context). The bridge root is no process root: the
    // context comes off at once, so the jump loads it as a plain CR3 (PCID 0) and its block frees like any other.
    let mut block = || frames.alloc_order(1);
    let root = hal::paging::new_user_root(kroot, &mut block)
        .ok_or_else(|| StageError::Bad(String::from("does not fit: no frame for the jump's page-table root")))?;
    hal::paging::retire_user_root(root);
    let mut alloc = || frames.alloc_order(0);
    hal::paging::map_4k(root, stub, stub, hal::paging::PageFlags(hal::paging::PageFlags::PRESENT), &mut alloc)
        .map_err(|e| StageError::NotReady(format!("the KEEP stub page {stub:#x} cannot be mapped for the jump: {e:?}")))?;
    hal::kexec_stub::check_stub_tables(root, stub)
        .map_err(|e| StageError::NotReady(format!("the KEEP stub page {stub:#x} is not usable: {e:?}")))?;
    Ok(root)
}

/// Whether the jump's page-table root can be built now and passes the stub checks, and whether the other CPUs
/// can be stopped (`reload_info`); the frames go back at once. Running on an AP is fine: the Reload's last phase
/// moves to CPU 0 itself (`tool::go`).
pub fn stub_ready(stub: u64) -> Result<(), String> {
    let mut frames = Frames::default();
    bridge_root(&mut frames, stub).map_err(|e| String::from(e.text()))?;
    match hal::kexec_stub::check_cpus() {
        Ok(()) | Err(hal::kexec_stub::StubError::NotBootCpu { .. }) => Ok(()),
        Err(e) => Err(format!("{e:?}")),
    }
}

/// Fetches, checks and stages the bundle. `deadline_ns`: the MCP call's deadline (the fetch ends before it).
pub fn stage(b: &Bundle, deadline_ns: u64) -> Result<Staged, StageError> {
    stage_with(b, deadline_ns, u64::MAX)
}

/// `stage` with the memory Reload may take capped at `cap` bytes (ktests prove the refusal with a small cap).
pub fn stage_with(b: &Bundle, deadline_ns: u64, cap: u64) -> Result<Staged, StageError> {
    let initrd = b.initrd.clone().map(FileRef::Url);
    stage_files(&FileRef::Url(b.kernel.clone()), initrd.as_ref(), None, deadline_ns, cap)
}

/// RB-9: stages generation `record.number`'s KERNEL and INITRD from the boot stick (`system_pin ... now`); the
/// new kernel gets `record` as its GENERATION record (what bitsboot would hand it at the next start).
pub fn stage_generation(record: bootinfo::Generation, deadline_ns: u64, cap: u64) -> Result<Staged, StageError> {
    let n = record.number;
    let (k, i) = (FileRef::Stick { generation: n, name: "KERNEL" }, FileRef::Stick { generation: n, name: "INITRD" });
    stage_files(&k, Some(&i), Some(record), deadline_ns, cap)
}

fn stage_files(kernel: &FileRef, initrd_from: Option<&FileRef>, generation: Option<bootinfo::Generation>, deadline_ns: u64, cap: u64) -> Result<Staged, StageError> {
    let spare = budget();
    if spare < START_NEED {
        return Err(StageError::Bad(format!(
            "does not fit: the running kernel can spare {} KiB (free memory beyond its {} MiB reserve); a Reload needs {} MiB to start",
            spare / 1024,
            RESERVE >> 20,
            START_NEED >> 20
        )));
    }
    let budget = spare.min(cap);
    let (bi, bi_phys) = boot::boot_info().ok_or_else(|| StageError::NotReady(String::from(
        "the kernel did not hand its BootInfo to kexec (kexec::set_boot_info, desk change request of RB-5a)",
    )))?;
    let stub = bi.reload_stub().ok_or_else(|| StageError::NotReady(String::from("BootInfo has no RELOAD_STUB page")))?.phys;
    let mut frames = Frames::default();
    // The page and tables only: staging (and a dry run) works with the APs up; the jump checks the CPUs.
    let kernel_root = bridge_root(&mut frames, stub)?;
    let old_map = bi.memory_map().ok_or_else(|| StageError::NotReady(String::from("BootInfo has no memory map")))?;
    // The jump writes the stub page and the new BootInfo raw; both must be bitsboot's KEEP pages, whatever the
    // RELOAD_STUB record and the header's capacity field (bytes 20..24) say.
    let capacity = handover::capacity(&bi);
    if capacity < bi.total_size() {
        return Err(StageError::NotReady(format!(
            "the BootInfo header names a KEEP block of {capacity} bytes, smaller than the BootInfo itself ({} bytes)",
            bi.total_size()
        )));
    }
    let keep = |start: u64, len: u64| {
        start.checked_add(len).is_some_and(|end| memmap::covered_by(old_map.iter(), start, end, memtype::BITS_KEEP))
    };
    if !keep(stub, PAGE) {
        return Err(StageError::NotReady(format!("the RELOAD_STUB page {stub:#x} is not a BITS_KEEP page of the memory map")));
    }
    if !keep(bi_phys, capacity as u64) {
        return Err(StageError::NotReady(format!(
            "the BootInfo block {bi_phys:#x} + {capacity} bytes is not wholly BITS_KEEP memory (header capacity field wrong?)"
        )));
    }
    if initrd_from.is_none() && handover::old_initrds(&bi).is_empty() {
        klog::event!(Warn, "kexec", "reload.no_initrd"; "no initramfs given and none running: the new kernel starts without one");
    }

    let (layout, image, file_bytes, kernel_fnv) = fetch_kernel(kernel, &mut frames, deadline_ns, budget)?;
    klog::event!(Info, "kexec", "reload.fetched", what = "kernel", bytes = file_bytes, span = layout.span, entry = klog::Hex(layout.entry), fnv = klog::Hex(kernel_fnv));
    if initrd_from.is_none() && crate::boot::initrd_released() && !handover::old_initrds(&bi).is_empty() {
        // INT-A1 (after the kernel's own checks): the running initramfs was given back to the allocator after it was unpacked into tmpfs; its
        // frames may hold anything now, so it cannot be handed on.
        klog::event!(Warn, "kexec", "reload.initrd_released"; "the running initramfs was released after unpacking; the reload needs an initrd URL");
        return Err(StageError::NotReady(String::from(
            "the running initramfs was released after it was unpacked into tmpfs: give initrd=<url> or a bundle directory url ending in / (KERNEL + INITRD)",
        )));
    }
    let initrd = match initrd_from {
        Some(u) => {
            let (p, size, fnv) = fetch_initrd(u, &mut frames, deadline_ns, budget)?;
            klog::event!(Info, "kexec", "reload.fetched", what = "initrd", bytes = size, fnv = klog::Hex(fnv));
            Some((p, size, fnv))
        }
        None => None,
    };

    let left = |f: &Frames| left_now(budget, f);
    if PAGE << STACK_ORDER > left(&frames) {
        return Err(StageError::Bad(no_memory("the boot stack", PAGE << STACK_ORDER, left(&frames))));
    }
    let stack = frames.alloc_order(STACK_ORDER).ok_or_else(|| StageError::Bad(no_memory("the boot stack", PAGE << STACK_ORDER, left(&frames))))?;
    let ram = memmap::ram_ranges(&old_map.iter().collect::<Vec<_>>());
    let fb = bi.framebuffer().filter(|f| f.format != PixelFormat::None && f.phys != 0);
    let fb_span = fb.map_or(0, |f| ((f.phys & 0xFFF) + f.size).next_multiple_of(PAGE));
    let estimate = tables::estimate(&ram, layout.span, fb_span);
    if PAGE << order_for(estimate * PAGE) > left(&frames) {
        return Err(StageError::Bad(no_memory("the page tables", estimate * PAGE, left(&frames))));
    }
    let tbase = frames.alloc_bytes(estimate * PAGE).ok_or_else(|| StageError::Bad(no_memory("the page tables", estimate * PAGE, left(&frames))))?;
    let mut t = Tables::new(tbase, estimate as usize, hal::cpu::detect().nx);
    let full = |_| StageError::Bad(String::from("the page tables did not fit their estimate (bug)"));
    for s in &layout.segs {
        let start = s.vaddr & !(PAGE - 1);
        let span = (s.vaddr + s.memsz).next_multiple_of(PAGE) - start;
        let perm = Perm { write: s.flags & PF_W != 0, exec: s.flags & PF_X != 0, pat1: false };
        t.map_range(start, image + (start - layout.lo), span, perm).map_err(full)?;
    }
    let top = t.map_direct(&ram).map_err(full)?;
    if let Some(f) = fb {
        t.map_range(VMAP_BASE, f.phys & !0xFFF, fb_span, Perm { write: true, exec: false, pat1: true }).map_err(full)?;
    }
    t.map_range(stub, stub, PAGE, Perm { write: false, exec: true, pat1: false }).map_err(full)?;
    let tbytes = t.bytes();
    kexec_stub::write_phys(tbase, &tbytes);

    let mut marks = alloc::vec![
        Mark { start: image, end: image + layout.span, ty: memtype::BITS_KERNEL },
        Mark { start: stack, end: stack + (PAGE << STACK_ORDER), ty: memtype::BITS_KERNEL },
        Mark { start: tbase, end: tbase + t.used() as u64 * PAGE, ty: memtype::BITS_PAGETABLE },
    ];
    let release = if let Some((p, size, _)) = initrd {
        marks.push(Mark { start: p, end: p + size.next_multiple_of(PAGE), ty: memtype::BITS_MODULE });
        handover::old_initrds(&bi)
    } else {
        Vec::new()
    };
    marks.sort_by_key(|m| m.start);
    let map = memmap::rebuild(old_map.iter(), &release, &marks).map_err(|e| StageError::Bad(format!("memory map: {e:?}")))?;
    let (new_free, new_largest) = memmap::free_after(&map);
    if new_largest < MIN_LARGEST_FREE {
        return Err(StageError::Bad(format!(
            "does not fit: the new kernel would find only {} MiB in one free piece (needs {} MiB)",
            new_largest >> 20,
            MIN_LARGEST_FREE >> 20
        )));
    }
    let tsc = handover::next_tsc(&bi, time::tsc_hz(), time::source(), time::rate_measured_here());
    let nb = NewBoot {
        image: KernelImage { phys_base: image, phys_size: layout.span, virt_base: layout.lo, entry: layout.entry },
        initrd: initrd.map(|(p, s, _)| (p, s)),
        tables: PageTables {
            pml4_phys: t.pml4(),
            direct_map_base: DIRECT_MAP_BASE,
            direct_map_size: top,
            kernel_virt_base: KERNEL_BASE,
            vmap_base: VMAP_BASE,
        },
        tsc,
        map: &map,
    };
    let bytes = handover::build(&bi, &nb, capacity).map_err(|e| StageError::Bad(format!(
        "the new BootInfo does not fit bitsboot's KEEP block of {capacity} bytes: {e:?}"
    )))?;
    let summary = Summary {
        counter: handover::next_counter(&bi),
        kernel_url: kernel.label(),
        kernel_file_bytes: file_bytes,
        kernel_fnv,
        entry: layout.entry,
        span: layout.span,
        image_phys: image,
        initrd: nb.initrd,
        initrd_fnv: initrd.map(|i| i.2).unwrap_or(0),
        initrd_kept: initrd.is_none(),
        table_pages: t.used(),
        bootinfo_bytes: bytes.len(),
        bootinfo_capacity: capacity,
        frames_bytes: frames.total(),
        new_free,
        new_largest,
        tsc_hz: tsc.calibrated_hz,
    };
    Ok(Staged {
        frames,
        kernel_root,
        summary,
        stub_phys: stub,
        root: t.pml4(),
        entry: layout.entry,
        stack_top: DIRECT_MAP_BASE + stack + (PAGE << STACK_ORDER),
        bootinfo_phys: bi_phys,
        bootinfo: bytes,
        generation,
    })
}
