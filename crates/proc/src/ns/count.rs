//! How many namespaces of each kind a user owns (linux-6.6/kernel/ucount.c `inc_ucount`, the
//! `user.max_*_namespaces` limits): every new namespace is charged to a user, and a charge past the limit is
//! ENOSPC (`create_user_ns`, `create_pid_namespace`, `clone_uts_ns`, `create_ipc_ns`, `copy_net_ns`).
//!
//! Linux counts at every user-namespace level up to the initial one (`inc_ucount` walks `ns->ucounts`): a new
//! namespace made by `euid` in user namespace `ns` counts for (ns, euid) against `ns`'s limit, then for
//! (ns's parent, ns's owner) against the parent's, and so on up to the initial level. Each user namespace has its
//! own limits (`ucount_max`, /proc/sys/user/max_*_namespaces as the reader's namespace sees them): the initial
//! one follows the RAM like Linux's default `max_threads / 2` (kernel/fork.c:1070; R1: no fixed count), a new one
//! starts at INT_MAX (`create_user_ns`). SX-3b: bwrap's --disable-userns writes 1 into its first namespace and
//! makes a second one inside, which then cannot make a third (SX-2 kept only the initial level's count).
//!
//! A namespace holds its `Charge`; dropping it gives every level back without a lock (namespaces are dropped
//! wherever a credential set or a process image goes). Slots no charge uses are pruned at the next charge.

use super::user::UserNs;
use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use alloc::sync::Arc;
use core::sync::atomic::{AtomicU64, Ordering};
use ksync::SpinLock;
use uapi::errno::{EINVAL, ENOSPC};
use vfs::Errno;

/// The kinds Linux counts (`UCOUNT_*_NAMESPACES`) that V2 builds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Counted {
    User = 0,
    Pid = 1,
    Uts = 2,
    Ipc = 3,
    Net = 4,
    /// SX-3a: `max_mnt_namespaces`.
    Mnt = 5,
}

impl Counted {
    pub fn name(self) -> &'static str {
        match self {
            Counted::User => "user",
            Counted::Pid => "pid",
            Counted::Uts => "uts",
            Counted::Ipc => "ipc",
            Counted::Net => "net",
            Counted::Mnt => "mnt",
        }
    }
}

const KINDS: usize = 6;

/// One (user namespace, kernel uid)'s counts, one per kind.
struct Slot([AtomicU64; KINDS]);

/// `ucounts_hashtable`: (user namespace inode number, kernel uid) -> counts (a spin leaf; nothing is taken under
/// it but the heap).
static SLOTS: SpinLock<BTreeMap<(u32, u32), Arc<Slot>>> = SpinLock::named("proc.nscount", BTreeMap::new());

/// Linux's `INT_MAX`: the largest limit /proc/sys/user takes and a new user namespace's default.
pub const INT_MAX: u64 = i32::MAX as u64;

/// Stored in the initial namespace's limits until root writes one: "the RAM-based default" (`limit()`).
const DEFAULT: u64 = u64::MAX;

/// A user namespace's `ucount_max` for the counted kinds.
pub struct Limits([AtomicU64; KINDS]);

impl Limits {
    /// The initial namespace's: the RAM-based default until written.
    pub(crate) const fn initial() -> Limits {
        Limits([const { AtomicU64::new(DEFAULT) }; KINDS])
    }
    /// A new namespace's: INT_MAX each (`create_user_ns`).
    pub(crate) const fn child() -> Limits {
        Limits([const { AtomicU64::new(INT_MAX) }; KINDS])
    }
    pub fn get(&self, kind: Counted) -> u64 {
        match self.0[kind as usize].load(Ordering::Relaxed) {
            DEFAULT => limit(),
            v => v,
        }
    }
    /// `proc_dointvec_minmax` with 0..=INT_MAX (EINVAL outside).
    pub fn set(&self, kind: Counted, v: u64) -> Result<(), Errno> {
        if v > INT_MAX {
            return Err(EINVAL);
        }
        self.0[kind as usize].store(v, Ordering::Relaxed);
        Ok(())
    }
}

/// One namespace's charge on every level; dropping it gives them back.
pub struct Charge {
    slots: Vec<Arc<Slot>>,
    kind: Counted,
}

impl Drop for Charge {
    fn drop(&mut self) {
        for s in &self.slots {
            s.0[self.kind as usize].fetch_sub(1, Ordering::Relaxed);
        }
    }
}

/// Linux's default `max_*_namespaces` of the initial namespace: `max_threads / 2` for this machine's RAM
/// (kernel/fork.c:1070).
pub fn limit() -> u64 {
    match mm_core::frames() {
        Some(f) => crate::rlimit::nproc_default(f.stats().total_bytes / 4096),
        // Host tests: no frame allocator, no RAM to scale with.
        None => u64::MAX,
    }
}

fn slot(key: (u32, u32)) -> Arc<Slot> {
    let fresh = Arc::new(Slot([const { AtomicU64::new(0) }; KINDS]));
    let mut m = SLOTS.lock();
    // Slots only the table holds count nothing any more (a charge holds the slot while it counts).
    m.retain(|_, s| Arc::strong_count(s) > 1);
    m.entry(key).or_insert(fresh).clone()
}

/// `atomic_long_inc_below`.
fn inc_below(s: &Slot, kind: Counted, max: u64) -> bool {
    s.0[kind as usize].try_update(Ordering::AcqRel, Ordering::Acquire, |n| (n < max).then_some(n + 1)).is_ok()
}

/// Charges one namespace of `kind` made by `euid` in user namespace `ns` on every level up to the initial one
/// (ENOSPC past a level's limit; the levels already charged are given back).
pub fn charge(kind: Counted, ns: &UserNs, euid: u32) -> Result<Charge, Errno> {
    let mut c = Charge { slots: Vec::new(), kind };
    let (mut n, mut uid) = (ns, euid);
    loop {
        let s = slot((n.inum, uid));
        let max = n.limits.get(kind);
        if c.slots.try_reserve(1).is_err() {
            return Err(ENOSPC);
        }
        if !inc_below(&s, kind, max) {
            klog::event!(Info, "ns", "limit", kind = kind.name(), uid = uid, max = max, level = n.level);
            return Err(ENOSPC);
        }
        c.slots.push(s);
        match &n.parent {
            Some(p) => {
                uid = n.owner;
                n = p;
            }
            None => return Ok(c),
        }
    }
}

/// How many namespaces of `kind` the initial level counts for kernel uid `uid` now (ktests).
pub fn used(kind: Counted, uid: u32) -> u64 {
    used_in(super::INIT_USER_INUM, kind, uid)
}

/// How many namespaces of `kind` user namespace `inum` counts for kernel uid `uid` (host tests).
pub fn used_in(inum: u32, kind: Counted, uid: u32) -> u64 {
    SLOTS.lock().get(&(inum, uid)).map_or(0, |s| s.0[kind as usize].load(Ordering::Relaxed))
}
