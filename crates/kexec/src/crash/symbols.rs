//! Function names for backtraces (RB-2). The table is the text `nm -n -C --defined-only <kernel ELF>` prints
//! (`ffffffff80001000 T kexec::crash::report`), made at build time from the very kernel file that boots and
//! shipped next to it: `boot/kernel.syms` in the initramfs (change request to the desk's xtask, so every bundle
//! and every Reload carries its own), or a `KSYMS` file in `\BITSOS2\fw\` (bitsboot loads every file there as a
//! FIRMWARE module; the RB scenario uses it). At boot it is parsed into a sorted index on the heap; a lookup is
//! a binary search without allocation, so the panic path and the NMI report can use it.
//!
//! A table from another build is refused: the address of `rb_ksyms_anchor` (a function of this crate) must
//! carry exactly that name in it.

use alloc::vec::Vec;

/// Bytes of a name kept (demangled generic names reach 1.4 KB; the start says what it is). A bound on one
/// entry, not a count.
pub const NAME_BYTES: usize = 200;
/// A lookup further than this past the nearest symbol is not believed (no function is that long).
pub const MAX_OFFSET: u64 = 1 << 20;

pub struct Symbols {
    addrs: Vec<u64>,
    /// Start of each name in `names` (one more entry than `addrs`: the end).
    at: Vec<u32>,
    names: Vec<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SymError {
    NoMemory,
    /// No usable line (not `nm` output, or no code symbols).
    Empty,
}

fn hex(s: &[u8]) -> Option<u64> {
    if s.is_empty() || s.len() > 16 {
        return None;
    }
    let mut v = 0u64;
    for &c in s {
        v = v << 4 | (c as char).to_digit(16)? as u64;
    }
    Some(v)
}

/// One `nm` line: `<hex address> <type letter> <name>`. Only code symbols (t, T, w, W).
fn line(l: &[u8]) -> Option<(u64, &[u8])> {
    let sp = l.iter().position(|&c| c == b' ')?;
    let addr = hex(&l[..sp])?;
    let rest = &l[sp + 1..];
    if rest.len() < 3 || rest[1] != b' ' || !matches!(rest[0], b't' | b'T' | b'w' | b'W') {
        return None;
    }
    let name = &rest[2..];
    (!name.is_empty()).then_some((addr, name))
}

fn cut_name(n: &[u8]) -> &[u8] {
    if n.len() <= NAME_BYTES {
        return n;
    }
    let mut k = NAME_BYTES;
    // Cut at a char boundary (names are UTF-8; demangled Rust is ASCII, but the file comes from outside).
    while k > 0 && (n[k] & 0xC0) == 0x80 {
        k -= 1;
    }
    &n[..k]
}

impl Symbols {
    /// Parses `nm` text, keeping symbols for which `keep(address)` holds (the kernel's code range). Lines it
    /// does not understand are skipped; never panics on the input.
    pub fn parse_nm(text: &[u8], keep: &dyn Fn(u64) -> bool) -> Result<Symbols, SymError> {
        let mut pairs: Vec<(u64, &[u8])> = Vec::new();
        for l in text.split(|&c| c == b'\n') {
            let l = l.strip_suffix(b"\r").unwrap_or(l);
            if let Some((a, n)) = line(l)
                && keep(a)
                && core::str::from_utf8(cut_name(n)).is_ok()
            {
                pairs.try_reserve(1).map_err(|_| SymError::NoMemory)?;
                pairs.push((a, cut_name(n)));
            }
        }
        if pairs.is_empty() {
            return Err(SymError::Empty);
        }
        // `nm -n` sorts already; sort anyway (stable: the first name at an address wins).
        pairs.sort_by_key(|p| p.0);
        pairs.dedup_by_key(|p| p.0);
        let bytes: usize = pairs.iter().map(|p| p.1.len()).sum();
        if bytes > u32::MAX as usize {
            return Err(SymError::NoMemory);
        }
        let mut s = Symbols { addrs: Vec::new(), at: Vec::new(), names: Vec::new() };
        s.addrs.try_reserve_exact(pairs.len()).map_err(|_| SymError::NoMemory)?;
        s.at.try_reserve_exact(pairs.len() + 1).map_err(|_| SymError::NoMemory)?;
        s.names.try_reserve_exact(bytes).map_err(|_| SymError::NoMemory)?;
        for (a, n) in pairs {
            s.addrs.push(a);
            s.at.push(s.names.len() as u32);
            s.names.extend_from_slice(n);
        }
        s.at.push(s.names.len() as u32);
        Ok(s)
    }

