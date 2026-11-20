//! SX-2: a thread group's namespaces (Linux `nsproxy`, linux-7.3.1/kernel/nsproxy.c) and the three calls that
//! change them: clone/clone3 with CLONE_NEW* (`copy_namespaces`), unshare(2) (`ksys_unshare`, kernel/fork.c
//! :3254) and setns(2) (`setns`, nsproxy.c :569; L7-RESTE-AB 2: also through a pidfd, several namespaces at once).
//!
//! SX-3a: CLONE_NEWNS copies the mount view (`mnt.rs`; the view itself is vfs's).
//!
//! What V2 refuses (EINVAL, a `ns.unsupported` event; a kernel built without them answers the same):
//! CLONE_NEWCGROUP (no cgroups), CLONE_NEWTIME (no time namespaces);
//! unshare of CLONE_FILES or CLONE_FS while another process shares the fd table (CLONE_FILES: Process keeps its
//! table for its whole image, and the file-system state lives in it). A file-system state shared only through
//! CLONE_FS is copied, as Linux does.

use super::misc::{self, IpcNs, NetNs, UtsNs};
use super::mnt::{self, MntNs};
use super::pid::{self, PidNs};
use super::user::{self, UserNs};
use crate::Process;
use crate::cred::{self, Cap, Cred};
use crate::task::Thread;
use alloc::sync::Arc;
use ksync::Once;
use uapi::ab::clone::*;
use uapi::errno::*;
use vfs::Errno;

/// One group's namespaces besides its user namespace (in its credentials) and its own pid namespace (fixed at
/// its creation, `Group::pid_ns`).
#[derive(Clone)]
pub struct NsSet {
    pub uts: Arc<UtsNs>,
    pub ipc: Arc<IpcNs>,
    pub net: Arc<NetNs>,
    /// The pid namespace its next children start in (`pid_ns_for_children`).
    pub pid_for_children: Arc<PidNs>,
    /// SX-3a: the mount namespace (its view: `Process::mount_ns`).
    pub mnt: Arc<MntNs>,
}

static INIT: Once<Arc<NsSet>> = Once::new();

/// `init_nsproxy`.
pub fn init_set() -> &'static Arc<NsSet> {
    INIT.get_or_init(|| Arc::new(NsSet { uts: misc::init_uts().clone(), ipc: misc::init_ipc().clone(), net: misc::init_net().clone(), pid_for_children: pid::init_pid_ns().clone(), mnt: mnt::init_mnt().clone() }))
}

impl NsSet {
    /// Its mount view is not the boot view every program starts in (`Process::ns`).
    pub(crate) fn in_copied_view(&self) -> bool {
        !Arc::ptr_eq(&self.mnt, mnt::init_mnt())
    }
}

/// The namespace flags V2 builds.
const BUILT: u64 = CLONE_NEWUTS | CLONE_NEWIPC | CLONE_NEWNET | CLONE_NEWPID | CLONE_NEWNS;
/// The namespace flags V2 does not build (EINVAL).
const NOT_BUILT: u64 = CLONE_NEWCGROUP | CLONE_NEWTIME;

fn unsupported(pid: u32, flags: u64, what: &'static str) -> Errno {
    klog::event!(Info, "ns", "unsupported", pid = pid, flags = klog::Hex(flags), what = what);
    EINVAL
}

