//! Pure checks and summaries over the bring-up results (host-tested): the machine's shape from the per-CPU
//! topology, consistency problems in it, and short names for the events.

use alloc::collections::BTreeSet;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use hal::smp::{MtrrOutcome, Topology, CORE_TYPE_EFFICIENCY, CORE_TYPE_PERFORMANCE};

/// Counts over the online CPUs' topology.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Shape {
    pub cpus: usize,
    pub packages: usize,
    pub dies: usize,
    pub cores: usize,
    pub clusters: usize,
    pub nodes: usize,
    /// Most threads seen on one core.
    pub max_threads_per_core: usize,
    pub p_cores: usize,
    pub e_cores: usize,
    /// The lowest capacity hint (`Topology::capacity` against the highest HWP number here; 1024 = uniform).
    pub min_capacity: u16,
}

pub fn shape(t: &[Topology]) -> Shape {
    let distinct = |f: &dyn Fn(&Topology) -> (u32, u32)| t.iter().map(f).collect::<BTreeSet<_>>().len();
    let mut per_core: alloc::collections::BTreeMap<(u32, u32), usize> = alloc::collections::BTreeMap::new();
    for x in t {
        *per_core.entry(x.core_key()).or_default() += 1;
    }
    let core_types = |ty: u8| t.iter().filter(|x| x.core_type == ty).map(|x| x.core_key()).collect::<BTreeSet<_>>().len();
    Shape {
        cpus: t.len(),
        packages: distinct(&|x| (x.package_id, 0)),
        dies: distinct(&|x| (x.die_id, 0)),
        cores: per_core.len(),
        clusters: distinct(&|x| (x.cluster_id, 0)),
        nodes: distinct(&|x| (x.node, 0)),
        max_threads_per_core: per_core.values().copied().max().unwrap_or(0),
        p_cores: core_types(CORE_TYPE_PERFORMANCE),
        e_cores: core_types(CORE_TYPE_EFFICIENCY),
        min_capacity: {
            let max = t.iter().map(|x| x.perf_highest).max().unwrap_or(0);
            t.iter().map(|x| x.capacity(max)).min().unwrap_or(hal::smp::CAPACITY_SCALE)
        },
    }
}

/// What is inconsistent in the topology of the online CPUs `(cpu, APIC ID from the MADT, topology)`; empty =
/// fine. A sibling whose thread 0 is not online (it failed to start) is not a problem.
pub fn topology_problems(cpus: &[(usize, u32, Topology)]) -> Vec<String> {
    let mut out = Vec::new();
    let mut ids = BTreeSet::new();
    let mut slots = BTreeSet::new();
    for &(cpu, apic, t) in cpus {
        if t.apic_id != apic {
            out.push(format!("cpu {cpu}: CPUID says APIC ID {}, the MADT {apic}", t.apic_id));
        }
        if !ids.insert(t.apic_id) {
            out.push(format!("cpu {cpu}: APIC ID {} twice", t.apic_id));
        }
        if !slots.insert((t.package_id, t.core_id, t.thread_id)) {
            out.push(format!("cpu {cpu}: package {} core {} thread {} twice", t.package_id, t.core_id, t.thread_id));
        }
        if t.threads_per_core != 0 && t.thread_id >= t.threads_per_core.next_power_of_two() {
            out.push(format!("cpu {cpu}: thread {} of a core with {} threads", t.thread_id, t.threads_per_core));
        }
        match (t.thread_id, t.smt_sibling_of) {
            (0, Some(s)) => out.push(format!("cpu {cpu}: thread 0 names sibling {s}")),
            (n, None) if n > 0 => out.push(format!("cpu {cpu}: thread {n} without a sibling")),
            (_, Some(s)) => {
                if let Some(&(_, _, first)) = cpus.iter().find(|c| c.2.apic_id == s)
                    && (first.core_key() != t.core_key() || first.cluster_id != t.cluster_id || first.die_id != t.die_id || first.thread_id != 0)
                {
                    out.push(format!("cpu {cpu}: sibling {s} is on another core, die or cluster"));
                }
            }
            _ => {}
        }
    }
    out
}

pub fn mtrr_name(m: MtrrOutcome) -> &'static str {
    match m {
        MtrrOutcome::Absent => "absent",
        MtrrOutcome::Same => "same",
        MtrrOutcome::Copied { syscfg: false, verified: true } => "copied",
        MtrrOutcome::Copied { syscfg: true, verified: true } => "copied_with_syscfg",
        MtrrOutcome::Copied { verified: false, .. } => "copied_not_verified",
    }
}

