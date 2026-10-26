//! AB-3 ktests: the wave-A calls through the real dispatcher (`crate::dispatch`, the same function the
//! process loop calls after SYSCALL) on a test process that is held before its first instruction. The test
//! thread plays the program: it writes arguments into the process's memory, sets the registers, dispatches
//! and reads the answers back. That checks Linux's argument rules and error order exactly; the real-program
//! path (musl and Rust std through SYSCALL) is `ab.wave_a.rust_std_hello` here, `smoke.*`, AB-1's asm
//! programs and the ABI diff (`cargo xtask abi-diff`).
//!
//! All `Safe` and rerunnable: every test makes its own processes and kills them at the end.

use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use hal::entry::UserContext;
use ktest::{TestResult, kassert, ktest};
use proc::Process;
use proc::status::ExitStatus;
use uapi::ab::nr_more;
use uapi::errno::*;
use uapi::nr;
use vfs::File;

const SEC: u64 = 1_000_000_000;
const MS: u64 = 1_000_000;
/// `mov eax, 231; xor edi, edi; syscall`: never runs (the process stays held), but it is a valid program.
const EXIT0: [u8; 9] = [0xb8, 0xe7, 0, 0, 0, 0x31, 0xff, 0x0f, 0x05];
/// Scratch memory of every test process: this many bytes are mapped, the page after them is not.
const SCRATCH: u64 = 1 << 20;
const KERNEL_ADDR: u64 = 0xffff_8880_0000_0000;

pub(crate) struct T {
    pub(crate) p: Arc<Process>,
    /// The process's main thread (the dispatcher's caller in these tests).
    pub(crate) th: Arc<proc::Thread>,
    ctx: UserContext,
    /// Start of SCRATCH mapped bytes (RW, private, anonymous); `mem + SCRATCH` is unmapped.
    mem: u64,
    /// The test thread acts as the program: the memory stays readable after a kill ends the process (AB-4
    /// gives a process's memory back at its end, as Linux's exit_mm).
    _pin: proc::MmPin,
}

