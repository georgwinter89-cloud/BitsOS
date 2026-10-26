//! The inotify calls (inotify_init 253, inotify_add_watch 254, inotify_rm_watch 255, inotify_init1 294) in
//! Linux's order of checks (linux-6.6/fs/notify/inotify/inotify_user.c:680-830), and the per-user limits under
//! `/proc/sys/fs/inotify` (R1: only Linux's own configurable limits; root may change them).
//!
//! - `max_user_instances` (128) and `max_user_watches` (1% of RAM by Linux's `INOTIFY_WATCH_COST`, clamped to
//!   8192..1048576) count per uid (Linux's ucounts of the init user namespace): EMFILE / ENOSPC past them.
//! - `max_queued_events` (16384): a new instance's queue length before IN_Q_OVERFLOW.

use super::group::Group;
use super::*;
use crate::abi::{self, AT_FDCWD};
use crate::fd::OpenFile;
use crate::sys::{ret, Args, Sys};
use crate::Errno;
use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use core::sync::atomic::{AtomicU64, Ordering};
use ksync::SpinLock;
use uapi::errno::*;

pub const IN_CLOEXEC: u64 = abi::O_CLOEXEC;
pub const IN_NONBLOCK: u64 = abi::O_NONBLOCK;
/// Linux's estimate of one watch's memory (a mark and twice an inode, `INOTIFY_WATCH_COST` on x86-64 6.6).
const WATCH_COST: u64 = 1280;

static MAX_INSTANCES: AtomicU64 = AtomicU64::new(128);
/// u64::MAX = not computed yet (it needs the frame allocator's size).
static MAX_WATCHES: AtomicU64 = AtomicU64::new(u64::MAX);
static MAX_QUEUED: AtomicU64 = AtomicU64::new(16384);

/// Per uid: (instances, watches).
static USERS: SpinLock<BTreeMap<u32, (u64, u64)>> = SpinLock::named("vfs.inotify.users", BTreeMap::new());

klog::counter!(REFUSED, "vfs.inotify.limit_refused");

fn max_watches() -> u64 {
    match MAX_WATCHES.load(Ordering::Relaxed) {
        u64::MAX => {
            let ram = mm_core::frames().map_or(0, |f| f.stats().total_bytes);
            let n = (ram / 100 / WATCH_COST).clamp(8192, 1_048_576);
            let _ = MAX_WATCHES.compare_exchange(u64::MAX, n, Ordering::AcqRel, Ordering::Relaxed);
            MAX_WATCHES.load(Ordering::Relaxed)
        }
        n => n,
    }
}

/// `inc_ucount` of one counter: false (nothing taken) past `limit`.
fn take(uid: u32, watches: bool, limit: u64) -> bool {
    let mut u = USERS.lock();
    let e = u.entry(uid).or_insert((0, 0));
    let c = if watches { &mut e.1 } else { &mut e.0 };
    if *c >= limit {
        REFUSED.inc();
        return false;
    }
    *c += 1;
    true
}

fn give(uid: u32, watches: bool) {
    let mut u = USERS.lock();
    if let Some(e) = u.get_mut(&uid) {
        let c = if watches { &mut e.1 } else { &mut e.0 };
        *c = c.saturating_sub(1);
        if *e == (0, 0) {
            u.remove(&uid);
        }
    }
}

pub(crate) fn take_watch(uid: u32) -> Result<(), Errno> {
    if take(uid, true, max_watches()) { Ok(()) } else { Err(ENOSPC) }
}
pub(crate) fn give_watch(uid: u32) {
    give(uid, true)
}
pub(crate) fn give_instance(uid: u32) {
    give(uid, false)
}
/// Host tests: a group made without inotify_init still gives its instance back when dropped.
#[cfg(test)]
pub(crate) fn take_instance_for_test(uid: u32) {
    take(uid, false, u64::MAX);
}

/// A new instance of `uid` (EMFILE past max_user_instances).
pub fn new_group(uid: u32) -> Result<Arc<Group>, Errno> {
    if !take(uid, false, MAX_INSTANCES.load(Ordering::Relaxed)) {
        return Err(EMFILE);
    }
    Ok(Group::new(uid, MAX_QUEUED.load(Ordering::Relaxed) as usize))
}

/// Kernel users and ktests: an instance without an fd (its watches end when it is dropped).
pub fn new_group_for_kernel(uid: u32) -> Result<Arc<Group>, Errno> {
    new_group(uid)
}

/// `do_inotify_init`: EINVAL for other flags, EMFILE past max_user_instances, then the fd.
fn init(s: &Sys<'_>, flags: u64) -> Result<i64, Errno> {
    if flags & !(IN_CLOEXEC | IN_NONBLOCK) != 0 {
        return Err(EINVAL);
    }
    let g = new_group(s.cred.uid)?;
    let f = OpenFile::anon(g, abi::O_RDONLY | (flags & IN_NONBLOCK));
    s.fds.install_flags(f, 0, flags & IN_CLOEXEC != 0).map(|fd| fd as i64)
}

