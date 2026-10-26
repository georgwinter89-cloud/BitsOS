//! `/proc/<pid>/{cmdline,environ,auxv,limits,oom_score,oom_score_adj}` and the `comm` write
//! (linux-6.6/fs/proc/base.c `get_mm_cmdline` :253, `get_mm_proctitle` :213, `environ_read` :937,
//! `proc_pid_auxv`, `proc_pid_limits` :596 with `lnames` :576, `proc_oom_score`, `oom_score_adj_write` :1213,
//! `comm_write` :1664). The strings are read from the program's memory as Linux's `access_remote_vm` does,
//! so a program that rewrites its argv (setproctitle) is seen.

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write;
use proc::{Group, Process, Thread};
use uapi::ab::rlimit::{RLIM_INFINITY, RLIM_NLIMITS};
use uapi::errno::*;
use vfs::Errno;

/// `access_remote_vm` page by page into a buffer that grows as it goes (no cap of its own: the ranges are the
/// ones the exec laid out). A page the program unmapped ends the text there (a short read, as Linux's);
/// `until_nul` stops after the first NUL.
fn read_mem(p: &Process, start: u64, end: u64, until_nul: bool) -> Result<Vec<u8>, Errno> {
    let mut v: Vec<u8> = Vec::new();
    let mut a = start;
    while a < end {
        let n = (4096 - (a & 4095)).min(end - a) as usize;
        v.try_reserve(n).map_err(|_| ENOMEM)?;
        let at = v.len();
        v.resize(at + n, 0);
        if p.mm.copy_from_user(&mut v[at..], a).is_err() {
            v.truncate(at);
            break;
        }
        if until_nul {
            if let Some(i) = v[at..].iter().position(|&b| b == 0) {
                v.truncate(at + i + 1);
                break;
            }
        }
        a += n as u64;
    }
    Ok(v)
}

/// `get_mm_cmdline`: `[arg_start, arg_end)`; when the program overwrote the last argument's NUL
/// (setproctitle), `get_mm_proctitle`: at most one page from `arg_start`, up to and including the first NUL.
pub(super) fn cmdline(g: &Group, p: &Process) -> Result<Vec<u8>, Errno> {
    if g.is_finished() {
        return Ok(Vec::new());
    }
    let i = p.image_info.lock().clone();
    if i.arg_start >= i.arg_end {
        return Ok(Vec::new());
    }
    let last = read_mem(p, i.arg_end - 1, i.arg_end, false)?;
    if last.first().is_some_and(|&c| c != 0) {
        return read_mem(p, i.arg_start, i.arg_start.saturating_add(4096), true);
    }
    read_mem(p, i.arg_start, i.arg_end, false)
}

/// `environ_read`: `[env_start, env_end)` (nothing once the memory is gone).
pub(super) fn environ(g: &Group, p: &Process) -> Result<Vec<u8>, Errno> {
    if g.is_finished() {
        return Ok(Vec::new());
    }
    let i = p.image_info.lock().clone();
    read_mem(p, i.env_start, i.env_end, false)
}

/// `proc_pid_auxv`: the saved auxiliary vector up to and including AT_NULL, as 64-bit words.
pub(super) fn auxv(g: &Group, p: &Process) -> Vec<u8> {
    if g.is_finished() {
        return Vec::new();
    }
    let a = p.saved_auxv.lock().clone();
    let mut v = Vec::new();
    for pair in a.chunks(2) {
        for w in pair {
            v.extend_from_slice(&w.to_le_bytes());
        }
        if pair.first() == Some(&0) {
            break;
        }
    }
    v
}

/// `lnames`: name and unit per resource.
const LNAMES: [(&str, &str); RLIM_NLIMITS as usize] = [
    ("Max cpu time", "seconds"),
    ("Max file size", "bytes"),
    ("Max data size", "bytes"),
    ("Max stack size", "bytes"),
    ("Max core file size", "bytes"),
    ("Max resident set", "bytes"),
    ("Max processes", "processes"),
    ("Max open files", "files"),
    ("Max locked memory", "bytes"),
    ("Max address space", "bytes"),
    ("Max file locks", "locks"),
    ("Max pending signals", "signals"),
    ("Max msgqueue size", "bytes"),
    ("Max nice priority", ""),
    ("Max realtime priority", ""),
    ("Max realtime timeout", "us"),
];

/// `proc_pid_limits` over (soft, hard) pairs in resource order.
pub(super) fn format_limits(l: &[(u64, u64)]) -> String {
    let mut s = String::from("Limit                     Soft Limit           Hard Limit           Units     \n");
    for (i, (name, unit)) in LNAMES.iter().enumerate() {
        let (cur, max) = l.get(i).copied().unwrap_or((RLIM_INFINITY, RLIM_INFINITY));
        let _ = write!(s, "{name:<25} ");
        for v in [cur, max] {
            if v == RLIM_INFINITY {
                let _ = write!(s, "{:<20} ", "unlimited");
            } else {
                let _ = write!(s, "{v:<20} ");
            }
        }
        if unit.is_empty() {
            s.push('\n');
        } else {
            let _ = writeln!(s, "{unit:<10}");
        }
    }
    s
}

