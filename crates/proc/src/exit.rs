//! The end of a thread and of a thread group, and how parents collect children (linux-6.6/kernel/exit.c:
//! `do_exit`, `exit_notify`, `forget_original_parent`, `do_wait`, `wait_task_zombie`; kernel/signal.c
//! `do_notify_parent`).

use crate::group::Group;
use crate::signal::{SA_NOCLDWAIT, SigInfo, To};
use crate::task::{self, Thread};
use crate::wait::Select;
use crate::{ExitStatus, Process, TABLE};
use alloc::sync::Arc;
use alloc::vec::Vec;
use ksync::SpinLock;
use mm::AddressSpace;
use uapi::ab::signal::SIGCHLD;
use uapi::ab::sigabi::{SIG_DFL, SIG_IGN};
use vfs::Errno;

/// A thread's run loop ended (its exit(2), the group's end, a sibling's execve). The last thread ends the
/// group. The group's current image is used, not the one the loop held (an execve may have just replaced it).
pub(crate) fn thread_ends(t: &Arc<Thread>) {
    let g = t.group();
    // SC-6: off the live list first, then the time goes to the group (a reader never counts it twice; see
    // `Group::cpu_times`), before anyone can collect the group.
    let (last, cld) = g.remove_thread(t);
    g.cpu.thread_ended(sched::cputime::current());
    if let Some(c) = cld {
        // Its end completed a group stop the others had already joined (`exit_signals`).
        crate::deliver::notify_parent_cld(g, c);
    }
    if !last && !g.group_exited() {
        // Linux `forget_original_parent` for a thread that is not the last: its children stay the group's
        // (another thread takes them over, `find_alive_thread`), but their parent-death signals go now.
        hand_over_children(g, t);
    }
    let image = g.image();
    let p = image.as_ref();
    if let Some(p) = p {
        // Linux `mm_release`: the CLONE_CHILD_CLEARTID word matters only to someone who still sees the memory.
        let others = !last || crate::mm_shared(&p.mm, p.pid);
        task::release_tid_words(t, p, others);
    }
    if t.is_leader() && !g.group_exited() {
        // The leader went first: its exit(2) code is the group's unless a group exit comes (Linux keeps the
        // leader as a zombie thread for this).
        g.set_leader_code(t.code());
    }
    task::unregister(t);
    if last {
        if !g.group_exited() {
            g.end_by_last_thread(t.code());
        }
        finish_group(g, p);
    }
    t.complete_done();
}

/// Linux `exit_mm` and `exit_files` for an image nobody runs any more: the memory and the open files go now
/// (a pipe's reader sees EOF when its writer ends, not when the writer's parent collects it). Kept when another
/// live process shares them (a vfork parent, CLONE_VM or CLONE_FILES without CLONE_THREAD) or the kernel holds
/// the memory (`MmPin`).
pub(crate) fn release_image(p: &Arc<Process>) {
    // The exe's deny-write goes with the memory (Linux `exit_mm` → `mmput` → `set_mm_exe_file(mm, NULL)`);
    // dropped outside the lock (it may hold the inode's last reference).
    let deny = p.image_info.lock().deny.take();
    drop(deny);
    release_memory(&p.mm, p.pid);
    if !crate::fds_shared(&p.fds, p.pid) {
        // FS-3 CR1: every fd at once (reserved slots included), Linux `exit_files`.
        p.fds.close_all();
        // AB-5: the table's POSIX record locks go with its files (`filp_close` → `locks_remove_posix`).
        crate::locks::release_owner(&p.fds);
    }
}

/// The parent-death signals of `children`' threads (PR_SET_PDEATHSIG), sent to each child's group
/// (`group_send_sig_info(t->pdeath_signal, SEND_SIG_NOINFO, .., PIDTYPE_TGID)`: SI_USER from the ending
/// parent `from`). Outside every proc lock.
fn send_pdeath(from: &Group, children: &[Arc<Process>]) {
    for c in children {
        for th in c.threads() {
            let sig = th.prctl().pdeath_signal();
            if sig != 0 && !c.is_finished() {
                klog::event!(Debug, "proc", "pdeath", pid = c.pid, tid = th.tid(), signal = sig);
                let _ = c.send_info(SigInfo::user(sig, uapi::ab::siginfo::SI_USER, from.tgid, from.real_uid()), To::Group, false);
            }
        }
    }
}

