//! The Gaming-PC's Intel Core Ultra 7 265K as test data (host tests only): raw CPUID of a normal P core (cpu0), a
//! favoured P core (cpu3) and E cores (cpu8, cpu19), read from the machine itself on 5.11.2026, and Linux's view of
//! all 20 CPUs (`real_265k/topology.txt`). Other crates' tests `include_str!` the same files.

use super::CpuidResult;
use alloc::collections::BTreeMap;
use alloc::vec::Vec;

/// The dump of one CPU.
#[derive(Clone)]
pub struct Dump(BTreeMap<(u32, u32), CpuidResult>);

fn parse(text: &str) -> Dump {
    let hex = |s: &str| u32::from_str_radix(s, 16).expect("hex in the dump");
    let mut t = BTreeMap::new();
    for line in text.lines() {
        let w: Vec<&str> = line.split_whitespace().collect();
        let [leaf, sub, eax, ebx, ecx, edx] = w[..] else { panic!("line {line:?}") };
        t.insert((hex(leaf), hex(sub.trim_end_matches(':'))), CpuidResult { eax: hex(eax), ebx: hex(ebx), ecx: hex(ecx), edx: hex(edx) });
    }
    Dump(t)
}

pub fn cpu0() -> Dump {
    parse(include_str!("real_265k/cpuid-cpu0.txt"))
}
pub fn cpu3() -> Dump {
    parse(include_str!("real_265k/cpuid-cpu3.txt"))
}
pub fn cpu8() -> Dump {
    parse(include_str!("real_265k/cpuid-cpu8.txt"))
}
pub fn cpu19() -> Dump {
    parse(include_str!("real_265k/cpuid-cpu19.txt"))
}

impl Dump {
    pub fn into_map(self) -> BTreeMap<(u32, u32), CpuidResult> {
        self.0
    }

    pub fn getter(&self) -> impl Fn(u32, u32) -> CpuidResult + '_ {
        |l, s| self.0.get(&(l, s)).copied().unwrap_or_default()
    }

    /// Changes one leaf (a test's own variation of the real CPU).
    pub fn patch(&mut self, leaf: u32, sub: u32, f: impl FnOnce(&mut CpuidResult)) {
        f(self.0.entry((leaf, sub)).or_default());
    }

    /// The same CPU kind with another APIC ID: the fields of CPUID that carry it (leaf 1 EBX[31:24], 0xB and 0x1F EDX
    /// in every sub-leaf). The CPUs that were not dumped differ from their kind's dump only there.
    pub fn with_apic(&self, apic: u32) -> Dump {
        let mut d = self.clone();
        for (&(leaf, _), r) in d.0.iter_mut() {
            match leaf {
                1 => r.ebx = (r.ebx & 0x00FF_FFFF) | (apic << 24),
                0xB | 0x1F => r.edx = apic,
                _ => {}
            }
        }
        d
    }
}

/// One line of `topology.txt`: Linux's view of one CPU.
#[derive(Clone, Copy, Debug)]
pub struct LinuxCpu {
    pub cpu: usize,
    pub core: u32,
    pub cap: u16,
    pub highest_perf: u8,
    pub apic: u32,
    /// The lowest CPU of its L2 domain.
    pub l2_first: usize,
}

pub fn linux_view() -> Vec<LinuxCpu> {
    include_str!("real_265k/topology.txt")
        .lines()
        .map(|line| {
            let mut w = line.split_whitespace();
            let cpu = w.next().and_then(|c| c.strip_prefix("cpu")).expect("cpuN").parse().expect("cpu number");
            let field = |name: &str| line.split_whitespace().find_map(|f| f.strip_prefix(name)?.strip_prefix('=')).expect("field");
            LinuxCpu {
                cpu,
                core: field("core").parse().expect("core"),
                cap: field("cap").parse().expect("cap"),
                highest_perf: field("highest_perf").parse().expect("perf"),
                apic: field("apic").parse().expect("apic"),
                l2_first: field("l2").split('-').next().expect("l2").parse().expect("l2 cpu"),
            }
        })
        .collect()
}