pub(super) fn render_limits(g: &Group) -> String {
    let l: Vec<(u64, u64)> = (0..RLIM_NLIMITS).map(|r| g.limits.get(r).map_or((RLIM_INFINITY, RLIM_INFINITY), |x| (x.cur, x.max))).collect();
    format_limits(&l)
}

/// `proc_oom_score`: `oom_badness` (resident pages plus the adjustment scaled to the memory) scaled back to
/// 0..2000 and then by 2/3; init and OOM_SCORE_ADJ_MIN get 0 (`LONG_MIN`).
pub(super) fn oom_score(g: &Group, p: &Process) -> u64 {
    let total = mm_core::frames().map_or(0, |f| f.stats().total_bytes / 4096).max(1) as i64;
    let adj = g.oom_score_adj();
    if g.tgid == 1 || adj == -1000 || g.is_finished() {
        return 0;
    }
    // rss + page tables (linux-6.6/mm/oom_kill.c:230; V2 has no swap); device pages are not rss.
    badness_points((p.mm.rss_pages() + p.mm.mem_stats().page_tables) as i64, adj, total)
}

pub(super) fn badness_points(rss: i64, adj: i32, total: i64) -> u64 {
    let badness = rss + adj as i64 * (total / 1000);
    ((1000 + badness * 1000 / total) * 2 / 3).max(0) as u64
}

/// `kstrtoint(strstrip(buf), 0, ..)`: optional sign, 0x hex, leading-0 octal, else decimal; EINVAL for
/// anything else, ERANGE past i32.
pub(super) fn parse_int(b: &[u8]) -> Result<i32, Errno> {
    // The write lands in a zeroed buffer read as a C string: a NUL ends it.
    let b = b.iter().position(|&c| c == 0).map_or(b, |n| &b[..n]);
    let s = b.trim_ascii();
    let (neg, s) = match s.first() {
        Some(b'-') => (true, &s[1..]),
        Some(b'+') => (false, &s[1..]),
        _ => (false, s),
    };
    let (radix, digits) = if (s.starts_with(b"0x") || s.starts_with(b"0X")) && s.len() > 2 {
        (16, &s[2..])
    } else if s.len() > 1 && s[0] == b'0' {
        (8, &s[1..])
    } else {
        (10, s)
    };
    if digits.is_empty() {
        return Err(EINVAL);
    }
    let mut v: i64 = 0;
    for &c in digits {
        let d = (c as char).to_digit(radix).ok_or(EINVAL)? as i64;
        v = v.checked_mul(radix as i64).and_then(|v| v.checked_add(d)).ok_or(ERANGE)?;
        if v > i32::MAX as i64 + 1 {
            return Err(ERANGE);
        }
    }
    let v = if neg { -v } else { v };
    i32::try_from(v).map_err(|_| ERANGE)
}

/// `oom_score_adj_write`: at most 12 bytes are looked at (PROC_NUMBUF - 1), the whole count is answered.
pub(super) fn write_oom_score_adj(g: &Group, data: &[u8], privileged: bool) -> Result<usize, Errno> {
    let v = parse_int(&data[..data.len().min(12)])?;
    if !(-1000..=1000).contains(&v) {
        return Err(EINVAL);
    }
    g.set_oom_score_adj(v, privileged)?;
    Ok(data.len().min(12))
}

/// `comm_write`: the first 15 bytes up to a NUL become the thread's name (a newline is kept, as Linux's);
/// only a thread of the same group may (EINVAL otherwise); a thread that ended answers ESRCH first, as
/// `get_proc_task` does. The whole count is answered.
pub(super) fn write_comm(g: &Group, th: Option<&Thread>, data: &[u8]) -> Result<usize, Errno> {
    let Some(t) = th else {
        klog::event!(Debug, "proc", "comm.write_gone", tgid = g.tgid);
        return Err(ESRCH);
    };
    let same = proc::current().is_some_and(|me| core::ptr::eq(&**me.group(), g));
    if !same {
        return Err(EINVAL);
    }
    let n = data.len().min(15);
    t.prctl().set_comm(proc::prctl_state::comm_of(&data[..n]));
    klog::event!(Info, "proc", "comm.write", tid = t.tid(), name = t.prctl().comm_str().as_str());
    Ok(data.len())
}

/// CAP_SYS_RESOURCE of the writer (`capable()`): `proc::cred::capable` within the thread's bounding set (as
/// `status` shows it); a kernel writer (MCP, ktests) has every capability.
pub(super) fn writer_has_sys_resource() -> bool {
    use proc::cred::Cap;
    match (proc::current(), proc::current_thread()) {
        (Some(p), Some(t)) => p.capable(Cap::SysResource) && t.prctl().cap_bset() & (1u64 << Cap::SysResource as u32) != 0,
        _ => true,
    }
}
