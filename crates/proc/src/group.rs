//! The thread group (Linux `signal_struct` plus the group's ids and its list of threads): what lives from
//! spawn or fork until the parent collects the process, across every execve.

use crate::rlimit::Limits;
use crate::signal::{self, SigInfo, SignalState, To};
use crate::task::Thread;
use crate::{ExitStatus, Process};
use alloc::string::String;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

/// Interrupts the kernel threads a signal change woke (never the caller itself: it looks on its own way out).
pub(crate) fn kick(w: &signal::Wake) {
    let me = sched::current_id();
    for &k in &w.0 {
        if k != me {
            sched::kick_thread(k);
        }
    }
}
use ksync::{Completion, SpinLock, WaitQueue};

/// `leader_code` before the leader called exit(2).
const NO_CODE: u32 = u32::MAX;
/// How often `cpu_times` reads again when threads end under it (a bound, not a count of things).
const CPU_READ_TRIES: usize = 4;

/// The ids a new group starts with.
pub(crate) struct Ids {
    pub tgid: u32,
    pub ppid: u32,
    pub pgid: u32,
    pub sid: u32,
    pub session_leader: bool,
    /// The thread that made it (wait4's __WNOTHREAD looks at this; the parent's tid).
    pub creator_tid: u32,
    /// AB-13: the credentials it starts with (fork: a copy of the parent's; spawn: `SpawnOptions`).
    pub cred: Arc<crate::cred::Cred>,
}

pub struct Group {
    /// The process id (Linux tgid): the leader's tid, kept across a non-leader's execve.
    pub tgid: u32,
    /// The real uid, a copy of `cred.uid` readable without the lock (RLIMIT_NPROC counts threads per real
    /// uid under `proc.tasks`; si_uid).
    pub(crate) owner_uid: AtomicU32,
    /// AB-13: the group's credentials (`cred.rs`); replaced whole by `Process::change_cred`.
    pub(crate) cred: SpinLock<Arc<crate::cred::Cred>>,
    /// Parent pid; 0 = the kernel. An orphan gets init (pid 1) while it runs, else the kernel.
    ppid: AtomicU32,
    pgid: AtomicU32,
    sid: AtomicU32,
    session_leader: AtomicBool,
    /// The controlling terminal's token (0: none), kept by the tty owner (IN-4) through `job`.
    ctty: AtomicU64,
    creator_tid: AtomicU32,
    /// The signal the parent gets at the end (SIGCHLD for fork; clone's CSIGNAL; 0: none).
    exit_signal: AtomicU32,
    /// Linux PF_FORKNOEXEC cleared: setpgid on it from the parent is EACCES.
    did_exec: AtomicBool,
    pub(crate) class: sched::Class,
    /// The group's end (exit_group, a kill, a fault; or the last thread's exit(2)).
    status: SpinLock<Option<ExitStatus>>,
    /// The leader thread's exit(2) code while other threads run on (Linux `exit_code` of the leader).
    leader_code: AtomicU32,
    exited: AtomicBool,
    /// A sibling's execve is ending the other threads (`has_exited` then also asks about the caller).
    stopping: AtomicBool,
    /// The sched id of the thread running execve's de_thread (0: none).
    exec_by: AtomicU64,
    /// Every thread has ended (it is a zombie until its parent collects it).
    finished: AtomicBool,
    /// No thread runs its image any more and its memory and files are being given back (set first thing in
    /// the group's end, under the table lock; `mm_shared`/`fds_shared` skip such sharers).
    released: AtomicBool,
    /// Woken when a child ends and when this group is told to end (wait4, sleeps and vfork sleep on it).
    pub wq: WaitQueue,
    /// Counts child ends, so a waiter does not miss one between its look and its sleep.
    child_events: AtomicU64,
    /// The V2 fault message when a fault ended it.
    end_message: SpinLock<Option<String>>,
    go: Completion,
    done: Completion,
    /// Completed when this child execs or ends: a vfork parent waits for it (Linux `vfork_done`).
    pub(crate) vfork_done: Completion,
    /// Signal actions, the shared pending queue and every thread's mask and queue (AB-3, AB-6; `signal.rs`).
    pub signals: SignalState,
    /// AB-6: woken whenever a signal is queued for the group or one of its threads (signalfd readers and
    /// pollers, Linux `sighand->signalfd_wqh`).
    pub sig_poll: vfs::poll::PollQueue,
    /// AB-12: pidfd pollers (Linux `pid->wait_pidfd`), woken once when the last thread ended
    /// (`do_notify_pidfd`); readiness itself is `is_finished()`.
    pub(crate) exit_poll: vfs::poll::PollQueue,
    /// Resource limits (getrlimit/setrlimit/prlimit64).
    pub limits: Limits,
    /// Monotonic time of the spawn or fork (ns since boot; /proc start time).
    pub start_ns: u64,
    threads: SpinLock<Vec<Arc<Thread>>>,
    /// The first thread's kernel thread (`thread_id` once every thread ended).
    first_kthread: AtomicU64,
    image_ref: SpinLock<Weak<Process>>,
    /// SC-6 (AB-5 wiring): CPU time of the threads that ended and of the children collected by wait (Linux
    /// `signal_struct` utime/stime, cutime/cstime); live threads are read from `sched::cputime`.
    pub cpu: sched::cputime::ProcessTimes,
    /// AB-5 `PR_SET_CHILD_SUBREAPER` (Linux `signal->is_child_subreaper`): orphaned descendants come here instead
    /// of to init. Not inherited by fork, kept across execve.
    child_subreaper: AtomicBool,
    /// AB-6 fixer: on the timer service's list (`crate::itimer`; changed only under its list lock).
    pub(crate) timers_listed: AtomicBool,
    /// AB-7a: `/proc/<pid>/oom_score_adj` and its floor (Linux `signal->oom_score_adj{,_min}`; `procinfo.rs`),
    /// one word so the floor check and both stores are one step (Linux holds `oom_adj_mutex`).
    pub(crate) oom: AtomicU64,
    /// AB-7c: peak resident pages of the group's earlier images and, once it ended, of the last (Linux
    /// `signal->maxrss`, raised in exec_mmap and do_exit); `procinfo::maxrss_pages` adds the current image.
    pub(crate) maxrss: AtomicU64,
    /// AB-7c: the largest peak of the children collected by wait (Linux `signal->cmaxrss`, wait_task_zombie).
    pub(crate) cmaxrss: AtomicU64,
}

