//! kcmp(2) (AB-17; linux-6.6/kernel/kcmp.c `SYSCALL_DEFINE5(kcmp)` :135). Mesa compares dma-buf fds with
//! KCMP_FILE when F_DUPFD_QUERY is missing, Wine and bwrap compare address spaces with KCMP_VM.
//!
//! Order as Linux: both pids are looked up in the caller's pid namespace (a thread id names that thread; a
//! zombie is still found until it is reaped): ESRCH; then both must pass `ptrace_may_access(READ_REALCREDS)`
//! (`Process::may_ptrace(.., false)`): EPERM; then the type: unknown is EINVAL. The answer is 0 for the same
//! object, else 1 or 2 by the order of the objects' addresses after a per-boot, per-type scramble
//! (`kcmp_ptr` :50, cookies :37), so only equality and its antisymmetry mean anything.
//!
//! The objects (an ended task has none of its own: NULL, so two ended tasks compare equal, as in Linux):
//! - KCMP_FILE: the open files behind fd `idx1` of task 1 and `idx2` of task 2 (raw: O_PATH counts); EBADF
//!   when either is not open or the task ended;
//! - KCMP_VM: the address space; KCMP_FILES: the fd table; KCMP_FS: the thread's cwd/root/umask state;
//! - KCMP_SIGHAND: the signal actions, which V2 keeps per thread group (CLONE_SIGHAND without CLONE_THREAD
//!   copies them, plan/notes/ab-AB-4.md), so the group; it stays until the reap, as `sighand` does;
//! - KCMP_SYSVSEM compares undo lists (SX-IPC; two tasks without one compare equal, as Linux's NULLs);
//! - KCMP_IO: V2 has no I/O contexts, so every task has NULL there, Linux's answer for tasks that never used
//!   ioprio: equal;
//! - KCMP_EPOLL_TFD: Linux's EFAULT (the slot), EBADF (idx1, the epoll fd) and EINVAL (not an epoll, an
//!   O_PATH fd too) come first; then the `toff`-th item watching `tfd` (`vfs::epoll::Epoll::target`, INT-W24),
//!   ENOENT without one, compared with idx1's open file as KCMP_FILE.

use crate::SysCtx;
use alloc::sync::Arc;
use proc::{Process, Thread};
use uapi::ab::kcmp::*;
use uapi::errno::*;

fn sys_kcmp(c: &mut SysCtx<'_>) -> i64 {
    let (pid1, pid2, ty, idx1, idx2) = (c.args[0] as i32, c.args[1] as i32, c.args[2] as i32, c.args[3], c.args[4]);
    let (Some(t1), Some(t2)) = (task(c, pid1), task(c, pid2)) else { return -ESRCH };
    if !c.proc.may_ptrace(&t1.p, false) || !c.proc.may_ptrace(&t2.p, false) {
        klog::event!(Info, "syscall", "kcmp.refused", pid = c.proc.pid, uid = c.proc.cred().uid, pid1 = t1.p.pid, pid2 = t2.p.pid);
        return -EPERM;
    }
    let Ok(ty) = u32::try_from(ty) else { return -EINVAL };
    match ty {
        KCMP_FILE => match (t1.file(idx1 as u32), t2.file(idx2 as u32)) {
            (Some(a), Some(b)) => kcmp_ptr(Arc::as_ptr(&a) as usize, Arc::as_ptr(&b) as usize, ty),
            _ => -EBADF,
        },
        KCMP_VM => kcmp_ptr(t1.live(|p, _| Arc::as_ptr(&p.mm) as usize), t2.live(|p, _| Arc::as_ptr(&p.mm) as usize), ty),
        KCMP_FILES => kcmp_ptr(t1.live(|p, _| Arc::as_ptr(&p.fds) as usize), t2.live(|p, _| Arc::as_ptr(&p.fds) as usize), ty),
        KCMP_FS => kcmp_ptr(t1.live(|p, t| Arc::as_ptr(&t.fs(p)) as usize), t2.live(|p, t| Arc::as_ptr(&t.fs(p)) as usize), ty),
        KCMP_SIGHAND => kcmp_ptr(Arc::as_ptr(t1.p.group()) as usize, Arc::as_ptr(t2.p.group()) as usize, ty),
        KCMP_IO => kcmp_ptr(0, 0, ty),
        KCMP_SYSVSEM => kcmp_ptr(t1.live(|_, t| t.ipc().undo.list_ptr()), t2.live(|_, t| t.ipc().undo.list_ptr()), ty),
        KCMP_EPOLL_TFD => epoll_target(c, &t1, &t2, idx1, idx2),
        _ => -EINVAL,
    }
}
crate::syscall_nr!(uapi::ab::nr_more::KCMP, "kcmp", sys_kcmp);