pub fn stage_name(s: hal::smp::ApStage) -> &'static str {
    use hal::smp::ApStage::*;
    match s {
        Silent => "silent",
        RealMode => "real_mode",
        LeftTrampoline => "left_trampoline",
        InKernel => "in_kernel",
        Refused => "refused",
        Online => "online",
        Ready => "ready",
    }
}

pub fn core_type_name(t: u8) -> &'static str {
    match t {
        CORE_TYPE_PERFORMANCE => "p",
        CORE_TYPE_EFFICIENCY => "e",
        _ => "uniform",
    }
}

/// Names joined with spaces (event fields).
/// The registers of `f` no boot line has named yet (`seen` were named): `None` when there is no new one. Text as
/// in the event's `new=` field.
pub fn new_fault_registers(f: &hal::msr::Faults, seen: usize) -> Option<String> {
    let new = f.registers().get(seen..).filter(|n| !n.is_empty())?;
    Some(new.iter().map(|r| format!("{r:#x}")).collect::<Vec<_>>().join(","))
}

pub fn joined(names: &[&str]) -> String {
    let mut s = String::new();
    for n in names {
        if !s.is_empty() {
            s.push(' ');
        }
        s.push_str(n);
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The boot lines name each faulted register once: the first line all of them, a later one only the new ones.
    #[test]
    fn fault_lines_name_each_register_once() {
        let log = hal::msr::FaultLog::new();
        assert_eq!(new_fault_registers(&log.snapshot(), 0), None, "nothing faulted: no line (the Mini-PC)");
        log.note(0xE2);
        log.note(0xE2);
        let at_smp = log.snapshot();
        assert_eq!(new_fault_registers(&at_smp, 0).as_deref(), Some("0xe2"));
        assert_eq!(new_fault_registers(&at_smp, at_smp.distinct), None, "a repeat of a named register: no second line");
        log.note(0x48);
        log.note(0x6A0);
        let at_boot = log.snapshot();
        assert_eq!(new_fault_registers(&at_boot, at_smp.distinct).as_deref(), Some("0x48,0x6a0"));
        assert_eq!(alloc::format!("{at_boot}"), "count=4 registers=0xe2,0x48,0x6a0");
    }

    fn t(apic: u32, pkg: u32, core: u32, thread: u32, tpc: u32) -> Topology {
        Topology {
            apic_id: apic,
            package_id: pkg,
            die_id: pkg,
            core_id: core,
            thread_id: thread,
            threads_per_core: tpc,
            smt_sibling_of: (thread > 0).then(|| apic - thread),
            cluster_id: pkg,
            node: 0,
            core_type: 0,
            source_leaf: 0xB,
            ..Default::default()
        }
    }

    #[test]
    fn two_sockets_smt() {
        let v: Vec<Topology> = (0..16).map(|a| t(a, a >> 3, (a & 7) >> 1, a & 1, 2)).collect();
        let s = shape(&v);
        assert_eq!(s, Shape { cpus: 16, packages: 2, dies: 2, cores: 8, clusters: 2, nodes: 1, max_threads_per_core: 2, p_cores: 0, e_cores: 0, min_capacity: 1024 });
        let cpus: Vec<(usize, u32, Topology)> = v.iter().enumerate().map(|(i, x)| (i, x.apic_id, *x)).collect();
        assert!(topology_problems(&cpus).is_empty(), "{:?}", topology_problems(&cpus));
    }

    #[test]
    fn problems_are_found() {
        let mut a = t(0, 0, 0, 0, 2);
        let mut b = t(1, 0, 0, 1, 2);
        b.cluster_id = 9;
        let c = t(2, 0, 0, 0, 2);
        a.smt_sibling_of = None;
        let p = topology_problems(&[(0, 0, a), (1, 1, b), (2, 7, c)]);
        assert!(p.iter().any(|x| x.contains("another core")), "{p:?}");
        assert!(p.iter().any(|x| x.contains("MADT 7")), "{p:?}");
        assert!(p.iter().any(|x| x.contains("twice")), "{p:?}");
        let mut d = t(5, 0, 2, 1, 2);
        d.smt_sibling_of = None;
        assert!(topology_problems(&[(0, 5, d)]).iter().any(|x| x.contains("without a sibling")));
    }

    #[test]
    fn hybrid_counts() {
        let mut v: Vec<Topology> = (0..8).map(|a| t(a * 2, 0, a, 0, 1)).collect();
        for (i, x) in v.iter_mut().enumerate() {
            x.core_type = if i < 3 { CORE_TYPE_PERFORMANCE } else { CORE_TYPE_EFFICIENCY };
        }
        let s = shape(&v);
        assert_eq!((s.p_cores, s.e_cores, s.max_threads_per_core), (3, 5, 1));
        assert_eq!(s.min_capacity, hal::smp::E_CORE_FALLBACK_CAPACITY, "no HWP numbers: the E-core fallback");
    }

    /// All 20 CPUs of the real 265K through `decode_topology` (the two dumped kinds with each CPU's real APIC ID), its
    /// real MADT and the numbers Linux shows on the machine (`hal/src/cpu/real_265k`).
    #[test]
    fn real_265k() {
        use hal::cpu::{CpuidResult, CpuidView};
        fn dump(text: &str) -> alloc::collections::BTreeMap<(u32, u32), CpuidResult> {
            let hex = |s: &str| u32::from_str_radix(s, 16).unwrap();
            text.lines()
                .map(|l| {
                    let w: Vec<&str> = l.split_whitespace().collect();
                    ((hex(w[0]), hex(w[1].trim_end_matches(':'))), CpuidResult { eax: hex(w[2]), ebx: hex(w[3]), ecx: hex(w[4]), edx: hex(w[5]) })
                })
                .collect()
        }
        let (p, e) = (dump(include_str!("../../hal/src/cpu/real_265k/cpuid-cpu0.txt")), dump(include_str!("../../hal/src/cpu/real_265k/cpuid-cpu8.txt")));
        let field = |l: &str, name: &str| l.split_whitespace().find_map(|f| f.strip_prefix(name)?.strip_prefix('=')).unwrap().parse::<u32>().unwrap();
        let madt = acpi::madt::parse(include_bytes!("../../acpi/src/tests/data/gaming-pc-265k-madt.bin"), 0, &mut acpi::Notes::new()).unwrap();
        let plan = crate::plan::plan(&madt, 0, true);
        assert_eq!(plan.aps.len(), 19, "every AP planned: {:?}", plan.skipped.iter().filter(|s| s.0 != 0).collect::<Vec<_>>());
        let mut all = Vec::new();
        let mut caps = Vec::new();
        for (i, l) in include_str!("../../hal/src/cpu/real_265k/topology.txt").lines().enumerate() {
            let apic = field(l, "apic");
            assert_eq!(madt.cpus[i].apic_id, apic, "the table's order is Linux's CPU numbering");
            if i > 0 {
                assert_eq!(plan.aps[i - 1].apic_id, apic);
            }
            let table = if i < 8 { &p } else { &e };
            let g = |leaf: u32, sub: u32| {
                let mut r = table.get(&(leaf, sub)).copied().unwrap_or_default();
                if leaf == 0x1F || leaf == 0xB {
                    r.edx = apic;
                }
                r
            };
            let t = Topology { perf_highest: field(l, "highest_perf") as u8, ..hal::smp::decode_topology(&CpuidView::new(&g), 0) };
            assert_eq!(t.core_id, field(l, "core"), "cpu {i}");
            caps.push(field(l, "cap") as u16);
            all.push((i, apic, t));
        }
        let topos: Vec<Topology> = all.iter().map(|x| x.2).collect();
        assert_eq!(topology_problems(&all), Vec::<String>::new());
        let s = shape(&topos);
        assert_eq!((s.cpus, s.packages, s.dies, s.cores, s.clusters, s.nodes), (20, 1, 1, 20, 1, 1));
        assert_eq!((s.max_threads_per_core, s.p_cores, s.e_cores), (1, 8, 12));
        assert_eq!(s.min_capacity, 802, "E cores: 69 of 88");
        for (t, c) in topos.iter().zip(&caps) {
            assert_eq!(t.capacity(88), *c);
        }
    }

    #[test]
    fn names() {
        assert_eq!(mtrr_name(MtrrOutcome::Copied { syscfg: false, verified: true }), "copied");
        assert_eq!(joined(&["a", "b"]), "a b");
        assert_eq!(joined(&[]), "");
        assert_eq!(core_type_name(2), "e");
    }
}
