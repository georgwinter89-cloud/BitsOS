//! AB-13: credentials as Linux keeps them (linux-6.6/include/linux/cred.h `struct cred`, kernel/cred.c,
//! kernel/sys.c set*id, kernel/groups.c): real, effective, saved and file-system uid and gid plus the
//! supplementary groups.
//!
//! Linux keeps them per task and libc makes every thread call set*id (musl `__synccall`, glibc `SETXID`); V2
//! keeps one set per thread group (`Group::cred`), which is what the program sees after libc's broadcast. They
//! are copied at fork, kept across execve (with execve's `suid = fsuid = euid`, `sgid = fsgid = egid`,
//! security/commoncap.c `cap_bprm_creds_from_file`) and replaced as a whole (`commit_creds`), so a reader
//! holds a consistent snapshot.
//!
//! Capabilities: without explicit sets (see SX-2 below) `capable` is the one place that answers "may it?": it follows
//! security/commoncap.c `cap_emulate_setxuid` and `cap_task_fix_setuid` for a process without file
//! capabilities or securebits, where the effective set is full exactly while euid is 0, and the file-system
//! subset (CAP_FS_MASK) exactly while fsuid is 0 (fsuid can only be 0 while one of ruid/euid/suid is 0, so
//! the permitted set is full then).
//!
//! SX-2: user namespaces. Every credential set names its user namespace (`user_ns`); the ids stay kernel ids.
//! "root" above means the kernel uid that is 0 in that namespace (`make_kuid(ns, 0)`), and capabilities count
//! in that namespace and the ones below it (`ns_capable`, Linux `cap_capable`). Explicit sets (`caps`) exist
//! once something set them: a new user namespace (full sets, `set_cred_user_ns`) and capset(2); set*uid then
//! adjusts them as `cap_task_fix_setuid` does, and execve goes back to the id rule above
//! (`cap_bprm_creds_from_file` for a program without file capabilities). The inheritable set is kept across
//! execve (`cap_inh`).

use crate::Process;
use crate::group::Group;
use crate::ns::user::{MapKind, UserNs, init_user_ns};
use crate::task::Thread;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::Ordering;
use uapi::errno::*;
use vfs::Errno;

/// linux-6.6/include/uapi/linux/limits.h `NGROUPS_MAX`: the most supplementary groups (an ABI value).
pub const NGROUPS_MAX: usize = 65536;
/// `(uid_t)-1`: "leave unchanged" for the set*id calls and an invalid id everywhere else (`uid_valid`).
pub const NO_ID: u32 = u32::MAX;

/// Capability sets stored by capset(2) or a new user namespace (permitted, effective).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CapSets {
    pub prm: u64,
    pub eff: u64,
}

/// One process's credentials (`struct cred` without keys and securebits).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cred {
    pub uid: u32,
    pub euid: u32,
    pub suid: u32,
    pub fsuid: u32,
    pub gid: u32,
    pub egid: u32,
    pub sgid: u32,
    pub fsgid: u32,
    /// Supplementary groups, sorted ascending (`groups_sort`; duplicates kept, as Linux).
    pub groups: Arc<Vec<u32>>,
    /// SX-2: the user namespace the ids are named in and the capabilities count in.
    pub user_ns: Arc<UserNs>,
    /// SX-2: stored permitted/effective sets (None: they follow from the ids, see the module doc).
    pub caps: Option<CapSets>,
    /// SX-2: the inheritable set (capset), kept across execve.
    pub cap_inh: u64,
}

impl PartialEq for UserNs {
    fn eq(&self, o: &UserNs) -> bool {
        core::ptr::eq(self, o)
    }
}
impl Eq for UserNs {}