/// `create_new_namespaces` (nsproxy.c :67) for flags `f` from set `old`, owned by the user namespace of `c`
/// and charged to its euid (`ns::count`, ENOSPC); `active` is the caller's own pid namespace (`copy_pid_ns`:
/// a new one only when the caller's children would still start in it, else EINVAL).
/// `empty`: the new mount namespace holds a copy of the root mount alone (CLONE_EMPTY_MNTNS, UNSHARE_EMPTY_MNTNS).
fn derive(old: &Arc<NsSet>, f: u64, c: &Cred, active: &Arc<PidNs>, empty: bool) -> Result<Arc<NsSet>, Errno> {
    let user_ns = &c.user_ns;
    let uts = if f & CLONE_NEWUTS != 0 { misc::new_uts(&old.uts, user_ns, c.euid)? } else { old.uts.clone() };
    let ipc = if f & CLONE_NEWIPC != 0 { misc::new_ipc(user_ns, c.euid)? } else { old.ipc.clone() };
    let pid_for_children = if f & CLONE_NEWPID != 0 {
        if !Arc::ptr_eq(active, &old.pid_for_children) {
            return Err(EINVAL);
        }
        pid::create(&old.pid_for_children, user_ns, c.euid)?
    } else {
        old.pid_for_children.clone()
    };
    let net = if f & CLONE_NEWNET != 0 { misc::new_net(user_ns, c.euid)? } else { old.net.clone() };
    let mnt = if f & CLONE_NEWNS != 0 { mnt::copy(&old.mnt, user_ns, c.euid, empty)? } else { old.mnt.clone() };
    Ok(Arc::new(NsSet { uts, ipc, net, pid_for_children, mnt }))
}

/// `copy_namespaces` for a new process made with flags `f` by a caller with set `old` and pid namespace
/// `active`; `cred` are the child's credentials (a new user namespace already in them). No namespace flag:
/// the same set. Else CAP_SYS_ADMIN in the child's user namespace (EPERM), and CLONE_NEWIPC with
/// CLONE_SYSVSEM is EINVAL.
pub(crate) fn for_clone(pid: u32, old: &Arc<NsSet>, f: u64, cred: &Cred, active: &Arc<PidNs>) -> Result<Arc<NsSet>, Errno> {
    if f & (BUILT | NOT_BUILT) == 0 {
        return Ok(old.clone());
    }
    if !cred::ns_capable(cred, &cred.user_ns, Cap::SysAdmin) {
        klog::event!(Info, "ns", "clone.refused", pid = pid, flags = klog::Hex(f), reason = "CAP_SYS_ADMIN");
        return Err(EPERM);
    }
    if f & (CLONE_NEWIPC | CLONE_SYSVSEM) == CLONE_NEWIPC | CLONE_SYSVSEM {
        return Err(EINVAL);
    }
    if f & NOT_BUILT != 0 {
        return Err(unsupported(pid, f, "cgroup or time namespace"));
    }
    derive(old, f, cred, active, f & CLONE_EMPTY_MNTNS != 0)
}

/// SX-3a: a child made with CLONE_NEWNS (set `set`, parent's set `parent`): its own copy of the file-system
/// state moves into the new view now (`copy_mnt_ns` with `new_fs`). Without this call the child's first file
/// call does it (`FsState::enter`), but its execve before that would still look in the parent's view (CR to
/// AB: clone.rs calls this after making the child's state).
pub fn enter_child_view(set: &NsSet, parent: &NsSet, fs: &vfs::FsState) {
    if !Arc::ptr_eq(&set.mnt, &parent.mnt) {
        mnt::enter(fs, &set.mnt);
    }
}

/// The credentials of a new user namespace made by `c` (`create_user_ns` + `set_cred_user_ns`): every
/// capability in it. The caller sets the thread's bounding set to full. `chrooted`: `Process::chrooted`.
pub(crate) fn new_user_cred(c: &Cred, chrooted: bool) -> Result<Cred, Errno> {
    let ns = user::create(c, chrooted)?;
    Ok(cred::in_new_user_ns(c, ns))
}

