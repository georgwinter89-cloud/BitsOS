//! ELF64 reader for x86-64 programs: static (`ET_EXEC`), static-PIE (`ET_DYN` without `PT_INTERP`) and, since
//! AB-8, dynamic programs (`PT_INTERP` names their loader, e.g. glibc's `/lib64/ld-linux-x86-64.so.2`).
//! `parse_from` reads only the header, the program headers and the PT_INTERP path through a reader, so a
//! program file is never read whole (its segments map from the page cache, `image.rs`).
//!
//! Everything here is pure and refuses hostile files with a reason instead of panicking (the file comes from a
//! program or the network): header and program-header table inside the file, segments inside the file and
//! inside user space, `p_offset` and `p_vaddr` congruent modulo the page (the condition Linux's `mmap` of a
//! segment needs), sizes that do not overflow.

use alloc::vec::Vec;

pub const PAGE: u64 = 4096;
/// Linux's `mmap_min_addr` default: nothing is mapped below it.
pub const MIN_ADDR: u64 = 0x10000;
/// The largest segment alignment honoured for static-PIE placement (a bound against hostile values; Linux
/// takes any power of two and then fails in mmap).
pub const MAX_ALIGN: u64 = 1 << 30;
/// Linux's `PATH_MAX`: the longest PT_INTERP string (NUL included).
pub const PATH_MAX: u64 = 4096;
/// `load_elf_phdrs`: the program header table is at most `ELF_MIN_ALIGN` (one page) large.
pub const MAX_PHDRS_SIZE: u64 = PAGE;

pub const PT_LOAD: u32 = 1;
pub const PT_INTERP: u32 = 3;
pub const PT_PHDR: u32 = 6;
pub const PT_TLS: u32 = 7;
pub const PT_GNU_STACK: u32 = 0x6474_e551;
pub const PF_X: u32 = 1;
pub const PF_W: u32 = 2;
pub const PF_R: u32 = 4;
pub const ET_EXEC: u16 = 2;
pub const ET_DYN: u16 = 3;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Load {
    pub vaddr: u64,
    pub offset: u64,
    pub filesz: u64,
    pub memsz: u64,
    pub flags: u32,
    pub align: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Elf {
    pub entry: u64,
    /// `ET_DYN`: placed at a load bias chosen by the loader.
    pub is_pie: bool,
    /// Where the program headers are in the (unbiased) image: inside the PT_LOAD that holds `e_phoff`
    /// (Linux's rule), else PT_PHDR's address, else 0.
    pub phdr_vaddr: u64,
    pub phoff: u64,
    pub phnum: u16,
    pub loads: Vec<Load>,
    /// PT_INTERP's path (the first PT_INTERP, as Linux; without the NUL).
    pub interp: Option<Vec<u8>>,
    /// PT_GNU_STACK with PF_X (an executable stack was asked for).
    pub exec_stack: bool,
    /// Largest power-of-two `p_align` of the PT_LOADs (at least a page).
    pub max_align: u64,
}

fn u16_at(b: &[u8], o: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(o..o.checked_add(2)?)?.try_into().ok()?))
}
fn u32_at(b: &[u8], o: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(o..o.checked_add(4)?)?.try_into().ok()?))
}
fn u64_at(b: &[u8], o: usize) -> Option<u64> {
    Some(u64::from_le_bytes(b.get(o..o.checked_add(8)?)?.try_into().ok()?))
}

/// Why a file is not a program V2 can start (the text goes into the spawn error and the log).
pub type ElfError = &'static str;
/// The file ends before the headers do (or a read failed): an interpreter's EIO, as Linux's `elf_read`.
pub const SHORT_READ: ElfError = "short file";
/// Another type than ET_EXEC/ET_DYN: an interpreter's EINVAL (`load_elf_interp`).
pub const NOT_PROGRAM_TYPE: ElfError = "not an executable (ET_EXEC or ET_DYN)";

impl Elf {
    /// Reads the ELF header and program headers of a file held in memory. `user_top` is the end of user space.
    pub fn parse(b: &[u8], user_top: u64) -> Result<Elf, ElfError> {
        Self::parse_from(b.len() as u64, &mut |off, buf| {
            let s = usize::try_from(off).ok().and_then(|o| b.get(o..o.checked_add(buf.len())?)).ok_or(SHORT_READ)?;
            buf.copy_from_slice(s);
            Ok(())
        }, user_top)
    }