/// The capabilities V2 asks about (linux-6.6/include/uapi/linux/capability.h numbers).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum Cap {
    Chown = 0,
    DacOverride = 1,
    DacReadSearch = 2,
    Fowner = 3,
    Fsetid = 4,
    Kill = 5,
    Setgid = 6,
    Setuid = 7,
    Setpcap = 8,
    NetBindService = 10,
    NetAdmin = 12,
    NetRaw = 13,
    SysChroot = 18,
    SysPtrace = 19,
    SysAdmin = 21,
    SysNice = 23,
    SysResource = 24,
    SysTtyConfig = 26,
    Lease = 28,
    WakeAlarm = 35,
    CheckpointRestore = 40,
}

/// The kernel uid that is root in `c`'s namespace (None: 0 has no mapping there yet).
fn ns_root(c: &Cred) -> Option<u32> {
    c.user_ns.to_kernel(MapKind::Uid, 0)
}

/// Whether capability number `cap` is in `c`'s effective set (in its own namespace).
pub fn effective(c: &Cred, cap: u32) -> bool {
    if cap >= 64 {
        return false;
    }
    match c.caps {
        Some(s) => s.eff & (1u64 << cap) != 0,
        None => {
            let id = if CAP_FS_MASK & (1u64 << cap) != 0 { c.fsuid } else { c.euid };
            Some(id) == ns_root(c)
        }
    }
}

/// Whether credentials `c` hold capability `cap` in the initial user namespace (Linux `capable`).
pub fn capable(c: &Cred, cap: Cap) -> bool {
    ns_capable(c, init_user_ns(), cap)
}

/// `ns_capable(ns, cap)` for credentials `c` (Linux `cap_capable`): in `c`'s own namespace its effective set
/// decides; in a namespace below it, the creator of that namespace (or of one between) holds every capability;
/// anywhere else none.
pub fn ns_capable(c: &Cred, ns: &UserNs, cap: Cap) -> bool {
    ns_capable_raw(c, ns, cap as u32)
}

/// `ns_capable` for a capability number V2's `Cap` does not name (CAP_SETFCAP).
pub fn ns_capable_raw(c: &Cred, target: &UserNs, cap: u32) -> bool {
    let mine: &UserNs = &c.user_ns;
    let mut ns = target;
    loop {
        if core::ptr::eq(ns, mine) {
            return effective(c, cap);
        }
        if ns.level <= mine.level {
            return false;
        }
        let Some(parent) = &ns.parent else { return false };
        if core::ptr::eq(&**parent, mine) && ns.owner == c.euid {
            return true;
        }
        ns = parent;
    }
}

/// `ns_capable(cred->user_ns, cap)`: a capability in the credentials' own namespace (the set*id rules,
/// seccomp's unprivileged check, chroot).
pub fn own_capable(c: &Cred, cap: Cap) -> bool {
    effective(c, cap as u32)
}

/// `cap_task_fix_setuid` for stored sets: leaving root (all of ruid/euid/suid) clears permitted and effective,
/// leaving an effective root clears effective, becoming one raises it to permitted (LSM_SETID_ID/RE/RES); a
/// change of the fsuid alone moves the file-system subset (LSM_SETID_FS).
fn fix_setuid(old: &Cred, new: &mut Cred) {
    let Some(mut s) = new.caps else { return };
    let r = ns_root(old);
    let is = |u: u32| Some(u) == r;
    if old.uid != new.uid || old.euid != new.euid || old.suid != new.suid {
        if (is(old.uid) || is(old.euid) || is(old.suid)) && !(is(new.uid) || is(new.euid) || is(new.suid)) {
            s = CapSets { prm: 0, eff: 0 };
        }
        if is(old.euid) && !is(new.euid) {
            s.eff = 0;
        }
        if !is(old.euid) && is(new.euid) {
            s.eff = s.prm;
        }
    } else if old.fsuid != new.fsuid {
        if is(old.fsuid) && !is(new.fsuid) {
            s.eff &= !CAP_FS_MASK;
        }
        if !is(old.fsuid) && is(new.fsuid) {
            s.eff |= s.prm & CAP_FS_MASK;
        }
    }
    new.caps = Some(s);
}

