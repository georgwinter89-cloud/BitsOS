//! PF-7 ktests, through the real per-syscall path (`super::run`) of a held program:
//! - `fs.pf7.no_shared_lock`: with a watch elsewhere in the system, preads of an unwatched file take neither
//!   `vfs.dentry.link` nor `proc.fs`, `proc.ownfs`, `proc.cred`, `proc.nsset`; also with another program in a copied
//!   mount view (the real machine's state: bitsmcp has one) and in such a view itself; opens in the boot view take no
//!   `proc.nsset` either;
//! - `fs.pf7.watched_still_reports`: the parent's and the file's own watch report from the next read on;
//! - `fs.pf7.moved_and_every_read_path`: a file renamed into or out of a watched directory after it was seen quiet,
//!   through read, readv and pread on the kernel's own entries, and getdents on a watched directory's child;
//! - `fs.pf7.watch_during_reads`: a watch added while another thread keeps reading hears that thread's next read;
//! - `fs.pf7.ids_and_cwd_follow`: a credential change and a chdir count from the very next call;
//! - `fs.pf7.ids_busy`: while the published ids count as being written the call takes `proc.cred` (counted) and
//!   gets the same ids; a writer in another thread never shows a reader half a set;
//! - `fs.pf7.sections`: `grace::in_section` counts open sections on this CPU, nested ones too;
//! - `fs.pf7.view_follows`: a call keeps the view it first asked for, the next call has the group's new one, also on
//!   the way back into the boot view.
//! Safe and rerunnable: names are fresh per run, the program is killed at the end. No timing is judged.

use super::run;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use hal::entry::UserContext;
use ktest::{TestResult, kassert, ktest};
use proc::Process;
use syscall::SysCtx;
use uapi::errno::EACCES;
use uapi::ab::clone::CLONE_NEWNS;
use vfs::inotify::{Group, IN_ACCESS, IN_ISDIR};
use vfs::node::File;
use vfs::sys as v;

const SEC: u64 = 1_000_000_000;
const SCRATCH: u64 = 4 * 4096;
const AT_READS: usize = 2000;
const LOCKS: [&str; 5] = ["vfs.dentry.link", "proc.fs", "proc.ownfs", "proc.cred", "proc.nsset"];
static RUN: AtomicU64 = AtomicU64::new(0);

ktest!("fs.pf7.no_shared_lock", Safe, no_shared_lock);
ktest!("fs.pf7.watched_still_reports", Safe, watched_still_reports);
ktest!("fs.pf7.moved_and_every_read_path", Safe, moved_and_every_read_path);
ktest!("fs.pf7.watch_during_reads", Safe, watch_during_reads);
ktest!("fs.pf7.ids_and_cwd_follow", Safe, ids_and_cwd_follow);
ktest!("fs.pf7.ids_busy", Safe, ids_busy);
ktest!("fs.pf7.sections", Safe, sections);
ktest!("fs.pf7.view_follows", Safe, view_follows);

/// A held program whose syscalls this thread plays, and a fresh directory with a file of 8 bytes.
struct T {
    p: Arc<Process>,
    th: Arc<proc::Thread>,
    ctx: UserContext,
    mem: u64,
    dir: String,
    _pin: proc::MmPin,
}

