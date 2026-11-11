//! SX-1 ktests (`ab.sx1.*`): seccomp(2) and prctl's seccomp options through the real dispatcher on held test
//! processes (the test thread plays the program, as AB-5's tests do), the `abi-seccomp` program in its modes
//! against CachyOS's transcripts (`user/abi-tests/expected/sx1-*.txt`), and the cost of the check on the
//! syscall path (numbers only: timing is judged on the Mini-PC).
//!
//! All `Safe` and rerunnable: every test makes its own processes and ends them; every wait has a deadline.

use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use hal::entry::UserContext;
use ktest::{TestResult, kassert, ktest};
use proc::{ExitStatus, Process};
use seccomp::consts::*;
use uapi::ab::nr_more;
use uapi::ab::prctl::*;
use uapi::errno::*;
use uapi::nr;

const SEC: u64 = 1_000_000_000;
const EXIT0: [u8; 9] = [0xb8, 0xe7, 0, 0, 0, 0x31, 0xff, 0x0f, 0x05];
const SCRATCH: u64 = 1 << 16;
const KERNEL_ADDR: u64 = 0xffff_8880_0000_0000;
const GETPID: u64 = 39;
const GETUID: u64 = 102;
const GETPPID: u64 = 110;

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
    fn call(&mut self, n: u64, args: &[u64]) -> i64 {
        let mut a = [0u64; 6];
        a[..args.len()].copy_from_slice(args);
        let r = &mut self.ctx.regs;
        (r.rax, r.rdi, r.rsi, r.rdx, r.r10, r.r8, r.r9) = (n, a[0], a[1], a[2], a[3], a[4], a[5]);
        crate::dispatch(&self.p, &self.th, &mut self.ctx);
        self.ctx.regs.rax as i64
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
    /// A sock_fprog at 0x100 pointing at the program at 0x1000.
    fn put_filter(&self, insns: &[(u16, u8, u8, u32)]) -> u64 {
        let mut b = Vec::new();
        for &(code, jt, jf, k) in insns {
            b.extend_from_slice(&code.to_le_bytes());
            b.push(jt);
            b.push(jf);
            b.extend_from_slice(&k.to_le_bytes());
        }
        self.put(0x1000, &b);
        let mut fp = [0u8; 16];
        fp[0..2].copy_from_slice(&(insns.len() as u16).to_le_bytes());
        fp[8..16].copy_from_slice(&self.a(0x1000).to_le_bytes());
        self.put(0x100, &fp);
        self.a(0x100)
    }
    fn seccomp(&mut self, op: u32, flags: u32, uargs: u64) -> i64 {
        self.call(NR_SECCOMP as u64, &[op as u64, flags as u64, uargs])
    }
    fn install(&mut self, flags: u32, insns: &[(u16, u8, u8, u32)]) -> i64 {
        let fp = self.put_filter(insns);
        self.seccomp(SET_MODE_FILTER, flags, fp)
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

/// Kill other arches, `rules` (nr, return) in order, ALLOW for the rest.
fn policy(rules: &[(u32, u32)]) -> Vec<(u16, u8, u8, u32)> {
    let mut p = vec![(0x20, 0, 0, 4), (0x15, 1, 0, AUDIT_ARCH_X86_64), (0x06, 0, 0, RET_KILL_PROCESS), (0x20, 0, 0, 0)];
    for &(n, ret) in rules {
        p.push((0x15, 0, 1, n));
        p.push((0x06, 0, 0, ret));
    }
    p.push((0x06, 0, 0, RET_ALLOW));
    p
}

ktest!("ab.sx1.calls", Safe, calls);
fn calls() -> TestResult {
    let mut t = T::new("sx1-calls")?;
    expect("op 99", t.seccomp(99, 0, 0), -EINVAL)?;
    expect("STRICT with flags", t.seccomp(SET_MODE_STRICT, 1, 0), -EINVAL)?;
    expect("STRICT with uargs", t.seccomp(SET_MODE_STRICT, 0, t.a(0)), -EINVAL)?;
    for (a, want) in [(RET_KILL_PROCESS, 0), (RET_KILL_THREAD, 0), (RET_TRAP, 0), (RET_ERRNO, 0), (RET_USER_NOTIF, 0), (RET_TRACE, 0), (RET_LOG, 0), (RET_ALLOW, 0), (0x0001_0000, -EOPNOTSUPP), (RET_ERRNO | 1, -EOPNOTSUPP)] {
        t.put(0x40, &a.to_le_bytes());
        let ua = t.a(0x40);
        expect(&format!("ACTION_AVAIL {a:#x}"), t.seccomp(GET_ACTION_AVAIL, 0, ua), want)?;
    }
    let ua = t.a(0x40);
    expect("ACTION_AVAIL flags", t.seccomp(GET_ACTION_AVAIL, 1, ua), -EINVAL)?;
    expect("ACTION_AVAIL NULL", t.seccomp(GET_ACTION_AVAIL, 0, 0), -EFAULT)?;
    let us = t.a(0x80);
    expect("NOTIF_SIZES", t.seccomp(GET_NOTIF_SIZES, 0, us), 0)?;
    kassert!(t.get(0x80, 6) == [80, 0, 24, 0, 64, 0], "notif sizes {:?}", t.get(0x80, 6));
    // Chromium's and libseccomp's probes: a NULL program with a built flag answers EFAULT.
    for f in [0, FLAG_TSYNC, FLAG_LOG, FLAG_SPEC_ALLOW, FLAG_TSYNC | FLAG_SPEC_ALLOW] {
        expect(&format!("FILTER NULL flags {f}"), t.seccomp(SET_MODE_FILTER, f, 0), -EFAULT)?;
    }
    for f in [FLAG_NEW_LISTENER, FLAG_WAIT_KILLABLE_RECV, 0x40, FLAG_TSYNC | FLAG_NEW_LISTENER] {
        expect(&format!("FILTER NULL flags {f:#x}"), t.seccomp(SET_MODE_FILTER, f, 0), -EINVAL)?;
    }
    expect("PR_SET_SECCOMP(2, kernel address)", t.call(nr_more::PRCTL as u64, &[PR_SET_SECCOMP as u64, 2, KERNEL_ADDR]), -EFAULT)?;
    // Hostile programs never attach.
    let bad: [&[(u16, u8, u8, u32)]; 6] = [
        &[(0x05, 0, 0, 1), (0x06, 0, 0, 0)],
        &[(0x20, 0, 0, 64), (0x06, 0, 0, 0)],
        &[(0x20, 0, 0, 2), (0x06, 0, 0, 0)],
        &[(0x34, 0, 0, 0), (0x06, 0, 0, 0)],
        &[(0x60, 0, 0, 0), (0x16, 0, 0, 0)],
        &[(0x00, 0, 0, 0)],
    ];
    if !t.th.prctl().no_new_privs() {
        expect("PR_SET_NO_NEW_PRIVS", t.call(nr_more::PRCTL as u64, &[PR_SET_NO_NEW_PRIVS as u64, 1]), 0)?;
    }
    for (i, b) in bad.iter().enumerate() {
        expect(&format!("hostile program {i}"), t.install(0, b), -EINVAL)?;
    }
    let mut fp = [0u8; 16];
    fp[0..2].copy_from_slice(&4097u16.to_le_bytes());
    t.put(0x100, &fp);
    let ufp = t.a(0x100);
    expect("4097 instructions", t.seccomp(SET_MODE_FILTER, 0, ufp), -EINVAL)?;
    fp[0..2].copy_from_slice(&1u16.to_le_bytes());
    fp[8..16].copy_from_slice(&KERNEL_ADDR.to_le_bytes());
    t.put(0x100, &fp);
    expect("program at a kernel address", t.seccomp(SET_MODE_FILTER, 0, ufp), -EFAULT)?;
    kassert!(t.th.prctl().seccomp().mode() == 0, "nothing attached");

    expect("attach", t.install(FLAG_SPEC_ALLOW, &policy(&[(GETPPID as u32, RET_ERRNO | EPERM as u32), (GETUID as u32, RET_ERRNO | 0xffff)])), 0)?;
    expect("getppid", t.call(GETPPID, &[]), -EPERM)?;
    expect("getuid (data capped)", t.call(GETUID, &[]), -(MAX_ERRNO as i64))?;
    kassert!(t.call(GETPID, &[]) == t.p.pid as i64, "getpid runs");
    expect("PR_GET_SECCOMP", t.call(nr_more::PRCTL as u64, &[PR_GET_SECCOMP as u64]), 2)?;
    expect("strict after a filter", t.seccomp(SET_MODE_STRICT, 0, 0), -EINVAL)?;
    expect("PR_SET_SECCOMP(1) after a filter", t.call(nr_more::PRCTL as u64, &[PR_SET_SECCOMP as u64, 1]), -EINVAL)?;
    // A second filter: equal actions take the newer data.
    expect("attach 2", t.install(0, &policy(&[(GETPPID as u32, RET_ERRNO | EACCES as u32)])), 0)?;
    expect("getppid (newer data)", t.call(GETPPID, &[]), -EACCES)?;
    kassert!(t.th.prctl().seccomp().filter_count() == 2, "two filters");
    // An x32 number is not the x86-64 one to the filter (and ENOSYS in V2).
    expect("x32 getppid", t.call(GETPPID | X32_SYSCALL_BIT as u64, &[]), -ENOSYS)?;
    Ok(())
}

ktest!("ab.sx1.kills", Safe, kills);
fn kills() -> TestResult {
    for (action, sig) in [(RET_KILL_PROCESS, 31u32), (RET_KILL_THREAD, 31), (0x0001_0000, 31)] {
        let mut t = T::new("sx1-kill")?;
        expect("PR_SET_NO_NEW_PRIVS", t.call(nr_more::PRCTL as u64, &[PR_SET_NO_NEW_PRIVS as u64, 1]), 0)?;
        expect("attach", t.install(0, &policy(&[(GETPPID as u32, action)])), 0)?;
        t.call(GETPPID, &[]);
        kassert!(t.p.group().status() == Some(ExitStatus::Signaled(sig)), "action {action:#x}: status {:?}", t.p.group().status());
        kassert!(t.th.prctl().seccomp().mode() == MODE_DEAD, "the thread is dead to seccomp");
    }
    // Strict mode: read/write/exit/rt_sigreturn only; anything else is SIGKILL.
    let mut t = T::new("sx1-strict")?;
    expect("PR_SET_SECCOMP(1)", t.call(nr_more::PRCTL as u64, &[PR_SET_SECCOMP as u64, 1, KERNEL_ADDR]), 0)?;
    kassert!(t.th.prctl().seccomp().mode() == MODE_STRICT, "strict");
    let ua = t.a(0);
    expect("write(-1) in strict mode", t.call(nr::WRITE as u64, &[u64::MAX, ua, 1]), -EBADF)?;
    t.call(GETPID, &[]);
    kassert!(t.p.group().status() == Some(ExitStatus::Signaled(9)), "strict: status {:?}", t.p.group().status());
    Ok(())
}

ktest!("ab.sx1.trap_and_inherit", Safe, trap_and_inherit);
fn trap_and_inherit() -> TestResult {
    let mut t = T::new("sx1-trap")?;
    expect("PR_SET_NO_NEW_PRIVS", t.call(nr_more::PRCTL as u64, &[PR_SET_NO_NEW_PRIVS as u64, 1]), 0)?;
    expect("attach", t.install(FLAG_LOG, &policy(&[(GETUID as u32, RET_TRAP | 0x42)])), 0)?;
    // A forked child carries the stack (fork through the dispatcher on the held thread is not possible:
    // inheritance itself is TaskPrctl::inherit, shown here as clone uses it).
    let child = proc::prctl_state::TaskPrctl::new();
    child.inherit(t.th.prctl());
    kassert!(child.seccomp().mode() == MODE_FILTER && child.seccomp().filter_count() == 1, "inherited");
    kassert!(child.no_new_privs(), "no_new_privs inherited");
    // TRAP: the call is skipped, rax keeps the number, SIGSYS (default action, no handler) ends the process.
    let r = t.call(GETUID, &[]);
    expect("rax after TRAP", r, GETUID as i64)?;
    kassert!(t.p.group().status() == Some(ExitStatus::Signaled(31)), "TRAP without a handler: status {:?}", t.p.group().status());
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

ktest!("ab.sx1.programs", Safe, programs);
fn programs() -> TestResult {
    if !have("/bin/abi-seccomp") {
        if have("/bin/abi-ids") {
            return Err(String::from("the image has ABI programs but not /bin/abi-seccomp (did it fail to build?)"));
        }
        return ktest::fail("the user/abi-tests programs are not in this image (no musl sysroot when it was built)");
    }
    let modes: [(&str, &str); 7] = [
        ("errno", include_str!("../../../user/abi-tests/expected/sx1-errno.txt")),
        ("trap", include_str!("../../../user/abi-tests/expected/sx1-trap.txt")),
        ("kill", include_str!("../../../user/abi-tests/expected/sx1-kill.txt")),
        ("tsync", include_str!("../../../user/abi-tests/expected/sx1-tsync.txt")),
        ("tsync-race", include_str!("../../../user/abi-tests/expected/sx1-tsync-race.txt")),
        ("inherit", include_str!("../../../user/abi-tests/expected/sx1-inherit.txt")),
        ("checks", include_str!("../../../user/abi-tests/expected/sx1-checks.txt")),
    ];
    for (mode, want) in modes {
        let out = proc::stdio::Capture::new(1 << 20);
        let argv = ["/bin/abi-seccomp", mode];
        let env = ["HOME=/", "PATH=/bin", "ABI_TEST=1"];
        let mut o = proc::SpawnOptions::new(&argv, &env, policy::ProgramId(1));
        o.stdin = Some(proc::stdio::Input::new(Vec::new()) as Arc<dyn vfs::File>);
        o.stdout = Some(out.clone() as Arc<dyn vfs::File>);
        o.stderr = Some(out.clone() as Arc<dyn vfs::File>);
        let p = proc::spawn_with("/bin/abi-seccomp", &o).map_err(|e| format!("spawn: {e}"))?;
        let code = match p.wait(Some(120 * SEC)) {
            Some(c) => c,
            None => {
                p.kill(9);
                let _ = p.wait(Some(10 * SEC));
                return Err(format!("abi-seccomp {mode} did not end within 120 s:\n{}", String::from_utf8_lossy(&out.contents())));
            }
        };
        let text = String::from_utf8_lossy(&out.contents()).into_owned();
        let d = diff(&text, want);
        kassert!(code == 0 && d.is_empty(), "abi-seccomp {mode} differs from CachyOS (exit {code}):\n{}", d.join("\n"));
        klog::event!(Info, "ktest", "sx1.program", mode = mode, lines = text.lines().count());
    }
    Ok(())
}

// The check's cost on the syscall path: getpid through the dispatcher without a mode, with a filter the
// action cache answers, and with one that must run (it reads an argument). Numbers only (QEMU).
ktest!("ab.sx1.getpid_cost", Safe, getpid_cost);
fn getpid_cost() -> TestResult {
    const N: u64 = 20_000;
    let mut t = T::new("sx1-cost")?;
    // The best of five laps (QEMU's own noise: interrupts, vCPU preemption).
    let lap = |t: &mut T| -> u64 {
        (0..5)
            .map(|_| {
                let t0 = time::now_ns();
                for _ in 0..N {
                    t.call(GETPID, &[]);
                }
                (time::now_ns() - t0) / N
            })
            .min()
            .unwrap_or(0)
    };
    let none = lap(&mut t);
    expect("PR_SET_NO_NEW_PRIVS", t.call(nr_more::PRCTL as u64, &[PR_SET_NO_NEW_PRIVS as u64, 1]), 0)?;
    // A Chromium-like shape: a few dozen numbers compared before getpid's ALLOW.
    let rules: Vec<(u32, u32)> = (200..240).map(|n| (n, RET_ERRNO | 1)).collect();
    expect("attach (cached)", t.install(0, &policy(&rules)), 0)?;
    let cached = lap(&mut t);
    // getpid's answer now depends on its first argument: every call runs both filters.
    let arg = [(0x20, 0, 0, 0), (0x15, 0, 3, GETPID as u32), (0x20, 0, 0, 16), (0x15, 1, 0, 0xdead), (0x06, 0, 0, RET_ALLOW), (0x06, 0, 0, RET_ERRNO | 1), (0x06, 0, 0, RET_ALLOW)];
    expect("attach (argument)", t.install(0, &arg), 0)?;
    kassert!(t.call(GETPID, &[0xdead]) == -1, "the argument rule works");
    let run = lap(&mut t);
    klog::event!(Info, "ktest", "sx1.getpid_cost", none_ns = none, cached_ns = cached, filtered_ns = run, calls = N; "ns per getpid through the dispatcher (QEMU: not judged)");
    Ok(())
}