/// linux-6.6/include/linux/capability.h `CAP_FS_MASK` as a bit set (CHOWN, DAC_OVERRIDE, DAC_READ_SEARCH,
/// FOWNER, FSETID, LINUX_IMMUTABLE, MKNOD, MAC_OVERRIDE).
pub const CAP_FS_MASK: u64 = 0x1f | 1 << 9 | 1 << 27 | 1 << 32;

/// The permitted and effective sets `/proc/<pid>/status` shows for credentials `c` under bounding set `bset`
/// (the model of the module doc: permitted while one of ruid/euid/suid is 0; effective as `capable`).
pub fn cap_sets(c: &Cred, bset: u64) -> (u64, u64) {
    if let Some(s) = c.caps {
        return (s.prm, s.eff);
    }
    let r = ns_root(c);
    let is = |u: u32| Some(u) == r;
    let prm = if is(c.uid) || is(c.euid) || is(c.suid) { bset } else { 0 };
    let eff = (if is(c.euid) { prm & !CAP_FS_MASK } else { 0 }) | (if is(c.fsuid) { prm & CAP_FS_MASK } else { 0 });
    (prm, eff)
}

/// What a new user namespace gives its creator (`set_cred_user_ns`): every capability in it, no inheritable
/// ones; the bounding set becomes full too (the caller sets the thread's).
pub fn in_new_user_ns(c: &Cred, ns: Arc<UserNs>) -> Cred {
    let full = crate::prctl_state::CAP_FULL_SET;
    Cred { user_ns: ns, caps: Some(CapSets { prm: full, eff: full }), cap_inh: 0, ..c.clone() }
}

/// `cap_capset` (security/commoncap.c): inheritable within the old inheritable|permitted (unless CAP_SETPCAP)
/// and within the old inheritable|bounding set, permitted within the old permitted, effective within the new
/// permitted; EPERM otherwise. The sets are `(eff, prm, inh)` as capset gives them.
pub fn capset(old: &Cred, bset: u64, want: [u64; 3]) -> Change {
    let [eff, prm, inh] = want;
    let (oprm, _) = cap_sets(old, bset);
    let capped = !own_capable(old, Cap::Setpcap);
    if (capped && inh & !(old.cap_inh | oprm) != 0) || inh & !(old.cap_inh | bset) != 0 || prm & !oprm != 0 || eff & !prm != 0 {
        return Err(EPERM);
    }
    Ok(Some(Cred { caps: Some(CapSets { prm, eff }), cap_inh: inh, ..old.clone() }))
}

impl Cred {
    /// All four uids `uid`, all four gids `gid`, no supplementary groups (a process the kernel starts).
    pub fn new(uid: u32, gid: u32) -> Cred {
        Cred { uid, euid: uid, suid: uid, fsuid: uid, gid, egid: gid, sgid: gid, fsgid: gid, groups: Arc::new(Vec::new()), user_ns: init_user_ns().clone(), caps: None, cap_inh: 0 }
    }

    /// linux-6.6/kernel/groups.c `in_group_p`: fsgid or a supplementary group (file access).
    pub fn in_group(&self, gid: u32) -> bool {
        self.fsgid == gid || self.groups.binary_search(&gid).is_ok()
    }

    /// What execve leaves (no set-id programs in V2, `prctl_state.rs`): `cap_bprm_creds_from_file` sets
    /// `suid = fsuid = euid` and `sgid = fsgid = egid`.
    /// SX-2: stored sets go; the id rule gives them again, relative to the namespace's root.
    pub fn for_exec(&self) -> Cred {
        Cred { suid: self.euid, fsuid: self.euid, sgid: self.egid, fsgid: self.egid, caps: None, ..self.clone() }
    }

    /// The file-system view (`vfs::Cred`): fsuid and fsgid.
    pub fn fs(&self) -> vfs::Cred {
        vfs::Cred { uid: self.fsuid, gid: self.fsgid }
    }

    fn has_uid(&self, id: u32) -> bool {
        id == self.uid || id == self.euid || id == self.suid
    }

