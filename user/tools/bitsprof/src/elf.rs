//! Function names of a program or library file: its `.symtab` (or, stripped, `.dynsym`) and the load segments
//! that turn a file offset (what MMAP2 records and `/proc/<pid>/maps` give) into the address the symbols use.

/// One file's functions, sorted by address.
#[derive(Default)]
pub struct ElfSyms {
    /// (start, size, name); size 0 = up to the next symbol.
    funcs: Vec<(u64, u64, String)>,
    /// PT_LOAD segments: (file offset, vaddr, file size).
    loads: Vec<(u64, u64, u64)>,
}

fn u16_at(b: &[u8], o: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(o..o + 2)?.try_into().ok()?))
}
fn u32_at(b: &[u8], o: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(o..o + 4)?.try_into().ok()?))
}
fn u64_at(b: &[u8], o: usize) -> Option<u64> {
    Some(u64::from_le_bytes(b.get(o..o + 8)?.try_into().ok()?))
}

const SHT_SYMTAB: u32 = 2;
const SHT_DYNSYM: u32 = 11;
const STT_FUNC: u8 = 2;
const PT_LOAD: u32 = 1;

impl ElfSyms {
    /// Parses a 64-bit little-endian ELF file; None for anything else.
    pub fn parse(b: &[u8]) -> Option<ElfSyms> {
        if b.get(..4)? != b"\x7fELF" || b[4] != 2 || b[5] != 1 {
            return None;
        }
        let mut s = ElfSyms::default();
        let (phoff, phentsize, phnum) = (u64_at(b, 32)? as usize, u16_at(b, 54)? as usize, u16_at(b, 56)? as usize);
        for i in 0..phnum {
            let p = phoff.checked_add(i.checked_mul(phentsize)?)?;
            if u32_at(b, p)? == PT_LOAD {
                s.loads.push((u64_at(b, p + 8)?, u64_at(b, p + 16)?, u64_at(b, p + 32)?));
            }
        }
        let (shoff, shentsize, shnum) = (u64_at(b, 40)? as usize, u16_at(b, 58)? as usize, u16_at(b, 60)? as usize);
        let sec = |i: usize| -> Option<(u32, u64, u64, u32, u64)> {
            let h = shoff.checked_add(i.checked_mul(shentsize)?)?;
            Some((u32_at(b, h + 4)?, u64_at(b, h + 24)?, u64_at(b, h + 32)?, u32_at(b, h + 40)?, u64_at(b, h + 56)?))
        };
        let tables: Vec<usize> = (0..shnum).filter(|&i| sec(i).is_some_and(|x| x.0 == SHT_SYMTAB)).collect();
        let tables = if tables.is_empty() { (0..shnum).filter(|&i| sec(i).is_some_and(|x| x.0 == SHT_DYNSYM)).collect() } else { tables };
        for t in tables {
            let (_, off, size, link, ent) = sec(t)?;
            let (_, stroff, strsize, _, _) = sec(link as usize)?;
            let strtab = b.get(stroff as usize..(stroff + strsize) as usize)?;
            let ent = if ent == 0 { 24 } else { ent as usize };
            for k in 0..(size as usize / ent) {
                let e = off as usize + k * ent;
                let (name, info, value, sz) = (u32_at(b, e)? as usize, *b.get(e + 4)?, u64_at(b, e + 8)?, u64_at(b, e + 16)?);
                if info & 0xf != STT_FUNC || value == 0 {
                    continue;
                }
                let n = strtab.get(name..)?;
                let n = &n[..n.iter().position(|&c| c == 0).unwrap_or(n.len())];
                s.funcs.push((value, sz, String::from_utf8_lossy(n).into_owned()));
            }
        }
        s.funcs.sort_by_key(|f| f.0);
        s.funcs.dedup_by_key(|f| f.0);
        Some(s)
    }

    /// The address a file offset is loaded at (by the segment that holds it).
    pub fn vaddr_of(&self, off: u64) -> Option<u64> {
        self.loads.iter().find(|&&(o, _, n)| off >= o && off < o + n.max(1)).map(|&(o, v, _)| off - o + v)
    }

    /// The function at `vaddr`: its name.
    pub fn lookup(&self, vaddr: u64) -> Option<&str> {
        let i = self.funcs.partition_point(|f| f.0 <= vaddr).checked_sub(1)?;
        let (start, size, name) = &self.funcs[i];
        let inside = if *size != 0 { vaddr < start + size } else { self.funcs.get(i + 1).is_none_or(|n| vaddr < n.0) };
        inside.then_some(name.as_str())
    }

    pub fn is_empty(&self) -> bool {
        self.funcs.is_empty()
    }
}

/// Rust's v0 names (`v0.rs`), legacy Rust mangling (`_ZN` len name ... `17h<hash>E`) and plain Itanium nested
/// names: `a::b::c`; anything else as it is.
pub fn demangle(s: &str) -> String {
    if let Some(v) = crate::v0::demangle(s) {
        return v;
    }
    let Some(mut rest) = s.strip_prefix("_ZN").or_else(|| s.strip_prefix("__ZN")) else { return s.to_string() };
    let mut parts: Vec<String> = Vec::new();
    while let Some(c) = rest.chars().next() {
        if c == 'E' {
            break;
        }
        let digits = rest.chars().take_while(|c| c.is_ascii_digit()).count();
        if digits == 0 {
            return s.to_string();
        }
        let Ok(n) = rest[..digits].parse::<usize>() else { return s.to_string() };
        let Some(part) = rest.get(digits..digits + n) else { return s.to_string() };
        parts.push(part.to_string());
        rest = &rest[digits + n..];
    }
    if parts.last().is_some_and(|p| p.len() == 17 && p.starts_with('h') && p[1..].chars().all(|c| c.is_ascii_hexdigit())) {
        parts.pop();
    }
    let joined = parts.join("::");
    // The escapes of legacy Rust names.
    joined.replace("$LT$", "<").replace("$GT$", ">").replace("$RF$", "&").replace("$BP$", "*").replace("$C$", ",").replace("$u20$", " ").replace("$u7b$", "{").replace("$u7d$", "}").replace("$LP$", "(").replace("$RP$", ")").replace("..", "::")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rust_names() {
        assert_eq!(demangle("_ZN8bitsprof4busy8hot_loop17h0123456789abcdefE"), "bitsprof::busy::hot_loop");
        assert_eq!(demangle("_ZN4core3ptr85drop_in_place$LT$std..rt..lang_start$LT$$LP$$RP$$GT$..$u7b$$u7b$closure$u7d$$u7d$$GT$17h1111111111111111E"), "core::ptr::drop_in_place<std::rt::lang_start<()>::{{closure}}>");
        assert_eq!(demangle("memcpy"), "memcpy");
        assert_eq!(demangle("_ZNbroken"), "_ZNbroken");
    }

    #[test]
    fn own_binary() {
        let me = std::fs::read(std::env::current_exe().unwrap()).unwrap();
        let s = ElfSyms::parse(&me).expect("the test binary is ELF");
        assert!(!s.is_empty());
        let f = rust_names as fn() as usize as u64;
        // A non-PIE test binary maps at its link address; a PIE one somewhere else: then only parse is checked.
        if s.funcs.iter().any(|x| x.0 == f) {
            assert!(s.lookup(f).is_some_and(|n| demangle(n).contains("rust_names")));
        }
        assert!(s.vaddr_of(0).is_some() || s.loads.is_empty());
    }
}