impl Group {
    pub(crate) fn new(ids: Ids, exit_signal: u32, class: sched::Class, signals: SignalState, limits: Limits, start_ns: u64) -> Arc<Group> {
        Arc::new(Group {
            tgid: ids.tgid,
            owner_uid: AtomicU32::new(ids.cred.uid),
            cred: SpinLock::named("proc.cred", ids.cred),
            ppid: AtomicU32::new(ids.ppid),
            pgid: AtomicU32::new(ids.pgid),
            sid: AtomicU32::new(ids.sid),
            session_leader: AtomicBool::new(ids.session_leader),
            ctty: AtomicU64::new(0),
            creator_tid: AtomicU32::new(ids.creator_tid),
            exit_signal: AtomicU32::new(exit_signal),
            did_exec: AtomicBool::new(false),
            class,
            status: SpinLock::named("proc.status", None),
            leader_code: AtomicU32::new(NO_CODE),
            exited: AtomicBool::new(false),
            stopping: AtomicBool::new(false),
            exec_by: AtomicU64::new(0),
            finished: AtomicBool::new(false),
            released: AtomicBool::new(false),
            wq: WaitQueue::new(),
            child_events: AtomicU64::new(0),
            end_message: SpinLock::named("proc.message", None),
            go: Completion::new(),
            done: Completion::new(),
            vfork_done: Completion::new(),
            signals,
            sig_poll: vfs::poll::PollQueue::new(),
            exit_poll: vfs::poll::PollQueue::new(),
            limits,
            start_ns,
            threads: SpinLock::named("proc.threads", Vec::new()),
            first_kthread: AtomicU64::new(0),
            image_ref: SpinLock::named("proc.gen", Weak::new()),
            cpu: sched::cputime::ProcessTimes::new(),
            child_subreaper: AtomicBool::new(false),
            timers_listed: AtomicBool::new(false),
            oom: AtomicU64::new(0),
            maxrss: AtomicU64::new(0),
            cmaxrss: AtomicU64::new(0),
        })
    }

