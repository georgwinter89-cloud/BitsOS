//! SX-2: pid namespaces (linux-6.6/kernel/pid_namespace.c, kernel/pid.c).
//!
//! V2 keeps one global id per process and thread (the initial namespace's number: TABLE, TASKS, `/proc`).
//! A namespace below the initial one holds a second numbering: `nr -> global id` and back, one entry per
//! process and thread whose namespace is this one or one below it (Linux's `upid` of that level). Numbers
//! start at 1 (the first process is the namespace's init and reaper) and wrap to 300 as the initial
//! namespace's do (`alloc_pid`); the bound is `pid_max`, a namespace size, not a count of things (R1).
//!
//! Lifetime: a number is taken when the process or thread is made (`alloc`, under `proc.table`) and given
//! back when it leaves (`free`: a reaped process, an ended thread). A namespace whose init ended takes no
//! new members (`disable`, Linux `PIDNS_ADDING`: fork answers ENOMEM); the init's end kills every member
//! (`zap_pid_ns_processes`). The object itself goes with the last group, set or child namespace naming it.
//!
//! Approximation: Linux keeps a number while a dead leader's id is still someone's process group or session;
//! V2 gives it back with the process, so such a group id reads as 0 inside the namespace (no Chromium path
//! asks for it).

use super::user::UserNs;
use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};
use ksync::{Once, SpinLock};
use uapi::errno::*;
use vfs::Errno;

/// linux-6.6/include/linux/pid_namespace.h `MAX_PID_NS_LEVEL`: 32 nested levels at most (ENOSPC beyond; an
/// ABI value).
pub const MAX_LEVEL: u32 = 32;
/// `RESERVED_PIDS`: numbers wrap to here once the cursor passed it.
const RESERVED: u32 = 300;

static LIVE: AtomicU64 = AtomicU64::new(0);
klog::counter!(MADE, "ns.pid.made");

struct Numbers {
    /// The last number handed out (`idr_get_cursor - 1`).
    cursor: u32,
    by_nr: BTreeMap<u32, u32>,
    by_id: BTreeMap<u32, u32>,
    /// The global id of the namespace's init (its number 1); 0 before it exists.
    reaper: u32,
    /// `PIDNS_ADDING`: cleared when the init ends (or its creation failed).
    adding: bool,
}

pub struct PidNs {
    pub inum: u32,
    pub level: u32,
    pub parent: Option<Arc<PidNs>>,
    /// The user namespace it was made in (setns and `CAP_CHECKPOINT_RESTORE` ask it).
    pub user_ns: Arc<UserNs>,
    nums: SpinLock<Numbers>,
    /// Its place in the creator's `max_pid_namespaces` (none for the initial one).
    _charge: Option<super::count::Charge>,
}

impl core::fmt::Debug for PidNs {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "pid:[{}]", self.inum)
    }
}

impl Drop for PidNs {
    fn drop(&mut self) {
        LIVE.fetch_sub(1, Ordering::Relaxed);
    }
}

static INIT: Once<Arc<PidNs>> = Once::new();

/// The initial pid namespace: its numbers are the global ids.
pub fn init_pid_ns() -> &'static Arc<PidNs> {
    INIT.get_or_init(|| {
        LIVE.fetch_add(1, Ordering::Relaxed);
        Arc::new(PidNs {
            inum: super::INIT_PID_INUM,
            level: 0,
            parent: None,
            user_ns: super::user::init_user_ns().clone(),
            nums: SpinLock::named("proc.pidns", Numbers { cursor: 0, by_nr: BTreeMap::new(), by_id: BTreeMap::new(), reaper: 1, adding: true }),
            _charge: None,
        })
    })
}

/// Pid namespaces besides the initial one that exist now (the lifetime ktest).
pub fn live() -> u64 {
    LIVE.load(Ordering::Relaxed).saturating_sub(INIT.get().map_or(0, |_| 1))
}

