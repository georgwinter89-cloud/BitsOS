//! Ids, keys and permissions (linux-7.3.1/ipc/util.c, util.h:29 IPCMNI_SHIFT). A table is two sorted growing vectors
//! instead of Linux's idr and hash table: found under a read lock, changed under the write lock.

use super::Caller;
use crate::cred::{self, Cap, Cred};
use crate::ns::misc::IpcNs;
use crate::ns::user::MapKind;
use alloc::sync::Arc;
use alloc::vec::Vec;
use ksync::{ReadGuard, RwSpinLock, WriteGuard};
use uapi::ab::sysv::*;
use uapi::errno::*;
use vfs::Errno;

/// util.h:29: an id is `seq << 15 | index`.
const SEQ_SHIFT: u32 = 15;
const IDX_MASK: i32 = (1 << SEQ_SHIFT) - 1;
/// `IPCMNI`.
pub const IPC_MNI: i32 = 1 << SEQ_SHIFT;
/// `ipc_min_cycle` = RADIX_TREE_MAP_SIZE: indices cycle through at least this many before one comes back.
const MIN_CYCLE: i32 = 64;
/// `ipcid_seq_max` (util.h:129).
const SEQ_MAX: i32 = i32::MAX >> SEQ_SHIFT;
const S_IRWXUGO: u32 = 0o777;

/// `kern_ipc_perm`; uid and gid fields are kernel ids.
#[derive(Clone)]
pub struct Perm {
    pub key: i32,
    pub id: i32,
    pub seq: u32,
    pub uid: u32,
    pub gid: u32,
    pub cuid: u32,
    pub cgid: u32,
    pub mode: u32,
    pub deleted: bool,
}

impl Perm {
    /// A new object's permissions; `ipc_addid` fills the id. `cuid = uid = euid`, `cgid = gid = egid`.
    pub(super) fn new(key: i32, flg: i32, c: &Cred) -> Perm {
        Perm { key, id: -1, seq: 0, uid: c.euid, gid: c.egid, cuid: c.euid, cgid: c.egid, mode: flg as u32 & S_IRWXUGO, deleted: false }
    }
    /// `ipcperms` (util.c:555): false means EACCES. `flag` holds wanted mode bits as a `short`.
    pub(super) fn permits(&self, ns: &IpcNs, c: &Cred, flag: i32) -> bool {
        let flag = flag as i16 as i32;
        let wanted = (flag >> 6) | (flag >> 3) | flag;
        let mut granted = self.mode as i32;
        if c.euid == self.cuid || c.euid == self.uid {
            granted >>= 6;
        } else if c.in_group(self.cgid) || c.in_group(self.gid) {
            granted >>= 3;
        }
        wanted & !granted & 0o7 == 0 || cred::ns_capable(c, &ns.user_ns, Cap::IpcOwner)
    }
    /// `ipcctl_obtain_check`: the owner, the creator or CAP_SYS_ADMIN over the namespace's user namespace (EPERM).
    pub(super) fn owner_or_admin(&self, ns: &IpcNs, c: &Cred) -> Result<(), Errno> {
        if c.euid == self.cuid || c.euid == self.uid || cred::ns_capable(c, &ns.user_ns, Cap::SysAdmin) {
            Ok(())
        } else {
            Err(EPERM)
        }
    }
    /// `ipc_update_perm` (util.c:680): `want` holds ids in the caller's user namespace (unmapped: EINVAL).
    pub(super) fn update(&mut self, want: &UserPerm, c: &Cred) -> Result<(), Errno> {
        let uid = c.user_ns.to_kernel(MapKind::Uid, want.uid).ok_or(EINVAL)?;
        let gid = c.user_ns.to_kernel(MapKind::Gid, want.gid).ok_or(EINVAL)?;
        self.uid = uid;
        self.gid = gid;
        self.mode = (self.mode & !S_IRWXUGO) | (want.mode & S_IRWXUGO);
        Ok(())
    }
    /// `kernel_to_ipc64_perm`: `struct ipc64_perm` as x86-64 lays it out, ids in the caller's user namespace.
    pub fn to_user(&self, c: &Cred) -> [u8; PERM64_SIZE] {
        let mut b = [0u8; PERM64_SIZE];
        let ns = &c.user_ns;
        let fields = [self.key as u32, ns.uid(self.uid), ns.gid(self.gid), ns.uid(self.cuid), ns.gid(self.cgid), self.mode];
        for (i, v) in fields.iter().enumerate() {
            b[i * 4..i * 4 + 4].copy_from_slice(&v.to_le_bytes());
        }
        b[24..26].copy_from_slice(&(self.seq as u16).to_le_bytes());
        b
    }
}