    // ---- ids ------------------------------------------------------------------------------------------

    /// Parent pid (0: the kernel).
    pub fn ppid(&self) -> u32 {
        self.ppid.load(Ordering::Acquire)
    }
    pub(crate) fn set_ppid(&self, p: u32) {
        self.ppid.store(p, Ordering::Release);
    }
    pub fn pgid(&self) -> u32 {
        self.pgid.load(Ordering::Acquire)
    }
    pub(crate) fn set_pgid(&self, g: u32) {
        self.pgid.store(g, Ordering::Release);
    }
    /// Session id (the session leader's pid).
    pub fn sid(&self) -> u32 {
        self.sid.load(Ordering::Acquire)
    }
    pub fn is_session_leader(&self) -> bool {
        self.session_leader.load(Ordering::Acquire)
    }
    /// setsid: leader of a new session and group named after itself; no controlling terminal.
    pub(crate) fn make_session(&self) {
        self.sid.store(self.tgid, Ordering::Release);
        self.pgid.store(self.tgid, Ordering::Release);
        self.session_leader.store(true, Ordering::Release);
        self.ctty.store(0, Ordering::Release);
    }
    /// The controlling terminal's token (0: none). The tty owner (IN-4) chooses the token.
    pub fn ctty(&self) -> u64 {
        self.ctty.load(Ordering::Acquire)
    }
    pub fn set_ctty(&self, token: u64) {
        self.ctty.store(token, Ordering::Release);
    }
    pub(crate) fn creator_tid(&self) -> u32 {
        self.creator_tid.load(Ordering::Acquire)
    }
    /// The thread that made it ended (AB-5): another thread of the parent, or the new parent, takes it over
    /// (Linux reparents to `find_new_reaper`'s thread).
    pub(crate) fn set_creator_tid(&self, tid: u32) {
        self.creator_tid.store(tid, Ordering::Release);
    }
    /// The signal its parent gets when it ends (0: none).
    pub fn exit_signal(&self) -> u32 {
        self.exit_signal.load(Ordering::Acquire)
    }
    pub(crate) fn set_exit_signal(&self, s: u32) {
        self.exit_signal.store(s, Ordering::Release);
    }
    /// It ran execve since its fork.
    pub fn did_exec(&self) -> bool {
        self.did_exec.load(Ordering::Acquire)
    }
    pub(crate) fn mark_exec(&self) {
        self.did_exec.store(true, Ordering::Release);
        self.leader_code.store(NO_CODE, Ordering::Release);
    }

    // ---- images and threads ------------------------------------------------------------------------------

