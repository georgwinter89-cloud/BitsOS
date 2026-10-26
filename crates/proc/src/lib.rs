//! Processes and threads (owner: AB; V2-PLAN §2.6, §2.7; contract `C: Process`).
//!
//! AB-2 built the process model and the static ELF loader; AB-4 adds threads, fork, vfork, clone/clone3,
//! execve/execveat, exit_group with every thread, wait4/waitid and process groups and sessions, with Linux
//! 6.6's rules (linux-6.6/kernel/fork.c, fs/exec.c, kernel/exit.c, kernel/sys.c).
//!
//! The model, in Linux's words:
//! - `Group` (`group.rs`) is the thread group: pid (tgid), parent, process group, session, exit status,
//!   signal actions, limits, the live threads. It lives from spawn/fork until its parent collects it.
//! - `Process` (this file) is one *image* of a group: the program's address space, fd table, mount view,
//!   name, path and argv. execve builds a new `Process` for the same group and pid and puts it into the
//!   table; the old one stays valid for whoever still holds it (it derefs to the same `Group`, so `wait`,
//!   `kill` and `status` keep working), but its memory and files are released. Readers outside AB (`/proc`,
//!   the console's `ps`, the `run` tool) keep using `p.mm`, `p.fds`, `p.name`, ... unchanged.
//! - `Thread` (`task.rs`) is one kernel thread running user code (Linux task): tid, clear_child_tid, robust
//!   list. Threads of one group share its `Process` (CLONE_VM|FILES|FS|SIGHAND|THREAD).
//!
//! Ownership has no cycle that outlives the threads: Process → Group, Thread → Group, the group lists its
//! live threads (a thread leaves the list when it ends) and points to its current image weakly.
//!
//! Loader (`elf`, `load`, `stack`, `image`): static `ET_EXEC` and static-PIE, segments placed and protected as
//! Linux does, the initial stack with argv, envp and the auxiliary vector in Linux's order and layout.
//!
//! fork's address space is MM-5b's copy-on-write `AddressSpace::fork`; the thread exit protocol (robust list,
//! CLONE_CHILD_CLEARTID wake) is SC-5's `syscall::futex::exit_release`, reached through `set_exit_release`.
//!
//! Locks (spin, never across a wait): proc.table > proc.tasks > proc.threads > the leaves (proc.gen,
//! proc.status, proc.message, proc.signals, proc.altstack, proc.rlimits, proc.pins, proc.cred). No `Arc<Process>` is
//! dropped under a proc lock (the drop frees an address space, which sleeps).
#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

mod clone;
pub mod cred;
pub mod deliver;
pub mod elf;
mod exec;
mod exit;
mod group;
mod image;
pub mod procinfo;
pub mod itimer;
pub mod job;
mod ktests;
mod ktests_ab4;
mod ktests_ab8;
pub mod load;
pub mod locks;
mod mcp_run;
pub mod pidfd;
pub mod prctl_state;
pub mod rlimit;
pub mod signal;
mod sigapi;
pub mod stack;
pub mod status;
pub mod stdio;
mod task;
pub mod wait;

pub use clone::{CloneArgs, clone, forks_total};
pub use exec::{ArgList, ExecRequest, ExecTarget, execve};
pub use exit::{MmPin, Reaped, WaitEvent, WaitOpts, pin_mm, wait_child, wait_for};
pub use group::Group;
pub use image::min_sigstack_size;
pub use status::ExitStatus;
pub use deliver::{current_signal_pending, kill_pgrp, signal_current_thread};
pub use task::{ExitRelease, Restart, Thread, current_thread, find_thread, set_exit_release};

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, Ordering};
use hal::entry::UserContext;
use ksync::{OnceRef, SpinLock};
use mm::AddressSpace;
use policy::{ProgramId, Subject};
use vfs::{Errno, FdTable, File, MountNs, OpenFile, OpenFlags};

