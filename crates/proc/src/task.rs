//! Threads (Linux tasks): one kernel thread per user thread runs `UserContext::execute` in a loop and hands
//! syscalls to the `syscall` crate; user page faults go to the address space first (lazy fill), a fault it
//! cannot serve ends the group with the V2 message (R3).
//!
//! `TASKS` keeps every user thread twice: by tid from its creation (`find_thread`, the id allocator; a clone's
//! caller can tkill the new thread at once) and by the scheduler's thread id once it runs (`current_thread`,
//! `current`). tids come from the same id space as pids (`crate::next_id`).

use crate::group::Group;
use crate::signal::AltStack;
use crate::{Process, SpawnError};
use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use hal::entry::{ReturnReason, UserContext};
use hal::xsave::RegDump;
use ksync::{Completion, OnceRef, SpinLock};

/// Keys of the threads' parts of their groups' signal state (unique for the kernel's life; 0 = none).
static SIG_KEYS: AtomicU64 = AtomicU64::new(1);

/// What a restarted `restart_syscall` continues (Linux `restart_block`: sleeps and timed futex waits).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Restart {
    /// nanosleep / relative clock_nanosleep: the monotonic deadline and where the time left goes.
    Sleep { deadline: u64, rmtp: u64 },
    /// A timed FUTEX_WAIT / FUTEX_WAIT_BITSET (`futex_wait_restart`, kernel/futex/waitwake.c): the same wait
    /// to the same absolute deadline (on CLOCK_REALTIME when `realtime`).
    Futex { uaddr: u64, shared: bool, val: u32, bitset: u32, deadline: u64, realtime: bool },
}

pub struct Thread {
    tid: AtomicU32,
    /// The scheduler's id of the kernel thread that runs it (0 before it started); shared with its signal
    /// part (kicks).
    kthread: Arc<AtomicU64>,
    /// AB-6: the key of its part of the group's signal state, and its TIF_SIGPENDING flag (shared with it).
    sig_key: u64,
    sigflag: Arc<AtomicBool>,
    /// The blocked mask it starts with (its creator's; 0 for programs the kernel starts).
    start_mask: AtomicU64,
    /// trapno, error code and cr2 of the last fault signal (Linux `thread.trap_nr/error_code/cr2`; kept
    /// stale like Linux, every frame shows them).
    trap: SpinLock<[u64; 3]>,
    /// The restart block of an interrupted sleep (`restart_block`).
    restart: SpinLock<Option<Restart>>,
    /// The V2 fault message (R3) of a fault signal not yet taken, with its signal: logged and kept as the
    /// end message when the fault's default action ends the group.
    fault_msg: SpinLock<Option<crate::deliver::FaultNote>>,
    kt: SpinLock<Option<Arc<sched::Thread>>>,
    group: Arc<Group>,
    /// `set_tid_address` / CLONE_CHILD_CLEARTID: cleared and futex-woken when the thread ends.
    pub clear_child_tid: AtomicU64,
    /// `set_robust_list`: the head SC-5's futex code walks when the thread ends.
    pub robust_list: AtomicU64,
    /// Told to end alone: its own exit(2), or a sibling's execve.
    exit_req: AtomicBool,
    /// Its exit(2) code (low 8 bits).
    code: AtomicU32,
    /// Set by execve: the run loop takes the group's new image.
    pub(crate) new_image: AtomicBool,
    /// `sigaltstack`: per thread as on Linux (`current->sas_ss_*`).
    alt: SpinLock<AltStack>,
    done: Completion,
    /// AB-5: what prctl keeps per task (Linux `task_struct` fields; `crate::prctl_state`).
    pub(crate) pr: crate::prctl_state::TaskPrctl,
    /// Completed by the creator once it copied its scheduling attributes to this thread (fork/clone); the
    /// thread waits for it before its first user instruction.
    attrs_ready: Completion,
}

