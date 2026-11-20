//! What `install_kernel` checks before anything reaches the stick (pure; host-tested). A bundle passes only if
//! bitsboot would start it at the next boot (its own `elf::parse` and INITRD rule, from `bitsboot_pure`), the
//! kernel is laid out like a BitsOS kernel (Reload's header checks, segments from `KERNEL_BASE`, the kernel's
//! banner inside a loaded segment), and the initramfs is a whole cpio newc archive (every entry inside the file,
//! ended by `TRAILER!!!`), so a cut-off download never becomes a generation.

use crate::Fnv;
use crate::elf::{self, ElfError};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use bootinfo::layout::KERNEL_BASE;

/// Bytes every BitsOS V2 kernel carries in its read-only data (the entry line `kernel/src/main.rs` prints).
/// A Linux `vmlinux` or another loader's ELF at the kernel base passes the ELF rules but not this.
pub const SIGNATURE: &[u8] = b"BitsOS V2 kernel";

/// Hostile-input bound (Reload's): the kernel file may be at most this long.
pub const KERNEL_FILE_MAX: u64 = crate::stage::KERNEL_FILE_MAX;
/// Hostile-input bound: an initramfs larger than this is a mistake, not a system (the kernel unpacks it into RAM).
pub const INITRD_FILE_MAX: u64 = 1 << 30;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct KernelFacts {
    pub bytes: u64,
    pub entry: u64,
    pub span: u64,
    pub fnv: u64,
    /// UPDATE: the version number its mark names (None: a development build, or a kernel from before the mark).
    pub version: Option<u32>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct InitrdFacts {
    pub bytes: u64,
    pub entries: usize,
    pub fnv: u64,
    /// UPDATE: the number in its `etc/bitsos/version` (None: no such entry).
    pub version: Option<u32>,
}

pub fn fnv(b: &[u8]) -> u64 {
    let mut h = Fnv::new();
    h.update(b);
    h.finish()
}

/// The kernel file as bitsboot (and a Reload) would take it.
pub fn kernel(b: &[u8]) -> Result<KernelFacts, String> {
    if b.len() as u64 > KERNEL_FILE_MAX {
        return Err(format!("KERNEL: {} bytes, more than the {} MiB bound", b.len(), KERNEL_FILE_MAX >> 20));
    }
    // Reload's own header checks (also: headers within the first 64 KiB).
    let layout = match elf::parse_head(b) {
        Ok(Some(l)) => l,
        Ok(None) if b.len() < 4 => return Err(String::from("KERNEL: not an ELF file (too short)")),
        Ok(None) => return Err(format!("KERNEL: {}", ElfError::Truncated { have: b.len() as u64, need: 64 }.describe())),
        Err(e) => return Err(format!("KERNEL: {}", e.describe())),
    };
    if layout.file_end() > b.len() as u64 {
        return Err(format!("KERNEL: {}", ElfError::Truncated { have: b.len() as u64, need: layout.file_end() }.describe()));
    }
    // The loader's rules, word for word (W/X sharing a page, segments in the file, ...).
    let loader = bitsboot_pure::elf::parse(b).map_err(|e| format!("KERNEL: bitsboot would refuse it: {e}"))?;
    if loader.lo != KERNEL_BASE || layout.lo != KERNEL_BASE {
        return Err(format!(
            "KERNEL: not laid out like a BitsOS kernel: its image starts at {:#x}, a BitsOS kernel at {KERNEL_BASE:#x} (kernel/linker.ld)",
            layout.lo
        ));
    }
    let signed = layout.segs.iter().any(|s| {
        let (a, e) = (s.offset as usize, (s.offset + s.filesz) as usize);
        b.get(a..e).is_some_and(|seg| seg.windows(SIGNATURE.len()).any(|w| w == SIGNATURE))
    });
    if !signed {
        return Err(String::from("KERNEL: no BitsOS V2 kernel signature (\"BitsOS V2 kernel\") in its loaded segments"));
    }
    let version = kernel_version(b, &loader.segs)?;
    Ok(KernelFacts { bytes: b.len() as u64, entry: layout.entry, span: layout.span, fnv: fnv(b), version })
}

/// UPDATE: the version mark in the loaded segments, as bitsboot lays them out (a mark outside them is not the
/// kernel's own). Mark 0 is a development build. Two different numbers: refused, no number can be trusted.
pub fn kernel_version(b: &[u8], segs: &[bitsboot_pure::elf::Segment]) -> Result<Option<u32>, String> {
    let mut seen = bitsver::Found::None;
    for s in segs {
        let end = s.offset.saturating_add(s.filesz);
        if let Some(seg) = b.get(s.offset as usize..end as usize) {
            seen = bitsver::find(seg, seen);
        }
    }
    match seen {
        bitsver::Found::None | bitsver::Found::One(0) => Ok(None),
        bitsver::Found::One(n) => Ok(Some(n)),
        bitsver::Found::Conflict(a, z) => Err(format!("KERNEL: two different version marks ({a} and {z}) in its loaded segments")),
    }
}

/// UPDATE: the version mark of a KERNEL already on the medium (the update page asks which code a slot holds), read
/// as a stream in pieces of `piece` bytes (a multiple of 512, as `Stick::read_at` hands them) with the mark's
/// length less one carried over, so a mark across two pieces is found. The whole file, not only its loaded
/// segments: this names a slot for Georg, it decides no install. 0: no mark (a development build, or a kernel from
/// before UPDATE) or two different marks.
pub fn stream_version(read: &mut dyn FnMut(u64, &mut [u8]) -> Result<usize, i64>, piece: usize) -> Result<u32, i64> {
    let keep = bitsver::MARK_LEN - 1;
    let mut buf = Vec::new();
    buf.try_reserve_exact(piece + keep).map_err(|_| uapi::errno::ENOMEM)?;
    buf.resize(piece + keep, 0);
    let (mut carry, mut off, mut seen) = (0usize, 0u64, bitsver::Found::None);
    loop {
        let got = read(off, &mut buf[carry..carry + piece])?;
        if got == 0 {
            break;
        }
        off += got as u64;
        let have = carry + got;
        seen = bitsver::find(&buf[..have], seen);
        carry = have.min(keep);
        buf.copy_within(have - carry..have, 0);
    }
    Ok(match seen {
        bitsver::Found::One(n) => n,
        _ => 0,
    })
}

/// UPDATE: the number a bundle installs as. A kernel and an initramfs of one build carry the same number (or none,
/// a development build); a mixed pair is refused, since its slot would carry a number that is true of one half only.
pub fn bundle_version(k: &KernelFacts, i: &InitrdFacts) -> Result<Option<u32>, String> {
    match (k.version, i.version) {
        (a, b) if a == b => Ok(a),
        (Some(a), None) => Err(format!("the KERNEL is version {a}, the INITRD names no version: not one build")),
        (None, Some(b)) => Err(format!("the INITRD is version {b}, the KERNEL names no version: not one build")),
        (Some(a), Some(b)) => Err(format!("the KERNEL is version {a}, the INITRD version {b}: not one build")),
        _ => unreachable!(),
    }
}

/// What a refusal for a kernel without the disk guard's mark says (install_kernel and Reload).
pub const UNGUARDED: &str = "older than the disk guard (no mark in its loaded segments): on this machine (nvme_bind=off) it would give the internal disks a driver, so it is not installed or started here";

/// GP-GUARD: where the internal disks are off limits (`internal_off`: CONFIG `nvme_bind=off`, the Gaming-PC
/// profile), a kernel becomes a generation only if it carries the disk guard's mark, judged by the loader's own
/// function: bitsboot would skip it at the next start, and a Reload into it would bind the NVMe driver. On every
/// other machine nothing is asked.
pub fn disk_guard(b: &[u8], internal_off: bool) -> Result<(), String> {
    if !internal_off {
        return Ok(());
    }
    let loader = bitsboot_pure::elf::parse(b).map_err(|e| format!("KERNEL: bitsboot would refuse it: {e}"))?;
    if bitsboot_pure::profile::kernel_guarded(b, &loader.segs) { Ok(()) } else { Err(format!("KERNEL: {UNGUARDED}")) }
}

fn hex8(h: &[u8]) -> Option<usize> {
    let s = core::str::from_utf8(h).ok()?;
    usize::from_str_radix(s, 16).ok()
}

/// The initramfs: bitsboot's header rule, then every entry of every concatenated newc archive inside the file
/// (the way `vfs::cpio::unpack` reads it), each archive ended by `TRAILER!!!`, zero padding between archives.
pub fn initrd(b: &[u8]) -> Result<InitrdFacts, String> {
    let mut entries = 0usize;
    let mut version: Option<Result<u32, ()>> = None;
    walk_inner(b, &mut |name, _, data| {
        entries += 1;
        if strip(name) == bitsver::INITRD_ENTRY {
            // A later entry of the same name wins, as when it is unpacked.
            version = Some(bitsver::parse_entry(data).ok_or(()));
        }
    })?;
    let version = match version {
        None => None,
        Some(Ok(n)) => Some(n),
        Some(Err(())) => return Err(String::from("INITRD: etc/bitsos/version holds no version number")),
    };
    Ok(InitrdFacts { bytes: b.len() as u64, entries, fnv: fnv(b), version })
}

fn strip(mut n: &[u8]) -> &[u8] {
    while let Some(r) = n.strip_prefix(b"./").or_else(|| n.strip_prefix(b"/")) {
        n = r;
    }
    n
}

/// `initrd`'s rules; `each(name, mode, size)` sees every entry but the trailers.
fn walk(b: &[u8], each: &mut dyn FnMut(&[u8], u32, usize)) -> Result<(), String> {
    walk_inner(b, &mut |name, mode, body| each(name, mode, body.len()))
}

/// The rules themselves; `each(name, mode, data)`.
fn walk_inner(b: &[u8], each: &mut dyn FnMut(&[u8], u32, &[u8])) -> Result<(), String> {
    if b.len() as u64 > INITRD_FILE_MAX {
        return Err(format!("INITRD: {} bytes, more than the {} MiB bound", b.len(), INITRD_FILE_MAX >> 20));
    }
    bitsboot_pure::gens::check_initrd(b).map_err(|e| format!("INITRD: bitsboot would refuse it: {e}"))?;
    let bad = |what: &str, off: usize| Err(format!("INITRD: {what} at byte {off} (cut short or not a cpio newc archive)"));
    let mut off = 0usize;
    let mut trailers = 0usize;
    loop {
        // Between archives: zero padding up to the next header or the end.
        while off < b.len() && b[off] == 0 {
            off += 1;
        }
        if off >= b.len() {
            break;
        }
        // Inside an archive until its trailer.
        loop {
            let Some(h) = b.get(off..off + 110) else { return bad("a header runs past the end", off) };
            if &h[..6] != b"070701" && &h[..6] != b"070702" {
                return bad("a header without the newc magic", off);
            }
            let (Some(mode), Some(size), Some(nlen)) = (hex8(&h[14..22]), hex8(&h[54..62]), hex8(&h[94..102])) else {
                return bad("a header with a field that is no hex number", off);
            };
            if nlen == 0 {
                return bad("an entry without a name", off);
            }
            let name_end = off + 110 + nlen;
            let Some(name) = b.get(off + 110..name_end - 1) else { return bad("a name runs past the end", off) };
            let body = (name_end + 3) & !3;
            let Some(end) = body.checked_add(size).filter(|&e| e <= b.len()) else {
                return bad("an entry's data runs past the end", off);
            };
            off = (end + 3) & !3;
            if name == b"TRAILER!!!" {
                trailers += 1;
                break;
            }
            each(name, mode as u32, &b[body..end]);
            if off >= b.len() {
                return bad("the archive ends without TRAILER!!!", end);
            }
        }
    }
    if trailers == 0 {
        return bad("no TRAILER!!!", b.len());
    }
    Ok(())
}

/// RB-9: what a system generation's initramfs must hold besides a kernel (V2-PLAN §7 "kernel + initramfs with
/// init and the early services"): the first program (`/bin/init`) and the services it starts itself.
pub const EARLY_SERVICES: &[&str] = &["bin/init", "bin/settingsd", "bin/bitssnap"];

/// `install_system`'s extra rule: every one of [`EARLY_SERVICES`] is in the initramfs as a regular, executable,
/// non-empty file (a later entry of the same name wins, as when it is unpacked). Returns their sizes.
pub fn services(b: &[u8]) -> Result<Vec<(&'static str, usize)>, String> {
    let mut found: Vec<Option<(u32, usize)>> = alloc::vec![None; EARLY_SERVICES.len()];
    walk(b, &mut |name, mode, size| {
        let mut n = name;
        while let Some(r) = n.strip_prefix(b"./").or_else(|| n.strip_prefix(b"/")) {
            n = r;
        }
        if let Some(i) = EARLY_SERVICES.iter().position(|s| s.as_bytes() == n) {
            found[i] = Some((mode, size));
        }
    })?;
    let mut out = Vec::new();
    let mut missing = Vec::new();
    for (i, f) in found.iter().enumerate() {
        match f {
            Some((mode, size)) if mode & 0o170000 == 0o100000 && mode & 0o111 != 0 && *size > 0 => out.push((EARLY_SERVICES[i], *size)),
            Some((mode, size)) => missing.push(format!("/{} (mode {mode:o}, {size} bytes: not a non-empty executable file)", EARLY_SERVICES[i])),
            None => missing.push(format!("/{} (missing)", EARLY_SERVICES[i])),
        }
    }
    if !missing.is_empty() {
        return Err(format!(
            "INITRD: not a whole system: a system generation needs init and the early services, and {} (install_kernel installs it without this check)",
            missing.join(", ")
        ));
    }
    Ok(out)
}

/// A new `\BITSOS2\CONFIG.TXT` against the one on the stick: FS-7's guard (no `nvme_write`,
/// `stick_write`, `usb_msd_write` line added or changed; `wlan` is free, R8d), compared as the parsers read them), its size bound,
/// and the lifeline: a stick whose CONFIG.TXT names an MCP `token=` keeps one (without it MCP would answer no
/// one on any generation, and only a plug-over could repair that).
pub fn config(old: Option<&[u8]>, new: &[u8]) -> Result<(), String> {
    if new.len() as u64 > fs_fat::protect::CONFIG_MAX {
        return Err(format!("CONFIG.TXT: {} bytes, more than {} KiB", new.len(), fs_fat::protect::CONFIG_MAX >> 10));
    }
    let old = old.unwrap_or(b"");
    fs_fat::protect::config_guard(old, new).map_err(|key| {
        // GP-GUARD CR12: the list is the guard's own, so it names every key it holds.
        format!("CONFIG.TXT: the {key}= line would be added or changed; {} are set only by Georg himself (R8a)", fs_fat::protect::GUARDED_KEYS.join(", "))
    })?;
    let token = |t: &[u8]| bitsboot_pure::config::get(t, "token").is_some_and(|v| !v.is_empty());
    if token(old) && !token(new) {
        return Err(String::from(
            "CONFIG.TXT: the new file has no token= line but the stick's has one: MCP would answer no one after the next boot (the lifeline); keep a token",
        ));
    }
    Ok(())
}

/// `sys_keep=` (the number of confirmed generations kept; V2-PLAN §7, default 5) from a CONFIG.TXT, read like
/// the stick tool reads it (last line wins, `#` comments, 0 counts as 1). A value that is no number refuses
/// pruning instead of guessing (Georg may have asked to keep more).
pub fn sys_keep(config: Option<&[u8]>) -> Result<u32, String> {
    const DEFAULT: u32 = 5;
    let Some(text) = config else { return Ok(DEFAULT) };
    match bitsboot_pure::config::get(text, "sys_keep") {
        None => Ok(DEFAULT),
        Some(v) => match v.parse::<u32>() {
            Ok(0) => Ok(1),
            Ok(n) => Ok(n),
            Err(_) => Err(format!("CONFIG.TXT: sys_keep={v} is no number; nothing is pruned on a guess")),
        },
    }
}