impl T {
    pub(crate) fn new(name: &str) -> Result<T, String> {
        Self::with(name, |_| {})
    }
    pub(crate) fn with(name: &str, f: impl FnOnce(&mut proc::SpawnOptions<'_>)) -> Result<T, String> {
        let image = crate::ktests::elf(&EXIT0);
        let argv = [name];
        let mut o = proc::SpawnOptions::new(&argv, &[], policy::ProgramId(1));
        o.held = true;
        f(&mut o);
        let p = proc::spawn_image_with(name, &image, &o).map_err(|e| format!("spawn {name}: {e}"))?;
        let mem = p.mm.sys_mmap(0, SCRATCH + 4096, 3, 0x22, 0).map_err(|e| format!("mmap: {e:?}"))?;
        p.mm.sys_munmap(mem + SCRATCH, 4096).map_err(|e| format!("munmap: {e:?}"))?;
        let ctx = UserContext::new(0x40_0078, hal::USER_TOP - 4096);
        let th = p.threads().into_iter().next().ok_or("the held process has no main thread")?;
        let _pin = proc::pin_mm(&p);
        Ok(T { p, th, ctx, mem, _pin })
    }
    pub(crate) fn call(&mut self, nr: usize, args: &[u64]) -> i64 {
        let mut a = [0u64; 6];
        a[..args.len()].copy_from_slice(args);
        let r = &mut self.ctx.regs;
        (r.rax, r.rdi, r.rsi, r.rdx, r.r10, r.r8, r.r9) = (nr as u64, a[0], a[1], a[2], a[3], a[4], a[5]);
        crate::dispatch(&self.p, &self.th, &mut self.ctx);
        self.ctx.regs.rax as i64
    }
    pub(crate) fn put(&self, at: u64, b: &[u8]) {
        let _ = self.p.mm.copy_to_user(self.mem + at, b);
    }
    pub(crate) fn get(&self, at: u64, len: usize) -> Vec<u8> {
        let mut v = vec![0u8; len];
        let _ = self.p.mm.copy_from_user(&mut v, self.mem + at);
        v
    }
    pub(crate) fn u64_at(&self, at: u64) -> u64 {
        u64::from_le_bytes(self.get(at, 8).try_into().unwrap_or([0; 8]))
    }
    pub(crate) fn u32_at(&self, at: u64) -> u32 {
        u32::from_le_bytes(self.get(at, 4).try_into().unwrap_or([0; 4]))
    }
    pub(crate) fn a(&self, at: u64) -> u64 {
        self.mem + at
    }
    pub(crate) fn alive(&self) -> bool {
        !self.p.has_exited()
    }
}

impl Drop for T {
    fn drop(&mut self) {
        self.p.kill(9);
        let _ = self.p.wait(Some(10 * SEC));
    }
}

pub(crate) fn expect(what: &str, got: i64, want: i64) -> TestResult {
    kassert!(got == want, "{what}: got {got}, want {want}");
    Ok(())
}

// ---------------------------------------------------------------------------------------------------------

ktest!("ab.wave_a.sigaction_mask_altstack", Safe, sigaction_mask_altstack);
fn sigaction_mask_altstack() -> TestResult {
    let mut t = T::new("ab3-sig")?;
    let act = t.a(0);
    let oact = t.a(64);
    // A handler with SA_RESTORER|SA_SIGINFO and an unknown flag bit, every signal in its mask.
    let mut b = Vec::new();
    for v in [0x40_1000u64, 0x0400_0004 | 0x100_0000_0000, 0x40_2000, u64::MAX] {
        b.extend_from_slice(&v.to_le_bytes());
    }
    t.put(0, &b);
    expect("rt_sigaction sigsetsize 4", t.call(nr::RT_SIGACTION, &[10, act, 0, 4]), -EINVAL)?;
    expect("rt_sigaction bad act", t.call(nr::RT_SIGACTION, &[10, 0x10, 0, 8]), -EFAULT)?;
    expect("rt_sigaction SIGKILL", t.call(nr::RT_SIGACTION, &[9, act, 0, 8]), -EINVAL)?;
    expect("rt_sigaction SIGSTOP", t.call(nr::RT_SIGACTION, &[19, act, 0, 8]), -EINVAL)?;
    expect("rt_sigaction query SIGKILL", t.call(nr::RT_SIGACTION, &[9, 0, oact, 8]), 0)?;
    expect("rt_sigaction 0", t.call(nr::RT_SIGACTION, &[0, 0, oact, 8]), -EINVAL)?;
    expect("rt_sigaction 65", t.call(nr::RT_SIGACTION, &[65, 0, oact, 8]), -EINVAL)?;
    expect("rt_sigaction SIGUSR1", t.call(nr::RT_SIGACTION, &[10, act, oact, 8]), 0)?;
    kassert!(t.get(64, 32) == [0u8; 32], "the old action of SIGUSR1 was not SIG_DFL");
    // The old action is set even when copying it out fails afterwards (Linux changes first).
    expect("rt_sigaction oact into the kernel", t.call(nr::RT_SIGACTION, &[12, act, KERNEL_ADDR, 8]), -EFAULT)?;
    expect("rt_sigaction read back", t.call(nr::RT_SIGACTION, &[10, 0, oact, 8]), 0)?;
    kassert!(t.u64_at(64) == 0x40_1000, "handler {:#x}", t.u64_at(64));
    kassert!(t.u64_at(72) == 0x0400_0004, "flags {:#x}: unknown bits must be cleared", t.u64_at(72));
    kassert!(t.u64_at(80) == 0x40_2000, "restorer");
    kassert!(t.u64_at(88) == !((1u64 << 8) | (1 << 18)), "mask {:#x}: SIGKILL/SIGSTOP removed", t.u64_at(88));
    expect("rt_sigaction SIGUSR2 changed before the fault", t.call(nr::RT_SIGACTION, &[12, 0, oact, 8]), 0)?;
    kassert!(t.u64_at(64) == 0x40_1000, "SIGUSR2's action");

    // rt_sigprocmask
    let set = t.a(128);
    let old = t.a(136);
    t.put(128, &u64::MAX.to_le_bytes());
    expect("rt_sigprocmask sigsetsize 16", t.call(nr::RT_SIGPROCMASK, &[0, set, old, 16]), -EINVAL)?;
    expect("rt_sigprocmask bad set", t.call(nr::RT_SIGPROCMASK, &[0, 0x10, old, 8]), -EFAULT)?;
    expect("rt_sigprocmask how 3", t.call(nr::RT_SIGPROCMASK, &[3, set, old, 8]), -EINVAL)?;
    expect("rt_sigprocmask how 3 without a set", t.call(nr::RT_SIGPROCMASK, &[3, 0, old, 8]), 0)?;
    expect("rt_sigprocmask SIG_BLOCK all", t.call(nr::RT_SIGPROCMASK, &[0, set, old, 8]), 0)?;
    kassert!(t.u64_at(136) == 0, "old mask {:#x}", t.u64_at(136));
    expect("rt_sigprocmask read", t.call(nr::RT_SIGPROCMASK, &[0, 0, old, 8]), 0)?;
    kassert!(t.u64_at(136) == !((1u64 << 8) | (1 << 18)), "blocked {:#x}: SIGKILL/SIGSTOP never blocked", t.u64_at(136));
    expect("rt_sigprocmask oset into code", t.call(nr::RT_SIGPROCMASK, &[2, 0, KERNEL_ADDR, 8]), -EFAULT)?;
    // A blocked SIGUSR1 sent to itself stays pending; rt_sigpending shows it.
    let pid = t.p.pid as u64;
    expect("tkill(self, SIGUSR1) blocked", t.call(nr::TKILL, &[pid, 10]), 0)?;
    expect("rt_sigpending size 9", t.call(nr_more::RT_SIGPENDING, &[t.a(144), 9]), -EINVAL)?;
    expect("rt_sigpending", t.call(nr_more::RT_SIGPENDING, &[t.a(144), 8]), 0)?;
    kassert!(t.u64_at(144) == 1 << 9, "pending {:#x}", t.u64_at(144));
    t.put(152, &[0xEE; 8]);
    expect("rt_sigpending size 1", t.call(nr_more::RT_SIGPENDING, &[t.a(152), 1]), 0)?;
    kassert!(t.get(152, 2) == [0, 0xEE], "a 1-byte sigpending wrote {:?}", t.get(152, 2));
    kassert!(t.alive(), "a caught, blocked signal ended the process");

    // sigaltstack
    let ss = t.a(256);
    let oss = t.a(288);
    expect("sigaltstack query", t.call(nr::SIGALTSTACK, &[0, oss]), 0)?;
    kassert!(t.u32_at(288 + 8) == 2, "initial flags {} (want SS_DISABLE)", t.u32_at(296));
    let stack = |sp: u64, flags: u32, size: u64| {
        let mut b = [0u8; 24];
        b[0..8].copy_from_slice(&sp.to_le_bytes());
        b[8..12].copy_from_slice(&flags.to_le_bytes());
        b[16..24].copy_from_slice(&size.to_le_bytes());
        b
    };
    let alt = t.a(0x8000);
    t.put(256, &stack(alt, 0, 2047));
    expect("sigaltstack below MINSIGSTKSZ", t.call(nr::SIGALTSTACK, &[ss, 0]), -ENOMEM)?;
    t.put(256, &stack(alt, 4, 8192));
    expect("sigaltstack bad flags", t.call(nr::SIGALTSTACK, &[ss, 0]), -EINVAL)?;
    expect("sigaltstack bad ss", t.call(nr::SIGALTSTACK, &[0x10, 0]), -EFAULT)?;
    t.put(256, &stack(alt, 1 << 31, 8192));
    expect("sigaltstack set (SS_AUTODISARM)", t.call(nr::SIGALTSTACK, &[ss, oss]), 0)?;
    kassert!(t.u32_at(296) == 2, "old flags {}", t.u32_at(296));
    expect("sigaltstack get", t.call(nr::SIGALTSTACK, &[0, oss]), 0)?;
    kassert!(t.get(288, 24) == stack(alt, 1 << 31, 8192), "the stack read back differs: {:?}", t.get(288, 24));
    // Running on it: with SS_AUTODISARM a thread is never "on" it (`on_sig_stack`, AB-6: Linux's rule), so
    // only the flag shows; without it SS_ONSTACK is reported and changing it is EPERM.
    let saved = t.ctx.regs.rsp;
    t.ctx.regs.rsp = alt + 4096;
    expect("sigaltstack get on the stack", t.call(nr::SIGALTSTACK, &[0, oss]), 0)?;
    kassert!(t.u32_at(296) == 1 << 31, "on-stack flags with SS_AUTODISARM {:#x}", t.u32_at(296));
    t.ctx.regs.rsp = saved;
    t.put(256, &stack(alt, 0, 8192));
    expect("sigaltstack set (no SS_AUTODISARM)", t.call(nr::SIGALTSTACK, &[ss, 0]), 0)?;
    t.ctx.regs.rsp = alt + 4096;
    expect("sigaltstack get on the stack", t.call(nr::SIGALTSTACK, &[0, oss]), 0)?;
    kassert!(t.u32_at(296) == 1, "on-stack flags {:#x}", t.u32_at(296));
    t.ctx.regs.rsp = alt + 8192;
    expect("sigaltstack get at its top (still on it)", t.call(nr::SIGALTSTACK, &[0, oss]), 0)?;
    kassert!(t.u32_at(296) == 1, "flags at the top {:#x}", t.u32_at(296));
    t.ctx.regs.rsp = alt + 4096;
    t.put(256, &stack(0, 2, 0));
    expect("sigaltstack change on the stack", t.call(nr::SIGALTSTACK, &[ss, 0]), -EPERM)?;
    t.ctx.regs.rsp = saved;
    expect("sigaltstack SS_DISABLE", t.call(nr::SIGALTSTACK, &[ss, 0]), 0)?;
    expect("sigaltstack get", t.call(nr::SIGALTSTACK, &[0, oss]), 0)?;
    kassert!(t.get(288, 24) == stack(0, 2, 0), "after SS_DISABLE: {:?}", t.get(288, 24));
    expect("sigaltstack oss into the kernel", t.call(nr::SIGALTSTACK, &[0, KERNEL_ADDR]), -EFAULT)?;
    Ok(())
}

ktest!("ab.wave_a.signals_end_or_wait", Safe, signals_end_or_wait);
fn signals_end_or_wait() -> TestResult {
    // kill(self, SIGTERM): the default action ends it at once.
    {
        let mut t = T::new("ab3-term")?;
        let pid = t.p.pid as u64;
        expect("kill(self, 0)", t.call(nr::KILL, &[pid, 0]), 0)?;
        expect("kill(self, 65)", t.call(nr::KILL, &[pid, 65]), -EINVAL)?;
        expect("kill(self, -1)", t.call(nr::KILL, &[pid, u64::MAX]), -EINVAL)?;
        expect("kill(no such pid)", t.call(nr::KILL, &[4_000_000, 15]), -ESRCH)?;
        expect("kill(no such pid, bad signal): ESRCH first", t.call(nr::KILL, &[4_000_000, 99]), -ESRCH)?;
        expect("kill(INT_MIN)", t.call(nr::KILL, &[i32::MIN as u32 as u64, 0]), -ESRCH)?;
        expect("kill(own group, 0)", t.call(nr::KILL, &[0, 0]), 0)?;
        expect("tkill(0)", t.call(nr::TKILL, &[0, 15]), -EINVAL)?;
        expect("tgkill(0, pid)", t.call(nr::TGKILL, &[0, pid, 0]), -EINVAL)?;
        expect("tgkill(wrong tgid)", t.call(nr::TGKILL, &[pid + 1_000_000, pid, 0]), -ESRCH)?;
        expect("tgkill(pid, pid, 0)", t.call(nr::TGKILL, &[pid, pid, 0]), 0)?;
        // Ignored ones: SIGCHLD by default, SIGTERM with SIG_IGN.
        expect("kill(self, SIGCHLD)", t.call(nr::KILL, &[pid, 17]), 0)?;
        expect("kill(self, SIGWINCH)", t.call(nr::KILL, &[pid, 28]), 0)?;
        kassert!(t.alive(), "a default-ignore signal ended the process");
        t.put(0, &[1u8, 0, 0, 0, 0, 0, 0, 0]); // SIG_IGN
        expect("SIG_IGN for SIGTERM", t.call(nr::RT_SIGACTION, &[15, t.a(0), 0, 8]), 0)?;
        expect("kill(self, SIGTERM) ignored", t.call(nr::KILL, &[pid, 15]), 0)?;
        kassert!(t.alive(), "an ignored SIGTERM ended the process");
        t.put(0, &[0u8; 32]); // SIG_DFL again
        expect("SIG_DFL for SIGTERM", t.call(nr::RT_SIGACTION, &[15, t.a(0), 0, 8]), 0)?;
        expect("kill(self, SIGTERM)", t.call(nr::KILL, &[pid, 15]), 0)?;
        kassert!(t.p.status() == Some(ExitStatus::Signaled(15)), "status {:?}, want signal 15", t.p.status());
    }
    // musl's raise()/abort(): block all, tkill(self, SIGABRT), restore the mask -> ends with SIGABRT then.
    {
        let mut t = T::new("ab3-abrt")?;
        let pid = t.p.pid as u64;
        t.put(0, &u64::MAX.to_le_bytes());
        expect("block all", t.call(nr::RT_SIGPROCMASK, &[0, t.a(0), t.a(8), 8]), 0)?;
        expect("tkill(self, SIGABRT)", t.call(nr::TKILL, &[pid, 6]), 0)?;
        kassert!(t.alive(), "a blocked SIGABRT ended the process early");
        expect("restore the mask", t.call(nr::RT_SIGPROCMASK, &[2, t.a(8), 0, 8]), 0)?;
        kassert!(t.p.status() == Some(ExitStatus::Signaled(6)), "status {:?}, want signal 6", t.p.status());
    }
    // A handler without SA_RESTORER (AB-6): x86-64 has no return path of its own, so the frame is refused
    // (`x64_setup_rt_frame` EFAULT) and the program ends with SIGSEGV (`force_sigsegv`), as on Linux. The
    // delivery with a restorer is ab.signal.*'s.
    {
        let mut t = T::new("ab3-hand")?;
        let pid = t.p.pid as u64;
        t.put(0, &0x40_1000u64.to_le_bytes());
        expect("handler for SIGUSR1", t.call(nr::RT_SIGACTION, &[10, t.a(0), 0, 8]), 0)?;
        let seq = klog::next_seq();
        expect("kill(self, SIGUSR1)", t.call(nr::KILL, &[pid, 10]), 0)?;
        kassert!(t.p.status() == Some(ExitStatus::Signaled(11)), "status {:?}, want SIGSEGV", t.p.status());
        let f = klog::Filter { level: None, subsystem: Some("proc"), name: Some("signal.frame_failed") };
        kassert!(!klog::read(seq, 100, &f).records.is_empty(), "no proc signal.frame_failed event");
    }
    // One process ends another: B3 asks (G0 policy allows), SIGTERM's default action ends the target.
    {
        let mut a = T::new("ab3-killer")?;
        let b = T::new("ab3-victim")?;
        expect("kill(other, 0)", a.call(nr::KILL, &[b.p.pid as u64, 0]), 0)?;
        expect("kill(other, SIGTERM)", a.call(nr::KILL, &[b.p.pid as u64, 15]), 0)?;
        kassert!(b.p.status() == Some(ExitStatus::Signaled(15)), "victim status {:?}", b.p.status());
        kassert!(a.alive(), "the sender ended");
        // A zombie or ended process still exists for kill until it is collected (here it left the table).
        kassert!(b.p.wait(Some(10 * SEC)).is_some(), "the victim's thread did not end");
    }
    Ok(())
}

ktest!("ab.wave_a.clocks_and_sleeps", Safe, clocks_and_sleeps);
fn clocks_and_sleeps() -> TestResult {
    let mut t = T::new("ab3-clock")?;
    let ts = t.a(0);
    for id in [0u64, 1, 2, 3, 4, 5, 6, 7, 8, 9, 11, (-6i64) as u64, (-2i64) as u64] {
        expect(&format!("clock_gettime({})", id as i64), t.call(nr::CLOCK_GETTIME, &[id, ts]), 0)?;
        kassert!(t.u64_at(8) < SEC, "clock {}: nsec {}", id as i64, t.u64_at(8));
        expect(&format!("clock_getres({})", id as i64), t.call(nr::CLOCK_GETRES, &[id, ts]), 0)?;
        kassert!(t.u64_at(0) == 0 && t.u64_at(8) == 1, "clock {} resolution", id as i64);
    }
    for id in [10u64, 12, 1000, (-1i64) as u64, (-5i64) as u64] {
        expect(&format!("clock_gettime({})", id as i64), t.call(nr::CLOCK_GETTIME, &[id, ts]), -EINVAL)?;
        expect(&format!("clock_getres({})", id as i64), t.call(nr::CLOCK_GETRES, &[id, ts]), -EINVAL)?;
    }
    expect("clock_getres(NULL)", t.call(nr::CLOCK_GETRES, &[1, 0]), 0)?;
    expect("clock_gettime into the kernel", t.call(nr::CLOCK_GETTIME, &[1, KERNEL_ADDR]), -EFAULT)?;
    // CPU clock of another process: exists -> 0; none -> EINVAL.
    let other = T::new("ab3-clock2")?;
    let cpu_of = |pid: u32| ((!(pid as i32)) << 3 | 2) as i64 as u64;
    expect("clock_gettime(cpu of a live process)", t.call(nr::CLOCK_GETTIME, &[cpu_of(other.p.pid), ts]), 0)?;
    expect("clock_gettime(cpu of no process)", t.call(nr::CLOCK_GETTIME, &[cpu_of(4_000_000), ts]), -EINVAL)?;
    // Monotonic never goes back.
    let mut last = 0u64;
    for _ in 0..200 {
        t.call(nr::CLOCK_GETTIME, &[1, ts]);
        let now = t.u64_at(0) * SEC + t.u64_at(8);
        kassert!(now >= last, "CLOCK_MONOTONIC went back: {last} -> {now}");
        last = now;
    }
    // gettimeofday and time agree with CLOCK_REALTIME.
    t.call(nr::CLOCK_GETTIME, &[0, ts]);
    let rt = t.u64_at(0);
    expect("gettimeofday", t.call(nr::GETTIMEOFDAY, &[t.a(32), t.a(48)]), 0)?;
    kassert!(t.u64_at(32).abs_diff(rt) <= 1 && t.u64_at(40) < 1_000_000, "timeval {} {} vs {rt}", t.u64_at(32), t.u64_at(40));
    kassert!(t.u64_at(48) == 0, "timezone not zero");
    expect("gettimeofday(NULL, NULL)", t.call(nr::GETTIMEOFDAY, &[0, 0]), 0)?;
    expect("gettimeofday into the kernel", t.call(nr::GETTIMEOFDAY, &[KERNEL_ADDR, 0]), -EFAULT)?;
    let s = t.call(nr_more::TIME, &[t.a(64)]);
    kassert!((s as u64).abs_diff(rt) <= 1 && t.u64_at(64) == s as u64, "time() {s}");

    // nanosleep: validity, then a real 20 ms sleep (functional: at least 20 ms).
    let put_ts = |t: &T, at: u64, sec: i64, nsec: i64| {
        let mut b = [0u8; 16];
        b[0..8].copy_from_slice(&sec.to_le_bytes());
        b[8..16].copy_from_slice(&nsec.to_le_bytes());
        t.put(at, &b);
    };
    put_ts(&t, 96, 0, 1_000_000_000);
    expect("nanosleep nsec 1e9", t.call(nr::NANOSLEEP, &[t.a(96), 0]), -EINVAL)?;
    put_ts(&t, 96, -1, 0);
    expect("nanosleep sec -1", t.call(nr::NANOSLEEP, &[t.a(96), 0]), -EINVAL)?;
    expect("nanosleep bad pointer", t.call(nr::NANOSLEEP, &[0x10, 0]), -EFAULT)?;
    put_ts(&t, 96, 0, 20 * MS as i64);
    let t0 = time::now_ns();
    expect("nanosleep 20 ms", t.call(nr::NANOSLEEP, &[t.a(96), t.a(112)]), 0)?;
    kassert!(time::now_ns() - t0 >= 20 * MS, "nanosleep returned after {} ns", time::now_ns() - t0);
    // clock_nanosleep: clocks without nsleep, bad ids, absolute times in the past.
    for id in [3u64, 4, 5, 6] {
        expect(&format!("clock_nanosleep({id})"), t.call(nr::CLOCK_NANOSLEEP, &[id, 0, t.a(96), 0]), -EOPNOTSUPP)?;
    }
    expect("clock_nanosleep(10)", t.call(nr::CLOCK_NANOSLEEP, &[10, 0, t.a(96), 0]), -EINVAL)?;
    // AB-13: the alarm clocks as Linux with an RTC: a user's sleep is EPERM (CAP_WAKE_ALARM), bad flags EINVAL.
    expect("clock_nanosleep(BOOTTIME_ALARM)", t.call(nr::CLOCK_NANOSLEEP, &[9, 0, t.a(96), 0]), -EPERM)?;
    expect("clock_nanosleep(BOOTTIME_ALARM, flags 2)", t.call(nr::CLOCK_NANOSLEEP, &[9, 2, t.a(96), 0]), -EINVAL)?;
    expect("clock_gettime(REALTIME_ALARM)", t.call(nr::CLOCK_GETTIME, &[8, t.a(128)]), 0)?;
    expect("clock_gettime(BOOTTIME_ALARM)", t.call(nr::CLOCK_GETTIME, &[9, t.a(128)]), 0)?;
    expect("clock_getres(REALTIME_ALARM)", t.call(nr::CLOCK_GETRES, &[8, t.a(128)]), 0)?;
    kassert!(t.u64_at(128) == 0 && t.u64_at(136) == 1, "alarm resolution");
    expect("clock_nanosleep bad pointer", t.call(nr::CLOCK_NANOSLEEP, &[1, 0, 0x10, 0]), -EFAULT)?;
    put_ts(&t, 96, 1, 0);
    let t0 = time::now_ns();
    expect("clock_nanosleep ABSTIME at 1 s after boot", t.call(nr::CLOCK_NANOSLEEP, &[1, 1, t.a(96), 0]), 0)?;
    expect("clock_nanosleep REALTIME ABSTIME in the past", t.call(nr::CLOCK_NANOSLEEP, &[0, 1, t.a(96), 0]), 0)?;
    kassert!(time::now_ns() - t0 < 5 * SEC, "absolute sleeps into the past waited");
    put_ts(&t, 96, 0, 5 * MS as i64);
    expect("clock_nanosleep 5 ms (unknown flag bits ignored)", t.call(nr::CLOCK_NANOSLEEP, &[1, 0x10, t.a(96), 0]), 0)?;

    // A kill ends a long sleep at once with EINTR and the time left.
    let p = t.p.clone();
    let killer = sched::spawn("ab3-killer", sched::Class::Normal, move || {
        sched::sleep_ns(100 * MS);
        p.kill(9);
    })
    .map_err(|e| format!("spawn: {e:?}"))?;
    put_ts(&t, 96, 10, 0);
    let t0 = time::now_ns();
    expect("nanosleep 10 s killed", t.call(nr::NANOSLEEP, &[t.a(96), t.a(112)]), -EINTR)?;
    kassert!(time::now_ns() - t0 < 5 * SEC, "the killed sleep went on for {} ms", (time::now_ns() - t0) / MS);
    let left = t.u64_at(112) * SEC + t.u64_at(120);
    kassert!(left > 5 * SEC && left < 10 * SEC, "time left {left} ns");
    kassert!(killer.join_timeout(10 * SEC), "killer thread");
    Ok(())
}

ktest!("ab.wave_a.ids_limits_random", Safe, ids_limits_random);
fn ids_limits_random() -> TestResult {
    let mut t = T::new("ab3-ids")?;
    let pid = t.p.pid as i64;
    expect("getpid", t.call(nr::GETPID, &[]), pid)?;
    expect("gettid", t.call(nr::GETTID, &[]), pid)?;
    for n in [nr::GETUID, nr::GETEUID, nr::GETGID, nr::GETEGID] {
        expect("uid/gid", t.call(n, &[]), 1000)?;
    }
    expect("getresuid", t.call(nr_more::GETRESUID, &[t.a(0), t.a(4), t.a(8)]), 0)?;
    kassert!(t.u32_at(0) == 1000 && t.u32_at(4) == 1000 && t.u32_at(8) == 1000, "getresuid values");
    expect("getresgid bad third", t.call(nr_more::GETRESGID, &[t.a(0), t.a(4), 0x10]), -EFAULT)?;
    expect("getgroups(0)", t.call(nr_more::GETGROUPS, &[0, 0]), 0)?;
    expect("getgroups(-1)", t.call(nr_more::GETGROUPS, &[u64::MAX, 0]), -EINVAL)?;
    // AB-13: an ordinary user may not set groups, whatever the size (EPERM comes first).
    expect("setgroups as a user", t.call(nr_more::SETGROUPS, &[0, 0]), -EPERM)?;
    expect("setgroups huge as a user", t.call(nr_more::SETGROUPS, &[1 << 20, 0]), -EPERM)?;
    expect("uname", t.call(nr::UNAME, &[t.a(0)]), 0)?;
    let u = t.get(0, 390);
    kassert!(&u[0..6] == b"Linux\0" && &u[260..267] == b"x86_64\0" && u[130].is_ascii_digit(), "utsname fields");
    expect("uname into the kernel", t.call(nr::UNAME, &[KERNEL_ADDR]), -EFAULT)?;
    expect("set_tid_address", t.call(nr::SET_TID_ADDRESS, &[t.a(512)]), pid)?;
    kassert!(t.th.clear_child_tid.load(core::sync::atomic::Ordering::Acquire) == t.a(512), "clear_child_tid not kept");
    expect("set_robust_list len 23", t.call(nr::SET_ROBUST_LIST, &[t.a(600), 23]), -EINVAL)?;
    expect("set_robust_list", t.call(nr::SET_ROBUST_LIST, &[t.a(600), 24]), 0)?;
    expect("get_robust_list(0)", t.call(nr::GET_ROBUST_LIST, &[0, t.a(700), t.a(708)]), 0)?;
    kassert!(t.u64_at(700) == t.a(600) && t.u64_at(708) == 24, "robust list head/len");
    expect("get_robust_list(no pid)", t.call(nr::GET_ROBUST_LIST, &[4_000_000, t.a(700), t.a(708)]), -ESRCH)?;
    expect("get_robust_list bad len ptr", t.call(nr::GET_ROBUST_LIST, &[0, t.a(700), 0x10]), -EFAULT)?;

    // Limits: Linux's defaults, its rules, and RLIMIT_NOFILE reaching the fd table.
    expect("prlimit64 NOFILE read", t.call(nr::PRLIMIT64, &[0, 7, 0, t.a(800)]), 0)?;
    kassert!(t.u64_at(800) == 1024 && t.u64_at(808) == 4096, "NOFILE {} {}", t.u64_at(800), t.u64_at(808));
    expect("getrlimit STACK", t.call(nr::GETRLIMIT, &[3, t.a(800)]), 0)?;
    kassert!(t.u64_at(800) == 8 << 20 && t.u64_at(808) == u64::MAX, "STACK limits");
    expect("getrlimit 16", t.call(nr::GETRLIMIT, &[16, t.a(800)]), -EINVAL)?;
    let lim = |t: &T, at: u64, cur: u64, max: u64| {
        t.put(at, &cur.to_le_bytes());
        t.put(at + 8, &max.to_le_bytes());
    };
    lim(&t, 816, 300, 200);
    expect("prlimit64 cur > max", t.call(nr::PRLIMIT64, &[0, 7, t.a(816), 0]), -EINVAL)?;
    lim(&t, 816, 300, 8192);
    expect("prlimit64 raise the hard limit", t.call(nr::PRLIMIT64, &[0, 7, t.a(816), 0]), -EPERM)?;
    lim(&t, 816, 300, (1 << 20) + 1);
    expect("prlimit64 above nr_open", t.call(nr::PRLIMIT64, &[0, 7, t.a(816), 0]), -EPERM)?;
    expect("prlimit64 bad new", t.call(nr::PRLIMIT64, &[0, 7, 0x10, 0]), -EFAULT)?;
    lim(&t, 816, 300, 2048);
    expect("prlimit64 NOFILE 300/2048", t.call(nr::PRLIMIT64, &[0, 7, t.a(816), t.a(800)]), 0)?;
    kassert!(t.u64_at(800) == 1024 && t.u64_at(808) == 4096, "old value");
    kassert!(t.p.fds.limit() == 300, "fd table limit {}", t.p.fds.limit());
    lim(&t, 816, 100, 2048);
    expect("setrlimit NOFILE 100", t.call(nr::SETRLIMIT, &[7, t.a(816)]), 0)?;
    kassert!(t.p.fds.limit() == 100, "fd table limit {}", t.p.fds.limit());
    let other = T::new("ab3-ids2")?;
    expect("prlimit64 of another process", t.call(nr::PRLIMIT64, &[other.p.pid as u64, 7, 0, t.a(800)]), 0)?;
    kassert!(t.u64_at(800) == 1024, "the other process has its own table");
    expect("prlimit64 of no process", t.call(nr::PRLIMIT64, &[4_000_000, 7, 0, t.a(800)]), -ESRCH)?;

    // getrandom
    expect("getrandom 0 bytes", t.call(nr::GETRANDOM, &[t.a(0), 0, 0]), 0)?;
    expect("getrandom flags 8", t.call(nr::GETRANDOM, &[t.a(0), 16, 8]), -EINVAL)?;
    expect("getrandom INSECURE|RANDOM", t.call(nr::GETRANDOM, &[t.a(0), 16, 6]), -EINVAL)?;
    expect("getrandom into the kernel", t.call(nr::GETRANDOM, &[KERNEL_ADDR, 16, 0]), -EFAULT)?;
    expect("getrandom unmapped", t.call(nr::GETRANDOM, &[0x10, 16, 0]), -EFAULT)?;
    expect("getrandom 1 MiB", t.call(nr::GETRANDOM, &[t.a(0), SCRATCH, 1]), SCRATCH as i64)?;
    let b = t.get(SCRATCH - 4096, 4096);
    kassert!(b.iter().filter(|&&x| x == 0).count() < 200, "the last page looks empty");
    // A buffer running into the unmapped page after the scratch memory: exactly the bytes before it.
    let got = t.call(nr::GETRANDOM, &[t.a(SCRATCH - 1000), 5000, 0]);
    kassert!(got == 1000, "partial getrandom gave {got}, want 1000");

    expect("sched_yield", t.call(nr::SCHED_YIELD, &[]), 0)?;
    let cpus = hal::percpu::cpu_count().max(1) as u64;
    if cpus > 8 {
        expect("sched_getaffinity len 1", t.call(nr::SCHED_GETAFFINITY, &[0, 1, t.a(0)]), -EINVAL)?;
    }
    expect("sched_getaffinity len 12", t.call(nr::SCHED_GETAFFINITY, &[0, 12, t.a(0)]), -EINVAL)?;
    let size = cpus.div_ceil(64) * 8;
    expect("sched_getaffinity len 128", t.call(nr::SCHED_GETAFFINITY, &[0, 128, t.a(0)]), size as i64)?;
    let ones: u32 = t.get(0, size as usize).iter().map(|b| b.count_ones()).sum();
    kassert!(ones as usize == sched::online_cpus(), "{ones} CPUs in the mask, {} online", sched::online_cpus());
    expect("sched_getaffinity no pid", t.call(nr::SCHED_GETAFFINITY, &[4_000_000, 128, t.a(0)]), -ESRCH)?;
    Ok(())
}

fn pollfd(t: &T, at: u64, fd: i32, events: u16) {
    let mut b = [0u8; 8];
    b[0..4].copy_from_slice(&fd.to_le_bytes());
    b[4..6].copy_from_slice(&events.to_le_bytes());
    b[6..8].copy_from_slice(&0xAAAAu16.to_le_bytes());
    t.put(at, &b);
}

fn rev(t: &T, at: u64) -> u16 {
    u16::from_le_bytes([t.get(at + 6, 2)[0], t.get(at + 6, 2)[1]])
}

ktest!("ab.wave_a.poll_ppoll", Safe, poll_ppoll);
fn poll_ppoll() -> TestResult {
    let input = proc::stdio::Input::new(b"abc".to_vec());
    let out = proc::stdio::Capture::new(4096);
    let mut t = T::with("ab3-poll", |o| {
        o.stdin = Some(input.clone() as Arc<dyn vfs::File>);
        o.stdout = Some(out.clone() as Arc<dyn vfs::File>);
    })?;
    let null = t.p.ns.open("/dev/null").map_err(|e| format!("/dev/null: {e}"))?;
    let nfd = t.p.fds.install(vfs::OpenFile::new(null, vfs::OpenFlags { read: true, write: true, ..Default::default() })).map_err(|e| format!("install: {e}"))?;
    let all = 0xffffu16;
    pollfd(&t, 0, 0, 1); // stdin, POLLIN: data left and the writer gone
    pollfd(&t, 8, 1, 4); // stdout, POLLOUT
    pollfd(&t, 16, nfd as i32, all); // /dev/null, everything
    pollfd(&t, 24, -1, all); // skipped
    pollfd(&t, 32, 77, 1); // not open
    pollfd(&t, 40, 1, 0); // asked for nothing
    expect("poll 6 entries", t.call(nr::POLL, &[t.a(0), 6, 0]), 4)?;
    let want = [0x11u16, 0x4, 0x145, 0, 0x20, 0];
    for (i, w) in want.iter().enumerate() {
        kassert!(rev(&t, i as u64 * 8) == *w, "entry {i}: revents {:#x}, want {w:#x}", rev(&t, i as u64 * 8));
    }
    kassert!(t.get(4, 2) == [1, 0], "events must not be touched");
    // Reading stdin to its end leaves only POLLHUP.
    let _ = input.read_at(0, &mut [0u8; 8]);
    expect("poll stdin at EOF", t.call(nr::POLL, &[t.a(0), 1, 0]), 1)?;
    kassert!(rev(&t, 0) == 0x10, "EOF pipe revents {:#x}", rev(&t, 0));
    // Nothing ready: timeout 0 returns at once, a timeout waits that long (functional: at least).
    pollfd(&t, 0, -1, all);
    expect("poll nothing, timeout 0", t.call(nr::POLL, &[t.a(0), 1, 0]), 0)?;
    let t0 = time::now_ns();
    expect("poll nothing, 50 ms", t.call(nr::POLL, &[t.a(0), 1, 50]), 0)?;
    kassert!(time::now_ns() - t0 >= 50 * MS, "poll waited {} ns", time::now_ns() - t0);
    expect("poll(NULL, 0, 0)", t.call(nr::POLL, &[0, 0, 0]), 0)?;
    expect("poll bad array", t.call(nr::POLL, &[0x10, 1, 0]), -EFAULT)?;
    expect("poll nfds above RLIMIT_NOFILE", t.call(nr::POLL, &[t.a(0), 1025, 0]), -EINVAL)?;
    // ppoll: timespec rules, sigmask size, the time left written back.
    let put_ts = |t: &T, at: u64, sec: i64, nsec: i64| {
        let mut b = [0u8; 16];
        b[0..8].copy_from_slice(&sec.to_le_bytes());
        b[8..16].copy_from_slice(&nsec.to_le_bytes());
        t.put(at, &b);
    };
    put_ts(&t, 64, 0, 1_000_000_000);
    expect("ppoll nsec 1e9", t.call(nr::PPOLL, &[t.a(0), 1, t.a(64), 0, 8]), -EINVAL)?;
    expect("ppoll bad timespec", t.call(nr::PPOLL, &[t.a(0), 1, 0x10, 0, 8]), -EFAULT)?;
    put_ts(&t, 64, 0, 30 * MS as i64);
    t.put(96, &(1u64 << 9).to_le_bytes());
    expect("ppoll sigsetsize 4", t.call(nr::PPOLL, &[t.a(0), 1, t.a(64), t.a(96), 4]), -EINVAL)?;
    expect("ppoll 30 ms with a mask", t.call(nr::PPOLL, &[t.a(0), 1, t.a(64), t.a(96), 8]), 0)?;
    let left = t.u64_at(64) * SEC + t.u64_at(72);
    kassert!(left < 30 * MS, "time left {left} ns");
    kassert!(t.p.signals.blocked() == 0, "ppoll did not restore the mask");
    pollfd(&t, 0, 1, 4);
    put_ts(&t, 64, 5, 0);
    expect("ppoll ready", t.call(nr::PPOLL, &[t.a(0), 1, t.a(64), 0, 0]), 1)?;
    kassert!(t.u64_at(64) <= 5 && t.u64_at(64) >= 4, "time left after a ready ppoll: {} s", t.u64_at(64));
    expect("ppoll(NULL timeout) ready", t.call(nr::PPOLL, &[t.a(0), 1, 0, 0, 0]), 1)?;
    Ok(())
}

ktest!("ab.wave_a.ppoll_mask_unblocks_fatal", Safe, ppoll_mask_unblocks_fatal);
fn ppoll_mask_unblocks_fatal() -> TestResult {
    // pselect-style wait: SIGTERM blocked and pending, then ppoll with an empty temporary mask and no timeout.
    // Linux ends the program at once (the signal is delivered under the temporary mask); it must not wait.
    let mut t = T::new("ab3-ppoll-sig")?;
    let pid = t.p.pid as u64;
    t.put(0, &(1u64 << 14).to_le_bytes());
    expect("block SIGTERM", t.call(nr::RT_SIGPROCMASK, &[0, t.a(0), 0, 8]), 0)?;
    expect("tkill(self, SIGTERM) blocked", t.call(nr::TKILL, &[pid, 15]), 0)?;
    kassert!(t.alive(), "a blocked SIGTERM ended the process early");
    pollfd(&t, 16, -1, 1); // nothing to wait for
    t.put(8, &0u64.to_le_bytes()); // the empty temporary mask
    // A watchdog so a regression fails instead of hanging the test.
    let p = t.p.clone();
    let guard = sched::spawn("ab3-ppoll-guard", sched::Class::Normal, move || {
        if p.wait(Some(5 * SEC)).is_none() {
            p.kill(9);
        }
    })
    .map_err(|e| format!("spawn: {e:?}"))?;
    let t0 = time::now_ns();
    expect("ppoll with SIGTERM unblocked", t.call(nr::PPOLL, &[t.a(16), 1, 0, t.a(8), 8]), -EINTR)?;
    kassert!(time::now_ns() - t0 < 3 * SEC, "ppoll waited {} ms before acting on SIGTERM", (time::now_ns() - t0) / MS);
    kassert!(t.p.status() == Some(ExitStatus::Signaled(15)), "status {:?}, want signal 15", t.p.status());
    kassert!(guard.join_timeout(10 * SEC), "guard thread");
    Ok(())
}

ktest!("ab.wave_a.ioctl_console", Safe, ioctl_console);
fn ioctl_console() -> TestResult {
    let out = proc::stdio::Capture::new(4096);
    let mut t = T::with("ab3-ioctl", |o| o.stdout = Some(out.clone() as Arc<dyn vfs::File>))?;
    // fd 0 and 2 are the console (SpawnOptions' default), fd 1 a pipe.
    t.put(0, &[0xEEu8; 64]);
    expect("TIOCGWINSZ on the console", t.call(nr::IOCTL, &[2, 0x5413, t.a(0)]), 0)?;
    kassert!(t.get(8, 8) == [0xEE; 8], "TIOCGWINSZ wrote past its 8 bytes");
    let rows = u16::from_le_bytes([t.get(0, 2)[0], t.get(0, 2)[1]]);
    kassert!(rows > 0, "console rows {rows}");
    t.put(0, &[0xEEu8; 64]);
    expect("TCGETS on the console", t.call(nr::IOCTL, &[0, 0x5401, t.a(0)]), 0)?;
    kassert!(t.get(36, 24) == [0xEE; 24], "TCGETS wrote past the kernel's 36-byte termios");
    expect("TCGETS into the kernel", t.call(nr::IOCTL, &[0, 0x5401, KERNEL_ADDR]), -EFAULT)?;
    expect("TCGETS on a pipe", t.call(nr::IOCTL, &[1, 0x5401, t.a(0)]), -ENOTTY)?;
    expect("TIOCGWINSZ on a pipe", t.call(nr::IOCTL, &[1, 0x5413, t.a(0)]), -ENOTTY)?;
    expect("ioctl on a closed fd", t.call(nr::IOCTL, &[55, 0x5401, t.a(0)]), -EBADF)?;
    expect("ioctl fd -1", t.call(nr::IOCTL, &[u64::MAX, 0x5401, t.a(0)]), -EBADF)?;
    let null = t.p.ns.open("/dev/null").map_err(|e| format!("/dev/null: {e}"))?;
    let nfd = t.p.fds.install(vfs::OpenFile::new(null, vfs::OpenFlags { read: true, ..Default::default() })).map_err(|e| format!("{e}"))? as u64;
    expect("TCGETS on /dev/null", t.call(nr::IOCTL, &[nfd, 0x5401, t.a(0)]), -ENOTTY)?;
    // The generic ioctls of every file.
    expect("FIOCLEX", t.call(nr::IOCTL, &[nfd, 0x5451, 0]), 0)?;
    kassert!(t.p.fds.cloexec(nfd as usize) == Ok(true), "FIOCLEX did not set close-on-exec");
    expect("FIONCLEX", t.call(nr::IOCTL, &[nfd, 0x5450, 0]), 0)?;
    kassert!(t.p.fds.cloexec(nfd as usize) == Ok(false), "FIONCLEX did not clear it");
    t.put(0, &1u32.to_le_bytes());
    expect("FIONBIO on", t.call(nr::IOCTL, &[nfd, 0x5421, t.a(0)]), 0)?;
    let f = t.p.fds.get(nfd as usize).map_err(|e| format!("{e}"))?;
    kassert!(f.nonblocking(), "FIONBIO did not set O_NONBLOCK");
    expect("FIONBIO bad pointer", t.call(nr::IOCTL, &[nfd, 0x5421, 0x10]), -EFAULT)?;
    // FIONREAD on a regular file: the bytes after the position.
    let path = format!("/tmp/ab3-ioctl-{}", t.p.pid);
    let _ = t.p.ns.remove(&path);
    t.p.ns.create_file(&path, 0o600, vec![7u8; 1234]).map_err(|e| format!("create {path}: {e}"))?;
    let file = t.p.ns.open(&path).map_err(|e| format!("open {path}: {e}"))?;
    let rfd = t.p.fds.install(vfs::OpenFile::new(file, vfs::OpenFlags { read: true, ..Default::default() })).map_err(|e| format!("{e}"))? as u64;
    expect("FIONREAD on a file", t.call(nr::IOCTL, &[rfd, 0x541B, t.a(0)]), 0)?;
    kassert!(t.u32_at(0) == 1234, "FIONREAD {}", t.u32_at(0));
    expect("TCGETS on a file", t.call(nr::IOCTL, &[rfd, 0x5401, t.a(0)]), -ENOTTY)?;
    let _ = t.p.ns.remove(&path);
    Ok(())
}

/// The ENOSYS lines of `stats()` as (number, count).
fn enosys_counts() -> Vec<(usize, u64)> {
    crate::stats().into_iter().filter(|s| s.enosys).map(|s| (s.nr, s.calls)).collect()
}

ktest!("ab.wave_a.rust_std_hello", Safe, rust_std_hello);
fn rust_std_hello() -> TestResult {
    // Rust std's start-up (poll on fds 0-2, sigaction for SIGPIPE/SIGSEGV/SIGBUS, sigaltstack with a guarded
    // mmap, ...) and musl's: both programs run to their end with no syscall answered ENOSYS.
    for (path, want) in [("/bin/hello-rs", "hello from Rust std on BitsOS V2"), ("/bin/hello", "hello from musl C on BitsOS V2")] {
        let before = enosys_counts();
        let out = proc::stdio::Capture::new(1 << 16);
        let err = proc::stdio::Capture::new(1 << 16);
        let argv = [path];
        let mut o = proc::SpawnOptions::new(&argv, &["PATH=/bin", "HOME=/"], policy::ProgramId(1));
        o.stdout = Some(out.clone() as Arc<dyn vfs::File>);
        o.stderr = Some(err.clone() as Arc<dyn vfs::File>);
        o.stdin = Some(proc::stdio::Input::new(Vec::new()) as Arc<dyn vfs::File>);
        let p = proc::spawn_with(path, &o).map_err(|e| format!("{path}: {e} (xtask builds it into every image)"))?;
        let code = p.wait(Some(30 * SEC)).ok_or_else(|| format!("{path} did not end within 30 s"))?;
        let text = String::from_utf8_lossy(&out.contents()).into_owned();
        let etext = String::from_utf8_lossy(&err.contents()).into_owned();
        kassert!(code == 0, "{path} ended with {code} ({:?}), stderr: {etext}", p.status());
        kassert!(text.starts_with(want), "{path} printed {text:?}");
        kassert!(etext.is_empty(), "{path} wrote to stderr: {etext}");
        let after = enosys_counts();
        let new: Vec<_> = after.iter().filter(|a| !before.contains(a)).collect();
        kassert!(new.is_empty(), "{path} got ENOSYS for {new:?}");
    }
    Ok(())
}
