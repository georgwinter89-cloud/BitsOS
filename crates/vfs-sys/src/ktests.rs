//! FS-1 ktests. Every test is rerunnable in one boot (fresh views, unique scratch names, clean-up at the end):
//! on the Mini-PC Safe tests may run several times over MCP.

use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, Ordering};
use ktest::{TestResult, kassert, ktest};
use uapi::errno::*;
use vfs::ops::Ctx;
use vfs::{Cred, FdTable, FsFreeze, MountNs};

const SEC: u64 = 1_000_000_000;
const CASES: &str = include_str!("../../../host/tests/fs/cases.txt");
const USER: Cred = Cred { uid: 1000, gid: 1000 };
const SUBJECT: policy::Subject = policy::Subject { program: policy::ProgramId(1), pid: 0x7fff_0000, uid: 1000 };

static RUN: AtomicU32 = AtomicU32::new(0);

fn errno<T>(r: Result<T, i64>) -> i64 {
    match r {
        Ok(_) => 0,
        Err(e) => e,
    }
}

/// Every call of `vfs::sys` has a handler in the syscall registry.
fn registered() -> TestResult {
    for (nr, name, _) in vfs::sys::CALLS.iter().chain(vfs::pollsys::CALLS).chain(vfs::sys_attr::CALLS).chain(vfs::sys_copy::CALLS).chain(vfs::inotify::sys::CALLS) {
        kassert!(syscall::SYSCALLS.entries().iter().any(|e| e.nr == *nr), "{name} ({nr}) is not registered");
    }
    Ok(())
}
ktest!("fs.calls.registered", Safe, registered);

/// The boot view: tmpfs at `/` with the initramfs, `/tmp` 1777, device nodes; a program (uid 1000) may
/// create in /tmp but not in `/`, and reads what it wrote.
fn root_view() -> TestResult {
    let ns = vfs::root_ns();
    let root = ns.lookup_path(b"/", true).map_err(|e| format!("/: {e}"))?;
    kassert!(root.attr().mode == uapi::S_IFDIR | 0o755, "/ mode {:o}", root.attr().mode);
    let tmp = ns.lookup_path(b"/tmp", true).map_err(|e| format!("/tmp: {e}"))?;
    kassert!(tmp.attr().mode == uapi::S_IFDIR | 0o1777, "/tmp mode {:o}", tmp.attr().mode);
    kassert!(ns.lookup("/bin/hello").is_ok(), "/bin/hello missing (initramfs not unpacked)");
    kassert!(ns.lookup("/dev/console").is_ok(), "/dev/console missing");
    let c = Ctx { ns: &ns, root: ns.root_path(), cwd: ns.root_path(), cred: USER, umask: 0o022, subject: SUBJECT };
    let n = RUN.fetch_add(1, Ordering::Relaxed);
    let name = format!("/tmp/fs-ktest-{n}");
    kassert!(errno(c.open(None, b"/fs-ktest-denied", vfs::abi::O_CREAT | vfs::abi::O_WRONLY, 0o644)) == EACCES, "a program created a file in /");
    let f = c.open(None, name.as_bytes(), vfs::abi::O_CREAT | vfs::abi::O_EXCL | vfs::abi::O_RDWR, 0o666).map_err(|e| format!("create {name}: {e}"))?;
    kassert!(f.write(b"tmpfs at /").map_err(|e| format!("write: {e}"))? == 10);
    kassert!(f.stat().mode == uapi::S_IFREG | 0o644, "umask not applied: {:o}", f.stat().mode);
    kassert!(f.stat().uid == 1000);
    kassert!(ns.read_file(&name, 100).map_err(|e| format!("read: {e}"))? == b"tmpfs at /");
    // Sticky /tmp: another user may not remove it.
    let other = Ctx { ns: &ns, root: ns.root_path(), cwd: ns.root_path(), cred: Cred { uid: 1001, gid: 1001 }, umask: 0, subject: SUBJECT };
    kassert!(errno(other.unlink(None, name.as_bytes())) == EPERM, "sticky bit ignored");
    c.unlink(None, name.as_bytes()).map_err(|e| format!("unlink: {e}"))?;
    kassert!(ns.lookup(&name).err() == Some(ENOENT));
    Ok(())
}
ktest!("fs.vfs.root_view", Safe, root_view);

