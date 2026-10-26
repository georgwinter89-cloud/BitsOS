//! AB-7b: the system-wide text files of /proc (linux-6.6 fs/proc/{meminfo,stat,uptime,loadavg,version}.c,
//! fs/filesystems.c). The formatters are pure (host tests); the `render_*` functions gather the numbers.
//! Values V2 does not keep are 0 (see the notes): page-cache, slab, page-table and IRQ counts, iowait.

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write;
use sched::cputime::clock_t;

/// `show_val_kb`: the name with its colon padded to 16 columns, the number right-aligned to 8, " kB".
pub(super) fn kb_line(s: &mut String, name: &str, kb: u64) {
    let key = alloc::format!("{name}:");
    let _ = writeln!(s, "{key:<16}{kb:>8} kB");
}

/// The numbers behind `/proc/meminfo`, in kB.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct Mem {
    pub total: u64,
    pub free: u64,
    pub shmem: u64,
    /// Page-cache file pages besides shmem (FAT, ext4, block files; FS-14 CR 1). No LRU, so all on the active list.
    pub file: u64,
    pub commit_limit: u64,
    pub committed: u64,
    /// MemAvailable: free memory plus the reclaimable page cache (MM-9, `vfs::reclaim::mem_available_bytes`).
    pub available: u64,
}

/// `VMALLOC_TOTAL >> 10` with 4-level paging, as Linux prints it.
const VMALLOC_TOTAL_KB: u64 = 34_359_738_367;

/// `meminfo_proc_show` with CachyOS's configuration (ZSWAP, MEMORY_FAILURE, THP, CMA, UNACCEPTED_MEMORY) and
/// `hugetlb_report_meminfo`. Shared memory (tmpfs, /dev/shm, memfd) is Linux's shmem: in Cached, on the anon
/// LRU, and never reclaimable (no swap). Other page-cache pages are file pages in Cached and Active(file).
/// MemAvailable is si_mem_available, linux-6.6/mm/show_mem.c:32 (MM-9 reclaims clean file pages).
pub(super) fn meminfo(m: &Mem) -> String {
    let mut s = String::new();
    let z = 0;
    for (k, v) in [
        ("MemTotal", m.total),
        ("MemFree", m.free),
        ("MemAvailable", m.available),
        ("Buffers", z),
        ("Cached", m.shmem + m.file),
        ("SwapCached", z),
        ("Active", m.shmem + m.file),
        ("Inactive", z),
        ("Active(anon)", m.shmem),
        ("Inactive(anon)", z),
        ("Active(file)", m.file),
        ("Inactive(file)", z),
        ("Unevictable", z),
        ("Mlocked", z),
        ("SwapTotal", z),
        ("SwapFree", z),
        ("Zswap", z),
        ("Zswapped", z),
        ("Dirty", z),
        ("Writeback", z),
        ("AnonPages", z),
        ("Mapped", z),
        ("Shmem", m.shmem),
        ("KReclaimable", z),
        ("Slab", z),
        ("SReclaimable", z),
        ("SUnreclaim", z),
        ("KernelStack", z),
        ("PageTables", z),
        ("SecPageTables", z),
        ("NFS_Unstable", z),
        ("Bounce", z),
        ("WritebackTmp", z),
        ("CommitLimit", m.commit_limit),
        ("Committed_AS", m.committed),
        ("VmallocTotal", VMALLOC_TOTAL_KB),
        ("VmallocUsed", z),
        ("VmallocChunk", z),
        ("Percpu", z),
    ] {
        kb_line(&mut s, k, v);
    }
    s.push_str("HardwareCorrupted:     0 kB\n");
    for k in ["AnonHugePages", "ShmemHugePages", "ShmemPmdMapped", "FileHugePages", "FilePmdMapped", "CmaTotal", "CmaFree", "Unaccepted"] {
        kb_line(&mut s, k, 0);
    }
    s.push_str("HugePages_Total:       0\nHugePages_Free:        0\nHugePages_Rsvd:        0\nHugePages_Surp:        0\n");
    kb_line(&mut s, "Hugepagesize", 2048);
    kb_line(&mut s, "Hugetlb", 0);
    s
}

