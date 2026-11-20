//! EINS-C (K5) ktests: the assistant's tree through the real dispatcher and real programs. All Safe and
//! rerunnable (own processes, own files under /tmp/fs-tree*).
//!
//! - `fs.tree.enter`: a program writes `enter` to /proc/self/bitsos_tree and forks: both are in one fresh tree,
//!   /proc says so to anyone, a process outside says `none`; nobody else may write the file.
//! - `fs.tree.stays`: enter once only, never out; the chat label; set*id keeps the tree.
//! - `fs.tree.guard`: the Subject of every file call carries it: a root process of the tree may not open the
//!   stick's CONFIG.TXT for writing (EPERM before anything changes); reading stays.
//! - `fs.tree.clone`: fork, vfork (CLONE_VM|CLONE_VFORK), CLONE_PARENT (the sibling still comes from the tree),
//!   a new user and mount namespace: every child is in the caller's tree.
//! - `fs.tree.exec`: execve (a plain program and a set-user-ID one) keeps the tree.

use crate::ab6_ktests::T;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use ktest::{TestResult, kassert, ktest};
use proc::Process;
use uapi::ab::clone::*;
use uapi::errno::*;
use uapi::nr;

const SEC: u64 = 1_000_000_000;
const SIGCHLD: u64 = 17;
// nanosleep({100, 0}); jmp .
const SLEEPER: [u8; 18] = [0x6a, 0x00, 0x6a, 0x64, 0x48, 0x89, 0xe7, 0x31, 0xf6, 0xb8, 0x23, 0, 0, 0, 0x0f, 0x05, 0xeb, 0xfe];
/// fd = open("/proc/self/bitsos_tree", O_WRONLY); write(fd, "enter\n", 6) == 6 (else exit 1 / 2); fork(); then
/// both sleep in a loop (nanosleep 100 s).
const ENTER: [u8; 122] = [
    0x48, 0x8d, 0x3d, 0x56, 0x00, 0x00, 0x00, 0xbe, 0x01, 0x00, 0x00, 0x00, 0xb8, 0x02, 0x00, 0x00, 0x00, 0x0f, 0x05, 0x85, 0xc0, 0x78,
    0x33, 0x89, 0xc7, 0x48, 0x8d, 0x35, 0x54, 0x00, 0x00, 0x00, 0xba, 0x06, 0x00, 0x00, 0x00, 0xb8, 0x01, 0x00, 0x00, 0x00, 0x0f, 0x05,
    0x83, 0xf8, 0x06, 0x75, 0x20, 0xb8, 0x39, 0x00, 0x00, 0x00, 0x0f, 0x05, 0x6a, 0x00, 0x6a, 0x64, 0x48, 0x89, 0xe7, 0x31, 0xf6, 0xb8,
    0x23, 0x00, 0x00, 0x00, 0x0f, 0x05, 0xeb, 0xee, 0xbf, 0x01, 0x00, 0x00, 0x00, 0xeb, 0x05, 0xbf, 0x02, 0x00, 0x00, 0x00, 0xb8, 0xe7,
    0x00, 0x00, 0x00, 0x0f, 0x05, 0x2f, 0x70, 0x72, 0x6f, 0x63, 0x2f, 0x73, 0x65, 0x6c, 0x66, 0x2f, 0x62, 0x69, 0x74, 0x73, 0x6f, 0x73,
    0x5f, 0x74, 0x72, 0x65, 0x65, 0x00, 0x65, 0x6e, 0x74, 0x65, 0x72, 0x0a,
];

fn until(secs: u64, mut f: impl FnMut() -> bool) -> bool {
    let end = time::now_ns() + secs * SEC;
    while time::now_ns() < end {
        if f() {
            return true;
        }
        sched::sleep_ns(5_000_000);
    }
    f()
}

/// Killed and collected at the end.
struct Owned(Vec<Arc<Process>>);
impl Drop for Owned {
    fn drop(&mut self) {
        for p in &self.0 {
            if !p.has_exited() {
                p.kill(9);
            }
            let _ = p.wait(Some(5 * SEC));
        }
    }
}

fn tree_text(pid: u32) -> Result<String, String> {
    crate::ab7_ktests::text(&format!("/proc/{pid}/bitsos_tree"))
}

fn children(of: u32) -> Vec<Arc<Process>> {
    proc::processes().into_iter().filter(|p| p.ppid() == of).collect()
}