    /// The current image (None once the group was collected and every holder let go).
    pub fn image(&self) -> Option<Arc<Process>> {
        self.image_ref.lock().upgrade()
    }
    pub(crate) fn set_image(&self, p: &Arc<Process>) {
        *self.image_ref.lock() = Arc::downgrade(p);
    }
    /// The live threads.
    pub fn threads(&self) -> Vec<Arc<Thread>> {
        self.threads.lock().clone()
    }
    pub fn thread_count(&self) -> usize {
        self.threads.lock().len()
    }
    /// Adds a thread; refused (EINTR) when the group is ending or a sibling's execve is ending the others.
    /// AB-6: its part of the signal state starts with the mask its creator gave it (`Thread::start_mask`).
    pub(crate) fn add_thread(&self, t: &Arc<Thread>) -> Result<(), crate::SpawnError> {
        {
            let mut th = self.threads.lock();
            if self.exited.load(Ordering::Acquire) || self.dethreading() {
                return Err(crate::SpawnError::Io(uapi::errno::EINTR));
            }
            th.try_reserve(1).map_err(|_| crate::SpawnError::NoMemory)?;
            th.push(t.clone());
        }
        self.signals.raw(|s| s.add_thread(t.sig_key(), t.tid(), t.start_mask(), t.sig_flag(), t.kthread_cell()));
        Ok(())
    }
    /// Takes `t` out; true when it was the last one. AB-6: its signal part goes too (`exit_signals`); a stop it
    /// still owed may complete the group stop (returned: the parent is owed CLD_STOPPED).
    pub(crate) fn remove_thread(&self, t: &Thread) -> (bool, Option<signal::Cld>) {
        let last = {
            let mut th = self.threads.lock();
            if let Some(i) = th.iter().position(|x| core::ptr::eq(&**x, t)) {
                th.swap_remove(i);
            }
            th.is_empty()
        };
        let (wake, cld) = self.signals.raw(|s| s.remove_thread(t.sig_key()));
        kick(&wake);
        (last, cld)
    }
    pub(crate) fn note_kthread(&self, k: u64) {
        let _ = self.first_kthread.compare_exchange(0, k, Ordering::AcqRel, Ordering::Acquire);
    }
    /// A kernel thread running this group (the leader's while it lives; 0 before the first started).
    pub fn thread_id(&self) -> u64 {
        let th = self.threads.lock();
        th.iter().find(|t| t.tid() == self.tgid).or_else(|| th.first()).map(|t| t.kthread()).filter(|&k| k != 0).unwrap_or_else(|| self.first_kthread.load(Ordering::Acquire))
    }
    /// Interrupts every thread of the group except the caller, so one busy in user mode comes back into the
    /// kernel at once (Linux `signal_wake_up`).
    pub(crate) fn kick_threads(&self) {
        let me = sched::current_id();
        for t in self.threads.lock().iter() {
            let k = t.kthread();
            if k != me {
                sched::kick_thread(k);
            }
        }
    }
    /// CPU time of the whole group (Linux `thread_group_cputime`): its ended threads (`cpu`) and the live
    /// ones (`sched::cputime::of_thread`, the running slice included), never less than reported before.
    ///
    /// Consistency: an ending thread leaves `threads` before it adds its time (`exit::thread_ends`), so a
    /// reader that took the ended count before the list cannot count a thread twice; when the count moved
    /// while the live threads were read (a thread ended in between, possibly counted live and ended) the read
    /// is repeated (bounded; the last try may miss an ending thread's last slice, which `monotonic` hides).
    pub fn cpu_times(&self) -> sched::cputime::CpuTimes {
        use sched::cputime::{CpuTimes, of_thread};
        let mut t = CpuTimes::ZERO;
        for _ in 0..CPU_READ_TRIES {
            let (ended, n1) = self.cpu.snapshot();
            let live = self.threads().iter().filter_map(|th| of_thread(th.kthread())).fold(CpuTimes::ZERO, CpuTimes::plus);
            let (_, n2) = self.cpu.snapshot();
            t = live.plus(ended);
            if n1 == n2 {
                break;
            }
        }
        self.cpu.monotonic(t)
    }
    /// All CPU time of the group in ns (user + system).
    pub fn cpu_ns(&self) -> u64 {
        self.cpu_times().total_ns()
    }
    pub fn is_child_subreaper(&self) -> bool {
        self.child_subreaper.load(Ordering::Acquire)
    }
    pub fn set_child_subreaper(&self, on: bool) {
        self.child_subreaper.store(on, Ordering::Release);
    }
    pub(crate) fn begin_dethread(&self, by: u64) {
        self.exec_by.store(by, Ordering::Release);
        self.stopping.store(true, Ordering::Release);
    }
    pub(crate) fn end_dethread(&self) {
        self.stopping.store(false, Ordering::Release);
        self.exec_by.store(0, Ordering::Release);
    }
    pub(crate) fn dethreading(&self) -> bool {
        self.exec_by.load(Ordering::Acquire) != 0
    }

    // ---- the end -------------------------------------------------------------------------------------------