fn render_meminfo() -> String {
    let (total, free) = mm_core::frames().map(|f| f.stats()).map_or((0, 0), |s| (s.total_bytes, s.free_bytes));
    let pages_kb = |p: u64| p.saturating_mul(4);
    // Since FS-14b every page-cache page, tmpfs included, is a Kind::PageCache frame (bytes here).
    let pc = mm_core::frames().map_or(0, |f| f.stats().used_by_kind[mm_core::Kind::PageCache as usize]) / 1024;
    let shmem = pages_kb(vfs::inode::tmpfs_pages_used());
    meminfo(&Mem {
        total: total / 1024,
        free: free / 1024,
        shmem,
        file: pc.saturating_sub(shmem),
        commit_limit: pages_kb(mm::commit::limit()),
        committed: pages_kb(mm::commit::committed()),
        available: vfs::reclaim::mem_available_bytes() / 1024,
    })
}
vfs::procfile!("/proc/meminfo", render_meminfo);

/// One `cpu` line's times in ns (user without nice, nice, system, idle).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct CpuTimes {
    pub user: u64,
    pub nice: u64,
    pub system: u64,
    pub idle: u64,
}

impl CpuTimes {
    fn add(&mut self, o: &CpuTimes) {
        self.user = self.user.saturating_add(o.user);
        self.nice = self.nice.saturating_add(o.nice);
        self.system = self.system.saturating_add(o.system);
        self.idle = self.idle.saturating_add(o.idle);
    }
}

/// The counters of `/proc/stat` after the cpu lines.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct StatTail {
    pub ctxt: u64,
    pub btime: u64,
    pub processes: u64,
    pub running: u64,
}

fn cpu_line(s: &mut String, name: &str, t: &CpuTimes) {
    // iowait, irq, softirq, steal, guest, guest_nice: V2 keeps none of them.
    let _ = writeln!(s, "{name} {} {} {} {} 0 0 0 0 0 0", clock_t(t.user), clock_t(t.nice), clock_t(t.system), clock_t(t.idle));
}

/// `show_stat`: the sum line ("cpu" and two spaces), one line per online CPU, intr (V2 counts no IRQs per
/// line: the sum alone, 0), ctxt, btime, processes, procs_running, procs_blocked (no iowait: 0), softirq.
pub(super) fn stat(cpus: &[(usize, CpuTimes)], t: &StatTail) -> String {
    let mut s = String::new();
    let mut sum = CpuTimes::default();
    for (_, c) in cpus {
        sum.add(c);
    }
    cpu_line(&mut s, "cpu ", &sum);
    for (n, c) in cpus {
        cpu_line(&mut s, &alloc::format!("cpu{n}"), c);
    }
    let _ = write!(
        s,
        "intr 0\nctxt {}\nbtime {}\nprocesses {}\nprocs_running {}\nprocs_blocked 0\nsoftirq 0 0 0 0 0 0 0 0 0 0 0\n",
        t.ctxt, t.btime, t.processes, t.running
    );
    s
}

/// Runnable threads now, the reader included (`nr_running`): each online CPU's queue plus what it runs.
fn nr_running(st: &[sched::CpuStat]) -> u64 {
    st.iter().filter(|c| c.online).map(|c| c.queued as u64 + if c.idle { 0 } else { 1 }).sum::<u64>().max(1)
}

fn render_stat() -> String {
    let st = sched::cpu_stats();
    let cpus: Vec<(usize, CpuTimes)> = st
        .iter()
        .filter(|c| c.online)
        .map(|c| {
            let user = c.user_ns.min(c.busy_ns);
            (c.cpu, CpuTimes { user: user - c.nice_ns.min(user), nice: c.nice_ns.min(user), system: c.busy_ns - user, idle: c.idle_ns })
        })
        .collect();
    let now = time::now_ns();
    let btime = time::wall::realtime_ns().map_or(0, |r| r.saturating_sub(now) / 1_000_000_000);
    let tail = StatTail { ctxt: st.iter().map(|c| c.switches).sum(), btime, processes: proc::forks_total(), running: nr_running(&st) };
    stat(&cpus, &tail)
}
vfs::procfile!("/proc/stat", render_stat);