/// Thread `t` of `g` ended while others run on: the children it made are taken over by a live thread of the
/// group (`find_alive_thread`), and their parent-death signals are sent (Linux sends them on every reparenting,
/// also within the group).
fn hand_over_children(g: &Arc<Group>, t: &Thread) {
    let heir = g.threads().first().map_or(g.tgid, |x| x.tid());
    let mine: Vec<Arc<Process>> = {
        let table = TABLE.lock();
        let mut v = Vec::new();
        for c in table.iter().filter(|c| c.ppid() == g.tgid && c.creator_tid() == t.tid()) {
            c.set_creator_tid(heir);
            if v.try_reserve(1).is_ok() {
                v.push(c.clone());
            }
        }
        v
    };
    send_pdeath(g, &mine);
    // Dropped outside the table lock (a child's image may be the last reference to its memory).
    drop(mine);
}

/// Linux `find_new_reaper` past the dying group's own threads: the nearest ancestor that is a child subreaper
/// (PR_SET_CHILD_SUBREAPER) and still runs, below init. Walks at most the table's length (a bound against a
/// parent cycle, which the table never holds).
fn find_subreaper(table: &[Arc<Process>], g: &Group) -> Option<Arc<Process>> {
    let mut pid = g.ppid();
    for _ in 0..table.len() {
        if pid == 0 || pid == 1 {
            return None;
        }
        let q = table.iter().find(|q| q.pid == pid)?;
        if q.is_child_subreaper() && !q.has_exited() && q.pid != g.tgid {
            return Some(q.clone());
        }
        pid = q.ppid();
    }
    None
}

/// The memory of an image nobody runs any more goes, unless a live process other than `except` or a pin
/// still uses it.
pub(crate) fn release_memory(mm: &Arc<AddressSpace>, except: u32) {
    if !crate::mm_shared(mm, except) && !pinned(mm) {
        let _ = mm.unmap(0, hal::USER_TOP);
    }
}

static PINS: SpinLock<Vec<u64>> = SpinLock::named("proc.pins", Vec::new());

fn pinned(mm: &AddressSpace) -> bool {
    PINS.lock().contains(&mm.root())
}

/// Keeps a process's memory from being given back at its end while the kernel still works in it (Linux
/// `mmget`/`mmput`: the memory goes with its last user, not with the task). When the last pin goes and no live
/// process uses the memory any more, it is given back then.
pub struct MmPin {
    mm: Arc<AddressSpace>,
}

/// Pins the memory of `p`'s image.
pub fn pin_mm(p: &Process) -> MmPin {
    PINS.lock().push(p.mm.root());
    MmPin { mm: p.mm.clone() }
}

impl Drop for MmPin {
    fn drop(&mut self) {
        let root = self.mm.root();
        let still = {
            let mut v = PINS.lock();
            if let Some(i) = v.iter().position(|&r| r == root) {
                v.swap_remove(i);
            }
            v.contains(&root)
        };
        if !still && !crate::mm_shared(&self.mm, 0) {
            let _ = self.mm.unmap(0, hal::USER_TOP);
        }
    }
}

/// Whether the parent's SIGCHLD disposition reaps the child at once (Linux `do_notify_parent`: SIG_IGN or
/// SA_NOCLDWAIT, for children that report with SIGCHLD), and whether the signal is sent at all.
fn parent_wants(parent: &Process, exit_signal: u32) -> (bool, bool) {
    if exit_signal == 0 {
        return (false, false);
    }
    let a = parent.signals.with(|s| s.action(exit_signal));
    if exit_signal != SIGCHLD {
        // Init never gets a signal it has no handler for (SIGNAL_UNKILLABLE, `sig_task_ignored`).
        return (false, !(parent.pid == 1 && a.handler == SIG_DFL));
    }
    let autoreap = a.handler == SIG_IGN || a.flags & SA_NOCLDWAIT != 0;
    (autoreap, a.handler != SIG_IGN)
}

