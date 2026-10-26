//! The memory-call script (`abi/mm-ops.txt`) and its interpreter: the same calls run on CachyOS
//! (`host/tests/mm/abi/mmops.c`, output kept in `abi/linux-expected.txt`), in the kernel against a real
//! `AddressSpace` (ktest `mm.as.abi_diff`) and on the host against the pure model; every run must print the
//! expected text exactly.
//!
//! Addresses are written relative to a window both systems leave free (`B+0x..`) or to the program break at
//! the start (`H+0x..`), so the output does not depend on where each system puts its program. Lines:
//! `mmap ADDR LEN PROT FLAGS`, `munmap ADDR LEN`, `mprotect ADDR LEN PROT`, `brk ADDR`, `store ADDR BYTE`,
//! `load ADDR`, `maps` (the window's mappings), `heap` (the mappings above the start break). MM-5b's second
//! script (`abi/mm-ops2.txt`, kernel and CachyOS only) adds `madvise ADDR LEN ADVICE`, `mremap ADDR OLD NEW
//! FLAGS NEWADDR` and `mincore ADDR LEN` (one 0/1 per page).

use crate::linux::{self, Errno};
use crate::vma::Prot;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write as _;

/// The window the script maps into (free on CachyOS: between PIE programs and the libraries).
pub const WINDOW: u64 = 0x2000_0000_0000;
pub const WINDOW_LEN: u64 = 0x4000_0000;
/// How much above the start break `heap` shows.
pub const HEAP_LEN: u64 = 0x100_0000;

pub const SCRIPT: &str = include_str!("../abi/mm-ops.txt");
pub const LINUX_EXPECTED: &str = include_str!("../abi/linux-expected.txt");
/// MM-5b: madvise, mremap, mincore, shared anonymous memory.
pub const SCRIPT2: &str = include_str!("../abi/mm-ops2.txt");
pub const LINUX_EXPECTED2: &str = include_str!("../abi/linux-expected2.txt");

/// What the script needs from a system.
pub trait MemApi {
    fn mmap(&mut self, addr: u64, len: u64, prot: u64, flags: u64) -> Result<u64, Errno>;
    fn munmap(&mut self, addr: u64, len: u64) -> Result<(), Errno>;
    fn mprotect(&mut self, addr: u64, len: u64, prot: u64) -> Result<(), Errno>;
    fn brk(&mut self, addr: u64) -> u64;
    fn store(&mut self, addr: u64, byte: u8) -> bool;
    fn load(&mut self, addr: u64) -> Option<u8>;
    /// Mapped ranges overlapping [lo, hi) with their rights, sorted.
    fn ranges(&self, lo: u64, hi: u64) -> Vec<(u64, u64, Prot)>;
    /// MM-5b calls (None: this system does not offer them, e.g. the pure model).
    fn madvise(&mut self, _addr: u64, _len: u64, _advice: u64) -> Option<Result<(), Errno>> {
        None
    }
    fn mremap(&mut self, _addr: u64, _old: u64, _new: u64, _flags: u64, _new_addr: u64) -> Option<Result<u64, Errno>> {
        None
    }
    fn mincore(&mut self, _addr: u64, _len: u64) -> Option<Result<Vec<u8>, Errno>> {
        None
    }
}

fn num(s: &str) -> Option<u64> {
    match s.strip_prefix("0x") {
        Some(h) => u64::from_str_radix(h, 16).ok(),
        None => s.parse().ok(),
    }
}

struct Ctx {
    heap: u64,
    last: u64,
}

impl Ctx {
    fn addr(&self, s: &str) -> Option<u64> {
        let (base, rest) = match s.as_bytes().first()? {
            b'B' => (WINDOW, &s[1..]),
            b'H' => (self.heap, &s[1..]),
            b'L' => (self.last, &s[1..]),
            _ => return num(s),
        };
        if rest.is_empty() {
            return Some(base);
        }
        let (neg, n) = match rest.as_bytes()[0] {
            b'+' => (false, num(&rest[1..])?),
            b'-' => (true, num(&rest[1..])?),
            _ => return None,
        };
        Some(if neg { base.wrapping_sub(n) } else { base.wrapping_add(n) })
    }