/// `create_pid_namespace` below `parent` for user namespace `user_ns` by `euid`: EINVAL when `user_ns` is not
/// the parent's or below it, ENOSPC past 32 levels or past the creator's `max_pid_namespaces` (`count`).
pub fn create(parent: &Arc<PidNs>, user_ns: &Arc<UserNs>, euid: u32) -> Result<Arc<PidNs>, Errno> {
    if !parent.user_ns.is_ancestor_of(user_ns) {
        return Err(EINVAL);
    }
    let level = parent.level + 1;
    if level > MAX_LEVEL {
        klog::event!(Info, "ns", "pid.too_deep", level = level);
        return Err(ENOSPC);
    }
    let charge = super::count::charge(super::count::Counted::Pid, user_ns, euid)?;
    let ns = Arc::new(PidNs {
        inum: super::alloc_inum(),
        level,
        parent: Some(parent.clone()),
        user_ns: user_ns.clone(),
        nums: SpinLock::named("proc.pidns", Numbers { cursor: 0, by_nr: BTreeMap::new(), by_id: BTreeMap::new(), reaper: 0, adding: true }),
        _charge: Some(charge),
    });
    LIVE.fetch_add(1, Ordering::Relaxed);
    MADE.inc();
    klog::event!(Info, "ns", "pid.new", inum = ns.inum, level = level);
    Ok(ns)
}

/// The smallest number in `from..to` that `used` does not hold.
fn gap(used: &BTreeMap<u32, u32>, from: u32, to: u32) -> Option<u32> {
    let mut c = from;
    for &k in used.range(from..to).map(|(k, _)| k) {
        if k != c {
            break;
        }
        c += 1;
    }
    (c < to).then_some(c)
}

impl PidNs {
    pub fn is_init(&self) -> bool {
        self.level == 0
    }

    /// The number of global id `id` in this namespace (`pid_nr_ns`; None: not a member, it reads as 0).
    pub fn nr(&self, id: u32) -> Option<u32> {
        if id == 0 {
            return None;
        }
        if self.is_init() {
            return Some(id);
        }
        self.nums.lock().by_id.get(&id).copied()
    }

    /// `nr` as the global id (`find_pid_ns`).
    pub fn id(&self, nr: u32) -> Option<u32> {
        if nr == 0 {
            return None;
        }
        if self.is_init() {
            return Some(nr);
        }
        self.nums.lock().by_nr.get(&nr).copied()
    }

    /// The global id of the namespace's init (0: none yet, or gone).
    pub fn reaper(&self) -> u32 {
        self.nums.lock().reaper
    }

    /// Whether `other` is this namespace or one below it (`pidns_is_ancestor` including itself).
    pub fn is_ancestor_of(&self, other: &PidNs) -> bool {
        let mut n = other;
        while n.level > self.level {
            match &n.parent {
                Some(p) => n = p,
                None => return false,
            }
        }
        core::ptr::eq(n, self)
    }

    /// Whether the namespace still takes members (its init has not ended).
    pub fn adding(&self) -> bool {
        self.nums.lock().adding
    }

    /// `disable_pid_allocation`: no new member from now on (its init is ending).
    pub(crate) fn disable(&self) {
        self.nums.lock().adding = false;
        klog::event!(Info, "ns", "pid.closed", inum = self.inum, level = self.level);
    }

    /// The global ids of every member (this level's numbers, so also those of namespaces below); empty, with a
    /// Warn event, when the list cannot be allocated (exit.rs's `zap_ns`, AB's, still takes this form).
    pub fn members(&self) -> Vec<u32> {
        self.try_members().unwrap_or_default()
    }

    /// `members`, ENOMEM when the list cannot be allocated (a /proc listing must not show "no processes").
    pub fn try_members(&self) -> Result<Vec<u32>, Errno> {
        let n = self.nums.lock();
        let mut v = Vec::new();
        if v.try_reserve(n.by_id.len()).is_err() {
            let want = n.by_id.len();
            drop(n);
            klog::event!(Warn, "ns", "pid.members_no_memory", inum = self.inum, members = want; "no memory for the member list of a pid namespace");
            return Err(ENOMEM);
        }
        v.extend(n.by_id.keys().copied());
        Ok(v)
    }

