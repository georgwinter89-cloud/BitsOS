//! SX-2 ktests (`ab.sx2.*`): the namespaces Chromium's sandbox uses, through the real dispatcher on held test
//! processes (the test thread plays the program, as SX-1's and AB-12's tests do) and real clone children (a
//! sleeper loop in the test image). What needs a real program behind `current()` (the `/proc/<pid>/*_map`
//! files' opener, stat's id view, the socket gate) is covered by the `abi-ns-*` programs against CachyOS's
//! transcripts (`ab.sx2.programs`, `user/abi-tests/expected/sx2-*.txt`); the map rules themselves are tested
//! here on `proc::ns::user` with the credentials a real unshare made.
//!
//! All `Safe` and rerunnable: every test makes its own processes and ends them; every wait has a deadline;
//! counters are compared only when no other program uses a namespace (Brave may run on the Mini-PC).

use alloc::format;
use alloc::string::String;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use hal::entry::UserContext;
use ktest::{TestResult, kassert, ktest};
use proc::Process;
use proc::ns::user::{MapKind, Opened};
use proc::ns::{misc, pid, user};
use proc::signal::{SigInfo, To};
use uapi::ab::clone::*;
use uapi::errno::*;
use uapi::nr;

use crate::ns_sys::{NR_CHROOT, NR_UNSHARE};

const SEC: u64 = 1_000_000_000;
const ENTRY: u64 = 0x40_0078;
/// exit_group(0) at the entry; at ENTRY + 9 a loop of nanosleep({100, 0}).
const CODE: [u8; 27] = [
    0xb8, 0xe7, 0, 0, 0, 0x31, 0xff, 0x0f, 0x05, // mov eax, 231; xor edi, edi; syscall
    0x6a, 0x00, 0x6a, 0x64, 0x48, 0x89, 0xe7, 0x31, 0xf6, // push 0; push 100; mov rdi, rsp; xor esi, esi
    0xb8, 0x23, 0, 0, 0, 0x0f, 0x05, 0xeb, 0xee, // mov eax, 35; syscall; jmp back to the pushes
];
const SLEEP_AT: u64 = ENTRY + 9;
const SCRATCH: u64 = 1 << 16;
const SIGCHLD: u64 = 17;
const SIGKILL: u64 = 9;
const GETUID: usize = 102;
const UMASK: usize = 95;
const AT_FDCWD: u64 = (-100i64) as u64;
const THREAD: u64 = CLONE_VM | CLONE_FS | CLONE_FILES | CLONE_SIGHAND | CLONE_THREAD;

struct T {
    p: Arc<Process>,
    th: Arc<proc::Thread>,
    ctx: UserContext,
    mem: u64,
    _pin: proc::MmPin,
    /// Clone children (global ids), killed at the end if still there.
    kids: Vec<u32>,
}

impl T {
    fn new(name: &str) -> Result<T, String> {
        let image = crate::ktests::elf(&CODE);
        let argv = [name];
        let mut o = proc::SpawnOptions::new(&argv, &[], policy::ProgramId(1));
        o.held = true;
        let p = proc::spawn_image_with(name, &image, &o).map_err(|e| format!("spawn {name}: {e}"))?;
        let mem = p.mm.sys_mmap(0, SCRATCH, 3, 0x22, 0).map_err(|e| format!("mmap: {e:?}"))?;
        let th = p.threads().into_iter().next().ok_or("the held process has no main thread")?;
        let _pin = proc::pin_mm(&p);
        Ok(T { p, th, ctx: UserContext::new(ENTRY, mem + SCRATCH - 4096), mem, _pin, kids: Vec::new() })
    }
    fn call(&mut self, n: usize, args: &[u64]) -> i64 {
        let mut a = [0u64; 6];
        a[..args.len()].copy_from_slice(args);
        let r = &mut self.ctx.regs;
        (r.rax, r.rdi, r.rsi, r.rdx, r.r10, r.r8, r.r9) = (n as u64, a[0], a[1], a[2], a[3], a[4], a[5]);
        crate::dispatch(&self.p, &self.th, &mut self.ctx);
        self.ctx.regs.rax as i64
    }
    fn a(&self, at: u64) -> u64 {
        self.mem + at
    }
    fn put(&self, at: u64, b: &[u8]) {
        let _ = self.p.mm.copy_to_user(self.mem + at, b);
    }
    /// A NUL-terminated string at `at`; its user address.
    fn s(&self, at: u64, text: &str) -> u64 {
        let mut b = Vec::from(text.as_bytes());
        b.push(0);
        self.put(at, &b);
        self.a(at)
    }
    /// clone3 with `flags`; `sleep`: the child runs the sleeper loop instead of exit_group(0).
    fn clone3(&mut self, flags: u64, sleep: bool) -> i64 {
        let mut b = [0u8; 88];
        b[0..8].copy_from_slice(&flags.to_le_bytes());
        if flags & CLONE_THREAD == 0 {
            b[32..40].copy_from_slice(&SIGCHLD.to_le_bytes());
        }
        self.put(0x200, &b);
        if sleep {
            self.ctx.regs.rip = SLEEP_AT;
        }
        let ua = self.a(0x200);
        let r = self.call(nr::CLONE3, &[ua, 88]);
        self.ctx.regs.rip = ENTRY;
        if r > 0 {
            self.kids.push(r as u32);
        }
        r
    }
    fn unshare(&mut self, flags: u64) -> i64 {
        self.call(NR_UNSHARE, &[flags])
    }
    fn open(&mut self, path: &str) -> i64 {
        let ua = self.s(0x400, path);
        self.call(nr::OPENAT, &[AT_FDCWD, ua, 0, 0])
    }
    /// The call made as thread `th` of the same process (the test thread plays it, as it plays `self.th`).
    fn call_as(&mut self, th: &Arc<proc::Thread>, n: usize, args: &[u64]) -> i64 {
        let own = core::mem::replace(&mut self.th, th.clone());
        let r = self.call(n, args);
        self.th = own;
        r
    }
    /// readlink(path): the link text, or "-<errno>".
    fn readlink(&mut self, path: &str) -> String {
        let up = self.s(0x400, path);
        let buf = self.a(0x1000);
        let n = self.call(nr::READLINKAT, &[AT_FDCWD, up, buf, 512]);
        if n < 0 {
            return format!("{n}");
        }
        let mut b = alloc::vec![0u8; n as usize];
        let _ = self.p.mm.copy_from_user(&mut b, buf);
        String::from_utf8_lossy(&b).into_owned()
    }
}

