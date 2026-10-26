//! AB-7b: `/sys/devices/system/cpu/cpuN/cache/indexM` (linux-6.6 drivers/base/cacheinfo.c
//! `cache_default_attrs`, arch/x86/kernel/cpu/cacheinfo.c `ci_leaf_init`, `__cache_amd_cpumap_setup`,
//! `get_cache_id`). The leaves come from CPUID 0x8000001D on AMD with TOPOEXT, else Intel's leaf 4 (both have
//! the same register layout). Both machines are homogeneous, so the leaves are read once on the asking CPU;
//! which CPUs share a leaf comes from each CPU's APIC ID and LLC domain (`hal::smp::topology`).

use super::attr::{Live, PAGE};
use super::cpuinfo::{Cpu, online};
use super::stat::{bitmap_hex, bitmap_list};
use super::sys_cpu::{possible, words};
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use hal::cpu::{CpuidResult, cpuid};
use vfs::Errno;
use vfs::synth::{DirSource, SynthEntry};

/// One cache leaf as `ci_leaf_init` keeps it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Leaf {
    pub level: u32,
    /// CTYPE_DATA 1, CTYPE_INST 2, CTYPE_UNIFIED 3.
    pub ty: u32,
    pub line: u32,
    pub ways: u32,
    pub partitions: u32,
    pub sets: u32,
    /// `num_threads_sharing + 1`.
    pub sharing: u32,
}

impl Leaf {
    pub(super) fn size(&self) -> u64 {
        self.line as u64 * self.ways as u64 * self.partitions as u64 * self.sets as u64
    }
    pub(super) fn type_name(&self) -> &'static str {
        match self.ty {
            1 => "Data",
            2 => "Instruction",
            _ => "Unified",
        }
    }
}

/// A leaf from its CPUID answer; None at type 0 (the end of the list) or for a type Linux does not map.
pub(super) fn decode(r: CpuidResult) -> Option<Leaf> {
    let ty = r.eax & 0x1f;
    if !(1..=3).contains(&ty) {
        return None;
    }
    Some(Leaf {
        level: (r.eax >> 5) & 7,
        ty,
        line: (r.ebx & 0xfff) + 1,
        partitions: ((r.ebx >> 12) & 0x3ff) + 1,
        ways: ((r.ebx >> 22) & 0x3ff) + 1,
        sets: r.ecx.wrapping_add(1),
        sharing: ((r.eax >> 14) & 0xfff) + 1,
    })
}

/// Which leaf holds the cache list: (leaf, AMD's sharing rules).
fn source() -> Option<(u32, bool)> {
    let l0 = cpuid(0, 0);
    let amd = l0.ebx == u32::from_le_bytes(*b"Auth") || l0.ebx == u32::from_le_bytes(*b"Hygo");
    if amd {
        let topoext = cpuid(0x8000_0000, 0).eax >= 0x8000_001d && cpuid(0x8000_0001, 0).ecx & (1 << 22) != 0;
        return topoext.then_some((0x8000_001d, true));
    }
    (l0.eax >= 4).then_some((4, false))
}

/// The leaves in index order (sub-leaves until type 0, at most the 32 a 5-bit type field allows).
fn leaves() -> (Vec<Leaf>, bool) {
    let Some((leaf, amd)) = source() else { return (Vec::new(), false) };
    let v = (0..32).map_while(|sub| decode(cpuid(leaf, sub))).collect();
    (v, amd)
}

/// `get_count_order`: the bits a count of `n` needs.
pub(super) fn count_order(n: u32) -> u32 {
    if n <= 1 { 0 } else { 32 - (n - 1).leading_zeros() }
}

/// `get_cache_id`: the APIC ID without the sharing threads' bits.
pub(super) fn cache_id(apic: u32, l: &Leaf) -> u32 {
    apic >> count_order(l.sharing)
}

