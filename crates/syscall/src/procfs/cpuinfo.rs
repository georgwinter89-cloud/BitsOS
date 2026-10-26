//! AB-7b: `/proc/cpuinfo` as linux-6.6/arch/x86/kernel/cpu/proc.c `show_cpuinfo` prints it (moved from FS-2's
//! `vfs-sys/src/sysinfo.rs`), now with each CPU's own topology (`hal::smp::topology`): physical id, siblings
//! (CPUs of the package), core id, cpu cores (cores of the package), apicid. Cache size and TLB size as
//! common.c `cpu_detect_cache_sizes` (AMD: L2 from 0x80000006) and cacheinfo.c (Intel: L3, else L2, from leaf
//! 4); power management from 0x80000007 EDX (powerflags.c).

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write;
use hal::cpu::cpuid;

/// One online CPU's place in the machine.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Cpu {
    pub cpu: usize,
    pub pkg: u32,
    pub die: u32,
    pub cluster: u32,
    pub core: u32,
    pub apic: u32,
}

fn cpu_of(cpu: usize) -> Cpu {
    cpu_from(cpu, hal::smp::topology(cpu))
}

pub(super) fn cpu_from(cpu: usize, t: Option<hal::smp::Topology>) -> Cpu {
    match t {
        Some(t) => Cpu { cpu, pkg: t.package_id, die: t.die_id, cluster: t.cluster_id, core: t.core_id, apic: t.apic_id },
        // Not decoded (a CPU that came up before `prepare`): one core per CPU, as FS-2 showed it.
        None => Cpu { cpu, pkg: 0, die: 0, cluster: 0, core: cpu as u32, apic: cpu as u32 },
    }
}

/// The online CPUs in number order (CPU 0 alone before the scheduler counts any).
pub(super) fn online() -> Vec<Cpu> {
    let mut v: Vec<Cpu> = sched::cpu_stats().iter().filter(|c| c.online).map(|c| cpu_of(c.cpu)).collect();
    if v.is_empty() {
        v.push(cpu_of(0));
    }
    v.sort_by_key(|c| c.cpu);
    v
}

/// `siblings` (CPUs in `c`'s package) and `cpu cores` (distinct cores in it) among `all`.
pub(super) fn package_counts(all: &[Cpu], c: &Cpu) -> (usize, usize) {
    let mut cores: Vec<(u32, u32)> = all.iter().filter(|o| o.pkg == c.pkg).map(|o| (o.die, o.core)).collect();
    let siblings = cores.len();
    cores.sort_unstable();
    cores.dedup();
    (siblings, cores.len())
}

/// What every block repeats (the CPUID answers are the same on every CPU of both machines).
pub(super) struct Common {
    pub vendor: String,
    pub family: u32,
    pub model: u32,
    pub brand: String,
    pub stepping: u32,
    pub tsc_hz: u64,
    pub cache_kb: u32,
    pub level: u32,
    pub flags: String,
    pub tlb: u32,
    pub clflush: u32,
    pub phys: u32,
    pub virt: u32,
    pub power: u32,
}

fn regs_text(regs: &[u32]) -> String {
    let mut b = Vec::new();
    for r in regs {
        b.extend_from_slice(&r.to_le_bytes());
    }
    let s = String::from_utf8_lossy(&b).into_owned();
    String::from(s.trim_matches(|c: char| c == '\0' || c.is_whitespace()))
}

/// Intel's deterministic cache leaf 4: L3, else L2, else L1i + L1d, in KB (cacheinfo.c :884). Sub-leaves end
/// at type 0; a CPU that never says so is read no further than the 32 levels its 5-bit type field allows.
fn intel_cache_kb() -> u32 {
    let (mut l1, mut l2, mut l3) = (0u64, 0u64, 0u64);
    for sub in 0..32 {
        let r = cpuid(4, sub);
        let ty = r.eax & 0x1f;
        if ty == 0 {
            break;
        }
        let size = (((r.ebx >> 22) & 0x3ff) as u64 + 1) * (((r.ebx >> 12) & 0x3ff) as u64 + 1) * ((r.ebx & 0xfff) as u64 + 1) * (r.ecx as u64 + 1);
        match (r.eax >> 5) & 7 {
            1 => l1 += size,
            2 => l2 = size,
            3 => l3 = size,
            _ => {}
        }
    }
    let b = if l3 != 0 { l3 } else if l2 != 0 { l2 } else { l1 };
    (b / 1024) as u32
}

