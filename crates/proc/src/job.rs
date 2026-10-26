//! Process groups and sessions, minimal (linux-6.6/kernel/sys.c `setpgid` :1073, `getpgid`, `setsid`,
//! `getsid`; drivers/tty/tty_jobctrl.c `tiocspgrp`, `session_of_pgrp`): what shells and init need; AB-6 adds
//! the orphaned-group rule of job control (kernel/exit.c `will_become_orphaned_pgrp` :326).
//!
//! A process the kernel starts leads its own session and group (V2 has no login: the MCP `run` tool, the
//! console and init start programs like a fresh `setsid`). Children inherit both.

use crate::{Process, TABLE, task};
use vfs::Errno;
use uapi::errno::*;

/// Who a pid names: a process, a thread of one (its group's ids), or nothing. Called with the table held.
fn ids_of(t: &[alloc::sync::Arc<Process>], id: i32) -> Option<(u32, u32, u32, bool)> {
    if id <= 0 {
        return None;
    }
    let id = id as u32;
    if let Some(p) = t.iter().find(|q| q.pid == id) {
        return Some((p.pid, p.pgid(), p.sid(), true));
    }
    task::find_thread(id).map(|th| (th.group().tgid, th.group().pgid(), th.group().sid(), false))
}

/// `setpgid(pid, pgid)` by `caller`.
pub fn setpgid(caller: &Process, pid: i32, pgid: i32) -> Result<(), Errno> {
    let pid = if pid == 0 { caller.pid as i32 } else { pid };
    let pgid = if pgid == 0 { pid } else { pgid };
    if pgid < 0 {
        return Err(EINVAL);
    }
    let t = TABLE.lock();
    let Some((_, _, _, leader)) = ids_of(&t, pid) else { return Err(ESRCH) };
    if !leader {
        return Err(EINVAL);
    }
    let Some(p) = t.iter().find(|q| q.pid == pid as u32) else { return Err(ESRCH) };
    if p.ppid() == caller.pid && p.pid != caller.pid {
        if p.sid() != caller.sid() {
            return Err(EPERM);
        }
        if p.did_exec() {
            return Err(EACCES);
        }
    } else if p.pid != caller.pid {
        return Err(ESRCH);
    }
    if p.is_session_leader() {
        return Err(EPERM);
    }
    if pgid != pid && !t.iter().any(|g| g.pgid() == pgid as u32 && g.sid() == caller.sid()) {
        return Err(EPERM);
    }
    p.set_pgid(pgid as u32);
    klog::event!(Debug, "proc", "setpgid", by = caller.pid, pid = pid, pgid = pgid);
    Ok(())
}

/// `getpgid(pid)`: 0 = the caller; a thread id answers for its group; ESRCH otherwise.
pub fn getpgid(caller: &Process, pid: i32) -> Result<u32, Errno> {
    if pid == 0 {
        return Ok(caller.pgid());
    }
    let t = TABLE.lock();
    ids_of(&t, pid).map(|(_, g, _, _)| g).ok_or(ESRCH)
}

/// `getsid(pid)`.
pub fn getsid(caller: &Process, pid: i32) -> Result<u32, Errno> {
    if pid == 0 {
        return Ok(caller.sid());
    }
    let t = TABLE.lock();
    ids_of(&t, pid).map(|(_, _, s, _)| s).ok_or(ESRCH)
}

/// `setsid()`: EPERM for a session leader and when a group with the caller's pid exists (the caller leads one,
/// or another process uses that number as its group); else a new session and group, no controlling terminal.
pub fn setsid(caller: &Process) -> Result<u32, Errno> {
    let t = TABLE.lock();
    if caller.is_session_leader() || t.iter().any(|q| q.pgid() == caller.pid) {
        return Err(EPERM);
    }
    caller.make_session();
    drop(t);
    klog::event!(Debug, "proc", "setsid", pid = caller.pid);
    Ok(caller.pid)
}

/// `session_of_pgrp`: the session of group `pgrp`, or of the process with that pid when no group has it.
pub fn pgrp_session(pgrp: u32) -> Option<u32> {
    let t = TABLE.lock();
    if let Some(g) = t.iter().find(|q| q.pgid() == pgrp) {
        return Some(g.sid());
    }
    ids_of(&t, pgrp as i32).map(|(_, _, s, _)| s)
}

/// IN-4's TIOCSPGRP, once its tty found it is the caller's controlling terminal (ENOTTY otherwise is the tty's
/// answer): EINVAL for a negative id, ESRCH when nothing has that id, EPERM when it belongs to another
/// session (`tiocspgrp`).
pub fn check_tcsetpgrp(caller: &Process, pgrp: i32) -> Result<(), Errno> {
    if pgrp < 0 {
        return Err(EINVAL);
    }
    match pgrp_session(pgrp as u32) {
        None => Err(ESRCH),
        Some(s) if s != caller.sid() => Err(EPERM),
        Some(_) => Ok(()),
    }
}

/// Whether some process is in group `pgid` (for the tty's foreground checks).
pub fn pgrp_exists(pgid: u32) -> bool {
    TABLE.lock().iter().any(|q| q.pgid() == pgid)
}

/// `will_become_orphaned_pgrp(pgrp, ignored)`: no member (other than `ignored`, ended ones and children of
/// init or the kernel) has a parent in another group of the same session. An orphaned group's terminal stop
/// signals are discarded, and it gets SIGHUP + SIGCONT when its last such link ends while it has stopped
/// members.
pub fn pgrp_orphaned(pgrp: u32, ignored: Option<u32>) -> bool {
    let t = TABLE.lock();
    for p in t.iter().filter(|p| p.pgid() == pgrp) {
        if Some(p.pid) == ignored || p.is_finished() || p.ppid() <= 1 {
            continue;
        }
        if let Some(parent) = t.iter().find(|q| q.pid == p.ppid())
            && parent.pgid() != pgrp
            && parent.sid() == p.sid()
        {
            return false;
        }
    }
    true
}

/// `has_stopped_jobs`: a live member of `pgrp` is stopped (SIGNAL_STOP_STOPPED).
pub fn pgrp_has_stopped(pgrp: u32) -> bool {
    let members: alloc::vec::Vec<alloc::sync::Arc<Process>> = TABLE.lock().iter().filter(|p| p.pgid() == pgrp && !p.is_finished()).cloned().collect();
    members.iter().any(|p| p.signals.raw(|s| s.group_stopped()))
}