impl Drop for T {
    fn drop(&mut self) {
        for &k in &self.kids {
            if let Some(c) = proc::find(k)
                && !c.has_exited()
            {
                c.kill(9);
            }
        }
        self.p.kill(9);
        let _ = self.p.wait(Some(10 * SEC));
    }
}

fn expect(what: &str, got: i64, want: i64) -> TestResult {
    kassert!(got == want, "{what}: got {got}, want {want}");
    Ok(())
}

fn errno_of(r: Result<usize, vfs::Errno>) -> i64 {
    r.map_or_else(|e| -e, |n| n as i64)
}

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

/// Gone or every thread ended: the image, its memory and its files are released (proc exit.rs `finish_group`).
/// `has_exited` alone is too early: the fd table still counts as shared and fdinfo still lists the fds (INT-W20,
/// BR-6b CR 3).
fn ended(pid: u32) -> bool {
    proc::find(pid).is_none_or(|p| p.group().is_finished())
}

/// Whether a process other than those in `except` uses a namespace besides the initial ones (then the global
/// counters may move while a test runs).
fn others_in_namespaces(except: &[u32]) -> bool {
    proc::processes().iter().any(|q| !except.contains(&q.pid) && !q.has_exited() && (!q.cred().user_ns.is_init() || !q.pid_ns.is_init() || !q.nsset().net.is_init()))
}

fn counters() -> (u64, u64, (u64, u64, u64)) {
    (user::live(), pid::live(), misc::live())
}

ktest!("ab.sx2.lifetime", Safe, lifetime);
/// A namespace lives as long as a process, a child or an open `/proc/<pid>/ns/*` file uses it, and goes with
/// the last of them (the live() counters come back).
fn lifetime() -> TestResult {
    let before = counters();
    let mut holder = T::new("sx2-life-holder")?;
    let (wu, wp, wn, wt): (Weak<user::UserNs>, Weak<pid::PidNs>, Weak<misc::NetNs>, Weak<misc::UtsNs>);
    let (tpid, kid, nsfd);
    {
        let mut t = T::new("sx2-life")?;
        tpid = t.p.pid;
        expect("unshare(user, pid, net, uts, ipc)", t.unshare(CLONE_NEWUSER | CLONE_NEWPID | CLONE_NEWNET | CLONE_NEWUTS | CLONE_NEWIPC), 0)?;
        let c = t.p.cred();
        kassert!(!c.user_ns.is_init() && c.user_ns.level == 1, "the credentials are in a new user namespace");
        wu = Arc::downgrade(&c.user_ns);
        drop(c);
        let set = t.p.nsset();
        kassert!(!set.net.is_init() && !set.uts.is_init(), "new net and uts namespaces");
        kassert!(t.p.pid_ns.is_init(), "the caller stays in its pid namespace");
        wp = Arc::downgrade(&set.pid_for_children);
        wn = Arc::downgrade(&set.net);
        wt = Arc::downgrade(&set.uts);
        drop(set);
        let now = counters();
        kassert!(now.0 > before.0 && now.1 > before.1 && now.2 .2 > before.2 .2, "counters did not go up: {before:?} -> {now:?}");
        // A child is the new pid namespace's init and keeps it.
        kid = t.clone3(0, true);
        kassert!(kid > 0, "clone3 = {kid}");
        let k = proc::find(kid as u32).ok_or("the child is not there")?;
        kassert!(wp.upgrade().is_some_and(|n| Arc::ptr_eq(&n, &k.pid_ns)), "the child is in the new pid namespace");
        kassert!(k.vpid() == 1 && k.is_ns_init(), "the child is its init (pid {})", k.vpid());
        drop(k);
        // Another process holds the net namespace through its ns file.
        nsfd = holder.open(&format!("/proc/{tpid}/ns/net"));
        kassert!(nsfd >= 0, "open /proc/{tpid}/ns/net = {nsfd}");
    }
    kassert!(until(10, || ended(tpid) && ended(kid as u32)), "the namespaced processes did not end");
    kassert!(until(10, || wp.upgrade().is_none() && wt.upgrade().is_none()), "outlived their last user: pid {} uts {} (strong counts)", wp.strong_count(), wt.strong_count());
    // The net namespace keeps its owner, the user namespace (`net->user_ns`), as on Linux.
    kassert!(wn.upgrade().is_some() && wu.upgrade().is_some(), "the open ns file must keep the net namespace and its owner");
    expect("close the ns file", holder.call(nr::CLOSE, &[nsfd as u64]), 0)?;
    kassert!(until(5, || wn.upgrade().is_none() && wu.upgrade().is_none()), "outlived the ns file: net {} user {}", wn.strong_count(), wu.strong_count());
    if !others_in_namespaces(&[holder.p.pid]) {
        kassert!(until(5, || counters() == before), "counters {:?}, before {before:?}", counters());
    } else {
        klog::event!(Info, "ktest", "sx2.lifetime", note = "other programs use namespaces: counters not compared");
    }
    Ok(())
}

