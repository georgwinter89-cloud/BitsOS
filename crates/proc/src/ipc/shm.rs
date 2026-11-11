//! Shared memory segments (linux-7.3.1/ipc/shm.c). The pages are a tmpfs file on the memfd instance (`vfs::memfd`,
//! Linux's `shm_mnt`), named `SYSV<key>` and already unlinked. Attaching and detaching are in `shm_map`.

use super::Caller;
use super::ids::{GetOps, Ids, Object, Perm, Table, UserPerm, ipcget};
use crate::cred::{self, Cap};
use crate::ns::misc::IpcNs;
use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::Ordering;
use ksync::SpinLock;
use uapi::ab::rlimit::RLIMIT_MEMLOCK;
use uapi::ab::sysv::*;
use uapi::errno::*;
use vfs::{Errno, OpenFile};

const PAGE: u64 = 4096;

klog::counter!(MADE, "ipc.shm.made");
klog::counter!(DESTROYED, "ipc.shm.destroyed");

pub(super) struct SegInner {
    pub perm: Perm,
    pub segsz: u64,
    pub cpid: u32,
    pub lpid: u32,
    pub atim: i64,
    pub dtim: i64,
    pub ctim: i64,
    pub nattch: u64,
    /// None once destroyed.
    pub file: Option<Arc<OpenFile>>,
    /// `shm_creator`: `Thread::sig_key` of its maker, 0 once that thread is gone.
    creator: u64,
    /// SHM_LOCK's charge: (uid, pages).
    locked: Option<(u32, u64)>,
}

/// `struct shmid_kernel`.
pub struct Segment {
    pub(super) inner: SpinLock<SegInner>,
}

impl Object for Segment {
    fn with_perm<R>(&self, f: impl FnOnce(&mut Perm) -> R) -> R {
        f(&mut self.inner.lock().perm)
    }
}

/// The namespace's segments; `Ids::used` is `shm_tot` (pages).
pub struct ShmTable {
    pub ids: Table<Segment>,
}

impl ShmTable {
    pub(super) const fn new() -> ShmTable {
        ShmTable { ids: Table::named("ipc.ids.shm") }
    }
}

impl Drop for ShmTable {
    /// `shm_exit_ns`: a mapping holds the namespace, so nothing here is attached any more.
    fn drop(&mut self) {
        let all = collect(&self.ids.read());
        for s in all {
            let mut i = s.inner.lock();
            i.perm.deleted = true;
            let file = i.file.take();
            let charge = i.locked.take();
            drop(i);
            drop(file);
            if let Some((uid, pages)) = charge {
                locked_sub(uid, pages);
            }
        }
    }
}

/// `user_shm_lock`'s count per user; one `ucounts` entry for all namespaces, apart from mlock's (V2 keeps mlock per
/// address space).
static LOCKED: SpinLock<BTreeMap<u32, u64>> = SpinLock::named("ipc.shmlock", BTreeMap::new());

fn locked_sub(uid: u32, pages: u64) {
    let mut g = LOCKED.lock();
    if let Some(n) = g.get_mut(&uid) {
        *n = n.saturating_sub(pages);
        if *n == 0 {
            g.remove(&uid);
        }
    }
}

/// `user_shm_lock` (mm/mlock.c): within RLIMIT_MEMLOCK or with CAP_IPC_LOCK.
fn locked_add(c: &Caller<'_>, pages: u64) -> bool {
    let limit = c.group.limits.get(RLIMIT_MEMLOCK).map_or(u64::MAX, |l| l.cur);
    let limit = if limit == u64::MAX { limit } else { limit >> 12 };
    let mut g = LOCKED.lock();
    let total = g.get(&c.cred.uid).copied().unwrap_or(0).saturating_add(pages);
    if total > limit && !cred::capable(&c.cred, Cap::IpcLock) {
        return false;
    }
    g.insert(c.cred.uid, total);
    true
}

