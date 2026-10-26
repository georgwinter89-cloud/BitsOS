//! A bundle = one plug-over's files: KERNEL + INITRD (+ bitsboot if it changed) (V2-PLAN §7.1). Checked before
//! any plan, so a file bitsboot would refuse never reaches the stick.
use super::gate::{V2_LOADER_MARK, contains};
use std::path::{Path, PathBuf};

pub struct Sources {
    pub kernel: PathBuf,
    pub initrd: PathBuf,
    pub bitsboot: Option<PathBuf>,
}

impl Sources {
    /// `DIR/KERNEL`, `DIR/INITRD` and, if present, `DIR/BOOTX64.EFI`.
    pub fn from_dir(d: &Path) -> Sources {
        let b = d.join("BOOTX64.EFI");
        Sources { kernel: d.join("KERNEL"), initrd: d.join("INITRD"), bitsboot: b.is_file().then_some(b) }
    }
}

pub struct Bundle {
    pub kernel: Vec<u8>,
    pub kernel_from: PathBuf,
    pub initrd: Vec<u8>,
    pub initrd_from: PathBuf,
    pub bitsboot: Option<(Vec<u8>, PathBuf)>,
}

fn read(p: &Path) -> Result<Vec<u8>, String> {
    std::fs::read(p).map_err(|e| format!("bundle: {}: {e}", p.display()))
}

pub fn load(s: &Sources) -> Result<Bundle, String> {
    let kernel = read(&s.kernel)?;
    check_kernel(&kernel).map_err(|e| format!("bundle: {} is no BitsOS kernel: {e}", s.kernel.display()))?;
    let initrd = read(&s.initrd)?;
    check_initrd(&initrd).map_err(|e| format!("bundle: {} is no initramfs: {e}", s.initrd.display()))?;
    let bitsboot = match &s.bitsboot {
        Some(p) => {
            let b = read(p)?;
            check_bitsboot(&b).map_err(|e| format!("bundle: {} is no V2 bitsboot: {e}", p.display()))?;
            Some((b, p.clone()))
        }
        None => None,
    };
    Ok(Bundle { kernel, kernel_from: s.kernel.clone(), initrd, initrd_from: s.initrd.clone(), bitsboot })
}

fn u16_at(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}
fn u32_at(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}
fn u64_at(b: &[u8], o: usize) -> u64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(&b[o..o + 8]);
    u64::from_le_bytes(a)
}

/// Every check of bitsboot's `elf::parse` (boot/bitsboot/src/pure/elf.rs), in its order: x86_64 ELF64
/// executable, PT_LOAD segments in the file, filesz <= memsz, in the kernel's top 2 GB without wrapping when
/// rounded to a page, empty segments dropped, no overlap, no page shared by different write/execute rights,
/// a span of at most 1 GiB, the entry in an executable segment. A kernel bitsboot refuses at boot must never
/// get onto the stick (with --proven it would be installed as OK). Until xtask can depend on bitsboot_pure
/// (change request), a change there must be copied here; `kernel_checks_match_bitsboot` pins the cases.
pub fn check_kernel(b: &[u8]) -> Result<(), String> {
    use bootinfo::layout::KERNEL_BASE;
    const PAGE: u64 = 4096;
    const MAX_SPAN: u64 = 1 << 30;
    const PF_X: u32 = 1;
    const PF_W: u32 = 2;
    if b.len() < 64 {
        return Err("too short for an ELF header (under 64 bytes)".into());
    }
    if b[0..4] != *b"\x7fELF" {
        return Err("not an ELF file (bad magic)".into());
    }
    if b[4] != 2 || b[5] != 1 || u16_at(b, 18) != 0x3E {
        return Err("not a 64-bit little-endian x86_64 ELF".into());
    }
    if u16_at(b, 16) != 2 {
        return Err("not an executable (ELF type is not EXEC)".into());
    }
    let (phoff, phentsize, phnum) = (u64_at(b, 32), u16_at(b, 54) as u64, u16_at(b, 56) as u64);
    if phentsize != 56 || phoff.checked_add(phnum * 56).is_none_or(|e| e > b.len() as u64) {
        return Err("program headers lie outside the file".into());
    }
    // (vaddr, memsz, flags) of every PT_LOAD with memory.
    let mut segs: Vec<(u64, u64, u32)> = Vec::new();
    for i in 0..phnum {
        let h = &b[(phoff + i * 56) as usize..][..56];
        if u32_at(h, 0) != 1 {
            continue;
        }
        let (flags, off, vaddr, filesz, memsz) = (u32_at(h, 4), u64_at(h, 8), u64_at(h, 16), u64_at(h, 32), u64_at(h, 40));
        if off.checked_add(filesz).is_none_or(|e| e > b.len() as u64) {
            return Err("a segment lies outside the file (cut short?)".into());
        }
        if filesz > memsz {
            return Err("a segment has more file bytes than memory bytes".into());
        }
        if vaddr < KERNEL_BASE {
            return Err("a segment lies below the kernel base 0xFFFFFFFF80000000".into());
        }
        if vaddr.checked_add(memsz).and_then(|e| e.checked_next_multiple_of(PAGE)).is_none() {
            return Err("a segment runs past the end of the address space".into());
        }
        if memsz > 0 {
            segs.push((vaddr, memsz, flags));
        }
    }
    if segs.is_empty() {
        return Err("no loadable segments".into());
    }
    segs.sort_unstable_by_key(|s| s.0);
    let rights = |f: u32| f & (PF_W | PF_X);
    for p in segs.windows(2) {
        let ((av, am, af), (bv, _, bf)) = (p[0], p[1]);
        let a_end = av + am; // checked above
        if bv < a_end {
            return Err("two segments overlap".into());
        }
        if rights(af) != rights(bf) && bv & !(PAGE - 1) < a_end.next_multiple_of(PAGE) {
            return Err("two segments with different write/execute rights share a page".into());
        }
    }
    let lo = segs.iter().map(|s| s.0).min().unwrap_or(0) & !(PAGE - 1);
    let hi = segs.iter().map(|s| (s.0 + s.1).next_multiple_of(PAGE)).max().unwrap_or(0);
    if hi - lo > MAX_SPAN {
        return Err("the segments span more than 1 GiB".into());
    }
    let entry = u64_at(b, 24);
    if !segs.iter().any(|&(v, m, f)| f & PF_X != 0 && entry >= v && entry < v + m) {
        return Err("the entry point lies outside every executable segment".into());
    }
    Ok(())
}

