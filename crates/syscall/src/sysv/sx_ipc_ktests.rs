//! SX-IPC ktests (`sx.ipc.*`): the calls through the real dispatcher on held test processes (the test thread plays the
//! program, as `sx2_ktests`), sleepers on helper threads, and the `abi-sysv-*` programs against CachyOS's transcripts.
//! All `Safe` and rerunnable: each test makes its own objects with IPC_PRIVATE or in its own namespace and removes
//! them; every wait has a deadline; a machine-wide number is compared only in a namespace of its own.

use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicI64, AtomicU32, Ordering};
use hal::entry::UserContext;
use ktest::{TestResult, kassert, ktest};
use proc::Process;
use uapi::ab::sysv::*;
use uapi::errno::*;

const SEC: u64 = 1_000_000_000;
const NR_UNSHARE: usize = 272;
const ENTRY: u64 = 0x40_0078;
/// exit_group(0) (never runs: the process stays held).
const CODE: [u8; 9] = [0xb8, 0xe7, 0, 0, 0, 0x31, 0xff, 0x0f, 0x05];
const SCRATCH: u64 = 1 << 16;
const NEWIPC: u64 = uapi::ab::clone::CLONE_NEWIPC;

static RUN: AtomicU32 = AtomicU32::new(0);

/// A held process whose calls the test thread makes.
struct T {
    p: Arc<Process>,
    th: Arc<proc::Thread>,
    ctx: UserContext,
    mem: u64,
    _pin: proc::MmPin,
}

impl T {
    fn new(name: &str, uid: u32) -> Result<T, String> {
        let image = crate::ktests::elf(&CODE);
        let argv = [name];
        let mut o = proc::SpawnOptions::new(&argv, &[], policy::ProgramId(1));
        o.held = true;
        o.uid = uid;
        let p = proc::spawn_image_with(name, &image, &o).map_err(|e| format!("spawn {name}: {e}"))?;
        let mem = p.mm.sys_mmap(0, SCRATCH, 3, 0x22, 0).map_err(|e| format!("mmap: {e:?}"))?;
        p.mm.copy_to_user(mem, &[0u8; SCRATCH as usize]).map_err(|e| format!("fill: {e:?}"))?;
        let th = p.threads().into_iter().next().ok_or("the held process has no main thread")?;
        let pin = proc::pin_mm(&p);
        Ok(T { ctx: UserContext::new(ENTRY, mem + SCRATCH - 4096), p, th, mem, _pin: pin })
    }
    fn call(&mut self, n: usize, args: &[u64]) -> i64 {
        let mut a = [0u64; 6];
        a[..args.len()].copy_from_slice(args);
        let r = &mut self.ctx.regs;
        (r.rax, r.rdi, r.rsi, r.rdx, r.r10, r.r8, r.r9) = (n as u64, a[0], a[1], a[2], a[3], a[4], a[5]);
        crate::dispatch(&self.p, &self.th, &mut self.ctx);
        self.ctx.regs.rax as i64
    }
    fn put(&self, at: u64, b: &[u8]) {
        let _ = self.p.mm.copy_to_user(self.mem + at, b);
    }
    fn get(&self, at: u64, b: &mut [u8]) {
        let _ = self.p.mm.copy_from_user(b, self.mem + at);
    }
    fn semget(&mut self, key: i32, n: u64, fl: i32) -> i64 {
        self.call(NR_SEMGET, &[key as u64, n, fl as u64])
    }
    fn semctl(&mut self, id: i64, num: u64, cmd: i32, arg: u64) -> i64 {
        self.call(NR_SEMCTL, &[id as u64, num, cmd as u64, arg])
    }
    /// One operation.
    fn semop(&mut self, id: i64, num: u16, op: i16, flg: i16) -> i64 {
        let mut b = [0u8; 6];
        b[..2].copy_from_slice(&num.to_le_bytes());
        b[2..4].copy_from_slice(&op.to_le_bytes());
        b[4..].copy_from_slice(&flg.to_le_bytes());
        self.put(0x100, &b);
        let a = self.mem + 0x100;
        self.call(NR_SEMOP, &[id as u64, a, 1])
    }
    fn nattch(&mut self, id: i64) -> i64 {
        let a = self.mem + 0x800;
        let r = self.call(NR_SHMCTL, &[id as u64, IPC_STAT as u64, a]);
        if r != 0 {
            return r;
        }
        let mut b = [0u8; 8];
        self.get(0x800 + 88, &mut b);
        u64::from_le_bytes(b) as i64
    }
    fn end(self) {
        self.p.kill(9);
        self.p.release();
        let _ = self.p.wait(Some(10 * SEC));
    }
}