/// `newseg`'s file part. Without a huge-page pool (CachyOS's `nr_hugepages = 0`) SHM_HUGETLB ends at the size check,
/// `can_do_hugetlb_shm` (fs/hugetlbfs/inode.c:1477, EPERM) or the reservation (ENOMEM). With SHM_NORESERVE Linux would
/// hand out a segment whose first touch is SIGBUS; V2 answers ENOMEM there too (plan/notes/sx-SX-IPC.md).
fn new_file(c: &Caller<'_>, key: i32, size: u64, flg: i32) -> Result<Arc<OpenFile>, Errno> {
    if flg & SHM_HUGETLB != 0 {
        if !matches!((flg >> SHM_HUGE_SHIFT) & SHM_HUGE_MASK, 0 | 21 | 30) {
            return Err(EINVAL);
        }
        let may = cred::capable(&c.cred, Cap::IpcLock) || c.cred.in_group(0);
        return Err(if may { ENOMEM } else { EPERM });
    }
    // `shmem_kernel_file_setup`: MAX_LFS_FILESIZE.
    if size > i64::MAX as u64 {
        return Err(EINVAL);
    }
    let name = alloc::format!("SYSV{:08x}", key as u32);
    vfs::memfd::create_sysv(name.as_bytes(), size, flg & SHM_NORESERVE != 0, c.cred.fs())
}

/// shmget (`ksys_shmget`, `newseg` shm.c:703).
pub fn shmget(c: &Caller<'_>, key: i32, size: u64, flg: i32) -> Result<i32, Errno> {
    let l = c.limits();
    let pages = size.wrapping_add(PAGE - 1) >> 12;
    let ops = GetOps {
        more_checks: &|s: &Segment| if s.inner.lock().segsz < size { Err(EINVAL) } else { Ok(()) },
        pre: &|g: &Ids<Segment>| {
            if size < SHMMIN || size > l.shmmax.load(Ordering::Relaxed) {
                return Err(EINVAL);
            }
            if pages << 12 < size {
                return Err(ENOSPC);
            }
            match g.used.checked_add(pages) {
                Some(t) if t <= l.shmall.load(Ordering::Relaxed) => Ok(()),
                _ => Err(ENOSPC),
            }
        },
        build: &|| {
            let file = new_file(c, key, size, flg)?;
            let now = super::now_secs();
            let inner = SegInner {
                perm: Perm::new(key, flg, &c.cred),
                segsz: size,
                cpid: c.tgid(),
                lpid: 0,
                atim: 0,
                dtim: 0,
                ctim: now,
                nattch: 0,
                file: Some(file),
                creator: c.thread_key,
                locked: None,
            };
            Ok(Arc::new(Segment { inner: SpinLock::named("ipc.shm", inner) }))
        },
        used: pages,
        limit: l.shmmni.load(Ordering::Relaxed),
    };
    let id = ipcget(&c.ns.tables.shm.ids, c, key, flg, &ops)?;
    MADE.inc();
    Ok(id)
}

/// `shm_may_destroy`.
pub(super) fn may_destroy(i: &SegInner, ns: &IpcNs) -> bool {
    i.nattch == 0 && (ns.tables.limits.shm_rmid_forced.load(Ordering::Relaxed) != 0 || i.perm.mode & SHM_DEST != 0)
}

/// `shm_destroy`: the id goes now; the file is the caller's to drop with no lock held.
fn destroy(ids: &mut Ids<Segment>, i: &mut SegInner) -> Option<Arc<OpenFile>> {
    let pages = i.segsz.wrapping_add(PAGE - 1) >> 12;
    ids.used = ids.used.saturating_sub(pages);
    ids.remove(i.perm.id, i.perm.key);
    i.perm.deleted = true;
    if let Some((uid, n)) = i.locked.take() {
        locked_sub(uid, n);
    }
    DESTROYED.inc();
    i.file.take()
}