/// Size of the main thread's stack: Linux's default RLIMIT_STACK (8 MiB). Lazy fill: only touched pages cost.
pub const STACK_SIZE: u64 = uapi::ab::exec::STACK_RLIMIT;
/// The stack ends where user space ends (Linux's STACK_TOP without randomisation).
pub const STACK_TOP: u64 = hal::USER_TOP;
/// Linux's default `pid_max` on 64-bit; pids wrap to 300 like Linux (`RESERVED_PIDS`). A namespace size, not a
/// count of things (R1): the table itself grows as needed.
pub(crate) const PID_MAX: u32 = 4_194_304;
const PID_WRAP: u32 = 300;
/// The first pid handed out (pids below stay free for init and early services).
const PID_FIRST: u32 = 100;

// Processes the kernel started and that got their first thread (init, MCP `run`, ktests); `/proc/stat`
// "processes" adds them to the clones.
klog::counter!(pub(crate) SPAWNED, "proc.spawned");
klog::counter!(FAULTED, "proc.faulted");

/// One program image of a thread group (see the module doc). Fields are the image's; everything that outlives
/// an execve is the `Group`'s, reached through `Deref` (`p.ppid()`, `p.signals`, `p.wait(..)`).
pub struct Process {
    pub pid: u32,
    /// Short name (file name of the path, Linux `comm` before its 15-byte cut) for logs and `ps`.
    pub name: String,
    pub path: String,
    pub argv: Vec<String>,
    pub program: ProgramId,
    /// AB-13: the effective uid and gid when this image was made (spawn, fork, execve). Kept for readers outside
    /// AB (vfs-sys, console-cmds, MC's ps); they go stale when the image calls set*id before its next execve:
    /// checks use `cred()` / `fs_cred()` / `capable()` (`cred.rs`).
    pub uid: u32,
    pub gid: u32,
    /// Shared by the group's threads, and with a vfork parent (CLONE_VM) until the child's execve.
    pub mm: Arc<AddressSpace>,
    pub fds: Arc<FdTable>,
    pub ns: Arc<MountNs>,
    /// AB-5: the address space's prctl flags (PR_SET_DUMPABLE, PR_SET_THP_DISABLE; Linux `mm->flags`).
    pub mm_flags: prctl_state::MmFlags,
    /// AB-5: the auxiliary vector the image started with (Linux `mm->saved_auxv`; PR_GET_AUXV). fork copies it.
    pub saved_auxv: SpinLock<Vec<u64>>,
    /// AB-7a: the image's layout and executable for /proc (Linux `mm->{start_code.., exe_file}`). fork copies it.
    pub image_info: SpinLock<procinfo::ImageInfo>,
    group: Arc<Group>,
}

impl core::ops::Deref for Process {
    type Target = Group;
    fn deref(&self) -> &Group {
        &self.group
    }
}

impl Process {
    pub fn subject(&self) -> Subject {
        Subject { program: self.program, pid: self.pid, uid: self.cred().euid }
    }
    /// The thread group behind this image.
    pub fn group(&self) -> &Arc<Group> {
        &self.group
    }
}

/// What the syscall layer does with one syscall of thread `t` in image `p`.
pub type SyscallHandler = fn(&Arc<Process>, &Arc<Thread>, &mut UserContext);

static HANDLER: OnceRef<SyscallHandler> = OnceRef::new();
static HANDLER_SLOT: ksync::Once<SyscallHandler> = ksync::Once::new();

/// Installed once by the `syscall` crate.
pub fn set_syscall_handler(h: SyscallHandler) {
    let r: &'static SyscallHandler = HANDLER_SLOT.get_or_init(|| h);
    HANDLER.set(r);
}

pub(crate) fn syscall_handler() -> Option<&'static SyscallHandler> {
    HANDLER.get()
}

/// Kernel init (called by `syscall::init`): lets user copies that fault on a page the program has not touched
/// yet fill it (MM-5a's lazy fill behind AB-1's fast copies).
pub fn init() {
    let _ = hal::usercopy::set_fault_resolver(Some(resolve_copy_fault));
    // MM-tlb2 CR 5: the resolver retries spurious faults, so MM's rights-only grants may skip their round.
    hal::paging::tlb::set_spurious_copy_retry(true);
    // The preemption point after an interrupt in ring 3 (SC-3 + AB-2; owned by AB): give way if the scheduler
    // asks, and stop the thread's run when it was told to end meanwhile.
    hal::entry::set_user_irq_hook(Some(user_irq));
    // AB-6 fixer: the service that fires interval and POSIX timers.
    itimer::start();
}