/// unshare(2)'s flags as Linux widens them (`ksys_unshare`): a user namespace also unshares the thread group
/// and the file-system state; the address space the signal handlers; those the thread group; a mount
/// namespace the file-system state.
fn widen(mut f: u64) -> u64 {
    if f & CLONE_NEWUSER != 0 {
        f |= CLONE_THREAD | CLONE_FS;
    }
    if f & CLONE_VM != 0 {
        f |= CLONE_SIGHAND;
    }
    if f & CLONE_SIGHAND != 0 {
        f |= CLONE_THREAD;
    }
    // An empty mount namespace is a new mount namespace (linux-7.3.1/kernel/fork.c:3282).
    if f & UNSHARE_EMPTY_MNTNS != 0 {
        f |= CLONE_NEWNS;
    }
    if f & CLONE_NEWNS != 0 {
        f |= CLONE_FS;
    }
    f
}

/// What unshare(2) accepts (`check_unshare_flags`, linux-7.3.1/kernel/fork.c:3179: the thread flags,
/// CLONE_NS_ALL and UNSHARE_EMPTY_MNTNS).
const UNSHARE_OK: u64 = CLONE_THREAD | CLONE_FS | CLONE_NEWNS | CLONE_SIGHAND | CLONE_VM | CLONE_FILES | CLONE_SYSVSEM | CLONE_NEWUTS | CLONE_NEWIPC | CLONE_NEWNET | CLONE_NEWUSER | CLONE_NEWPID | CLONE_NEWCGROUP | CLONE_NEWTIME | UNSHARE_EMPTY_MNTNS;

impl Process {
    /// `current_chrooted` (linux-7.3.1/fs/namespace.c:6556): thread `t`'s root is not its mount view's root.
    pub fn chrooted(&self, t: &Thread) -> bool {
        let view = self.mount_ns();
        t.fs(self).root().is_some_and(|r| !r.same(&view.cross_down(view.root_path())) && !r.same(&view.root_path()))
    }

    /// The group's namespace set now.
    pub fn nsset(&self) -> Arc<NsSet> {
        self.group().nsset()
    }

    /// unshare(2) by thread `t` (linux-7.3.1/kernel/fork.c:3254 `ksys_unshare` and :3179 `check_unshare_flags`):
    /// EINVAL for unknown flags, for thread-group flags while other threads run, and for what V2 does not build
    /// (cgroup and time namespaces); then
    /// the user namespace (`unshare_userns`: ENOSPC, EPERM), the other namespaces (CAP_SYS_ADMIN in the new
    /// or current user namespace: EPERM; a second CLONE_NEWPID before a fork: EINVAL), and only when all
    /// succeeded the new set and credentials are put in place.
    pub fn unshare(&self, t: &Thread, flags: u64) -> Result<(), Errno> {
        if flags & !UNSHARE_OK != 0 {
            return Err(EINVAL);
        }
        let f = widen(flags);
        let g = self.group();
        if f & (CLONE_THREAD | CLONE_SIGHAND | CLONE_VM) != 0 && g.thread_count() > 1 {
            klog::event!(Info, "ns", "unshare.threaded", pid = self.pid, flags = klog::Hex(flags));
            return Err(EINVAL);
        }
        // `current_is_single_threaded`: no other process uses the address space (a vfork parent, CLONE_VM).
        if f & CLONE_VM != 0 && crate::mm_shared(&self.mm, self.pid) {
            return Err(EINVAL);
        }
        // `unshare_fd`: V2 cannot give one process of a CLONE_FILES pair its own table (the image holds it).
        // The cwd/root/umask are apart from the table (FS-16) and copied below (`unshare_fs`).
        if f & CLONE_FILES != 0 && crate::fds_shared(&self.fds, self.pid) {
            return Err(unsupported(self.pid, flags, "fd table shared with another process (CLONE_FILES)"));
        }
        let old = self.cred();
        let new_cred = if f & CLONE_NEWUSER != 0 { Some(new_user_cred(&old, self.chrooted(t))?) } else { None };
        // `unshare_nsproxy_namespaces`: CAP_SYS_ADMIN first (EPERM), then what V2 does not build (EINVAL).
        let new_set = if f & (BUILT | NOT_BUILT) != 0 {
            let c: &Cred = new_cred.as_ref().unwrap_or(&old);
            if !cred::ns_capable(c, &c.user_ns, Cap::SysAdmin) {
                klog::event!(Info, "ns", "unshare.refused", pid = self.pid, flags = klog::Hex(flags), reason = "CAP_SYS_ADMIN");
                return Err(EPERM);
            }
            if f & NOT_BUILT != 0 {
                return Err(unsupported(self.pid, flags, "cgroup or time namespace"));
            }
            Some(derive(&g.nsset(), f, c, &g.pid_ns, f & UNSHARE_EMPTY_MNTNS != 0)?)
        } else {
            None
        };
        if f & CLONE_FS != 0 {
            self.unshare_fs(t);
        }
        // kernel/fork.c:3300, :3321: CLONE_SYSVSEM is exit_sem(); a new IPC namespace also leaves the undo list (the
        // old arrays are out of reach) and orphans the caller's segments in the old one.
        if f & (CLONE_NEWIPC | CLONE_SYSVSEM) != 0 {
            crate::ipc::leave_undo(t, f & CLONE_NEWIPC != 0);
        }
        if let Some(s) = new_set {
            // `copy_mnt_ns`: the caller's (now private) state moves onto the copies; its other threads follow
            // at their next file call (V2 keeps namespaces per thread group).
            if !Arc::ptr_eq(&s.mnt, &g.nsset().mnt) {
                mnt::enter(&t.fs(self), &s.mnt);
            }
            g.set_nsset(s);
        }
        if let Some(c) = new_cred {
            let inum = c.user_ns.inum;
            self.change_cred(t, move |_| Ok(Some(c)))?;
            t.prctl().set_full_bset();
            klog::event!(Info, "ns", "unshare.user", pid = self.pid, inum = inum);
        }
        klog::event!(Info, "ns", "unshare", pid = self.pid, flags = klog::Hex(flags));
        Ok(())
    }