    fn show(&self, a: u64) -> String {
        let rel = |base: u64, tag: &str| if a >= base { format!("{tag}+{:#x}", a - base) } else { format!("{tag}-{:#x}", base - a) };
        if (WINDOW..=WINDOW + WINDOW_LEN).contains(&a) {
            rel(WINDOW, "B")
        } else if a.abs_diff(self.heap) <= HEAP_LEN {
            rel(self.heap, "H")
        } else if (0x4000_0000..0x8000_0000).contains(&a) {
            String::from("in-32bit-window")
        } else if a < 0x1_0000_0000 {
            format!("abs {a:#x}")
        } else {
            String::from("elsewhere")
        }
    }
}

fn prot(s: &str) -> Option<u64> {
    if s.starts_with("0x") {
        return num(s);
    }
    if s == "none" {
        return Some(0);
    }
    s.bytes().try_fold(0, |p, c| match c {
        b'r' => Some(p | linux::PROT_READ),
        b'w' => Some(p | linux::PROT_WRITE),
        b'x' => Some(p | linux::PROT_EXEC),
        _ => None,
    })
}

fn flags(s: &str) -> Option<u64> {
    s.split('|').try_fold(0, |f, n| {
        let b = match n {
            "none" => 0,
            "private" => linux::MAP_PRIVATE,
            "shared" => linux::MAP_SHARED,
            "validate" => 3,
            "anon" => linux::MAP_ANONYMOUS,
            "fixed" => linux::MAP_FIXED,
            "noreplace" => linux::MAP_FIXED_NOREPLACE,
            "noreserve" => linux::MAP_NORESERVE,
            "populate" => linux::MAP_POPULATE,
            "nonblock" => linux::MAP_NONBLOCK,
            "stack" => linux::MAP_STACK,
            "hugetlb" => linux::MAP_HUGETLB,
            "32bit" => linux::MAP_32BIT,
            _ => num(n)?,
        };
        Some(f | b)
    })
}

fn advice(s: &str) -> Option<u64> {
    use crate::linux::madv::*;
    Some(match s {
        "normal" => NORMAL,
        "random" => RANDOM,
        "sequential" => SEQUENTIAL,
        "willneed" => WILLNEED,
        "dontneed" => DONTNEED,
        "free" => FREE,
        "remove" => REMOVE,
        "dontfork" => DONTFORK,
        "dofork" => DOFORK,
        "mergeable" => MERGEABLE,
        "unmergeable" => UNMERGEABLE,
        "hugepage" => HUGEPAGE,
        "nohugepage" => NOHUGEPAGE,
        "dontdump" => DONTDUMP,
        "dodump" => DODUMP,
        "wipeonfork" => WIPEONFORK,
        "keeponfork" => KEEPONFORK,
        "cold" => COLD,
        "pageout" => PAGEOUT,
        "populate_read" => POPULATE_READ,
        "populate_write" => POPULATE_WRITE,
        "dontneed_locked" => DONTNEED_LOCKED,
        "collapse" => COLLAPSE,
        "hwpoison" => HWPOISON,
        "soft_offline" => SOFT_OFFLINE,
        _ => num(s)?,
    })
}

fn rflags(s: &str) -> Option<u64> {
    s.split('|').try_fold(0, |f, n| {
        Some(
            f | match n {
                "none" => 0,
                "maymove" => linux::MREMAP_MAYMOVE,
                "fixed" => linux::MREMAP_FIXED,
                "dontunmap" => linux::MREMAP_DONTUNMAP,
                _ => num(n)?,
            },
        )
    })
}

fn err(e: Errno) -> String {
    format!("-{}", e.name())
}