fn user_irq() -> bool {
    sched::cond_resched();
    // AB-6: a signal to handle comes back to the run loop too (`deliver::exit_to_user`).
    task::current_thread().is_some_and(|t| t.should_stop() || t.signal_pending())
}

fn resolve_copy_fault(addr: u64, write: bool) -> bool {
    let Some(p) = current() else { return false };
    // Missing pages, and (MM-5b CR 1) a write to a present read-only page: the fault path breaks copy-on-write
    // and sets dirty marks, and refuses pages that really are read-only (EFAULT).
    // MM-tlb2 CR 5 (INT-W7): an access the page table allows faulted on a stale, narrower TLB entry this CPU
    // still held (MM's grants skip the shootdown round): a spurious fault. Drop the entry and retry, as Linux
    // does (linux-6.6/mm/memory.c:4995-5017, flush_tlb_fix_spurious_fault). It cannot loop: the fast copy
    // returns EFAULT when the same address faults twice in a row (hal::usercopy).
    if let Some((_, fl)) = hal::paging::translate(p.mm.root(), addr & !0xFFF)
        && fl.contains(hal::paging::PageFlags::USER)
        && (!write || fl.contains(hal::paging::PageFlags::WRITABLE))
    {
        hal::paging::flush_page(addr & !0xFFF);
        return true;
    }
    let error = mm::fault::PF_USER | if write { mm::fault::PF_WRITE } else { 0 };
    p.mm.handle_user_fault(addr, error).is_ok()
}

static NEXT_PID: AtomicU32 = AtomicU32::new(PID_FIRST);
pub(crate) static TABLE: SpinLock<Vec<Arc<Process>>> = SpinLock::named("proc.table", Vec::new());

/// Every process in the table (running ones and zombies), as their current images.
pub fn processes() -> Vec<Arc<Process>> {
    TABLE.lock().clone()
}

/// The last id handed out (`/proc/loadavg`'s fifth field, Linux `idr_get_cursor - 1`).
pub fn last_pid() -> u32 {
    NEXT_PID.load(Ordering::Acquire).saturating_sub(1)
}

/// `/proc/sys/kernel/pid_max`: ids stay below it; root may move it within Linux's bounds.
static PID_LIMIT: AtomicU32 = AtomicU32::new(PID_MAX);

/// The largest id plus one (`/proc/sys/kernel/pid_max`).
pub fn pid_max() -> u32 {
    PID_LIMIT.load(Ordering::Relaxed)
}

/// Sets `pid_max` as `proc_dointvec_minmax` with `pid_max_min`/`pid_max_max` (linux-6.6/kernel/pid.c :61,
/// kernel/sysctl.c): EINVAL outside RESERVED_PIDS + 1 ..= PID_MAX_LIMIT. Live processes keep their ids; the
/// next id wraps when the cursor is at or above the new limit (`alloc_pid`).
pub fn set_pid_max(v: u64) -> Result<(), Errno> {
    if !(PID_WRAP as u64 + 1..=PID_MAX as u64).contains(&v) {
        return Err(uapi::errno::EINVAL);
    }
    PID_LIMIT.store(v as u32, Ordering::Relaxed);
    klog::event!(Info, "proc", "pid_max.set", value = v);
    Ok(())
}

/// The process with pid `pid` (its current image).
pub fn find(pid: u32) -> Option<Arc<Process>> {
    TABLE.lock().iter().find(|p| p.pid == pid).cloned()
}

/// The image of the process whose thread is running this code (None in kernel threads).
pub fn current() -> Option<Arc<Process>> {
    task::current_thread()?.group().image()
}

/// Whether `id` is taken in the one space pids and tids share (Linux: both are `struct pid`s): a process, a
/// thread, or a process group or session still named by a live member (Linux keeps a `struct pid` alive while
/// it is anyone's pgid or sid, so a new process never gets the number of a live group or session). Called with
/// the table lock held; takes `proc.tasks` inside it without copying it.
fn id_in_use(table: &[Arc<Process>], id: u32) -> bool {
    table.iter().any(|p| p.pid == id || p.pgid() == id || p.sid() == id) || task::tid_in_use(id)
}

