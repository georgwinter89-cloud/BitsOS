//! SX-2: user namespaces (linux-6.6/kernel/user_namespace.c): a parent, an owner (the creator's euid/egid as
//! kernel ids), the uid/gid/projid maps written once through `/proc/<pid>/*_map`, and the setgroups switch.
//! Credentials keep kernel ids (`Cred` fields); a namespace only changes how a program sees and names them
//! (`make_kuid` / `from_kuid`) and where its capabilities count (`cred::ns_capable`).

use super::idmap::{self, Extent, IdMap};
use crate::cred::{self, Cap, Cred};
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use ksync::{Once, SpinLock};
use uapi::errno::*;
use vfs::Errno;

/// `/proc/sys/kernel/overflowuid` and `overflowgid`: what an id without a mapping reads as.
pub const OVERFLOW_ID: u32 = 65534;
/// linux-6.6/kernel/user_namespace.c:91: a parent at a deeper level than this gets ENOSPC (33 levels at most;
/// an ABI value, not a count of things V2 keeps).
const MAX_PARENT_LEVEL: u32 = 32;
/// `CAP_SETFCAP` (V2 names no file capabilities otherwise).
const CAP_SETFCAP: u32 = 31;

/// Which map a `/proc/<pid>/*_map` file is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MapKind {
    Uid,
    Gid,
    Projid,
}

pub struct UserNs {
    /// The nsfs inode number (`/proc/<pid>/ns/user` shows `user:[inum]`).
    pub inum: u32,
    pub level: u32,
    pub parent: Option<Arc<UserNs>>,
    /// The creator's euid and egid (kernel ids): it holds every capability here from the parent namespace.
    pub owner: u32,
    pub group: u32,
    parent_could_setfcap: bool,
    uid: Once<IdMap>,
    gid: Once<IdMap>,
    projid: Once<IdMap>,
    /// `USERNS_SETGROUPS_ALLOWED`: inherited from the parent; "deny" clears it for good.
    setgroups_allowed: AtomicBool,
    /// Its place in the creator's `max_user_namespaces` (none for the initial one).
    _charge: Option<super::count::Charge>,
    /// SX-3b: its `ucount_max` (/proc/sys/user/max_*_namespaces as seen from inside).
    pub limits: super::count::Limits,
}

impl core::fmt::Debug for UserNs {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "user:[{}]", self.inum)
    }
}

/// `userns_state_mutex`: every map write and setgroups change (rare; a spin lock, nothing sleeps under it).
static STATE: SpinLock<()> = SpinLock::named("proc.userns", ());
static INIT: Once<Arc<UserNs>> = Once::new();

/// User namespaces besides the initial one that exist now (the lifetime ktest).
static LIVE: AtomicU64 = AtomicU64::new(0);
klog::counter!(MADE, "ns.user.made");

/// The initial user namespace (identity maps, setgroups allowed).
pub fn init_user_ns() -> &'static Arc<UserNs> {
    INIT.get_or_init(|| {
        let m = || {
            let o = Once::new();
            o.get_or_init(IdMap::identity);
            o
        };
        Arc::new(UserNs { inum: super::INIT_USER_INUM, level: 0, parent: None, owner: 0, group: 0, parent_could_setfcap: true, uid: m(), gid: m(), projid: m(), setgroups_allowed: AtomicBool::new(true), _charge: None, limits: super::count::Limits::initial() })
    })
}

impl UserNs {
    pub fn is_init(&self) -> bool {
        self.parent.is_none()
    }

    fn map(&self, k: MapKind) -> &Once<IdMap> {
        match k {
            MapKind::Uid => &self.uid,
            MapKind::Gid => &self.gid,
            MapKind::Projid => &self.projid,
        }
    }

    /// `make_kuid`/`make_kgid`: the kernel id of this namespace's `id` (None: no mapping; -1 never maps).
    pub fn to_kernel(&self, k: MapKind, id: u32) -> Option<u32> {
        if self.is_init() {
            return (id != u32::MAX).then_some(id);
        }
        self.map(k).get()?.down(id)
    }