    fn has_gid(&self, id: u32) -> bool {
        id == self.gid || id == self.egid || id == self.sgid
    }
}

/// The outcome of a set*id call: `Ok(None)` changes nothing.
pub type Change = Result<Option<Cred>, Errno>;

/// linux-6.6/kernel/sys.c:610 `__sys_setuid`.
pub fn set_uid(old: &Cred, uid: u32) -> Change {
    if uid == NO_ID {
        return Err(EINVAL);
    }
    let mut n = old.clone();
    if own_capable(old, Cap::Setuid) {
        n.uid = uid;
        n.suid = uid;
    } else if uid != old.uid && uid != old.suid {
        return Err(EPERM);
    }
    n.euid = uid;
    n.fsuid = uid;
    Ok(Some(n))
}

/// linux-6.6/kernel/sys.c:438 `__sys_setgid`.
pub fn set_gid(old: &Cred, gid: u32) -> Change {
    if gid == NO_ID {
        return Err(EINVAL);
    }
    let mut n = old.clone();
    if own_capable(old, Cap::Setgid) {
        n.gid = gid;
        n.sgid = gid;
    } else if gid != old.gid && gid != old.sgid {
        return Err(EPERM);
    }
    n.egid = gid;
    n.fsgid = gid;
    Ok(Some(n))
}

/// The ids of a setre*id call (`r`, `e`) applied to (`real`, `eff`, `saved`, `fs`) with Linux's rules:
/// linux-6.6/kernel/sys.c:529 `__sys_setreuid` and :372 `__sys_setregid` (same shape; the real id may become
/// the old real or effective id, the effective id also the saved one, unless privileged).
fn set_re(old: [u32; 3], r: u32, e: u32, privileged: bool) -> Result<[u32; 4], Errno> {
    let [ruid, euid, suid] = old;
    let mut n = [ruid, euid, suid, euid];
    if r != NO_ID {
        if r != ruid && r != euid && !privileged {
            return Err(EPERM);
        }
        n[0] = r;
    }
    if e != NO_ID {
        if e != ruid && e != euid && e != suid && !privileged {
            return Err(EPERM);
        }
        n[1] = e;
    }
    if r != NO_ID || (e != NO_ID && e != ruid) {
        n[2] = n[1];
    }
    n[3] = n[1];
    Ok(n)
}

/// `setreuid(ruid, euid)`.
pub fn set_reuid(old: &Cred, r: u32, e: u32) -> Change {
    let n = set_re([old.uid, old.euid, old.suid], r, e, own_capable(old, Cap::Setuid))?;
    Ok(Some(Cred { uid: n[0], euid: n[1], suid: n[2], fsuid: n[3], ..old.clone() }))
}

/// `setregid(rgid, egid)` (the real gid is checked against the old gid and egid only, as Linux).
pub fn set_regid(old: &Cred, r: u32, e: u32) -> Change {
    let n = set_re([old.gid, old.egid, old.sgid], r, e, own_capable(old, Cap::Setgid))?;
    Ok(Some(Cred { gid: n[0], egid: n[1], sgid: n[2], fsgid: n[3], ..old.clone() }))
}

/// linux-6.6/kernel/sys.c:667 `__sys_setresuid` and :769 `__sys_setresgid`: `old` = (real, eff, saved, fs).
/// Each new id must be one of the old real/effective/saved ids unless privileged; the call that changes
/// nothing (and leaves fs equal to eff) returns early. That no-op test is Linux's own (sys.c:691-695), so
/// `setresuid(-1, -1, -1)` after a `setfsuid` keeps the fsuid there too; any other call sets fs = eff.
fn set_res(old: [u32; 4], want: [u32; 3], privileged: bool) -> Result<Option<[u32; 4]>, Errno> {
    let [r, e, s] = want;
    let noop = (r == NO_ID || r == old[0]) && (e == NO_ID || (e == old[1] && e == old[3])) && (s == NO_ID || s == old[2]);
    if noop {
        return Ok(None);
    }
    let is_new = |id: u32| id != NO_ID && id != old[0] && id != old[1] && id != old[2];
    if (is_new(r) || is_new(e) || is_new(s)) && !privileged {
        return Err(EPERM);
    }
    let pick = |id: u32, keep: u32| if id == NO_ID { keep } else { id };
    let eff = pick(e, old[1]);
    Ok(Some([pick(r, old[0]), eff, pick(s, old[2]), eff]))
}