/// A free id, or None when all are used. Called with the table lock held.
fn next_id(table: &[Arc<Process>]) -> Option<u32> {
    let limit = pid_max();
    for _ in 0..limit {
        // Distinct values for concurrent callers; after pid_max - 1 comes PID_WRAP (Linux).
        let pid = NEXT_PID.try_update(Ordering::AcqRel, Ordering::Acquire, |n| Some(if n + 1 >= limit { PID_WRAP } else { n + 1 })).unwrap_or(PID_FIRST);
        if !id_in_use(table, pid) {
            return Some(pid);
        }
    }
    None
}

/// Puts a new process image into the table under a fresh id (or `want`: pid 1, clone3's set_tid). `make` runs
/// under the table lock and sees the table (fork reads its parent's ids there, consistent with an exit's
/// reparenting, which runs under the same lock).
pub(crate) fn insert_new(make: impl FnOnce(u32, &[Arc<Process>]) -> Arc<Process>, want: Option<u32>) -> Result<Arc<Process>, SpawnError> {
    let mut t = TABLE.lock();
    t.try_reserve(1).map_err(|_| SpawnError::NoMemory)?;
    let pid = match want {
        Some(w) => {
            if id_in_use(&t, w) {
                return Err(SpawnError::Io(if w == 1 { uapi::errno::EBUSY } else { uapi::errno::EEXIST }));
            }
            w
        }
        None => next_id(&t).ok_or(SpawnError::NoPid)?,
    };
    let p = make(pid, &t);
    t.push(p.clone());
    Ok(p)
}

/// A fresh id for a new thread of an existing group.
pub(crate) fn new_tid() -> Option<u32> {
    let t = TABLE.lock();
    next_id(&t)
}

/// Takes `pid` out of the table (the caller drops it, outside the lock).
pub(crate) fn remove(pid: u32) -> Option<Arc<Process>> {
    let mut t = TABLE.lock();
    let i = t.iter().position(|q| q.pid == pid)?;
    Some(t.swap_remove(i))
}

/// Replaces the table entry of `new.pid` with the new image; returns the old one.
pub(crate) fn replace_image(new: &Arc<Process>) -> Option<Arc<Process>> {
    let mut t = TABLE.lock();
    let i = t.iter().position(|q| q.pid == new.pid)?;
    Some(core::mem::replace(&mut t[i], new.clone()))
}

/// Whether a process other than `except` that still runs its image uses this address space (a vfork parent,
/// CLONE_VM). A process whose last thread ended marks itself (`Group::mark_released`) under the table lock
/// before it looks here, so of two sharers ending at once the later one always sees the earlier as gone and
/// gives the memory back (both may; a second unmap finds nothing).
pub(crate) fn mm_shared(mm: &Arc<AddressSpace>, except: u32) -> bool {
    TABLE.lock().iter().any(|q| q.pid != except && !q.is_released() && Arc::ptr_eq(&q.mm, mm))
}

/// Whether a process other than `except` that still runs its image uses this fd table (CLONE_FILES without
/// CLONE_THREAD); see `mm_shared`.
pub fn fds_shared(fds: &Arc<FdTable>, except: u32) -> bool {
    TABLE.lock().iter().any(|q| q.pid != except && !q.is_released() && Arc::ptr_eq(&q.fds, fds))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpawnError {
    NotFound,
    NotExecutable(&'static str),
    NoMemory,
    Io(Errno),
    /// argv/envp too large (E2BIG).
    TooBig,
    /// Every pid is in use.
    NoPid,
}

impl SpawnError {
    /// The errno `execve` gives for it.
    pub fn errno(&self) -> Errno {
        use uapi::errno::*;
        match self {
            SpawnError::NotFound => ENOENT,
            SpawnError::NotExecutable(_) => ENOEXEC,
            SpawnError::NoMemory => ENOMEM,
            SpawnError::Io(e) => *e,
            SpawnError::TooBig => E2BIG,
            SpawnError::NoPid => EAGAIN,
        }
    }
}

impl core::fmt::Display for SpawnError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            SpawnError::NotFound => f.write_str("no such file"),
            SpawnError::NotExecutable(why) => write!(f, "not executable: {why}"),
            SpawnError::NoMemory => f.write_str("out of memory"),
            SpawnError::Io(e) => write!(f, "read error (errno {e})"),
            SpawnError::TooBig => f.write_str("arguments too large (E2BIG)"),
            SpawnError::NoPid => f.write_str("no free process id"),
        }
    }
}

