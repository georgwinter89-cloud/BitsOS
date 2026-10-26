//! AB-6 fixer ktests (Safe, rerunnable): the kill rules the INT-A1 review asked for (uid, PID 1, ESRCH/EINVAL,
//! pid 0 and -1 with signal 0 only), a fatal signal waking a sleeping program (nanosleep and futex), SIGCHLD with
//! CLD_EXITED/CLD_KILLED siginfo to the parent, `proc::kill_pgrp` for IN-4, and the timer service
//! (ITIMER_REAL and a POSIX timer reach a held process as signals). Restarts of interrupted calls need a real
//! program's thread (the held processes here run on the test's kernel thread): `abi-sig2 restart`.

use crate::ab6_ktests::T;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use ktest::{TestResult, kassert, ktest};
use proc::{ExitStatus, Process};
use uapi::ab::nr_more;
use uapi::errno::*;
use uapi::nr;

const SEC: u64 = 1_000_000_000;
const SIGCHLD: u64 = 17;

/// A real (not held) program from machine code; `parent` collects it with wait4.
fn run(name: &str, code: &[u8], parent: Option<&Arc<Process>>) -> Result<Arc<Process>, String> {
    let image = crate::ktests::elf(code);
    let argv = [name];
    let mut o = proc::SpawnOptions::new(&argv, &[], policy::ProgramId(1));
    o.parent = parent;
    proc::spawn_image_with(name, &image, &o).map_err(|e| format!("spawn {name}: {e}"))
}

/// Waits up to `secs` for `p` to end; its status.
fn ended(p: &Process, secs: u64) -> Option<ExitStatus> {
    p.wait(Some(secs * SEC))?;
    p.status()
}

/// Kills a program that is still running at the end of a test.
struct Reap(Arc<Process>);
impl Drop for Reap {
    fn drop(&mut self) {
        if !self.0.has_exited() {
            self.0.kill(9);
        }
        let _ = self.0.wait(Some(5 * SEC));
    }
}

// nanosleep({100, 0}); jmp .
const SLEEPER: [u8; 18] = [0x6a, 0x00, 0x6a, 0x64, 0x48, 0x89, 0xe7, 0x31, 0xf6, 0xb8, 0x23, 0, 0, 0, 0x0f, 0x05, 0xeb, 0xfe];
// futex(&word = 0, FUTEX_WAIT_PRIVATE, 0, NULL); jmp .
const FUTEX_SLEEPER: [u8; 25] = [0x6a, 0x00, 0x48, 0x89, 0xe7, 0xbe, 0x80, 0, 0, 0, 0x31, 0xd2, 0x4d, 0x31, 0xd2, 0xb8, 0xca, 0, 0, 0, 0x0f, 0x05, 0xeb, 0xfe, 0x90];
// exit(3)
const EXIT3: [u8; 14] = [0xbf, 0x03, 0, 0, 0, 0xb8, 0x3c, 0, 0, 0, 0x0f, 0x05, 0xeb, 0xfe];
// jmp .
const SPIN: [u8; 2] = [0xeb, 0xfe];