/// The fields of IPC_SET's `ipc64_perm`.
pub struct UserPerm {
    pub uid: u32,
    pub gid: u32,
    pub mode: u32,
}

impl UserPerm {
    pub fn from_user(b: &[u8; PERM64_SIZE]) -> UserPerm {
        let f = |i: usize| u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]);
        UserPerm { uid: f(4), gid: f(8), mode: f(20) }
    }
}

/// An object in a table.
pub trait Object: Send + Sync {
    fn with_perm<R>(&self, f: impl FnOnce(&mut Perm) -> R) -> R;
}

/// `ipc_ids`.
pub struct Ids<T> {
    /// By index, with the object's sequence number (read without its lock, as Linux's `ipc_checkid`).
    slots: Vec<Option<(u32, Arc<T>)>>,
    /// (key, index) sorted, without IPC_PRIVATE.
    keys: Vec<(i32, u32)>,
    in_use: i32,
    seq: i32,
    last_idx: i32,
    /// `idr_next` of `idr_alloc_cyclic`.
    next: u32,
    /// `ids->next_id`, the checkpoint sysctl (`sem_next_id` and friends); -1: unset.
    pub next_id: i32,
    /// `used_sems` or `shm_tot`.
    pub used: u64,
}

impl<T> Ids<T> {
    pub(super) const fn new() -> Ids<T> {
        Ids { slots: Vec::new(), keys: Vec::new(), in_use: 0, seq: 0, last_idx: -1, next: 0, next_id: -1, used: 0 }
    }
    pub fn in_use(&self) -> i32 {
        self.in_use
    }
    /// `ipc_get_maxidx`: -1 when empty. The slots end at the last object.
    pub fn max_idx(&self) -> i32 {
        self.slots.len() as i32 - 1
    }
    /// The objects in index order (`/proc/sysvipc`).
    pub fn iter(&self) -> impl Iterator<Item = &Arc<T>> {
        self.slots.iter().filter_map(|s| s.as_ref().map(|(_, o)| o))
    }
    /// `ipc_obtain_object_idr`: by index alone (EINVAL when free).
    pub fn obtain(&self, id: i32) -> Result<Arc<T>, Errno> {
        match self.slots.get((id & IDX_MASK) as usize) {
            Some(Some((_, o))) => Ok(o.clone()),
            _ => Err(EINVAL),
        }
    }
    /// `ipc_obtain_object_check`: the sequence number must fit too (a stale id is EINVAL).
    pub fn obtain_check(&self, id: i32) -> Result<Arc<T>, Errno> {
        match self.slots.get((id & IDX_MASK) as usize) {
            Some(Some((seq, o))) if *seq as i32 == id >> SEQ_SHIFT => Ok(o.clone()),
            _ => Err(EINVAL),
        }
    }
    #[cfg(test)]
    pub(super) fn find_key_for_test(&self, key: i32) -> Option<Arc<T>> {
        self.find_key(key)
    }
    fn find_key(&self, key: i32) -> Option<Arc<T>> {
        let i = self.keys.binary_search_by_key(&key, |e| e.0).ok()?;
        self.obtain(self.keys[i].1 as i32).ok()
    }
    /// First free index in `from..end`.
    fn free_from(&self, from: u32, end: u32) -> Option<u32> {
        (from..end).find(|&i| !matches!(self.slots.get(i as usize), Some(Some(_))))
    }
    /// `ipc_idr_alloc`: the index and sequence number of a new object.
    fn alloc_idx(&mut self) -> Result<(u32, u32), Errno> {
        let want = core::mem::replace(&mut self.next_id, -1);
        if want >= 0 {
            let idx = self.free_from((want & IDX_MASK) as u32, IPC_MNI as u32).ok_or(ENOSPC)?;
            return Ok((idx, (want >> SEQ_SHIFT) as u32));
        }
        // `idr_alloc_cyclic(.., 0, max_idx)`: from the cursor, then once from the start.
        let end = (self.in_use.saturating_mul(3) / 2).max(MIN_CYCLE).min(IPC_MNI) as u32;
        let idx = self.free_from(self.next, end).or_else(|| if self.next > 0 { self.free_from(0, end) } else { None }).ok_or(ENOSPC)?;
        self.next = idx + 1;
        if idx as i32 <= self.last_idx {
            self.seq += 1;
            if self.seq >= SEQ_MAX {
                self.seq = 0;
            }
        }
        self.last_idx = idx as i32;
        Ok((idx, self.seq as u32))
    }
    /// `ipc_rmid`: the object leaves; its id and key can come back.
    pub(super) fn remove(&mut self, id: i32, key: i32) {
        let idx = (id & IDX_MASK) as usize;
        if let Some(s) = self.slots.get_mut(idx) {
            *s = None;
        }
        self.drop_key(key);
        self.in_use -= 1;
        while matches!(self.slots.last(), Some(None)) {
            self.slots.pop();
        }
    }
    /// `ipc_set_key_private` (and the key's part of `ipc_rmid`).
    pub(super) fn drop_key(&mut self, key: i32) {
        if key != IPC_PRIVATE
            && let Ok(i) = self.keys.binary_search_by_key(&key, |e| e.0)
        {
            self.keys.remove(i);
        }
    }
}

