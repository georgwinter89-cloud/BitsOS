//! clone, clone3, fork and vfork (linux-6.6/kernel/fork.c `kernel_clone` :2868, `copy_process` :2240): a new
//! thread in the caller's group (CLONE_THREAD) or a new process with a copied (fork) or shared (CLONE_VM,
//! vfork) address space.
//!
//! V2's limits, each refused with EINVAL and an event (Linux accepts them): namespaces (a kernel built without
//! them answers EINVAL too), CLONE_INTO_CGROUP, and a CLONE_THREAD without
//! CLONE_FILES and CLONE_FS (the fd table and cwd belong to the group's image; no libc asks for that). Two
//! approximations, recorded in plan/notes/ab-AB-4.md: CLONE_SIGHAND without CLONE_THREAD copies the actions
//! instead of sharing them, and CLONE_FS without CLONE_FILES copies the cwd.

use crate::cred::{Cap, capable};
use crate::group::{Group, Ids};
use crate::task::{self, Thread};
use crate::{Process, SpawnError, make_image};
use alloc::sync::Arc;
use hal::entry::UserContext;
use uapi::ab::clone::*;
use uapi::ab::rlimit::RLIMIT_NPROC;
use uapi::errno::*;
use vfs::Errno;

klog::counter!(FORKS, "proc.forks");
klog::counter!(THREADS, "proc.threads_made");

/// Processes and threads made since boot, by clone and by the kernel's spawn (`/proc/stat` "processes", Linux
/// `total_forks`, which also counts the kernel's own threads).
pub fn forks_total() -> u64 {
    FORKS.get().saturating_add(THREADS.get()).saturating_add(crate::SPAWNED.get())
}

/// clone's arguments as `struct kernel_clone_args` (legacy clone and clone3 both fill it).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CloneArgs {
    /// Flags without the exit signal.
    pub flags: u64,
    pub exit_signal: u32,
    /// The child's stack pointer (0: the caller's).
    pub stack: u64,
    pub parent_tid: u64,
    pub child_tid: u64,
    pub tls: u64,
    pub pidfd: u64,
    /// clone3's set_tid: its first entry and how many were given (V2 has one pid namespace, so one at most).
    pub set_tid: Option<i32>,
    pub set_tid_count: u64,
}

fn refuse(p: &Process, flags: u64, what: &'static str) -> Errno {
    klog::event!(Info, "proc", "clone.unsupported", pid = p.pid, flags = klog::Hex(flags), what = what);
    EINVAL
}

/// `copy_process`'s flag checks in Linux's order, then V2's limits.
fn check(p: &Process, a: &CloneArgs) -> Result<(), Errno> {
    let f = a.flags;
    if f & CLONE_PIDFD != 0 && f & CLONE_PARENT_SETTID != 0 && a.pidfd == a.parent_tid {
        return Err(EINVAL);
    }
    if f & (CLONE_NEWNS | CLONE_FS) == CLONE_NEWNS | CLONE_FS || f & (CLONE_NEWUSER | CLONE_FS) == CLONE_NEWUSER | CLONE_FS {
        return Err(EINVAL);
    }
    if f & CLONE_THREAD != 0 && f & CLONE_SIGHAND == 0 {
        return Err(EINVAL);
    }
    if f & CLONE_SIGHAND != 0 && f & CLONE_VM == 0 {
        return Err(EINVAL);
    }
    // Init (SIGNAL_UNKILLABLE) must not make siblings: nobody would reap them.
    if f & CLONE_PARENT != 0 && p.pid == 1 {
        return Err(EINVAL);
    }
    if f & CLONE_THREAD != 0 && f & (CLONE_NEWUSER | CLONE_NEWPID) != 0 {
        return Err(EINVAL);
    }
    if f & CLONE_PIDFD != 0 && f & (CLONE_DETACHED | CLONE_THREAD) != 0 {
        return Err(EINVAL);
    }
    if f & CLONE_NAMESPACES != 0 {
        return Err(refuse(p, f, "namespaces"));
    }
    if f & CLONE_INTO_CGROUP != 0 {
        return Err(refuse(p, f, "cgroups"));
    }
    if f & CLONE_THREAD != 0 && f & (CLONE_FILES | CLONE_FS) != CLONE_FILES | CLONE_FS {
        return Err(refuse(p, f, "thread with its own files or cwd"));
    }
    Ok(())
}