impl Thread {
    pub(crate) fn new(tid: u32, group: &Arc<Group>) -> Arc<Thread> {
        Arc::new(Thread {
            tid: AtomicU32::new(tid),
            kthread: Arc::new(AtomicU64::new(0)),
            sig_key: SIG_KEYS.fetch_add(1, Ordering::Relaxed),
            sigflag: Arc::new(AtomicBool::new(false)),
            start_mask: AtomicU64::new(0),
            trap: SpinLock::named("proc.trap", [0; 3]),
            restart: SpinLock::named("proc.restart", None),
            fault_msg: SpinLock::named("proc.faultmsg", None),
            kt: SpinLock::named("proc.kt", None),
            group: group.clone(),
            clear_child_tid: AtomicU64::new(0),
            robust_list: AtomicU64::new(0),
            exit_req: AtomicBool::new(false),
            code: AtomicU32::new(0),
            new_image: AtomicBool::new(false),
            alt: SpinLock::named("proc.altstack", AltStack::DISABLED),
            done: Completion::new(),
            pr: crate::prctl_state::TaskPrctl::new(),
            attrs_ready: Completion::new(),
        })
    }
    /// AB-5: the prctl state of this task (comm, pdeath signal, no_new_privs, timer slack, ...).
    pub fn prctl(&self) -> &crate::prctl_state::TaskPrctl {
        &self.pr
    }
    pub fn tid(&self) -> u32 {
        self.tid.load(Ordering::Acquire)
    }
    /// AB-6: the key of this thread's signal part (`signal::To::Thread`).
    pub fn sig_key(&self) -> u64 {
        self.sig_key
    }
    pub(crate) fn sig_flag(&self) -> Arc<AtomicBool> {
        self.sigflag.clone()
    }
    pub(crate) fn kthread_cell(&self) -> Arc<AtomicU64> {
        self.kthread.clone()
    }
    pub(crate) fn start_mask(&self) -> u64 {
        self.start_mask.load(Ordering::Acquire)
    }
    /// The mask the thread starts with (set by its creator before it starts).
    pub(crate) fn set_start_mask(&self, m: u64) {
        self.start_mask.store(m, Ordering::Release);
    }
    /// Linux `signal_pending()` for this thread: a signal it does not block, a stop to take part in, or the
    /// group's end. Lock-free (the flag `Signals::recalc` keeps).
    pub fn signal_pending(&self) -> bool {
        self.sigflag.load(Ordering::Acquire) || self.group.group_exited()
    }
    /// Its blocked mask.
    pub fn blocked(&self) -> u64 {
        self.group.signals.raw(|s| s.blocked_of(self.sig_key))
    }
    /// A signal to this thread from the kernel on behalf of itself (`send_sig(sig, current, 0)`: SI_USER
    /// with its own pid and uid; FS-3's SIGPIPE).
    pub fn send_signal(&self, sig: u32) -> crate::signal::Sent {
        let info = crate::signal::SigInfo::user(sig, uapi::ab::siginfo::SI_USER, self.group.tgid, self.group.real_uid());
        self.group.send_info(info, crate::signal::To::Thread(self.sig_key), false).unwrap_or(crate::signal::Sent::Dropped)
    }
    pub(crate) fn set_trap(&self, trapno: u64, err: u64, cr2: u64) {
        *self.trap.lock() = [trapno, err, cr2];
    }
    pub(crate) fn trap(&self) -> [u64; 3] {
        *self.trap.lock()
    }
    pub(crate) fn set_restart(&self, r: Option<Restart>) {
        *self.restart.lock() = r;
    }
    /// Takes the restart block (`restart_syscall`; sigreturn drops it).
    pub fn take_restart(&self) -> Option<Restart> {
        self.restart.lock().take()
    }
    /// An interrupted sleep's restart block (clock.rs).
    pub fn set_sleep_restart(&self, deadline: u64, rmtp: u64) {
        self.set_restart(Some(Restart::Sleep { deadline, rmtp }));
    }
    /// Any restart block (the syscall layer's `set_restart_fn`).
    pub fn set_restart_block(&self, r: Restart) {
        self.set_restart(Some(r));
    }
    pub(crate) fn set_fault_msg(&self, n: crate::deliver::FaultNote) {
        // The old note (if any) is dropped outside the lock: its String frees heap memory.
        let old = self.fault_msg.lock().replace(n);
        drop(old);
    }
    pub(crate) fn take_fault_msg(&self) -> Option<crate::deliver::FaultNote> {
        self.fault_msg.lock().take()
    }
    /// A non-leader's execve takes the leader's id (linux-6.6/fs/exec.c `exchange_tids`).
    pub(crate) fn set_tid(&self, tid: u32) {
        self.tid.store(tid, Ordering::Release);
    }
    pub fn kthread(&self) -> u64 {
        self.kthread.load(Ordering::Acquire)
    }
    pub fn group(&self) -> &Arc<Group> {
        &self.group
    }
    pub(crate) fn is_of(&self, g: &Group) -> bool {
        core::ptr::eq(&*self.group, g)
    }
    pub fn is_leader(&self) -> bool {
        self.tid() == self.group.tgid
    }
    pub(crate) fn exit_requested(&self) -> bool {
        self.exit_req.load(Ordering::Acquire)
    }
    /// Ends this thread alone (exit(2)); the loop stops after the syscall.
    pub fn exit_thread(&self, code: u32) {
        self.code.store(code & 0xFF, Ordering::Release);
        self.exit_req.store(true, Ordering::Release);
    }
    pub(crate) fn code(&self) -> u32 {
        self.code.load(Ordering::Acquire)
    }
    /// A sibling's execve ends it: interrupt it wherever it is.
    pub(crate) fn request_stop(&self) {
        self.exit_req.store(true, Ordering::Release);
        sched::kick_thread(self.kthread());
    }
    /// The run loop ends: the group ended or this thread was told to.
    pub fn should_stop(&self) -> bool {
        self.group.group_exited() || self.exit_requested()
    }
    pub fn cpu_ns(&self) -> u64 {
        self.kt.lock().as_ref().map_or(0, |k| k.runtime_ns())
    }
    /// `do_sigaltstack` on this thread's alternate stack (`sp`: the caller's user stack pointer).
    pub fn sigaltstack(&self, new: Option<AltStack>, sp: u64) -> Result<AltStack, vfs::Errno> {
        self.alt.lock().change(new, sp)
    }
    pub fn alt_stack(&self) -> AltStack {
        *self.alt.lock()
    }
    pub(crate) fn set_alt_stack(&self, a: AltStack) {
        *self.alt.lock() = a;
    }
    /// execve forgets it (`begin_new_exec`).
    pub(crate) fn reset_alt_stack(&self) {
        self.set_alt_stack(AltStack::DISABLED);
    }
    pub(crate) fn wait_done(&self, ns: u64) -> bool {
        self.done.wait_timeout(ns)
    }
    pub(crate) fn complete_done(&self) {
        self.done.complete();
    }
    fn set_kthread(&self, id: u64, kt: Option<Arc<sched::Thread>>) {
        self.kthread.store(id, Ordering::Release);
        if let Some(k) = kt {
            *self.kt.lock() = Some(k);
        }
    }
}