/// How to start a process. `SpawnOptions::new(argv, envp, program)` gives the defaults: the kernel as parent,
/// stdin/stdout/stderr on `/dev/console`, scheduling class Normal, running at once.
pub struct SpawnOptions<'a> {
    pub argv: &'a [&'a str],
    pub envp: &'a [&'a str],
    pub program: ProgramId,
    /// The parent that collects the exit status with `wait4` (None: the kernel, through `Process::wait`).
    pub parent: Option<&'a Arc<Process>>,
    pub stdin: Option<Arc<dyn File>>,
    pub stdout: Option<Arc<dyn File>>,
    pub stderr: Option<Arc<dyn File>>,
    pub class: sched::Class,
    /// Created but held before its first instruction until `Process::release` (tests that must set up
    /// children first).
    pub held: bool,
    /// The process's uid, also its gid (all real/effective/saved/fs ids; no supplementary groups); default 1000.
    /// UL-2 change request 2 (INT-A1): the system's init runs as 0.
    pub uid: u32,
    /// Take pid 1 (the system's init; orphans are given to it while it lives). `SpawnError::Io(EBUSY)` when pid
    /// 1 is in the table.
    pub pid1: bool,
}

impl<'a> SpawnOptions<'a> {
    pub fn new(argv: &'a [&'a str], envp: &'a [&'a str], program: ProgramId) -> Self {
        SpawnOptions { argv, envp, program, parent: None, stdin: None, stdout: None, stderr: None, class: sched::Class::Normal, held: false, uid: 1000, pid1: false }
    }
}

/// Starts `path` with `argv`/`envp` as a new process (contract `C: Process`).
pub fn spawn(path: &str, argv: &[&str], envp: &[&str], program: ProgramId) -> Result<Arc<Process>, SpawnError> {
    spawn_with(path, &SpawnOptions::new(argv, envp, program))
}

/// Reads `path` from the process's mount view (G0: the root view) into memory.
pub fn read_program(ns: &MountNs, path: &str) -> Result<Vec<u8>, SpawnError> {
    let file = ns.open(path).map_err(|e| if e == uapi::errno::ENOENT || e == uapi::errno::ENOTDIR { SpawnError::NotFound } else { SpawnError::Io(e) })?;
    if file.mode() & 0o170000 == 0o040000 {
        return Err(SpawnError::Io(uapi::errno::EACCES));
    }
    read_file(&*file)
}

/// The whole content of an open file (ktests and tools that look at a program's bytes; the loader maps files).
pub(crate) fn read_file(file: &dyn File) -> Result<Vec<u8>, SpawnError> {
    let size = file.size() as usize;
    let mut image: Vec<u8> = Vec::new();
    image.try_reserve_exact(size).map_err(|_| SpawnError::NoMemory)?;
    image.resize(size, 0);
    let mut got = 0;
    while got < size {
        let n = file.read_at(got as u64, &mut image[got..]).map_err(SpawnError::Io)?;
        if n == 0 {
            break;
        }
        got += n;
    }
    image.truncate(got);
    Ok(image)
}

pub fn spawn_with(path: &str, o: &SpawnOptions<'_>) -> Result<Arc<Process>, SpawnError> {
    let ns = vfs::root_ns();
    // AB-8: the file is mapped, not read (AB-7a: it is also /proc/<pid>/exe).
    let (exe, f) = open_program(&ns, path.as_bytes())?;
    spawn_inner(path, image::Source::File { path: &exe, file: &*f }, o, Some(exe.clone()))
}