    /// The same for a file of `len` bytes read through `read(offset, buf)` (fills all of `buf` or fails).
    /// Linux's checks (fs/binfmt_elf.c `load_elf_binary`, `load_elf_phdrs`, the PT_INTERP read :878-896).
    pub fn parse_from(len: u64, read: &mut dyn FnMut(u64, &mut [u8]) -> Result<(), ElfError>, user_top: u64) -> Result<Elf, ElfError> {
        Self::parse_impl(len, read, user_top, true)
    }

    /// The same for an interpreter: its own PT_INTERP is neither read nor checked (`load_elf_interp` never
    /// looks at it; Linux loads one level).
    pub fn parse_interp_from(len: u64, read: &mut dyn FnMut(u64, &mut [u8]) -> Result<(), ElfError>, user_top: u64) -> Result<Elf, ElfError> {
        Self::parse_impl(len, read, user_top, false)
    }

    fn parse_impl(len: u64, read: &mut dyn FnMut(u64, &mut [u8]) -> Result<(), ElfError>, user_top: u64, want_interp: bool) -> Result<Elf, ElfError> {
        let mut h = [0u8; 64];
        if len < 64 {
            return Err(SHORT_READ);
        }
        read(0, &mut h)?;
        let b = &h[..];
        if &b[0..4] != b"\x7fELF" {
            return Err("not an ELF file");
        }
        if b[4] != 2 || b[5] != 1 {
            return Err("not ELF64 little endian");
        }
        if b[6] != 1 {
            return Err("unknown ELF version");
        }
        let ty = u16_at(b, 16).ok_or("short header")?;
        if u16_at(b, 18) != Some(0x3E) {
            return Err("not x86-64");
        }
        if ty != ET_EXEC && ty != ET_DYN {
            return Err(NOT_PROGRAM_TYPE);
        }
        let entry = u64_at(b, 24).ok_or("short header")?;
        let phoff = u64_at(b, 32).ok_or("short header")?;
        let phentsize = u16_at(b, 54).ok_or("short header")? as u64;
        let phnum = u16_at(b, 56).ok_or("short header")?;
        if phentsize != 56 {
            return Err("bad program header size");
        }
        if phnum == 0 || phnum == 0xFFFF {
            return Err("no program headers");
        }
        let size = phnum as u64 * 56;
        if size > MAX_PHDRS_SIZE {
            return Err("program header table larger than a page");
        }
        let table_end = phoff.checked_add(size).ok_or("program headers outside the file")?;
        if table_end > len {
            return Err("program headers outside the file");
        }
        let mut ph: Vec<u8> = Vec::new();
        ph.try_reserve_exact(size as usize).map_err(|_| "no memory for the program headers")?;
        ph.resize(size as usize, 0);
        read(phoff, &mut ph).map_err(|_| "program headers outside the file")?;
        let b = &ph[..];
        let mut loads: Vec<Load> = Vec::new();
        loads.try_reserve(phnum as usize).map_err(|_| "no memory for the program headers")?;
        let mut pt_phdr = None;
        let mut interp = None;
        let mut exec_stack = false;
        let mut max_align = PAGE;
        for i in 0..phnum as usize {
            let o = i * 56;
            let p_type = u32_at(b, o).ok_or("short program header")?;
            let flags = u32_at(b, o + 4).ok_or("short program header")?;
            let offset = u64_at(b, o + 8).ok_or("short program header")?;
            let vaddr = u64_at(b, o + 16).ok_or("short program header")?;
            let filesz = u64_at(b, o + 32).ok_or("short program header")?;
            let memsz = u64_at(b, o + 40).ok_or("short program header")?;
            let align = u64_at(b, o + 48).ok_or("short program header")?;
            match p_type {
                PT_LOAD => {
                    if memsz == 0 {
                        // Linux maps nothing for an empty segment.
                        continue;
                    }
                    if filesz > memsz {
                        return Err("segment file size larger than its memory size");
                    }
                    if offset.checked_add(filesz).is_none_or(|e| e > len) {
                        return Err("segment outside the file");
                    }
                    if offset % PAGE != vaddr % PAGE {
                        return Err("segment offset and address differ modulo the page size");
                    }
                    if vaddr.checked_add(memsz).and_then(|e| e.checked_add(PAGE)).is_none_or(|e| e > user_top) {
                        return Err("segment outside user space");
                    }
                    if align.is_power_of_two() && align > max_align {
                        if align > MAX_ALIGN {
                            return Err("segment alignment above 1 GiB");
                        }
                        max_align = align;
                    }
                    loads.push(Load { vaddr, offset, filesz, memsz, flags, align });
                }
                PT_INTERP if want_interp && interp.is_none() => {
                    // Linux: 2..=PATH_MAX bytes, read from the file, the last one NUL (else ENOEXEC).
                    if !(2..=PATH_MAX).contains(&filesz) {
                        return Err("PT_INTERP path size not in 2..=4096");
                    }
                    if offset.checked_add(filesz).is_none_or(|e| e > len) {
                        return Err("PT_INTERP outside the file");
                    }
                    let mut p: Vec<u8> = Vec::new();
                    p.try_reserve_exact(filesz as usize).map_err(|_| "no memory for the PT_INTERP path")?;
                    p.resize(filesz as usize, 0);
                    read(offset, &mut p).map_err(|_| "PT_INTERP outside the file")?;
                    if p.pop() != Some(0) {
                        return Err("PT_INTERP path not NUL-terminated");
                    }
                    // `open_exec` takes it as a C string: the path ends at the first NUL.
                    if let Some(n) = p.iter().position(|&c| c == 0) {
                        p.truncate(n);
                    }
                    interp = Some(p);
                }
                PT_PHDR => pt_phdr = Some(vaddr),
                PT_GNU_STACK => exec_stack = flags & PF_X != 0,
                _ => {}
            }
        }
        if loads.is_empty() {
            return Err("no loadable segment");
        }
        // Linux (binfmt_elf): AT_PHDR comes from the PT_LOAD whose file range holds e_phoff.
        let in_load = loads.iter().find(|l| l.offset <= phoff && phoff < l.offset + l.filesz).map(|l| phoff - l.offset + l.vaddr);
        let phdr_vaddr = in_load.or(pt_phdr).unwrap_or(0);
        Ok(Elf { entry, is_pie: ty == ET_DYN, phdr_vaddr, phoff, phnum, loads, interp, exec_stack, max_align })
    }