/// A held process's call on a helper thread, which ends the process after it and keeps the answer.
struct Sleeper {
    h: sched::JoinHandle,
    r: Arc<AtomicI64>,
}

const PENDING: i64 = i64::MIN;

fn sleeper(name: &'static str, mut t: T, n: usize, args: [u64; 6]) -> Result<Sleeper, String> {
    let r = Arc::new(AtomicI64::new(PENDING));
    let r2 = r.clone();
    let h = sched::spawn(name, sched::Class::Normal, move || {
        let v = t.call(n, &args);
        r2.store(v, Ordering::Release);
        t.end();
    })
    .map_err(|e| format!("spawn helper: {e:?}"))?;
    Ok(Sleeper { h, r })
}

impl Sleeper {
    fn result(&self, ns: u64) -> Option<i64> {
        if !self.h.join_timeout(ns) {
            return None;
        }
        Some(self.r.load(Ordering::Acquire))
    }
    fn still(&self) -> bool {
        self.r.load(Ordering::Acquire) == PENDING
    }
}

/// Waits until `f` holds (a sleeper has queued).
fn until(mut f: impl FnMut() -> bool) -> bool {
    let end = time::now_ns() + 5 * SEC;
    while time::now_ns() < end {
        if f() {
            return true;
        }
        sched::sleep_ms(2);
    }
    false
}

/// A process of uid 1000 in the initial namespace (these tests use IPC_PRIVATE objects there).
fn other(tag: &str) -> Result<T, String> {
    T::new(&format!("sxipc-{tag}-{}", RUN.fetch_add(1, Ordering::Relaxed)), 1000)
}

fn names() -> TestResult {
    let want = [
        (NR_SHMGET, "shmget"),
        (NR_SHMAT, "shmat"),
        (NR_SHMCTL, "shmctl"),
        (NR_SEMGET, "semget"),
        (NR_SEMOP, "semop"),
        (NR_SEMCTL, "semctl"),
        (NR_SHMDT, "shmdt"),
        (NR_MSGGET, "msgget"),
        (NR_MSGSND, "msgsnd"),
        (NR_MSGRCV, "msgrcv"),
        (NR_MSGCTL, "msgctl"),
        (NR_SEMTIMEDOP, "semtimedop"),
    ];
    for (nr, name) in want {
        kassert!(uapi::nr::name(nr) == Some(name), "number {nr} is not named {name}");
    }
    let mut t = other("names")?;
    let id = t.semget(IPC_PRIVATE, 1, 0o600);
    kassert!(id >= 0, "semget {id}");
    let text = crate::stats_text();
    let line = text.lines().find(|l| l.contains("semget")).unwrap_or("");
    kassert!(!line.is_empty(), "/proc/bitsos/syscalls has no semget line");
    kassert!(t.semctl(id, 0, IPC_RMID, 0) == 0);
    t.end();
    Ok(())
}
ktest!("sx.ipc.names", Safe, names);

/// Ids: index and sequence as Linux hands them out, stale ids, keys.
fn ids() -> TestResult {
    let mut t = other("ids")?;
    kassert!(t.call(NR_UNSHARE, &[uapi::ab::clone::CLONE_NEWUSER | NEWIPC]) == 0, "unshare");
    let a = t.semget(IPC_PRIVATE, 1, 0o600);
    let b = t.semget(IPC_PRIVATE, 1, 0o600);
    kassert!(a == 0 && b == 1, "first ids {a} {b}");
    kassert!(t.semctl(a, 0, IPC_RMID, 0) == 0);
    let c = t.semget(IPC_PRIVATE, 1, 0o600);
    kassert!(c == 2, "the index moves on: {c}");
    kassert!(t.semctl(a, 0, GETVAL, 0) == -EINVAL);
    kassert!(t.semctl(b + (1 << 15), 0, GETVAL, 0) == -EINVAL, "another sequence number");
    let k = t.semget(0x1234, 2, IPC_CREAT | 0o600);
    kassert!(t.semget(0x1234, 3, 0) == -EINVAL);
    kassert!(t.semget(0x1234, 2, IPC_CREAT | IPC_EXCL) == -EEXIST);
    kassert!(t.semget(0x1234, 0, 0) == k);
    kassert!(t.semctl(k, 0, IPC_RMID, 0) == 0);
    kassert!(t.semget(0x1234, 2, 0) == -ENOENT);
    t.end();
    Ok(())
}
ktest!("sx.ipc.ids", Safe, ids);

