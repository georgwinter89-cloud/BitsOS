//! AB-5 ktests: timerfd, prctl, sysinfo, flock and fcntl's locks and owners through the real dispatcher on held
//! test processes (the test thread plays the program, as the wave-A tests do), and the `abi-ab5` program in
//! its five modes against CachyOS's transcripts (`user/abi-tests/expected/ab5-*.txt`).
//!
//! All `Safe` and rerunnable: every test makes its own processes, files and timers and removes them; every
//! wait has a deadline. Timing is never judged (QEMU): only "it happened" and "not before its time".

use crate::fcntl;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use hal::entry::UserContext;
use ktest::{TestResult, kassert, ktest};
use proc::Process;
use uapi::ab::errno_more::ECANCELED;
use uapi::ab::fcntl::*;
use uapi::ab::nr_more;
use uapi::ab::prctl::*;
use uapi::ab::timerfd::*;
use uapi::errno::*;
use uapi::nr;

const SEC: u64 = 1_000_000_000;
const MS: u64 = 1_000_000;
const EXIT0: [u8; 9] = [0xb8, 0xe7, 0, 0, 0, 0x31, 0xff, 0x0f, 0x05];
const SCRATCH: u64 = 1 << 16;
const KERNEL_ADDR: u64 = 0xffff_8880_0000_0000;
const CLOCK_MONOTONIC: u64 = 1;
const CLOCK_REALTIME: u64 = 0;
const POLLIN: u16 = 1;
const O_RDWR: u64 = 2;
const O_CREAT: u64 = 0o100;
const AT_FDCWD: u64 = -100i64 as u64;

struct T {
    p: Arc<Process>,
    th: Arc<proc::Thread>,
    ctx: UserContext,
    mem: u64,
    _pin: proc::MmPin,
}

impl T {
    fn new(name: &str) -> Result<T, String> {
        let image = crate::ktests::elf(&EXIT0);
        let argv = [name];
        let mut o = proc::SpawnOptions::new(&argv, &[], policy::ProgramId(1));
        o.held = true;
        let p = proc::spawn_image_with(name, &image, &o).map_err(|e| format!("spawn {name}: {e}"))?;
        let mem = p.mm.sys_mmap(0, SCRATCH, 3, 0x22, 0).map_err(|e| format!("mmap: {e:?}"))?;
        let th = p.threads().into_iter().next().ok_or("the held process has no main thread")?;
        let _pin = proc::pin_mm(&p);
        Ok(T { p, th, ctx: UserContext::new(0x40_0078, hal::USER_TOP - 4096), mem, _pin })
    }
    fn call(&mut self, n: usize, args: &[u64]) -> i64 {
        let mut a = [0u64; 6];
        a[..args.len()].copy_from_slice(args);
        let r = &mut self.ctx.regs;
        (r.rax, r.rdi, r.rsi, r.rdx, r.r10, r.r8, r.r9) = (n as u64, a[0], a[1], a[2], a[3], a[4], a[5]);
        crate::dispatch(&self.p, &self.th, &mut self.ctx);
        self.ctx.regs.rax as i64
    }
    /// fcntl through AB-5's handler, registered or not (the integration patch moves the registration).
    fn fcntl(&mut self, args: &[u64]) -> i64 {
        let mut a = [0u64; 6];
        a[..args.len()].copy_from_slice(args);
        let mut c = crate::SysCtx { proc: &self.p, thread: &self.th, user: &mut self.ctx, args: a };
        fcntl::sys_fcntl(&mut c)
    }
    fn a(&self, at: u64) -> u64 {
        self.mem + at
    }
    fn put(&self, at: u64, b: &[u8]) {
        let _ = self.p.mm.copy_to_user(self.mem + at, b);
    }
    fn get(&self, at: u64, len: usize) -> Vec<u8> {
        let mut v = vec![0u8; len];
        let _ = self.p.mm.copy_from_user(&mut v, self.mem + at);
        v
    }
    fn u64_at(&self, at: u64) -> u64 {
        u64::from_le_bytes(self.get(at, 8).try_into().unwrap_or([0; 8]))
    }
    fn i64s(&self, at: u64, n: usize) -> Vec<i64> {
        self.get(at, n * 8).chunks(8).map(|c| i64::from_le_bytes(c.try_into().unwrap_or([0; 8]))).collect()
    }
    fn put_its(&self, at: u64, interval: u64, value: u64) {
        let mut b = [0u8; 32];
        for (i, v) in [interval / SEC, interval % SEC, value / SEC, value % SEC].into_iter().enumerate() {
            b[i * 8..i * 8 + 8].copy_from_slice(&v.to_le_bytes());
        }
        self.put(at, &b);
    }
    fn put_str(&self, at: u64, s: &str) {
        let mut b = Vec::from(s.as_bytes());
        b.push(0);
        self.put(at, &b);
    }
    /// read(fd, 8 bytes) as the program would.
    fn read8(&mut self, fd: i64, at: u64) -> i64 {
        let a = self.a(at);
        self.call(nr::READ, &[fd as u64, a, 8])
    }
    /// poll one fd for POLLIN with `ms`: its revents (or -errno).
    fn poll_in(&mut self, fd: i64, ms: i64) -> i64 {
        let mut b = [0u8; 8];
        b[0..4].copy_from_slice(&(fd as i32).to_le_bytes());
        b[4..6].copy_from_slice(&POLLIN.to_le_bytes());
        self.put(0x800, &b);
        let a = self.a(0x800);
        let r = self.call(nr::POLL, &[a, 1, ms as u64]);
        if r < 0 {
            return r;
        }
        u16::from_le_bytes([self.get(0x806, 2)[0], self.get(0x806, 2)[1]]) as i64
    }
}

impl Drop for T {
    fn drop(&mut self) {
        self.p.kill(9);
        let _ = self.p.wait(Some(10 * SEC));
    }
}

fn expect(what: &str, got: i64, want: i64) -> TestResult {
    kassert!(got == want, "{what}: got {got}, want {want}");
    Ok(())
}

// ---------------------------------------------------------------------------------------------------------
// timerfd