/// `setresuid(ruid, euid, suid)`.
pub fn set_resuid(old: &Cred, r: u32, e: u32, s: u32) -> Change {
    let n = set_res([old.uid, old.euid, old.suid, old.fsuid], [r, e, s], own_capable(old, Cap::Setuid))?;
    Ok(n.map(|n| Cred { uid: n[0], euid: n[1], suid: n[2], fsuid: n[3], ..old.clone() }))
}

/// `setresgid(rgid, egid, sgid)`.
pub fn set_resgid(old: &Cred, r: u32, e: u32, s: u32) -> Change {
    let n = set_res([old.gid, old.egid, old.sgid, old.fsgid], [r, e, s], own_capable(old, Cap::Setgid))?;
    Ok(n.map(|n| Cred { gid: n[0], egid: n[1], sgid: n[2], fsgid: n[3], ..old.clone() }))
}

/// linux-6.6/kernel/sys.c:863 `__sys_setfsuid`: never an error; the caller returns the previous fsuid.
pub fn set_fsuid(old: &Cred, uid: u32) -> Option<Cred> {
    let ok = uid != NO_ID && (old.has_uid(uid) || uid == old.fsuid || own_capable(old, Cap::Setuid));
    (ok && uid != old.fsuid).then(|| Cred { fsuid: uid, ..old.clone() })
}

/// linux-6.6/kernel/sys.c:907 `__sys_setfsgid`.
pub fn set_fsgid(old: &Cred, gid: u32) -> Option<Cred> {
    let ok = gid != NO_ID && (old.has_gid(gid) || gid == old.fsgid || own_capable(old, Cap::Setgid));
    (ok && gid != old.fsgid).then(|| Cred { fsgid: gid, ..old.clone() })
}

/// linux-6.6/kernel/groups.c:66 `groups_sort`, done before the credentials lock is taken: up to NGROUPS_MAX
/// ids must not be sorted while other threads spin on it (B4). The result goes to `set_groups`.
pub fn sorted_groups(mut groups: Vec<u32>) -> Arc<Vec<u32>> {
    groups.sort_unstable();
    Arc::new(groups)
}

/// linux-6.6/kernel/groups.c:198 `setgroups` after the copy: `may_setgroups` (CAP_SETGID), then the sorted
/// list (`sorted_groups`) replaces the old one; ids already checked for `NO_ID` (EINVAL in `groups_from_user`).
/// Runs under the credentials lock: only the capability check and an Arc clone happen here.
pub fn set_groups(old: &Cred, groups: Arc<Vec<u32>>) -> Change {
    // SX-2 `may_setgroups`: CAP_SETGID in the own namespace, and the namespace allows setgroups.
    if !own_capable(old, Cap::Setgid) || !old.user_ns.may_setgroups() {
        return Err(EPERM);
    }
    debug_assert!(groups.windows(2).all(|w| w[0] <= w[1]), "set_groups wants a sorted list");
    Ok(Some(Cred { groups, ..old.clone() }))
}

/// For FS's permission checks (change request in plan/notes/ab-AB-13.md): whether `gid` is a supplementary
/// group of the calling process, when `cred` (a `vfs::Cred`) is that process's file-system view. Linux
/// `in_group_p` (fsgid or a supplementary group); the fsgid itself is FS's `cred.gid == gid` check.
pub fn current_in_group(cred: vfs::Cred, gid: u32) -> bool {
    crate::current().is_some_and(|p| {
        let c = p.cred();
        c.fsuid == cred.uid && c.fsgid == cred.gid && c.groups.binary_search(&gid).is_ok()
    })
}