/// Two users: mode bits, group, owner's rights, CAP_IPC_OWNER and CAP_SYS_ADMIN of root (the ABI programs can only be
/// one user).
fn perms() -> TestResult {
    let mut owner = T::new("sxipc-owner", 1000)?;
    let mut stranger = T::new("sxipc-stranger", 1001)?;
    let mut root = T::new("sxipc-root", 0)?;
    let id = owner.semget(IPC_PRIVATE, 1, 0o640);
    kassert!(id >= 0);
    kassert!(stranger.semctl(id, 0, GETVAL, 0) == -EACCES, "other: no rights");
    kassert!(stranger.semctl(id, 0, SETVAL, 1) == -EACCES);
    kassert!(stranger.semop(id, 0, 1, 0) == -EACCES);
    kassert!(stranger.semctl(id, 0, IPC_RMID, 0) == -EPERM, "only the owner removes");
    let buf = stranger.mem + 0x800;
    kassert!(stranger.semctl(id, 0, IPC_SET, buf) == -EPERM);
    let rbuf = root.mem + 0x800;
    kassert!(root.semctl(id, 0, GETVAL, 0) == 0, "CAP_IPC_OWNER reads");
    kassert!(root.semctl(id, 0, SETVAL, 3) == 0, "CAP_IPC_OWNER writes");
    // IPC_STAT, then IPC_SET: the stranger's uid and group become the object's; the creator keeps its rights.
    kassert!(root.semctl(id, 0, IPC_STAT, rbuf) == 0);
    let mut stat = [0u8; SEMID64_SIZE];
    root.get(0x800, &mut stat);
    stat[4..8].copy_from_slice(&1001u32.to_le_bytes());
    stat[20..24].copy_from_slice(&0o604u32.to_le_bytes());
    root.put(0x800, &stat);
    kassert!(root.semctl(id, 0, IPC_SET, rbuf) == 0, "CAP_SYS_ADMIN sets");
    kassert!(stranger.semctl(id, 0, GETVAL, 0) == 3, "uid 1001 is the owner now, mode 6");
    kassert!(owner.semctl(id, 0, SETVAL, 4) == 0, "the creator (cuid) still counts as owner");
    let mut third = T::new("sxipc-third", 1002)?;
    kassert!(third.semctl(id, 0, GETVAL, 0) == 4, "others: read, mode 4");
    kassert!(third.semctl(id, 0, SETVAL, 1) == -EACCES);
    kassert!(third.semget(IPC_PRIVATE, 1, 0o600) >= 0);
    kassert!(stranger.semctl(id, 0, IPC_RMID, 0) == 0, "the new owner removes");
    for t in [owner, stranger, root, third] {
        t.end();
    }
    Ok(())
}
ktest!("sx.ipc.perms", Safe, perms);