impl<T: Object> Ids<T> {
    /// `ipc_addid` (util.c:262): ENOSPC past `limit`, ENOMEM without room; the object gets its id.
    pub(super) fn add(&mut self, o: &Arc<T>, limit: i32) -> Result<i32, Errno> {
        if self.in_use >= limit.min(IPC_MNI) {
            return Err(ENOSPC);
        }
        self.keys.try_reserve(1).map_err(super::nomem)?;
        let (idx, seq) = self.alloc_idx()?;
        let i = idx as usize;
        if i >= self.slots.len() {
            self.slots.try_reserve(i + 1 - self.slots.len()).map_err(super::nomem)?;
            self.slots.resize_with(i + 1, || None);
        }
        let id = ((seq << SEQ_SHIFT) + idx) as i32;
        let key = o.with_perm(|p| {
            p.id = id;
            p.seq = seq;
            p.key
        });
        self.slots[i] = Some((seq, o.clone()));
        if key != IPC_PRIVATE
            && let Err(at) = self.keys.binary_search_by_key(&key, |e| e.0)
        {
            self.keys.insert(at, (key, idx));
        }
        self.in_use += 1;
        Ok(id)
    }
}

/// A table with its lock.
pub struct Table<T>(RwSpinLock<Ids<T>>);

impl<T> Table<T> {
    pub(super) const fn named(name: &'static str) -> Table<T> {
        Table(RwSpinLock::named(name, Ids::new()))
    }
    pub fn read(&self) -> ReadGuard<'_, Ids<T>> {
        self.0.read()
    }
    pub fn write(&self) -> WriteGuard<'_, Ids<T>> {
        self.0.write()
    }
}

/// What `ipcget` needs to know about one kind of object.
pub(super) struct GetOps<'x, T> {
    /// `ops->more_checks` on an object that already has the key.
    pub more_checks: &'x dyn Fn(&T) -> Result<(), Errno>,
    /// The checks of `ops->getnew` before it allocates, repeated under the lock.
    pub pre: &'x dyn Fn(&Ids<T>) -> Result<(), Errno>,
    /// The new object, made with no lock held (`getnew`'s allocations).
    pub build: &'x dyn Fn() -> Result<Arc<T>, Errno>,
    /// The count `getnew` adds once the id is taken (`Ids::used`).
    pub used: u64,
    pub limit: i32,
}

/// `ipcget` (util.c:663): IPC_PRIVATE always makes one; another key finds its object (EEXIST with IPC_CREAT and
/// IPC_EXCL, then the kind's checks, then EACCES) or makes it with IPC_CREAT (else ENOENT).
pub(super) fn ipcget<T: Object>(t: &Table<T>, c: &Caller<'_>, key: i32, flg: i32, ops: &GetOps<'_, T>) -> Result<i32, Errno> {
    let mut built: Option<Arc<T>> = None;
    loop {
        let mut g = t.write();
        if key != IPC_PRIVATE {
            match g.find_key(key) {
                Some(o) => {
                    if flg & IPC_CREAT != 0 && flg & IPC_EXCL != 0 {
                        return Err(EEXIST);
                    }
                    (ops.more_checks)(&o)?;
                    return o.with_perm(|p| if p.permits(&c.ns, &c.cred, flg) { Ok(p.id) } else { Err(EACCES) });
                }
                None if flg & IPC_CREAT == 0 => return Err(ENOENT),
                None => {}
            }
        }
        (ops.pre)(&g)?;
        let Some(o) = built.as_ref() else {
            drop(g);
            built = Some((ops.build)()?);
            continue;
        };
        // On every way out `built` goes after `g`: a refused object's file must not end under the lock.
        let id = g.add(o, ops.limit)?;
        g.used += ops.used;
        return Ok(id);
    }
}