    /// Lowest page and end (page-aligned) of all PT_LOADs, unbiased.
    pub fn span(&self) -> (u64, u64) {
        let lo = self.loads.iter().map(|l| l.vaddr & !(PAGE - 1)).min().unwrap_or(0);
        let hi = self.loads.iter().map(|l| (l.vaddr + l.memsz).next_multiple_of(PAGE)).max().unwrap_or(0);
        (lo, hi)
    }
}

/// A test ELF: (type, entry, [(p_type, flags, offset, vaddr, filesz, memsz, align)]), file `size` bytes.
#[cfg(test)]
pub(crate) fn build_test_elf(ty: u16, entry: u64, phs: &[(u32, u32, u64, u64, u64, u64, u64)], size: usize) -> Vec<u8> {
    let mut b = alloc::vec![0u8; size.max(64 + 56 * phs.len())];
    b[0..4].copy_from_slice(b"\x7fELF");
    b[4] = 2;
    b[5] = 1;
    b[6] = 1;
    b[16..18].copy_from_slice(&ty.to_le_bytes());
    b[18..20].copy_from_slice(&0x3Eu16.to_le_bytes());
    b[24..32].copy_from_slice(&entry.to_le_bytes());
    b[32..40].copy_from_slice(&64u64.to_le_bytes());
    b[54..56].copy_from_slice(&56u16.to_le_bytes());
    b[56..58].copy_from_slice(&(phs.len() as u16).to_le_bytes());
    for (i, p) in phs.iter().enumerate() {
        let o = 64 + i * 56;
        b[o..o + 4].copy_from_slice(&p.0.to_le_bytes());
        b[o + 4..o + 8].copy_from_slice(&p.1.to_le_bytes());
        b[o + 8..o + 16].copy_from_slice(&p.2.to_le_bytes());
        b[o + 16..o + 24].copy_from_slice(&p.3.to_le_bytes());
        b[o + 32..o + 40].copy_from_slice(&p.4.to_le_bytes());
        b[o + 40..o + 48].copy_from_slice(&p.5.to_le_bytes());
        b[o + 48..o + 56].copy_from_slice(&p.6.to_le_bytes());
    }
    b
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOP: u64 = 0x0000_7FFF_FFFF_F000;
    use build_test_elf as build;

    #[test]
    fn static_exec() {
        let b = build(ET_EXEC, 0x401000, &[(PT_LOAD, PF_R | PF_X, 0, 0x400000, 0x2000, 0x2000, 0x1000), (PT_LOAD, PF_R | PF_W, 0x2000, 0x402000, 0x100, 0x3000, 0x1000), (PT_GNU_STACK, PF_R | PF_W, 0, 0, 0, 0, 16)], 0x2100);
        let e = Elf::parse(&b, TOP).unwrap();
        assert!(!e.is_pie && !e.exec_stack && e.interp.is_none());
        assert_eq!(e.loads.len(), 2);
        assert_eq!(e.phdr_vaddr, 0x400040, "AT_PHDR from the PT_LOAD holding e_phoff");
        assert_eq!(e.span(), (0x400000, 0x405000));
        assert_eq!(e.phnum, 3);
    }

    #[test]
    fn static_pie_and_phdr_rules() {
        // Headers not inside a PT_LOAD: PT_PHDR decides.
        let b = build(ET_DYN, 0x1000, &[(PT_PHDR, PF_R, 64, 0x40, 0xa8, 0xa8, 8), (PT_LOAD, PF_R | PF_X, 0x1000, 0x1000, 0x10, 0x10, 0x1000)], 0x1010);
        let e = Elf::parse(&b, TOP).unwrap();
        assert!(e.is_pie);
        assert_eq!(e.phdr_vaddr, 0x40);
        let b = build(ET_DYN, 0, &[(PT_LOAD, PF_R | PF_X, 0, 0, 0x200, 0x200, 0x20_0000), (PT_GNU_STACK, PF_R | PF_W | PF_X, 0, 0, 0, 0, 0)], 0x200);
        let e = Elf::parse(&b, TOP).unwrap();
        assert_eq!(e.max_align, 0x20_0000);
        assert!(e.exec_stack);
    }

    #[test]
    fn interp_path() {
        let mut b = build(ET_DYN, 0x1000, &[(PT_INTERP, PF_R, 0x800, 0, 4, 4, 1), (PT_INTERP, PF_R, 0x900, 0, 4, 4, 1), (PT_LOAD, PF_R, 0, 0, 0x1000, 0x1000, 0x1000)], 0x1000);
        b[0x800..0x804].copy_from_slice(b"/ld\0");
        b[0x900..0x904].copy_from_slice(b"/xx\0");
        assert_eq!(Elf::parse(&b, TOP).unwrap().interp.as_deref(), Some(&b"/ld"[..]), "the first PT_INTERP counts");
        b[0x803] = b'x';
        assert_eq!(Elf::parse(&b, TOP).err(), Some("PT_INTERP path not NUL-terminated"));
        // Read through a reader that sees only what is asked for (the program file is never read whole).
        b[0x803] = 0;
        let mut asked = Vec::new();
        let e = Elf::parse_from(b.len() as u64, &mut |o, buf| {
            asked.push((o, buf.len()));
            buf.copy_from_slice(&b[o as usize..o as usize + buf.len()]);
            Ok(())
        }, TOP).unwrap();
        assert_eq!(e.interp.as_deref(), Some(&b"/ld"[..]));
        assert_eq!(asked, [(0, 64), (64, 3 * 56), (0x800, 4)]);
        // An interpreter's own PT_INTERP is not read, even a broken one.
        b[0x803] = b'x';
        let mut asked = Vec::new();
        let e = Elf::parse_interp_from(b.len() as u64, &mut |o, buf| {
            asked.push((o, buf.len()));
            buf.copy_from_slice(&b[o as usize..o as usize + buf.len()]);
            Ok(())
        }, TOP).unwrap();
        assert_eq!((e.interp, asked), (None, alloc::vec![(0, 64), (64, 3 * 56)]));
        // A C string: the path ends at the first NUL (an empty one stays empty: ENOENT at the lookup).
        b[0x800..0x804].copy_from_slice(b"/a\0\0");
        assert_eq!(Elf::parse(&b, TOP).unwrap().interp.as_deref(), Some(&b"/a"[..]));
        b[0x800..0x804].copy_from_slice(b"\0ab\0");
        assert_eq!(Elf::parse(&b, TOP).unwrap().interp.as_deref(), Some(&b""[..]));
    }

    #[test]
    fn hostile_files_are_refused() {
        let ok = |phs: &[(u32, u32, u64, u64, u64, u64, u64)], size| Elf::parse(&build(ET_EXEC, 0x400000, phs, size), TOP);
        assert_eq!(Elf::parse(b"\x7fELF", TOP).err(), Some(SHORT_READ));
        assert_eq!(Elf::parse(&[0u8; 100], TOP).err(), Some("not an ELF file"));
        assert_eq!(ok(&[(PT_LOAD, PF_R, 0, 0x400000, 0x2000, 0x1000, 0)], 0x2000).err(), Some("segment file size larger than its memory size"));
        assert_eq!(ok(&[(PT_LOAD, PF_R, 0, 0x400000, 0x2000, 0x2000, 0)], 0x1000).err(), Some("segment outside the file"));
        assert_eq!(ok(&[(PT_LOAD, PF_R, u64::MAX - 4, 0x400000, 0x10, 0x10, 0)], 0x1000).err(), Some("segment outside the file"));
        assert_eq!(ok(&[(PT_LOAD, PF_R, 0x10, 0x400000, 0x10, 0x10, 0)], 0x1000).err(), Some("segment offset and address differ modulo the page size"));
        assert_eq!(ok(&[(PT_LOAD, PF_R, 0, TOP - 0x1000, 0x10, 0x10, 0)], 0x1000).err(), Some("segment outside user space"));
        assert_eq!(ok(&[(PT_LOAD, PF_R, 0, 0xFFFF_8000_0000_0000, 0x10, 0x10, 0)], 0x1000).err(), Some("segment outside user space"));
        assert_eq!(ok(&[(PT_LOAD, PF_R, 0, 0x400000, 0x10, u64::MAX, 0)], 0x1000).err(), Some("segment outside user space"));
        assert_eq!(ok(&[(PT_LOAD, PF_R, 0, 0x400000, 0x10, 0x10, 1 << 40)], 0x1000).err(), Some("segment alignment above 1 GiB"));
        assert_eq!(ok(&[(PT_INTERP, PF_R, 0, 0, 0, 0, 0), (PT_LOAD, PF_R, 0, 0x400000, 0x10, 0x10, 0)], 0x1000).err(), Some("PT_INTERP path size not in 2..=4096"));
        assert_eq!(ok(&[(PT_INTERP, PF_R, 0x800, 0, 4097, 0, 0), (PT_LOAD, PF_R, 0, 0x400000, 0x10, 0x10, 0)], 0x2000).err(), Some("PT_INTERP path size not in 2..=4096"));
        assert_eq!(ok(&[(PT_INTERP, PF_R, 0xFF0, 0, 0x20, 0, 0), (PT_LOAD, PF_R, 0, 0x400000, 0x10, 0x10, 0)], 0x1000).err(), Some("PT_INTERP outside the file"));
        assert_eq!(ok(&[(PT_LOAD, PF_R, 0, 0x400000, 0, 0, 0)], 0x1000).err(), Some("no loadable segment"));
        let mut b = build(ET_EXEC, 0, &[(PT_LOAD, PF_R, 0, 0x400000, 0x10, 0x10, 0)], 0x1000);
        b[56..58].copy_from_slice(&0x400u16.to_le_bytes());
        assert_eq!(Elf::parse(&b, TOP).err(), Some("program header table larger than a page"), "Linux: at most ELF_MIN_ALIGN bytes");
        b[56..58].copy_from_slice(&73u16.to_le_bytes());
        assert_eq!(Elf::parse(&b, TOP).err(), Some("program headers outside the file"), "73 x 56 bytes fit a page, the file is too short");
        b[32..40].copy_from_slice(&u64::MAX.to_le_bytes());
        assert_eq!(Elf::parse(&b, TOP).err(), Some("program headers outside the file"));
        let mut b = build(ET_EXEC, 0, &[(PT_LOAD, PF_R, 0, 0x400000, 0x10, 0x10, 0)], 0x1000);
        b[16] = 1; // ET_REL
        assert!(Elf::parse(&b, TOP).is_err());
        b[16] = 2;
        b[18] = 3; // i386
        assert_eq!(Elf::parse(&b, TOP).err(), Some("not x86-64"));
    }

    #[test]
    fn every_truncation_and_garbling_is_refused_without_panic() {
        let b = build(ET_EXEC, 0x401000, &[(PT_LOAD, PF_R | PF_X, 0, 0x400000, 0x200, 0x200, 0x1000), (PT_LOAD, PF_R | PF_W, 0x200, 0x401200, 0x10, 0x20, 0x1000)], 0x210);
        for n in 0..b.len() {
            let _ = Elf::parse(&b[..n], TOP);
        }
        let mut x = 0x1234_5678_9abc_def0u64;
        for _ in 0..5000 {
            let mut c = b.clone();
            for _ in 0..8 {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                let i = (x as usize) % c.len();
                c[i] = (x >> 32) as u8;
            }
            if let Ok(e) = Elf::parse(&c, TOP) {
                // Whatever passes is consistent: every segment inside the file and user space.
                for l in &e.loads {
                    assert!(l.offset + l.filesz <= c.len() as u64 && l.vaddr + l.memsz < TOP && l.filesz <= l.memsz);
                }
            }
        }
    }
}