struct Tasks {
    by_tid: BTreeMap<u32, Arc<Thread>>,
    by_kthread: BTreeMap<u64, Arc<Thread>>,
}

static TASKS: SpinLock<Tasks> = SpinLock::named("proc.tasks", Tasks { by_tid: BTreeMap::new(), by_kthread: BTreeMap::new() });

/// The user thread the calling kernel thread runs (None in kernel threads).
pub fn current_thread() -> Option<Arc<Thread>> {
    let k = sched::current_id();
    if k == 0 {
        return None;
    }
    TASKS.lock().by_kthread.get(&k).cloned()
}

/// The live thread with id `tid` (tkill, tgkill, kill of a thread id).
pub fn find_thread(tid: u32) -> Option<Arc<Thread>> {
    TASKS.lock().by_tid.get(&tid).cloned()
}

/// Whether a live thread has id `id` (the id allocator skips them).
pub(crate) fn tid_in_use(id: u32) -> bool {
    TASKS.lock().by_tid.contains_key(&id)
}

/// How many user threads are registered (the leak ktest compares it with its start).
pub(crate) fn thread_total() -> usize {
    TASKS.lock().by_tid.len()
}

/// A new thread is known by its tid from now on.
fn register(t: &Arc<Thread>) {
    TASKS.lock().by_tid.insert(t.tid(), t.clone());
}

fn tasks_insert(k: u64, t: &Arc<Thread>) {
    TASKS.lock().by_kthread.insert(k, t.clone());
}