impl T {
    fn new(what: &str) -> Result<T, String> {
        Self::new_as(what, proc::SpawnOptions::new(&[], &[], policy::ProgramId(1)).uid)
    }
    fn new_as(what: &str, uid: u32) -> Result<T, String> {
        let argv = ["fs-inotify"];
        let mut o = proc::SpawnOptions::new(&argv, &[], policy::ProgramId(1));
        o.held = true;
        o.uid = uid;
        let p = proc::spawn_with("/bin/fs-inotify", &o).map_err(|e| format!("spawn: {e}"))?;
        let mem = p.mm.sys_mmap(0, SCRATCH, 3, 0x22, 0).map_err(|e| format!("mmap: {e:?}"))?;
        let th = p.threads().into_iter().next().ok_or("the held program has no thread")?;
        let _pin = proc::pin_mm(&p);
        let dir = format!("/tmp/fs-pf7-{what}-{}-{}", time::now_ns() / 1_000_000, RUN.fetch_add(1, Ordering::Relaxed));
        let d = vfs::root_ns().mkdir_p(&dir).map_err(|e| format!("mkdir {dir}: {e}"))?;
        d.inode.meta.set_perm(0o777);
        vfs::root_ns().create_file(&format!("{dir}/f"), 0o644, alloc::vec![7u8; 8]).map_err(|e| format!("create: {e}"))?;
        Ok(T { p, th, ctx: UserContext::new(0x40_0078, hal::USER_TOP - 4096), mem, dir, _pin })
    }
    fn call(&mut self, f: v::SysFn, args: &[u64]) -> i64 {
        let mut a = [0u64; 6];
        a[..args.len()].copy_from_slice(args);
        run(&mut SysCtx { proc: &self.p, thread: &self.th, user: &mut self.ctx, args: a }, f)
    }
    /// `s` as a C string in the program's memory.
    fn cstr(&self, s: &str) -> u64 {
        let mut b: Vec<u8> = s.as_bytes().to_vec();
        b.push(0);
        let _ = self.p.mm.copy_to_user(self.mem + 4096, &b);
        self.mem + 4096
    }
    /// A second C string, for calls that take two names.
    fn cstr2(&self, s: &str) -> u64 {
        let mut b: Vec<u8> = s.as_bytes().to_vec();
        b.push(0);
        let _ = self.p.mm.copy_to_user(self.mem + 2 * 4096, &b);
        self.mem + 2 * 4096
    }
    fn rename(&mut self, from: &str, to: &str) -> i64 {
        let (a, b) = (self.cstr(from), self.cstr2(to));
        self.call(v::rename, &[a, b])
    }
    fn open(&mut self, path: &str) -> i64 {
        let at = self.cstr(path);
        self.call(v::open, &[at, 0, 0])
    }
    fn pread(&mut self, fd: i64) -> i64 {
        let buf = self.mem;
        self.call(v::pread64, &[fd as u64, buf, 8, 0])
    }
}

impl Drop for T {
    fn drop(&mut self) {
        self.p.kill(9);
        self.p.wait(Some(5 * SEC));
        let _ = vfs::root_ns().remove(&format!("{}/f", self.dir));
        let _ = vfs::root_ns().remove(&format!("{}/mine", self.dir));
        for name in ["a/g", "b/g", "a", "b"] {
            let _ = vfs::root_ns().remove(&format!("{}/{name}", self.dir));
        }
        let _ = vfs::root_ns().remove(&self.dir);
    }
}

fn acquired(name: &str) -> u64 {
    ksync::stats::class(name).map_or(0, |c| c.acquired)
}

/// How often each of `LOCKS` was taken by `reads` preads of `fd` (and the calls that found the ids busy): best of three
/// runs, because what other programs of a live machine take during a run is not this read's.
fn locks_of_reads(t: &mut T, fd: i64) -> Result<([u64; LOCKS.len()], u64), String> {
    let mut best = [u64::MAX; LOCKS.len()];
    let mut best_locked = u64::MAX;
    for _ in 0..3 {
        let before = LOCKS.map(acquired);
        let locked = proc::cred::ids_locked();
        for _ in 0..AT_READS {
            kassert!(t.pread(fd) == 8, "pread");
        }
        for (i, l) in LOCKS.iter().enumerate() {
            best[i] = best[i].min(acquired(l) - before[i]);
        }
        best_locked = best_locked.min(proc::cred::ids_locked() - locked);
    }
    Ok((best, best_locked))
}

fn check_locks(what: &str, (best, best_locked): ([u64; LOCKS.len()], u64)) -> TestResult {
    kassert!(best_locked == 0, "{what}: {best_locked} calls found the published ids busy with no writer");
    for (l, n) in LOCKS.iter().zip(best) {
        klog::event!(Info, "fs", "pf7.locks", what = what, class = *l, reads = AT_READS, acquired = n);
        kassert!(n < AT_READS as u64 / 20, "{what}: {AT_READS} reads of an unwatched file took {l} {n} times");
    }
    Ok(())
}