/// bitsboot `gens::check_initrd`: an uncompressed cpio newc archive (the kernel unpacks nothing else).
pub fn check_initrd(b: &[u8]) -> Result<(), String> {
    if b.len() < 110 {
        return Err("too short for a cpio header".into());
    }
    if &b[..6] != b"070701" && &b[..6] != b"070702" {
        return Err("not a cpio newc archive".into());
    }
    Ok(())
}

/// A PE32+ EFI application for x86_64 (V1 `stick-vm.mjs` peProblems) that names itself V2's loader.
pub fn check_bitsboot(b: &[u8]) -> Result<(), String> {
    if b.len() < 0x40 || &b[..2] != b"MZ" {
        return Err("not a PE file (no MZ)".into());
    }
    let pe = u32_at(b, 0x3C) as usize;
    if pe.checked_add(24 + 70).is_none_or(|e| e > b.len()) || &b[pe..pe + 4] != b"PE\0\0" {
        return Err("no PE header".into());
    }
    if u16_at(b, pe + 4) != 0x8664 {
        return Err("not built for x86_64".into());
    }
    if u16_at(b, pe + 24) != 0x20B {
        return Err("not PE32+".into());
    }
    if u16_at(b, pe + 24 + 68) != 10 {
        return Err("not an EFI application".into());
    }
    if !contains(b, V2_LOADER_MARK) {
        return Err("does not name itself 'BitsOS V2 loader'".into());
    }
    Ok(())
}