    /// `unshare_fs` (linux-7.3.1/kernel/fork.c:3210): the calling thread gets its own copy of its cwd/root/umask when
    /// anybody else uses them (`fs->users != 1`: another thread of the group, or another process through
    /// CLONE_FS). Alone in its group it stays the image's state, so /proc and later threads follow it.
    fn unshare_fs(&self, t: &Thread) {
        let fs = t.fs(self);
        let threads = self.group().threads();
        let by_thread = threads.iter().any(|o| !core::ptr::eq(&**o, t) && Arc::ptr_eq(&o.fs(self), &fs));
        if !by_thread && !crate::fs_shared(&fs, self.pid) {
            return;
        }
        let copy = Arc::new(fs.copy());
        if threads.len() > 1 {
            t.set_own_fs(Some(copy));
        } else {
            self.set_image_fs(copy);
            t.set_own_fs(None);
        }
        klog::event!(Debug, "ns", "unshare.fs", pid = self.pid, tid = t.tid(), threads = threads.len());
    }

    /// setns(2) with a namespace file (`validate_ns` → the kind's `install`, linux-7.3.1/kernel/nsproxy.c:569):
    /// `nstype` 0 or the file's kind (else EINVAL), then the kind's rules (`install`).
    pub fn setns(&self, t: &Thread, target: &super::file::NsRef, nstype: u64) -> Result<(), Errno> {
        let kind = target.kind();
        if nstype != 0 && nstype != kind.flag() {
            return Err(EINVAL);
        }
        let mut n = Pending::new(&self.group().nsset(), kind.flag());
        self.install(t, &mut n, target)?;
        self.commit(t, n);
        klog::event!(Info, "ns", "setns", pid = self.pid, kind = kind.name(), inum = target.inum());
        Ok(())
    }