/// `uptime_proc_show`: seconds and hundredths of the boot clock and of the idle time summed over the CPUs.
pub(super) fn uptime(up_ns: u64, idle_ns: u64) -> String {
    let two = |ns: u64| alloc::format!("{}.{:02}", ns / 1_000_000_000, (ns % 1_000_000_000) / 10_000_000);
    alloc::format!("{} {}\n", two(up_ns), two(idle_ns))
}

fn render_uptime() -> String {
    uptime(time::now_ns(), sched::cputime::idle_ns_total())
}
vfs::procfile!("/proc/uptime", render_uptime);

const FSHIFT: u32 = 11;
const FIXED_1: u64 = 1 << FSHIFT;

/// `loadavg_proc_show`: the three averages rounded (`get_avenrun(.., FIXED_1/200, 0)`) as LOAD_INT.LOAD_FRAC,
/// running/total threads, the last pid handed out.
pub(super) fn loadavg(avenrun: [u64; 3], running: u64, threads: u64, last_pid: u32) -> String {
    let mut s = String::new();
    for a in avenrun {
        let x = a + FIXED_1 / 200;
        let _ = write!(s, "{}.{:02} ", x >> FSHIFT, ((x & (FIXED_1 - 1)) * 100) >> FSHIFT);
    }
    let _ = writeln!(s, "{running}/{threads} {last_pid}");
    s
}

fn render_loadavg() -> String {
    loadavg(crate::sysinfo::avenrun(), nr_running(&sched::cpu_stats()), sched::thread_count() as u64, proc::last_pid())
}
vfs::procfile!("/proc/loadavg", render_loadavg);

/// `/proc/interrupts`, the IPI rows only (INT-W7, YT-5 CR 1 / MM-tlb): the header of `show_interrupts`
/// (linux-6.6/kernel/irq/proc.c:477-483, prec 3) and `arch_show_interrupts`' RES and TLB rows
/// (arch/x86/kernel/irq.c:97-108), so a playback's reschedule and TLB-shootdown IPIs can be read before and
/// after. Device IRQ rows are not listed (hw_info's msi section has them).
pub(super) fn interrupts(cpus: &[(usize, u64, u64)]) -> String {
    let mut s = String::new();
    let _ = write!(s, "{:11}", "");
    for (c, _, _) in cpus {
        let _ = write!(s, "CPU{c:<8}");
    }
    s.push('\n');
    for (name, what, pick) in [("RES", "Rescheduling interrupts", 1usize), ("TLB", "TLB shootdowns", 2)] {
        let _ = write!(s, "{name:>3}: ");
        for &(_, res, tlb) in cpus {
            let _ = write!(s, "{:10} ", if pick == 1 { res } else { tlb } as u32);
        }
        let _ = writeln!(s, "  {what}");
    }
    s
}

fn render_interrupts() -> String {
    let cpus: Vec<(usize, u64, u64)> =
        sched::cpu_stats().iter().filter(|c| c.online).map(|c| (c.cpu, c.resched_ipis, hal::paging::tlb::cpu_irqs(c.cpu))).collect();
    interrupts(&cpus)
}
vfs::procfile!("/proc/interrupts", render_interrupts);

/// `linux_proc_banner`: "%s version %s (builder@host) (compiler) %s".
pub(super) fn version() -> String {
    alloc::format!("Linux version {} (bitsos@bitsos) (rustc) {}\n", crate::info::RELEASE, crate::info::VERSION)
}
vfs::procfile!("/proc/version", version);

/// `filesystems_proc_show`: the file system types V2's kernel has, in Linux's registration order (ext4 before
/// vfat, linux-6.6/fs/Makefile:65,77), "nodev" for those without a device. pipefs and sockfs name the objects behind pipe and socket fds (their
/// `/proc/<pid>/fd` links); V2 has no mount(2) yet, so none of them can be mounted by a program.
pub(super) fn filesystems() -> String {
    String::from("nodev\tsysfs\nnodev\ttmpfs\nnodev\tproc\nnodev\tdevtmpfs\nnodev\tsockfs\nnodev\tpipefs\n\text4\n\tvfat\n")
}
vfs::procfile!("/proc/filesystems", filesystems);