ktest!("ab.sx2.ns_limit", Safe, ns_limit);
/// `max_*_namespaces` (linux-6.6/kernel/ucount.c inc_ucount): a user's namespaces of one kind stop at
/// `max_threads / 2` with ENOSPC, the charges come back when the namespaces go, and a real unshare charges
/// the caller's uid. The charges of the limit run go to a uid no program uses.
fn ns_limit() -> TestResult {
    use proc::ns::count::{self, Counted};
    let uid = 0x5a2_0101;
    let init = proc::ns::user::init_user_ns();
    let max = init.limits.get(Counted::Net);
    kassert!(max >= 10 && max < 1 << 24, "limit {max}");
    let mut held = Vec::new();
    held.try_reserve(max as usize).map_err(|_| "no memory for the charges")?;
    let refused = loop {
        match count::charge(Counted::Net, init, uid) {
            Ok(c) if (held.len() as u64) < max => held.push(c),
            Ok(_) => return Err(format!("a charge past the limit {max} was granted")),
            Err(e) => break e,
        }
    };
    kassert!(refused == ENOSPC && held.len() as u64 == max, "refused with {refused} after {} of {max}", held.len());
    kassert!(count::charge(Counted::Pid, init, uid).is_ok(), "another kind counts apart");
    drop(held);
    expect("charges back", count::used(Counted::Net, uid) as i64, 0)?;
    // A real unshare: one user and one net namespace charged to the caller's uid, given back when it ends.
    let mut t = T::new("sx2-limit")?;
    let me = t.p.cred().euid;
    let quiet = !others_in_namespaces(&[]);
    let before = (count::used(Counted::User, me), count::used(Counted::Net, me));
    expect("unshare(user, net)", t.unshare(CLONE_NEWUSER | CLONE_NEWNET), 0)?;
    let now = (count::used(Counted::User, me), count::used(Counted::Net, me));
    kassert!(now.0 > before.0 && now.1 > before.1, "not charged: {before:?} -> {now:?}");
    if quiet {
        kassert!(now == (before.0 + 1, before.1 + 1), "charged {before:?} -> {now:?}");
    }
    let pid = t.p.pid;
    drop(t);
    kassert!(until(10, || ended(pid)), "the process did not end");
    if quiet {
        kassert!(until(5, || (count::used(Counted::User, me), count::used(Counted::Net, me)) == before), "not given back: {:?}", (count::used(Counted::User, me), count::used(Counted::Net, me)));
    }
    Ok(())
}

ktest!("ab.sx2.maps", Safe, maps);
/// The id map rules for an unprivileged writer (linux-6.6/kernel/user_namespace.c map_write :920 and :966,
/// new_idmap_permitted :1160, the writer's namespace :1116, proc_setgroups_write :1213) with the credentials
/// a real unshare made.
fn maps() -> TestResult {
    let mut t = T::new("sx2-maps")?;
    let outside = t.p.cred();
    expect("unshare(CLONE_NEWUSER)", t.unshare(CLONE_NEWUSER), 0)?;
    let inside = t.p.cred();
    let ns = inside.user_ns.clone();
    kassert!(!Arc::ptr_eq(&ns, &outside.user_ns), "a new namespace");
    expect("getuid unmapped", t.call(GETUID, &[]), 65534)?;
    let o = Opened { ns: ns.clone(), opener: inside.clone() };
    let w = |k: MapKind, text: &str| errno_of(user::write_map(&o, k, 0, text.as_bytes(), &inside));
    let me = outside.euid;
    let line = format!("{me} {me} 1\n");
    let root_line = format!("0 {me} 1\n");
    expect("gid_map before setgroups deny", w(MapKind::Gid, &line), -EPERM)?;
    expect("setgroups deny", errno_of(user::write_setgroups(&o, 0, b"deny")), 4)?;
    expect("uid_map of another id", w(MapKind::Uid, "0 0 1\n"), -EPERM)?;
    expect("uid_map with count 2", w(MapKind::Uid, &format!("{me} {me} 2\n")), -EPERM)?;
    expect("uid_map bad syntax", w(MapKind::Uid, "x\n"), -EINVAL)?;
    expect("gid_map", w(MapKind::Gid, &line), line.len() as i64)?;
    expect("second gid_map", w(MapKind::Gid, &line), -EPERM)?;
    expect("setgroups allow after deny", errno_of(user::write_setgroups(&o, 0, b"allow")), -EPERM)?;
    expect("uid_map", w(MapKind::Uid, &root_line), root_line.len() as i64)?;
    expect("second uid_map", w(MapKind::Uid, &line), -EPERM)?;
    expect("getuid mapped", t.call(GETUID, &[]), 0)?;
    let shown = user::render_map(&ns, MapKind::Uid, &outside.user_ns);
    kassert!(shown == format!("{:>10} {:>10} {:>10}\n", 0, me, 1), "uid_map seen from outside: {shown:?}");
    // A writer outside the namespace and its parent may not write: a sibling namespace's process.
    let mut s = T::new("sx2-maps-sibling")?;
    expect("sibling unshare", s.unshare(CLONE_NEWUSER), 0)?;
    let mut u = T::new("sx2-maps-target")?;
    expect("target unshare", u.unshare(CLONE_NEWUSER), 0)?;
    let sc = s.p.cred();
    let ot = Opened { ns: u.p.cred().user_ns.clone(), opener: sc.clone() };
    expect("map written from a sibling namespace", errno_of(user::write_map(&ot, MapKind::Uid, 0, line.as_bytes(), &sc)), -EPERM)?;
    Ok(())
}