    /// `from_kuid`/`from_kgid`: this namespace's name for kernel id `kid`.
    pub fn from_kernel(&self, k: MapKind, kid: u32) -> Option<u32> {
        if self.is_init() {
            return (kid != u32::MAX).then_some(kid);
        }
        self.map(k).get()?.up(kid)
    }

    /// `from_kuid_munged`: an id without a mapping reads as the overflow id.
    pub fn view(&self, k: MapKind, kid: u32) -> u32 {
        self.from_kernel(k, kid).unwrap_or(OVERFLOW_ID)
    }

    pub fn uid(&self, kid: u32) -> u32 {
        self.view(MapKind::Uid, kid)
    }

    pub fn gid(&self, kid: u32) -> u32 {
        self.view(MapKind::Gid, kid)
    }

    /// `in_userns(self, other)`: `other` is this namespace or one below it.
    pub fn is_ancestor_of(&self, other: &UserNs) -> bool {
        let mut n = other;
        while n.level > self.level {
            match &n.parent {
                Some(p) => n = p,
                None => return false,
            }
        }
        core::ptr::eq(n, self)
    }

    /// `userns_may_setgroups`: a gid map exists and setgroups was not denied. Lock-free: the gid map is read
    /// first, and once it exists "deny" can no longer be written (both writes run under `proc.userns`, the
    /// deny before the map), so the flag read after it is final. Callers hold the credentials lock.
    pub fn may_setgroups(&self) -> bool {
        self.gid.get().is_some() && self.setgroups_allowed.load(Ordering::Acquire)
    }

    pub fn setgroups_allowed(&self) -> bool {
        self.setgroups_allowed.load(Ordering::Acquire)
    }
}

impl Drop for UserNs {
    /// Only the count: the last reference can go wherever a credential set is dropped (no log, no lock).
    /// A chain of parents drops recursively, at most 33 levels deep.
    fn drop(&mut self) {
        if self.parent.is_some() {
            LIVE.fetch_sub(1, Ordering::Relaxed);
        }
    }
}

/// How many user namespaces besides the initial one exist (the lifetime ktest).
pub fn live() -> u64 {
    LIVE.load(Ordering::Relaxed)
}

/// `create_user_ns` for credentials `c` (their namespace is the parent), in Linux's order: ENOSPC past 33
/// levels or past a level's `max_user_namespaces` (`count`), then EPERM when
/// a chrooted creator (`current_chrooted`: user namespaces must not undo a chroot) and when the creator's euid
/// or egid has no name in the parent.
pub fn create(c: &Cred, chrooted: bool) -> Result<Arc<UserNs>, Errno> {
    let parent = &c.user_ns;
    if parent.level > MAX_PARENT_LEVEL {
        klog::event!(Info, "ns", "user.too_deep", level = parent.level);
        return Err(ENOSPC);
    }
    // `inc_user_namespaces(parent_ns, owner)`: ENOSPC past a level's limit, before the EPERM checks as in
    // Linux (SX-3b: bwrap's disabled second level answers ENOSPC, not EPERM for its unmapped ids).
    let charge = super::count::charge(super::count::Counted::User, parent, c.euid)?;
    if chrooted {
        klog::event!(Info, "ns", "user.chrooted", euid = c.euid);
        return Err(EPERM);
    }
    if parent.from_kernel(MapKind::Uid, c.euid).is_none() || parent.from_kernel(MapKind::Gid, c.egid).is_none() {
        klog::event!(Info, "ns", "user.creator_unmapped", euid = c.euid, egid = c.egid);
        return Err(EPERM);
    }
    let ns = Arc::new(UserNs {
        inum: super::alloc_inum(),
        level: parent.level + 1,
        parent: Some(parent.clone()),
        owner: c.euid,
        group: c.egid,
        parent_could_setfcap: cred::effective(c, CAP_SETFCAP),
        uid: Once::new(),
        gid: Once::new(),
        projid: Once::new(),
        setgroups_allowed: AtomicBool::new(parent.setgroups_allowed()),
        _charge: Some(charge),
        limits: super::count::Limits::child(),
    });
    LIVE.fetch_add(1, Ordering::Relaxed);
    MADE.inc();
    klog::event!(Info, "ns", "user.new", inum = ns.inum, level = ns.level, owner = ns.owner);
    Ok(ns)
}