/// A directory chain far deeper than any kernel stack could recurse over: walks, `..`, getcwd-style paths and
/// the release of the whole tree must all be iterative.
fn deep_tree() -> TestResult {
    let ns = MountNs::new_empty();
    let c = Ctx::kernel(&ns);
    let depth = 20_000;
    let mut cur = ns.root_path();
    for _ in 0..depth {
        cur = c.mkdir(Some(&cur), b"d", 0o755).map_err(|e| format!("mkdir: {e}"))?;
    }
    let path = ns.path_of(&ns.root_path(), &cur).ok_or("deep path unreachable")?;
    kassert!(path.len() == depth * 2, "path length {}", path.len());
    let up = c.lookup(Some(&cur), b"../../..", true).map_err(|e| format!("..: {e}"))?;
    kassert!(Arc::ptr_eq(&up.dentry, &cur.dentry.parent().and_then(|p| p.parent()).and_then(|p| p.parent()).ok_or("no parent")?));
    drop(cur);
    drop(up);
    drop(c);
    drop(ns);
    // Also a removed chain kept alive only by an open file below it.
    let ns = MountNs::new_empty();
    let c = Ctx::kernel(&ns);
    let mut cur = ns.root_path();
    for _ in 0..depth {
        cur = c.mkdir(Some(&cur), b"e", 0o755).map_err(|e| format!("mkdir: {e}"))?;
    }
    let held = c.open(Some(&cur), b".", vfs::abi::O_RDONLY, 0).map_err(|e| format!("open: {e}"))?;
    drop(cur);
    ns.remove_tree("/e").map_err(|e| format!("remove_tree: {e}"))?;
    kassert!(ns.lookup("/e").err() == Some(ENOENT));
    drop(held);
    Ok(())
}
ktest!("fs.vfs.deep_tree", Safe, deep_tree);

/// fd tables grow on demand (R1) up to RLIMIT_NOFILE, the lowest free number is taken, EMFILE at the limit.
fn many_fds() -> TestResult {
    let ns = MountNs::new_empty();
    ns.create_file("/f", 0o644, Vec::from(&b"x"[..])).map_err(|e| format!("{e}"))?;
    let t = FdTable::new();
    kassert!(t.limit() == 1024);
    let f = ns.open_flags(b"/f", vfs::abi::O_RDONLY, 0).map_err(|e| format!("{e}"))?;
    t.set_limit(50_000);
    for i in 0..20_000 {
        kassert!(t.install(f.clone()) == Ok(i), "fd {i}");
    }
    t.close(777).map_err(|e| format!("{e}"))?;
    kassert!(t.install(f.clone()) == Ok(777));
    t.set_limit(20_000);
    kassert!(t.install(f.clone()) == Err(EMFILE));
    Ok(())
}
ktest!("fs.vfs.many_fds", Safe, many_fds);