ktest!("ab.ab5.timerfd_calls", Safe, timerfd_calls);
fn timerfd_calls() -> TestResult {
    let timers0 = crate::timerfd::count();
    let hr0 = time::hrtimer::count();
    {
        let mut t = T::new("ab5-tfd")?;
        expect("create(99)", t.call(nr_more::TIMERFD_CREATE, &[99, 0]), -EINVAL)?;
        expect("create(bad flags)", t.call(nr_more::TIMERFD_CREATE, &[CLOCK_MONOTONIC, 1]), -EINVAL)?;
        expect("create(REALTIME_ALARM) as uid 1000", t.call(nr_more::TIMERFD_CREATE, &[8, 0]), -EPERM)?;
        let fd = t.call(nr_more::TIMERFD_CREATE, &[CLOCK_MONOTONIC, TFD_NONBLOCK | TFD_CLOEXEC]);
        kassert!(fd >= 0, "create: {fd}");
        expect("F_GETFL", t.call(nr::FCNTL, &[fd as u64, F_GETFL, 0]), 0o4002)?;
        expect("F_GETFD", t.call(nr::FCNTL, &[fd as u64, F_GETFD, 0]), 1)?;
        expect("read unarmed", t.read8(fd, 0x100), -EAGAIN)?;
        expect("read 4 bytes", t.call(nr::READ, &[fd as u64, t.a(0x100), 4]), -EINVAL)?;
        expect("write", t.call(nr::WRITE, &[fd as u64, t.a(0x100), 8]), -EINVAL)?;
        // Errors in Linux's order: the copy, then flags/values, then the fd.
        t.put_its(0, 0, 50 * MS);
        expect("settime(new kernel address)", t.call(nr_more::TIMERFD_SETTIME, &[fd as u64, 0, KERNEL_ADDR, 0]), -EFAULT)?;
        expect("settime(bad flags)", t.call(nr_more::TIMERFD_SETTIME, &[fd as u64, 4, t.a(0), 0]), -EINVAL)?;
        expect("settime(fd 0: not a timerfd)", t.call(nr_more::TIMERFD_SETTIME, &[0, 0, t.a(0), 0]), -EINVAL)?;
        expect("settime(fd 999)", t.call(nr_more::TIMERFD_SETTIME, &[999, 0, t.a(0), 0]), -EBADF)?;
        let mut bad = [0u8; 32];
        bad[24..32].copy_from_slice(&SEC.to_le_bytes());
        t.put(0x40, &bad);
        expect("settime(tv_nsec 1e9)", t.call(nr_more::TIMERFD_SETTIME, &[fd as u64, 0, t.a(0x40), 0]), -EINVAL)?;
        // One-shot 50 ms: not readable at once, readable within 5 s, never before its time, one tick.
        let t0 = time::now_ns();
        expect("settime 50 ms", t.call(nr_more::TIMERFD_SETTIME, &[fd as u64, 0, t.a(0), 0]), 0)?;
        expect("gettime", t.call(nr_more::TIMERFD_GETTIME, &[fd as u64, t.a(0x80)]), 0)?;
        let g = t.i64s(0x80, 4);
        kassert!(g[0] == 0 && g[1] == 0 && g[2] == 0 && g[3] > 0 && g[3] <= 50 * MS as i64, "gettime after arming: {g:?}");
        let r = t.poll_in(fd, 5000);
        kassert!(r == POLLIN as i64, "poll: {r}");
        kassert!(time::now_ns() - t0 >= 50 * MS, "readable before its time");
        expect("read", t.read8(fd, 0x100), 8)?;
        expect("ticks", t.u64_at(0x100) as i64, 1)?;
        expect("read again", t.read8(fd, 0x100), -EAGAIN)?;
        // Periodic 5 ms: after 60 ms at least 5 expiries in one read; gettime moves an expired one on.
        t.put_its(0, 5 * MS, 5 * MS);
        expect("settime periodic", t.call(nr_more::TIMERFD_SETTIME, &[fd as u64, 0, t.a(0), t.a(0xc0)]), 0)?;
        sched::sleep_ms(60);
        expect("read periodic", t.read8(fd, 0x100), 8)?;
        let n = t.u64_at(0x100);
        kassert!(n >= 5, "periodic: {n} expiries after 60 ms");
        sched::sleep_ms(20);
        t.call(nr_more::TIMERFD_GETTIME, &[fd as u64, t.a(0x80)]);
        let g = t.i64s(0x80, 4);
        kassert!(g[1] == 5 * MS as i64 && g[3] > 0 && g[3] <= 5 * MS as i64, "gettime of an expired periodic timer: {g:?}");
        expect("read after gettime", t.read8(fd, 0x100), 8)?;
        kassert!(t.u64_at(0x100) >= 3, "the periods gettime moved past are counted: {}", t.u64_at(0x100));
        // TFD_IOC_SET_TICKS, then a disarm through settime with the old value.
        t.put(0x140, &7u64.to_le_bytes());
        expect("TFD_IOC_SET_TICKS", t.call(nr::IOCTL, &[fd as u64, TFD_IOC_SET_TICKS, t.a(0x140)]), 0)?;
        t.put_its(0, 0, 0);
        expect("disarm", t.call(nr_more::TIMERFD_SETTIME, &[fd as u64, 0, t.a(0), t.a(0xc0)]), 0)?;
        expect("read after the disarm", t.read8(fd, 0x100), -EAGAIN)?;
        // Absolute in the past on CLOCK_REALTIME: at once.
        let rfd = t.call(nr_more::TIMERFD_CREATE, &[CLOCK_REALTIME, TFD_NONBLOCK]);
        t.put_its(0, 0, 1);
        expect("REALTIME absolute 1 ns", t.call(nr_more::TIMERFD_SETTIME, &[rfd as u64, TFD_TIMER_ABSTIME | TFD_TIMER_CANCEL_ON_SET, t.a(0), 0]), 0)?;
        kassert!(t.poll_in(rfd, 5000) == POLLIN as i64, "a past REALTIME expiry is readable");
        expect("read", t.read8(rfd, 0x100), 8)?;
        // An fd left armed when the process ends: the timer goes with the last close.
        t.put_its(0, MS, MS);
        t.call(nr_more::TIMERFD_SETTIME, &[rfd as u64, 0, t.a(0), 0]);
        kassert!(crate::timerfd::count() >= timers0 + 2, "two timerfds live");
    }
    // The held process was killed and its files closed: every timerfd and HrTimer it made is gone.
    let until = time::now_ns() + 5 * SEC;
    while crate::timerfd::count() != timers0 && time::now_ns() < until {
        sched::sleep_ms(5);
    }
    kassert!(crate::timerfd::count() == timers0, "timerfds left behind: {} (start {timers0})", crate::timerfd::count());
    kassert!(time::hrtimer::count() <= hr0, "HrTimers left behind: {} (start {hr0})", time::hrtimer::count());
    Ok(())
}

