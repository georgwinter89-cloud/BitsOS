//! AB-7a: the per-process part of /proc (moved here from FS-2's `vfs-sys/src/procpid.rs`, content providers
//! belong to their owners): `/proc/self`, `/proc/thread-self`, `/proc/<pid>` and `/proc/<pid>/task/<tid>` with
//! the files of linux-6.6/fs/proc/base.c `tgid_base_stuff`/`tid_base_stuff` that programs read (in the
//! table's order): task, fd, fdinfo, environ, auxv, status, personality, limits, comm, cmdline, stat, statm,
//! maps, mem, cwd, root, exe, mounts, mountinfo, smaps, smaps_rollup, cgroup, oom_score, oom_score_adj. The framework
//! is FS's `vfs::synth`.
//!
//! A directory holds its thread group weakly (`Weak<Group>`; the image is asked for at each read, so an execve
//! or the leader's end never orphans it); an ended and reaped process answers ESRCH. Access to another
//! process's memory (environ, auxv, maps, smaps, smaps_rollup, fdinfo, the exe/cwd/root links) follows
//! `ptrace_may_access(PTRACE_MODE_READ)`: the same thread group, root, or the same uid while dumpable.
//!
//! AB-7b: the system-wide files: `system` (/proc/{meminfo,stat,uptime,loadavg,version,filesystems}),
//! `cpuinfo`, `sysctl` (/proc/sys/kernel, /proc/sys/vm/overcommit_memory), `sysfs` with `sys_cpu` and
//! `sys_net` (/sys/devices/system/cpu, /sys/class/net); `attr::Live` is their text-at-open file.

mod attr;
mod cpuinfo;
mod fd;
mod maps;
mod mem;
mod mounts;
mod stat;
mod sys_cache;
mod sys_cpu;
mod sys_net;
mod sysctl;
mod sysfs;
mod system;
mod text;
#[cfg(test)]
mod tests;

use alloc::format;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use proc::{Group, Process, Thread};
use uapi::errno::*;
use vfs::synth::{DirSource, FileSource, MagicLink, SynthEntry};
use vfs::{Cred, Errno};

/// Listing positions in /proc: "self", "thread-self", "mounts", then the processes by pid.
const SELF_COOKIE: u64 = 1;
const THREAD_SELF_COOKIE: u64 = 2;
const MOUNTS_COOKIE: u64 = 3;
const PID_COOKIE: u64 = 16;

struct PidRoot;

fn pid_dir(g: &Arc<Group>, p: &Process, tid: u32) -> SynthEntry {
    let t = Target { g: Arc::downgrade(g), tgid: g.tgid, tid, whole: true };
    owned(SynthEntry::dir(&format!("{tid}"), Arc::new(PidDir(t))), p, true).cookie(PID_COOKIE + tid as u64).immutable()
}

fn self_link() -> Option<SynthEntry> {
    proc::current().map(|me| SynthEntry::link("self", &format!("{}", me.pid)).cookie(SELF_COOKIE))
}

/// `/proc/thread-self` (linux-6.6/fs/proc/thread_self.c): `<pid>/task/<tid>` of the reader.
fn thread_self_link() -> Option<SynthEntry> {
    let t = proc::current_thread()?;
    let tgid = t.group().tgid;
    Some(SynthEntry::link("thread-self", &format!("{tgid}/task/{}", t.tid())).cookie(THREAD_SELF_COOKIE))
}

/// `/proc/mounts` -> `self/mounts` (linux-6.6/fs/proc/root.c:291 `proc_symlink("mounts", NULL, "self/mounts")`),
/// there for every reader (a kernel thread's lookup through it fails at `self`).
fn mounts_link() -> SynthEntry {
    SynthEntry::link("mounts", "self/mounts").cookie(MOUNTS_COOKIE)
}

impl DirSource for PidRoot {
    fn entries(&self) -> Result<Vec<SynthEntry>, Errno> {
        let mut out = Vec::new();
        out.extend(self_link());
        out.extend(thread_self_link());
        out.push(mounts_link());
        let mut ps = proc::processes();
        // proc's table is in creation order, which is pid order until pids wrap: the stable sort is one pass then.
        ps.sort_by_key(|p| p.pid);
        out.extend(ps.iter().map(|p| pid_dir(p.group(), p, p.pid)));
        Ok(out)
    }
    fn lookup(&self, name: &[u8]) -> Result<Option<SynthEntry>, Errno> {
        match name {
            // Kernel threads have no process: both links are missing for them.
            b"self" => return Ok(self_link()),
            b"thread-self" => return Ok(thread_self_link()),
            b"mounts" => return Ok(Some(mounts_link())),
            _ => {}
        }
        let Some(id) = vfs::synth::name_to_int(name) else { return Ok(None) };
        if let Some(p) = proc::find(id) {
            return Ok(Some(pid_dir(p.group(), &p, id)));
        }
        // `proc_pid_lookup` finds any task: a thread's id names a (hidden) directory of its group.
        Ok(proc::find_thread(id).and_then(|t| {
            let g = t.group().clone();
            let p = g.image()?;
            Some(pid_dir(&g, &p, id))
        }))
    }
}