/// The last thread of `g` ended: children go to the reaper, the parent learns about it (signal and wait4),
/// kernel-parented and auto-reaped groups leave the table. Nothing here allocates while a lock is held: the
/// entries that leave the table are taken out and dropped one at a time outside it.
fn finish_group(g: &Arc<Group>, p: Option<&Arc<Process>>) {
    let st = g.status().unwrap_or(ExitStatus::Exited(0));
    klog::event!(Info, "proc", "exit", pid = g.tgid, program = p.map_or("?", |p| p.name.as_str()), code = st.shell_code(), status = st.describe().as_str());
    // Marked under the table lock before looking for sharers: of two CLONE_VM/CLONE_FILES processes ending at
    // once, the later look sees the earlier one released and gives the memory and files back.
    {
        let _t = TABLE.lock();
        g.mark_released();
    }
    if let Some(p) = p {
        release_image(p);
    }
    // A vfork parent sharing the memory may go on (Linux `mm_release` → `complete_vfork_done`).
    g.vfork_done.complete();
    let (parent, reaper, orphans) = {
        let t = TABLE.lock();
        // Orphans go to the nearest child subreaper above (AB-5, PR_SET_CHILD_SUBREAPER), else to init (pid 1)
        // while it lives, as Linux's `find_new_reaper`/`find_child_reaper` (linux-6.6/kernel/exit.c:583, :623,
        // :688); otherwise to the kernel, and the sweep below collects the zombies among them. A reparented
        // child reports with SIGCHLD from now on (`reparent_leader`).
        let init = t.iter().find(|q| q.pid == 1 && q.pid != g.tgid && !q.has_exited()).cloned();
        let new_parent = find_subreaper(&t, g).or(init);
        let to = new_parent.as_ref().map_or(0, |i| i.pid);
        let mut zombie_for_reaper: Option<Arc<Process>> = None;
        let mut orphans = Vec::new();
        for c in t.iter().filter(|c| c.ppid() == g.tgid) {
            c.set_ppid(to);
            c.set_creator_tid(to);
            if c.exit_signal() != 0 {
                c.set_exit_signal(SIGCHLD);
            }
            if to != 0 && c.is_finished() && zombie_for_reaper.is_none() {
                zombie_for_reaper = Some(c.clone());
            }
            if orphans.try_reserve(1).is_ok() {
                orphans.push(c.clone());
            }
        }
        g.mark_finished();
        let ppid = g.ppid();
        (if ppid != 0 { t.iter().find(|q| q.pid == ppid).cloned() } else { None }, new_parent.zip(zombie_for_reaper), orphans)
    };
    // AB-12: pidfds of this process turn readable (Linux `do_notify_pidfd` from `do_notify_parent`); outside
    // the table lock, before the parent hears of it.
    if g.exit_poll.waiter_count() != 0 {
        g.exit_poll.wake_poll(vfs::poll::Events::IN | vfs::poll::Events::RDNORM);
    }
    // AB-5: every reparented child's parent-death signals (`forget_original_parent`).
    send_pdeath(g, &orphans);
    // AB-6: process groups this end orphans get SIGHUP + SIGCONT when they hold stopped members (POSIX
    // 3.2.2.2, `kill_orphaned_pgrp`): this group's own, and those of the children just handed over.
    orphaned_pgrps(g, parent.as_ref(), &orphans);
    drop(orphans);
    let mut autoreap = false;
    if let Some(pp) = &parent {
        let sig = g.exit_signal();
        let (reap, send) = parent_wants(pp, sig);
        autoreap = reap;
        if send && !pp.is_finished() {
            let _ = pp.send_info(child_info(g, sig), To::Group, false);
        }
    }
    // Every ended process without a parent leaves the table: this one when its parent is the kernel, gone, or
    // reaps it automatically, and zombies just orphaned to the kernel (nobody else can collect them). Outside
    // the lock: dropping a process frees its address space.
    loop {
        let gone = {
            let mut t = TABLE.lock();
            let i = t.iter().position(|q| q.is_finished() && (q.ppid() == 0 || (q.pid == g.tgid && (parent.is_none() || autoreap))));
            i.map(|i| t.swap_remove(i))
        };
        match gone {
            Some(x) => drop(x),
            None => break,
        }
    }
    if let Some(pp) = parent {
        pp.child_event();
    }
    // The reaper's wait4(-1) collects the zombies it was just given; it hears of them as a parent hears of an
    // ending child (`reparent_leader` → `do_notify_parent`).
    if let Some((r, z)) = reaper {
        if parent_wants(&r, SIGCHLD).1 && !r.is_finished() {
            let _ = r.send_info(child_info(&z, SIGCHLD), To::Group, false);
        }
        r.child_event();
    }
    g.complete_done();
}