fn no_shared_lock() -> TestResult {
    let mut t = T::new("locks")?;
    let fd = t.open(&format!("{}/f", t.dir));
    kassert!(fd >= 0, "open: {fd}");
    // A watch elsewhere: "any watch alive" says yes, as on a machine with a compositor running.
    let other = vfs::root_ns().mkdir_p(&format!("{}/other", t.dir)).map_err(|e| format!("mkdir: {e}"))?;
    let g = vfs::inotify::sys::new_group_for_kernel(0).map_err(|e| format!("group: {e}"))?;
    kassert!(g.update_watch(&other.inode, IN_ACCESS).is_ok(), "watch");
    let quiet = locks_of_reads(&mut t, fd)?;

    // What the real machine has and a bare test boot has not: a program in a mount view of its own (bitsmcp's
    // `private_mounts`, a browser's sandbox). Its life must cost the other programs nothing.
    let mut own = T::new_as("own-view", 0)?;
    let r = own.p.unshare(&own.th, CLONE_NEWNS);
    kassert!(r.is_ok(), "unshare: {r:?}");
    let with_copy = locks_of_reads(&mut t, fd)?;

    // A name looked up goes to the view: still none of the group's `proc.nsset` for a program in the boot view.
    let path = format!("{}/f", t.dir);
    let named = nsset_of_opens(&mut t, &path)?;

    // The program in its own view: reads on an fd never ask for the view at all.
    let own_path = format!("{}/f", own.dir);
    let in_copy_fd = own.open(&own_path);
    kassert!(in_copy_fd >= 0, "open in the own view: {in_copy_fd}");
    let in_copy = locks_of_reads(&mut own, in_copy_fd)?;

    // A name looked up there asks once per call, its group's own lock (plan/notes/fs-FS-W33.md): once, not more. A later
    // lock-free holder passes this too.
    let named_in_copy = nsset_of_opens(&mut own, &own_path)?;
    klog::event!(Info, "fs", "pf7.path_in_copy", opens = AT_READS, acquired = named_in_copy);

    drop(g);
    drop(other);
    let _ = vfs::root_ns().remove(&format!("{}/other", t.dir));
    check_locks("quiet", quiet)?;
    check_locks("a copied view alive", with_copy)?;
    check_locks("in a copied view", in_copy)?;
    kassert!(named < AT_READS as u64 / 20, "{AT_READS} opens in the boot view took proc.nsset {named} times with a copied view alive");
    kassert!(named_in_copy < AT_READS as u64 * 3 / 2, "{AT_READS} open and close in a copied view took proc.nsset {named_in_copy} times");
    Ok(())
}

/// How often `AT_READS` open and close of `path` took `proc.nsset`: best of three runs, as `locks_of_reads`.
fn nsset_of_opens(t: &mut T, path: &str) -> Result<u64, String> {
    let mut best = u64::MAX;
    for _ in 0..3 {
        let before = acquired("proc.nsset");
        for _ in 0..AT_READS {
            let f = t.open(path);
            kassert!(f >= 0, "open: {f}");
            kassert!(t.call(v::close, &[f as u64]) == 0, "close");
        }
        best = best.min(acquired("proc.nsset") - before);
    }
    Ok(best)
}

fn view_follows() -> TestResult {
    use proc::ns::mnt::CallView;
    use vfs::sys::ViewSource;
    let t = T::new_as("view", 0)?;
    let boot = t.p.ns.clone();
    let call = CallView::new(&t.p);
    kassert!(Arc::ptr_eq(call.view(), &boot), "a new program is in the boot view");
    let r = t.p.unshare(&t.th, CLONE_NEWNS);
    kassert!(r.is_ok(), "unshare: {r:?}");
    kassert!(Arc::ptr_eq(call.view(), &boot), "a call keeps the view it asked for");
    let copy = t.p.mount_ns();
    kassert!(!Arc::ptr_eq(&copy, &boot), "unshare made a copy");
    kassert!(Arc::ptr_eq(CallView::new(&t.p).view(), &copy), "the next call has the copy");
    let mut held = None;
    kassert!(Arc::ptr_eq(t.p.mount_view(&mut held), &copy), "mount_view has the copy");
    let call = CallView::new(&t.p);
    kassert!(Arc::ptr_eq(call.view(), &copy), "a call in the copy");
    let r = t.p.setns(&t.th, &proc::ns::file::NsRef::Mnt(proc::ns::mnt::init_mnt().clone()), CLONE_NEWNS);
    kassert!(r.is_ok(), "setns: {r:?}");
    kassert!(Arc::ptr_eq(call.view(), &copy), "a call keeps the copy through setns");
    kassert!(Arc::ptr_eq(CallView::new(&t.p).view(), &boot), "the next call is back in the boot view");
    let mut held = None;
    kassert!(Arc::ptr_eq(t.p.mount_view(&mut held), &boot) && held.is_none(), "mount_view borrows the boot view again");
    Ok(())
}