/// `/proc/<pid>/*_map` read by a process in namespace `reader` (`uid_m_show`): the lower ids as the reader's
/// namespace names them, or its parent's when the reader reads its own namespace's map.
pub fn render_map(ns: &UserNs, k: MapKind, reader: &Arc<UserNs>) -> String {
    let lower_ns: &UserNs = match &reader.parent {
        Some(p) if core::ptr::eq(&**reader, ns) => p,
        _ => reader,
    };
    let Some(m) = ns.map(k).get() else { return String::new() };
    let mut s = String::new();
    for e in m.extents() {
        s.push_str(&idmap::show_line(e.first, lower_ns.from_kernel(k, e.lower), e.count));
    }
    s
}

/// `verify_root_map`: mapping the parent's id 0 needs CAP_SETFCAP: when the opener is in `ns`, the creator had
/// it when it made `ns`; else the opener has it over the parent.
fn verify_root_map(ns: &UserNs, ext: &[Extent], opener: &Cred) -> bool {
    if !ext.iter().any(|e| e.lower == 0) {
        return true;
    }
    if core::ptr::eq(&*opener.user_ns, ns) {
        return ns.parent_could_setfcap;
    }
    ns.parent.as_ref().is_some_and(|p| cred::ns_capable_raw(opener, p, CAP_SETFCAP))
}

/// `new_idmap_permitted`: one id mapped onto the opener's own euid (egid with setgroups denied) by the
/// namespace's owner, a projid map, or CAP_SETUID/CAP_SETGID over the parent for both the writer and the
/// opener (`ns_capable` + `file_ns_capable`).
fn permitted(ns: &UserNs, k: MapKind, ext: &[Extent], opener: &Cred, writer: &Cred) -> bool {
    if k == MapKind::Uid && !verify_root_map(ns, ext, opener) {
        return false;
    }
    let Some(parent) = &ns.parent else { return false };
    if ext.len() == 1 && ext[0].count == 1 && ns.owner == opener.euid {
        let id = ext[0].lower;
        match k {
            MapKind::Uid if parent.to_kernel(k, id) == Some(opener.euid) => return true,
            MapKind::Gid if !ns.setgroups_allowed() && parent.to_kernel(k, id) == Some(opener.egid) => return true,
            _ => {}
        }
    }
    let cap = match k {
        MapKind::Projid => return true,
        MapKind::Uid => Cap::Setuid,
        MapKind::Gid => Cap::Setgid,
    };
    cred::ns_capable(writer, parent, cap) && cred::ns_capable(opener, parent, cap)
}

/// What a `/proc/<pid>/*_map` or `setgroups` file was opened on (Linux keeps both in the open file: the
/// target's namespace in `seq->private`, the opener's credentials in `f_cred`).
#[derive(Clone)]
pub struct Opened {
    pub ns: Arc<UserNs>,
    pub opener: Arc<Cred>,
}

