//! Loading an ELF image into a fresh address space (AB-2's loader, used by spawn and execve): segments with
//! their final protection, the heap start, the stack and Linux's initial stack with the auxiliary vector
//! (linux-6.6/fs/binfmt_elf.c `load_elf_binary`, `load_elf_interp`, `create_elf_tables`).
//!
//! AB-8: a program file's segments are file-backed `MAP_PRIVATE` mappings of its page cache (FS-5; MM-5b
//! copies a page on the first write), the rest of a writable segment's last file page is cleared (`padzero`),
//! the bss is anonymous memory. A file system without a page cache gives the bytes by `read_at` instead (copied
//! into anonymous memory, event `proc image.copied`); an image in memory (ktests, MCP `run` with `elf_b64`) is
//! copied. A dynamic program's interpreter (PT_INTERP) is loaded the same way, top-down below the mmap base;
//! the thread starts at the interpreter's entry, AT_BASE is its load bias.

use crate::{SpawnError, STACK_TOP, elf, load, stack};
use alloc::vec::Vec;
use mm::linux::{MAP_ANONYMOUS, MAP_FIXED, MAP_PRIVATE};
use mm::{AddressSpace, Prot, VmaKind};
use vfs::File;

/// Linux's `stack_guard_gap` (256 pages): the image must end this far below the stack.
pub(crate) const STACK_GAP: u64 = 256 * 4096;
/// The smallest main-thread stack (two times Linux's ARG_MAX: the argument space alone may take ARG_MAX).
pub const MIN_STACK: u64 = 256 << 10;
/// The largest main-thread stack. V2 maps the whole stack at exec and charges it to the commit (Linux grows it
/// up to RLIMIT_STACK and charges as it grows), so a larger or unlimited RLIMIT_STACK gets this much.
pub const MAX_STACK: u64 = 256 << 20;

/// The main thread's stack size for a soft RLIMIT_STACK of `rlim` (AB-4 open point 3).
pub fn stack_size(rlim: u64) -> u64 {
    (rlim & !(elf::PAGE - 1)).clamp(MIN_STACK, MAX_STACK)
}

/// Where an image's bytes come from.
#[derive(Clone, Copy)]
pub(crate) enum Source<'a> {
    /// In memory: copied.
    Mem(&'a [u8]),
    /// A file: mapped from its page cache when it has one, else read and copied.
    File { path: &'a vfs::Path, file: &'a dyn File },
}

impl Source<'_> {
    fn len(&self) -> u64 {
        match self {
            Source::Mem(b) => b.len() as u64,
            Source::File { file, .. } => file.size(),
        }
    }

    /// Fills all of `buf` from `off`.
    fn read_exact(&self, off: u64, buf: &mut [u8]) -> Result<(), elf::ElfError> {
        match self {
            Source::Mem(b) => {
                let s = usize::try_from(off).ok().and_then(|o| b.get(o..o.checked_add(buf.len())?)).ok_or(elf::SHORT_READ)?;
                buf.copy_from_slice(s);
            }
            Source::File { file, .. } => {
                let mut got = 0;
                while got < buf.len() {
                    match file.read_at(off + got as u64, &mut buf[got..]) {
                        // Linux `elf_read`: a short read is EIO, a failed one its errno (V2 reports EIO for both).
                        Ok(0) | Err(_) => return Err(elf::SHORT_READ),
                        Ok(n) => got += n,
                    }
                }
            }
        }
        Ok(())
    }

    /// Reads the ELF headers (and the PT_INTERP path) only.
    pub(crate) fn elf(&self) -> Result<elf::Elf, elf::ElfError> {
        elf::Elf::parse_from(self.len(), &mut |o, b| self.read_exact(o, b), hal::USER_TOP)
    }

    /// The same for an interpreter (its own PT_INTERP is ignored).
    pub(crate) fn interp_elf(&self) -> Result<elf::Elf, elf::ElfError> {
        elf::Elf::parse_interp_from(self.len(), &mut |o, b| self.read_exact(o, b), hal::USER_TOP)
    }
}

/// One image to load: where its bytes are and its headers.
pub(crate) struct Image<'a> {
    pub src: Source<'a>,
    pub elf: &'a elf::Elf,
}

/// What the loader placed.
pub(crate) struct Loaded {
    /// Where the thread starts: the interpreter's entry for a dynamic program.
    pub entry: u64,
    pub rsp: u64,
    pub bias: u64,
    /// The interpreter's load bias (AT_BASE), 0 without one.
    pub interp_base: u64,
    /// The auxiliary vector as the program got it (AB-5, PR_GET_AUXV).
    pub auxv: Vec<u64>,
    /// AB-7a: the layout /proc shows (no `exe` yet: the caller knows the file).
    pub info: crate::procinfo::ImageInfo,
}