/// Wake-ups across processes: a sleeper is queued (GETNCNT) and finishes on +1; EIDRM on removal; a kill ends a wait.
fn sem_wait() -> TestResult {
    let mut t = other("wait")?;
    let id = t.semget(IPC_PRIVATE, 2, 0o666);
    kassert!(id >= 0);
    let w = other("waiter")?;
    let sops = w.mem + 0x100;
    w.put(0x100, &[0, 0, 0xff, 0xff, 0, 0]);
    let s = sleeper("sxipc-sleep", w, NR_SEMOP, [id as u64, sops, 1, 0, 0, 0])?;
    kassert!(until(|| t.semctl(id, 0, GETNCNT, 0) == 1), "no waiter queued");
    kassert!(s.still(), "finished too early");
    kassert!(t.semop(id, 0, 1, 0) == 0);
    kassert!(s.result(5 * SEC) == Some(0), "the sleeper did not get it");
    kassert!(t.semctl(id, 0, GETVAL, 0) == 0);

    let w = other("removed")?;
    let sops = w.mem + 0x100;
    w.put(0x100, &[0, 0, 0xff, 0xff, 0, 0]);
    let s = sleeper("sxipc-rmid", w, NR_SEMOP, [id as u64, sops, 1, 0, 0, 0])?;
    kassert!(until(|| t.semctl(id, 0, GETNCNT, 0) == 1));
    kassert!(t.semctl(id, 0, IPC_RMID, 0) == 0);
    kassert!(s.result(5 * SEC) == Some(-EIDRM), "EIDRM expected");

    // The process is killed while its call sleeps: EINTR, off the queue.
    let id = t.semget(IPC_PRIVATE, 1, 0o666);
    let w = other("killed")?;
    let sops = w.mem + 0x100;
    w.put(0x100, &[0, 0, 0xff, 0xff, 0, 0]);
    let victim = w.p.clone();
    let s = sleeper("sxipc-kill", w, NR_SEMOP, [id as u64, sops, 1, 0, 0, 0])?;
    kassert!(until(|| t.semctl(id, 0, GETNCNT, 0) == 1));
    victim.kill(9);
    kassert!(s.result(5 * SEC) == Some(-EINTR), "EINTR expected");
    kassert!(t.semctl(id, 0, GETNCNT, 0) == 0, "still queued");
    kassert!(t.semctl(id, 0, IPC_RMID, 0) == 0);
    t.end();
    Ok(())
}
ktest!("sx.ipc.sem_wait", Safe, sem_wait);

/// Many threads with two-semaphore lists: the sum of both never leaves its bound, nothing is lost.
fn sem_atomic() -> TestResult {
    const ROUNDS: u16 = 300;
    let mut t = other("atomic")?;
    let id = t.semget(IPC_PRIVATE, 2, 0o666);
    kassert!(t.semctl(id, 0, SETVAL, 1) == 0, "one token");
    let mut hs = Vec::new();
    for n in 0..4 {
        let w = other(if n % 2 == 0 { "even" } else { "odd" })?;
        // Moves the token from one semaphore to the other, both changes in one list.
        let (from, to): (u16, u16) = if n % 2 == 0 { (0, 1) } else { (1, 0) };
        let mut l = [0u8; 12];
        l[0..2].copy_from_slice(&from.to_le_bytes());
        l[2..4].copy_from_slice(&(-1i16).to_le_bytes());
        l[6..8].copy_from_slice(&to.to_le_bytes());
        l[8..10].copy_from_slice(&1i16.to_le_bytes());
        w.put(0x100, &l);
        let done = Arc::new(AtomicI64::new(0));
        let d = done.clone();
        let mut w = w;
        let h = sched::spawn("sxipc-atomic", sched::Class::Normal, move || {
            for _ in 0..ROUNDS {
                let a = w.mem + 0x100;
                let r = w.call(NR_SEMOP, &[id as u64, a, 2]);
                if r != 0 {
                    d.store(r, Ordering::Release);
                    break;
                }
            }
            w.end();
        })
        .map_err(|e| format!("spawn: {e:?}"))?;
        hs.push((h, done));
    }
    let mut bad = 0;
    let check = |t: &mut T, bad: &mut i32| {
        // GETALL: both values of one moment.
        let out = t.mem + 0x400;
        let _ = t.semctl(id, 0, GETALL, out);
        let mut v = [0u8; 4];
        t.get(0x400, &mut v);
        let sum = u16::from_le_bytes([v[0], v[1]]) as i32 + u16::from_le_bytes([v[2], v[3]]) as i32;
        if sum != 1 {
            *bad += 1;
        }
        sum
    };
    let end = time::now_ns() + 60 * SEC;
    while hs.iter().any(|(h, _)| !h.is_finished()) && time::now_ns() < end {
        check(&mut t, &mut bad);
    }
    kassert!(bad == 0, "the token was seen doubled or lost {bad} times");
    for (h, d) in &hs {
        kassert!(h.join_timeout(5 * SEC), "a worker did not finish");
        kassert!(d.load(Ordering::Acquire) == 0, "worker: {}", d.load(Ordering::Acquire));
    }
    kassert!(check(&mut t, &mut bad) == 1);
    kassert!(t.semctl(id, 0, IPC_RMID, 0) == 0);
    t.end();
    Ok(())
}
ktest!("sx.ipc.sem_atomic", Safe, sem_atomic);