/// The thread leaves both maps (its end).
pub(crate) fn unregister(t: &Thread) {
    let mut g = TASKS.lock();
    if g.by_tid.get(&t.tid()).is_some_and(|x| core::ptr::eq(&**x, t)) {
        g.by_tid.remove(&t.tid());
    }
    let k = t.kthread();
    if g.by_kthread.get(&k).is_some_and(|x| core::ptr::eq(&**x, t)) {
        g.by_kthread.remove(&k);
    }
}

/// execve by a non-leader: the thread takes the leader's id.
pub(crate) fn retid(t: &Arc<Thread>, tid: u32) {
    let mut g = TASKS.lock();
    if g.by_tid.get(&t.tid()).is_some_and(|x| Arc::ptr_eq(x, t)) {
        g.by_tid.remove(&t.tid());
    }
    t.set_tid(tid);
    g.by_tid.insert(tid, t.clone());
}

/// SC-5's thread exit protocol (Linux `mm_release`/`exit_robust_list`): walk the robust list at `robust_head`,
/// and when others still see the memory (`mm_shared`) zero the CLONE_CHILD_CLEARTID word and wake one waiter.
/// It must run in the ending thread with the image's address space loaded. `syscall::init` installs
/// `syscall::futex::exit_release` (`proc` sits below `syscall`).
pub type ExitRelease = fn(p: &Arc<Process>, tid: u32, robust_head: u64, clear_child_tid: u64, mm_shared: bool);

static EXIT_RELEASE: OnceRef<ExitRelease> = OnceRef::new();
static EXIT_RELEASE_SLOT: ksync::Once<ExitRelease> = ksync::Once::new();

/// Installed once by the `syscall` crate.
pub fn set_exit_release(f: ExitRelease) {
    let r: &'static ExitRelease = EXIT_RELEASE_SLOT.get_or_init(|| f);
    EXIT_RELEASE.set(r);
}

/// Linux `mm_release` for thread `t` leaving image `p`'s address space (its end, or an execve), in `t` itself
/// while that address space is loaded. Without SC's hook the tid word is still zeroed (a waiter that polls sees
/// it) but nobody is woken and no robust list is walked.
pub(crate) fn release_tid_words(t: &Thread, p: &Arc<Process>, others_see_it: bool) {
    let head = t.robust_list.swap(0, Ordering::AcqRel);
    let ctid = t.clear_child_tid.swap(0, Ordering::AcqRel);
    if head == 0 && ctid == 0 {
        return;
    }
    match EXIT_RELEASE.get() {
        Some(f) => f(p, t.tid(), head, ctid, others_see_it),
        None => {
            if ctid != 0 && others_see_it {
                // A fault is ignored, as Linux's `put_user` there.
                let _ = p.mm.copy_to_user(ctid, &0u32.to_le_bytes());
            }
        }
    }
}

/// Starts thread `t` of `p`'s group in user mode at `ctx`. `fpu`: the creator's vector registers (fork and
/// clone copy them, Linux `fpu_clone`); `child_settid`: CLONE_CHILD_SETTID's address in the new memory.
/// `attrs_from`: the creator's kernel thread (fork and clone: its nice, policy and affinity go to the new thread
/// before it runs user code, SC-6 `copy_attrs`; 0 for programs the kernel starts).
pub(crate) fn start(p: &Arc<Process>, t: Arc<Thread>, ctx: UserContext, fpu: Option<RegDump>, child_settid: u64, attrs_from: u64) -> Result<(), SpawnError> {
    p.group().add_thread(&t)?;
    register(&t);
    let tc = t.clone();
    // SC-7a: the thread joins its process's group (the tgid), so an Interactive grant to the process (the
    // program in front, B4) reaches it from its first instruction.
    match sched::spawn_in_group(&p.name, p.group().class, p.pid as u64, move || run(tc, ctx, fpu, child_settid, attrs_from != 0)) {
        Ok(h) => {
            // Only the thread itself enters `TASKS` (in `run`): a spawner inserting it could put back one that
            // already ended. Kicks need only the id.
            t.set_kthread(h.id(), Some(h.thread().clone()));
            if attrs_from != 0 {
                // SC-6: nice, policy and affinity of the creator (Linux `sched_fork` + the mask `dup_task_struct`
                // copies) are the child's before fork/clone returns and before the child runs user code.
                if let Err(e) = sched::attr::copy_attrs(attrs_from, h.id()) {
                    klog::event!(Debug, "proc", "clone.attrs_lost", tid = t.tid(), from = attrs_from, error = alloc::format!("{e:?}").as_str());
                }
                // FS-12: the creator's I/O priority (Linux `__copy_io`).
                block::prio::inherit(attrs_from, h.id());
                t.attrs_ready.complete();
            }
            Ok(())
        }
        Err(_) => {
            unregister(&t);
            p.group().remove_thread(&t);
            Err(SpawnError::NoMemory)
        }
    }
}

