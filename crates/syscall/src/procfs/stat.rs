//! `/proc/<pid>/{stat,status,statm}` and their per-thread forms (linux-6.6/fs/proc/array.c `do_task_stat`
//! :467, `proc_pid_status` :439 with `task_state`, `task_sig`, `task_cap`, `task_seccomp`,
//! `task_cpus_allowed`, `task_context_switch_counts`; fs/proc/task_mmu.c `task_mem` :31, `task_statm` :88).
//! The process view (`whole`) sums CPU time over the group and shows the leader's state, signals and switches
//! as Linux does; a thread view shows that thread.

use super::Target;
use super::maps::{Mem, mem_of};
use alloc::format;
use alloc::string::String;
use core::fmt::Write;
use proc::procinfo::SigView;
use proc::{Group, Process, Thread};
use sched::ThreadStat;
use sched::cputime::clock_t;
use uapi::ab::rlimit::{RLIMIT_RSS, RLIMIT_SIGPENDING};

/// PF_RANDOMIZE and PF_FORKNOEXEC (include/linux/sched.h).
const PF_RANDOMIZE: u64 = 0x0040_0000;
const PF_FORKNOEXEC: u64 = 0x0000_0040;
/// The capability set of a kernel without user namespaces' restrictions (CAP_LAST_CAP 40).
const CAP_FULL: u64 = 0x0000_01ff_ffff_ffff;
/// MAX_NUMNODES of CachyOS's kernel (NODES_SHIFT 10): `Mems_allowed` prints that many bits.
const NODE_BITS: usize = 1024;

/// `comm`: the thread's name (PR_SET_NAME, a comm write, the exec's base name); a view whose thread has
/// ended keeps the image's name cut to 15 bytes.
pub(super) fn comm_of(p: &Process, th: Option<&Thread>) -> String {
    match th {
        Some(t) => t.prctl().comm_str(),
        None => {
            let c = proc::prctl_state::comm_of(p.name.as_bytes());
            let n = c.iter().position(|&b| b == 0).unwrap_or(c.len());
            String::from_utf8_lossy(&c[..n]).into_owned()
        }
    }
}

/// `seq_escape_str(.., ESCAPE_SPACE | ESCAPE_SPECIAL, "\n\\")`: only newline and backslash are escaped.
fn escape_name(s: &str) -> String {
    let mut o = String::new();
    for c in s.chars() {
        match c {
            '\n' => o.push_str("\\n"),
            '\\' => o.push_str("\\\\"),
            _ => o.push(c),
        }
    }
    o
}

fn sched_of(th: Option<&Thread>) -> Option<ThreadStat> {
    let k = th?.kthread();
    sched::thread_stats().into_iter().find(|s| s.id == k)
}

/// `task_state_array`: zombie for an ended group or an ended leader, T in a group stop, S while blocked.
fn state(g: &Group, th: Option<&Thread>, st: Option<&ThreadStat>, sig: &SigView) -> &'static str {
    if g.is_finished() || th.is_none() {
        return "Z (zombie)";
    }
    if sig.stopped {
        return "T (stopped)";
    }
    match st.map(|s| s.state) {
        Some("sleeping") => "S (sleeping)",
        Some("dead") => "Z (zombie)",
        _ => "R (running)",
    }
}

/// The signal key of the view's thread (None when it has ended).
fn sig_of(g: &Group, th: Option<&Thread>) -> SigView {
    g.sig_view(th.map(|t| t.sig_key()))
}

/// `render_sigset_t`: 64 signals as 16 hex digits, highest first.
pub(super) fn sigset(v: u64) -> String {
    format!("{v:016x}")
}