/// SEM_UNDO through a real process end.
fn sem_undo() -> TestResult {
    let mut t = other("undo")?;
    let id = t.semget(IPC_PRIVATE, 1, 0o666);
    let mut w = other("undoer")?;
    kassert!(w.semop(id, 0, 5, SEM_UNDO) == 0);
    kassert!(t.semctl(id, 0, GETVAL, 0) == 5);
    let pid = w.p.pid;
    w.end();
    kassert!(t.semctl(id, 0, GETVAL, 0) == 0, "the end of the process undoes");
    kassert!(t.semctl(id, 0, GETPID, 0) == pid as i64, "sempid is the leaver");
    // A list on a removed array: nothing to do at the end, no trouble.
    let mut w = other("undoer2")?;
    kassert!(w.semop(id, 0, 1, SEM_UNDO) == 0);
    kassert!(t.semctl(id, 0, IPC_RMID, 0) == 0);
    w.end();
    t.end();
    Ok(())
}
ktest!("sx.ipc.sem_undo", Safe, sem_undo);

/// Attachments and the file's life: RMID with an attachment, attach after it, memory reachable until the last detach.
fn shm_life() -> TestResult {
    let mut t = other("shm")?;
    let id = t.call(NR_SHMGET, &[IPC_PRIVATE as u64, 3 * 4096, 0o600]);
    kassert!(id >= 0, "shmget {id}");
    kassert!(t.nattch(id) == 0);
    let a = t.call(NR_SHMAT, &[id as u64, 0, 0]);
    kassert!(a > 0, "shmat {a}");
    kassert!(t.nattch(id) == 1);
    let mut zero = [1u8; 8];
    kassert!(t.p.mm.copy_from_user(&mut zero, a as u64 + 8192).is_ok());
    kassert!(zero == [0; 8], "zero-filled");
    kassert!(t.p.mm.copy_to_user(a as u64, b"kept").is_ok());
    kassert!(t.call(NR_SHMCTL, &[id as u64, IPC_RMID as u64, 0]) == 0);
    kassert!(t.nattch(id) == 1, "marked, not gone");
    let b = t.call(NR_SHMAT, &[id as u64, 0, 0]);
    kassert!(b > 0, "attach after RMID");
    let mut got = [0u8; 4];
    kassert!(t.p.mm.copy_from_user(&mut got, b as u64).is_ok());
    kassert!(&got == b"kept", "the same pages");
    kassert!(t.call(uapi::nr::MUNMAP, &[b as u64 + 4096, 4096]) == 0);
    kassert!(t.nattch(id) == 3, "a hole makes two VMAs");
    kassert!(t.call(NR_SHMDT, &[b as u64]) == 0);
    kassert!(t.nattch(id) == 1, "detach takes both parts");
    kassert!(t.call(NR_SHMDT, &[a as u64]) == 0);
    kassert!(t.nattch(id) == -EINVAL, "gone at the last detach");
    // Bounded by the namespace's limits, not by a table here: a thousand segments come and go.
    let mut ids = Vec::new();
    for _ in 0..1000 {
        let s = t.call(NR_SHMGET, &[IPC_PRIVATE as u64, 4096, 0o600]);
        kassert!(s >= 0, "segment {}: {s}", ids.len());
        ids.push(s);
    }
    for s in ids {
        kassert!(t.call(NR_SHMCTL, &[s as u64, IPC_RMID as u64, 0]) == 0);
    }
    t.end();
    Ok(())
}
ktest!("sx.ipc.shm_life", Safe, shm_life);

/// SHM_HUGETLB without a pool, as root (the ABI programs only see EPERM): the size check, then ENOMEM.
fn shm_huge() -> TestResult {
    let mut root = T::new("sxipc-huge", 0)?;
    let flags = (SHM_HUGETLB | 0o600) as u64;
    kassert!(root.call(NR_SHMGET, &[IPC_PRIVATE as u64, 4096, flags | (20 << 26)]) == -EINVAL);
    kassert!(root.call(NR_SHMGET, &[IPC_PRIVATE as u64, 4096, flags]) == -ENOMEM);
    kassert!(root.call(NR_SHMGET, &[IPC_PRIVATE as u64, 4096, flags | (30 << 26)]) == -ENOMEM);
    root.end();
    let mut user = other("huge")?;
    kassert!(user.call(NR_SHMGET, &[IPC_PRIVATE as u64, 4096, flags]) == -EPERM);
    user.end();
    Ok(())
}
ktest!("sx.ipc.shm_huge", Safe, shm_huge);