fn enter() -> TestResult {
    let img = crate::ktests::elf(&ENTER);
    let p = proc::spawn_image("fs-tree-enter", &img, &["fs-tree-enter"], &[], policy::ProgramId(1)).map_err(|e| format!("spawn: {e:?}"))?;
    let mut own = Owned(alloc::vec![p.clone()]);
    kassert!(until(10, || p.group().tree_id() != 0 && !children(p.pid).is_empty() || p.has_exited()), "no tree and no child");
    kassert!(!p.has_exited(), "the program ended: {:?}", p.status());
    let kids = children(p.pid);
    own.0.extend(kids.iter().cloned());
    let id = p.group().tree_id();
    kassert!(kids.len() == 1 && kids[0].group().tree_id() == id, "the child is not in tree {id}");
    let text = tree_text(p.pid)?;
    kassert!(text.starts_with(&format!("tree={id} root={} since=", p.pid)) && text.ends_with(" chat=-\n"), "{text:?}");
    kassert!(tree_text(kids[0].pid)? == text, "the child's file differs");
    kassert!(p.subject().tree == id && kids[0].subject().tree == id, "the Subject lacks the tree");
    let other = T::new("fs-tree-outside")?;
    kassert!(tree_text(other.p.pid)? == "none\n", "a process outside: {:?}", tree_text(other.p.pid));
    // nobody else writes it: not the kernel, not another process
    let ns = vfs::root_ns();
    for (pid, what) in [(p.pid, "enter"), (other.p.pid, "enter"), (p.pid, "chat x")] {
        let r = ns.write_file(&format!("/proc/{pid}/bitsos_tree"), what.as_bytes(), None);
        kassert!(r == Err(EPERM), "a foreign write of {what:?} to {pid}: {r:?}");
    }
    kassert!(policy::tree::get(id).is_some_and(|t| t.root == p.pid), "the registry lacks tree {id}");
    Ok(())
}
ktest!("fs.tree.enter", Safe, enter);

fn stays() -> TestResult {
    let mut t = T::with("fs-tree-stays", 0, false)?;
    let g = t.p.group().clone();
    let id = g.enter_tree(t.p.pid).map_err(|e| format!("enter: {e}"))?;
    kassert!(g.enter_tree(t.p.pid) == Err(EPERM), "a second entry");
    kassert!(g.tree_id() == id, "the tree changed");
    kassert!(g.set_chat("chat-17").is_ok() && g.chat().as_deref() == Some("chat-17"), "the chat label");
    kassert!(g.set_chat("not ok!") == Err(EINVAL), "a bad label was taken");
    kassert!(tree_text(t.p.pid)?.ends_with(" chat=chat-17\n"), "{:?}", tree_text(t.p.pid));
    let outside = T::new("fs-tree-stays-out")?;
    kassert!(outside.p.group().set_chat("c") == Err(EPERM), "a label outside a tree");
    // set*id: root gives up its ids, the tree stays
    kassert!(t.call(uapi::ab::nr_more::SETRESUID, &[1000, 1000, 1000]) == 0, "setresuid");
    kassert!(t.p.group().tree_id() == id && t.p.subject().tree == id && t.p.subject().uid == 1000, "set*id lost the tree");
    Ok(())
}
ktest!("fs.tree.stays", Safe, stays);

/// The file calls see the tree: a root process of it may not open the stick's CONFIG.TXT for writing (refused
/// before anything changes), reading stays.
fn guard() -> TestResult {
    let ns = vfs::root_ns();
    kassert!(until(60, || ns.lookup("/boot/BITSOS2/CONFIG.TXT").is_ok()), "no /boot/BITSOS2/CONFIG.TXT within 60 s (no boot stick)");
    let mut r = T::with("fs-tree-guard", 0, false)?;
    let id = r.p.group().enter_tree(r.p.pid).map_err(|e| format!("enter: {e}"))?;
    r.put(0, b"/boot/BITSOS2/CONFIG.TXT\0");
    let path = r.a(0);
    let before = ns.read_file("/boot/BITSOS2/CONFIG.TXT", 1 << 20).map_err(|e| format!("read: {e}"))?;
    for (what, flags) in [("O_WRONLY", 1u64), ("O_RDWR", 2), ("O_TRUNC", 0o1000), ("O_APPEND", 0o2001)] {
        let w = r.call(nr::OPENAT, &[(-100i64) as u64, path, flags, 0]);
        kassert!(w == -EPERM, "tree {id}: open {what} = {w}");
    }
    let ro = r.call(nr::OPENAT, &[(-100i64) as u64, path, 0, 0]);
    kassert!(ro >= 0, "tree {id}: open for reading = {ro}");
    r.call(nr::CLOSE, &[ro as u64]);
    kassert!(ns.read_file("/boot/BITSOS2/CONFIG.TXT", 1 << 20).ok().as_ref() == Some(&before), "CONFIG.TXT changed");
    Ok(())
}
ktest!("fs.tree.guard", Safe, guard);