/// `%*pb`: a bitmap of `bits` bits in hex, comma-separated 32-bit groups (the first group only as wide as
/// needed for `bits`).
pub(super) fn bitmap_hex(words: &[u64], bits: usize) -> String {
    let groups = bits.div_ceil(32).max(1);
    let mut o = String::new();
    for gi in (0..groups).rev() {
        let w = words.get(gi / 2).copied().unwrap_or(0);
        let mut v = (w >> ((gi % 2) * 32)) as u32;
        let width = if gi == groups - 1 { (bits - gi * 32).div_ceil(4).max(1) } else { 8 };
        if width < 8 {
            v &= (1u32 << (width * 4)) - 1;
        }
        let _ = write!(o, "{v:0width$x}");
        if gi != 0 {
            o.push(',');
        }
    }
    o
}

/// `%*pbl`: the set bits as ranges ("0-3,8").
pub(super) fn bitmap_list(words: &[u64], bits: usize) -> String {
    let has = |i: usize| words.get(i / 64).is_some_and(|w| w >> (i % 64) & 1 != 0);
    let mut o = String::new();
    let mut i = 0;
    while i < bits {
        if !has(i) {
            i += 1;
            continue;
        }
        let s = i;
        while i + 1 < bits && has(i + 1) {
            i += 1;
        }
        if !o.is_empty() {
            o.push(',');
        }
        if s == i {
            let _ = write!(o, "{s}");
        } else {
            let _ = write!(o, "{s}-{i}");
        }
        i += 1;
    }
    o
}

/// `task_prio`: 20 + nice for the normal policies, -1 - rt_priority for FIFO/RR.
fn prio(st: Option<&ThreadStat>) -> (i64, i64, u64, u64) {
    match st {
        Some(s) => {
            let pol = s.policy.linux() as u64;
            if pol == 1 || pol == 2 {
                (-1 - s.rt_prio as i64, 0, s.rt_prio as u64, pol)
            } else {
                (20 + s.nice as i64, s.nice as i64, 0, pol)
            }
        }
        None => (20, 0, 0, 0),
    }
}

/// `/proc/<pid>/stat` (`whole`) and `/proc/<pid>/task/<tid>/stat`: 52 fields as `do_task_stat`.
/// `permitted` (ptrace READ) shows the addresses and the exit code, as Linux.
pub(super) fn render_stat(t: &Target, g: &Group, p: &Process, th: Option<&Thread>, permitted: bool) -> String {
    let st = sched_of(th);
    let sig = sig_of(g, th);
    let state = state(g, th, st.as_ref(), &sig);
    let alive = !g.is_finished();
    let m = if alive { mem_of(p) } else { Mem::default() };
    let info = p.image_info.lock().clone();
    let threads = g.thread_count();
    let (utime, stime) = if t.whole {
        let c = g.cpu_times();
        (c.user_ns, c.system_ns)
    } else {
        st.as_ref().map_or((0, 0), |s| (s.user_ns, s.system_ns))
    };
    let ch = g.cpu.children();
    let (prio, nice, rt, policy) = prio(st.as_ref());
    let leader = t.tid == t.tgid;
    let flags = PF_RANDOMIZE | if !leader || info.forked { PF_FORKNOEXEC } else { 0 };
    let rsslim = g.limits.get(RLIMIT_RSS).map_or(u64::MAX, |l| l.cur);
    let wchan = permitted && (!t.whole || threads < 2) && state.starts_with('S');
    let exit_signal: i64 = if leader { g.exit_signal() as i64 } else { -1 };
    let exit_code = if permitted && t.whole { g.status().map_or(0, |s| s.wait_status()) } else { 0 };
    let cpu = st.as_ref().map_or(0, |s| s.cpu);
    let low = |v: u64| v & 0x7fff_ffff;
    let (scode, ecode, sstack) = match (alive, permitted) {
        (false, _) => (0, 0, 0),
        (true, true) => (info.start_code, info.end_code, info.start_stack),
        (true, false) => (1, 1, 0),
    };
    let mut s = format!(
        "{pid} ({comm}) {st} {ppid} {pgrp} {sid} 0 -1 {flags} 0 0 0 0 {ut} {stt} {cut} {cst} {prio} {nice} {threads} 0 {start} {vsize} {rss} {rsslim} \
         {scode} {ecode} {sstack} 0 0 {pend} {blk} {ign} {cgt} {wchan} 0 0 {exit_signal} {cpu} {rt} {policy} 0 0 0",
        pid = t.tid,
        comm = comm_of(p, th),
        st = &state[..1],
        ppid = g.ppid(),
        pgrp = g.pgid(),
        sid = g.sid(),
        ut = clock_t(utime),
        stt = clock_t(stime),
        cut = clock_t(ch.user_ns),
        cst = clock_t(ch.system_ns),
        start = clock_t(g.start_ns),
        vsize = m.total,
        rss = m.rss_pages,
        pend = low(sig.pending),
        blk = low(sig.blocked),
        ign = low(sig.ignored),
        cgt = low(sig.caught),
        wchan = wchan as u8,
    );
    if alive && permitted {
        let _ = write!(s, " {} {} {} {} {} {} {}", info.start_data, info.end_data, m.start_brk, info.arg_start, info.arg_end, info.env_start, info.env_end);
    } else {
        s.push_str(" 0 0 0 0 0 0 0");
    }
    let _ = writeln!(s, " {exit_code}");
    s
}

