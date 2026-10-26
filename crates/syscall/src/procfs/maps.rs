//! `/proc/<pid>/maps`, `smaps` and `smaps_rollup` (linux-6.6/fs/proc/task_mmu.c `show_vma_header_prefix` :240,
//! `show_map_vma`, `show_smap` :851, `show_smaps_rollup` :877, `__show_smap` :809) and the memory sums `status`, `stat` and
//! `statm` share (`task_mem` :31).
//!
//! The program's segments are copied into anonymous memory at load (AB-8 maps them from the file): their lines
//! show the file's device, inode and page offset and its path, split into the file part and the anonymous
//! `.bss` rest as Linux's `elf_map` + `set_brk` leave them.

use alloc::string::String;
use core::fmt::Write;
use alloc::vec::Vec;
use mm::vma::{Vma, VmaKind};
use mm::{PageUsage, PSS_SHIFT};
use uapi::errno::ENOMEM;
use vfs::Errno;
use proc::procinfo::ImageInfo;
use proc::{Group, Process};

const PAGE: u64 = 4096;
/// `seq_setwidth(m, 25 + sizeof(void *) * 6 - 1)`: names start after column 73.
const NAME_COLUMN: usize = 72;

/// What `task_mem` sums (bytes, except `rss_pages`).
#[derive(Default)]
pub(super) struct Mem {
    pub total: u64,
    /// `data_vm`: private writable mappings other than the stack.
    pub data: u64,
    pub stack: u64,
    /// `exec_vm`: executable, not writable, not stack.
    pub exec: u64,
    pub rss_pages: u64,
    pub start_brk: u64,
}

impl Mem {
    /// `task_mem`'s split of `exec_vm` into the program's text and the rest (VmExe, VmLib).
    pub(super) fn text_lib(&self, i: &ImageInfo) -> (u64, u64) {
        let text = (i.end_code.next_multiple_of(PAGE).saturating_sub(i.start_code & !(PAGE - 1))).min(self.exec);
        (text, self.exec - text)
    }
}

fn shared(v: &Vma) -> bool {
    v.file.as_ref().is_some_and(|f| f.shared)
}

pub(super) fn mem_of(p: &Process) -> Mem {
    let mut m = Mem { rss_pages: p.mm.rss_pages(), ..Default::default() };
    let mut heap = None;
    for v in p.mm.vmas() {
        let len = v.end - v.start;
        m.total += len;
        let stack = v.kind == VmaKind::Stack;
        if stack {
            m.stack += len;
        } else if v.prot.write && !shared(&v) {
            m.data += len;
        }
        if v.prot.exec && !v.prot.write && !stack {
            m.exec += len;
        }
        if v.kind == VmaKind::Heap {
            heap = Some(heap.map_or(v.start, |h: u64| h.min(v.start)));
        }
    }
    // `mm->start_brk`: where the heap starts (the break itself before the first brk call grew it).
    m.start_brk = heap.unwrap_or_else(|| p.mm.current_brk());
    m
}

/// One maps line: the header (with Linux's trailing space), then the name after column 73.
pub(super) fn line(out: &mut String, start: u64, end: u64, perms: [u8; 4], off: u64, dev: u64, ino: u64, name: Option<&str>) {
    let at = out.len();
    let p = |i: usize| perms[i] as char;
    let _ = write!(out, "{start:08x}-{end:08x} {}{}{}{} {off:08x} {:02x}:{:02x} {ino} ", p(0), p(1), p(2), p(3), vfs::abi::dev_major(dev), vfs::abi::dev_minor(dev));
    if let Some(n) = name {
        while out.len() - at < NAME_COLUMN {
            out.push(' ');
        }
        out.push(' ');
        out.push_str(n);
    }
    out.push('\n');
}

/// The image VMA `[vs, ve)` as Linux shows the ELF's mappings: per PT_LOAD the file part (offset, device,
/// inode, path) and the anonymous rest; space outside every segment is anonymous. `each` follows every line.
pub(super) fn image_lines(out: &mut String, vs: u64, ve: u64, perms: [u8; 4], info: &ImageInfo, file: (u64, u64, &str), each: &mut dyn FnMut(&mut String, u64, u64)) {
    let mut put = |out: &mut String, a: u64, b: u64, off: u64, f: Option<(u64, u64, &str)>| {
        match f {
            Some((dev, ino, name)) => line(out, a, b, perms, off, dev, ino, Some(name)),
            None => line(out, a, b, perms, 0, 0, 0, None),
        }
        each(out, a, b);
    };
    let mut at = vs;
    for s in info.segs.iter().filter(|s| s.end > vs && s.start < ve) {
        let a = at.max(s.start);
        if at < a {
            put(out, at, a, 0, None);
        }
        let fe = s.file_end.min(ve);
        if a < fe {
            put(out, a, fe, s.off + (a - s.start), Some(file));
        }
        let b = a.max(fe);
        let e = s.end.min(ve);
        if b < e {
            put(out, b, e, 0, None);
        }
        at = at.max(e);
    }
    if at < ve {
        put(out, at, ve, 0, None);
    }
}