    pub fn len(&self) -> usize {
        self.addrs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.addrs.is_empty()
    }

    /// Bytes the table takes on the heap.
    pub fn bytes(&self) -> usize {
        self.addrs.len() * 8 + self.at.len() * 4 + self.names.len()
    }

    fn name(&self, i: usize) -> &str {
        let (s, e) = (self.at[i] as usize, self.at[i + 1] as usize);
        core::str::from_utf8(&self.names[s..e]).unwrap_or("?")
    }

    /// The function containing `addr`: its name and `addr`'s offset into it. No allocation.
    pub fn lookup(&self, addr: u64) -> Option<(&str, u64)> {
        let i = self.addrs.partition_point(|&a| a <= addr).checked_sub(1)?;
        let off = addr - self.addrs[i];
        (off <= MAX_OFFSET).then(|| (self.name(i), off))
    }

    /// The address of the first symbol whose name ends with `suffix` (tests and the anchor check).
    pub fn find(&self, suffix: &str) -> Option<u64> {
        (0..self.len()).find(|&i| self.name(i).ends_with(suffix)).map(|i| self.addrs[i])
    }
}

/// The anchor a table must name correctly to be believed (see the module doc).
#[inline(never)]
pub fn rb_ksyms_anchor() -> u64 {
    core::hint::black_box(0x5259_4d53) // "RYMS": something the optimiser cannot fold away
}

pub const ANCHOR_SUFFIX: &str = "rb_ksyms_anchor";

/// True if `t` names the anchor at `anchor_addr` exactly (offset 0, right name).
pub fn matches_build(t: &Symbols, anchor_addr: u64) -> bool {
    t.lookup(anchor_addr).is_some_and(|(n, off)| off == 0 && n.ends_with(ANCHOR_SUFFIX))
}

/// Finds `want` (a path without a leading slash) in a cpio "newc" archive read through `read(offset, buf)`
/// (`size` bytes in all); returns the data's (offset, length). Never trusts the archive: every header field is
/// checked against `size`.
pub fn cpio_find(read: &mut dyn FnMut(u64, &mut [u8]) -> bool, size: u64, want: &str) -> Option<(u64, u64)> {
    const HDR: u64 = 110;
    let field = |h: &[u8], i: usize| -> Option<u64> { hex(&h[6 + i * 8..14 + i * 8]) };
    let pad4 = |x: u64| (x + 3) & !3;
    let mut pos = 0u64;
    let mut name = [0u8; 256];
    while pos.checked_add(HDR)? <= size {
        let mut h = [0u8; HDR as usize];
        if !read(pos, &mut h) || &h[..6] != b"070701" {
            return None;
        }
        let filesize = field(&h, 6)?;
        let namesize = field(&h, 11)?;
        if namesize == 0 {
            return None;
        }
        let name_end = pos + HDR + namesize;
        let data = pad4(name_end);
        let next = pad4(data.checked_add(filesize)?);
        if data > size || next > pad4(size) {
            return None;
        }
        // Names longer than our buffer cannot be `want` (it is short); skip them unread.
        if namesize as usize <= name.len() {
            let n = &mut name[..namesize as usize];
            if !read(pos + HDR, n) {
                return None;
            }
            let n = &n[..n.len() - 1];
            if n == b"TRAILER!!!" {
                return None;
            }
            if n == want.as_bytes() || n.strip_prefix(b"./") == Some(want.as_bytes()) {
                return Some((data, filesize));
            }
        }
        if next <= pos {
            return None;
        }
        pos = next;
    }
    None
}