fn pid_root() -> Arc<dyn DirSource> {
    Arc::new(PidRoot)
}
vfs::procdir!("/proc", pid_root);

/// Whose files a directory shows: the thread group, and the thread (`tid`; the leader's id for /proc/<pid>).
/// `whole`: the process view (/proc/<pid>: CPU times summed, `task/` present), else one thread's.
#[derive(Clone)]
pub(super) struct Target {
    g: Weak<Group>,
    tgid: u32,
    tid: u32,
    whole: bool,
}

impl Target {
    fn group(&self) -> Result<Arc<Group>, Errno> {
        self.g.upgrade().ok_or(ESRCH)
    }
    /// The group and its current image (ESRCH once reaped).
    fn live(&self) -> Result<(Arc<Group>, Arc<Process>), Errno> {
        let g = self.group()?;
        let p = g.image().ok_or(ESRCH)?;
        Ok((g, p))
    }
    /// The thread the view is about (None: it ended; for the whole view, the leader has ended).
    fn thread(&self, g: &Group) -> Option<Arc<Thread>> {
        g.threads().into_iter().find(|t| t.tid() == self.tid)
    }
}

/// AB-13: who owns a process's /proc entries (linux-6.6/fs/proc/base.c `task_dump_owner`): its euid and egid;
/// a file (not the r-x pid/tid directories) of a process that is not dumpable is root's.
pub(super) fn owned(e: SynthEntry, p: &Process, dir: bool) -> SynthEntry {
    if !dir && !p.is_finished() && p.mm_flags.dumpable() != 1 {
        return e.owner(0, 0);
    }
    let c = p.cred();
    e.owner(c.euid, c.egid)
}

/// `ptrace_may_access(task, PTRACE_MODE_READ_FSCREDS)` for the reader (`Process::may_ptrace`, AB-13). Kernel
/// readers (ktests, MCP) may.
pub(super) fn may_read_mm(p: &Process) -> bool {
    proc::current().is_none_or(|me| me.may_ptrace(p, true))
}

/// The reader's mount view (paths in links and maps are shown relative to the reader's root, as `d_path`).
pub(super) fn reader_ns() -> Arc<vfs::MountNs> {
    proc::current().map_or_else(vfs::root_ns, |me| me.ns.clone())
}

pub(super) fn d_path(path: &vfs::Path) -> Vec<u8> {
    let ns = reader_ns();
    ns.d_path(&ns.root_path(), path)
}

/// One file of a process directory.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum What {
    Environ,
    Auxv,
    Status,
    Limits,
    Comm,
    Cmdline,
    Stat,
    Statm,
    Maps,
    Mounts,
    MountInfo,
    Smaps,
    SmapsRollup,
    OomScore,
    OomScoreAdj,
    Personality,
    Cgroup,
}