impl Group {
    /// The group's credentials now (a snapshot; set*id replaces the whole set).
    pub fn cred(&self) -> Arc<Cred> {
        self.cred.lock().clone()
    }

    /// Puts `c` in place without `commit_creds`' side effects (execve's own rules apply there; fork's copy).
    pub(crate) fn set_cred(&self, c: Arc<Cred>) {
        let mut l = self.cred.lock();
        self.owner_uid.store(c.uid, Ordering::Release);
        *l = c;
    }

    /// The real uid (RLIMIT_NPROC counts threads per real uid; si_uid of what the group sends).
    pub fn real_uid(&self) -> u32 {
        self.owner_uid.load(Ordering::Acquire)
    }
}

impl Process {
    /// The credentials of this process now (`current_cred()`).
    pub fn cred(&self) -> Arc<Cred> {
        self.group().cred()
    }

    /// What path walks and file checks of this process use (fsuid, fsgid; `current_fsuid()`).
    pub fn fs_cred(&self) -> vfs::Cred {
        self.cred().fs()
    }

    /// `capable(cap)` for this process (`capable` above: the one place).
    pub fn capable(&self, cap: Cap) -> bool {
        capable(&self.cred(), cap)
    }

    /// linux-6.6/kernel/ptrace.c:276 `__ptrace_may_access` without an LSM: the same thread group; else the
    /// caller's uid and gid (fsuid/fsgid with `fs_creds`, PTRACE_MODE_FSCREDS; else the real ones) equal all of
    /// the target's uids and gids, or the caller has CAP_SYS_PTRACE; and then the target is dumpable or the
    /// caller has the capability (an ended target has no memory to ask).
    pub fn may_ptrace(&self, target: &Process, fs_creds: bool) -> bool {
        if Arc::ptr_eq(self.group(), target.group()) {
            return true;
        }
        let (me, t) = (self.cred(), target.cred());
        let (u, g) = if fs_creds { (me.fsuid, me.fsgid) } else { (me.uid, me.gid) };
        let ids = [t.uid, t.euid, t.suid].iter().all(|&x| x == u) && [t.gid, t.egid, t.sgid].iter().all(|&x| x == g);
        let cap = ns_capable(&me, &t.user_ns, Cap::SysPtrace);
        (ids || cap) && (cap || target.is_finished() || target.mm_flags.dumpable() == 1)
    }