    /// One number on this level: `want` exactly (EEXIST when taken), else the next free one after the
    /// cursor, 1 first, wrapping to 300 (EAGAIN when none is free).
    fn take(&self, id: u32, want: Option<u32>, limit: u32) -> Result<u32, Errno> {
        let mut n = self.nums.lock();
        let nr = match want {
            Some(w) => {
                if n.by_nr.contains_key(&w) {
                    return Err(EEXIST);
                }
                w
            }
            None => {
                let min = if n.cursor >= RESERVED { RESERVED } else { 1 };
                let start = if n.cursor + 1 >= limit || n.cursor + 1 < min { min } else { n.cursor + 1 };
                // The first gap at or after `start`, then from `min` (`idr_alloc_cyclic`): one walk over the
                // used numbers in order, so the work is bounded by the members, not by pid_max.
                gap(&n.by_nr, start, limit).or_else(|| gap(&n.by_nr, min, start)).ok_or(EAGAIN)?
            }
        };
        n.by_nr.insert(nr, id);
        n.by_id.insert(id, nr);
        if want.is_none() {
            n.cursor = nr;
        }
        Ok(nr)
    }

    fn untake(&self, id: u32) -> Option<u32> {
        let mut n = self.nums.lock();
        let nr = n.by_id.remove(&id)?;
        n.by_nr.remove(&nr);
        Some(nr)
    }

    /// `alloc_pid` for global id `id` (a new process when `process`, else a thread) whose namespace is this
    /// one: a number on every level from here up to level 1, the innermost `want` when clone3 gave set_tid.
    /// ENOMEM once the namespace takes no members, EEXIST / EAGAIN from a level. Called under `proc.table`
    /// (`insert_new`, `new_tid_in`), so a global id gets its numbers before anyone else can see it.
    pub(crate) fn alloc(self: &Arc<Self>, id: u32, want: Option<u32>, process: bool) -> Result<(), Errno> {
        if self.is_init() {
            return Ok(());
        }
        let limit = crate::pid_max();
        let mut done: Vec<&PidNs> = Vec::new();
        let mut cur: &PidNs = self;
        let mut first = true;
        while !cur.is_init() {
            let r = done.try_reserve(1).map_err(|_| ENOMEM).and_then(|_| cur.take(id, if first { want } else { None }, limit));
            match r {
                Ok(nr) => {
                    if first && nr == 1 && process {
                        cur.nums.lock().reaper = id;
                    }
                    done.push(cur);
                }
                Err(e) => {
                    for d in done {
                        d.untake(id);
                    }
                    return Err(e);
                }
            }
            first = false;
            match &cur.parent {
                Some(p) => cur = p,
                None => break,
            }
        }
        if !self.adding() {
            for d in done {
                d.untake(id);
            }
            return Err(ENOMEM);
        }
        Ok(())
    }

    /// `free_pid`: global id `id` leaves every level. The init's number going (its process was reaped, or its
    /// first start failed) closes the namespace (`PIDNS_ADDING` cleared).
    pub(crate) fn free(&self, id: u32) {
        let mut cur: &PidNs = self;
        while !cur.is_init() {
            if cur.untake(id) == Some(1) {
                let mut n = cur.nums.lock();
                n.adding = false;
                if n.reaper == id {
                    n.reaper = 0;
                }
            }
            match &cur.parent {
                Some(p) => cur = p,
                None => break,
            }
        }
    }

    /// Numbers of global id `id` from this namespace's level up to level 1, innermost first (`NSpid:` in
    /// `/proc/<pid>/status` lists them outermost first, after the global id).
    pub fn chain(&self, id: u32) -> Vec<u32> {
        let mut v = Vec::new();
        let mut cur: &PidNs = self;
        while !cur.is_init() {
            if let Some(nr) = cur.nr(id)
                && v.try_reserve(1).is_ok()
            {
                v.push(nr);
            }
            match &cur.parent {
                Some(p) => cur = p,
                None => break,
            }
        }
        v
    }
}