/// A program file of the root view for the kernel's own starts (no permission check: the kernel starts it).
fn open_program(ns: &MountNs, path: &[u8]) -> Result<(vfs::Path, Arc<dyn File>), SpawnError> {
    if path.is_empty() {
        // `getname` of "": ENOENT (an empty PT_INTERP path included).
        return Err(SpawnError::NotFound);
    }
    let p = ns.lookup_path(path, true).map_err(|e| if e == uapi::errno::ENOENT || e == uapi::errno::ENOTDIR { SpawnError::NotFound } else { SpawnError::Io(e) })?;
    if !p.dentry.inode.is_reg() {
        return Err(SpawnError::Io(uapi::errno::EACCES));
    }
    let f = vfs::node::file_of(&p.dentry);
    Ok((p, f))
}

/// Starts a program from an ELF image in memory (`path` names it in logs, AT_EXECFN and `/proc`); `spawn`
/// reads the file and calls this. Used directly by ktests that build small programs.
pub fn spawn_image(path: &str, image: &[u8], argv: &[&str], envp: &[&str], program: ProgramId) -> Result<Arc<Process>, SpawnError> {
    spawn_image_with(path, image, &SpawnOptions::new(argv, envp, program))
}

pub fn spawn_image_with(path: &str, image: &[u8], o: &SpawnOptions<'_>) -> Result<Arc<Process>, SpawnError> {
    spawn_inner(path, image::Source::Mem(image), o, None)
}