    /// Computes and commits new credentials from the current ones in one step (two threads of the group cannot
    /// interleave; Linux's per-task `prepare_creds`/`commit_creds`). `f` runs under the group's credentials lock:
    /// it must not allocate a lot or sleep. Returns the old credentials. When the effective or file-system
    /// ids change, `commit_creds` makes the memory not dumpable (`suid_dumpable` 0) and clears the calling
    /// thread's parent-death signal (linux-6.6/kernel/cred.c:409).
    pub fn change_cred(&self, t: &Thread, f: impl FnOnce(&Cred) -> Change) -> Result<Arc<Cred>, Errno> {
        let g = self.group();
        let (old, new) = {
            let mut c = g.cred.lock();
            let Some(mut new) = f(&c)? else { return Ok(c.clone()) };
            fix_setuid(&c, &mut new);
            let new = Arc::new(new);
            g.owner_uid.store(new.uid, Ordering::Release);
            (core::mem::replace(&mut *c, new.clone()), new)
        };
        if old.euid != new.euid || old.egid != new.egid || old.fsuid != new.fsuid || old.fsgid != new.fsgid {
            self.mm_flags.set_dumpable(0);
            t.prctl().set_pdeath_signal(0);
        }
        klog::event!(Info, "proc", "cred.change", pid = self.pid, uid = new.uid, euid = new.euid, suid = new.suid, fsuid = new.fsuid, gid = new.gid, egid = new.egid, sgid = new.sgid, fsgid = new.fsgid, groups = new.groups.len());
        Ok(old)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(r: u32, e: u32, s: u32) -> Cred {
        Cred { uid: r, euid: e, suid: s, fsuid: e, ..Cred::new(r, r) }
    }
    fn ids(c: &Cred) -> [u32; 4] {
        [c.uid, c.euid, c.suid, c.fsuid]
    }

    #[test]
    fn setuid_root_sets_all_user_only_effective() {
        assert_eq!(ids(&set_uid(&c(0, 0, 0), 1000).unwrap().unwrap()), [1000; 4]);
        // seteuid-style root (ruid 0, euid 1000): not capable, may go back to its saved 0 (effective only).
        assert_eq!(ids(&set_uid(&c(0, 1000, 0), 0).unwrap().unwrap()), [0, 0, 0, 0]);
        assert_eq!(ids(&set_uid(&c(1000, 1000, 0), 0).unwrap().unwrap()), [1000, 0, 0, 0]);
        assert_eq!(set_uid(&c(1000, 1000, 1000), 0), Err(EPERM));
        assert_eq!(set_uid(&c(0, 0, 0), NO_ID), Err(EINVAL));
        assert_eq!(ids(&set_uid(&c(1000, 1000, 1000), 1000).unwrap().unwrap()), [1000; 4]);
    }

    #[test]
    fn setreuid_rules() {
        // Unprivileged swap of real and effective: suid follows the new euid (ruid given).
        assert_eq!(ids(&set_reuid(&c(1000, 1001, 1001), 1001, 1000).unwrap().unwrap()), [1001, 1000, 1000, 1000]);
        // Only euid, to the real id: suid unchanged.
        assert_eq!(ids(&set_reuid(&c(1000, 1001, 1002), NO_ID, 1000).unwrap().unwrap()), [1000, 1000, 1002, 1000]);
        // Only euid, to the saved id (not the real one): suid = new euid.
        assert_eq!(ids(&set_reuid(&c(1000, 1001, 1002), NO_ID, 1002).unwrap().unwrap()), [1000, 1002, 1002, 1002]);
        // The real id may not become the saved id.
        assert_eq!(set_reuid(&c(1000, 1001, 1002), 1002, NO_ID), Err(EPERM));
        assert_eq!(set_reuid(&c(1000, 1000, 1000), NO_ID, 0), Err(EPERM));
        assert_eq!(ids(&set_reuid(&c(0, 0, 0), 5, 6).unwrap().unwrap()), [5, 6, 6, 6]);
    }

    #[test]
    fn setresuid_rules() {
        assert_eq!(set_resuid(&c(1000, 1000, 1000), NO_ID, NO_ID, NO_ID), Ok(None));
        assert_eq!(set_resuid(&c(1000, 1000, 1000), 0, NO_ID, NO_ID), Err(EPERM));
        assert_eq!(ids(&set_resuid(&c(1000, 1001, 1002), 1002, 1000, 1001).unwrap().unwrap()), [1002, 1000, 1001, 1000]);
        assert_eq!(ids(&set_resuid(&c(0, 0, 0), 1, 2, 3).unwrap().unwrap()), [1, 2, 3, 2]);
        // euid equal but fsuid differs: not a no-op, fsuid follows euid.
        let mut x = c(1000, 1000, 1000);
        x.fsuid = 7;
        assert_eq!(ids(&set_resuid(&x, NO_ID, 1000, NO_ID).unwrap().unwrap()), [1000; 4]);
        assert_eq!(set_resuid(&c(0, 0, 0), NO_ID, 5, NO_ID).unwrap().map(|n| capable(&n, Cap::Setuid)), Some(false));
    }

    #[test]
    fn setfsuid_and_fs_caps() {
        assert_eq!(set_fsuid(&c(1000, 1000, 1000), 0), None);
        let root = c(0, 0, 0);
        let n = set_fsuid(&root, 1000).unwrap();
        assert!(!capable(&n, Cap::DacOverride) && capable(&n, Cap::Setuid));
        let full = (1u64 << 41) - 1;
        assert_eq!(cap_sets(&n, full), (full, full & !CAP_FS_MASK));
        assert_eq!(cap_sets(&root, full), (full, full));
        assert_eq!(cap_sets(&c(1000, 1000, 0), full), (full, 0), "a saved root keeps the permitted set");
        assert_eq!(set_fsuid(&root, NO_ID), None);
        assert_eq!(set_fsuid(&c(1000, 1001, 0), 0).map(|n| capable(&n, Cap::Fowner)), Some(true));
    }

    #[test]
    fn groups_sorted_and_privileged() {
        let n = set_groups(&Cred::new(0, 0), sorted_groups(alloc::vec![30, 10, 20, 10])).unwrap().unwrap();
        assert_eq!(*n.groups, alloc::vec![10, 10, 20, 30]);
        assert!(n.in_group(20) && n.in_group(0) && !n.in_group(25));
        assert_eq!(set_groups(&Cred::new(1000, 1000), sorted_groups(alloc::vec![])), Err(EPERM));
        let g = set_gid(&Cred::new(0, 0), 1000).unwrap().unwrap();
        assert_eq!((g.gid, g.egid, g.sgid, g.fsgid), (1000, 1000, 1000, 1000));
        assert_eq!(set_regid(&Cred::new(1000, 1000), 5, NO_ID), Err(EPERM));
        assert_eq!(set_resgid(&Cred::new(1000, 1000), NO_ID, 1000, NO_ID), Ok(None));
    }

    #[test]
    fn setresuid_noop_keeps_fsuid_as_linux() {
        let x = set_fsuid(&c(0, 0, 0), 1234).unwrap();
        assert_eq!(set_resuid(&x, NO_ID, NO_ID, NO_ID), Ok(None));
        assert_eq!(ids(&set_resuid(&x, NO_ID, 0, NO_ID).unwrap().unwrap()), [0; 4]);
        assert_eq!(set_resuid(&x, 0, NO_ID, NO_ID), Ok(None), "the same real id with eff -1 is a no-op too");
    }

    /// Credentials are per thread group, and musl/glibc repeat every set*id in each thread (`__synccall`;
    /// musl kills the process when a later thread fails). So each call must also succeed when it is repeated
    /// on its own result, even when the first one dropped the privilege it needed.
    #[test]
    fn repeated_calls_succeed_after_a_drop() {
        type F = fn(&Cred) -> Change;
        let calls: [(&str, F); 10] = [
            ("setuid", |o| set_uid(o, 1000)),
            ("setgid", |o| set_gid(o, 1000)),
            ("setreuid", |o| set_reuid(o, 1000, 2000)),
            ("setreuid -1", |o| set_reuid(o, NO_ID, 1000)),
            ("setregid", |o| set_regid(o, 1000, 2000)),
            ("setresuid", |o| set_resuid(o, 1000, 2000, 3000)),
            ("seteuid", |o| set_resuid(o, NO_ID, 1000, NO_ID)),
            ("setresgid", |o| set_resgid(o, 1000, 2000, 3000)),
            ("setgroups", |o| set_groups(o, sorted_groups(alloc::vec![5, 1000]))),
            ("setuid back", |o| set_uid(o, 0)),
        ];
        let starts = [c(0, 0, 0), c(1000, 0, 0), c(0, 1000, 0), set_uid(&Cred::new(0, 0), 1000).unwrap().unwrap()];
        for start in &starts {
            for (name, f) in &calls {
                let Ok(first) = f(start) else { continue };
                let once = first.unwrap_or_else(|| start.clone());
                let again = f(&once);
                assert!(again.is_ok(), "{name} from {start:?}: the repeat failed with {again:?}");
                assert_eq!(again.unwrap().unwrap_or_else(|| once.clone()), once, "{name}: the repeat changed the ids");
            }
        }
    }

    #[test]
    fn exec_moves_saved_to_effective() {
        let x = Cred { euid: 0, ..c(1000, 1000, 1000) }.for_exec();
        assert_eq!(ids(&x), [1000, 0, 0, 0]);
    }
}