/// clone3 with flags and SIGCHLD where allowed, its child's pid.
fn clone3(t: &mut T, flags: u64) -> i64 {
    let mut a = [0u8; 64];
    a[0..8].copy_from_slice(&flags.to_le_bytes());
    if flags & (CLONE_THREAD | CLONE_PARENT) == 0 {
        a[32..40].copy_from_slice(&SIGCHLD.to_le_bytes());
    }
    t.put(2048, &a);
    t.call(nr::CLONE3, &[t.a(2048), 64])
}

fn clone() -> TestResult {
    let outer = T::new("fs-tree-clone-outer")?;
    let mut t = T::child_of("fs-tree-clone", &outer.p)?;
    let id = t.p.group().enter_tree(t.p.pid).map_err(|e| format!("enter: {e}"))?;
    kassert!(outer.p.group().tree_id() == 0, "the parent came into the tree");
    let mut own = Owned(Vec::new());
    for (what, flags) in [("fork", 0), ("vfork", CLONE_VM | CLONE_VFORK), ("CLONE_PARENT", CLONE_PARENT), ("new user and mount namespace", CLONE_NEWUSER | CLONE_NEWNS)] {
        let pid = clone3(&mut t, flags);
        kassert!(pid > 0, "{what}: clone3 = {pid}");
        let c = proc::find(pid as u32).ok_or_else(|| format!("{what}: child {pid} not found"))?;
        own.0.push(c.clone());
        kassert!(c.group().tree_id() == id, "{what}: the child is in tree {} not {id}", c.group().tree_id());
        if flags & CLONE_PARENT != 0 {
            kassert!(c.ppid() == outer.p.pid, "CLONE_PARENT: parent {}", c.ppid());
        }
    }
    // unshare does not take the caller out either
    kassert!(t.call(nr::UNSHARE, &[CLONE_NEWUSER]) == 0, "unshare(CLONE_NEWUSER)");
    kassert!(t.p.group().tree_id() == id, "unshare lost the tree");
    drop(own);
    Ok(())
}
ktest!("fs.tree.clone", Safe, clone);

fn exec() -> TestResult {
    let code = hal::entry::ab4_progs::exec_arg();
    kassert!(!code.is_empty(), "no exec test program in this build");
    let ns = vfs::root_ns();
    let dir = "/tmp/fs-tree-exec";
    ns.mkdir_p(dir).map_err(|e| format!("mkdir: {e}"))?;
    let r = exec_cases(code, dir);
    let _ = ns.remove_tree(dir);
    r
}

fn exec_cases(code: &[u8], dir: &str) -> TestResult {
    let ns = vfs::root_ns();
    for (name, mode) in [("plain", 0o755), ("setuid", 0o4755)] {
        let target = format!("{dir}/{name}");
        ns.create_file(&target, mode, crate::ktests::elf(&SLEEPER)).map_err(|e| format!("create {name}: {e}"))?;
        let img = crate::ktests::elf(code);
        let argv = ["e", target.as_str()];
        let mut o = proc::SpawnOptions::new(&argv, &[], policy::ProgramId(1));
        o.held = true;
        let p = proc::spawn_image_with("fs-tree-exec", &img, &o).map_err(|e| format!("spawn: {e}"))?;
        let _own = Owned(alloc::vec![p.clone()]);
        let id = p.group().enter_tree(p.pid).map_err(|e| format!("enter: {e}"))?;
        p.release();
        kassert!(until(10, || proc::find(p.pid).is_some_and(|q| q.path == target) || p.has_exited()), "{name}: no exec ({:?})", p.status());
        let q = proc::find(p.pid).ok_or("gone")?;
        kassert!(q.group().tree_id() == id && q.subject().tree == id, "{name}: execve lost the tree");
        kassert!(tree_text(q.pid)?.starts_with(&format!("tree={id} ")), "{name}: {:?}", tree_text(q.pid));
    }
    Ok(())
}
ktest!("fs.tree.exec", Safe, exec);