/// The child's registers: the caller's at the syscall, rax = 0, its own stack and TLS when given.
fn child_context(ctx: &UserContext, a: &CloneArgs) -> Result<UserContext, Errno> {
    let mut c = *ctx;
    c.regs.rax = 0;
    c.stats = Default::default();
    if a.stack != 0 {
        c.regs.rsp = a.stack;
    }
    if a.flags & CLONE_SETTLS != 0 {
        // `do_arch_prctl_64(ARCH_SET_FS)`: a base at or above TASK_SIZE_MAX is EPERM.
        if a.tls >= hal::USER_TOP {
            return Err(EPERM);
        }
        c.fs_base = a.tls;
    }
    Ok(c)
}

/// clone/clone3/fork/vfork for thread `t` of image `p` at the registers `ctx`: the child's tid (its pid for a
/// new process). A vfork caller comes back once the child ran execve or ended.
pub fn clone(p: &Arc<Process>, t: &Arc<Thread>, ctx: &UserContext, a: &CloneArgs) -> Result<u32, Errno> {
    check(p, a)?;
    // `fatal_signal_pending`: a dying caller makes no children.
    if p.has_exited() {
        return Err(EINTR);
    }
    // RLIMIT_NPROC counts the real uid's tasks; exempt are the root user and CAP_SYS_RESOURCE/ADMIN
    // (`copy_process`, linux-6.6/kernel/fork.c:2370).
    let cred = p.cred();
    let exempt = cred.uid == 0 || capable(&cred, Cap::SysResource) || capable(&cred, Cap::SysAdmin);
    if !exempt && p.limits.get(RLIMIT_NPROC).is_some_and(|l| task::threads_of_uid(cred.uid) >= l.cur) {
        klog::event!(Info, "proc", "clone.nproc", pid = p.pid, uid = cred.uid);
        return Err(EAGAIN);
    }
    let want = wanted_tid(p, a)?;
    let cctx = child_context(ctx, a)?;
    // The caller's vector registers go to the child (Linux `fpu_clone`): a compiler may keep values in them
    // across the syscall instruction.
    let fpu = Some(hal::xsave::read_user_regs());
    let settid = if a.flags & CLONE_CHILD_SETTID != 0 { a.child_tid } else { 0 };
    if a.flags & CLONE_THREAD != 0 {
        return new_thread(p, t, a, want, cctx, fpu, settid);
    }
    new_process(p, t, a, want, cctx, fpu, settid)
}

/// clone3's set_tid as Linux's `alloc_pid` checks it: one namespace level, 1..pid_max, a running init unless
/// it asks for 1, CAP_CHECKPOINT_RESTORE (root); EEXIST comes when the id is taken.
fn wanted_tid(p: &Process, a: &CloneArgs) -> Result<Option<u32>, Errno> {
    if a.set_tid_count > 1 {
        return Err(EINVAL);
    }
    let Some(t) = a.set_tid else { return Ok(None) };
    if t < 1 || t as u32 >= crate::pid_max() {
        return Err(EINVAL);
    }
    if t != 1 && crate::find(1).is_none_or(|i| i.is_finished()) {
        return Err(EINVAL);
    }
    if !p.capable(Cap::CheckpointRestore) {
        return Err(EPERM);
    }
    Ok(Some(t as u32))
}

/// The new thread's alternate signal stack: the caller's, unless the child shares the memory without vfork
/// (`sas_ss_reset`, linux-6.6/kernel/fork.c:2551): two threads on one stack would overwrite each other.
fn child_alt_stack(t: &Thread, flags: u64) -> crate::signal::AltStack {
    if flags & CLONE_VM != 0 && flags & CLONE_VFORK == 0 { crate::signal::AltStack::DISABLED } else { t.alt_stack() }
}