/// What every line of a read needs, looked up once: the image's segments and the executable's identity and
/// name (d_path, " (deleted)" once unlinked).
struct Ctx {
    info: ImageInfo,
    exe: Option<(u64, u64, String)>,
}

fn ctx_of(p: &Process) -> Ctx {
    let info = p.image_info.lock().clone();
    let exe = info.exe.as_ref().map(|path| {
        let a = path.attr();
        (a.dev, a.ino, String::from_utf8_lossy(&super::d_path(path)).into_owned())
    });
    Ctx { info, exe }
}

/// The maps line(s) of `v` (one per Linux VMA; an image VMA shows as several), `each(out, start, end)` after
/// every line.
fn vma_lines(out: &mut String, v: &Vma, c: &Ctx, p: &Process, each: &mut dyn FnMut(&mut String, u64, u64)) {
    let l = v.prot.letters();
    // FS-5: a file mapping shows its offset, device, inode and path (" (deleted)" once unlinked), and `s`
    // for MAP_SHARED (Linux's VM_MAYSHARE), as `show_map_vma`.
    let file = v.file.as_ref().and_then(|f| f.backing.file().map(|m| (m, f.pgoff)));
    let share = match file {
        Some((m, _)) if m.may_share() => b's',
        _ => b'p',
    };
    let perms = [l[0], l[1], l[2], share];
    let (vs, ve) = (v.start, v.end);
    match (v.kind, &c.exe) {
        (VmaKind::Image, Some((dev, ino, name))) => return image_lines(out, vs, ve, perms, &c.info, (*dev, *ino, name), each),
        // An image started from memory (ktests) has no file: it keeps the name it was started as.
        (VmaKind::Image, None) => line(out, vs, ve, perms, 0, 0, 0, Some(&p.path)),
        (VmaKind::Stack, _) => line(out, vs, ve, perms, 0, 0, 0, Some("[stack]")),
        (VmaKind::Heap, _) => line(out, vs, ve, perms, 0, 0, 0, Some("[heap]")),
        (VmaKind::File, _) if file.is_some() => {
            if let Some((m, pgoff)) = file {
                let a = m.inode().attr(m.path().dentry.sb.dev, 0);
                // FS-10: `d_path` (a memfd shows as `/memfd:<name> (deleted)`).
                let name = String::from_utf8_lossy(&super::d_path(m.path())).into_owned();
                line(out, vs, ve, perms, pgoff << 12, a.dev, a.ino, Some(&name));
            }
        }
        _ => line(out, vs, ve, perms, 0, 0, 0, None),
    }
    each(out, vs, ve);
}

pub(super) fn render_maps(g: &Group, p: &Process) -> String {
    let mut s = String::new();
    if g.is_finished() {
        return s;
    }
    let c = ctx_of(p);
    for v in p.mm.vmas() {
        vma_lines(&mut s, &v, &c, p, &mut |_, _, _| {});
    }
    s
}

/// `__show_smap`'s rows from `Rss` on; `rollup` adds Pss_Anon/Pss_File/Pss_Shmem as Linux does only there.
fn usage_rows(s: &mut String, u: &PageUsage, rollup: bool) {
    let kb = |b: u64| b >> 10;
    let pk = |x: u64| (x >> PSS_SHIFT) >> 10;
    let mut row = |name: &str, v: u64| {
        // Private_Hugetlb's label is one wider; Linux prints its number one narrower.
        let w = if name.len() > 16 { 7 } else { 8 };
        let _ = writeln!(s, "{name}{v:>w$} kB");
    };
    row("Rss:            ", kb(u.resident));
    row("Pss:            ", pk(u.pss));
    row("Pss_Dirty:      ", pk(u.pss_dirty));
    if rollup {
        row("Pss_Anon:       ", pk(u.pss_anon));
        row("Pss_File:       ", pk(u.pss_file));
        row("Pss_Shmem:      ", pk(u.pss_shmem));
    }
    row("Shared_Clean:   ", kb(u.shared_clean));
    row("Shared_Dirty:   ", kb(u.shared_dirty));
    row("Private_Clean:  ", kb(u.private_clean));
    row("Private_Dirty:  ", kb(u.private_dirty));
    row("Referenced:     ", kb(u.referenced));
    row("Anonymous:      ", kb(u.anonymous));
    // V2 has no KSM, lazy free, huge pages, swap or mlock: Linux's zero rows.
    for name in ["KSM:            ", "LazyFree:       ", "AnonHugePages:  ", "ShmemPmdMapped: ", "FilePmdMapped:  ",
        "Shared_Hugetlb: ", "Private_Hugetlb: ", "Swap:           ", "SwapPss:        ", "Locked:         "]
    {
        row(name, 0);
    }
}