/// Maps the program (and its interpreter) and builds a stack of `stack_size` bytes in `mm` (empty, not yet
/// used by anyone).
#[allow(clippy::too_many_arguments)]
pub(crate) fn load(mm: &AddressSpace, prog: &Image<'_>, interp: Option<&Image<'_>>, stack_size: u64, execfn: &[u8], argv: &[&[u8]], envp: &[&[u8]], cred: &crate::cred::Cred) -> Result<Loaded, SpawnError> {
    let nomem = |_| SpawnError::NoMemory;
    let limit = STACK_TOP - stack_size - STACK_GAP;
    let plan = load::plan(prog.elf, mm::MMAP_TOP, limit).map_err(SpawnError::NotExecutable)?;
    place(mm, &prog.src, &plan)?;
    let iplan = match interp {
        None => None,
        Some(i) => {
            // An interpreter is a static-PIE for the planner: top-down below the mmap base, or below the program
            // when the program sits up there itself (`load_elf_interp` maps it where mmap finds room).
            let mut ip = load::plan(i.elf, mm::MMAP_TOP, limit).map_err(SpawnError::NotExecutable)?;
            if ip.start < plan.end && plan.start < ip.end {
                ip = load::plan(i.elf, plan.start, limit).map_err(SpawnError::NotExecutable)?;
                if ip.start < plan.end && plan.start < ip.end {
                    return Err(SpawnError::NotExecutable("interpreter overlaps the program"));
                }
            }
            place(mm, &i.src, &ip)?;
            Some(ip)
        }
    };
    mm.init_brk(plan.brk);
    let stack_prot = Prot { read: true, write: true, exec: prog.elf.exec_stack };
    mm.map_fixed(STACK_TOP - stack_size, stack_size, stack_prot, VmaKind::Stack).map_err(nomem)?;

    let mut rnd = [0u8; 16];
    crate::random_bytes(&mut rnd);
    let interp_base = iplan.as_ref().map_or(0, |p| p.bias);
    let aux = auxv(prog.elf, &plan, interp_base, cred);
    let img = stack::build(STACK_TOP, stack_size, argv, envp, execfn, rnd, &aux).map_err(|e| match e {
        stack::StackError::TooBig => SpawnError::TooBig,
        stack::StackError::NoMemory => SpawnError::NoMemory,
    })?;
    mm.copy_to_user(img.rsp, &img.bytes).map_err(nomem)?;
    let info = crate::procinfo::ImageInfo::of_load(prog.elf, plan.bias, &img);
    let entry = iplan.as_ref().map_or(plan.entry, |p| p.entry);
    Ok(Loaded { entry, rsp: img.rsp, bias: plan.bias, interp_base, auxv: img.auxv, info })
}

/// Linux's PROT_* bits of `p`.
fn bits(p: Prot) -> u64 {
    p.read as u64 | (p.write as u64) << 1 | (p.exec as u64) << 2
}

/// Puts one planned image into `mm`.
fn place(mm: &AddressSpace, src: &Source<'_>, plan: &load::Plan) -> Result<(), SpawnError> {
    if let Source::File { path, .. } = src
        && path.dentry.inode.page_cache().is_some()
    {
        return map_file(mm, path, plan);
    }
    if let Source::File { path, .. } = src {
        klog::event!(Info, "proc", "image.copied", ino = path.dentry.inode.meta.ino, bytes = src.len(); "no page cache on this file system: the segments are read and copied");
    }
    copy_in(mm, src, plan)
}

/// AB-8: `elf_map` per segment (MAP_PRIVATE|MAP_FIXED of the page cache), `padzero`, the bss.
fn map_file(mm: &AddressSpace, path: &vfs::Path, plan: &load::Plan) -> Result<(), SpawnError> {
    let err = |e: i64| if e == uapi::errno::ENOMEM { SpawnError::NoMemory } else { SpawnError::Io(e) };
    for s in &plan.segs {
        let prot = bits(s.prot);
        if s.file_end > s.start {
            // The exec file is opened without FMODE_READ checks (`open_exec`): no `vfs::mmap::check`, the
            // mapping is private, never shared.
            let mut ok = |_len: u64| Ok(vfs::mmap::Mapped { backing: vfs::mmap::FileMap::new(path.clone(), false, false), shared: false, may_write: true, may_exec: true });
            mm.sys_mmap_file(s.start, s.file_end - s.start, prot, MAP_PRIVATE | MAP_FIXED, s.off, &mut ok).map_err(err)?;
        }
        if let Some((from, to)) = s.zero {
            // A private copy of the cache page (MM-5b's write fault), cleared past the segment's file bytes.
            let z = [0u8; elf::PAGE as usize];
            mm.copy_to_user(from, &z[..(to - from) as usize]).map_err(|_| SpawnError::NoMemory)?;
        }
        if s.end > s.file_end {
            mm.sys_mmap(s.file_end, s.end - s.file_end, prot, MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED, 0).map_err(|e| err(e.code()))?;
        }
    }
    Ok(())
}