ktest!("ab.ab5.timerfd_epoll_edge", Safe, timerfd_epoll_edge);
/// EPOLLET on a periodic timerfd: one edge per expiry that a read re-armed; none while it is not read.
fn timerfd_epoll_edge() -> TestResult {
    let mut t = T::new("ab5-tfd-ep")?;
    let fd = t.call(nr_more::TIMERFD_CREATE, &[CLOCK_MONOTONIC, TFD_NONBLOCK]);
    let ep = t.call(nr::EPOLL_CREATE1, &[0]);
    kassert!(fd >= 0 && ep >= 0, "fd {fd} ep {ep}");
    // struct epoll_event (packed on x86-64): u32 events, u64 data.
    let mut ev = [0u8; 12];
    ev[0..4].copy_from_slice(&(1u32 | 1 << 31).to_le_bytes());
    ev[4..12].copy_from_slice(&42u64.to_le_bytes());
    t.put(0x200, &ev);
    expect("EPOLL_CTL_ADD", t.call(nr::EPOLL_CTL, &[ep as u64, 1, fd as u64, t.a(0x200)]), 0)?;
    t.put_its(0, 10 * MS, 10 * MS);
    t.call(nr_more::TIMERFD_SETTIME, &[fd as u64, 0, t.a(0), 0]);
    expect("first edge", t.call(nr::EPOLL_WAIT, &[ep as u64, t.a(0x300), 4, 5000]), 1)?;
    expect("data", t.u64_at(0x304) as i64, 42)?;
    sched::sleep_ms(40);
    expect("no new edge while not read", t.call(nr::EPOLL_WAIT, &[ep as u64, t.a(0x300), 4, 0]), 0)?;
    expect("read", t.read8(fd, 0x100), 8)?;
    expect("the next expiry after the read is an edge", t.call(nr::EPOLL_WAIT, &[ep as u64, t.a(0x300), 4, 5000]), 1)?;
    Ok(())
}