/// B1: a frozen tmpfs holds writers back until thaw; a second freeze is EBUSY.
fn freeze() -> TestResult {
    let ns = Arc::new(MountNs::new_empty());
    ns.create_file("/f", 0o644, Vec::new()).map_err(|e| format!("{e}"))?;
    let sb = ns.root().sb.clone();
    sb.freeze().map_err(|e| format!("freeze: {e}"))?;
    kassert!(sb.freeze() == Err(EBUSY));
    let f = ns.open_flags(b"/f", vfs::abi::O_WRONLY, 0).map_err(|e| format!("{e}"))?;
    let done = Arc::new(AtomicU32::new(0));
    let d2 = done.clone();
    let h = sched::spawn("fs-ktest-writer", sched::Class::Normal, move || {
        let _ = f.write(b"after thaw");
        d2.store(1, Ordering::Release);
    })
    .map_err(|e| format!("spawn: {e:?}"))?;
    sched::sleep_ns(SEC / 20);
    let early = done.load(Ordering::Acquire);
    sb.thaw();
    let joined = h.join_timeout(5 * SEC);
    kassert!(early == 0, "a write went through a frozen file system");
    kassert!(joined && done.load(Ordering::Acquire) == 1, "the writer did not finish after thaw");
    kassert!(ns.read_file("/f", 100).map_err(|e| format!("{e}"))? == b"after thaw");
    Ok(())
}
ktest!("fs.vfs.freeze", Safe, freeze);

/// R8: the tmpfs budget is measured from the frame allocator (half of its memory, Linux's tmpfs default) and
/// covers what is in use (the initramfs copy included).
fn tmpfs_budget() -> TestResult {
    let limit = vfs::inode::tmpfs_page_limit();
    let used = vfs::inode::tmpfs_pages_used();
    kassert!(limit != u64::MAX, "no budget: the frame allocator was not measured");
    kassert!(used < limit, "used {used} pages of {limit}");
    Ok(())
}
ktest!("fs.vfs.tmpfs_budget", Safe, tmpfs_budget);

/// The case list of host/tests/fs/cases.txt (checked against Linux) run by the static C program
/// `/bin/fs-cases` through the real syscall path, as uid 1000 on a plain tmpfs under /tmp.
fn linux_cases() -> TestResult {
    let ns = vfs::root_ns();
    if ns.lookup("/bin/fs-cases").is_err() {
        return ktest::skip("/bin/fs-cases is not in this image (xtask change request in plan/notes/fs-FS-1.md)");
    }
    let n = RUN.fetch_add(1, Ordering::Relaxed);
    // FS-13: the cases run on a plain tmpfs of their own: /tmp is nodev on V2 (AB-7c) but was not where CachyOS
    // recorded them (`open wh1` of a 0:0 character node is ENXIO there, EACCES on a nodev mount).
    let mnt = crate::fs13_ktests::ScratchMount::new(format!("/tmp/fs-cases-m{n}"), 0o1777, None, None)?;
    let base = format!("{}/k", mnt.path);
    let cases = format!("{}/cases.txt", mnt.path);
    ns.create_file(&cases, 0o644, Vec::from(CASES.as_bytes())).map_err(|e| format!("{cases}: {e}"))?;
    let out = proc::stdio::Capture::new(1 << 16);
    let err = proc::stdio::Capture::new(1 << 16);
    let argv = ["fs-cases", cases.as_str(), base.as_str()];
    let env = ["HOME=/", "PATH=/bin"];
    let mut o = proc::SpawnOptions::new(&argv, &env, policy::ProgramId(1));
    o.stdout = Some(out.clone() as Arc<dyn vfs::File>);
    o.stderr = Some(err.clone() as Arc<dyn vfs::File>);
    let p = proc::spawn_with("/bin/fs-cases", &o).map_err(|e| format!("spawn: {e}"))?;
    let code = p.wait(Some(60 * SEC));
    let text = String::from_utf8_lossy(&out.contents()).into_owned();
    let etext = String::from_utf8_lossy(&err.contents()).into_owned();
    drop(mnt);
    for l in text.lines().filter(|l| l.starts_with("FAIL")).take(20) {
        klog::event!(Warn, "fs", "case.fail", line = l);
    }
    kassert!(code == Some(0), "fs-cases ended {:?} ({:?}): {} stderr {etext:?}", code, p.end_message(), text.lines().last().unwrap_or(""));
    kassert!(text.contains(" passed, 0 failed"), "output: {text}");
    Ok(())
}
ktest!("fs.calls.linux_cases", Safe, linux_cases);