fn new_thread(p: &Arc<Process>, t: &Thread, a: &CloneArgs, want: Option<u32>, cctx: UserContext, fpu: Option<hal::xsave::RegDump>, settid: u64) -> Result<u32, Errno> {
    let tid = match want {
        Some(w) if crate::find(w).is_some() || task::find_thread(w).is_some() => return Err(EEXIST),
        Some(w) => w,
        None => crate::new_tid().ok_or(EAGAIN)?,
    };
    let th = Thread::new(tid, p.group());
    th.prctl().inherit(t.prctl());
    th.set_alt_stack(child_alt_stack(t, a.flags));
    // AB-6: the creator's blocked mask (`dup_task_struct`), nothing pending.
    th.set_start_mask(t.blocked());
    if a.flags & CLONE_CHILD_CLEARTID != 0 {
        th.clear_child_tid.store(a.child_tid, core::sync::atomic::Ordering::Release);
    }
    if a.flags & CLONE_PARENT_SETTID != 0 {
        let _ = p.mm.copy_to_user(a.parent_tid, &tid.to_le_bytes());
    }
    task::start(p, th, cctx, fpu, settid, t.kthread()).map_err(|e| if e == SpawnError::NoMemory { EAGAIN } else { e.errno() })?;
    THREADS.inc();
    klog::event!(Debug, "proc", "clone.thread", pid = p.pid, tid = tid, flags = klog::Hex(a.flags));
    Ok(tid)
}

/// `copy_process`'s CLONE_PIDFD step (linux-6.6/kernel/fork.c:2531): an fd of the caller's table reserved
/// (close-on-exec, `get_unused_fd_flags(O_RDWR | O_CLOEXEC)`) and its number written to the `int` at `uaddr`
/// (`put_user`). The file is published only once the child started (`fd_install` at the end of copy_process),
/// so a failed start only gives the reserved number back and nobody sees a pidfd of a child that never ran.
fn reserve_pidfd(p: &Process, child: &Process, uaddr: u64) -> Result<usize, Errno> {
    let fd = p.fds.reserve(0, true)?;
    if p.mm.copy_to_user(uaddr, &(fd as i32).to_le_bytes()).is_err() {
        p.fds.unreserve(fd);
        klog::event!(Info, "proc", "clone.pidfd_fault", pid = p.pid, child = child.pid);
        return Err(EFAULT);
    }
    Ok(fd)
}