#[derive(Clone, Copy)]
enum Item {
    Task,
    Fd,
    FdInfo,
    Link(LinkKind),
    File(What, u32),
    Mem,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum LinkKind {
    Cwd,
    Root,
    Exe,
}

/// `tgid_base_stuff`'s order (the subset V2 has); `task` only in the process view.
const ITEMS: [(&str, Item); 24] = [
    ("task", Item::Task),
    ("fd", Item::Fd),
    ("fdinfo", Item::FdInfo),
    ("environ", Item::File(What::Environ, 0o400)),
    ("auxv", Item::File(What::Auxv, 0o400)),
    ("status", Item::File(What::Status, 0o444)),
    ("personality", Item::File(What::Personality, 0o400)),
    ("limits", Item::File(What::Limits, 0o444)),
    ("comm", Item::File(What::Comm, 0o644)),
    ("cmdline", Item::File(What::Cmdline, 0o444)),
    ("stat", Item::File(What::Stat, 0o444)),
    ("statm", Item::File(What::Statm, 0o444)),
    ("maps", Item::File(What::Maps, 0o444)),
    ("mem", Item::Mem),
    ("cwd", Item::Link(LinkKind::Cwd)),
    ("root", Item::Link(LinkKind::Root)),
    ("exe", Item::Link(LinkKind::Exe)),
    ("mounts", Item::File(What::Mounts, 0o444)),
    ("mountinfo", Item::File(What::MountInfo, 0o444)),
    ("smaps", Item::File(What::Smaps, 0o444)),
    ("smaps_rollup", Item::File(What::SmapsRollup, 0o444)),
    ("cgroup", Item::File(What::Cgroup, 0o444)),
    ("oom_score", Item::File(What::OomScore, 0o444)),
    ("oom_score_adj", Item::File(What::OomScoreAdj, 0o644)),
];

fn make_entry(t: &Target, p: &Arc<Process>, i: usize, name: &str, item: Item) -> Option<SynthEntry> {
    let own = |e: SynthEntry| owned(e, p, false).cookie(i as u64 + 1);
    Some(match item {
        Item::Task if !t.whole => return None,
        Item::Task => own(SynthEntry::dir(name, Arc::new(TaskDir(t.clone())))),
        Item::Fd => {
            // dr-x------ for the owner: one magic link per open fd (FS-10).
            let mut e = own(SynthEntry::dir(name, Arc::new(fd::FdDir(t.clone()))));
            e.perm = 0o500;
            e
        }
        Item::FdInfo => {
            // `proc_fdinfo_permission`: ptrace READ access, else EACCES at the open (`DirSource::may_open`) and
            // at a lookup inside; the mode stays r-x for everyone, as Linux shows it.
            own(SynthEntry::dir(name, Arc::new(fd::FdInfoDir(t.clone()))))
        }
        Item::Link(k) => own(link_entry(t, p, name, k)),
        Item::Mem => own(SynthEntry::file(name, 0o600, Arc::new(mem::Mem(t.clone())))),
        Item::File(what, perm) => own(SynthEntry::file(name, perm, Arc::new(PidFile { t: t.clone(), what }))),
    })
}

/// `proc_pid_get_link`/`proc_pid_readlink`: a magic link to the place itself; a reader without access gets
/// EACCES from readlink (`MagicLink::may_read`) and when following, while lstat works as on Linux; a zombie's
/// link leads nowhere (ENOENT).
fn link_entry(t: &Target, p: &Arc<Process>, name: &str, k: LinkKind) -> SynthEntry {
    let to = LinkTo { t: t.clone(), k };
    let text = match to.place() {
        Ok(path) => d_path(&path),
        // An image started from memory (ktests) has no file: its exe stays the name it was started as.
        Err(ENOENT) if k == LinkKind::Exe && !p.is_finished() => return SynthEntry::link(name, &p.path),
        Err(_) => Vec::new(),
    };
    SynthEntry::magic(name, 0o777, text, Arc::new(to))
}

struct LinkTo {
    t: Target,
    k: LinkKind,
}

impl LinkTo {
    fn place(&self) -> Result<vfs::Path, Errno> {
        let (_, p) = self.t.live().map_err(|_| ENOENT)?;
        if !may_read_mm(&p) {
            return Err(EACCES);
        }
        if p.is_finished() {
            return Err(ENOENT);
        }
        match self.k {
            LinkKind::Cwd => Ok(p.fds.fs.cwd().unwrap_or_else(|| p.ns.root_path())),
            LinkKind::Root => Ok(p.ns.root_path()),
            LinkKind::Exe => p.image_info.lock().exe.clone().ok_or(ENOENT),
        }
    }
}

impl MagicLink for LinkTo {
    fn target(&self) -> Result<vfs::Path, Errno> {
        self.place()
    }
    fn may_read(&self) -> Result<(), Errno> {
        self.place().map(|_| ())
    }
}

fn dir_entries(t: &Target) -> Result<Vec<SynthEntry>, Errno> {
    let (g, p) = t.live()?;
    if !t.whole && t.thread(&g).is_none() {
        return Err(ESRCH);
    }
    Ok(ITEMS.iter().enumerate().filter_map(|(i, (n, it))| make_entry(t, &p, i, n, *it)).collect())
}

fn dir_lookup(t: &Target, name: &[u8]) -> Result<Option<SynthEntry>, Errno> {
    let (g, p) = t.live()?;
    if !t.whole && t.thread(&g).is_none() {
        return Err(ESRCH);
    }
    Ok(ITEMS.iter().enumerate().find(|(_, (n, _))| n.as_bytes() == name).and_then(|(i, (n, it))| make_entry(t, &p, i, n, *it)))
}

/// `pid_entry_nlink` (linux-6.6/fs/proc/base.c:158-167): the links stat adds for the table's directories
/// (`/proc/<pid>` base.c:3426-3429, `/proc/<pid>/task/<tid>` base.c:3926-3929; `task` only in the process view).
fn dir_items(whole: bool) -> u32 {
    ITEMS.iter().filter(|(_, it)| matches!(it, Item::Fd | Item::FdInfo) || (whole && matches!(it, Item::Task))).count() as u32
}

/// `/proc/<pid>`.
struct PidDir(Target);

impl DirSource for PidDir {
    fn extra_nlink(&self) -> u32 {
        dir_items(self.0.whole)
    }
    fn entries(&self) -> Result<Vec<SynthEntry>, Errno> {
        dir_entries(&self.0)
    }
    fn lookup(&self, name: &[u8]) -> Result<Option<SynthEntry>, Errno> {
        dir_lookup(&self.0, name)
    }
}

/// `/proc/<pid>/task`: one directory per live thread, in tid order.
struct TaskDir(Target);

impl TaskDir {
    fn entry(&self, p: &Process, tid: u32) -> SynthEntry {
        let t = Target { g: self.0.g.clone(), tgid: self.0.tgid, tid, whole: false };
        owned(SynthEntry::dir(&format!("{tid}"), Arc::new(TidDir(t))), p, true).cookie(tid as u64 + 1).immutable()
    }
}

impl DirSource for TaskDir {
    fn extra_nlink(&self) -> u32 {
        self.0.live().map(|(g, _)| g.threads().len() as u32).unwrap_or(0)
    }
    fn entries(&self) -> Result<Vec<SynthEntry>, Errno> {
        let (g, p) = self.0.live()?;
        let mut tids: Vec<u32> = g.threads().iter().map(|t| t.tid()).collect();
        tids.sort_unstable();
        Ok(tids.into_iter().map(|tid| self.entry(&p, tid)).collect())
    }
    fn lookup(&self, name: &[u8]) -> Result<Option<SynthEntry>, Errno> {
        let (g, p) = self.0.live()?;
        let Some(tid) = vfs::synth::name_to_int(name) else { return Ok(None) };
        Ok(g.threads().iter().any(|t| t.tid() == tid).then(|| self.entry(&p, tid)))
    }
}

/// `/proc/<pid>/task/<tid>`.
struct TidDir(Target);

impl DirSource for TidDir {
    fn extra_nlink(&self) -> u32 {
        dir_items(self.0.whole)
    }
    fn entries(&self) -> Result<Vec<SynthEntry>, Errno> {
        dir_entries(&self.0)
    }
    fn lookup(&self, name: &[u8]) -> Result<Option<SynthEntry>, Errno> {
        dir_lookup(&self.0, name)
    }
}

struct PidFile {
    t: Target,
    what: What,
}

impl FileSource for PidFile {
    fn may_open(&self, _who: Cred, _write: bool) -> Result<(), Errno> {
        let (_, p) = self.t.live()?;
        // `proc_mem_open(PTRACE_MODE_READ)` for the files that show the memory.
        if matches!(self.what, What::Environ | What::Auxv | What::Maps | What::Smaps | What::SmapsRollup | What::Personality) && !may_read_mm(&p) {
            return Err(EACCES);
        }
        Ok(())
    }
    fn render(&self, _who: Cred) -> Result<Vec<u8>, Errno> {
        let (g, p) = self.t.live()?;
        let th = self.t.thread(&g);
        let t = &self.t;
        Ok(match self.what {
            What::Stat => stat::render_stat(t, &g, &p, th.as_deref(), may_read_mm(&p)).into_bytes(),
            What::Status => stat::render_status(t, &g, &p, th.as_deref()).into_bytes(),
            What::Statm => stat::render_statm(&g, &p).into_bytes(),
            What::Maps => maps::render_maps(&g, &p).into_bytes(),
            What::Smaps => maps::render_smaps(&g, &p)?.into_bytes(),
            What::SmapsRollup => maps::render_smaps_rollup(&g, &p).into_bytes(),
            What::Comm => {
                let mut c = stat::comm_of(&p, th.as_deref()).into_bytes();
                c.push(b'\n');
                c
            }
            What::Cmdline => text::cmdline(&g, &p)?,
            What::Environ => text::environ(&g, &p)?,
            What::Auxv => text::auxv(&g, &p),
            What::Limits => text::render_limits(&g).into_bytes(),
            What::OomScore => format!("{}\n", text::oom_score(&g, &p)).into_bytes(),
            What::OomScoreAdj => format!("{}\n", g.oom_score_adj()).into_bytes(),
            What::Mounts => mounts::render(&p, false).into_bytes(),
            What::MountInfo => mounts::render(&p, true).into_bytes(),
            // `proc_pid_personality`: V2 has no personality(2), every process runs as PER_LINUX.
            What::Personality => b"00000000\n".to_vec(),
            // `proc_cgroup_show` with only the unified hierarchy and no cgroups made: the root cgroup.
            What::Cgroup => b"0::/\n".to_vec(),
        })
    }
    fn write(&self, _who: Cred, data: &[u8]) -> Result<usize, Errno> {
        let (g, _) = self.t.live()?;
        match self.what {
            What::Comm => text::write_comm(&g, self.t.thread(&g).as_deref(), data),
            What::OomScoreAdj => text::write_oom_score_adj(&g, data, text::writer_has_sys_resource()),
            _ => Err(EIO),
        }
    }
}