ktest!("ab.sx2.threaded", Safe, threaded);
/// unshare(CLONE_NEWUSER) implies CLONE_THREAD: EINVAL with a second thread (kernel/fork.c:3322); a thread
/// cannot be made into a new user or pid namespace (:2293); CLONE_NEWUSER|CLONE_FS is EINVAL (:2260).
fn threaded() -> TestResult {
    let mut t = T::new("sx2-threaded")?;
    expect("clone(CLONE_NEWUSER|CLONE_FS)", t.clone3(CLONE_NEWUSER | CLONE_FS, false), -EINVAL)?;
    expect("thread with CLONE_NEWUSER", t.clone3(THREAD | CLONE_NEWUSER, false), -EINVAL)?;
    expect("thread with CLONE_NEWPID", t.clone3(THREAD | CLONE_NEWPID, false), -EINVAL)?;
    let tid = t.clone3(THREAD, true);
    kassert!(tid > 0, "a second thread: {tid}");
    expect("unshare(CLONE_NEWUSER) threaded", t.unshare(CLONE_NEWUSER), -EINVAL)?;
    expect("unshare(CLONE_THREAD) threaded", t.unshare(CLONE_THREAD), -EINVAL)?;
    // FS-16: the calling thread alone gets its own fs state (`unshare_fs`), the other thread keeps the old one.
    let other = proc::find_thread(tid as u32).ok_or("second thread gone")?;
    expect("unshare(CLONE_FS) threaded", t.unshare(CLONE_FS), 0)?;
    kassert!(!Arc::ptr_eq(&t.th.fs(&t.p), &other.fs(&t.p)), "the caller's state is its own");
    kassert!(t.p.cred().user_ns.is_init(), "nothing changed");
    // An unknown flag; namespaces without CAP_SYS_ADMIN (EPERM before V2's own EINVAL for a mount namespace).
    expect("unshare(SIGCHLD)", t.unshare(SIGCHLD), -EINVAL)?;
    expect("unshare(CLONE_NEWNET) unprivileged", t.unshare(CLONE_NEWNET), -EPERM)?;
    expect("unshare(CLONE_NEWNS) unprivileged", t.unshare(CLONE_NEWNS), -EPERM)?;
    Ok(())
}

ktest!("ab.sx2.init_death", Safe, init_death);
/// When a pid namespace's init ends, every other member gets SIGKILL (kernel/pid_namespace.c:169), the
/// namespace takes nobody more (a fork there is ENOMEM, kernel/pid.c alloc_pid); a signal the init has no
/// handler for is dropped unless it is SIGKILL/SIGSTOP from an ancestor namespace (kernel/signal.c:91).
fn init_death() -> TestResult {
    let mut t = T::new("sx2-init")?;
    expect("unshare(user, pid)", t.unshare(CLONE_NEWUSER | CLONE_NEWPID), 0)?;
    let init = t.clone3(0, true);
    let second = t.clone3(0, true);
    kassert!(init > 0 && second > 0, "two children: {init} {second}");
    {
        let (i, s) = (proc::find(init as u32).ok_or("init")?, proc::find(second as u32).ok_or("second")?);
        kassert!(i.vpid() == 1 && s.vpid() == 2, "pids in the namespace: {} {}", i.vpid(), s.vpid());
        kassert!(i.is_ns_init() && !s.is_ns_init(), "the first is the init");
        kassert!(i.send_info(SigInfo::kernel(15), To::Group, false).is_ok(), "send SIGTERM");
    }
    sched::sleep_ns(50_000_000);
    kassert!(!ended(init as u32), "the init died of a SIGTERM it has no handler for");
    expect("kill(init, SIGKILL) from the parent namespace", t.call(nr::KILL, &[init as u64, SIGKILL]), 0)?;
    kassert!(until(10, || ended(init as u32)), "the init did not end");
    kassert!(until(10, || ended(second as u32)), "the second member outlived its init");
    expect("fork into the dead namespace", t.clone3(0, false), -ENOMEM)?;
    Ok(())
}