/// `proc_{uid,gid,projid}_map_write` + `map_write`: the namespace not the initial one and the opener in it or
/// its parent (EPERM), `pos` 0 and the text shorter than a page (EINVAL), the map not yet written (EPERM),
/// the opener with CAP_SYS_ADMIN in it for uid/gid (EPERM), the text valid (EINVAL), the ids permitted
/// (EPERM) and mapped in the parent (EPERM). `writer` is the calling process's credentials.
pub fn write_map(o: &Opened, k: MapKind, pos: u64, text: &[u8], writer: &Cred) -> Result<usize, Errno> {
    let ns: &UserNs = &o.ns;
    let opener: &Cred = &o.opener;
    let Some(parent) = &ns.parent else { return Err(EPERM) };
    if !core::ptr::eq(&*opener.user_ns, ns) && !Arc::ptr_eq(&opener.user_ns, parent) {
        return Err(EPERM);
    }
    if pos != 0 || text.len() >= idmap::MAX_WRITE {
        return Err(EINVAL);
    }
    // Parsed (and allocated) before the lock; the answers keep Linux's order below.
    let parsed = idmap::parse(text);
    let r = {
        let _l = STATE.lock();
        install(ns, parent, k, parsed, opener, writer)
    };
    match &r {
        Ok(n) => klog::event!(Info, "ns", "map.written", inum = ns.inum, which = k as u32, extents = *n),
        Err(e) => klog::event!(Info, "ns", "map.refused", inum = ns.inum, which = k as u32, euid = writer.euid, errno = *e),
    }
    r.map(|_| text.len())
}

/// The part of `write_map` under `proc.userns`: no allocation, no log. The number of extents installed.
fn install(ns: &UserNs, parent: &UserNs, k: MapKind, parsed: Result<Vec<Extent>, Errno>, opener: &Cred, writer: &Cred) -> Result<usize, Errno> {
    if ns.map(k).get().is_some() {
        return Err(EPERM);
    }
    if k != MapKind::Projid && !cred::ns_capable(opener, ns, Cap::SysAdmin) {
        return Err(EPERM);
    }
    let mut ext = parsed?;
    if !permitted(ns, k, &ext, opener, writer) {
        return Err(EPERM);
    }
    let pmap = parent.map(k).get();
    for e in ext.iter_mut() {
        let Some(l) = pmap.and_then(|m| m.down_range(e.lower, e.count)) else { return Err(EPERM) };
        e.lower = l;
    }
    let n = ext.len();
    ns.map(k).get_or_init(|| IdMap::from_extents(ext));
    Ok(n)
}

/// `proc_setgroups_open` for writing: the opener needs CAP_SYS_ADMIN in the namespace (EPERM).
pub fn may_open_setgroups(ns: &UserNs, opener: &Cred) -> Result<(), Errno> {
    if cred::ns_capable(opener, ns, Cap::SysAdmin) { Ok(()) } else { Err(EPERM) }
}

/// `proc_setgroups_write`: "allow" or "deny" (then only spaces), shorter than 8 bytes at position 0 (EINVAL);
/// allowing again after a deny, or denying once a gid map exists, is EPERM. The open checked the opener
/// (`may_open_setgroups`), as Linux does; it is checked again here for an fd opened read-only and reopened
/// through a link.
pub fn write_setgroups(o: &Opened, pos: u64, text: &[u8]) -> Result<usize, Errno> {
    let ns: &UserNs = &o.ns;
    if pos != 0 || text.len() >= 8 {
        return Err(EINVAL);
    }
    let s = &text[..text.iter().position(|&b| b == 0).unwrap_or(text.len())];
    let (allow, rest) = if s.starts_with(b"allow") {
        (true, &s[5..])
    } else if s.starts_with(b"deny") {
        (false, &s[4..])
    } else {
        return Err(EINVAL);
    };
    if !rest.iter().all(|&b| matches!(b, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r' | 0xa0)) {
        return Err(EINVAL);
    }
    may_open_setgroups(ns, &o.opener)?;
    let r = {
        let _l = STATE.lock();
        if allow {
            if ns.setgroups_allowed() { Ok(()) } else { Err(EPERM) }
        } else if ns.gid.get().is_some() {
            Err(EPERM)
        } else {
            ns.setgroups_allowed.store(false, Ordering::Release);
            Ok(())
        }
    };
    if r.is_ok() && !allow {
        klog::event!(Info, "ns", "setgroups.denied", inum = ns.inum);
    }
    r.map(|_| text.len())
}
