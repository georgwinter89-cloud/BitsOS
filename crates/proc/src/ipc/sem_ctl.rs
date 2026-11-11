//! semctl (linux-7.3.1/ipc/sem.c:1248 `semctl_stat`, :1303 `semctl_info`, :1340 `semctl_setval`, :1399 `semctl_main`,
//! :1590 `semctl_down`). The glue copies user memory through `read`/`write` closures, called with no lock held.

use super::Caller;
use super::ids::UserPerm;
use super::sem::{SemArray, SemStat};
use alloc::sync::Arc;
use alloc::vec::Vec;
use uapi::ab::sysv::*;
use uapi::errno::*;
use vfs::Errno;

fn lookup(c: &Caller<'_>, id: i32) -> Result<Arc<SemArray>, Errno> {
    c.ns.tables.sem.ids.read().obtain_check(id)
}

/// IPC_STAT (by id), SEM_STAT (by index, read permission), SEM_STAT_ANY (by index, no check); with its id.
pub fn stat(c: &Caller<'_>, id: i32, cmd: i32) -> Result<(SemStat, i32), Errno> {
    let ids = c.ns.tables.sem.ids.read();
    let a = if cmd == IPC_STAT { ids.obtain_check(id)? } else { ids.obtain(id)? };
    drop(ids);
    let s = a.stat();
    if cmd != SEM_STAT_ANY && !s.perm.permits(&c.ns, &c.cred, S_IRUGO) {
        return Err(EACCES);
    }
    if s.perm.deleted {
        return Err(EIDRM);
    }
    let r = if cmd == IPC_STAT { 0 } else { s.perm.id };
    Ok((s, r))
}

/// IPC_INFO and SEM_INFO: `struct seminfo` as ints in its order, and the highest index.
pub fn info(c: &Caller<'_>, cmd: i32) -> ([i32; 10], i32) {
    let l = c.limits();
    let ids = c.ns.tables.sem.ids.read();
    let (usz, aem) = if cmd == SEM_INFO { (ids.in_use(), ids.used as i32) } else { (SEMUSZ, SEMAEM) };
    let v = [SEMMAP, l.sem_at(SC_SEMMNI), l.sem_at(SC_SEMMNS), SEMMNU, l.sem_at(SC_SEMMSL), l.sem_at(SC_SEMOPM), SEMUME, usz, SEMVMX, aem];
    (v, ids.max_idx().max(0))
}

/// SETVAL.
pub fn setval(c: &Caller<'_>, id: i32, num: i32, val: i32) -> Result<(), Errno> {
    if !(0..=SEMVMX).contains(&val) {
        return Err(ERANGE);
    }
    let a = lookup(c, id)?;
    if num < 0 || num as usize >= a.nsems {
        return Err(EINVAL);
    }
    let mut g = a.inner.lock();
    if !g.perm.permits(&c.ns, &c.cred, S_IWUGO) {
        return Err(EACCES);
    }
    if g.perm.deleted {
        return Err(EIDRM);
    }
    let n = num as usize;
    for (_, adj) in &mut g.undo {
        adj[n] = 0;
    }
    let s = &mut g.sems[n];
    s.val = val;
    s.pid = c.tgid();
    g.ctime = super::now_secs();
    g.smart_update(None, false);
    g.settle();
    Ok(())
}

/// GETALL: the values go to `write` with no lock held.
pub fn getall(c: &Caller<'_>, id: i32, write: &mut dyn FnMut(&[u16]) -> Result<(), Errno>) -> Result<(), Errno> {
    let a = lookup(c, id)?;
    if !a.inner.lock().perm.permits(&c.ns, &c.cred, S_IRUGO) {
        return Err(EACCES);
    }
    let mut out = Vec::new();
    out.try_reserve_exact(a.nsems).map_err(super::nomem)?;
    {
        let g = a.inner.lock();
        if g.perm.deleted {
            return Err(EIDRM);
        }
        out.extend(g.sems.iter().map(|s| s.val as u16));
    }
    write(&out)
}