ktest!("ab.sx2.tsync", Safe, tsync);
/// SX-1 under SX-2: root of a user namespace may attach a filter without no_new_privs
/// (`ns_capable_noaudit(current_user_ns(), CAP_SYS_ADMIN)`, kernel/seccomp.c), TSYNC reaches a thread made
/// after the unshare, and a thread cloned after the sync inherits the filter.
fn tsync() -> TestResult {
    use seccomp::consts::*;
    let mut t = T::new("sx2-tsync")?;
    // RET ALLOW (one instruction): only the attach rules matter here.
    let mut ins = [0u8; 8];
    ins[0..2].copy_from_slice(&0x06u16.to_le_bytes());
    ins[4..8].copy_from_slice(&RET_ALLOW.to_le_bytes());
    t.put(0x1000, &ins);
    let mut fp = [0u8; 16];
    fp[0..2].copy_from_slice(&1u16.to_le_bytes());
    fp[8..16].copy_from_slice(&t.a(0x1000).to_le_bytes());
    t.put(0x100, &fp);
    let ufp = t.a(0x100);
    expect("filter without no_new_privs, uid 1000", t.call(NR_SECCOMP, &[SET_MODE_FILTER as u64, FLAG_TSYNC as u64, ufp]), -EACCES)?;
    expect("unshare(CLONE_NEWUSER)", t.unshare(CLONE_NEWUSER), 0)?;
    let before = t.clone3(THREAD, true);
    kassert!(before > 0, "thread {before}");
    expect("filter as root of the user namespace, TSYNC", t.call(NR_SECCOMP, &[SET_MODE_FILTER as u64, FLAG_TSYNC as u64, ufp]), 0)?;
    let after = t.clone3(THREAD, true);
    kassert!(after > 0, "thread {after}");
    for tid in [before, after] {
        let th = proc::find_thread(tid as u32).ok_or("thread gone")?;
        kassert!(th.prctl().seccomp().mode() == MODE_FILTER, "thread {tid} has no filter");
    }
    kassert!(!t.th.prctl().no_new_privs(), "no_new_privs stays off");
    Ok(())
}

ktest!("ab.sx2.fs_share", Safe, fs_share);
/// CLONE_FS without CLONE_FILES shares cwd, root and umask; a fork copies them; unshare(CLONE_FS) splits them.
fn fs_share() -> TestResult {
    let mut t = T::new("sx2-fs")?;
    let shared = t.clone3(CLONE_FS, true);
    let copied = t.clone3(0, true);
    kassert!(shared > 0 && copied > 0, "children {shared} {copied}");
    let (a, b) = (proc::find(shared as u32).ok_or("shared")?, proc::find(copied as u32).ok_or("copied")?);
    kassert!(!Arc::ptr_eq(&a.fds, &t.p.fds) && Arc::ptr_eq(&a.fs(), &t.p.fs()), "CLONE_FS: an own fd table, one fs state");
    kassert!(!Arc::ptr_eq(&b.fs(), &t.p.fs()), "fork: a copy");
    let ua = t.s(0x400, "/proc");
    expect("chdir(/proc)", t.call(nr::CHDIR, &[ua]), 0)?;
    expect("umask(077)", t.call(UMASK, &[0o077]), 0o022)?;
    kassert!(a.fs().umask() == 0o077 && b.fs().umask() == 0o022, "umask seen by the sharer only");
    kassert!(a.fs().cwd().is_some() && b.fs().cwd().is_none(), "cwd seen by the sharer only");
    expect("unshare(CLONE_FS)", t.unshare(CLONE_FS), 0)?;
    kassert!(!Arc::ptr_eq(&a.fs(), &t.p.fs()) && t.p.fs().umask() == 0o077, "split, the state kept");
    kassert!(a.fs().cwd().is_some() && t.p.fs().cwd().is_some(), "both keep the cwd /proc");
    expect("umask(022)", t.call(UMASK, &[0o022]), 0o077)?;
    kassert!(a.fs().umask() == 0o077, "no longer shared");
    Ok(())
}