/// A task as `find_task_by_vpid` finds it: a live thread with its process, or a process whose leader ended
/// (a zombie until the reap) without one.
struct Task {
    p: Arc<Process>,
    t: Option<Arc<Thread>>,
}

impl Task {
    /// `f` of the task's process and thread while it runs, else 0 (Linux's NULL after `exit_mm`, `exit_files`,
    /// `exit_fs`).
    fn live(&self, f: impl FnOnce(&Process, &Thread) -> usize) -> usize {
        match &self.t {
            Some(t) if !self.p.is_finished() => f(&self.p, t),
            _ => 0,
        }
    }
    /// `get_file_raw_ptr`: the open file behind `fd` (O_PATH ones too); None after the task ended.
    fn file(&self, fd: u32) -> Option<Arc<vfs::OpenFile>> {
        if self.t.is_none() || self.p.is_finished() {
            return None;
        }
        self.p.fds.get(fd as usize).ok()
    }
}

fn task(c: &SysCtx<'_>, nr: i32) -> Option<Task> {
    if nr <= 0 {
        return None;
    }
    if let Some(t) = crate::ns_sys::find_thread(c, nr as u32) {
        return Some(Task { p: t.group().image()?, t: Some(t) });
    }
    Some(Task { p: crate::ns_sys::find(c, nr as u32)?, t: None })
}

/// `kcmp_epoll_target` (:98): `idx2` points at `struct kcmp_epoll_slot { efd, tfd, toff }`.
fn epoll_target(c: &SysCtx<'_>, t1: &Task, t2: &Task, idx1: u64, slot: u64) -> i64 {
    let mut b = [0u8; EPOLL_SLOT_SIZE];
    if c.proc.mm.copy_from_user(&mut b, slot).is_err() {
        return -EFAULT;
    }
    let efd = u32::from_le_bytes([b[0], b[1], b[2], b[3]]);
    let tfd = i32::from_le_bytes([b[4], b[5], b[6], b[7]]);
    let toff = u32::from_le_bytes([b[8], b[9], b[10], b[11]]) as u64;
    let Some(filp) = t1.file(idx1 as u32) else { return -EBADF };
    // `fget_task` looks up with mask 0 (linux-6.6/fs/file.c:945): an O_PATH fd is found, then is no epoll.
    let Some(ep) = t2.file(efd) else { return -EBADF };
    let Some(epoll) = ep.file.as_epoll() else { return -EINVAL };
    // `get_epoll_tfile_raw_ptr` (linux-6.6/fs/eventpoll.c:1060): ENOENT when no item matches.
    match epoll.target(tfd, toff) {
        Some(tgt) => kcmp_ptr(Arc::as_ptr(&filp) as usize, Arc::as_ptr(&tgt) as usize, KCMP_FILE),
        None => -ENOENT,
    }
}

/// Per-boot cookies per type: the multiplier is odd (a bijection, so equality survives) and negative.
static COOKIES: ksync::Once<[[u64; 2]; KCMP_TYPES as usize]> = ksync::Once::new();

/// `kcmp_ptr`: 0 equal, 1 less, 2 greater, on the scrambled addresses compared as signed longs.
fn kcmp_ptr(a: usize, b: usize, ty: u32) -> i64 {
    let k = COOKIES.get_or_init(|| {
        let mut raw = [0u8; 16 * KCMP_TYPES as usize];
        proc::random_bytes(&mut raw);
        let mut k = [[0u64; 2]; KCMP_TYPES as usize];
        for (i, e) in k.iter_mut().enumerate() {
            let w = |o: usize| u64::from_le_bytes(raw[i * 16 + o..i * 16 + o + 8].try_into().unwrap_or([0; 8]));
            *e = [w(0), w(8) | (1 << 63) | 1];
        }
        k
    });
    let [x, m] = k[ty as usize % KCMP_TYPES as usize];
    let s = |v: usize| ((v as u64 ^ x).wrapping_mul(m)) as i64;
    let (t1, t2) = (s(a), s(b));
    (t1 < t2) as i64 | (((t1 > t2) as i64) << 1)
}