/// A CONFIG.TXT the tool may put on a stick that has none: never one that sets `nvme_write`, `stick_write` or
/// `usb_msd_write` (R8a: only Georg's own edit turns them on; `wlan` is free since 21.10., R8d).
pub fn check_config(text: &[u8]) -> Result<(), String> {
    for raw in text.split(|&c| c == b'\n') {
        let line = String::from_utf8_lossy(raw);
        let line = line.split('#').next().unwrap_or("");
        if let Some((k, _)) = line.split_once('=') {
            let k = k.trim();
            // R8a (FS-6, US-4): stick_write and usb_msd_write open USB disks for writing.
            if ["nvme_write", "stick_write", "usb_msd_write"].iter().any(|g| k.eq_ignore_ascii_case(g)) {
                return Err(format!("it sets {k}: only Georg turns that on, by his own edit (R8)"));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
pub mod tests {
    use super::*;

    /// A minimal kernel ELF the checks accept: one PT_LOAD R+X at the kernel base holding the entry.
    pub fn fake_kernel(tag: &[u8]) -> Vec<u8> {
        let mut b = vec![0u8; 64 + 56];
        b[0..4].copy_from_slice(b"\x7fELF");
        b[4] = 2;
        b[5] = 1;
        b[16..18].copy_from_slice(&2u16.to_le_bytes());
        b[18..20].copy_from_slice(&0x3Eu16.to_le_bytes());
        b[24..32].copy_from_slice(&0xFFFF_FFFF_8000_0010u64.to_le_bytes());
        b[32..40].copy_from_slice(&64u64.to_le_bytes());
        b[54..56].copy_from_slice(&56u16.to_le_bytes());
        b[56..58].copy_from_slice(&1u16.to_le_bytes());
        let h = 64;
        b[h..h + 4].copy_from_slice(&1u32.to_le_bytes());
        b[h + 4..h + 8].copy_from_slice(&5u32.to_le_bytes());
        b[h + 16..h + 24].copy_from_slice(&0xFFFF_FFFF_8000_0000u64.to_le_bytes());
        b[h + 40..h + 48].copy_from_slice(&0x1000u64.to_le_bytes());
        b.extend_from_slice(tag);
        b
    }

    pub fn fake_initrd(tag: &[u8]) -> Vec<u8> {
        let mut b = b"070701".to_vec();
        b.resize(110, b'0');
        b.extend_from_slice(tag);
        b
    }

    pub fn fake_bitsboot(tag: &[u8]) -> Vec<u8> {
        let mut b = vec![0u8; 0x200];
        b[..2].copy_from_slice(b"MZ");
        b[0x3C..0x40].copy_from_slice(&0x80u32.to_le_bytes());
        b[0x80..0x84].copy_from_slice(b"PE\0\0");
        b[0x84..0x86].copy_from_slice(&0x8664u16.to_le_bytes());
        b[0x98..0x9A].copy_from_slice(&0x20Bu16.to_le_bytes());
        b[0x98 + 68..0x98 + 70].copy_from_slice(&10u16.to_le_bytes());
        b.extend_from_slice(b"bitsboot: BitsOS V2 loader (BT-1)");
        b.extend_from_slice(tag);
        b
    }

    #[test]
    fn kernel_checks() {
        assert!(check_kernel(&fake_kernel(b"x")).is_ok());
        let k = fake_kernel(b"");
        let mut bad = k.clone();
        bad[0] = 0;
        assert!(check_kernel(&bad).is_err());
        let mut bad = k.clone();
        bad[16] = 3; // ET_DYN
        assert!(check_kernel(&bad).is_err());
        let mut bad = k.clone();
        bad[64 + 4] = 4; // R only: entry not executable
        assert!(check_kernel(&bad).is_err());
        let mut bad = k.clone();
        bad[64 + 32..64 + 40].copy_from_slice(&u64::MAX.to_le_bytes()); // filesz past the end
        assert!(check_kernel(&bad).is_err());
        let mut bad = k.clone();
        bad[56..58].copy_from_slice(&0xFFFFu16.to_le_bytes()); // phnum huge
        assert!(check_kernel(&bad).is_err());
        assert!(check_kernel(&k[..100]).is_err());
        assert!(check_kernel(b"").is_err());
        let real = crate::util::root().join("target/x86_64-unknown-none/debug/kernel");
        if let Ok(b) = std::fs::read(&real) {
            assert!(check_kernel(&b).is_ok(), "the built kernel passes");
        }
    }

    fn set_u64(b: &mut [u8], o: usize, v: u64) {
        b[o..o + 8].copy_from_slice(&v.to_le_bytes());
    }

    /// bitsboot's `elf::tests::sample`: an RX text segment with 64 code bytes at the kernel base and an RW
    /// bss segment 2 MiB up. Program headers at 64 (text) and 120 (data), code at 176.
    fn two_segments() -> Vec<u8> {
        let mut b = fake_kernel(b"");
        b.truncate(64);
        b[56..58].copy_from_slice(&2u16.to_le_bytes());
        b.resize(176, 0);
        let ph = |b: &mut Vec<u8>, h: usize, flags: u32, vaddr: u64, filesz: u64, memsz: u64| {
            b[h..h + 4].copy_from_slice(&1u32.to_le_bytes());
            b[h + 4..h + 8].copy_from_slice(&flags.to_le_bytes());
            set_u64(b, h + 8, 176);
            set_u64(b, h + 16, vaddr);
            set_u64(b, h + 32, filesz);
            set_u64(b, h + 40, memsz);
        };
        ph(&mut b, 64, 5, 0xFFFF_FFFF_8000_0000, 64, 64);
        ph(&mut b, 120, 6, 0xFFFF_FFFF_8020_0000, 0, 0x3000);
        b.extend_from_slice(&[0x90; 64]);
        b
    }

    #[test]
    fn kernel_checks_match_bitsboot() {
        let good = two_segments();
        assert_eq!(check_kernel(&good), Ok(()));
        let (text, data) = (64, 120);
        let err = |b: &[u8]| check_kernel(b).unwrap_err();
        // filesz > memsz.
        let mut b = good.clone();
        set_u64(&mut b, text + 40, 32);
        assert_eq!(err(&b), "a segment has more file bytes than memory bytes");
        // The RW segment moved onto the text page: W and X rights share a page.
        let mut b = good.clone();
        set_u64(&mut b, data + 16, 0xFFFF_FFFF_8000_0800);
        assert_eq!(err(&b), "two segments with different write/execute rights share a page");
        // The RW segment starts inside the text bytes.
        let mut b = good.clone();
        set_u64(&mut b, data + 16, 0xFFFF_FFFF_8000_0020);
        assert_eq!(err(&b), "two segments overlap");
        // Same rights on one page are fine, in any header order.
        let mut b = good.clone();
        set_u64(&mut b, data + 16, 0xFFFF_FFFF_8000_0800);
        b[data + 4..data + 8].copy_from_slice(&5u32.to_le_bytes());
        assert_eq!(check_kernel(&b), Ok(()));
        let mut b = good.clone();
        let t: Vec<u8> = b[text..data].to_vec();
        b.copy_within(data..data + 56, text);
        b[data..data + 56].copy_from_slice(&t);
        assert_eq!(check_kernel(&b), Ok(()));
        // A 1.25 GiB span; a segment that wraps when rounded up to a page.
        let mut b = good.clone();
        set_u64(&mut b, data + 40, 0x5000_0000);
        assert_eq!(err(&b), "the segments span more than 1 GiB");
        let mut b = good.clone();
        set_u64(&mut b, data + 16, u64::MAX - 0x1000);
        set_u64(&mut b, data + 40, 0x800);
        assert_eq!(err(&b), "a segment runs past the end of the address space");
        // An empty segment is dropped: an entry only inside it lies outside every segment.
        let mut b = good.clone();
        set_u64(&mut b, text + 32, 0);
        set_u64(&mut b, text + 40, 0);
        assert_eq!(err(&b), "the entry point lies outside every executable segment");
        let mut b = good.clone();
        set_u64(&mut b, text + 16, 0x40_0000);
        assert!(err(&b).contains("below the kernel base"));
        // Every cut and random byte flips: an answer, never a panic.
        for n in 0..good.len() {
            assert!(check_kernel(&good[..n]).is_err(), "cut at {n}");
        }
        let mut x: u64 = 0x1234_5678;
        for _ in 0..20_000 {
            let mut b = good.clone();
            for _ in 0..4 {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                let i = (x as usize) % b.len();
                b[i] = (x >> 32) as u8;
            }
            let _ = check_kernel(&b);
        }
    }

    #[test]
    fn initrd_bitsboot_config_checks() {
        assert!(check_initrd(&fake_initrd(b"")).is_ok());
        assert!(check_initrd(b"\x1f\x8b gzip").is_err());
        assert!(check_bitsboot(&fake_bitsboot(b"")).is_ok());
        let mut b = fake_bitsboot(b"");
        b[0x98 + 68] = 11; // boot service driver
        assert!(check_bitsboot(&b).is_err());
        let mut b = fake_bitsboot(b"");
        b.truncate(0x200); // without the name
        assert!(check_bitsboot(&b).is_err());
        let mut b = fake_bitsboot(b"");
        b[0x3C..0x40].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(check_bitsboot(&b).is_err());
        let real = crate::util::root().join("target/x86_64-unknown-uefi/debug/bitsboot.efi");
        if let Ok(b) = std::fs::read(&real) {
            assert!(check_bitsboot(&b).is_ok(), "the built bitsboot passes");
        }
        assert!(check_config(b"loglevel=info\n# nvme_write=on is Georg's\npanic=reboot\n").is_ok());
        assert!(check_config(b"loglevel=info\n nvme_write = on\n").is_err());
        assert!(check_config(b"loglevel=info\nwlan=on\n").is_ok());
        assert!(check_config(b"NVME_WRITE=off\n").is_err());
        assert!(check_config(b"stick_write=on\n").is_err());
        assert!(check_config(b"usb_msd_write=on\n").is_err());
    }
}