/// (mask, name) of every queued event.
fn drain(g: &Group) -> Vec<(u32, Vec<u8>)> {
    let mut b = alloc::vec![0u8; 4096];
    let n = g.read_stream(&mut b, true).unwrap_or(0);
    let mut out = Vec::new();
    let mut at = 0;
    while at + 16 <= n {
        let w = |o: usize| u32::from_le_bytes([b[at + o], b[at + o + 1], b[at + o + 2], b[at + o + 3]]);
        let len = w(12) as usize;
        let name = &b[at + 16..at + 16 + len];
        let end = name.iter().position(|&c| c == 0).unwrap_or(len);
        out.push((w(4), name[..end].to_vec()));
        at += 16 + len;
    }
    out
}

fn watched_still_reports() -> TestResult {
    let mut t = T::new("watch")?;
    let fd = t.open(&format!("{}/f", t.dir));
    kassert!(fd >= 0, "open: {fd}");
    kassert!(t.pread(fd) == 8, "pread");
    let ns = vfs::root_ns();
    let dir = ns.lookup_path(t.dir.as_bytes(), true).map_err(|e| format!("lookup: {e}"))?;
    let file = ns.lookup_path(format!("{}/f", t.dir).as_bytes(), true).map_err(|e| format!("lookup: {e}"))?;
    let g = vfs::inotify::sys::new_group_for_kernel(0).map_err(|e| format!("group: {e}"))?;
    let wd = g.update_watch(&dir.dentry.inode, IN_ACCESS).map_err(|e| format!("watch: {e}"))?;
    kassert!(t.pread(fd) == 8, "pread");
    let got = drain(&g);
    kassert!(got == [(IN_ACCESS, b"f".to_vec())], "the parent's new watch hears the next read: {got:?}");
    g.remove_watch(wd).map_err(|e| format!("rm: {e}"))?;
    let _ = drain(&g);
    kassert!(t.pread(fd) == 8, "pread");
    kassert!(drain(&g).is_empty(), "removed: silence");
    g.update_watch(&file.dentry.inode, IN_ACCESS).map_err(|e| format!("watch: {e}"))?;
    kassert!(t.pread(fd) == 8, "pread");
    let got = drain(&g);
    kassert!(got == [(IN_ACCESS, Vec::new())], "the file's own watch: {got:?}");
    Ok(())
}

/// The kernel's own read, readv and getdents entries too, and a file that moves after it was seen quiet.
fn moved_and_every_read_path() -> TestResult {
    let mut t = T::new("moved")?;
    let ns = vfs::root_ns();
    let (a, b) = (format!("{}/a", t.dir), format!("{}/b", t.dir));
    for d in [&a, &b] {
        ns.mkdir_p(d).map_err(|e| format!("mkdir {d}: {e}"))?.inode.meta.set_perm(0o777);
    }
    ns.create_file(&format!("{a}/g"), 0o644, alloc::vec![7u8; 8]).map_err(|e| format!("create: {e}"))?;
    let fd = t.open(&format!("{a}/g"));
    kassert!(fd >= 0, "open: {fd}");
    let g = vfs::inotify::sys::new_group_for_kernel(0).map_err(|e| format!("group: {e}"))?;
    let bdir = ns.lookup_path(b.as_bytes(), true).map_err(|e| format!("lookup: {e}"))?;
    g.update_watch(&bdir.dentry.inode, IN_ACCESS).map_err(|e| format!("watch: {e}"))?;
    // Seen quiet in the unwatched directory, by every road.
    let iov = t.mem + 3 * 4096;
    let mut one = [0u8; 16];
    one[..8].copy_from_slice(&t.mem.to_le_bytes());
    one[8..].copy_from_slice(&1u64.to_le_bytes());
    t.p.mm.copy_to_user(iov, &one).map_err(|e| format!("iovec: {e:?}"))?;
    let (mem, g_fd) = (t.mem, fd as u64);
    kassert!(t.call(super::read_any, &[g_fd, mem, 1]) == 1, "read");
    kassert!(t.call(super::readv_any, &[g_fd, iov, 1]) == 1, "readv");
    kassert!(t.pread(fd) == 8, "pread");
    kassert!(drain(&g).is_empty(), "the file's directory has no watch");
    // Into the watched directory: every road reports, under its new name's directory.
    kassert!(t.rename(&format!("{a}/g"), &format!("{b}/g")) == 0, "rename in");
    let heard: Vec<(u32, Vec<u8>)> = alloc::vec![(IN_ACCESS, b"g".to_vec())];
    kassert!(t.call(super::read_any, &[g_fd, mem, 1]) == 1, "read");
    kassert!(drain(&g) == heard, "read after the move in");
    kassert!(t.call(super::readv_any, &[g_fd, iov, 1]) == 1, "readv");
    kassert!(drain(&g) == heard, "readv after the move in");
    kassert!(t.pread(fd) == 8, "pread");
    kassert!(drain(&g) == heard, "pread after the move in");
    // And out again: silence.
    kassert!(t.rename(&format!("{b}/g"), &format!("{a}/g")) == 0, "rename out");
    kassert!(t.call(super::read_any, &[g_fd, mem, 1]) == 1, "read");
    kassert!(t.pread(fd) == 8, "pread");
    kassert!(drain(&g).is_empty(), "moved out: silence");
    // getdents on a directory: its parent's watch hears it with IN_ISDIR, from the next call after the watch.
    let dirfd = t.open(&a);
    kassert!(dirfd >= 0, "open dir: {dirfd}");
    kassert!(t.call(v::getdents64, &[dirfd as u64, mem, 4096]) > 0, "getdents");
    let parent = ns.lookup_path(t.dir.as_bytes(), true).map_err(|e| format!("lookup: {e}"))?;
    g.update_watch(&parent.dentry.inode, IN_ACCESS).map_err(|e| format!("watch: {e}"))?;
    let (at, seek) = (dirfd as u64, v::lseek as v::SysFn);
    kassert!(t.call(seek, &[at, 0, 0]) == 0, "rewind");
    kassert!(t.call(v::getdents64, &[at, mem, 4096]) > 0, "getdents");
    let got = drain(&g);
    kassert!(got == [(IN_ACCESS | IN_ISDIR, b"a".to_vec())], "getdents under a new watch: {got:?}");
    Ok(())
}