/// The SIGCHLD (or clone's exit signal) siginfo of ended group `g` (`do_notify_parent`): CLD_EXITED with the
/// exit status, CLD_KILLED with the signal (V2 dumps no core: never CLD_DUMPED), the group's CPU time in
/// clock ticks.
fn child_info(g: &Group, sig: u32) -> SigInfo {
    use uapi::ab::siginfo::{CLD_EXITED, CLD_KILLED};
    let (code, status) = match g.status() {
        Some(ExitStatus::Signaled(s)) => (CLD_KILLED, s as i32),
        Some(ExitStatus::Exited(c)) => (CLD_EXITED, c as i32),
        None => (CLD_EXITED, 0),
    };
    let t = g.cpu_times();
    SigInfo::chld(sig, code, g.tgid, g.real_uid(), status, crate::deliver::ticks(t.user_ns), crate::deliver::ticks(t.system_ns))
}

/// `kill_orphaned_pgrp` for an ending group `g` (its parent `parent`) and the children it handed over.
fn orphaned_pgrps(g: &Group, parent: Option<&Arc<Process>>, children: &[Arc<Process>]) {
    use uapi::ab::signal::{SIGCONT, SIGHUP};
    let hup = |pgrp: u32| {
        klog::event!(Info, "proc", "pgrp.orphaned", pgrp = pgrp; "SIGHUP and SIGCONT to an orphaned group with stopped members");
        crate::deliver::kill_pgrp(pgrp, SIGHUP);
        crate::deliver::kill_pgrp(pgrp, SIGCONT);
    };
    if let Some(pp) = parent
        && pp.pgid() != g.pgid()
        && pp.sid() == g.sid()
        && crate::job::pgrp_orphaned(g.pgid(), Some(g.tgid))
        && crate::job::pgrp_has_stopped(g.pgid())
    {
        hup(g.pgid());
    }
    for c in children {
        if c.pgid() != g.pgid() && c.sid() == g.sid() && crate::job::pgrp_orphaned(c.pgid(), None) && crate::job::pgrp_has_stopped(c.pgid()) {
            hup(c.pgid());
        }
    }
}

/// What wait4/waitid collected.
/// What a wait reports about a child (AB-6: job control besides the end).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WaitEvent {
    /// It ended (collected unless WNOWAIT); `Reaped::status` says how.
    Ended,
    /// It stopped with this signal (WUNTRACED / WSTOPPED; reported once unless WNOWAIT).
    Stopped(u32),
    /// SIGCONT continued it (WCONTINUED; reported once unless WNOWAIT).
    Continued,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Reaped {
    pub pid: u32,
    pub status: ExitStatus,
    pub uid: u32,
    /// SC-6: the child's CPU time with its own reaped children's (wait4's rusage, Linux `getrusage(p,
    /// RUSAGE_BOTH)` in `wait_task_zombie`).
    pub times: sched::cputime::CpuTimes,
    /// AB-7c: the child's `ru_maxrss` in pages for wait4/waitid (`getrusage(p, RUSAGE_BOTH)`).
    pub maxrss_pages: u64,
    pub event: WaitEvent,
}

impl Reaped {
    /// The `int` wait4 stores (`wo_stat`): the end's status, `(sig << 8) | 0x7f` for a stop, 0xffff for a
    /// continue.
    pub fn wait_status(&self) -> u32 {
        match self.event {
            WaitEvent::Ended => self.status.wait_status(),
            WaitEvent::Stopped(sig) => (sig << 8) | 0x7f,
            WaitEvent::Continued => 0xffff,
        }
    }
}

/// Which children a wait looks at and what it does with them (Linux `struct wait_opts`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WaitOpts {
    pub sel: Select,
    pub nohang: bool,
    /// WEXITED: ended children are reported (wait4 always; waitid when asked).
    pub exited: bool,
    /// WUNTRACED / WSTOPPED and WCONTINUED (AB-6).
    pub stopped: bool,
    pub continued: bool,
    /// WNOWAIT: report but leave the child a zombie.
    pub nowait: bool,
    /// __WALL / __WCLONE: which exit signals count (Linux `eligible_child`).
    pub all: bool,
    pub clone: bool,
    /// __WNOTHREAD: only children the calling thread made.
    pub nothread: bool,
}

impl WaitOpts {
    /// wait4's defaults for a selection.
    pub fn wait4(sel: Select, nohang: bool) -> Self {
        WaitOpts { sel, nohang, exited: true, stopped: false, continued: false, nowait: false, all: false, clone: false, nothread: false }
    }
}

fn eligible(c: &Process, parent: &Process, caller_tid: u32, o: &WaitOpts) -> bool {
    c.ppid() == parent.pid
        && o.sel.matches(c.pid, c.pgid(), parent.pgid())
        && o.sel.identity().is_none_or(|g| Arc::as_ptr(c.group()) as usize == g)
        && (o.all || (c.exit_signal() != SIGCHLD) == o.clone)
        && (!o.nothread || c.creator_tid() == caller_tid)
}