ktest!("ab.sx2.fs_matrix", Safe, fs_matrix);
/// FS-16: CLONE_FS and CLONE_FILES are independent (linux-6.6/kernel/fork.c `copy_fs`, `copy_files`): the four
/// combinations for processes, chroot reaching exactly the CLONE_FS sharers (fs/open.c `set_fs_root`), a thread
/// without CLONE_FS with its own copy, and unshare(CLONE_FS) of one thread of several (`unshare_fs`).
fn fs_matrix() -> TestResult {
    let mut t = T::new("sx2-fsmatrix")?;
    // First: CAP_SYS_CHROOT for the chroot below (the implied CLONE_FS copies nothing, nobody shares yet).
    let before = t.p.fs();
    expect("unshare(CLONE_NEWUSER)", t.unshare(CLONE_NEWUSER), 0)?;
    kassert!(Arc::ptr_eq(&before, &t.p.fs()), "an unshared state is not copied (fs->users == 1)");
    drop(before);
    let mut kids = Vec::new();
    for f in [CLONE_FS, CLONE_FILES, CLONE_FS | CLONE_FILES, 0] {
        let k = t.clone3(f, true);
        kassert!(k > 0, "child with flags {f:#x}: {k}");
        kids.push((f, proc::find(k as u32).ok_or("child gone")?));
    }
    let ua = t.s(0x400, "/proc");
    expect("chdir(/proc)", t.call(nr::CHDIR, &[ua]), 0)?;
    expect("umask(077)", t.call(UMASK, &[0o077]), 0o022)?;
    let ur = t.s(0x400, "/sys");
    expect("chroot(/sys)", t.call(NR_CHROOT, &[ur]), 0)?;
    for (f, k) in &kids {
        let (fs, files) = (f & CLONE_FS != 0, f & CLONE_FILES != 0);
        kassert!(Arc::ptr_eq(&k.fds, &t.p.fds) == files, "flags {f:#x}: fd table shared = {files}");
        kassert!(Arc::ptr_eq(&k.fs(), &t.p.fs()) == fs, "flags {f:#x}: fs state shared = {fs}");
        let st = k.fs();
        kassert!((st.umask() == 0o077) == fs && st.cwd().is_some() == fs && st.root().is_some() == fs, "flags {f:#x}: umask {:o}, cwd {}, root {}", st.umask(), st.cwd().is_some(), st.root().is_some());
    }
    kassert!(t.p.chrooted(&t.th), "the caller is chrooted");
    // Threads: CLONE_FS shares the creator's state, a thread without it gets a copy (V2 refused it before FS-16).
    let shared = t.clone3(THREAD, true);
    let apart = t.clone3(THREAD & !CLONE_FS, true);
    kassert!(shared > 0 && apart > 0, "threads {shared} {apart}");
    let (ts, ta) = (proc::find_thread(shared as u32).ok_or("thread")?, proc::find_thread(apart as u32).ok_or("thread")?);
    kassert!(Arc::ptr_eq(&ts.fs(&t.p), &t.th.fs(&t.p)), "CLONE_FS thread shares");
    kassert!(!Arc::ptr_eq(&ta.fs(&t.p), &t.th.fs(&t.p)) && ta.fs(&t.p).umask() == 0o077, "thread without CLONE_FS: a copy");
    // One thread of several: only the caller leaves; the CLONE_FS thread and the CLONE_FS children keep the old.
    let old = t.th.fs(&t.p);
    expect("unshare(CLONE_FS) threaded", t.unshare(CLONE_FS), 0)?;
    kassert!(!Arc::ptr_eq(&old, &t.th.fs(&t.p)) && Arc::ptr_eq(&old, &ts.fs(&t.p)), "the caller alone has a new state");
    expect("umask(022)", t.call(UMASK, &[0o022]), 0o077)?;
    kassert!(old.umask() == 0o077 && ts.fs(&t.p).umask() == 0o077 && kids[0].1.fs().umask() == 0o077, "the others keep 077");
    kassert!(t.p.fs().umask() == 0o022, "/proc shows the leader's own state");
    // A process made now shares (CLONE_FS) or copies the caller's own state.
    let k = t.clone3(CLONE_FS, true);
    kassert!(k > 0, "child {k}");
    kassert!(Arc::ptr_eq(&proc::find(k as u32).ok_or("child")?.fs(), &t.th.fs(&t.p)), "CLONE_FS child of the split thread");
    drop(old);
    Ok(())
}

ktest!("ab.sx2.fs_threads", Safe, fs_threads);
/// FS-16 per thread: /proc/<pid>/task/<tid>/{cwd,root} show the viewed thread's state (linux-6.6/fs/proc/base.c
/// `proc_cwd_link`, `proc_root_link`: the task's fs), and chroot, chdir and umask of a thread made without
/// CLONE_FS reach that thread only (fs/open.c `set_fs_root` on `current->fs`).
fn fs_threads() -> TestResult {
    let mut t = T::new("sx2-fsthreads")?;
    // CAP_SYS_CHROOT for the thread's chroot below; one thread yet, so nothing is copied.
    expect("unshare(CLONE_NEWUSER)", t.unshare(CLONE_NEWUSER), 0)?;
    let apart = t.clone3(THREAD & !CLONE_FS, true);
    let shared = t.clone3(THREAD, true);
    kassert!(apart > 0 && shared > 0, "threads {apart} {shared}");
    let ta = proc::find_thread(apart as u32).ok_or("thread")?;
    let pid = t.p.pid;
    let ua = t.s(0x400, "/proc");
    expect("chdir(/proc)", t.call(nr::CHDIR, &[ua]), 0)?;
    for (tid, want) in [(pid as i64, "/proc"), (shared, "/proc"), (apart, "/")] {
        let got = t.readlink(&format!("/proc/{pid}/task/{tid}/cwd"));
        kassert!(got == want, "task {tid} cwd: got {got:?}, want {want:?}");
    }
    let got = t.readlink(&format!("/proc/{pid}/cwd"));
    kassert!(got == "/proc", "/proc/<pid>/cwd: got {got:?}");
    let ur = t.s(0x400, "/sys");
    expect("chroot(/sys) by the thread without CLONE_FS", t.call_as(&ta, NR_CHROOT, &[ur]), 0)?;
    let ur = t.s(0x400, "/");
    expect("chdir(/) by that thread", t.call_as(&ta, nr::CHDIR, &[ur]), 0)?;
    expect("umask(077) by that thread", t.call_as(&ta, UMASK, &[0o077]), 0o022)?;
    for (tid, root, cwd) in [(pid as i64, "/", "/proc"), (shared, "/", "/proc"), (apart, "/sys", "/sys")] {
        let (r, c) = (t.readlink(&format!("/proc/{pid}/task/{tid}/root")), t.readlink(&format!("/proc/{pid}/task/{tid}/cwd")));
        kassert!(r == root && c == cwd, "task {tid}: root {r:?} cwd {c:?}, want {root:?} {cwd:?}");
    }
    let got = t.readlink(&format!("/proc/{pid}/root"));
    kassert!(got == "/", "/proc/<pid>/root: got {got:?}");
    kassert!(!t.p.chrooted(&t.th) && t.p.chrooted(&ta), "only the thread is chrooted");
    expect("umask of the caller", t.call(UMASK, &[0o022]), 0o022)?;
    Ok(())
}