/// A watch added while another thread reads: once `update_watch` returned, that thread's next read reports; a watch
/// taken away and put back in rounds, so that the dentry has remembered "quiet" before each new watch.
fn watch_during_reads() -> TestResult {
    const ROUNDS: usize = 50;
    let mut t = T::new("race")?;
    let fd = t.open(&format!("{}/f", t.dir));
    kassert!(fd >= 0, "open: {fd}");
    let ns = vfs::root_ns();
    let dir = ns.lookup_path(t.dir.as_bytes(), true).map_err(|e| format!("lookup: {e}"))?;
    let g = vfs::inotify::sys::new_group_for_kernel(0).map_err(|e| format!("group: {e}"))?;
    let reads = Arc::new(AtomicU64::new(0));
    let failed = Arc::new(AtomicU64::new(0));
    let stop = Arc::new(AtomicBool::new(false));
    let h = {
        let (p, th, mem) = (t.p.clone(), t.th.clone(), t.mem);
        let (reads, failed, stop) = (reads.clone(), failed.clone(), stop.clone());
        sched::spawn("fs-ktest-pf7", sched::Class::Normal, move || {
            let mut ctx = UserContext::new(0x40_0078, hal::USER_TOP - 4096);
            while !stop.load(Ordering::Acquire) {
                let args = [fd as u64, mem, 8, 0, 0, 0];
                if run(&mut SysCtx { proc: &p, thread: &th, user: &mut ctx, args }, v::pread64) != 8 {
                    failed.fetch_add(1, Ordering::SeqCst);
                }
                reads.fetch_add(1, Ordering::SeqCst);
                sched::yield_now();
            }
        })
        .map_err(|e| format!("spawn: {e:?}"))?
    };
    // Two reads that end after now: at least the second began after it.
    let two_more = || {
        let (seen, until) = (reads.load(Ordering::SeqCst), time::now_ns() + 5 * SEC);
        while reads.load(Ordering::SeqCst) < seen + 2 {
            if time::now_ns() > until {
                return false;
            }
            sched::yield_now();
        }
        true
    };
    let result = (|| -> TestResult {
        let heard = (IN_ACCESS, b"f".to_vec());
        for round in 0..ROUNDS {
            kassert!(two_more(), "the reader made no progress in 5 s");
            let wd = g.update_watch(&dir.dentry.inode, IN_ACCESS).map_err(|e| format!("watch: {e}"))?;
            kassert!(two_more(), "the reader made no progress in 5 s");
            let got = drain(&g);
            // Equal events in a row merge in the queue, but the reader goes on while this read takes them: one or more.
            kassert!(!got.is_empty() && got.iter().all(|e| *e == heard), "round {round}: {got:?}");
            g.remove_watch(wd).map_err(|e| format!("rm: {e}"))?;
            kassert!(two_more(), "the reader made no progress in 5 s");
            let _ = drain(&g);
        }
        Ok(())
    })();
    stop.store(true, Ordering::Release);
    kassert!(h.join_timeout(10 * SEC), "the reader did not end in 10 s");
    kassert!(failed.load(Ordering::SeqCst) == 0, "{} preads failed", failed.load(Ordering::SeqCst));
    result
}