    /// Ends the whole group with exit status `code` (the low 8 bits, as `exit_group`). The first end wins.
    pub fn exit(&self, code: i32) {
        self.end_with(ExitStatus::Exited(code as u8));
    }
    /// Ends the group as if by signal `sig` (SIGKILL semantics: it cannot be caught). Threads sleeping in the
    /// kernel wake at once; one in user mode is interrupted.
    pub fn kill(&self, sig: u32) {
        if self.end_with(ExitStatus::Signaled(sig)) {
            klog::event!(Info, "proc", "kill", pid = self.tgid, signal = uapi::ab::signal_names::name(sig));
        }
    }
    /// Records the group's end and tells every thread; false when it had already ended.
    pub(crate) fn end_with(&self, s: ExitStatus) -> bool {
        let first = {
            let mut g = self.status.lock();
            if g.is_none() {
                *g = Some(s);
                true
            } else {
                false
            }
        };
        if first {
            self.exited.store(true, Ordering::Release);
            // FS-13: the pid records nothing more for a preload session (it may be given out again).
            vfs::preload::exited(self.tgid);
            self.signals.raw(|s| s.begin_exit());
            self.wq.wake_all();
            self.kick_threads();
        }
        // A process held at spawn must still run its loop once to end.
        self.go.complete();
        first
    }
    /// The last thread ended by exit(2): the group's status is the leader's code (Linux `wait_task_zombie`
    /// reads the leader's `exit_code` when no group exit happened).
    pub(crate) fn end_by_last_thread(&self, code: u32) {
        let leader = self.leader_code.load(Ordering::Acquire);
        let c = if leader != NO_CODE { leader } else { code };
        self.end_with(ExitStatus::Exited(c as u8));
    }
    pub(crate) fn set_leader_code(&self, code: u32) {
        self.leader_code.store(code, Ordering::Release);
    }
    /// The group was told to end (exit_group, kill, fault, the last thread's exit), or, during a sibling's
    /// execve, the calling thread was.
    pub fn has_exited(&self) -> bool {
        if self.exited.load(Ordering::Acquire) {
            return true;
        }
        self.stopping.load(Ordering::Acquire) && crate::task::current_thread().is_some_and(|t| t.is_of(self) && t.exit_requested())
    }
    pub(crate) fn group_exited(&self) -> bool {
        self.exited.load(Ordering::Acquire)
    }
    /// Every thread has ended.
    pub fn is_finished(&self) -> bool {
        self.finished.load(Ordering::Acquire)
    }
    pub(crate) fn mark_finished(&self) {
        self.released.store(true, Ordering::Release);
        self.finished.store(true, Ordering::Release);
    }
    /// No thread runs the image any more (the start of the group's end; `finished` follows).
    pub(crate) fn is_released(&self) -> bool {
        self.released.load(Ordering::Acquire)
    }
    pub(crate) fn mark_released(&self) {
        self.released.store(true, Ordering::Release);
    }
    pub(crate) fn complete_done(&self) {
        self.done.complete();
    }
    pub fn status(&self) -> Option<ExitStatus> {
        *self.status.lock()
    }
    /// Shell-style code: the exit status, or 128 + signal.
    pub fn exit_code(&self) -> Option<i32> {
        self.status().map(ExitStatus::shell_code)
    }
    /// The V2 fault message, when a fault ended the process.
    pub fn end_message(&self) -> Option<String> {
        self.end_message.lock().clone()
    }
    pub(crate) fn set_end_message(&self, m: String) {
        *self.end_message.lock() = Some(m);
    }
    /// Waits until every thread has ended; returns the shell-style exit code (None on timeout).
    pub fn wait(&self, timeout_ns: Option<u64>) -> Option<i32> {
        match timeout_ns {
            Some(t) => {
                if !self.done.wait_timeout(t) {
                    return None;
                }
            }
            None => self.done.wait(),
        }
        self.exit_code()
    }
    /// Lets a process spawned with `SpawnOptions::held` enter user mode.
    pub fn release(&self) {
        self.go.complete();
    }
    pub(crate) fn wait_go(&self) {
        self.go.wait();
    }
    /// Sleeps up to `ns` unless the process is told to end or (AB-6) the calling thread gets a signal to
    /// handle meanwhile; true when it was (the caller returns EINTR or a restart code).
    pub fn sleep_interruptible(&self, ns: u64) -> bool {
        self.wq.wait_until_timeout(|| self.signal_pending(), ns)
    }
    /// Linux `signal_pending(current)` for a thread of this group: the calling thread has a signal to look at
    /// (one it does not block, a stop to take part in) or the group is ending. A kernel thread calling for the
    /// process sees only the end.
    pub fn signal_pending(&self) -> bool {
        if self.has_exited() {
            return true;
        }
        crate::task::current_thread().is_some_and(|t| t.is_of(self) && t.signal_pending())
    }