ktest!("ab.ab5.timerfd_many", Safe, timerfd_many);
/// 200 timerfds armed at once from one thread (the slot table grows), each expires exactly once, then all go.
fn timerfd_many() -> TestResult {
    const N: usize = 200;
    let timers0 = crate::timerfd::count();
    {
        let mut t = T::new("ab5-tfd-many")?;
        let mut fds = Vec::new();
        for i in 0..N {
            let fd = t.call(nr_more::TIMERFD_CREATE, &[CLOCK_MONOTONIC, TFD_NONBLOCK]);
            kassert!(fd >= 0, "create #{i}: {fd}");
            t.put_its(0, 0, (1 + (i as u64 * 7) % 40) * MS);
            expect("settime", t.call(nr_more::TIMERFD_SETTIME, &[fd as u64, 0, t.a(0), 0]), 0)?;
            fds.push(fd);
        }
        sched::sleep_ms(100);
        let until = time::now_ns() + 10 * SEC;
        for (i, fd) in fds.iter().enumerate() {
            loop {
                let r = t.read8(*fd, 0x100);
                if r == 8 {
                    break;
                }
                kassert!(r == -EAGAIN && time::now_ns() < until, "timerfd #{i}: {r}");
                sched::sleep_ms(2);
            }
            expect("one expiry", t.u64_at(0x100) as i64, 1)?;
        }
        for fd in fds {
            expect("close", t.call(nr::CLOSE, &[fd as u64]), 0)?;
        }
        expect("all closed", crate::timerfd::count() as i64, timers0 as i64)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------------------
// prctl, sysinfo

ktest!("ab.ab5.prctl_calls", Safe, prctl_calls);
fn prctl_calls() -> TestResult {
    let mut t = T::new("ab5-prctl")?;
    t.put_str(0, "a-name-longer-than-15");
    expect("PR_SET_NAME", t.call(nr_more::PRCTL, &[PR_SET_NAME as u64, t.a(0)]), 0)?;
    expect("PR_GET_NAME", t.call(nr_more::PRCTL, &[PR_GET_NAME as u64, t.a(0x40)]), 0)?;
    kassert!(&t.get(0x40, 16)[..] == b"a-name-longer-t\0", "comm {:?}", t.get(0x40, 16));
    kassert!(t.th.prctl().comm_str() == "a-name-longer-t", "the thread's comm");
    expect("PR_SET_NAME(kernel address)", t.call(nr_more::PRCTL, &[PR_SET_NAME as u64, KERNEL_ADDR]), -EFAULT)?;
    expect("PR_GET_DUMPABLE", t.call(nr_more::PRCTL, &[PR_GET_DUMPABLE as u64]), 1)?;
    expect("PR_SET_DUMPABLE(2)", t.call(nr_more::PRCTL, &[PR_SET_DUMPABLE as u64, 2]), -EINVAL)?;
    expect("PR_SET_PDEATHSIG(65)", t.call(nr_more::PRCTL, &[PR_SET_PDEATHSIG as u64, 65]), -EINVAL)?;
    expect("PR_SET_PDEATHSIG(9)", t.call(nr_more::PRCTL, &[PR_SET_PDEATHSIG as u64, 9]), 0)?;
    expect("PR_GET_PDEATHSIG", t.call(nr_more::PRCTL, &[PR_GET_PDEATHSIG as u64, t.a(0x80)]), 0)?;
    expect("pdeath", i32::from_le_bytes(t.get(0x80, 4).try_into().unwrap_or([0; 4])) as i64, 9)?;
    expect("PR_SET_CHILD_SUBREAPER", t.call(nr_more::PRCTL, &[PR_SET_CHILD_SUBREAPER as u64, 1]), 0)?;
    kassert!(t.p.is_child_subreaper(), "the group is a subreaper");
    expect("PR_SET_NO_NEW_PRIVS(1, 1)", t.call(nr_more::PRCTL, &[PR_SET_NO_NEW_PRIVS as u64, 1, 1]), -EINVAL)?;
    expect("PR_SET_NO_NEW_PRIVS", t.call(nr_more::PRCTL, &[PR_SET_NO_NEW_PRIVS as u64, 1]), 0)?;
    expect("PR_GET_NO_NEW_PRIVS", t.call(nr_more::PRCTL, &[PR_GET_NO_NEW_PRIVS as u64]), 1)?;
    expect("PR_GET_AUXV(NULL, 0)", t.call(nr_more::PRCTL, &[PR_GET_AUXV as u64, 0, 0]), 416)?;
    expect("PR_SET_SECCOMP (not built)", t.call(nr_more::PRCTL, &[PR_SET_SECCOMP as u64, 1]), -EINVAL)?;
    expect("unknown option", t.call(nr_more::PRCTL, &[0x7fff_ffff]), -EINVAL)?;
    Ok(())
}

ktest!("ab.ab5.sysinfo", Safe, sysinfo_call);
fn sysinfo_call() -> TestResult {
    let mut t = T::new("ab5-sysinfo")?;
    expect("sysinfo", t.call(nr_more::SYSINFO, &[t.a(0)]), 0)?;
    let b = t.get(0, crate::sysinfo::SYSINFO_SIZE);
    let w = |off: usize| u64::from_le_bytes(b[off..off + 8].try_into().unwrap_or([0; 8]));
    kassert!(w(0) > 0, "uptime {}", w(0));
    kassert!(w(32) > 0 && w(40) <= w(32), "totalram {} freeram {}", w(32), w(40));
    kassert!(u16::from_le_bytes([b[80], b[81]]) > 0, "procs");
    kassert!(u32::from_le_bytes(b[104..108].try_into().unwrap_or([0; 4])) == 1, "mem_unit");
    expect("sysinfo(kernel address)", t.call(nr_more::SYSINFO, &[KERNEL_ADDR]), -EFAULT)?;
    kassert!(sched::threads().iter().any(|(_, n, _, _)| n == "loadavg"), "the load-average sampler runs");
    Ok(())
}

// ---------------------------------------------------------------------------------------------------------
// flock and fcntl locks

/// A file in /tmp both test processes open (removed at the end).
struct TmpFile(String);

impl Drop for TmpFile {
    fn drop(&mut self) {
        let _ = vfs::root_ns().remove(&self.0);
    }
}

fn open_in(t: &mut T, path: &str) -> i64 {
    t.put_str(0x600, path);
    let a = t.a(0x600);
    t.call(nr::OPENAT, &[AT_FDCWD, a, O_RDWR | O_CREAT, 0o600])
}

fn flock_bytes(l_type: i16, start: i64, len: i64) -> [u8; FLOCK_SIZE] {
    let mut b = [0u8; FLOCK_SIZE];
    b[0..2].copy_from_slice(&l_type.to_le_bytes());
    b[8..16].copy_from_slice(&start.to_le_bytes());
    b[16..24].copy_from_slice(&len.to_le_bytes());
    b
}

static FILE_SEQ: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

fn tmp_name(what: &str) -> TmpFile {
    TmpFile(format!("/tmp/ab5-{what}-{}", FILE_SEQ.fetch_add(1, core::sync::atomic::Ordering::Relaxed)))
}

ktest!("ab.ab5.flock", Safe, flock_test);
fn flock_test() -> TestResult {
    let file = tmp_name("flock");
    let nodes0 = proc::locks::inode_count();
    {
        let mut a = T::new("ab5-flock-a")?;
        let mut b = T::new("ab5-flock-b")?;
        let fa = open_in(&mut a, &file.0);
        let fb = open_in(&mut b, &file.0);
        kassert!(fa >= 0 && fb >= 0, "open {fa} {fb}");
        expect("a LOCK_EX", a.call(nr_more::FLOCK, &[fa as u64, LOCK_EX]), 0)?;
        expect("b LOCK_SH|LOCK_NB", b.call(nr_more::FLOCK, &[fb as u64, LOCK_SH | LOCK_NB]), -EAGAIN)?;
        expect("bad command", b.call(nr_more::FLOCK, &[fb as u64, LOCK_SH | LOCK_EX]), -EINVAL)?;
        expect("bad fd", b.call(nr_more::FLOCK, &[999, LOCK_SH]), -EBADF)?;
        // b waits (blocking flock) in a helper thread that plays b's thread; a's unlock lets it through.
        let bp = b.p.clone();
        let bth = b.th.clone();
        let res = Arc::new(core::sync::atomic::AtomicI64::new(i64::MIN));
        let r2 = res.clone();
        let h = sched::spawn("ab5-flock-wait", sched::Class::Normal, move || {
            let mut ctx = UserContext::new(0x40_0078, hal::USER_TOP - 4096);
            let mut c = crate::SysCtx { proc: &bp, thread: &bth, user: &mut ctx, args: [fb as u64, LOCK_SH, 0, 0, 0, 0] };
            r2.store(fcntl::sys_flock(&mut c), core::sync::atomic::Ordering::Release);
        })
        .map_err(|e| format!("spawn: {e:?}"))?;
        sched::sleep_ms(50);
        kassert!(res.load(core::sync::atomic::Ordering::Acquire) == i64::MIN, "the blocking flock did not wait");
        expect("a LOCK_UN", a.call(nr_more::FLOCK, &[fa as u64, LOCK_UN]), 0)?;
        kassert!(h.join_timeout(5 * SEC), "the waiter did not get the lock within 5 s");
        expect("the waiter's flock", res.load(core::sync::atomic::Ordering::Acquire), 0)?;
        expect("a LOCK_SH|LOCK_NB shares", a.call(nr_more::FLOCK, &[fa as u64, LOCK_SH | LOCK_NB]), 0)?;
        expect("a LOCK_EX|LOCK_NB fails", a.call(nr_more::FLOCK, &[fa as u64, LOCK_EX | LOCK_NB]), -EAGAIN)?;
        // b ends: its open file goes, and with it its lock.
        drop(b);
        let until = time::now_ns() + 5 * SEC;
        loop {
            let r = a.call(nr_more::FLOCK, &[fa as u64, LOCK_EX | LOCK_NB]);
            if r == 0 {
                break;
            }
            kassert!(r == -EAGAIN && time::now_ns() < until, "after b's end: {r}");
            sched::sleep_ms(5);
        }
    }
    // The inode's node stays until the next operation finds its locks' owners gone (lazy, by design): at most
    // the one of this test's file.
    kassert!(proc::locks::inode_count() <= nodes0 + 1, "lock nodes: {} (start {nodes0})", proc::locks::inode_count());
    Ok(())
}

ktest!("ab.ab5.record_locks", Safe, record_locks);
/// POSIX record locks between two processes through AB-5's fcntl: F_GETLK reports the holder, F_SETLK
/// conflicts, F_SETLKW waits and gets EDEADLK in a circle, the end of a process frees its locks.
fn record_locks() -> TestResult {
    let file = tmp_name("lock");
    let posix0 = proc::locks::posix_count();
    let blocked0 = proc::locks::blocked_requests();
    {
        let mut a = T::new("ab5-lock-a")?;
        let mut b = T::new("ab5-lock-b")?;
        let fa = open_in(&mut a, &file.0);
        let fb = open_in(&mut b, &file.0);
        kassert!(fa >= 0 && fb >= 0, "open {fa} {fb}");
        a.put(0x700, &flock_bytes(1, 0, 10));
        expect("a F_SETLK W 0..9", a.fcntl(&[fa as u64, F_SETLK, a.a(0x700)]), 0)?;
        b.put(0x700, &flock_bytes(0, 5, 1));
        expect("b F_GETLK R 5", b.fcntl(&[fb as u64, F_GETLK, b.a(0x700)]), 0)?;
        let got = b.get(0x700, FLOCK_SIZE);
        let typ = i16::from_le_bytes([got[0], got[1]]);
        let start = i64::from_le_bytes(got[8..16].try_into().unwrap_or([0; 8]));
        let len = i64::from_le_bytes(got[16..24].try_into().unwrap_or([0; 8]));
        let pid = i32::from_le_bytes(got[24..28].try_into().unwrap_or([0; 4]));
        kassert!((typ, start, len, pid) == (1, 0, 10, a.p.pid as i32), "F_GETLK: {typ} {start} {len} {pid}");
        b.put(0x700, &flock_bytes(1, 20, 5));
        expect("b F_SETLK W 20..24", b.fcntl(&[fb as u64, F_SETLK, b.a(0x700)]), 0)?;
        b.put(0x700, &flock_bytes(0, 9, 1));
        expect("b F_SETLK R 9 conflicts", b.fcntl(&[fb as u64, F_SETLK, b.a(0x700)]), -EAGAIN)?;
        // b waits for 9 in a helper thread (its process's thread), then a asks for 20 with F_SETLKW: EDEADLK.
        let bp = b.p.clone();
        let bth = b.th.clone();
        let mem = b.a(0x780);
        let _ = b.p.mm.copy_to_user(mem, &flock_bytes(1, 9, 1));
        let res = Arc::new(core::sync::atomic::AtomicI64::new(i64::MIN));
        let r2 = res.clone();
        let h = sched::spawn("ab5-lock-wait", sched::Class::Normal, move || {
            let mut ctx = UserContext::new(0x40_0078, hal::USER_TOP - 4096);
            let mut c = crate::SysCtx { proc: &bp, thread: &bth, user: &mut ctx, args: [fb as u64, F_SETLKW, mem, 0, 0, 0] };
            r2.store(fcntl::sys_fcntl(&mut c), core::sync::atomic::Ordering::Release);
        })
        .map_err(|e| format!("spawn: {e:?}"))?;
        // INT-A2a2: wait until b's request really sleeps (its edge is in the deadlock table); a fixed 100 ms
        // let a ask first on a busy host, and then a waited for b forever (gate hang).
        let until = time::now_ns() + 5 * SEC;
        while proc::locks::blocked_requests() <= blocked0 && res.load(core::sync::atomic::Ordering::Acquire) == i64::MIN && time::now_ns() < until {
            sched::sleep_ms(2);
        }
        kassert!(res.load(core::sync::atomic::Ordering::Acquire) == i64::MIN, "b's F_SETLKW did not wait: {}", res.load(core::sync::atomic::Ordering::Acquire));
        kassert!(proc::locks::blocked_requests() > blocked0, "b's F_SETLKW did not block within 5 s");
        a.put(0x700, &flock_bytes(1, 22, 1));
        // A guard ends a after 5 s should the circle be missed, so a wrong answer fails the test instead of
        // hanging the gate (the wait then ends with EINTR).
        let a_done = Arc::new(core::sync::atomic::AtomicBool::new(false));
        let guard = {
            let (ap, done) = (a.p.clone(), a_done.clone());
            sched::spawn("ab5-lock-guard", sched::Class::Normal, move || {
                let until = time::now_ns() + 5 * SEC;
                while !done.load(core::sync::atomic::Ordering::Acquire) {
                    if time::now_ns() >= until {
                        ap.kill(9);
                        return;
                    }
                    sched::sleep_ms(5);
                }
            })
            .map_err(|e| format!("spawn guard: {e:?}"))?
        };
        let r = a.fcntl(&[fa as u64, F_SETLKW, a.a(0x700)]);
        a_done.store(true, core::sync::atomic::Ordering::Release);
        let _ = guard.join_timeout(5 * SEC);
        expect("a F_SETLKW W 22 closes the circle", r, -EDEADLK)?;
        // a unlocks 9: b's waiter gets it.
        a.put(0x700, &flock_bytes(2, 9, 1));
        expect("a F_SETLK U 9", a.fcntl(&[fa as u64, F_SETLK, a.a(0x700)]), 0)?;
        kassert!(h.join_timeout(5 * SEC), "b's waiter did not get 9 within 5 s");
        expect("b's F_SETLKW", res.load(core::sync::atomic::Ordering::Acquire), 0)?;
        // b ends: all its locks go (release_owner at the end of its files).
        drop(b);
        a.put(0x700, &flock_bytes(1, 9, 20));
        expect("a F_SETLK W 9..28 after b's end", a.fcntl(&[fa as u64, F_SETLK, a.a(0x700)]), 0)?;
        // OFD lock of a's open file conflicts with a's own POSIX lock (different owners).
        a.put(0x700, &flock_bytes(1, 0, 1));
        expect("F_OFD_SETLK on a POSIX-locked range", a.fcntl(&[fa as u64, F_OFD_SETLK, a.a(0x700)]), -EAGAIN)?;
        // F_SETOWN / F_GETOWN_EX on the same open file.
        expect("F_SETOWN(own pid)", a.fcntl(&[fa as u64, F_SETOWN, a.p.pid as u64]), 0)?;
        expect("F_GETOWN_EX", a.fcntl(&[fa as u64, F_GETOWN_EX, a.a(0x7c0)]), 0)?;
        let ex = a.get(0x7c0, 8);
        kassert!(i32::from_le_bytes(ex[0..4].try_into().unwrap_or([0; 4])) == F_OWNER_PID && u32::from_le_bytes(ex[4..8].try_into().unwrap_or([0; 4])) == a.p.pid, "F_GETOWN_EX {ex:?}");
        expect("F_SETOWN(no such pid)", a.fcntl(&[fa as u64, F_SETOWN, 4_000_000]), -ESRCH)?;
        // INT-A2a: with FS-10 every tmpfs file is a shmem file whose seals start at F_SEAL_SEAL
        // (linux-6.6/mm/shmem.c:2464), so memfd_add_seals answers EPERM (mm/memfd.c), as AB-5 CR5 saw on CachyOS.
        expect("F_ADD_SEALS on a tmpfs file (not a memfd)", a.fcntl(&[fa as u64, F_ADD_SEALS, 1]), -EPERM)?;
        expect("F_DUPFD (FS's part)", a.fcntl(&[fa as u64, F_DUPFD, 40]), 40)?;
    }
    let until = time::now_ns() + 5 * SEC;
    while proc::locks::posix_count() > posix0 && time::now_ns() < until {
        sched::sleep_ms(5);
    }
    kassert!(proc::locks::posix_count() <= posix0, "POSIX locks left: {} (start {posix0})", proc::locks::posix_count());
    Ok(())
}

impl T {
    /// One of AB-5's not-yet-registered handlers (close/dup2/dup3 with the lock release) as the program.
    fn direct(&mut self, h: crate::Handler, args: &[u64]) -> i64 {
        let mut a = [0u64; 6];
        a[..args.len()].copy_from_slice(args);
        let mut c = crate::SysCtx { proc: &self.p, thread: &self.th, user: &mut self.ctx, args: a };
        h(&mut c)
    }
}

/// b's F_SETLK W on [0, 9]: 0 when a no longer holds it, -EAGAIN while it does (b unlocks again at once).
fn b_can_lock(b: &mut T, fb: i64) -> Result<bool, String> {
    b.put(0x700, &flock_bytes(1, 0, 10));
    let r = b.fcntl(&[fb as u64, F_SETLK, b.a(0x700)]);
    if r == 0 {
        b.put(0x700, &flock_bytes(2, 0, 10));
        b.fcntl(&[fb as u64, F_SETLK, b.a(0x700)]);
        return Ok(true);
    }
    kassert!(r == -EAGAIN, "b's F_SETLK: {r}");
    Ok(false)
}

const CLOSE_RANGE_UNSHARE: u64 = 2;
const CLOSE_RANGE_CLOEXEC: u64 = 4;

ktest!("ab.ab5.lock_release", Safe, lock_release);
/// Every way an fd of the locked file goes drops the process's POSIX locks on it (`filp_close` →
/// `locks_remove_posix`): close of *another* fd of the file, dup2 and dup3 over an fd of it (also when the
/// replaced fd names the very open file installed there), close_range; dup2(fd, fd) replaces nothing.
/// close/dup2/dup3 through AB-5's wrappers (registered by the integration patch), close_range through the
/// dispatcher (AB registers it). Also close_range's own answers.
fn lock_release() -> TestResult {
    let file = tmp_name("release");
    let other = tmp_name("release-other");
    let posix0 = proc::locks::posix_count();
    {
        let mut a = T::new("ab5-rel-a")?;
        let mut b = T::new("ab5-rel-b")?;
        let fa = open_in(&mut a, &file.0);
        let fb = open_in(&mut b, &file.0);
        let fo = open_in(&mut a, &other.0);
        kassert!(fa >= 0 && fb >= 0 && fo >= 0, "open {fa} {fb} {fo}");
        let lock = |a: &mut T| -> TestResult {
            a.put(0x700, &flock_bytes(1, 0, 10));
            expect("a F_SETLK W 0..9", a.fcntl(&[fa as u64, F_SETLK, a.a(0x700)]), 0)
        };
        // close of a second fd of the same file (its own open file description).
        lock(&mut a)?;
        kassert!(!b_can_lock(&mut b, fb)?, "a holds the lock");
        let fa2 = open_in(&mut a, &file.0);
        expect("close(second fd)", a.direct(fcntl::sys_close, &[fa2 as u64]), 0)?;
        kassert!(b_can_lock(&mut b, fb)?, "close of another fd of the file released a's lock");
        expect("close(999)", a.direct(fcntl::sys_close, &[999]), -EBADF)?;
        // dup2(fd, fd): nothing replaced, the lock stays.
        lock(&mut a)?;
        expect("dup2(fa, fa)", a.direct(fcntl::sys_dup2, &[fa as u64, fa as u64]), fa)?;
        kassert!(!b_can_lock(&mut b, fb)?, "dup2(fd, fd) kept the lock");
        // dup2 over a dup of the same open file: Linux still closes the replaced file.
        let d = a.call(nr::DUP, &[fa as u64]);
        kassert!(d >= 0, "dup: {d}");
        expect("dup2(fa, dup of fa)", a.direct(fcntl::sys_dup2, &[fa as u64, d as u64]), d)?;
        kassert!(b_can_lock(&mut b, fb)?, "dup2 over an fd naming the same open file released the lock");
        // dup3 of another file over an fd of the locked file.
        lock(&mut a)?;
        expect("dup3(other, d, O_CLOEXEC)", a.direct(fcntl::sys_dup3, &[fo as u64, d as u64, 0o2000000]), d)?;
        kassert!(b_can_lock(&mut b, fb)?, "dup3 over an fd of the file released the lock");
        expect("dup3(fa, fa, 0)", a.direct(fcntl::sys_dup3, &[fa as u64, fa as u64, 0]), -EINVAL)?;
        // dup2 into a free fd closes nothing.
        lock(&mut a)?;
        expect("dup2(fa, 77) into a free fd", a.direct(fcntl::sys_dup2, &[fa as u64, 77]), 77)?;
        kassert!(!b_can_lock(&mut b, fb)?, "dup2 into a free fd kept the lock");
        // close_range over 77 (an fd of the file): released; CLOEXEC only marks.
        expect("close_range(5, 3)", a.call(nr_more::CLOSE_RANGE, &[5, 3, 0]), -EINVAL)?;
        expect("close_range(bad flag)", a.call(nr_more::CLOSE_RANGE, &[77, 77, 8]), -EINVAL)?;
        expect("close_range(77, ~0, CLOEXEC)", a.call(nr_more::CLOSE_RANGE, &[77, u32::MAX as u64, CLOSE_RANGE_CLOEXEC]), 0)?;
        expect("F_GETFD(77)", a.call(nr::FCNTL, &[77, F_GETFD, 0]), 1)?;
        kassert!(!b_can_lock(&mut b, fb)?, "CLOSE_RANGE_CLOEXEC closes nothing");
        expect("close_range(77, ~0, UNSHARE) on an own table", a.call(nr_more::CLOSE_RANGE, &[77, u32::MAX as u64, CLOSE_RANGE_UNSHARE]), 0)?;
        expect("F_GETFD(77) after", a.call(nr::FCNTL, &[77, F_GETFD, 0]), -EBADF)?;
        kassert!(b_can_lock(&mut b, fb)?, "close_range released the lock");
        expect("close_range(1000, ~0) of nothing", a.call(nr_more::CLOSE_RANGE, &[1000, u32::MAX as u64, 0]), 0)?;
        expect("fa still open", a.call(nr::FCNTL, &[fa as u64, F_GETFD, 0]), 0)?;
    }
    let until = time::now_ns() + 5 * SEC;
    while proc::locks::posix_count() > posix0 && time::now_ns() < until {
        sched::sleep_ms(5);
    }
    kassert!(proc::locks::posix_count() <= posix0, "POSIX locks left: {} (start {posix0})", proc::locks::posix_count());
    Ok(())
}

/// Moves the wall clock by `ns` and tells the timerfds (as a clock_settime would); put back on drop.
struct WallJump(i64);

impl WallJump {
    fn new(ns: i64) -> WallJump {
        jump(ns);
        WallJump(ns)
    }
}

fn jump(ns: i64) {
    let now = crate::clock::realtime_ns() as i64;
    time::wall::set_realtime(now.saturating_add(ns).max(0) as u64);
    crate::timerfd::clock_was_set();
}

impl Drop for WallJump {
    fn drop(&mut self) {
        jump(-self.0);
    }
}

ktest!("ab.ab5.timerfd_cancel_on_set", QemuOnly, timerfd_cancel_on_set);
/// TFD_TIMER_CANCEL_ON_SET and REALTIME re-arming when the wall clock jumps (QemuOnly: it moves the wall
/// clock for a moment). A call of `clock_was_set` without a jump cancels nothing; after a 3 h jump forward the
/// cancel-on-set timer polls readable and reads ECANCELED once, an absolute REALTIME timer 1 h ahead fires
/// (now in the past), one 10 h ahead stays (7 h left), a relative REALTIME timer and a MONOTONIC one are not
/// moved.
fn timerfd_cancel_on_set() -> TestResult {
    if time::wall::realtime_ns().is_none() {
        return ktest::skip("no wall clock in this boot");
    }
    const H: u64 = 3600 * SEC;
    let mut t = T::new("ab5-tfd-cancel")?;
    let mk = |t: &mut T| t.call(nr_more::TIMERFD_CREATE, &[CLOCK_REALTIME, TFD_NONBLOCK]);
    let (cancel, near, far, rel) = (mk(&mut t), mk(&mut t), mk(&mut t), mk(&mut t));
    let mono = t.call(nr_more::TIMERFD_CREATE, &[CLOCK_MONOTONIC, TFD_NONBLOCK]);
    kassert!(cancel >= 0 && near >= 0 && far >= 0 && rel >= 0 && mono >= 0, "create {cancel} {near} {far} {rel} {mono}");
    let now = crate::clock::realtime_ns();
    t.put_its(0, 0, now + H);
    expect("cancel-on-set at +1 h", t.call(nr_more::TIMERFD_SETTIME, &[cancel as u64, TFD_TIMER_ABSTIME | TFD_TIMER_CANCEL_ON_SET, t.a(0), 0]), 0)?;
    expect("absolute at +1 h", t.call(nr_more::TIMERFD_SETTIME, &[near as u64, TFD_TIMER_ABSTIME, t.a(0), 0]), 0)?;
    t.put_its(0, 0, now + 10 * H);
    expect("absolute at +10 h", t.call(nr_more::TIMERFD_SETTIME, &[far as u64, TFD_TIMER_ABSTIME, t.a(0), 0]), 0)?;
    t.put_its(0, 0, 2 * H);
    expect("relative 2 h", t.call(nr_more::TIMERFD_SETTIME, &[rel as u64, 0, t.a(0), 0]), 0)?;
    expect("monotonic relative 2 h", t.call(nr_more::TIMERFD_SETTIME, &[mono as u64, 0, t.a(0), 0]), 0)?;
    crate::timerfd::clock_was_set();
    expect("no jump: nothing cancelled", t.poll_in(cancel, 0), 0)?;
    expect("no jump: read", t.read8(cancel, 0x100), -EAGAIN)?;
    {
        let _j = WallJump::new(3 * H as i64);
        expect("after the jump: the cancel-on-set timer is readable", t.poll_in(cancel, 5000), POLLIN as i64)?;
        expect("read: ECANCELED", t.read8(cancel, 0x100), -ECANCELED)?;
        expect("read again: EAGAIN (reported once)", t.read8(cancel, 0x100), -EAGAIN)?;
        expect("the absolute +1 h timer is in the past now and fires", t.poll_in(near, 5000), POLLIN as i64)?;
        expect("read it", t.read8(near, 0x100), 8)?;
        expect("one expiry", t.u64_at(0x100) as i64, 1)?;
        expect("the +10 h timer still waits", t.poll_in(far, 0), 0)?;
        expect("gettime +10 h", t.call(nr_more::TIMERFD_GETTIME, &[far as u64, t.a(0x80)]), 0)?;
        // it_value.tv_sec (struct itimerspec: it_interval, then it_value).
        let rem = t.i64s(0x80, 4)[2];
        kassert!(rem > 6 * 3600 && rem < 7 * 3600, "the +10 h timer has {rem} s left, want just under 7 h");
        for (what, fd) in [("relative REALTIME", rel), ("MONOTONIC", mono)] {
            t.call(nr_more::TIMERFD_GETTIME, &[fd as u64, t.a(0x80)]);
            let rem = t.i64s(0x80, 4)[2];
            kassert!(rem > 3600 && rem < 2 * 3600, "{what} timer: {rem} s left, want just under 2 h (not moved)");
            expect(what, t.poll_in(fd, 0), 0)?;
        }
        // A cancel-on-set setting made after the jump sees the next jump (the wall clock going back).
        t.put_its(0, 0, crate::clock::realtime_ns() + H);
        expect("settime cancel-on-set again", t.call(nr_more::TIMERFD_SETTIME, &[cancel as u64, TFD_TIMER_ABSTIME | TFD_TIMER_CANCEL_ON_SET, t.a(0), 0]), 0)?;
    }
    expect("after the jump back: ECANCELED", t.read8(cancel, 0x100), -ECANCELED)?;
    Ok(())
}

ktest!("ab.ab5.timerfd_churn", QemuOnly, timerfd_churn);
/// Several threads (own processes, spread over the CPUs) arm short periodic timerfds, re-arm them while they
/// fire, and close them with the callback possibly on its way, many times: no panic, and afterwards no
/// timerfd, slot or HrTimer is left (QemuOnly: a load test; run with -smp 16).
fn timerfd_churn() -> TestResult {
    const THREADS: usize = 6;
    const ROUNDS: usize = 1000;
    let timers0 = crate::timerfd::count();
    let hr0 = time::hrtimer::count();
    let errors = Arc::new(ksync::SpinLock::named("ktest.ab5.churn", Vec::<String>::new()));
    let mut hs = Vec::new();
    for i in 0..THREADS {
        let errors = errors.clone();
        let h = sched::spawn("ab5-tfd-churn", sched::Class::Normal, move || {
            let r = (|| -> TestResult {
                let mut t = T::new("ab5-churn")?;
                for round in 0..ROUNDS {
                    let fd = t.call(nr_more::TIMERFD_CREATE, &[CLOCK_MONOTONIC, TFD_NONBLOCK]);
                    kassert!(fd >= 0, "thread {i} round {round}: create {fd}");
                    let us = 1_000 * (20 + (round as u64 * 37 + i as u64 * 11) % 200);
                    t.put_its(0, us, us / 2);
                    expect("settime", t.call(nr_more::TIMERFD_SETTIME, &[fd as u64, 0, t.a(0), 0]), 0)?;
                    if round % 3 == 0 {
                        sched::sleep_ms(1);
                    }
                    t.put_its(0, us / 2 + 1, us / 3 + 1);
                    expect("re-arm", t.call(nr_more::TIMERFD_SETTIME, &[fd as u64, 0, t.a(0), 0]), 0)?;
                    if round % 5 == 0 {
                        let r = t.read8(fd, 0x100);
                        kassert!(r == 8 || r == -EAGAIN, "read {r}");
                    }
                    expect("close", t.call(nr::CLOSE, &[fd as u64]), 0)?;
                }
                Ok(())
            })();
            if let Err(e) = r {
                errors.lock().push(e);
            }
        })
        .map_err(|e| format!("spawn: {e:?}"))?;
        hs.push(h);
    }
    for h in &hs {
        kassert!(h.join_timeout(120 * SEC), "a churn thread did not end within 120 s");
    }
    let errs = core::mem::take(&mut *errors.lock());
    kassert!(errs.is_empty(), "churn: {}", errs.join("; "));
    let until = time::now_ns() + 5 * SEC;
    while (crate::timerfd::count() != timers0 || time::hrtimer::count() > hr0) && time::now_ns() < until {
        sched::sleep_ms(5);
    }
    kassert!(crate::timerfd::count() == timers0, "timerfds left: {} (start {timers0})", crate::timerfd::count());
    kassert!(time::hrtimer::count() <= hr0, "HrTimers left: {} (start {hr0})", time::hrtimer::count());
    Ok(())
}

// ---------------------------------------------------------------------------------------------------------
// The program against CachyOS

fn have(path: &str) -> bool {
    vfs::root_ns().lookup(path).is_ok()
}

fn diff(got: &str, want: &str) -> Vec<String> {
    let g: Vec<&str> = got.lines().collect();
    let mut d = Vec::new();
    for (i, w) in want.lines().enumerate() {
        let gl = g.get(i).copied().unwrap_or("<missing>");
        if gl != w {
            d.push(format!("line {}: linux '{w}' got '{gl}'", i + 1));
        }
    }
    if g.len() > want.lines().count() {
        d.push(format!("{} extra lines", g.len() - want.lines().count()));
    }
    d
}

ktest!("ab.ab5.programs", Safe, programs);
fn programs() -> TestResult {
    if !have("/bin/abi-ab5") {
        if have("/bin/abi-ids") {
            return Err(String::from("the image has ABI programs but not /bin/abi-ab5 (did it fail to build?)"));
        }
        return ktest::skip("the user/abi-tests programs are not in this image (no musl sysroot when it was built)");
    }
    let modes: [(&str, &str); 5] = [
        ("timerfd", include_str!("../../../user/abi-tests/expected/ab5-timerfd.txt")),
        ("prctl", include_str!("../../../user/abi-tests/expected/ab5-prctl.txt")),
        ("misc", include_str!("../../../user/abi-tests/expected/ab5-misc.txt")),
        ("flock", include_str!("../../../user/abi-tests/expected/ab5-flock.txt")),
        ("fcntl", include_str!("../../../user/abi-tests/expected/ab5-fcntl.txt")),
    ];
    let mut skipped = Vec::new();
    for (mode, want) in modes {
        if mode == "fcntl" && crate::fcntl_is_ab5() {
            // The integration patch moves four registrations together: fcntl without the close/dup2/dup3
            // wrappers would leave POSIX locks behind at a close.
            kassert!(crate::close_dup_are_ab5(), "fcntl is AB-5's but close/dup2/dup3 are not: the integration patch is half applied");
        }
        if mode == "fcntl" && !crate::fcntl_is_ab5() {
            // Until the desk applies AB-5's integration patch, fcntl is FS's (no lock table, no owners).
            skipped.push(mode);
            continue;
        }
        let out = proc::stdio::Capture::new(1 << 20);
        let argv = ["abi-ab5", mode];
        let env = ["HOME=/", "PATH=/bin", "ABI_TEST=1"];
        let mut o = proc::SpawnOptions::new(&argv, &env, policy::ProgramId(1));
        o.stdin = Some(proc::stdio::Input::new(Vec::new()) as Arc<dyn vfs::File>);
        o.stdout = Some(out.clone() as Arc<dyn vfs::File>);
        o.stderr = Some(out.clone() as Arc<dyn vfs::File>);
        let p = proc::spawn_with("/bin/abi-ab5", &o).map_err(|e| format!("spawn: {e}"))?;
        let code = match p.wait(Some(120 * SEC)) {
            Some(c) => c,
            None => {
                p.kill(9);
                let _ = p.wait(Some(10 * SEC));
                return Err(format!("abi-ab5 {mode} did not end within 120 s:\n{}", String::from_utf8_lossy(&out.contents())));
            }
        };
        let text = String::from_utf8_lossy(&out.contents()).into_owned();
        let d = diff(&text, want);
        kassert!(code == 0 && d.is_empty(), "abi-ab5 {mode} differs from CachyOS (exit {code}):\n{}", d.join("\n"));
        klog::event!(Info, "ktest", "ab5.program", mode = mode, lines = text.lines().count());
    }
    if !skipped.is_empty() {
        klog::event!(Info, "ktest", "ab5.program_skipped", modes = format!("{skipped:?}").as_str(); "fcntl is FS's until the AB-5 integration patch");
    }
    Ok(())
}