fn ranges_text(out: &mut String, ctx: &Ctx, api: &dyn MemApi, lo: u64, hi: u64) {
    // Adjacent ranges with the same rights print as one: only the rights per page are compared, not where
    // each system draws VMA boundaries.
    let mut merged: Vec<(u64, u64, Prot)> = Vec::new();
    for (s, e, p) in api.ranges(lo, hi) {
        let (s, e) = (s.max(lo), e.min(hi));
        match merged.last_mut() {
            Some(m) if m.1 == s && m.2 == p => m.1 = e,
            _ => merged.push((s, e, p)),
        }
    }
    if merged.is_empty() {
        out.push_str("  (none)\n");
    }
    for (s, e, p) in merged {
        let l = p.letters();
        let _ = writeln!(out, "  {}..{} {}{}{}", ctx.show(s), ctx.show(e), l[0] as char, l[1] as char, l[2] as char);
    }
}

/// Runs the script; `heap` is the program break before the first line.
pub fn run(script: &str, api: &mut dyn MemApi, heap: u64) -> String {
    let mut ctx = Ctx { heap, last: 0 };
    let mut out = String::new();
    for line in script.lines() {
        let line = line.trim_end();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let w: Vec<&str> = line.split(' ').collect();
        let res = step(&w, &mut ctx, api);
        let _ = writeln!(out, "{line} => {}", res.unwrap_or_else(|| String::from("?? bad line")));
        match w[0] {
            "maps" => ranges_text(&mut out, &ctx, api, WINDOW, WINDOW + WINDOW_LEN),
            "heap" => ranges_text(&mut out, &ctx, api, ctx.heap, ctx.heap + HEAP_LEN),
            _ => {}
        }
    }
    out
}

fn step(w: &[&str], ctx: &mut Ctx, api: &mut dyn MemApi) -> Option<String> {
    let ok0 = |r: Result<(), Errno>| Some(r.map(|_| String::from("0")).unwrap_or_else(err));
    match (w[0], w.len()) {
        ("mmap", 5) => {
            let r = api.mmap(ctx.addr(w[1])?, num(w[2])?, prot(w[3])?, flags(w[4])?);
            Some(match r {
                Ok(a) => {
                    ctx.last = a;
                    ctx.show(a)
                }
                Err(e) => err(e),
            })
        }
        ("munmap", 3) => ok0(api.munmap(ctx.addr(w[1])?, num(w[2])?)),
        ("mprotect", 4) => ok0(api.mprotect(ctx.addr(w[1])?, num(w[2])?, prot(w[3])?)),
        ("brk", 2) => {
            let b = api.brk(ctx.addr(w[1])?);
            Some(ctx.show(b))
        }
        ("store", 3) => {
            let b = num(w[2])? as u8;
            Some(String::from(if api.store(ctx.addr(w[1])?, b) { "ok" } else { "fault" }))
        }
        ("load", 2) => Some(api.load(ctx.addr(w[1])?).map(|b| format!("{b:#x}")).unwrap_or_else(|| String::from("fault"))),
        ("madvise", 4) => {
            let r = api.madvise(ctx.addr(w[1])?, num(w[2])?, advice(w[3])?)?;
            ok0(r)
        }
        ("mremap", 6) => {
            let r = api.mremap(ctx.addr(w[1])?, num(w[2])?, num(w[3])?, rflags(w[4])?, ctx.addr(w[5])?)?;
            Some(match r {
                Ok(a) => {
                    ctx.last = a;
                    ctx.show(a)
                }
                Err(e) => err(e),
            })
        }
        ("mincore", 3) => Some(match api.mincore(ctx.addr(w[1])?, num(w[2])?)? {
            Ok(v) => v.iter().map(|b| if b & 1 != 0 { '1' } else { '0' }).collect(),
            Err(e) => err(e),
        }),
        ("maps", 1) | ("heap", 1) => Some(String::new()),
        _ => None,
    }
}

/// The first line where `got` and `want` differ, with both versions (`None`: equal).
pub fn first_difference(got: &str, want: &str) -> Option<String> {
    let (g, w): (Vec<&str>, Vec<&str>) = (got.lines().collect(), want.lines().collect());
    for i in 0..g.len().max(w.len()) {
        let (a, b) = (g.get(i).copied().unwrap_or("<end>"), w.get(i).copied().unwrap_or("<end>"));
        if a != b {
            return Some(format!("line {}: got `{a}`, Linux `{b}`", i + 1));
        }
    }
    None
}

#[cfg(test)]
#[path = "abi_tests.rs"]
mod tests;