/// `__shm_close`'s and `do_shmat`'s end: the last attachment left a segment that may go.
pub(super) fn destroy_if_due(s: &Segment, ns: &IpcNs) {
    let mut ids = ns.tables.shm.ids.write();
    let mut i = s.inner.lock();
    if i.perm.deleted || !may_destroy(&i, ns) {
        return;
    }
    let file = destroy(&mut ids, &mut i);
    drop(i);
    drop(ids);
    drop(file);
}

/// `do_shm_rmid` under the table's lock: attached, it waits for the last detach with the key private (SHM_DEST).
fn do_rmid(ids: &mut Ids<Segment>, i: &mut SegInner) -> Option<Arc<OpenFile>> {
    if i.nattch != 0 {
        i.perm.mode |= SHM_DEST;
        ids.drop_key(i.perm.key);
        i.perm.key = IPC_PRIVATE;
        None
    } else {
        destroy(ids, i)
    }
}

/// IPC_RMID.
pub fn rmid(c: &Caller<'_>, id: i32) -> Result<(), Errno> {
    let mut ids = c.ns.tables.shm.ids.write();
    let s = ids.obtain_check(id)?;
    let mut i = s.inner.lock();
    i.perm.owner_or_admin(&c.ns, &c.cred)?;
    let file = do_rmid(&mut ids, &mut i);
    drop(i);
    drop(ids);
    drop(file);
    Ok(())
}

/// IPC_SET.
pub fn set(c: &Caller<'_>, id: i32, want: &UserPerm) -> Result<(), Errno> {
    let ids = c.ns.tables.shm.ids.write();
    let s = ids.obtain_check(id)?;
    let mut i = s.inner.lock();
    i.perm.owner_or_admin(&c.ns, &c.cred)?;
    i.perm.update(want, &c.cred)?;
    i.ctim = super::now_secs();
    Ok(())
}

/// IPC_STAT's content (`/proc/sysvipc/shm` too). `rss` in pages.
pub struct ShmStat {
    pub perm: Perm,
    pub segsz: u64,
    pub atim: i64,
    pub dtim: i64,
    pub ctim: i64,
    pub cpid: u32,
    pub lpid: u32,
    pub nattch: u64,
    pub rss: u64,
}

/// `shm_add_rss_swap`, outside the object's lock (the page cache has a sleeping lock).
fn file_pages(f: &Option<Arc<OpenFile>>) -> u64 {
    f.as_ref().and_then(|f| f.inode()).and_then(|i| i.page_cache().map(|c| c.pages())).unwrap_or(0)
}

impl Segment {
    pub(super) fn stat(&self) -> ShmStat {
        let (mut st, file) = {
            let i = self.inner.lock();
            let st = ShmStat { perm: i.perm.clone(), segsz: i.segsz, atim: i.atim, dtim: i.dtim, ctim: i.ctim, cpid: i.cpid, lpid: i.lpid, nattch: i.nattch, rss: 0 };
            (st, i.file.clone())
        };
        st.rss = file_pages(&file);
        st
    }
}

/// IPC_STAT, SHM_STAT, SHM_STAT_ANY (as `sem_ctl::stat`).
pub fn stat(c: &Caller<'_>, id: i32, cmd: i32) -> Result<(ShmStat, i32), Errno> {
    let ids = c.ns.tables.shm.ids.read();
    let s = if cmd == IPC_STAT { ids.obtain_check(id)? } else { ids.obtain(id)? };
    drop(ids);
    let st = s.stat();
    if cmd != SHM_STAT_ANY && !st.perm.permits(&c.ns, &c.cred, S_IRUGO) {
        return Err(EACCES);
    }
    if st.perm.deleted {
        return Err(EIDRM);
    }
    let r = if cmd == IPC_STAT { 0 } else { st.perm.id };
    Ok((st, r))
}

/// IPC_INFO: `shminfo64`'s five numbers (shmmax, shmmin, shmmni, shmseg, shmall) and the highest index.
pub fn ipc_info(c: &Caller<'_>) -> ([u64; 5], i32) {
    let l = c.limits();
    let mni = l.shmmni.load(Ordering::Relaxed) as u64;
    let v = [l.shmmax.load(Ordering::Relaxed), SHMMIN, mni, mni, l.shmall.load(Ordering::Relaxed)];
    (v, c.ns.tables.shm.ids.read().max_idx().max(0))
}