/// AB-2's copy: anonymous image memory, the file bytes written in (read-only pages too), then PROT_NONE.
fn copy_in(mm: &AddressSpace, src: &Source<'_>, plan: &load::Plan) -> Result<(), SpawnError> {
    let nomem = |_| SpawnError::NoMemory;
    for r in &plan.regions {
        // A PROT_NONE segment is filled while readable, then closed (the loader cannot write PROT_NONE pages).
        let p = if r.prot.accessible() { r.prot } else { Prot { read: true, write: false, exec: false } };
        mm.map_fixed(r.start, r.end - r.start, p, VmaKind::Image).map_err(nomem)?;
    }
    let mut buf: Vec<u8> = Vec::new();
    for c in &plan.copies {
        match src {
            Source::Mem(b) => mm.write_loader(c.dst, &b[c.src as usize..(c.src + c.len) as usize]).map_err(nomem)?,
            Source::File { .. } => {
                // In pieces of at most 64 KiB: no buffer of the file's size.
                let mut done = 0;
                while done < c.len {
                    let n = (c.len - done).min(64 << 10) as usize;
                    buf.clear();
                    buf.try_reserve(n).map_err(|_| SpawnError::NoMemory)?;
                    buf.resize(n, 0);
                    src.read_exact(c.src + done, &mut buf).map_err(|_| SpawnError::Io(uapi::errno::EIO))?;
                    mm.write_loader(c.dst + done, &buf).map_err(nomem)?;
                    done += n as u64;
                }
            }
        }
    }
    for r in plan.regions.iter().filter(|r| !r.prot.accessible()) {
        mm.protect(r.start, r.end - r.start, Prot::NONE).map_err(nomem)?;
    }
    Ok(())
}

/// AT_HWCAP2 bit: the kernel allows and keeps FSGSBASE in user mode (Linux `HWCAP2_FSGSBASE`).
const HWCAP2_FSGSBASE: u64 = 1 << 1;

/// Linux's AT_MINSIGSTKSZ for x86-64 (`init_sigframe_size`): the rt_sigframe (440 bytes) + 15 bytes alignment
/// + the XSAVE area for the enabled user features (CPUID 0xD.0 EBX) + 4 (FP_XSTATE_MAGIC2) + 112 (the legacy
/// fregs space Linux always adds with IA32 emulation) + 63 bytes XSAVE alignment, rounded up to 16. AB-6's
/// signal frame must fit in it.
pub fn min_sigstack_size() -> u64 {
    let xsave = hal::cpu::cpuid(0xD, 0).ebx as u64;
    (440 + 15 + xsave + 4 + 112 + 63).next_multiple_of(16)
}

/// The auxiliary vector in Linux's order (ARCH_DLINFO first, then `create_elf_tables`). Not there: AT_SYSINFO_EHDR
/// (no vDSO before AB-9: glibc and musl then make the real clock syscalls), AT_RSEQ_* (rseq is ENOSYS, AB-8's
/// decision), AT_BASE_PLATFORM (x86 has none). `interp_base` is AT_BASE (0 without an interpreter).
pub(crate) fn auxv(elf: &elf::Elf, plan: &load::Plan, interp_base: u64, cred: &crate::cred::Cred) -> Vec<(u64, stack::AuxVal)> {
    use stack::AuxVal::*;
    use uapi::auxv::*;
    alloc::vec![
        (AT_MINSIGSTKSZ, Value(min_sigstack_size())),
        (AT_HWCAP, Value(hal::cpu::cpuid(1, 0).edx as u64)),
        (AT_PAGESZ, Value(4096)),
        (AT_CLKTCK, Value(100)),
        (AT_PHDR, Value(plan.phdr)),
        (AT_PHENT, Value(56)),
        (AT_PHNUM, Value(elf.phnum as u64)),
        (AT_BASE, Value(interp_base)),
        (AT_FLAGS, Value(0)),
        (AT_ENTRY, Value(plan.entry)),
        // AB-13: the ids after execve (`create_elf_tables`); AT_SECURE as `cap_bprm_creds_from_file` sets
        // `secureexec` without set-id files: the effective ids differ from the real ones.
        (AT_UID, Value(cred.uid as u64)),
        (AT_EUID, Value(cred.euid as u64)),
        (AT_GID, Value(cred.gid as u64)),
        (AT_EGID, Value(cred.egid as u64)),
        (AT_SECURE, Value((cred.euid != cred.uid || cred.egid != cred.gid) as u64)),
        (AT_RANDOM, Random),
        // HWCAP2_FSGSBASE only when the entry code keeps what wrfsbase/wrgsbase write (AB-1).
        (AT_HWCAP2, Value(if hal::entry::user_fsgsbase() { HWCAP2_FSGSBASE } else { 0 })),
        (AT_EXECFN, ExecFn),
        (AT_PLATFORM, Platform),
    ]
}