/// `/proc/<pid>/status` and the thread form (`proc_pid_status`; the mm part only while the image lives).
pub(super) fn render_status(t: &Target, g: &Group, p: &Process, th: Option<&Thread>) -> String {
    let st = sched_of(th);
    let sig = sig_of(g, th);
    let alive = !g.is_finished();
    let mut s = String::new();
    let k = p.cred();
    let groups: String = k.groups.iter().enumerate().map(|(i, g)| if i == 0 { format!("{g}") } else { format!(" {g}") }).collect();
    let _ = write!(
        s,
        "Name:\t{name}\nUmask:\t{umask:04o}\nState:\t{state}\nTgid:\t{tgid}\nNgid:\t0\nPid:\t{pid}\nPPid:\t{ppid}\nTracerPid:\t0\n\
         Uid:\t{}\t{}\t{}\t{}\nGid:\t{}\t{}\t{}\t{}\nFDSize:\t{fds}\nGroups:\t{groups} \n\
         NStgid:\t{tgid}\nNSpid:\t{pid}\nNSpgid:\t{pgid}\nNSsid:\t{sid}\nKthread:\t0\n",
        k.uid,
        k.euid,
        k.suid,
        k.fsuid,
        k.gid,
        k.egid,
        k.sgid,
        k.fsgid,
        name = escape_name(&comm_of(p, th)),
        umask = p.fds.fs.umask(),
        state = state(g, th, st.as_ref(), &sig),
        tgid = g.tgid,
        pid = t.tid,
        ppid = g.ppid(),
        fds = p.fds.capacity(),
        pgid = g.pgid(),
        sid = g.sid(),
    );
    if alive {
        let m = mem_of(p);
        let info = p.image_info.lock().clone();
        let kb = |b: u64| b / 1024;
        // Counters (task_mem, linux-6.6/fs/proc/task_mmu.c:31): the peaks are read after the walk, so a
        // mapping added in between may lift the current value above them; Linux's hiwater_* are maxima too.
        let st = p.mm.mem_stats();
        let vm = kb(m.total);
        let rss = st.rss * 4;
        let (text, lib) = m.text_lib(&info);
        let _ = write!(
            s,
            "VmPeak:\t{peak:>8} kB\nVmSize:\t{vm:>8} kB\nVmLck:\t{z:>8} kB\nVmPin:\t{z:>8} kB\nVmHWM:\t{hwm:>8} kB\nVmRSS:\t{rss:>8} kB\n\
             RssAnon:\t{anon:>8} kB\nRssFile:\t{file:>8} kB\nRssShmem:\t{shmem:>8} kB\nVmData:\t{data:>8} kB\nVmStk:\t{stk:>8} kB\n\
             VmExe:\t{text:>8} kB\nVmLib:\t{lib:>8} kB\nVmPTE:\t{pte:>8} kB\nVmSwap:\t{z:>8} kB\nHugetlbPages:\t{z:>8} kB\n\
             CoreDumping:\t0\nTHP_enabled:\t{thp}\nuntag_mask:\t0xffffffffffffffff\n",
            peak = (st.peak * 4).max(vm),
            hwm = (st.hiwater_rss * 4).max(rss),
            anon = st.anon * 4,
            file = st.file * 4,
            shmem = st.shmem * 4,
            pte = st.page_table_bytes() / 1024,
            z = 0,
            data = kb(m.data),
            stk = kb(m.stack),
            text = kb(text),
            lib = kb(lib),
            thp = !p.mm_flags.thp_disabled() as u8,
        );
    }
    let qlim = g.limits.get(RLIMIT_SIGPENDING).map_or(0, |l| l.cur);
    let bset = th.map_or(CAP_FULL, |t| t.prctl().cap_bset());
    let (prm, eff) = proc::cred::cap_sets(&k, bset);
    let nnp = th.is_some_and(|t| t.prctl().no_new_privs()) as u8;
    let _ = write!(
        s,
        "Threads:\t{threads}\nSigQ:\t{q}/{qlim}\nSigPnd:\t{pnd}\nShdPnd:\t{shd}\nSigBlk:\t{blk}\nSigIgn:\t{ign}\nSigCgt:\t{cgt}\n\
         CapInh:\t0000000000000000\nCapPrm:\t{prm:016x}\nCapEff:\t{eff:016x}\nCapBnd:\t{bset:016x}\nCapAmb:\t0000000000000000\n\
         NoNewPrivs:\t{nnp}\nSeccomp:\t0\nSeccomp_filters:\t0\nSpeculation_Store_Bypass:\tnot vulnerable\n\
         SpeculationIndirectBranch:\tnot affected\n",
        threads = g.thread_count(),
        q = sig.queued,
        pnd = sigset(sig.pending),
        shd = sigset(sig.shared),
        blk = sigset(sig.blocked),
        ign = sigset(sig.ignored),
        cgt = sigset(sig.caught),
    );
    let cpus = sched::online_cpus().max(1);
    let mask = th.and_then(|t| sched::attr::affinity(t.kthread())).unwrap_or_else(|| alloc::vec![u64::MAX; cpus.div_ceil(64)]);
    let (vol, invol) = st.as_ref().map_or((0, 0), |s| (s.nvcsw, s.nivcsw));
    let _ = write!(
        s,
        "Cpus_allowed:\t{}\nCpus_allowed_list:\t{}\nMems_allowed:\t{}\nMems_allowed_list:\t0\n\
         voluntary_ctxt_switches:\t{vol}\nnonvoluntary_ctxt_switches:\t{invol}\nx86_Thread_features:\t\nx86_Thread_features_locked:\t\n",
        bitmap_hex(&mask, cpus),
        bitmap_list(&mask, cpus),
        bitmap_hex(&[1], NODE_BITS),
    );
    s
}

/// `/proc/<pid>/statm`: size resident shared text lib(0) data dt(0), in pages (`task_statm`).
pub(super) fn render_statm(g: &Group, p: &Process) -> String {
    if g.is_finished() {
        return String::from("0 0 0 0 0 0 0\n");
    }
    let m = mem_of(p);
    let st = p.mm.mem_stats();
    let info = p.image_info.lock().clone();
    let text = (info.end_code.next_multiple_of(4096).saturating_sub(info.start_code & !4095)) / 4096;
    // shared = file + shmem pages (task_statm, linux-6.6/fs/proc/task_mmu.c:88).
    format!("{} {} {} {} 0 {} 0\n", m.total / 4096, st.rss, st.file + st.shmem, text, (m.data + m.stack) / 4096)
}