fn run(t: Arc<Thread>, mut ctx: UserContext, fpu: Option<RegDump>, child_settid: u64, copied_attrs: bool) {
    let k = sched::current_id();
    if copied_attrs {
        // The creator completes it right after spawning this thread (bounded: it never waits for anything).
        let _ = t.attrs_ready.wait_timeout(1_000_000_000);
    }
    t.set_kthread(k, sched::current_thread());
    tasks_insert(k, &t);
    t.group.note_kthread(k);
    // Held at spawn: wait for `release` (or a kill). A Normal-class thread may wait for that (B4).
    t.group.wait_go();
    let Some(mut p) = t.group.image() else {
        crate::exit::thread_ends(&t);
        return;
    };
    p.mm.activate();
    if let Some(d) = &fpu {
        hal::xsave::load_user_regs(d);
    }
    if child_settid != 0 {
        // Linux `schedule_tail`: in the child's own memory, a fault ignored.
        let _ = p.mm.copy_to_user(child_settid, &t.tid().to_le_bytes());
    }
    // A thread that starts with something to do (a group stop it joined) does it before its first instruction
    // (Linux: `ret_from_fork` → `syscall_exit_to_user_mode`).
    crate::deliver::exit_to_user(&t, &mut ctx, None);
    // This thread's own run of faults on already valid pages (threads racing for a page never add up).
    let mut spurious = mm::fault::SpuriousRun::default();
    while !t.should_stop() {
        // Interrupts stay on while the program runs (AB-1). `execute_or_stop` handles them and comes back here
        // (None) when the user-IRQ hook asks or none is installed: the loop condition sees an end, and AB-6's
        // delivery a signal.
        // SC-6: the time between the two marks is the program's user time, everything else system time.
        sched::cputime::user_enter();
        let r = ctx.execute_or_stop(true);
        sched::cputime::user_exit();
        let Some(reason) = r else {
            sched::cond_resched();
            crate::deliver::exit_to_user(&t, &mut ctx, None);
            continue;
        };
        match reason {
            ReturnReason::Syscall => {
                // The dispatcher delivers signals on the syscall's way out (it knows the restart answer).
                match crate::syscall_handler() {
                    Some(h) => h(&p, &t, &mut ctx),
                    None => ctx.set_syscall_return(-uapi::errno::ENOSYS),
                }
                if t.new_image.swap(false, Ordering::AcqRel)
                    && let Some(n) = t.group.image()
                {
                    p = n;
                }
            }
            ReturnReason::Trap { vector: 14, error, cr2 } => {
                // MM-5a: a page the program may use but never touched gets its frame here.
                if let Err(why) = p.mm.handle_thread_fault(cr2, error, &mut spurious) {
                    crate::deliver::page_fault(&p, &t, &ctx, error, cr2, why);
                    crate::deliver::exit_to_user(&t, &mut ctx, None);
                }
            }
            ReturnReason::Trap { vector, error, .. } => {
                crate::deliver::trap(&p, &t, &ctx, vector, error);
                crate::deliver::exit_to_user(&t, &mut ctx, None);
            }
        }
        // SC-3: a program making syscalls gives way here.
        sched::cond_resched();
    }
    drop(p);
    crate::exit::thread_ends(&t);
}

/// Live threads whose group runs as `uid` (Linux counts RLIMIT_NPROC in tasks per real uid).
pub(crate) fn threads_of_uid(uid: u32) -> u64 {
    TASKS.lock().by_tid.values().filter(|t| t.group.real_uid() == uid).count() as u64
}