    // ---- signals ----------------------------------------------------------------------------------------

    /// A signal from the kernel to the whole group (`SEND_SIG_PRIV`: SI_KERNEL), e.g. the terminal's SIGINT
    /// (IN-4) or a parent-death signal: its default action may end the group at once; ignored ones are
    /// dropped; blocked ones and ones with a handler stay pending until a thread takes them.
    pub fn send_signal(&self, sig: u32) -> signal::Sent {
        self.send_info(SigInfo::kernel(sig), To::Group, false).unwrap_or(signal::Sent::Dropped)
    }
    /// `send_signal_locked` with a siginfo, to the group or one of its threads (`To::Thread(sig_key)`). EAGAIN
    /// for a real-time signal with its own siginfo when RLIMIT_SIGPENDING is reached; ESRCH for a thread that
    /// is gone.
    pub fn send_info(&self, info: SigInfo, to: To, force: bool) -> Result<signal::Sent, vfs::Errno> {
        if self.is_finished() {
            return Ok(signal::Sent::Dropped);
        }
        let limit = self.limits.get(uapi::ab::rlimit::RLIMIT_SIGPENDING).map_or(u64::MAX, |l| l.cur);
        let out = self.signals.raw(|s| s.send(info, to, force, limit))?;
        Ok(self.after_send(info.signo(), out))
    }
    /// `force_sig_info_to_task` on thread `key` (faults, bad frames): see `Signals::force`.
    pub(crate) fn force_info(&self, key: u64, info: SigInfo, to_default: bool) -> signal::Sent {
        let limit = self.limits.get(uapi::ab::rlimit::RLIMIT_SIGPENDING).map_or(u64::MAX, |l| l.cur);
        match self.signals.raw(|s| s.force(key, info, to_default, limit)) {
            Ok(out) => self.after_send(info.signo(), out),
            Err(_) => signal::Sent::Dropped,
        }
    }
    pub(crate) fn after_send(&self, sig: u32, out: signal::Outcome) -> signal::Sent {
        if out.sent == signal::Sent::Fatal {
            self.end_by_signal(sig);
        }
        kick(&out.wake);
        if out.sent == signal::Sent::Pending && self.sig_poll.waiter_count() != 0 {
            self.sig_poll.wake_poll(vfs::poll::Events::IN);
        }
        if out.continued {
            self.wq.wake_all();
            klog::event!(Info, "proc", "signal.continued", pid = self.tgid);
        }
        out.sent
    }
    /// After a syscall changed the mask or an action outside the syscall exit (FS's temporary ppoll mask): a
    /// pending signal of the calling thread that is now deliverable and whose default action terminates ends
    /// the process at once. Handlers and stops wait for the delivery on the way back to user mode.
    pub fn check_signals(&self) {
        if !self.signals.has_unblocked_pending() {
            return;
        }
        if let Some(sig) = self.signals.with(|s| s.take_fatal()) {
            self.end_by_signal(sig);
        }
    }
    pub(crate) fn end_by_signal(&self, sig: u32) {
        if self.end_with(ExitStatus::Signaled(sig)) {
            klog::event!(Info, "proc", "signal.fatal", pid = self.tgid, signal = uapi::ab::signal_names::name(sig));
        }
    }

    // ---- children ---------------------------------------------------------------------------------------------

    pub(crate) fn child_event(&self) {
        self.child_events.fetch_add(1, Ordering::AcqRel);
        self.wq.wake_all();
    }
    pub(crate) fn child_events(&self) -> u64 {
        self.child_events.load(Ordering::Acquire)
    }
}