fn new_process(p: &Arc<Process>, t: &Arc<Thread>, a: &CloneArgs, want: Option<u32>, cctx: UserContext, fpu: Option<hal::xsave::RegDump>, settid: u64) -> Result<u32, Errno> {
    let f = a.flags;
    let vm = f & CLONE_VM != 0;
    let vfork = f & CLONE_VFORK != 0;
    // MM-5b's copy-on-write fork (ENOMEM also when the strict commit refuses the child's memory, as Linux).
    let mm = if vm { p.mm.clone() } else { Arc::new(p.mm.fork().map_err(|_| ENOMEM)?) };
    let fds = if f & CLONE_FILES != 0 { p.fds.clone() } else { Arc::new(p.fds.duplicate()?) };
    let signals = crate::signal::SignalState::new(p.signals.with(|s| s.for_child(f & CLONE_CLEAR_SIGHAND != 0)));
    let limits = p.limits.copy();
    let parent_of_caller = f & CLONE_PARENT != 0;
    let creator = if parent_of_caller { 0 } else { t.tid() };
    // Copied before the table lock: argv can be long, and nothing waits on proc.table for a copy.
    let (name, path) = (p.name.clone(), p.path.clone());
    let mut argv = alloc::vec::Vec::new();
    argv.try_reserve(p.argv.len()).map_err(|_| ENOMEM)?;
    argv.extend(p.argv.iter().cloned());
    let (pgid, sid, ctty, ns) = (p.pgid(), p.sid(), p.ctty(), p.ns.clone());
    // AB-13: the child starts with the caller's credentials (`copy_creds`; shared until either changes them).
    let cred = p.cred();
    let child = crate::insert_new(
        |pid, table| {
            // CLONE_PARENT: the caller's parent gets the child, which then reports with the caller's own exit
            // signal. Read under the table lock, where an ending parent reparents its children: either the
            // caller already carries its new parent, or the child is in the table when the reparenting runs.
            let (ppid, exit_signal) = if parent_of_caller { (p.ppid(), p.exit_signal()) } else { (p.pid, a.exit_signal) };
            // Belt and braces: a parent that is not a live entry any more gives the child to init (or the kernel),
            // reporting with SIGCHLD as a reparented child does.
            let (ppid, exit_signal) = if ppid == 0 || table.iter().any(|q| q.pid == ppid && !q.is_released()) {
                (ppid, exit_signal)
            } else {
                let init = table.iter().find(|q| q.pid == 1 && !q.is_released()).map_or(0, |i| i.pid);
                (init, if exit_signal != 0 { uapi::ab::signal::SIGCHLD } else { 0 })
            };
            let ids = Ids { tgid: pid, ppid, pgid, sid, session_leader: false, creator_tid: creator, cred: cred.clone() };
            let g = Group::new(ids, exit_signal, p.class, signals, limits, time::now_ns());
            g.set_ctty(ctty);
            make_image(pid, name, path, argv, p.program, &cred, mm, fds, ns, g)
        },
        want,
    )
    .map_err(|e| e.errno())?;
    child.group().set_image(&child);
    child.mm_flags.copy_from(&p.mm_flags);
    // `dup_mm` copies `saved_auxv` with the rest of the mm.
    let aux = p.saved_auxv.lock().clone();
    *child.saved_auxv.lock() = aux;
    // AB-7a: the mm's layout and exe_file go with it; the child has not exec'd (PF_FORKNOEXEC).
    let info = p.image_info.lock().clone();
    *child.image_info.lock() = crate::procinfo::ImageInfo { forked: true, ..info };
    child.group().copy_oom_from(p.group());
    // AB-12 CLONE_PIDFD: after the child's fd table was chosen, so the pidfd is the caller's alone (and the
    // child's only when it shares the table); its number is stored before the child runs (EFAULT fails it), the
    // file published after task::start.
    let pidfd = if f & CLONE_PIDFD != 0 {
        match reserve_pidfd(p, &child, a.pidfd) {
            Ok(fd) => Some(fd),
            Err(e) => {
                drop(crate::remove(child.pid));
                return Err(e);
            }
        }
    } else {
        None
    };
    child.release();
    let th = Thread::new(child.pid, child.group());
    th.prctl().inherit(t.prctl());
    th.set_alt_stack(child_alt_stack(t, f));
    th.set_start_mask(t.blocked());
    if f & CLONE_CHILD_CLEARTID != 0 {
        th.clear_child_tid.store(a.child_tid, core::sync::atomic::Ordering::Release);
    }
    if f & CLONE_PARENT_SETTID != 0 {
        let _ = p.mm.copy_to_user(a.parent_tid, &child.pid.to_le_bytes());
    }
    // FS-13: a forked child reads for its parent's preload session (Brave's zygote children). Before the start
    // (FS-15 AB-1): once running, the child may fault in pages on another CPU, and those belong to the session.
    vfs::preload::forked(p.pid, child.pid);
    if let Err(e) = task::start(&child, th, cctx, fpu, settid, t.kthread()) {
        klog::event!(Warn, "proc", "clone.no_thread", pid = p.pid, child = child.pid);
        if let Some(fd) = pidfd {
            p.fds.unreserve(fd);
        }
        vfs::preload::exited(child.pid);
        drop(crate::remove(child.pid));
        return Err(if e == SpawnError::NoMemory { EAGAIN } else { e.errno() });
    }
    if let Some(fd) = pidfd {
        p.fds.commit(fd, crate::pidfd::new_file(child.group(), false));
    }
    FORKS.inc();
    klog::event!(Info, "proc", "fork", pid = p.pid, child = child.pid, flags = klog::Hex(f), kind = if vfork { "vfork" } else if vm { "clone_vm" } else { "fork" });
    if vfork {
        // Linux `wait_for_vfork_done`: killable; the child's execve or end completes it.
        let g = child.group().clone();
        let pid = child.pid;
        drop(child);
        while !g.vfork_done.wait_timeout(100_000_000) {
            if p.has_exited() {
                break;
            }
        }
        return Ok(pid);
    }
    Ok(child.pid)
}
