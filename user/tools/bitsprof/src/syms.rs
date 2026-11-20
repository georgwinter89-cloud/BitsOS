//! Names for the addresses of a run: the kernel's from `/proc/kallsyms` (zeros when the kernel hides them:
//! `kernel.kptr_restrict`), programs' and libraries' from their files (`elf`), each file read once.

use crate::elf::{ElfSyms, demangle};
use std::collections::HashMap;

pub struct Kernel {
    /// (address, name), sorted.
    syms: Vec<(u64, String)>,
}

impl Kernel {
    pub fn load() -> Kernel {
        let text = std::fs::read_to_string("/proc/kallsyms").unwrap_or_default();
        Kernel::parse(&text)
    }

    pub fn parse(text: &str) -> Kernel {
        let mut syms: Vec<(u64, String)> = text
            .lines()
            .filter_map(|l| {
                let mut it = l.splitn(3, ' ');
                let a = u64::from_str_radix(it.next()?, 16).ok()?;
                let t = it.next()?;
                let name = it.next()?;
                (a != 0 && matches!(t, "t" | "T" | "w" | "W")).then(|| (a, name.split('\t').next().unwrap_or(name).to_string()))
            })
            .collect();
        syms.sort_by_key(|s| s.0);
        syms.dedup_by_key(|s| s.0);
        Kernel { syms }
    }

    /// Whether the kernel showed its addresses.
    pub fn visible(&self) -> bool {
        !self.syms.is_empty()
    }

    pub fn lookup(&self, addr: u64) -> Option<&str> {
        let i = self.syms.partition_point(|s| s.0 <= addr).checked_sub(1)?;
        // Past the last symbol by more than a large function: not the kernel's text.
        (addr - self.syms[i].0 < 1 << 20).then_some(self.syms[i].1.as_str())
    }
}

/// Every file's function table, loaded on first use (None: unreadable or not ELF).
#[derive(Default)]
pub struct Files {
    files: HashMap<String, Option<ElfSyms>>,
}

impl Files {
    /// The function at file offset `off` of `path`.
    pub fn lookup(&mut self, path: &str, off: u64) -> Option<String> {
        let e = self.files.entry(path.to_string()).or_insert_with(|| {
            if !path.starts_with('/') {
                return None;
            }
            std::fs::read(path).ok().and_then(|b| ElfSyms::parse(&b)).filter(|s| !s.is_empty())
        });
        let s = e.as_ref()?;
        let va = s.vaddr_of(off)?;
        s.lookup(va).map(demangle)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kallsyms_lines() {
        let k = Kernel::parse("ffffffff80001000 t sched::run::schedule\nffffffff80002000 T <T as core::fmt::Debug>::fmt\n0000000000000000 t hidden\nffffffff80003000 d data\n");
        assert!(k.visible());
        assert_eq!(k.lookup(0xffffffff80001010), Some("sched::run::schedule"));
        assert_eq!(k.lookup(0xffffffff80002004), Some("<T as core::fmt::Debug>::fmt"));
        assert_eq!(k.lookup(0x1000), None);
        assert!(!Kernel::parse("0000000000000000 t a\n").visible());
    }
}