/// SETALL: the values come from `read` with no lock held (EFAULT, then ERANGE, then EIDRM as Linux).
pub fn setall(c: &Caller<'_>, id: i32, read: &mut dyn FnMut(&mut [u16]) -> Result<(), Errno>) -> Result<(), Errno> {
    let a = lookup(c, id)?;
    if !a.inner.lock().perm.permits(&c.ns, &c.cred, S_IWUGO) {
        return Err(EACCES);
    }
    let mut vals = Vec::new();
    vals.try_reserve_exact(a.nsems).map_err(super::nomem)?;
    vals.resize(a.nsems, 0u16);
    read(&mut vals)?;
    if vals.iter().any(|&v| v as i32 > SEMVMX) {
        return Err(ERANGE);
    }
    let mut g = a.inner.lock();
    if g.perm.deleted {
        return Err(EIDRM);
    }
    let pid = c.tgid();
    for (s, &v) in g.sems.iter_mut().zip(&vals) {
        s.val = v as i32;
        s.pid = pid;
    }
    for (_, adj) in &mut g.undo {
        adj.fill(0);
    }
    g.ctime = super::now_secs();
    g.smart_update(None, false);
    g.settle();
    Ok(())
}

/// GETVAL, GETPID, GETNCNT, GETZCNT.
pub fn get_one(c: &Caller<'_>, id: i32, num: i32, cmd: i32) -> Result<i32, Errno> {
    let a = lookup(c, id)?;
    let g = a.inner.lock();
    if !g.perm.permits(&c.ns, &c.cred, S_IRUGO) {
        return Err(EACCES);
    }
    if num < 0 || num as usize >= a.nsems {
        return Err(EINVAL);
    }
    if g.perm.deleted {
        return Err(EIDRM);
    }
    let n = num as usize;
    let s = &g.sems[n];
    Ok(match cmd {
        GETVAL => s.val,
        GETPID => c.vnr(s.pid) as i32,
        GETNCNT => g.count_waiting(n, false),
        _ => g.count_waiting(n, true),
    })
}

impl super::sem::SemInner {
    /// `count_semcnt`: the simple waiters of the semaphore, and the complex ones blocked at it (`check_qop`).
    fn count_waiting(&self, n: usize, zero: bool) -> i32 {
        use core::sync::atomic::Ordering::Relaxed;
        let s = &self.sems[n];
        let at_it = |q: &&Arc<super::sem::Queue>| {
            let b = q.sops[q.blocking.load(Relaxed)];
            b.num as usize == n && if zero { b.op == 0 } else { b.op < 0 }
        };
        let simple = if zero { s.zero.len() } else { s.alter.len() };
        let mut count = simple + self.alter.iter().filter(at_it).count();
        if zero {
            count += self.zero.iter().filter(at_it).count();
        }
        count as i32
    }
}

/// IPC_RMID (`freeary`): every waiter gets EIDRM, the values and undo entries go.
pub fn rmid(c: &Caller<'_>, id: i32) -> Result<(), Errno> {
    let t = &c.ns.tables.sem.ids;
    let mut ids = t.write();
    let a = ids.obtain_check(id)?;
    let (dead, undo) = {
        let mut g = a.inner.lock();
        g.perm.owner_or_admin(&c.ns, &c.cred)?;
        g.free_queues();
        g.perm.deleted = true;
        ids.remove(g.perm.id, g.perm.key);
        ids.used -= a.nsems as u64;
        (core::mem::take(&mut g.sems), core::mem::take(&mut g.undo))
    };
    drop(ids);
    drop((dead, undo));
    Ok(())
}

/// IPC_SET.
pub fn set(c: &Caller<'_>, id: i32, want: &UserPerm) -> Result<(), Errno> {
    let ids = c.ns.tables.sem.ids.write();
    let a = ids.obtain_check(id)?;
    let mut g = a.inner.lock();
    g.perm.owner_or_admin(&c.ns, &c.cred)?;
    g.perm.update(want, &c.cred)?;
    g.ctime = super::now_secs();
    Ok(())
}