    /// L7-RESTE-AB 2: setns(pidfd, flags) (linux-7.3.1/kernel/nsproxy.c:569, `check_setns_flags` :300,
    /// `validate_nsset` :396, `commit_nsset` :531): enters the namespaces `flags` names of the task `target` (the
    /// pidfd's thread, or for a process pidfd its leader) at once, all or nothing. EINVAL for no flag or one that
    /// is not a namespace (and for cgroup and time namespaces, which V2 does not build: a kernel without them
    /// answers the same); ESRCH when the task is gone; EPERM unless the caller may read it as ptrace does with its
    /// real ids (`PTRACE_MODE_READ_REALCREDS`); ESRCH when it already ended (its namespaces are let go at exit).
    /// Then each namespace's `install` in Linux's order (user, mount, uts, ipc, pid, net) against the caller's
    /// credentials, a later one judged in the user namespace an earlier step entered; the first refusal leaves
    /// everything as it was.
    pub fn setns_task(&self, t: &Thread, target: &crate::pidfd::Target, flags: u64) -> Result<(), Errno> {
        if flags == 0 || flags & !CLONE_NAMESPACES != 0 || flags & NOT_BUILT != 0 {
            return Err(EINVAL);
        }
        let p = crate::pidfd::process_of(&target.group).ok_or(ESRCH)?;
        if target.thread.is_some() && target.live_thread().is_none() {
            return Err(ESRCH);
        }
        if !self.may_ptrace(&p, false) {
            return Err(EPERM);
        }
        // `exit_task_namespaces`: the task ran do_exit (a leader that ended while other threads run, too).
        let task = match &target.thread {
            Some(_) => target.live_thread(),
            None => crate::task::find_thread(target.group.tgid).filter(|x| Arc::ptr_eq(x.group(), &target.group)),
        };
        if task.is_none_or(|x| x.exit_requested()) || target.group.is_finished() {
            return Err(ESRCH);
        }
        use super::{Kind, file::ns_of};
        let mut n = Pending::new(&self.group().nsset(), flags);
        for (flag, kind) in [(CLONE_NEWUSER, Kind::User), (CLONE_NEWNS, Kind::Mnt), (CLONE_NEWUTS, Kind::Uts), (CLONE_NEWIPC, Kind::Ipc), (CLONE_NEWPID, Kind::Pid), (CLONE_NEWNET, Kind::Net)] {
            if flags & flag != 0 {
                self.install(t, &mut n, &ns_of(&p, kind))?;
            }
        }
        self.commit(t, n);
        klog::event!(Info, "ns", "setns.pidfd", pid = self.pid, target = target.id(), flags = klog::Hex(flags));
        Ok(())
    }