/// `show_smap_vma_flags` (task_mmu.c:636) in bit order for the flags V2 has. `me` from the file's VM_MAYEXEC
/// (`FileRef::may_exec`: clear on a noexec mount, FS-13 CR3). No `sd`: V2 has no soft-dirty tracking
/// (a kernel without CONFIG_MEM_SOFT_DIRTY).
fn vm_flags(s: &mut String, v: &Vma) {
    let f = v.file.as_ref();
    let device = f.is_some_and(|f| f.device.is_some());
    let may_share = f.is_some_and(|f| f.shared || f.backing.file().is_some_and(|m| m.may_share()));
    let flags = [
        (v.prot.read, "rd"),
        (v.prot.write, "wr"),
        (v.prot.exec, "ex"),
        (f.is_some_and(|f| f.shared), "sh"),
        (true, "mr"),
        (f.is_none_or(|f| f.may_write), "mw"),
        (f.is_none_or(|f| f.may_exec), "me"),
        (may_share, "ms"),
        (v.kind == VmaKind::Stack, "gd"),
        // A device mapping is Linux's `io_remap_pfn_range`: VM_PFNMAP | VM_IO | VM_DONTEXPAND | VM_DONTDUMP.
        (device, "pf"),
        (device, "io"),
        (v.flags.dontfork, "dc"),
        (device, "de"),
        (v.flags.account, "ac"),
        (v.flags.wipeonfork, "wf"),
        (device, "dd"),
    ];
    s.push_str("VmFlags: ");
    for (on, m) in flags {
        if on {
            s.push_str(m);
            s.push(' ');
        }
    }
    s.push('\n');
}

/// The share of `u` that the line `[a, b)` of an image VMA gets. MM counts per VMA; an image VMA shows as
/// several Linux VMAs (file part, `.bss` rest). Its resident pages are given to the lines in address order
/// (`taken` pages already given): the loader copies every file part, so those pages are resident, and the rest
/// goes to `.bss`. Every field is scaled by the same share; the last line gets the remainder, so the lines sum
/// to the VMA exactly. Change request to MM: usage per address range for exact numbers.
pub(super) fn share_of(u: &PageUsage, a: u64, b: u64, last: bool, taken: &mut u64, given: &mut PageUsage) -> PageUsage {
    let total = u.resident / PAGE;
    let mine = if last { total.saturating_sub(*taken) } else { (b.saturating_sub(a) / PAGE).min(total.saturating_sub(*taken)) };
    *taken += mine;
    let part = |x: u64, g: &mut u64| {
        let v = if last { x.saturating_sub(*g) } else if total == 0 { 0 } else { ((x as u128 * mine as u128) / total as u128) as u64 };
        *g += v;
        v
    };
    let g = given;
    PageUsage {
        resident: part(u.resident, &mut g.resident),
        pss: part(u.pss, &mut g.pss),
        pss_dirty: part(u.pss_dirty, &mut g.pss_dirty),
        pss_anon: part(u.pss_anon, &mut g.pss_anon),
        pss_file: part(u.pss_file, &mut g.pss_file),
        pss_shmem: part(u.pss_shmem, &mut g.pss_shmem),
        shared_clean: part(u.shared_clean, &mut g.shared_clean),
        shared_dirty: part(u.shared_dirty, &mut g.shared_dirty),
        private_clean: part(u.private_clean, &mut g.private_clean),
        private_dirty: part(u.private_dirty, &mut g.private_dirty),
        referenced: part(u.referenced, &mut g.referenced),
        anonymous: part(u.anonymous, &mut g.anonymous),
    }
}

/// `show_smap`: per VMA its maps line, size, page sizes, the page numbers MM walked and `VmFlags`.
pub(super) fn render_smaps(g: &Group, p: &Process) -> Result<String, Errno> {
    let mut s = String::new();
    if g.is_finished() {
        return Ok(s);
    }
    // MM walks under its own lock, piece by piece; no Group lock is held here.
    let usage = p.mm.page_usage().map_err(|_| ENOMEM)?;
    let c = ctx_of(p);
    for (v, u) in &usage {
        // The lines of this VMA, so the last one is known (it takes the rounding remainder).
        let mut ends = Vec::new();
        vma_lines(&mut String::new(), v, &c, p, &mut |_, _, b| ends.push(b));
        let last_end = ends.last().copied().unwrap_or(v.end);
        let (mut taken, mut given) = (0u64, PageUsage::default());
        vma_lines(&mut s, v, &c, p, &mut |s, a, b| {
            let part = if a == v.start && b == v.end { *u } else { share_of(u, a, b, b == last_end, &mut taken, &mut given) };
            let _ = write!(s, "Size:           {:>8} kB\nKernelPageSize: {:>8} kB\nMMUPageSize:    {:>8} kB\n", b.saturating_sub(a) >> 10, 4, 4);
            usage_rows(s, &part, false);
            let _ = writeln!(s, "THPeligible:    {:>8}", 0);
            vm_flags(s, v);
        });
    }
    Ok(s)
}

/// `show_smaps_rollup`: the span of all mappings and the sums of every VMA's pages (MM's walk).
pub(super) fn render_smaps_rollup(g: &Group, p: &Process) -> String {
    let mut s = String::new();
    if g.is_finished() {
        return s;
    }
    let vmas = p.mm.vmas();
    let (Some(first), Some(last)) = (vmas.first(), vmas.last()) else { return s };
    line(&mut s, first.start, last.end, *b"---p", 0, 0, 0, Some("[rollup]"));
    usage_rows(&mut s, &p.mm.page_usage_total(), true);
    s
}