/// The end of a process detaches; a segment already marked goes then.
fn shm_exit() -> TestResult {
    let mut t = other("shmexit")?;
    let id = t.call(NR_SHMGET, &[IPC_PRIVATE as u64, 4096, 0o666]);
    let mut w = other("attacher")?;
    let a = w.call(NR_SHMAT, &[id as u64, 0, 0]);
    kassert!(a > 0, "shmat {a}");
    kassert!(t.nattch(id) == 1);
    kassert!(t.call(NR_SHMCTL, &[id as u64, IPC_RMID as u64, 0]) == 0);
    w.end();
    kassert!(until(|| t.nattch(id) == -EINVAL), "the process's end did not destroy it: {}", t.nattch(id));
    t.end();
    Ok(())
}
ktest!("sx.ipc.shm_exit", Safe, shm_exit);

/// Blocking queues: a sleeping receiver takes a message directly, a full queue blocks a sender, removal wakes both.
fn msg_wait() -> TestResult {
    let mut t = other("msg")?;
    let id = t.call(NR_MSGGET, &[IPC_PRIVATE as u64, 0o666]);
    kassert!(id >= 0);
    let r = other("receiver")?;
    let buf = r.mem + 0x100;
    let s = sleeper("sxipc-rcv", r, NR_MSGRCV, [id as u64, buf, 64, 0, 0, 0])?;
    sched::sleep_ms(20);
    kassert!(s.still());
    let mut m = [0u8; 16];
    m[0] = 3;
    m[8..12].copy_from_slice(b"text");
    t.put(0x100, &m);
    let a = t.mem + 0x100;
    kassert!(t.call(NR_MSGSND, &[id as u64, a, 4, 0]) == 0);
    kassert!(s.result(5 * SEC) == Some(4), "the receiver did not get it");

    // Queue limit 4: the second message waits.
    let qs = t.mem + 0x800;
    kassert!(t.call(NR_MSGCTL, &[id as u64, IPC_STAT as u64, qs]) == 0);
    t.put(0x800 + 88, &4u64.to_le_bytes());
    kassert!(t.call(NR_MSGCTL, &[id as u64, IPC_SET as u64, qs]) == 0);
    kassert!(t.call(NR_MSGSND, &[id as u64, a, 4, 0]) == 0);
    kassert!(t.call(NR_MSGSND, &[id as u64, a, 4, IPC_NOWAIT as u64]) == -EAGAIN);
    let w = other("sender")?;
    w.put(0x100, &m);
    let wa = w.mem + 0x100;
    let s = sleeper("sxipc-snd", w, NR_MSGSND, [id as u64, wa, 4, 0, 0, 0])?;
    sched::sleep_ms(20);
    kassert!(s.still());
    let b = t.mem + 0x400;
    kassert!(t.call(NR_MSGRCV, &[id as u64, b, 64, 0, 0]) == 4);
    kassert!(s.result(5 * SEC) == Some(0), "the sender did not get room");

    let w = other("sender2")?;
    w.put(0x100, &m);
    let wa = w.mem + 0x100;
    let s = sleeper("sxipc-snd2", w, NR_MSGSND, [id as u64, wa, 4, 0, 0, 0])?;
    sched::sleep_ms(20);
    kassert!(t.call(NR_MSGCTL, &[id as u64, IPC_RMID as u64, 0]) == 0);
    kassert!(s.result(5 * SEC) == Some(-EIDRM), "EIDRM expected");
    t.end();
    Ok(())
}
ktest!("sx.ipc.msg_wait", Safe, msg_wait);

