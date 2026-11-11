//! msgctl (linux-7.3.1/ipc/msg.c:312 `msgctl_down`, :379 `msgctl_info`, :415 `msgctl_stat`).

use super::Caller;
use super::ids::{Perm, UserPerm};
use crate::cred::{self, Cap};
use core::sync::atomic::Ordering;
use uapi::ab::sysv::*;
use uapi::errno::*;
use vfs::Errno;

/// IPC_STAT's content (`/proc/sysvipc/msg` too).
pub struct MsgStat {
    pub perm: Perm,
    pub stime: i64,
    pub rtime: i64,
    pub ctime: i64,
    pub cbytes: u64,
    pub qnum: u64,
    pub qbytes: u64,
    pub lspid: u32,
    pub lrpid: u32,
}

impl super::msg::MsgQueue {
    pub(super) fn stat(&self) -> MsgStat {
        let g = self.inner.lock();
        MsgStat { perm: g.perm.clone(), stime: g.stime, rtime: g.rtime, ctime: g.ctime, cbytes: g.cbytes, qnum: g.messages.len() as u64, qbytes: g.qbytes, lspid: g.lspid, lrpid: g.lrpid }
    }
}

/// IPC_STAT, MSG_STAT, MSG_STAT_ANY (as `sem_ctl::stat`).
pub fn stat(c: &Caller<'_>, id: i32, cmd: i32) -> Result<(MsgStat, i32), Errno> {
    let ids = c.ns.tables.msg.ids.read();
    let q = if cmd == IPC_STAT { ids.obtain_check(id)? } else { ids.obtain(id)? };
    drop(ids);
    let s = q.stat();
    if cmd != MSG_STAT_ANY && !s.perm.permits(&c.ns, &c.cred, S_IRUGO) {
        return Err(EACCES);
    }
    if s.perm.deleted {
        return Err(EIDRM);
    }
    let r = if cmd == IPC_STAT { 0 } else { s.perm.id };
    Ok((s, r))
}

/// IPC_INFO and MSG_INFO: `struct msginfo`'s first seven ints in order, its `msgseg`, and the highest index.
pub fn info(c: &Caller<'_>, cmd: i32) -> ([i32; 7], u16, i32) {
    let l = c.limits();
    let t = &c.ns.tables.msg;
    let (in_use, max) = {
        let ids = t.ids.read();
        (ids.in_use(), ids.max_idx().max(0))
    };
    let clamp = |v: u64| v.min(i32::MAX as u64) as i32;
    let (pool, map, tql) = if cmd == MSG_INFO {
        (in_use, clamp(t.totals.hdrs.load(Ordering::Relaxed)), clamp(t.totals.bytes.load(Ordering::Relaxed)))
    } else {
        (MSGPOOL, MSGMAP, MSGTQL)
    };
    let v = [pool, map, l.msgmax.load(Ordering::Relaxed), l.msgmnb.load(Ordering::Relaxed), l.msgmni.load(Ordering::Relaxed), MSGSSZ, tql];
    (v, MSGSEG, max)
}

/// IPC_RMID (`freeque`): EIDRM for all who sleep, the messages go.
pub fn rmid(c: &Caller<'_>, id: i32) -> Result<(), Errno> {
    let t = &c.ns.tables.msg;
    let mut ids = t.ids.write();
    let q = ids.obtain_check(id)?;
    let dead = {
        let mut g = q.inner.lock();
        g.perm.owner_or_admin(&c.ns, &c.cred)?;
        g.free(&t.totals);
        g.perm.deleted = true;
        ids.remove(g.perm.id, g.perm.key);
        core::mem::take(&mut g.messages)
    };
    drop(ids);
    drop(dead);
    Ok(())
}

/// IPC_SET: raising `msg_qbytes` past `msgmnb` needs CAP_SYS_RESOURCE (the initial namespace's). The value is an
/// `int` in `msgctl_down`, so a large one arrives negative and passes that check, as on Linux.
pub fn set(c: &Caller<'_>, id: i32, want: &UserPerm, qbytes: u64) -> Result<(), Errno> {
    let qbytes = qbytes as i32;
    let ids = c.ns.tables.msg.ids.write();
    let q = ids.obtain_check(id)?;
    let mut g = q.inner.lock();
    g.perm.owner_or_admin(&c.ns, &c.cred)?;
    if qbytes > c.limits().msgmnb.load(Ordering::Relaxed) && !cred::capable(&c.cred, Cap::SysResource) {
        return Err(EPERM);
    }
    g.perm.update(want, &c.cred)?;
    g.qbytes = qbytes as i64 as u64;
    g.ctime = super::now_secs();
    g.wake_receivers();
    g.wake_senders();
    Ok(())
}