pub fn inotify_init(s: &Sys<'_>, _a: Args) -> i64 {
    ret(init(s, 0))
}

pub fn inotify_init1(s: &Sys<'_>, a: Args) -> i64 {
    ret(init(s, a[0] as u32 as u64))
}

/// The inotify instance behind fd `a` (`fdget`: EBADF; another file: EINVAL).
fn group_of(s: &Sys<'_>, fd: u64) -> Result<Arc<OpenFile>, Errno> {
    let f = s.file(fd)?;
    if f.file.as_inotify().is_none() {
        return Err(EINVAL);
    }
    Ok(f)
}

fn add_watch(s: &Sys<'_>, a: Args) -> Result<i64, Errno> {
    let mask = a[2] as u32;
    if mask & !ALL_INOTIFY_BITS != 0 || mask & ALL_INOTIFY_BITS == 0 {
        return Err(EINVAL);
    }
    let f = s.file(a[0])?;
    if mask & IN_MASK_ADD != 0 && mask & IN_MASK_CREATE != 0 {
        return Err(EINVAL);
    }
    let g = f.file.as_inotify().ok_or(EINVAL)?;
    // `inotify_find_inode`: user_path_at (LOOKUP_FOLLOW unless IN_DONT_FOLLOW, LOOKUP_DIRECTORY for IN_ONLYDIR),
    // then MAY_READ on it. B3: watching a place is reading it.
    let p = s.path(a[1], false)?;
    let base = s.base(AT_FDCWD as u32 as u64, &p)?;
    let c = s.ctx();
    let path = c.lookup(base.as_ref(), &p, mask & IN_DONT_FOLLOW == 0)?;
    if mask & IN_ONLYDIR != 0 && !path.dentry.is_dir() {
        return Err(ENOTDIR);
    }
    path.dentry.inode.meta.permission(s.cred, crate::inode::MAY_READ)?;
    c.policy(&path, None, false, false)?;
    g.update_watch(&path.dentry.inode, mask).map(|wd| wd as i64)
}

pub fn inotify_add_watch(s: &Sys<'_>, a: Args) -> i64 {
    ret(add_watch(s, a))
}

pub fn inotify_rm_watch(s: &Sys<'_>, a: Args) -> i64 {
    ret(group_of(s, a[0]).and_then(|f| f.file.as_inotify().ok_or(EINVAL)?.remove_watch(a[1] as u32 as i32)).map(|_| 0))
}

fn set_limit(cell: &AtomicU64, v: u64) -> Result<(), Errno> {
    if v > i32::MAX as u64 {
        return Err(EINVAL);
    }
    cell.store(v, Ordering::Relaxed);
    Ok(())
}

fn get_instances() -> u64 {
    MAX_INSTANCES.load(Ordering::Relaxed)
}
fn set_instances(v: u64) -> Result<(), Errno> {
    set_limit(&MAX_INSTANCES, v)
}
fn set_watches(v: u64) -> Result<(), Errno> {
    set_limit(&MAX_WATCHES, v)
}
fn get_queued() -> u64 {
    MAX_QUEUED.load(Ordering::Relaxed)
}
fn set_queued(v: u64) -> Result<(), Errno> {
    set_limit(&MAX_QUEUED, v)
}

/// `/proc/sys/fs/inotify` (Linux's `inotify_table`): the two ucount limits are `proc_doulongvec_minmax`
/// (0..INT_MAX), max_queued_events `proc_dointvec_minmax` (0..INT_MAX); 0644, root writes.
struct ProcSysInotify;

impl crate::synth::DirSource for ProcSysInotify {
    fn entries(&self) -> Result<alloc::vec::Vec<crate::synth::SynthEntry>, Errno> {
        use crate::synth::SynthEntry;
        use crate::sysctl::Knob;
        Ok(alloc::vec![
            SynthEntry::file("max_queued_events", 0o644, Arc::new(Knob { get: get_queued, set: set_queued, uint: true })).cookie(1),
            SynthEntry::file("max_user_instances", 0o644, Arc::new(Knob { get: get_instances, set: set_instances, uint: false })).cookie(2),
            SynthEntry::file("max_user_watches", 0o644, Arc::new(Knob { get: max_watches, set: set_watches, uint: false })).cookie(3),
        ])
    }
}

fn proc_sys_inotify() -> Arc<dyn crate::synth::DirSource> {
    Arc::new(ProcSysInotify)
}
crate::procdir!("/proc/sys/fs/inotify", proc_sys_inotify);

/// Every call of this module with its x86-64 number (the glue registers exactly these).
pub const CALLS: &[(usize, &str, crate::sys::SysFn)] = &[
    (253, "inotify_init", inotify_init),
    (254, "inotify_add_watch", inotify_add_watch),
    (255, "inotify_rm_watch", inotify_rm_watch),
    (294, "inotify_init1", inotify_init1),
];