fn spawn_inner(path: &str, src: image::Source<'_>, o: &SpawnOptions<'_>, exe: Option<vfs::Path>) -> Result<Arc<Process>, SpawnError> {
    let ns = vfs::root_ns();
    // `do_open_execat`'s deny_write_access, kept with the image (AB-8 CR2).
    let deny = match &exe {
        Some(e) => Some(Arc::new(e.dentry.inode.deny_write().map_err(SpawnError::Io)?)),
        None => None,
    };
    let elf = src.elf().map_err(SpawnError::NotExecutable)?;
    // AB-8: a dynamic program's interpreter from the root view (ENOENT when missing, ELIBBAD/EINVAL when it is
    // no x86-64 ELF program, as execve).
    let interp = match &elf.interp {
        None => None,
        Some(name) => {
            let (ip, ifile) = open_program(&ns, name)?;
            let ideny = ip.dentry.inode.deny_write().map_err(SpawnError::Io)?;
            let ie = image::Source::File { path: &ip, file: &*ifile }.interp_elf().map_err(|why| {
                klog::event!(Info, "proc", "spawn.interp_refused", path = path, why = why);
                SpawnError::Io(exec::interp_errno(why))
            })?;
            Some((ip, ifile, ie, ideny))
        }
    };
    let iimg = interp.as_ref().map(|(ip, ifile, ie, _)| image::Image { src: image::Source::File { path: ip, file: &**ifile }, elf: ie });
    let mm = AddressSpace::with_fill(mm::Fill::OnFault).map_err(|_| SpawnError::NoMemory)?;
    let argv: Vec<&[u8]> = o.argv.iter().map(|s| s.as_bytes()).collect();
    let envp: Vec<&[u8]> = o.envp.iter().map(|s| s.as_bytes()).collect();
    let cred = Arc::new(cred::Cred::new(o.uid, o.uid));
    let l = image::load(&mm, &image::Image { src, elf: &elf }, iimg.as_ref(), STACK_SIZE, path.as_bytes(), &argv, &envp, &cred)?;
    drop(iimg);
    // FS-13: the program and its interpreter for the start-up preload's record.
    let preload = (exe.clone(), interp.as_ref().map(|(ip, ..)| ip.clone()));
    drop(interp);

    let fds = FdTable::new();
    let console = |what: &Option<Arc<dyn File>>| -> Result<Arc<dyn File>, SpawnError> {
        match what {
            Some(f) => Ok(f.clone()),
            None => ns.open("/dev/console").map_err(SpawnError::Io),
        }
    };
    for (fd, f, read, write) in [(0, &o.stdin, true, false), (1, &o.stdout, false, true), (2, &o.stderr, false, true)] {
        // The console is opened read-write on all three, as a login shell gets it.
        let (read, write) = if f.is_none() { (true, true) } else { (read, write) };
        fds.install_at(fd, OpenFile::new(console(f)?, OpenFlags { read, write, ..Default::default() })).map_err(SpawnError::Io)?;
    }
    let mut args = Vec::new();
    args.try_reserve(o.argv.len()).map_err(|_| SpawnError::NoMemory)?;
    args.extend(o.argv.iter().map(|s| String::from(*s)));
    let ram_pages = mm_core::frames().map_or(0, |f| f.stats().total_bytes / 4096);
    let parent = o.parent.cloned();
    let (mm, fds) = (Arc::new(mm), Arc::new(fds));
    let p = insert_new(
        |pid, _| {
            // A child of a process joins its group and session; a child of the kernel leads both (like setsid).
            let (ppid, pgid, sid, leader) = match &parent {
                Some(q) => (q.pid, q.pgid(), q.sid(), false),
                None => (0, pid, pid, true),
            };
            let ids = group::Ids { tgid: pid, ppid, pgid, sid, session_leader: leader, creator_tid: ppid, cred: cred.clone() };
            // AB-6: init (pid 1) is SIGNAL_UNKILLABLE: no signal it has no handler for reaches it.
            let sigs = signal::SignalState::default();
            sigs.raw(|s| s.unkillable = pid == 1);
            let g = Group::new(ids, uapi::ab::signal::SIGCHLD, o.class, sigs, rlimit::Limits::new(ram_pages), time::now_ns());
            make_image(pid, base_name(path), String::from(path), args, o.program, &cred, mm, fds, ns, g)
        },
        o.pid1.then_some(1),
    )?;
    p.group.set_image(&p);
    *p.saved_auxv.lock() = l.auxv;
    *p.image_info.lock() = procinfo::ImageInfo { exe, deny, ..l.info };
    if !o.held {
        p.release();
    }
    if let Some(x) = &preload.0 {
        vfs::preload::exec_started(p.pid, &p.ns, &p.ns.root_path(), x, preload.1.as_ref());
    }
    klog::event!(Info, "proc", "spawn", pid = p.pid, ppid = p.ppid(), path = path, entry = klog::Hex(l.entry), pie = elf.is_pie, bias = klog::Hex(l.bias), interp_base = klog::Hex(l.interp_base), program = o.program.0);
    let t = Thread::new(p.pid, &p.group);
    t.prctl().set_comm(prctl_state::comm_of(p.name.as_bytes()));
    if let Err(e) = task::start(&p, t, UserContext::new(l.entry, l.rsp), None, 0, 0) {
        klog::event!(Warn, "proc", "spawn.no_thread", pid = p.pid, path = path);
        drop(remove(p.pid));
        return Err(e);
    }
    SPAWNED.inc();
    Ok(p)
}

/// The last component of a path (Linux `kbasename`, the `comm` of an exec).
pub(crate) fn base_name(path: &str) -> String {
    String::from(path.rsplit('/').next().unwrap_or(path))
}

/// Builds the image struct of a group (spawn and fork/clone: a new group; execve: the same group).
#[allow(clippy::too_many_arguments)]
pub(crate) fn make_image(
    pid: u32,
    name: String,
    path: String,
    argv: Vec<String>,
    program: ProgramId,
    cred: &cred::Cred,
    mm: Arc<AddressSpace>,
    fds: Arc<FdTable>,
    ns: Arc<MountNs>,
    group: Arc<Group>,
) -> Arc<Process> {
    Arc::new(Process { pid, name, path, argv, program, uid: cred.euid, gid: cred.egid, mm, fds, ns, mm_flags: prctl_state::MmFlags::new(), saved_auxv: SpinLock::named("proc.auxv", Vec::new()), image_info: SpinLock::named("proc.image", procinfo::ImageInfo::default()), group })
}

/// Random bytes for AT_RANDOM and getrandom: FS-2's ChaCha20 generator (seeded from RDSEED/RDRAND, never
/// blocks; V2-PLAN §2.8).
pub fn random_bytes(out: &mut [u8]) {
    vfs::random::fill(out);
}