/// `wait4` with its defaults for `parent` (kernel callers and AB-2's shape).
pub fn wait_child(parent: &Arc<Process>, sel: Select, nohang: bool) -> Result<Option<Reaped>, Errno> {
    wait_for(parent, 0, &WaitOpts::wait4(sel, nohang))
}

/// `wait_task_stopped` / `wait_task_continued` for a live child (consumed unless WNOWAIT).
fn job_event(c: &Process, o: &WaitOpts) -> Option<WaitEvent> {
    c.signals.raw(|s| {
        if o.stopped && s.group_stopped() && s.stop_code != 0 {
            let sig = s.stop_code;
            if !o.nowait {
                s.stop_code = 0;
            }
            return Some(WaitEvent::Stopped(sig));
        }
        if o.continued && s.stop_continued {
            if !o.nowait {
                s.stop_continued = false;
            }
            return Some(WaitEvent::Continued);
        }
        None
    })
}

/// Linux `do_wait` for `parent` called from thread `caller_tid`: the first ended eligible child is collected
/// (it leaves the table unless WNOWAIT), or (AB-6) a stop or continue is reported when asked. `nohang`:
/// Ok(None) when eligible children exist but none has news. Blocks otherwise until one has; the caller's end
/// makes the wait EINTR, a signal for the calling thread ERESTARTSYS. ECHILD when no child is eligible.
pub fn wait_for(parent: &Arc<Process>, caller_tid: u32, o: &WaitOpts) -> Result<Option<Reaped>, Errno> {
    loop {
        let seen = parent.child_events();
        let (found, job, any) = {
            let mut t = TABLE.lock();
            let mut any = false;
            let mut found = None;
            let mut job = None;
            for (i, c) in t.iter().enumerate() {
                if !eligible(c, parent, caller_tid, o) {
                    continue;
                }
                any = true;
                if c.is_finished() {
                    if o.exited {
                        found = Some(i);
                        break;
                    }
                    continue;
                }
                if (o.stopped || o.continued)
                    && let Some(e) = job_event(c, o)
                {
                    job = Some((c.clone(), e));
                    break;
                }
            }
            let f = found.map(|i| if o.nowait { t[i].clone() } else { t.swap_remove(i) });
            (f, job, any)
        };
        if let Some((c, e)) = job {
            let r = Reaped { pid: c.pid, status: ExitStatus::Exited(0), uid: c.real_uid(), times: c.cpu.with_children(), maxrss_pages: c.both_maxrss_pages(&c.mm), event: e };
            drop(c);
            return Ok(Some(r));
        }
        if let Some(c) = found {
            let r = Reaped { pid: c.pid, status: c.status().unwrap_or(ExitStatus::Exited(0)), uid: c.real_uid(), times: c.cpu.with_children(), maxrss_pages: c.both_maxrss_pages(&c.mm), event: WaitEvent::Ended };
            // SC-6: a collected child's times go to the parent's children (`psig->cutime += ...`); WNOWAIT
            // leaves the child as it is and counts nothing.
            if !o.nowait {
                parent.cpu.reap(&c.cpu);
                // AB-7c: the child's peak (its image's mm keeps its high-water mark after the teardown).
                parent.note_child_maxrss(&c);
            }
            drop(c);
            return Ok(Some(r));
        }
        if !any {
            return Err(uapi::errno::ECHILD);
        }
        if o.nohang {
            return Ok(None);
        }
        // Sleep until a child event, a signal or the caller's end; in slices so nothing can be lost for long.
        parent.wq.wait_until_timeout(|| parent.child_events() != seen || parent.signal_pending(), 1_000_000_000);
        if parent.has_exited() {
            return Err(uapi::errno::EINTR);
        }
        if parent.signal_pending() {
            return Err(uapi::ab::siginfo::ERESTARTSYS);
        }
    }
}

/// Leaves `g`'s other threads' ends to their loops; used by execve's de_thread.
pub(crate) fn stop_siblings(g: &Group, me: &Thread) -> Vec<Arc<Thread>> {
    let others: Vec<Arc<Thread>> = g.threads().into_iter().filter(|t| !core::ptr::eq(&**t, me)).collect();
    for t in &others {
        t.request_stop();
    }
    g.wq.wake_all();
    others
}

/// Whether the group of `t` has any thread left but `t` (exec waits for that).
pub(crate) fn alone(g: &Group) -> bool {
    g.thread_count() <= 1
}