ktest!("ab.signal.kill_perms", Safe, kill_perms);
fn kill_perms() -> TestResult {
    let mut me = T::new("ab6-killer")?;
    let root = T::with("ab6-root-target", 0, false)?;
    let peer = T::with("ab6-peer", 1000, false)?;
    let (rp, pp) = (root.p.pid as u64, peer.p.pid as u64);
    // uid 1000 may not signal uid 0 (not even probe it); its own uid it may.
    kassert!(me.call(nr::KILL, &[rp, 15]) == -EPERM, "kill(root, SIGTERM) from uid 1000");
    kassert!(me.call(nr::KILL, &[rp, 0]) == -EPERM, "kill(root, 0) from uid 1000");
    kassert!(root.alive(), "the refused signal reached its target");
    kassert!(me.call(nr::KILL, &[pp, 0]) == 0, "kill(same uid, 0)");
    kassert!(me.call(nr::TKILL, &[root.th.tid() as u64, 0]) == -EPERM, "tkill(root thread, 0)");
    // ESRCH and EINVAL.
    kassert!(me.call(nr::KILL, &[i32::MAX as u64, 15]) == -ESRCH, "kill(no such pid)");
    kassert!(me.call(nr::KILL, &[(-(i32::MAX as i64)) as u64, 0]) == -ESRCH, "kill(-no such group)");
    kassert!(me.call(nr::KILL, &[me.p.pid as u64, 65]) == -EINVAL, "kill(self, 65)");
    kassert!(me.call(nr::KILL, &[me.p.pid as u64, u64::MAX]) == -EINVAL, "kill(self, -1)");
    kassert!(me.call(nr::TKILL, &[0, 0]) == -EINVAL, "tkill(0)");
    kassert!(me.call(nr::TGKILL, &[0, me.th.tid() as u64, 0]) == -EINVAL, "tgkill(0, tid)");
    kassert!(me.call(nr::TGKILL, &[pp, me.th.tid() as u64, 0]) == -ESRCH, "tgkill(other tgid, my tid)");
    // pid 0 (my group) and -1 (everyone I may signal) with signal 0 only: probes, nothing is sent.
    kassert!(me.call(nr::KILL, &[0, 0]) == 0, "kill(0, 0)");
    kassert!(me.call(nr::KILL, &[u64::MAX, 0]) == 0, "kill(-1, 0): peers exist");
    // rt_sigqueueinfo: a program cannot pose as kill() (si_code >= 0) towards another process.
    let mut info = [0u8; 128];
    info[8..12].copy_from_slice(&0i32.to_le_bytes());
    me.put(0, &info);
    kassert!(me.call(nr_more::RT_SIGQUEUEINFO, &[pp, 10, me.a(0)]) == -EPERM, "sigqueueinfo SI_USER to a peer");
    info[8..12].copy_from_slice(&(-1i32).to_le_bytes());
    me.put(0, &info);
    kassert!(me.call(nr_more::RT_SIGQUEUEINFO, &[rp, 10, me.a(0)]) == -EPERM, "sigqueueinfo SI_QUEUE to root");
    kassert!(peer.alive() && root.alive());
    pid1_rules(&mut me)
}

/// SIGNAL_UNKILLABLE: an init without handlers survives even SIGKILL from kill(); a handled signal reaches
/// it. Only with a pid 1 of our own: the real init (the Mini-PC's) is only probed.
fn pid1_rules(me: &mut T) -> TestResult {
    if proc::find(1).is_some() {
        kassert!(me.call(nr::KILL, &[1, 0]) == -EPERM, "kill(1, 0) from uid 1000 (init runs as root)");
        return Ok(());
    }
    let mut init = T::with("ab6-init", 0, true)?;
    let mut boss = T::with("ab6-root", 0, false)?;
    kassert!(init.p.pid == 1, "the fake init got pid {}", init.p.pid);
    kassert!(boss.call(nr::KILL, &[1, 15]) == 0 && boss.call(nr::KILL, &[1, 9]) == 0, "kill(1) answers 0");
    kassert!(init.alive(), "init ended on a signal it has no handler for");
    init.install(10, 0x40_3000, 0, 0)?;
    kassert!(boss.call(nr::KILL, &[1, 10]) == 0);
    kassert!(init.th.pending_blocked() == 0 && init.alive(), "init's handled signal");
    let pending = init.p.signals.raw(|s| s.pending_of(init.th.sig_key()));
    kassert!((pending.0 | pending.1) & (1 << 9) != 0, "SIGUSR1 with a handler is pending at init: {pending:?}");
    Ok(())
}

ktest!("ab.signal.kill_wakes_sleeper", Safe, kill_wakes_sleeper);
fn kill_wakes_sleeper() -> TestResult {
    for (name, code) in [("ab6-nanosleep", &SLEEPER[..]), ("ab6-futex", &FUTEX_SLEEPER[..])] {
        let p = Reap(run(name, code, None)?);
        // Let it reach its sleep (100 s / an untimed futex wait).
        sched::sleep_ns(200_000_000);
        kassert!(!p.0.has_exited(), "{name} ended on its own");
        let mut k = T::new("ab6-kill-sender")?;
        let t0 = time::now_ns();
        kassert!(k.call(nr::KILL, &[p.0.pid as u64, 15]) == 0, "kill {name}");
        let st = ended(&p.0, 3);
        let ms = (time::now_ns() - t0) / 1_000_000;
        kassert!(st == Some(ExitStatus::Signaled(15)), "{name}: {st:?} after SIGTERM");
        klog::event!(Info, "ktest", "ab6.kill_wakes", program = name, ms = ms);
    }
    Ok(())
}