/// The online CPUs sharing leaf `index` with `me`: AMD's L3 (index 3) by LLC domain, other AMD leaves by the
/// APIC range `nshared` wide, Intel by the APIC ID above `index_msb`.
pub(super) fn sharers(all: &[Cpu], me: &Cpu, index: usize, l: &Leaf, amd: bool) -> Vec<usize> {
    let same = |o: &Cpu| -> bool {
        if amd && index == 3 {
            o.pkg == me.pkg && o.cluster == me.cluster
        } else if amd {
            let first = me.apic - me.apic % l.sharing;
            (first..=first.saturating_add(l.sharing - 1)).contains(&o.apic)
        } else {
            cache_id(o.apic, l) == cache_id(me.apic, l)
        }
    };
    all.iter().filter(|o| same(o)).map(|o| o.cpu).collect()
}

struct IndexDir {
    cpu: usize,
    index: usize,
}

impl IndexDir {
    /// The leaf and the CPU at open time (ENODEV once the CPU or the leaf is gone).
    fn with<T>(cpu: usize, index: usize, f: impl Fn(&Leaf, &Cpu, &[Cpu], bool) -> T) -> Result<T, Errno> {
        let (ls, amd) = leaves();
        let all = online();
        let l = ls.get(index).ok_or(uapi::errno::ENODEV)?;
        let me = all.iter().find(|c| c.cpu == cpu).ok_or(uapi::errno::ENODEV)?;
        Ok(f(l, me, &all, amd))
    }
}

impl DirSource for IndexDir {
    fn entries(&self) -> Result<Vec<SynthEntry>, Errno> {
        let (cpu, index) = (self.cpu, self.index);
        let num = |f: fn(&Leaf, &Cpu) -> String| Arc::new(Live::new(PAGE, move || IndexDir::with(cpu, index, |l, me, _, _| f(l, me))));
        let set = move |list: bool| {
            Arc::new(Live::new(PAGE, move || {
                IndexDir::with(cpu, index, |l, me, all, amd| {
                    let w = words(sharers(all, me, index, l, amd).into_iter(), possible());
                    if list { format!("{}\n", bitmap_list(&w, possible())) } else { format!("{}\n", bitmap_hex(&w, possible())) }
                })
            }))
        };
        let mut v = alloc::vec![
            SynthEntry::file("id", 0o444, num(|l, me| format!("{}\n", cache_id(me.apic, l)))),
            SynthEntry::file("type", 0o444, num(|l, _| format!("{}\n", l.type_name()))),
            SynthEntry::file("level", 0o444, num(|l, _| format!("{}\n", l.level))),
            SynthEntry::file("shared_cpu_map", 0o444, set(false)),
            SynthEntry::file("shared_cpu_list", 0o444, set(true)),
            SynthEntry::file("coherency_line_size", 0o444, num(|l, _| format!("{}\n", l.line))),
            SynthEntry::file("ways_of_associativity", 0o444, num(|l, _| format!("{}\n", l.ways))),
            SynthEntry::file("number_of_sets", 0o444, num(|l, _| format!("{}\n", l.sets))),
            SynthEntry::file("size", 0o444, num(|l, _| format!("{}K\n", l.size() >> 10))),
            SynthEntry::file("physical_line_partition", 0o444, num(|l, _| format!("{}\n", l.partitions))),
        ];
        for (i, e) in v.iter_mut().enumerate() {
            e.cookie = i as u64 + 1;
        }
        Ok(v)
    }
}

/// `cpuN/cache`: one `indexM` per leaf (none when CPUID lists no caches, as on a CPU without leaf 4).
pub(super) struct CacheDir(pub usize);

impl DirSource for CacheDir {
    fn entries(&self) -> Result<Vec<SynthEntry>, Errno> {
        let n = leaves().0.len();
        Ok((0..n).map(|i| SynthEntry::dir(&format!("index{i}"), Arc::new(IndexDir { cpu: self.0, index: i })).with_perm(0o755).cookie(i as u64 + 1)).collect())
    }
}