/// SHM_INFO: used_ids, shm_tot, shm_rss, the highest index (shm_swp and the swap counts stay 0: no swap).
pub fn shm_info(c: &Caller<'_>) -> (i32, u64, u64, i32) {
    let (used, tot, max, all) = {
        let ids = c.ns.tables.shm.ids.read();
        (ids.in_use(), ids.used, ids.max_idx().max(0), collect(&ids))
    };
    let rss = all
        .iter()
        .map(|s| {
            let file = s.inner.lock().file.clone();
            file_pages(&file)
        })
        .sum();
    (used, tot, rss, max)
}

pub(super) fn collect(ids: &Ids<Segment>) -> Vec<Arc<Segment>> {
    let mut v = Vec::new();
    if v.try_reserve_exact(ids.in_use().max(0) as usize).is_ok() {
        v.extend(ids.iter().cloned());
    }
    v
}

/// SHM_LOCK and SHM_UNLOCK (`shmctl_do_lock`, shm.c:1140): V2 does not swap tmpfs pages, so this is the mode bit and
/// the RLIMIT_MEMLOCK charge.
pub fn lock(c: &Caller<'_>, id: i32, cmd: i32) -> Result<(), Errno> {
    let s = c.ns.tables.shm.ids.read().obtain_check(id)?;
    let mut i = s.inner.lock();
    if i.perm.deleted {
        return Err(EIDRM);
    }
    if !cred::ns_capable(&c.cred, &c.ns.user_ns, Cap::IpcLock) {
        if c.cred.euid != i.perm.uid && c.cred.euid != i.perm.cuid {
            return Err(EPERM);
        }
        if cmd == SHM_LOCK && c.group.limits.get(RLIMIT_MEMLOCK).is_some_and(|l| l.cur == 0) {
            return Err(EPERM);
        }
    }
    if cmd == SHM_LOCK {
        if i.perm.mode & SHM_LOCKED == 0 {
            let pages = i.segsz.wrapping_add(PAGE - 1) >> 12;
            if !locked_add(c, pages) {
                return Err(ENOMEM);
            }
            i.perm.mode |= SHM_LOCKED;
            i.locked = Some((c.cred.uid, pages));
        }
        return Ok(());
    }
    if i.perm.mode & SHM_LOCKED != 0 {
        i.perm.mode &= !SHM_LOCKED;
        if let Some((uid, n)) = i.locked.take() {
            locked_sub(uid, n);
        }
    }
    Ok(())
}

/// `exit_shm` for one namespace: the thread `key` no longer creates its segments.
pub(super) fn creator_gone(ns: &IpcNs, key: u64) {
    let mut ids = ns.tables.shm.ids.write();
    let forced = ns.tables.limits.shm_rmid_forced.load(Ordering::Relaxed) != 0;
    let mut files = Vec::new();
    for s in collect(&ids) {
        let mut i = s.inner.lock();
        if i.creator == key {
            i.creator = 0;
            if forced && may_destroy(&i, ns) && files.try_reserve(1).is_ok() {
                files.push(destroy(&mut ids, &mut i));
            }
        }
    }
    drop(ids);
    drop(files);
}

/// `shm_destroy_orphaned`: writing 1 to `shm_rmid_forced` destroys what nobody made or holds any more.
pub fn destroy_orphaned(ns: &IpcNs) {
    let mut ids = ns.tables.shm.ids.write();
    let mut files = Vec::new();
    for s in collect(&ids) {
        let mut i = s.inner.lock();
        if i.creator == 0 && may_destroy(&i, ns) && files.try_reserve(1).is_ok() {
            files.push(destroy(&mut ids, &mut i));
        }
    }
    drop(ids);
    drop(files);
}