/// rt_sigtimedwait({SIGCHLD}) with a zero timeout as `t`: the siginfo bytes.
fn take_sigchld(t: &mut T) -> Result<[u8; 128], String> {
    t.put(256, &(1u64 << (SIGCHLD - 1)).to_le_bytes());
    t.put(272, &[0u8; 16]);
    let r = t.call(nr_more::RT_SIGTIMEDWAIT, &[t.a(256), t.a(512), t.a(272), 8]);
    if r != SIGCHLD as i64 {
        return Err(format!("sigtimedwait = {r}, want SIGCHLD"));
    }
    let mut b = [0u8; 128];
    b.copy_from_slice(&t.get(512, 128));
    Ok(b)
}

fn w32(b: &[u8], o: usize) -> i32 {
    i32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

ktest!("ab.signal.sigchld_parent", Safe, sigchld_parent);
fn sigchld_parent() -> TestResult {
    let mut parent = T::new("ab6-parent")?;
    // A default-ignored SIGCHLD is only kept while blocked.
    parent.put(256, &(1u64 << (SIGCHLD - 1)).to_le_bytes());
    kassert!(parent.call(nr::RT_SIGPROCMASK, &[0, parent.a(256), 0, 8]) == 0, "block SIGCHLD");
    let c = run("ab6-child-exit", &EXIT3, Some(&parent.p))?;
    kassert!(ended(&c, 5) == Some(ExitStatus::Exited(3)), "the child: {:?}", c.status());
    let i = take_sigchld(&mut parent)?;
    kassert!(w32(&i, 8) == uapi::ab::siginfo::CLD_EXITED, "si_code {}", w32(&i, 8));
    kassert!(w32(&i, 16) as u32 == c.pid && w32(&i, 20) == 1000 && w32(&i, 24) == 3, "si_pid/uid/status {} {} {}", w32(&i, 16), w32(&i, 20), w32(&i, 24));
    kassert!(parent.call(nr::WAIT4, &[c.pid as u64, 0, 0, 0]) == c.pid as i64, "wait4 collects the child");
    let c = run("ab6-child-spin", &SPIN, Some(&parent.p))?;
    sched::sleep_ns(50_000_000);
    kassert!(parent.call(nr::KILL, &[c.pid as u64, 9]) == 0);
    kassert!(ended(&c, 5) == Some(ExitStatus::Signaled(9)), "the spinner: {:?}", c.status());
    let i = take_sigchld(&mut parent)?;
    kassert!(w32(&i, 8) == uapi::ab::siginfo::CLD_KILLED && w32(&i, 24) == 9 && w32(&i, 16) as u32 == c.pid, "CLD_KILLED siginfo");
    kassert!(parent.call(nr::WAIT4, &[c.pid as u64, 0, 0, 0]) == c.pid as i64);
    Ok(())
}

ktest!("ab.signal.kill_pgrp", Safe, kill_pgrp);
fn kill_pgrp() -> TestResult {
    let mut a = T::new("ab6-pgrp-a")?;
    let mut b = T::new("ab6-pgrp-b")?;
    // Each in a group of its own (a program the kernel starts leads its own session and group already).
    for t in [&mut a, &mut b] {
        if t.p.pgid() != t.p.pid {
            let r = t.call(nr::SETPGID, &[0, 0]);
            kassert!(r == 0, "setpgid: {r}");
        }
        kassert!(t.p.pgid() == t.p.pid, "pgid {} of {}", t.p.pgid(), t.p.pid);
    }
    b.install(2, 0x40_3000, 0, 0)?;
    kassert!(proc::kill_pgrp(a.p.pid, 2) == 1, "one member reached");
    kassert!(a.p.status() == Some(ExitStatus::Signaled(2)), "SIGINT default ends a: {:?}", a.p.status());
    kassert!(b.alive(), "another group was hit");
    kassert!(proc::kill_pgrp(b.p.pid, 2) == 1 && b.alive(), "b handles SIGINT");
    kassert!(proc::kill_pgrp(i32::MAX as u32, 2) == 0, "an empty group");
    Ok(())
}

ktest!("ab.signal.timers", Safe, timers);
/// The timer service: alarm/ITIMER_REAL and a POSIX timer send their signals to a held process (blocked, so
/// they stay pending); getitimer and timer_gettime read back; timer_delete and setitimer(0) disarm.
fn timers() -> TestResult {
    let mut t = T::new("ab6-timers")?;
    t.put(0, &(1u64 << 13 | 1u64 << 39).to_le_bytes()); // SIGALRM, SIGRTMIN+7 (40)
    kassert!(t.call(nr::RT_SIGPROCMASK, &[0, t.a(0), 0, 8]) == 0, "block");
    kassert!(t.call(nr_more::ALARM, &[100]) == 0, "first alarm");
    let left = t.call(nr_more::ALARM, &[0]);
    kassert!(left == 100, "alarm(0) answered {left}, want 100 (rounded up)");
    // setitimer(ITIMER_REAL, 50 ms), then wait for SIGALRM.
    let mut it = [0u8; 32];
    it[24..32].copy_from_slice(&50_000i64.to_le_bytes());
    t.put(64, &it);
    kassert!(t.call(nr_more::SETITIMER, &[0, t.a(64), 0]) == 0, "setitimer");
    kassert!(t.call(nr_more::GETITIMER, &[0, t.a(128)]) == 0);
    let usec = i64::from_le_bytes(t.get(128 + 24, 8).try_into().unwrap_or([0; 8]));
    kassert!(usec > 0 && usec <= 50_000, "getitimer value {usec} us");
    kassert!(pending_within(&t, 1 << 13, 3), "SIGALRM did not come within 3 s");
    // A POSIX timer on CLOCK_MONOTONIC with SIGEV_SIGNAL 40, every 20 ms.
    let mut ev = [0u8; 64];
    ev[0..8].copy_from_slice(&0x55u64.to_le_bytes());
    ev[8..12].copy_from_slice(&40i32.to_le_bytes());
    t.put(192, &ev);
    kassert!(t.call(nr_more::TIMER_CREATE, &[1, t.a(192), t.a(256)]) == 0, "timer_create");
    let id = i32::from_le_bytes(t.get(256, 4).try_into().unwrap_or([0; 4]));
    let mut spec = [0u8; 32];
    spec[0..16].copy_from_slice(&uapi::Timespec::from_ns(20_000_000).to_bytes());
    spec[16..32].copy_from_slice(&uapi::Timespec::from_ns(20_000_000).to_bytes());
    t.put(320, &spec);
    kassert!(t.call(nr_more::TIMER_SETTIME, &[id as u64, 0, t.a(320), 0]) == 0, "timer_settime");
    kassert!(pending_within(&t, 1 << 39, 3), "the POSIX timer's signal did not come within 3 s");
    // Taken through sigtimedwait: SI_TIMER with the id and sigev_value; the overrun counts what was missed.
    sched::sleep_ns(100_000_000);
    t.put(400, &(1u64 << 39).to_le_bytes());
    t.put(416, &[0u8; 16]);
    kassert!(t.call(nr_more::RT_SIGTIMEDWAIT, &[t.a(400), t.a(512), t.a(416), 8]) == 40, "sigtimedwait");
    let i = t.get(512, 40);
    kassert!(w32(&i, 8) == uapi::ab::siginfo::SI_TIMER && w32(&i, 16) == id, "SI_TIMER of timer {id}");
    kassert!(u64::from_le_bytes(i[24..32].try_into().unwrap_or([0; 8])) == 0x55, "si_value");
    kassert!(w32(&i, 20) >= 2, "si_overrun {} after 100 ms of a 20 ms timer", w32(&i, 20));
    kassert!(t.call(nr_more::TIMER_GETOVERRUN, &[id as u64]) == w32(&i, 20) as i64);
    kassert!(t.call(nr_more::TIMER_GETTIME, &[id as u64, t.a(600)]) == 0);
    kassert!(t.call(nr_more::TIMER_DELETE, &[id as u64]) == 0 && t.call(nr_more::TIMER_DELETE, &[id as u64]) == -EINVAL, "delete twice");
    kassert!(t.call(nr_more::TIMER_GETTIME, &[id as u64, t.a(600)]) == -EINVAL);
    kassert!(t.call(nr_more::SETITIMER, &[3, t.a(64), 0]) == -EINVAL, "which 3");
    Ok(())
}

/// Whether one of `bits` is pending (and blocked) at `t` within `secs`.
fn pending_within(t: &T, bits: u64, secs: u64) -> bool {
    let end = time::now_ns() + secs * SEC;
    while time::now_ns() < end {
        if t.th.pending_blocked() & bits != 0 {
            return true;
        }
        sched::sleep_ns(5_000_000);
    }
    false
}