pub(super) fn common() -> Common {
    let l0 = cpuid(0, 0);
    let vendor = regs_text(&[l0.ebx, l0.edx, l0.ecx]);
    let l1 = cpuid(1, 0);
    let stepping = l1.eax & 0xf;
    let mut model = (l1.eax >> 4) & 0xf;
    let mut family = (l1.eax >> 8) & 0xf;
    if family == 0xf {
        family += (l1.eax >> 20) & 0xff;
    }
    if family >= 6 {
        model += ((l1.eax >> 16) & 0xf) << 4;
    }
    let max_ext = cpuid(0x8000_0000, 0).eax;
    let brand = if max_ext >= 0x8000_0004 {
        let mut r = Vec::new();
        for leaf in 0x8000_0002..=0x8000_0004u32 {
            let c = cpuid(leaf, 0);
            r.extend_from_slice(&[c.eax, c.ebx, c.ecx, c.edx]);
        }
        regs_text(&r)
    } else {
        String::new()
    };
    let (phys, virt) = if max_ext >= 0x8000_0008 {
        let e = cpuid(0x8000_0008, 0).eax;
        (e & 0xff, (e >> 8) & 0xff)
    } else {
        (36, 48)
    };
    // cpu_detect_cache_sizes: the L1 sum, then the L2 size and the L2 TLB entries.
    let (mut cache_kb, mut tlb) = (0, 0);
    if max_ext >= 0x8000_0005 {
        let r = cpuid(0x8000_0005, 0);
        cache_kb = (r.ecx >> 24) + (r.edx >> 24);
    }
    if max_ext >= 0x8000_0006 {
        let r = cpuid(0x8000_0006, 0);
        tlb = ((r.ebx >> 16) & 0xfff) + (r.ebx & 0xfff);
        cache_kb = r.ecx >> 16;
    }
    if vendor == "GenuineIntel" && l0.eax >= 4 {
        cache_kb = intel_cache_kb();
    }
    let power = if max_ext >= 0x8000_0007 { cpuid(0x8000_0007, 0).edx } else { 0 };
    let view = hal::cpu::live();
    let flags: Vec<&str> = hal::cpu::FLAGS.iter().filter(|d| view.bit(d)).map(|d| d.name).collect();
    Common {
        vendor,
        family,
        model,
        brand,
        stepping,
        tsc_hz: time::tsc_hz(),
        cache_kb,
        level: l0.eax,
        flags: flags.join(" "),
        tlb,
        clflush: ((l1.ebx >> 8) & 0xff) * 8,
        phys,
        virt,
        power,
    }
}

/// powerflags.c `x86_power_flags` (bit 8, the invariant TSC, prints nothing: it is `constant_tsc`).
const POWER: [&str; 13] = ["ts", "fid", "vid", "ttp", "tm", "stc", "100mhzsteps", "hwpstate", "", "cpb", "eff_freq_ro", "proc_feedback", "acc_power"];

/// One CPU's block (`show_cpuinfo`), ending with the empty line.
pub(super) fn block(out: &mut String, m: &Common, c: &Cpu, siblings: usize, cores: usize) {
    let unknown = |s: &str| if s.is_empty() { String::from("unknown") } else { String::from(s) };
    let _ = write!(
        out,
        "processor\t: {}\nvendor_id\t: {}\ncpu family\t: {}\nmodel\t\t: {}\nmodel name\t: {}\nstepping\t: {}\n",
        c.cpu,
        unknown(&m.vendor),
        m.family,
        m.model,
        unknown(&m.brand),
        m.stepping
    );
    let khz = m.tsc_hz / 1000;
    let _ = write!(out, "cpu MHz\t\t: {}.{:03}\n", khz / 1000, khz % 1000);
    if m.cache_kb != 0 {
        let _ = write!(out, "cache size\t: {} KB\n", m.cache_kb);
    }
    let _ = write!(
        out,
        "physical id\t: {}\nsiblings\t: {siblings}\ncore id\t\t: {}\ncpu cores\t: {cores}\napicid\t\t: {}\ninitial apicid\t: {}\n\
         fpu\t\t: yes\nfpu_exception\t: yes\ncpuid level\t: {}\nwp\t\t: yes\nflags\t\t: {}\nbugs\t\t:\n",
        c.pkg, c.core, c.apic, c.apic, m.level, m.flags
    );
    // loops_per_jiffy from the TSC: lpj = hz / HZ, bogomips = lpj / (500000 / HZ).
    let lpj_hz = m.tsc_hz / 1000;
    let _ = write!(out, "bogomips\t: {}.{:02}\n", lpj_hz * 2 / 1000, (lpj_hz * 2 / 10) % 100);
    if m.tlb > 0 {
        let _ = write!(out, "TLB size\t: {} 4K pages\n", m.tlb);
    }
    let _ = write!(
        out,
        "clflush size\t: {}\ncache_alignment\t: {}\naddress sizes\t: {} bits physical, {} bits virtual\npower management:",
        m.clflush, m.clflush, m.phys, m.virt
    );
    for i in 0..32 {
        if m.power & (1 << i) != 0 {
            match POWER.get(i) {
                Some(n) if n.is_empty() => {}
                Some(n) => {
                    let _ = write!(out, " {n}");
                }
                None => {
                    let _ = write!(out, " [{i}]");
                }
            }
        }
    }
    out.push_str("\n\n");
}

fn render() -> String {
    let m = common();
    let all = online();
    let mut s = String::new();
    for c in &all {
        let (sib, cores) = package_counts(&all, c);
        block(&mut s, &m, c, sib, cores);
    }
    s
}
vfs::procfile!("/proc/cpuinfo", render);