    /// One namespace's `install` (linux-7.3.1/kernel/user_namespace.c:1343 `userns_install`, fs/namespace.c:6703
    /// `mntns_install`, kernel/utsname.c:127, ipc/namespace.c:233, kernel/pid_namespace.c:393,
    /// net/core/net_namespace.c:1534) into `n`. Capabilities are the caller's present credentials, asked in the
    /// namespace's owner and in the user namespace `n` holds by then (`nsset->cred->user_ns`). User: not the
    /// current one and a single thread whose file-system state nobody shares (EINVAL), CAP_SYS_ADMIN in the target
    /// (EPERM); the new credentials hold every capability there. UTS, IPC, network: both CAP_SYS_ADMIN (EPERM). Pid:
    /// the same, and the target at or below the caller's own pid namespace (EINVAL); only the children start there.
    /// Mount: CAP_SYS_ADMIN in its owner, CAP_SYS_CHROOT and CAP_SYS_ADMIN in `n`'s user namespace (EPERM); setns
    /// of the mount namespace alone needs a state nobody else shares (EINVAL), with other namespaces Linux works on
    /// a copy and puts its root and cwd in place at the commit; both go to the view's root.
    fn install(&self, t: &Thread, n: &mut Pending, target: &super::file::NsRef) -> Result<(), Errno> {
        use super::file::NsRef;
        let g = self.group();
        let c = self.cred();
        let admin_in = |ns: &UserNs| cred::ns_capable(&c, ns, Cap::SysAdmin);
        let user_now = n.cred.as_ref().map_or_else(|| c.user_ns.clone(), |x| x.user_ns.clone());
        let both = |owner: &Arc<UserNs>| if admin_in(owner) && admin_in(&user_now) { Ok(()) } else { Err(EPERM) };
        match target {
            NsRef::User(ns) => {
                if Arc::ptr_eq(ns, &c.user_ns) || g.thread_count() > 1 || crate::fs_shared(&t.fs(self), self.pid) {
                    return Err(EINVAL);
                }
                if !admin_in(ns) {
                    return Err(EPERM);
                }
                let full = Some(cred::CapSets { prm: crate::prctl_state::CAP_FULL_SET, eff: crate::prctl_state::CAP_FULL_SET });
                n.cred = Some(Cred { user_ns: ns.clone(), caps: full, cap_inh: 0, ..(*c).clone() });
            }
            NsRef::Uts(ns) => {
                both(&ns.user_ns)?;
                n.set.uts = ns.clone();
            }
            NsRef::Ipc(ns) => {
                both(&ns.user_ns)?;
                n.set.ipc = ns.clone();
            }
            NsRef::Net(ns) => {
                both(&ns.user_ns)?;
                n.set.net = ns.clone();
            }
            NsRef::Pid(ns) => {
                both(&ns.user_ns)?;
                if !g.pid_ns.is_ancestor_of(ns) {
                    return Err(EINVAL);
                }
                n.set.pid_for_children = ns.clone();
            }
            NsRef::Mnt(ns) => {
                if !admin_in(&ns.user_ns) || !cred::ns_capable(&c, &user_now, Cap::SysChroot) || !admin_in(&user_now) {
                    return Err(EPERM);
                }
                if n.flags == CLONE_NEWNS {
                    let fs = t.fs(self);
                    let by_thread = g.threads().iter().any(|o| !core::ptr::eq(&**o, t) && Arc::ptr_eq(&o.fs(self), &fs));
                    if by_thread || crate::fs_shared(&fs, self.pid) {
                        return Err(EINVAL);
                    }
                }
                n.set.mnt = ns.clone();
                n.root = Some(mnt::top_root(ns));
            }
        }
        Ok(())
    }

    /// `commit_nsset`: the credentials, then root and cwd, then the IPC undo list (it belongs to the old
    /// namespace, kernel/nsproxy.c:556), then the namespaces.
    fn commit(&self, t: &Thread, n: Pending) {
        let g = self.group();
        if let Some(nc) = n.cred {
            let inum = nc.user_ns.inum;
            // A copy of the present credentials with the user namespace replaced: nothing in it can fail.
            let _ = self.change_cred(t, move |_| Ok(Some(nc)));
            t.prctl().set_full_bset();
            klog::event!(Info, "ns", "setns.user", pid = self.pid, inum = inum);
        }
        if let Some(top) = n.root {
            t.fs(self).place(&n.set.mnt.view, top.clone(), top);
        }
        if n.flags & CLONE_NEWIPC != 0 {
            crate::ipc::leave_undo(t, false);
        }
        if n.flags & !CLONE_NEWUSER != 0 {
            g.set_nsset(Arc::new(n.set));
        }
    }
}

/// setns's work in progress (Linux `struct nsset`): a copy of the caller's set with the namespaces installed so
/// far, the new credentials once a user namespace was entered, and the root a mount namespace gives.
struct Pending {
    flags: u64,
    set: NsSet,
    cred: Option<Cred>,
    root: Option<vfs::Path>,
}

impl Pending {
    fn new(set: &Arc<NsSet>, flags: u64) -> Pending {
        Pending { flags, set: (**set).clone(), cred: None, root: None }
    }
}
