//! AB-7b: `/sys/devices/system/cpu` (linux-6.6 drivers/base/cpu.c: online, possible, present, offline,
//! kernel_max, isolated, cpuN/online; drivers/base/topology.c: cpuN/topology/*). CPU lists in
//! `bitmap_print_list_to_buf` form ("0-15"), masks as `%*pb` over `nr_cpu_ids` (the possible CPUs) bits.
//! Topology comes from each CPU's own CPUID (`hal::smp::topology`); only online CPUs have a topology and a
//! cache directory (Linux makes both in CPU-hotplug callbacks, `CPUHP_TOPOLOGY_PREPARE` and
//! `cacheinfo_cpu_online`), and the masks hold online CPUs, as Linux's. `smt/` is kernel/cpu.c's.

use super::attr::{CPULIST_MAX, Live, PAGE};
use super::cpuinfo::{Cpu, online};
use super::sys_cache::CacheDir;
use super::stat::{bitmap_hex, bitmap_list};
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use uapi::errno::*;
use vfs::synth::{DirSource, SynthEntry};
use vfs::{Cred, Errno};

/// Linux distributions build with NR_CPUS 8192; V2 has no such limit (R1) but programs size masks from it.
const KERNEL_MAX: usize = 8191;

/// `nr_cpu_ids`: the CPUs the firmware lists (at least the online ones).
pub(super) fn possible() -> usize {
    hal::percpu::possible_cpus().max(online().last().map_or(1, |c| c.cpu + 1))
}

/// A bit set of CPU numbers as 64-bit words.
pub(super) fn words(cpus: impl Iterator<Item = usize>, bits: usize) -> Vec<u64> {
    let mut w = alloc::vec![0u64; bits.div_ceil(64).max(1)];
    for c in cpus {
        if let Some(x) = w.get_mut(c / 64) {
            *x |= 1 << (c % 64);
        }
    }
    w
}

pub(super) fn list_line(cpus: impl Iterator<Item = usize>, bits: usize) -> String {
    format!("{}\n", bitmap_list(&words(cpus, bits), bits))
}

fn online_set() -> Vec<usize> {
    online().iter().map(|c| c.cpu).collect()
}

/// `cpuN/online`: "1"; writing "1" to an online CPU is accepted, taking a CPU off is not possible in V2
/// (EBUSY, what Linux answers when a CPU cannot go down).
fn online_file(n: usize) -> Live {
    Live::new(PAGE, move || Ok(String::from(if online_set().contains(&n) { "1\n" } else { "0\n" }))).writable(move |_who: Cred, data: &[u8]| {
        match core::str::from_utf8(data).ok().map(str::trim) {
            Some("1") if online_set().contains(&n) => Ok(data.len()),
            Some("0") | Some("1") => {
                klog::event!(Info, "syscall", "sysfs.cpu_online_refused", cpu = n);
                Err(EBUSY)
            }
            _ => Err(EINVAL),
        }
    })
}

/// Which CPUs share something with `c`: the mask (`%*pb`) and list (`%*pbl`) file pair.
fn pair(out: &mut Vec<SynthEntry>, name: &str, n: usize, same: fn(&Cpu, &Cpu) -> bool) {
    let set = move || {
        let all = online();
        let me = all.iter().find(|c| c.cpu == n).copied();
        let ids: Vec<usize> = me.map(|m| all.iter().filter(|o| same(&m, o)).map(|o| o.cpu).collect()).unwrap_or_default();
        words(ids.into_iter(), possible())
    };
    out.push(SynthEntry::file(name, 0o444, Arc::new(Live::new(PAGE, move || Ok(format!("{}\n", bitmap_hex(&set(), possible())))))));
    out.push(SynthEntry::file(&format!("{name}_list"), 0o444, Arc::new(Live::new(CPULIST_MAX, move || Ok(format!("{}\n", bitmap_list(&set(), possible())))))));
}

struct TopologyDir(usize);

impl DirSource for TopologyDir {
    fn entries(&self) -> Result<Vec<SynthEntry>, Errno> {
        let n = self.0;
        let id = move |f: fn(&Cpu) -> u32| {
            Arc::new(Live::new(PAGE, move || online().iter().find(|c| c.cpu == n).map(|c| format!("{}\n", f(c))).ok_or(ENODEV)))
        };
        let mut v = alloc::vec![
            SynthEntry::file("physical_package_id", 0o444, id(|c| c.pkg)),
            SynthEntry::file("die_id", 0o444, id(|c| c.die)),
            SynthEntry::file("cluster_id", 0o444, id(|c| c.cluster)),
            SynthEntry::file("core_id", 0o444, id(|c| c.core)),
        ];
        pair(&mut v, "core_cpus", n, |a, b| a.pkg == b.pkg && a.die == b.die && a.core == b.core);
        pair(&mut v, "thread_siblings", n, |a, b| a.pkg == b.pkg && a.die == b.die && a.core == b.core);
        pair(&mut v, "core_siblings", n, |a, b| a.pkg == b.pkg);
        pair(&mut v, "cluster_cpus", n, |a, b| a.pkg == b.pkg && a.cluster == b.cluster);
        pair(&mut v, "die_cpus", n, |a, b| a.pkg == b.pkg && a.die == b.die);
        pair(&mut v, "package_cpus", n, |a, b| a.pkg == b.pkg);
        Ok(v)
    }
}