/// A namespace goes with everything in it, its queued messages, values and segments included.
fn ns_ends() -> TestResult {
    let mut t = other("ns")?;
    kassert!(t.call(NR_UNSHARE, &[uapi::ab::clone::CLONE_NEWUSER | NEWIPC]) == 0, "unshare");
    let ns = Arc::downgrade(&t.p.nsset().ipc);
    let sem = t.semget(1, 4, IPC_CREAT | 0o600);
    kassert!(t.semop(sem, 0, 1, SEM_UNDO) == 0);
    let shm = t.call(NR_SHMGET, &[1, 8192, (IPC_CREAT | 0o600) as u64]);
    let at = t.call(NR_SHMAT, &[shm as u64, 0, 0]);
    kassert!(at > 0);
    let q = t.call(NR_MSGGET, &[1, (IPC_CREAT | 0o600) as u64]);
    let mut m = [0u8; 16];
    m[0] = 1;
    t.put(0x100, &m);
    let a = t.mem + 0x100;
    kassert!(t.call(NR_MSGSND, &[q as u64, a, 8, 0]) == 0);
    t.end();
    kassert!(until(|| ns.upgrade().is_none()), "the namespace is still there");
    Ok(())
}
ktest!("sx.ipc.ns_ends", Safe, ns_ends);

fn have(path: &str) -> bool {
    vfs::root_ns().lookup(path).is_ok()
}

/// The ABI programs against CachyOS's transcripts (the cases `cargo xtask abi-diff abi-sysv` runs).
fn programs() -> TestResult {
    macro_rules! case {
        ($bin:literal, $mode:literal) => {
            (concat!("/bin/", $bin), $mode, include_str!(concat!("../../../../user/abi-tests/expected/sx-ipc-", $bin, "-", $mode, ".txt")))
        };
    }
    let cases = [
        case!("abi-sysv-sem", "basic"),
        case!("abi-sysv-sem", "order"),
        case!("abi-sysv-sem", "signals"),
        case!("abi-sysv-sem", "undo"),
        case!("abi-sysv-sem", "undoexec"),
        case!("abi-sysv-sem", "seccomp"),
        case!("abi-sysv-sem", "plain"),
        case!("abi-sysv-shm", "basic"),
        case!("abi-sysv-shm", "attach"),
        case!("abi-sysv-shm", "life"),
        case!("abi-sysv-shm", "exec"),
        case!("abi-sysv-shm", "limits"),
        case!("abi-sysv-shm", "plain"),
        case!("abi-sysv-msg", "basic"),
        case!("abi-sysv-msg", "block"),
        case!("abi-sysv-msg", "plain"),
        case!("abi-sysv-ns", "ns"),
        case!("abi-sysv-ns", "proc"),
        case!("abi-sysv-ns", "plainproc"),
    ];
    if !have("/bin/abi-sysv-sem") {
        return ktest::fail("the user/abi-tests programs are not in this image (no musl sysroot when it was built, or abi-sysv-sem failed to build)");
    }
    for (path, mode, want) in cases {
        let out = proc::stdio::Capture::new(1 << 20);
        let argv = [path, mode];
        let env = ["HOME=/", "PATH=/bin", "ABI_TEST=1"];
        let mut o = proc::SpawnOptions::new(&argv, &env, policy::ProgramId(1));
        o.stdin = Some(proc::stdio::Input::new(Vec::new()) as Arc<dyn vfs::File>);
        o.stdout = Some(out.clone() as Arc<dyn vfs::File>);
        o.stderr = Some(out.clone() as Arc<dyn vfs::File>);
        let p = proc::spawn_with(path, &o).map_err(|e| format!("spawn {path}: {e}"))?;
        let Some(code) = p.wait(Some(120 * SEC)) else {
            p.kill(9);
            let _ = p.wait(Some(10 * SEC));
            return Err(format!("{path} {mode} did not end within 120 s:\n{}", String::from_utf8_lossy(&out.contents())));
        };
        let text = String::from_utf8_lossy(&out.contents()).into_owned();
        let bad: Vec<String> = text.lines().zip(want.lines()).enumerate().filter(|(_, (g, w))| g != w).map(|(i, (g, w))| format!("line {}: got {g:?}, want {w:?}", i + 1)).collect();
        let same_len = text.lines().count() == want.lines().count();
        kassert!(code == 0 && bad.is_empty() && same_len, "{path} {mode} differs (exit {code}, {} lines, want {}):\n{}", text.lines().count(), want.lines().count(), bad.join("\n"));
    }
    Ok(())
}
ktest!("sx.ipc.programs", Safe, programs);