ktest!("ab.sx2.fs_setns", Safe, fs_setns);
/// FS-16: setns into a user namespace needs a file-system state nobody else uses (linux-6.6/kernel/user_namespace.c
/// `userns_install`: `current->fs->users != 1` is EINVAL); unshare(CLONE_FS) of a single-threaded sharer
/// (kernel/fork.c `unshare_fs`) gives it its own copy, the sibling keeps the old state, and setns then works.
fn fs_setns() -> TestResult {
    let mut t = T::new("sx2-fssetns")?;
    let target = t.clone3(CLONE_NEWUSER, true);
    kassert!(target > 0, "child in a new user namespace: {target}");
    let fd = t.open(&format!("/proc/{target}/ns/user"));
    kassert!(fd >= 0, "open ns/user: {fd}");
    let sib = t.clone3(CLONE_FS, true);
    kassert!(sib > 0, "CLONE_FS sibling {sib}");
    let s = proc::find(sib as u32).ok_or("sibling")?;
    let ua = t.s(0x400, "/proc");
    expect("chdir(/proc)", t.call(nr::CHDIR, &[ua]), 0)?;
    expect("setns(user) with a CLONE_FS sibling", t.call(nr::SETNS, &[fd as u64, CLONE_NEWUSER]), -EINVAL)?;
    expect("unshare(CLONE_FS)", t.unshare(CLONE_FS), 0)?;
    kassert!(!Arc::ptr_eq(&s.fs(), &t.p.fs()), "split");
    kassert!(s.fs().cwd().is_some() && t.p.fs().cwd().is_some(), "both keep the cwd /proc");
    expect("setns(user) alone", t.call(nr::SETNS, &[fd as u64, CLONE_NEWUSER]), 0)?;
    let _ = t.call(nr::CLOSE, &[fd as u64]);
    Ok(())
}

ktest!("ab.sx2.chroot", Safe, chroot);
/// chroot needs CAP_SYS_CHROOT in the caller's user namespace (fs/open.c:593); Chromium's trick: the root is
/// the fdinfo directory of a task that then ends, and every lookup below it is ENOENT, never a crash; a
/// chrooted process makes no user namespace (EPERM, `current_chrooted`).
fn chroot() -> TestResult {
    let mut t = T::new("sx2-chroot")?;
    let kid = t.clone3(0, true);
    kassert!(kid > 0, "child {kid}");
    let dir = format!("/proc/{kid}/fdinfo/");
    let ua = t.s(0x400, &dir);
    expect("chroot unprivileged", t.call(NR_CHROOT, &[ua]), -EPERM)?;
    expect("chroot(NULL)", t.call(NR_CHROOT, &[0]), -EFAULT)?;
    let ue = t.s(0x400, "");
    expect("chroot(\"\")", t.call(NR_CHROOT, &[ue]), -ENOENT)?;
    let uf = t.s(0x400, &format!("/proc/{}/status", t.p.pid));
    expect("chroot(a file)", t.call(NR_CHROOT, &[uf]), -ENOTDIR)?;
    expect("unshare(CLONE_NEWUSER)", t.unshare(CLONE_NEWUSER), 0)?;
    // FS-16: a process sharing only the fd table (CLONE_FILES without CLONE_FS) keeps its own root.
    let files = t.clone3(CLONE_FILES, true);
    kassert!(files > 0, "CLONE_FILES child {files}");
    let ua = t.s(0x400, &dir);
    expect("chroot(fdinfo of the child) with the fd table shared", t.call(NR_CHROOT, &[ua]), 0)?;
    kassert!(proc::find(files as u32).ok_or("CLONE_FILES child")?.fs().root().is_none(), "the CLONE_FILES child is not chrooted");
    expect("kill the CLONE_FILES child", t.call(nr::KILL, &[files as u64, SIGKILL]), 0)?;
    kassert!(until(10, || ended(files as u32)), "the CLONE_FILES child did not end");
    let ur = t.s(0x400, "/");
    expect("chdir(/)", t.call(nr::CHDIR, &[ur]), 0)?;
    kassert!(t.p.chrooted(&t.th), "chrooted");
    expect("unshare(CLONE_NEWUSER) chrooted", t.unshare(CLONE_NEWUSER), -EPERM)?;
    expect("kill the child", t.call(nr::KILL, &[kid as u64, SIGKILL]), 0)?;
    kassert!(until(10, || ended(kid as u32)), "the child did not end");
    for p in ["/proc/self/maps", "/proc/stat", "/sys/devices/system/cpu/online", "/0", "/../../proc", "/bin/sh"] {
        expect(&format!("open({p}) under a dead root"), t.open(p), -ENOENT)?;
    }
    let up = t.s(0x400, "/proc");
    let st = t.a(0x800);
    expect("stat(/proc) under a dead root", t.call(nr::NEWFSTATAT, &[AT_FDCWD, up, st, 0]), -ENOENT)?;
    Ok(())
}

fn have(path: &str) -> bool {
    vfs::root_ns().lookup(path).is_ok()
}