struct CpuDir(usize);

impl DirSource for CpuDir {
    fn entries(&self) -> Result<Vec<SynthEntry>, Errno> {
        let n = self.0;
        let mut v = Vec::new();
        // cpu0 of x86 cannot be taken off: Linux gives it no `online` file.
        if n != 0 {
            v.push(SynthEntry::file("online", 0o644, Arc::new(online_file(n))).cookie(1));
        }
        if online_set().contains(&n) {
            v.push(SynthEntry::dir("topology", Arc::new(TopologyDir(n))).with_perm(0o755).cookie(2));
            v.push(SynthEntry::dir("cache", Arc::new(CacheDir(n))).with_perm(0o755).cookie(3));
        }
        Ok(v)
    }
}

/// The CPU number of a "cpuN" name (no sign, no leading zero, as `name_to_int` reads names).
fn cpu_number(name: &[u8]) -> Option<usize> {
    vfs::synth::name_to_int(name.strip_prefix(b"cpu")?).map(|n| n as usize)
}

/// SMT threads per core as CPU 0's CPUID says (`cpu_smt_max_threads`); 1 without SMT or before the topology
/// is known.
fn smt_threads() -> u32 {
    hal::smp::topology(0).map_or(1, |t| t.threads_per_core.max(1))
}

/// `sched_smt_active`: some online CPU has an online sibling on its core.
pub(super) fn smt_active(all: &[Cpu]) -> bool {
    all.iter().any(|a| all.iter().any(|b| b.cpu != a.cpu && b.pkg == a.pkg && b.die == a.die && b.core == a.core))
}

/// `control_show`: "on" with SMT, "notsupported" without (V2 never turns SMT off).
pub(super) fn smt_control_text(threads: u32) -> &'static str {
    if threads > 1 { "on\n" } else { "notsupported\n" }
}

/// `__store_smt_control`: ENODEV without SMT; "on" or the full thread count is what V2 already does; "off",
/// "forceoff" or 1 would take CPUs down, which V2 cannot (EBUSY, as `cpuN/online`); anything else EINVAL.
pub(super) fn smt_control_store(threads: u32, data: &[u8]) -> Result<(), Errno> {
    if threads <= 1 {
        return Err(ENODEV);
    }
    let w = data.strip_suffix(b"\n").unwrap_or(data);
    let n = core::str::from_utf8(w).ok().and_then(|s| s.parse::<i32>().ok());
    match (w, n) {
        (b"on", _) => Ok(()),
        (_, Some(n)) if n == threads as i32 => Ok(()),
        (b"off" | b"forceoff", _) | (_, Some(1)) => Err(EBUSY),
        _ => Err(EINVAL),
    }
}

struct SmtDir;

impl DirSource for SmtDir {
    fn entries(&self) -> Result<Vec<SynthEntry>, Errno> {
        let control = Live::new(PAGE, || Ok(String::from(smt_control_text(smt_threads())))).writable(|_who: Cred, data: &[u8]| {
            let r = smt_control_store(smt_threads(), data);
            if let Err(e) = r {
                klog::event!(Info, "syscall", "sysfs.smt_control_refused", errno = e);
            }
            r.map(|()| data.len())
        });
        Ok(alloc::vec![
            SynthEntry::file("active", 0o444, Arc::new(Live::new(PAGE, || Ok(String::from(if smt_active(&online()) { "1\n" } else { "0\n" }))))).cookie(1),
            SynthEntry::file("control", 0o644, Arc::new(control)).cookie(2),
        ])
    }
}

struct CpuRoot;

impl CpuRoot {
    fn files() -> Vec<SynthEntry> {
        let f = |name: &str, g: fn() -> String| SynthEntry::file(name, 0o444, Arc::new(Live::new(PAGE, move || Ok(g()))));
        alloc::vec![
            f("isolated", || String::from("\n")),
            f("kernel_max", || format!("{KERNEL_MAX}\n")),
            f("offline", || {
                let on = online_set();
                list_line((0..possible()).filter(|c| !on.contains(c)), possible())
            }),
            f("online", || list_line(online_set().into_iter(), possible())),
            f("possible", || list_line(0..possible(), possible())),
            f("present", || list_line(0..possible(), possible())),
            SynthEntry::dir("smt", Arc::new(SmtDir)).with_perm(0o755),
        ]
    }
}

impl DirSource for CpuRoot {
    fn entries(&self) -> Result<Vec<SynthEntry>, Errno> {
        let mut v: Vec<SynthEntry> = (0..possible()).map(|n| SynthEntry::dir(&format!("cpu{n}"), Arc::new(CpuDir(n))).with_perm(0o755)).collect();
        v.extend(Self::files());
        Ok(v)
    }
    fn lookup(&self, name: &[u8]) -> Result<Option<SynthEntry>, Errno> {
        if let Some(n) = cpu_number(name) {
            return Ok((n < possible()).then(|| SynthEntry::dir(&format!("cpu{n}"), Arc::new(CpuDir(n))).with_perm(0o755)));
        }
        Ok(Self::files().into_iter().find(|e| e.name == name))
    }
}

pub(super) fn cpu_root() -> Arc<dyn DirSource> {
    Arc::new(CpuRoot)
}