fn ids_and_cwd_follow() -> TestResult {
    let mut t = T::new("ids")?;
    // chdir counts from the next call, relative names with it.
    let at = t.cstr(&t.dir.clone());
    kassert!(t.call(v::chdir, &[at]) == 0, "chdir");
    let fd = t.open("f");
    kassert!(fd >= 0, "open relative: {fd}");
    // A file of the program's own, closed to everybody else.
    let old = t.p.fs_cred();
    kassert!(old.uid != 0, "the program runs as root: this check needs another user");
    let at = t.cstr("mine");
    let mine = t.call(v::open, &[at, 0o100, 0o600]);
    kassert!(mine >= 0, "create: {mine}");
    kassert!(t.open("mine") >= 0, "its owner opens it");
    // A new fsuid counts from the next call, and the ids move as one set.
    let new = old.uid + 1;
    let th = t.th.clone();
    t.p.change_cred(&th, |c| Ok(Some(proc::cred::Cred { fsuid: new, fsgid: new, euid: new, ..c.clone() }))).map_err(|e| format!("change_cred: {e}"))?;
    let ids = t.p.group().call_ids();
    kassert!(ids.fs == vfs::Cred { uid: new, gid: new } && ids.euid == new, "published: {ids:?}");
    kassert!(t.p.subject().uid == new && t.p.fs_cred() == ids.fs, "subject and fs view follow");
    kassert!(t.open("mine") == -EACCES, "the new fsuid is refused");
    kassert!(t.pread(fd) == 8, "an open fd keeps working");
    Ok(())
}

fn ids_busy() -> TestResult {
    const CHANGES: u32 = 200;
    let mut t = T::new("busy")?;
    let fd = t.open(&format!("{}/f", t.dir));
    kassert!(fd >= 0, "open: {fd}");
    let group = t.p.group().clone();
    let want = group.call_ids();
    let before = proc::cred::ids_locked();
    {
        let _busy = group.published_ids_held_for_test();
        kassert!(group.call_ids() == want, "the locked way answers the same ids");
        kassert!(proc::cred::ids_locked() > before, "the fallback was not counted");
        kassert!(t.pread(fd) == 8, "a read while the ids are busy");
    }
    kassert!(group.call_ids() == want, "after the hold");
    // A writer in another thread: readers see old or new, never half of it.
    let base = want.fs.uid + 1;
    let set = |n: u32| proc::cred::CallIds { fs: vfs::Cred { uid: n, gid: n }, euid: n };
    let th = t.th.clone();
    t.p.change_cred(&th, |c| Ok(Some(proc::cred::Cred { fsuid: base, fsgid: base, euid: base, ..c.clone() }))).map_err(|e| format!("change_cred: {e}"))?;
    let done = Arc::new(AtomicBool::new(false));
    let h = {
        let (p, th, done) = (t.p.clone(), t.th.clone(), done.clone());
        sched::spawn("fs-ktest-pf7w", sched::Class::Normal, move || {
            for n in 0..CHANGES {
                let id = base + n % 3;
                let _ = p.change_cred(&th, |c| Ok(Some(proc::cred::Cred { fsuid: id, fsgid: id, euid: id, ..c.clone() })));
                sched::yield_now();
            }
            done.store(true, Ordering::Release);
        })
        .map_err(|e| format!("spawn: {e:?}"))?
    };
    let until = time::now_ns() + 10 * SEC;
    let mut torn = 0u64;
    while !done.load(Ordering::Acquire) && time::now_ns() < until {
        let ids = group.call_ids();
        if ![base, base + 1, base + 2].iter().any(|&n| ids == set(n)) {
            torn += 1;
        }
        sched::yield_now();
    }
    kassert!(h.join_timeout(10 * SEC), "the writer did not end in 10 s");
    kassert!(torn == 0, "{torn} reads saw half a set");
    Ok(())
}

fn sections() -> TestResult {
    use mm_core::grace;
    // A ktest thread may move between sections: each look stays inside its own section.
    let a = grace::enter().ok_or("no slot")?;
    kassert!(grace::in_section(), "one open");
    let b = grace::enter().ok_or("no slot")?;
    kassert!(grace::in_section(), "two open");
    drop(b);
    kassert!(grace::in_section(), "the outer one still");
    drop(a);
    Ok(())
}
