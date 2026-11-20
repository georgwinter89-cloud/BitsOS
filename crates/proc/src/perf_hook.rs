//! SELFOPT-1: the task events of the perf events that only proc sees (linux-7.3.1/kernel/events/core.c
//! `perf_event_fork`, `perf_event_exit_task`, and `perf_event_exec` with the new image's maps): proc calls them
//! here, the perf events (`syscall::perf`) install the receivers once and switch them on while an event wants
//! side-band records. One relaxed load per fork, exit and exec when nobody profiles.

use crate::Process;
use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, Ordering};

/// The receivers (ids as the initial pid namespace numbers them).
pub struct TaskHooks {
    /// A new task (`PERF_RECORD_FORK`): the child's pid and tid, the creating thread's pid and tid.
    pub fork: fn(pid: u32, tid: u32, ppid: u32, ptid: u32),
    /// A task ends (`PERF_RECORD_EXIT`): its pid, its parent's pid and its tid.
    pub exit: fn(pid: u32, ppid: u32, tid: u32),
    /// A new image runs in thread `tid` of `p` (execve, or a program the kernel started): its name and maps.
    pub image: fn(p: &Arc<Process>, tid: u32),
}

static HOOKS: ksync::Once<TaskHooks> = ksync::Once::new();
static ON: AtomicBool = AtomicBool::new(false);

/// Installs the receivers (the first call wins).
pub fn install(h: TaskHooks) {
    HOOKS.get_or_init(|| h);
}

/// Whether the receivers are called (true while an event wants side-band records).
pub fn set_on(on: bool) {
    ON.store(on, Ordering::Release);
}

fn hooks() -> Option<&'static TaskHooks> {
    if ON.load(Ordering::Relaxed) { HOOKS.get() } else { None }
}

pub(crate) fn fork(pid: u32, tid: u32, ppid: u32, ptid: u32) {
    if let Some(h) = hooks() {
        (h.fork)(pid, tid, ppid, ptid);
    }
}

pub(crate) fn exit(pid: u32, ppid: u32, tid: u32) {
    if let Some(h) = hooks() {
        (h.exit)(pid, ppid, tid);
    }
}

pub(crate) fn image(p: &Arc<Process>, tid: u32) {
    if let Some(h) = hooks() {
        (h.image)(p, tid);
    }
}