fn diff(got: &str, want: &str) -> Vec<String> {
    let g: Vec<&str> = got.lines().collect();
    let mut d = Vec::new();
    for (i, w) in want.lines().enumerate() {
        match g.get(i) {
            Some(l) if *l == w => {}
            Some(l) => d.push(format!("line {}: got {l:?}, want {w:?}", i + 1)),
            None => d.push(format!("line {}: missing, want {w:?}", i + 1)),
        }
    }
    if g.len() > want.lines().count() {
        d.push(format!("{} extra lines", g.len() - want.lines().count()));
    }
    d
}

ktest!("ab.sx2.programs", Safe, programs);
/// The `abi-ns-*` programs against CachyOS's transcripts (the same cases `cargo xtask abi-diff` runs).
fn programs() -> TestResult {
    let cases: [(&str, &str, &str); 16] = [
        ("/bin/abi-ns-unshare", "", include_str!("../../../user/abi-tests/expected/sx2-unshare.txt")),
        // FS-16: one thread of several unshares its cwd/umask.
        ("/bin/abi-ns-unshare", "threadfs", include_str!("../../../user/abi-tests/expected/sx2-unshare-threadfs.txt")),
        // FS-16 (fixer): execve from a thread with its own state starts the image with that state.
        ("/bin/abi-ns-unshare", "threadexec", include_str!("../../../user/abi-tests/expected/sx2-unshare-threadexec.txt")),
        ("/bin/abi-ns-clone", "", include_str!("../../../user/abi-tests/expected/sx2-clone.txt")),
        ("/bin/abi-ns-idmap", "", include_str!("../../../user/abi-tests/expected/sx2-idmap.txt")),
        // FS-16: pwrite on seq_file files (maps, comm, uid_map) is ESPIPE, oom_score_adj keeps it.
        ("/bin/abi-ns-idmap", "pwrite", include_str!("../../../user/abi-tests/expected/sx2-idmap-pwrite.txt")),
        ("/bin/abi-ns-setns", "", include_str!("../../../user/abi-tests/expected/sx2-setns.txt")),
        // FS-16 (fixer): setns(user) is EINVAL while a CLONE_FS sibling shares the state, 0 after unshare(CLONE_FS).
        ("/bin/abi-ns-setns", "fsshared", include_str!("../../../user/abi-tests/expected/sx2-setns-fsshared.txt")),
        ("/bin/abi-ns-chroot", "", include_str!("../../../user/abi-tests/expected/sx2-chroot.txt")),
        // FS-16: chroot reaches a CLONE_FILES sibling only when it shares CLONE_FS too.
        ("/bin/abi-ns-chroot", "files", include_str!("../../../user/abi-tests/expected/sx2-chroot-files.txt")),
        // Linux 6.6's answers (CachyOS 7.3 says ESRCH under the dead root: expected/sx2.known).
        ("/bin/abi-ns-chroot", "trick", include_str!("../../../user/abi-tests/expected/sx2-chroot-trick.txt")),
        ("/bin/abi-ns-pid", "", include_str!("../../../user/abi-tests/expected/sx2-pid.txt")),
        ("/bin/abi-ns-net", "", include_str!("../../../user/abi-tests/expected/sx2-net.txt")),
        // INT-W20: NW-11's views (abstract names, any IPv4 bind) and its SCM_CREDENTIALS/SO_PEERCRED translation.
        ("/bin/abi-ns-net", "nw", include_str!("../../../user/abi-tests/expected/sx2-net-nw.txt")),
        ("/bin/abi-ns-cred", "", include_str!("../../../user/abi-tests/expected/sx2-cred.txt")),
        // NW-12: addresses, MTU and /proc/net of a namespace's lo.
        ("/bin/abi-ns-lo", "", include_str!("../../../user/abi-tests/expected/sx2-lo.txt")),
    ];
    if !have("/bin/abi-ns-unshare") {
        if have("/bin/abi-ids") {
            return Err(String::from("the image has ABI programs but not /bin/abi-ns-unshare (did it fail to build?)"));
        }
        return ktest::skip("the user/abi-tests programs are not in this image (no musl sysroot when it was built)");
    }
    for (path, arg, want) in cases {
        let out = proc::stdio::Capture::new(1 << 20);
        let argv: Vec<&str> = if arg.is_empty() { Vec::from([path]) } else { Vec::from([path, arg]) };
        let env = ["HOME=/", "PATH=/bin", "ABI_TEST=1"];
        let mut o = proc::SpawnOptions::new(&argv, &env, policy::ProgramId(1));
        o.stdin = Some(proc::stdio::Input::new(Vec::new()) as Arc<dyn vfs::File>);
        o.stdout = Some(out.clone() as Arc<dyn vfs::File>);
        o.stderr = Some(out.clone() as Arc<dyn vfs::File>);
        let p = proc::spawn_with(path, &o).map_err(|e| format!("spawn {path}: {e}"))?;
        let code = match p.wait(Some(120 * SEC)) {
            Some(c) => c,
            None => {
                p.kill(9);
                let _ = p.wait(Some(10 * SEC));
                return Err(format!("{path} did not end within 120 s:\n{}", String::from_utf8_lossy(&out.contents())));
            }
        };
        let text = String::from_utf8_lossy(&out.contents()).into_owned();
        let d = diff(&text, want);
        kassert!(code == 0 && d.is_empty(), "{path} {arg} differs from the expected transcript (exit {code}):\n{}", d.join("\n"));
        klog::event!(Info, "ktest", "sx2.program", path = path, mode = arg, lines = text.lines().count());
    }
    Ok(())
}
