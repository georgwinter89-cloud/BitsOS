//! SC-6 ktests: the affinity, priority, policy, getcpu, times/getrusage and CPU-clock syscalls through the
//! real dispatcher on held test processes (the test thread plays the program, as AB-3's wave-A tests do), and
//! real programs: `abi-sched` (transcript equal to CachyOS's, `expected/sched.txt`) and `abi-sched burn` (user
//! and system time of a real program; needs the proc side of SC-6, skipped until it is applied).
//!
//! All `Safe` and rerunnable: every test makes its own processes and kills them at the end.

use crate::sched_sys::{self, nr as snr};
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use hal::entry::UserContext;
use ktest::{TestResult, kassert, ktest};
use proc::Process;
use uapi::errno::*;
use uapi::nr;

const SEC: u64 = 1_000_000_000;
/// `mov eax, 231; xor edi, edi; syscall` (never runs: the process stays held).
const EXIT0: [u8; 9] = [0xb8, 0xe7, 0, 0, 0, 0x31, 0xff, 0x0f, 0x05];
const SCRATCH: u64 = 64 * 1024;
const KERNEL_ADDR: u64 = 0xffff_8880_0000_0000;
const NOBODY: u64 = 4_000_000;
/// A uid no other process uses (PRIO_USER tests).
const TEST_UID: u32 = 4_242;

struct T {
    p: Arc<Process>,
    /// The main thread (the dispatcher's caller; AB-4).
    th: Arc<proc::Thread>,
    ctx: UserContext,
    mem: u64,
    _pin: proc::MmPin,
}

impl T {
    fn new(name: &str, uid: u32) -> Result<T, String> {
        let image = crate::ktests::elf(&EXIT0);
        let argv = [name];
        let mut o = proc::SpawnOptions::new(&argv, &[], policy::ProgramId(1));
        o.held = true;
        o.uid = uid;
        let p = proc::spawn_image_with(name, &image, &o).map_err(|e| format!("spawn {name}: {e}"))?;
        let mem = p.mm.sys_mmap(0, SCRATCH, 3, 0x22, 0).map_err(|e| format!("mmap: {e:?}"))?;
        let end = time::now_ns() + 5 * SEC;
        // Its thread records its id first thing (then waits for `release`).
        while p.thread_id() == 0 {
            kassert!(time::now_ns() < end, "{name}: its thread did not start within 5 s");
            sched::sleep_ms(1);
        }
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
    fn u32_at(&self, at: u64) -> u32 {
        u32::from_le_bytes(self.get(at, 4).try_into().unwrap_or([0; 4]))
    }
    fn tid(&self) -> u64 {
        self.p.thread_id()
    }
    fn pid(&self) -> u64 {
        self.p.pid as u64
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

fn online(c: usize) -> bool {
    sched::cpu_stats().get(c).is_some_and(|s| s.online)
}

ktest!("sc.sched.affinity_calls", Safe, affinity_calls);
fn affinity_calls() -> TestResult {
    let mut t = T::new("sc6-aff", 1000)?;
    let size = sched_sys::cpumask_size() as i64;
    let possible = hal::percpu::possible_cpus();
    let last = (0..possible).rev().find(|&c| online(c)).ok_or("no online CPU")?;
    // getaffinity: Linux's length rules; the full mask = the online CPUs.
    expect("getaffinity len 4", t.call(nr::SCHED_GETAFFINITY, &[0, 4, t.a(0)]), -EINVAL)?;
    expect("getaffinity len 12", t.call(nr::SCHED_GETAFFINITY, &[0, 12, t.a(0)]), -EINVAL)?;
    expect("getaffinity len 128", t.call(nr::SCHED_GETAFFINITY, &[0, 128, t.a(0)]), size)?;
    expect("getaffinity pid -1", t.call(nr::SCHED_GETAFFINITY, &[u64::MAX, 128, t.a(0)]), -ESRCH)?;
    expect("getaffinity into the kernel", t.call(nr::SCHED_GETAFFINITY, &[0, 128, KERNEL_ADDR]), -EFAULT)?;
    // `unsigned int len`, multiplied in 32 bits: 2^29 wraps to 0 bits (EINVAL); bits above 32 are dropped.
    expect("getaffinity len 2^29", t.call(nr::SCHED_GETAFFINITY, &[0, 0x2000_0000, t.a(0)]), -EINVAL)?;
    expect("getaffinity len 2^32 + 128", t.call(nr::SCHED_GETAFFINITY, &[0, (1 << 32) + 128, t.a(0)]), size)?;
    // setaffinity: the copy before the lookup, then ESRCH, then the mask check.
    t.put(0, &[0u8; 8]);
    expect("setaffinity bad pointer, no such pid", t.call(nr::SCHED_SETAFFINITY, &[NOBODY, 8, KERNEL_ADDR]), -EFAULT)?;
    expect("setaffinity no such pid", t.call(nr::SCHED_SETAFFINITY, &[NOBODY, 8, t.a(0)]), -ESRCH)?;
    expect("setaffinity pid -1", t.call(nr::SCHED_SETAFFINITY, &[u64::MAX, 8, t.a(0)]), -ESRCH)?;
    expect("setaffinity empty", t.call(nr::SCHED_SETAFFINITY, &[0, 8, t.a(0)]), -EINVAL)?;
    expect("setaffinity len 0", t.call(nr::SCHED_SETAFFINITY, &[0, 0, t.a(0)]), -EINVAL)?;
    if possible < 64 {
        t.put(0, &(1u64 << 63).to_le_bytes());
        expect("setaffinity only a CPU beyond the possible ones", t.call(nr::SCHED_SETAFFINITY, &[0, 8, t.a(0)]), -EINVAL)?;
    }
    // One CPU, with a long length (cut to cpumask_size) and with one byte (zero-extended).
    let mut one = vec![0u8; 128];
    one[last / 8] = 1 << (last % 8);
    one[127] = 0xFF;
    t.put(0, &one);
    expect("setaffinity {last}", t.call(nr::SCHED_SETAFFINITY, &[0, 128, t.a(0)]), 0)?;
    expect("getaffinity after", t.call(nr::SCHED_GETAFFINITY, &[0, 128, t.a(256)]), size)?;
    let got = t.get(256, size as usize);
    let want: Vec<u8> = (0..size as usize).map(|i| one[i]).collect();
    kassert!(got == want, "mask read back {got:?}, want {want:?}");
    kassert!(sched::attr::affinity(t.tid()).is_some_and(|m| (0..possible).all(|c| sched::attr::mask_has(&m, c) == (c == last))), "the thread's mask is not {{{last}}}");
    if online(0) && last != 0 {
        t.put(0, &[1u8]);
        expect("setaffinity len 1 = {0}", t.call(nr::SCHED_SETAFFINITY, &[0, 1, t.a(0)]), 0)?;
        kassert!(sched::attr::affinity(t.tid()).is_some_and(|m| sched::attr::mask_has(&m, 0) && !sched::attr::mask_has(&m, last)), "len 1 did not give {{0}}");
    }
    t.put(0, &[0xFF; 128]);
    expect("setaffinity all", t.call(nr::SCHED_SETAFFINITY, &[0, 128, t.a(0)]), 0)?;
    // Another process: by pid; another owner's needs root.
    let root = T::new("sc6-aff-root", 0)?;
    expect("setaffinity of another user's process", t.call(nr::SCHED_SETAFFINITY, &[root.pid(), 8, t.a(0)]), -EPERM)?;
    expect("getaffinity of another user's process", t.call(nr::SCHED_GETAFFINITY, &[root.pid(), size as u64, t.a(256)]), size)?;
    let mut r = root;
    let other = T::new("sc6-aff-other", 1000)?;
    r.put(0, &one);
    expect("root sets another user's affinity", r.call(nr::SCHED_SETAFFINITY, &[other.pid(), size as u64, r.a(0)]), 0)?;
    // It sticks: read back by another process, and in the target's own thread.
    expect("getaffinity of the other process", t.call(nr::SCHED_GETAFFINITY, &[other.pid(), 128, t.a(512)]), size)?;
    let got = t.get(512, size as usize);
    kassert!(got == want, "another process's mask read back {got:?}, want {want:?}");
    kassert!(sched::attr::affinity(other.tid()).is_some_and(|m| (0..possible).all(|c| sched::attr::mask_has(&m, c) == (c == last))), "the other thread's mask is not {{{last}}}");
    expect("same user sets another process's affinity", t.call(nr::SCHED_SETAFFINITY, &[other.pid(), 8, t.a(0)]), 0)?;
    expect("getaffinity of the other process again", t.call(nr::SCHED_GETAFFINITY, &[other.pid(), 128, t.a(512)]), size)?;
    let got = t.get(512, size as usize);
    let all: Vec<u8> = (0..size as usize).map(|i| (0..8).filter(|b| online(i * 8 + b)).fold(0u8, |a, b| a | 1 << b)).collect();
    kassert!(got == all, "after 'all': {got:?}, want the online CPUs {all:?}");
    Ok(())
}

ktest!("sc.sched.getcpu", Safe, getcpu);
fn getcpu() -> TestResult {
    let mut t = T::new("sc6-getcpu", 1000)?;
    let cpu = sched::current_cpu();
    // The test thread is the boot thread, pinned: the CPU cannot change under the call.
    t.put(0, &[0xEE; 8]);
    expect("getcpu", t.call(nr::GETCPU, &[t.a(0), t.a(4), 0]), 0)?;
    kassert!(t.u32_at(0) == cpu as u32 && t.u32_at(4) == 0, "getcpu wrote cpu {} node {}, want {cpu} 0", t.u32_at(0), t.u32_at(4));
    expect("getcpu NULLs", t.call(nr::GETCPU, &[0, 0, 0]), 0)?;
    expect("getcpu bad cpu pointer", t.call(nr::GETCPU, &[KERNEL_ADDR, t.a(4), 0]), -EFAULT)?;
    expect("getcpu bad node pointer", t.call(nr::GETCPU, &[t.a(0), KERNEL_ADDR, 0]), -EFAULT)?;
    Ok(())
}

ktest!("sc.sched.priority_calls", Safe, priority_calls);
fn priority_calls() -> TestResult {
    let mut t = T::new("sc6-prio", 1000)?;
    let _ = t.p.limits.prlimit(uapi::ab::rlimit::RLIMIT_NICE, Some(proc::rlimit::Rlimit { cur: 0, max: 0 }), vfs::abi::NR_OPEN as u64, true);
    let (pp, pg, pu) = (sched_sys::PRIO_PROCESS as u64, sched_sys::PRIO_PGRP as u64, sched_sys::PRIO_USER as u64);
    expect("setpriority which 3", t.call(nr::SETPRIORITY, &[3, 0, 0]), -EINVAL)?;
    expect("setpriority which -1", t.call(nr::SETPRIORITY, &[u64::MAX, 0, 0]), -EINVAL)?;
    expect("getpriority which 3", t.call(nr::GETPRIORITY, &[3, 0]), -EINVAL)?;
    expect("getpriority", t.call(nr::GETPRIORITY, &[pp, 0]), 20)?;
    expect("setpriority 5", t.call(nr::SETPRIORITY, &[pp, 0, 5]), 0)?;
    expect("getpriority after 5", t.call(nr::GETPRIORITY, &[pp, 0]), 15)?;
    kassert!(sched::attr::nice(t.tid()) == Some(5), "nice {:?}", sched::attr::nice(t.tid()));
    kassert!(sched::thread_stats().iter().any(|s| s.id == t.tid() && s.nice == 5), "thread_stats does not show nice 5");
    expect("setpriority 100 clamps to 19", t.call(nr::SETPRIORITY, &[pp, 0, 100]), 0)?;
    expect("getpriority after 100", t.call(nr::GETPRIORITY, &[pp, 0]), 1)?;
    expect("setpriority back to 0 without RLIMIT_NICE", t.call(nr::SETPRIORITY, &[pp, 0, 0]), -EACCES)?;
    expect("setpriority 19 again (no change)", t.call(nr::SETPRIORITY, &[pp, 0, 19]), 0)?;
    expect("setpriority no such pid", t.call(nr::SETPRIORITY, &[pp, NOBODY, 0]), -ESRCH)?;
    expect("getpriority no such pid", t.call(nr::GETPRIORITY, &[pp, NOBODY]), -ESRCH)?;
    expect("setpriority pid -1", t.call(nr::SETPRIORITY, &[pp, u64::MAX, 0]), -ESRCH)?;
    expect("setpriority no such user", t.call(nr::SETPRIORITY, &[pu, NOBODY, 0]), -ESRCH)?;
    expect("getpriority no such user", t.call(nr::GETPRIORITY, &[pu, NOBODY]), -ESRCH)?;
    let g = t.call(nr::GETPRIORITY, &[pg, 0]);
    kassert!((1..=40).contains(&g), "getpriority of the process group: {g}");
    // Another owner: EPERM; root may lower nice (CAP_SYS_NICE) and change another user's process.
    let mut root = T::new("sc6-prio-root", 0)?;
    expect("setpriority of root's process", t.call(nr::SETPRIORITY, &[pp, root.pid(), 10]), -EPERM)?;
    expect("getpriority of root's process", t.call(nr::GETPRIORITY, &[pp, root.pid()]), 20)?;
    expect("root lowers its nice", root.call(nr::SETPRIORITY, &[pp, 0, (-10i64) as u64]), 0)?;
    expect("root's getpriority", root.call(nr::GETPRIORITY, &[pp, 0]), 30)?;
    kassert!(sched::thread_stats().iter().any(|s| s.id == root.tid() && s.nice == -10), "root's thread not at nice -10");
    expect("root lowers another user's nice", root.call(nr::SETPRIORITY, &[pp, t.pid(), u64::MAX]), 0)?;
    expect("getpriority after root", t.call(nr::GETPRIORITY, &[pp, 0]), 21)?;
    // PRIO_USER by root over a uid only this test uses (never uid 1000: that would renice every program).
    let mut x1 = T::new("sc6-prio-u1", TEST_UID)?;
    let x2 = T::new("sc6-prio-u2", TEST_UID)?;
    expect("PRIO_USER to 3 by root", root.call(nr::SETPRIORITY, &[pu, TEST_UID as u64, 3]), 0)?;
    for (who, x) in [("first", &x1), ("second", &x2)] {
        kassert!(sched::attr::nice(x.tid()) == Some(3), "{who}: nice {:?} after PRIO_USER", sched::attr::nice(x.tid()));
        kassert!(sched::thread_stats().iter().any(|s| s.id == x.tid() && s.nice == 3), "{who}: nice not visible in thread_stats");
    }
    expect("PRIO_USER of the own uid", x1.call(nr::SETPRIORITY, &[pu, 0, 4]), 0)?;
    expect("getpriority PRIO_USER", x1.call(nr::GETPRIORITY, &[pu, 0]), 16)?;
    kassert!(sched::attr::nice(t.tid()) == Some(-1), "uid 1000's process was touched by PRIO_USER {TEST_UID}");
    Ok(())
}

ktest!("sc.sched.scheduler_calls", Safe, scheduler_calls);
fn scheduler_calls() -> TestResult {
    let mut t = T::new("sc6-policy", 1000)?;
    let _ = t.p.limits.prlimit(uapi::ab::rlimit::RLIMIT_NICE, Some(proc::rlimit::Rlimit { cur: 0, max: 0 }), vfs::abi::NR_OPEN as u64, true);
    let param = |t: &T, v: i32| t.put(0, &v.to_le_bytes());
    let p = t.a(0);
    let set = snr::SCHED_SETPARAM;
    param(&t, 0);
    expect("getscheduler", t.call(snr::SCHED_GETSCHEDULER, &[0]), 0)?;
    expect("getscheduler pid -1", t.call(snr::SCHED_GETSCHEDULER, &[u64::MAX]), -EINVAL)?;
    expect("getscheduler no such pid", t.call(snr::SCHED_GETSCHEDULER, &[NOBODY]), -ESRCH)?;
    expect("setscheduler policy -1", t.call(nr::SCHED_SETSCHEDULER, &[0, u64::MAX, p]), -EINVAL)?;
    expect("setscheduler NULL param", t.call(nr::SCHED_SETSCHEDULER, &[0, 0, 0]), -EINVAL)?;
    expect("setscheduler pid -1", t.call(nr::SCHED_SETSCHEDULER, &[u64::MAX, 0, p]), -EINVAL)?;
    expect("setscheduler bad param", t.call(nr::SCHED_SETSCHEDULER, &[0, 0, KERNEL_ADDR]), -EFAULT)?;
    expect("setscheduler bad param, no such pid", t.call(nr::SCHED_SETSCHEDULER, &[NOBODY, 0, KERNEL_ADDR]), -EFAULT)?;
    expect("setscheduler no such pid", t.call(nr::SCHED_SETSCHEDULER, &[NOBODY, 0, p]), -ESRCH)?;
    for pol in [4u64, 8, 0x4000_0004] {
        expect(&format!("setscheduler policy {pol:#x}"), t.call(nr::SCHED_SETSCHEDULER, &[0, pol, p]), -EINVAL)?;
    }
    expect("SCHED_DEADLINE without parameters", t.call(nr::SCHED_SETSCHEDULER, &[0, 6, p]), -EINVAL)?;
    expect("SCHED_FIFO priority 0", t.call(nr::SCHED_SETSCHEDULER, &[0, 1, p]), -EINVAL)?;
    param(&t, 1);
    expect("SCHED_OTHER priority 1", t.call(nr::SCHED_SETSCHEDULER, &[0, 0, p]), -EINVAL)?;
    expect("SCHED_FIFO priority 1 (no Interactive grant)", t.call(nr::SCHED_SETSCHEDULER, &[0, 1, p]), -EPERM)?;
    expect("SCHED_RR priority 1 (no Interactive grant)", t.call(nr::SCHED_SETSCHEDULER, &[0, 2, p]), -EPERM)?;
    param(&t, 100);
    expect("SCHED_FIFO priority 100", t.call(nr::SCHED_SETSCHEDULER, &[0, 1, p]), -EINVAL)?;
    param(&t, -1);
    expect("SCHED_FIFO priority -1", t.call(nr::SCHED_SETSCHEDULER, &[0, 1, p]), -EINVAL)?;
    param(&t, 0);
    expect("SCHED_BATCH", t.call(nr::SCHED_SETSCHEDULER, &[0, 3, p]), 0)?;
    expect("getscheduler BATCH", t.call(snr::SCHED_GETSCHEDULER, &[0]), 3)?;
    kassert!(sched::attr::policy(t.tid()) == Some((sched::Policy::Batch, false)), "policy {:?}", sched::attr::policy(t.tid()));
    expect("SCHED_IDLE", t.call(nr::SCHED_SETSCHEDULER, &[0, 5, p]), 0)?;
    kassert!(sched::thread_stats().iter().any(|s| s.id == t.tid() && s.policy == sched::Policy::Idle), "not idle in thread_stats");
    expect("SCHED_IDLE -> OTHER without RLIMIT_NICE", t.call(nr::SCHED_SETSCHEDULER, &[0, 0, p]), -EPERM)?;
    expect("SCHED_IDLE again", t.call(nr::SCHED_SETSCHEDULER, &[0, 5, p]), 0)?;
    expect("setparam 0", t.call(set, &[0, p]), 0)?;
    param(&t, 1);
    expect("setparam 1", t.call(set, &[0, p]), -EINVAL)?;
    expect("setparam NULL", t.call(set, &[0, 0]), -EINVAL)?;
    t.put(8, &[0xEE; 4]);
    expect("getparam", t.call(snr::SCHED_GETPARAM, &[0, t.a(8)]), 0)?;
    kassert!(t.u32_at(8) == 0, "getparam priority {}", t.u32_at(8));
    expect("getparam NULL", t.call(snr::SCHED_GETPARAM, &[0, 0]), -EINVAL)?;
    expect("getparam pid -1", t.call(snr::SCHED_GETPARAM, &[u64::MAX, t.a(8)]), -EINVAL)?;
    expect("getparam no such pid", t.call(snr::SCHED_GETPARAM, &[NOBODY, t.a(8)]), -ESRCH)?;
    expect("getparam bad pointer", t.call(snr::SCHED_GETPARAM, &[0, KERNEL_ADDR]), -EFAULT)?;
    for (pol, max, min) in [(0u64, 0i64, 0i64), (1, 99, 1), (2, 99, 1), (3, 0, 0), (5, 0, 0), (6, 0, 0), (7, 0, 0), (4, -EINVAL, -EINVAL), (8, -EINVAL, -EINVAL), (u64::MAX, -EINVAL, -EINVAL)] {
        expect(&format!("priority max of {pol}"), t.call(snr::SCHED_GET_PRIORITY_MAX, &[pol]), max)?;
        expect(&format!("priority min of {pol}"), t.call(snr::SCHED_GET_PRIORITY_MIN, &[pol]), min)?;
    }
    // Reset-on-fork: set with the flag, read back; a normal user cannot clear it.
    let mut u = T::new("sc6-policy-rof", 1000)?;
    let q = u.a(0);
    u.put(0, &0i32.to_le_bytes());
    expect("BATCH | RESET_ON_FORK", u.call(nr::SCHED_SETSCHEDULER, &[0, 0x4000_0003, q]), 0)?;
    expect("getscheduler with the flag", u.call(snr::SCHED_GETSCHEDULER, &[0]), 0x4000_0003)?;
    expect("clearing the flag", u.call(nr::SCHED_SETSCHEDULER, &[0, 3, q]), -EPERM)?;
    expect("setparam keeps the flag", u.call(set, &[0, q]), 0)?;
    expect("still with the flag", u.call(snr::SCHED_GETSCHEDULER, &[0]), 0x4000_0003)?;
    // Root: may leave SCHED_IDLE and change another user's process; FIFO stays refused.
    let mut root = T::new("sc6-policy-root", 0)?;
    let rp = root.a(0);
    root.put(0, &0i32.to_le_bytes());
    expect("a user changes root's policy", t.call(nr::SCHED_SETSCHEDULER, &[root.pid(), 3, t.a(16)]), -EPERM)?;
    expect("root takes the caller out of SCHED_IDLE", root.call(nr::SCHED_SETSCHEDULER, &[t.pid(), 0, rp]), 0)?;
    expect("caller is SCHED_OTHER", t.call(snr::SCHED_GETSCHEDULER, &[0]), 0)?;
    root.put(0, &50i32.to_le_bytes());
    expect("root asks for SCHED_FIFO 50 (root alone is no grant, B4)", root.call(nr::SCHED_SETSCHEDULER, &[0, 1, rp]), -EPERM)?;
    // SC-7a: the program in front may: FIFO/RR run in the Interactive class, getparam reports the priority.
    let prev = sched::interactive::focus().unwrap_or(0);
    kassert!(sched::interactive::set_focus(root.pid()).is_ok(), "the held process has no thread in its group");
    let r = (|| -> TestResult {
        kassert!(sched::thread_stats().iter().any(|s| s.id == root.tid() && s.class == sched::Class::Interactive && s.group == root.pid()), "not Interactive in front");
        expect("root in front asks for SCHED_FIFO 50", root.call(nr::SCHED_SETSCHEDULER, &[0, 1, rp]), 0)?;
        expect("getscheduler FIFO", root.call(snr::SCHED_GETSCHEDULER, &[0]), 1)?;
        root.put(8, &[0xEE; 4]);
        expect("getparam FIFO", root.call(snr::SCHED_GETPARAM, &[0, root.a(8)]), 0)?;
        kassert!(root.u32_at(8) == 50, "getparam priority {}", root.u32_at(8));
        root.put(0, &70i32.to_le_bytes());
        expect("setparam 70 in front", root.call(set, &[0, rp]), 0)?;
        expect("SCHED_RR 70 in front", root.call(nr::SCHED_SETSCHEDULER, &[0, 2, rp]), 0)?;
        expect("getscheduler RR", root.call(snr::SCHED_GETSCHEDULER, &[0]), 2)?;
        expect("a user changes root's RR thread", t.call(nr::SCHED_SETSCHEDULER, &[root.pid(), 1, rp]), -EPERM)?;
        Ok(())
    })();
    let _ = sched::interactive::set_focus(prev).or_else(|_| sched::interactive::set_focus(0));
    r?;
    // Out of focus: the policy stays (the thread runs Normal), lowering is allowed, raising and changing are not.
    kassert!(sched::thread_stats().iter().any(|s| s.id == root.tid() && s.class == sched::Class::Normal && s.policy == sched::Policy::Rr && s.rt_prio == 70), "not back to Normal");
    root.put(0, &20i32.to_le_bytes());
    expect("setparam 20 out of focus (lowering)", root.call(set, &[0, rp]), 0)?;
    root.put(0, &21i32.to_le_bytes());
    expect("setparam 21 out of focus (raising)", root.call(set, &[0, rp]), -EPERM)?;
    expect("RR -> FIFO out of focus", root.call(nr::SCHED_SETSCHEDULER, &[0, 1, rp]), -EPERM)?;
    root.put(0, &0i32.to_le_bytes());
    expect("back to SCHED_OTHER", root.call(nr::SCHED_SETSCHEDULER, &[0, 0, rp]), 0)?;
    root.put(8, &[0xEE; 4]);
    expect("getparam OTHER", root.call(snr::SCHED_GETPARAM, &[0, root.a(8)]), 0)?;
    kassert!(root.u32_at(8) == 0, "getparam priority {}", root.u32_at(8));
    Ok(())
}

ktest!("sc.sched.times_rusage", Safe, times_rusage);
fn times_rusage() -> TestResult {
    let mut t = T::new("sc6-times", 1000)?;
    let (times, rusage) = (snr::TIMES, snr::GETRUSAGE);
    let before = sched::cputime::clock_t(time::now_ns()) as i64;
    let r = t.call(times, &[t.a(0)]);
    kassert!(r >= before && r <= before + 100, "times() = {r}, ticks since boot {before}");
    kassert!(t.get(16, 16) == [0u8; 16], "children's times not zero");
    let r2 = t.call(times, &[0]);
    kassert!(r2 >= r && r2 <= r + 100, "times(NULL) = {r2} after {r}");
    expect("times(bad)", t.call(times, &[KERNEL_ADDR]), -EFAULT)?;
    for who in [2u64, (-2i64) as u64, 3] {
        expect(&format!("getrusage who {}", who as i64), t.call(rusage, &[who, t.a(0)]), -EINVAL)?;
    }
    expect("getrusage bad pointer", t.call(rusage, &[0, KERNEL_ADDR]), -EFAULT)?;
    t.put(0, &[0xEE; 144]);
    expect("getrusage CHILDREN", t.call(rusage, &[u64::MAX, t.a(0)]), 0)?;
    kassert!(t.get(0, 144) == [0u8; 144], "RUSAGE_CHILDREN of a process without children is not all zero");
    expect("getrusage SELF", t.call(rusage, &[0, t.a(0)]), 0)?;
    kassert!(t.u64_at(32) > 0, "ru_maxrss 0 with memory resident");
    kassert!(t.u64_at(8) < 1_000_000 && t.u64_at(24) < 1_000_000, "tv_usec out of range");
    expect("getrusage THREAD", t.call(rusage, &[1, t.a(0)]), 0)?;
    // CPU clocks: PROF/VIRT/SCHED of the caller, resolutions, other processes, sleeps.
    let ts = |t: &T, at: u64| t.u64_at(at) * SEC + t.u64_at(at + 8);
    for (id, what) in [(2i64, "PROCESS"), (3, "THREAD"), (-8, "PROF"), (-7, "VIRT"), (-6, "SCHED"), (-2, "thread SCHED")] {
        expect(&format!("clock_gettime {what}"), t.call(nr::CLOCK_GETTIME, &[id as u64, t.a(0)]), 0)?;
        kassert!(t.u64_at(8) < SEC, "{what}: tv_nsec {}", t.u64_at(8));
    }
    t.call(nr::CLOCK_GETTIME, &[(-8i64) as u64, t.a(0)]);
    let prof = ts(&t, 0);
    t.call(nr::CLOCK_GETTIME, &[(-7i64) as u64, t.a(16)]);
    let virt = ts(&t, 16);
    kassert!(virt <= prof, "VIRT {virt} > PROF {prof}");
    for (id, res) in [(2i64, 1u64), (3, 1), (-6, 1), (-8, 1_000_000), (-7, 1_000_000)] {
        expect(&format!("clock_getres {id}"), t.call(nr::CLOCK_GETRES, &[id as u64, t.a(0)]), 0)?;
        kassert!(t.u64_at(0) == 0 && t.u64_at(8) == res, "clock {id}: resolution {} ns, want {res}", t.u64_at(8));
    }
    let other = T::new("sc6-times-other", 1000)?;
    let thread_clock_of = |pid: u64| (((!pid as i64) << 3) | 6) as u64;
    let process_clock_of = |pid: u64| (((!pid as i64) << 3) | 2) as u64;
    expect("thread clock of another process", t.call(nr::CLOCK_GETTIME, &[thread_clock_of(other.pid()), t.a(0)]), -EINVAL)?;
    expect("process clock of another process", t.call(nr::CLOCK_GETTIME, &[process_clock_of(other.pid()), t.a(0)]), 0)?;
    expect("process clock of nobody", t.call(nr::CLOCK_GETTIME, &[process_clock_of(NOBODY), t.a(0)]), -EINVAL)?;
    expect("own thread clock by pid", t.call(nr::CLOCK_GETTIME, &[thread_clock_of(t.pid()), t.a(0)]), 0)?;
    t.put(32, &[0u8; 16]);
    expect("sleep on CLOCK_THREAD_CPUTIME_ID", t.call(nr::CLOCK_NANOSLEEP, &[3, 0, t.a(32), 0]), -EOPNOTSUPP)?;
    expect("sleep on the own thread clock", t.call(nr::CLOCK_NANOSLEEP, &[(-2i64) as u64, 0, t.a(32), 0]), -EINVAL)?;
    expect("sleep 0 on the process clock", t.call(nr::CLOCK_NANOSLEEP, &[2, 0, t.a(32), 0]), 0)?;
    expect("sleep until 0 (past) on the process clock", t.call(nr::CLOCK_NANOSLEEP, &[2, 1, t.a(32), 0]), 0)?;
    expect("sleep on nobody's process clock", t.call(nr::CLOCK_NANOSLEEP, &[process_clock_of(NOBODY), 0, t.a(32), 0]), -EINVAL)?;
    Ok(())
}

// ---------------------------------------------------------------------------------------------------------
// Real programs

fn have(path: &str) -> bool {
    vfs::root_ns().lookup(path).is_ok()
}

fn run(path: &str, argv: &[&str]) -> Result<(Arc<Process>, Arc<proc::stdio::Capture>), String> {
    let out = proc::stdio::Capture::new(1 << 20);
    let env = ["HOME=/", "PATH=/bin", "ABI_TEST=1"];
    let mut o = proc::SpawnOptions::new(argv, &env, policy::ProgramId(1));
    o.stdin = Some(proc::stdio::Input::new(Vec::new()) as Arc<dyn vfs::File>);
    o.stdout = Some(out.clone() as Arc<dyn vfs::File>);
    o.stderr = Some(out.clone() as Arc<dyn vfs::File>);
    let p = proc::spawn_with(path, &o).map_err(|e| format!("spawn {path}: {e}"))?;
    Ok((p, out))
}

fn text(c: &proc::stdio::Capture) -> String {
    String::from_utf8_lossy(&c.contents()).into_owned()
}

fn finish(p: &Arc<Process>, out: &proc::stdio::Capture, ns: u64) -> Result<i32, String> {
    match p.wait(Some(ns)) {
        Some(c) => Ok(c),
        None => {
            p.kill(9);
            let _ = p.wait(Some(10 * SEC));
            Err(format!("{} did not end within {} s; output so far:\n{}", p.name, ns / SEC, text(out)))
        }
    }
}

/// Lines of `got` that differ from CachyOS's `want` (lines starting with a known prefix are skipped).
fn diff(got: &str, want: &str, known: &str) -> Vec<String> {
    let known: Vec<&str> = known.lines().filter(|l| !l.starts_with('#') && !l.trim().is_empty()).filter_map(|l| l.split(": ").next()).collect();
    let g: Vec<&str> = got.lines().collect();
    let mut d = Vec::new();
    for (i, w) in want.lines().enumerate() {
        let gl = g.get(i).copied().unwrap_or("<missing>");
        if gl != w && !known.iter().any(|k| w.starts_with(k)) {
            d.push(format!("line {}: linux '{w}' got '{gl}'", i + 1));
        }
    }
    if g.len() > want.lines().count() {
        d.push(format!("{} extra lines", g.len() - want.lines().count()));
    }
    d
}

ktest!("sc.sched.abi", Safe, abi);
fn abi() -> TestResult {
    if !have("/bin/abi-sched") {
        return ktest::skip("/bin/abi-sched is not in this image (the ABI-diff programs need UL-1's musl sysroot)");
    }
    let (p, out) = run("/bin/abi-sched", &["abi-sched"])?;
    let code = finish(&p, &out, 60 * SEC)?;
    let d = diff(&text(&out), include_str!("../../../user/abi-tests/expected/sched.txt"), include_str!("../../../user/abi-tests/expected/sched.known"));
    kassert!(d.is_empty() && code == 0, "abi-sched differs from CachyOS (exit {code}):\n{}", d.join("\n"));
    Ok(())
}

// fork inherits affinity, nice and policy; the parent changes the child's by pid and the child sees it; wait4
// rusage, RUSAGE_CHILDREN and times() agree (`expected/sched-child.txt`, recorded on CachyOS). Needs fork (AB-4),
// `copy_attrs` at fork (SC-6 CR 2) and the proc side of SC-6 (CR 1): skipped while fork is missing.
ktest!("sc.sched.child", Safe, child);
fn child() -> TestResult {
    if !have("/bin/abi-sched") {
        return ktest::skip("/bin/abi-sched is not in this image (the ABI-diff programs need UL-1's musl sysroot)");
    }
    let (p, out) = run("/bin/abi-sched", &["abi-sched", "child"])?;
    let code = finish(&p, &out, 60 * SEC)?;
    let got = text(&out);
    if let Some(l) = got.lines().find(|l| l.starts_with("fork = -") || l.starts_with("pipe = -")) {
        klog::event!(Info, "sc", "sched.child.missing", line = l);
        return ktest::skip("no pipe or fork yet (FS pipes, AB-4 fork); then copy_attrs at fork (SC-6 CR 2)");
    }
    let d = diff(&got, include_str!("../../../user/abi-tests/expected/sched-child.txt"), "");
    kassert!(d.is_empty() && code == 0, "abi-sched child differs from CachyOS (exit {code}):\n{}", d.join("\n"));
    Ok(())
}

// A real program burns user time, then system time (getppid in a loop): its own view (times, getrusage, CPU
// clocks) equals CachyOS's relations, and `sched`'s view of its thread shows both parts while it runs.
ktest!("sc.sched.burn", Safe, burn);
fn burn() -> TestResult {
    if !have("/bin/abi-sched") {
        return ktest::skip("/bin/abi-sched is not in this image (the ABI-diff programs need UL-1's musl sysroot)");
    }
    let (p, out) = run("/bin/abi-sched", &["abi-sched", "burn"])?;
    // Watch the thread while it runs: the largest user/system parts seen.
    let (mut user, mut system) = (0u64, 0u64);
    let end = time::now_ns() + 60 * SEC;
    while !p.is_finished() && time::now_ns() < end {
        if let Some(s) = sched::thread_stats().into_iter().find(|s| s.id == p.thread_id()) {
            user = user.max(s.user_ns);
            system = system.max(s.system_ns);
        }
        sched::sleep_ms(5);
    }
    let code = finish(&p, &out, 10 * SEC)?;
    let got = text(&out);
    klog::event!(Info, "sc", "sched.burn", user_ms = user / 1_000_000, system_ms = system / 1_000_000, code = code, marked = sched::cputime::user_marked());
    if !sched::cputime::user_marked() {
        return ktest::skip("proc does not mark user mode yet (SC-6 change request 1: user_enter/user_exit around execute)");
    }
    let d = diff(&got, include_str!("../../../user/abi-tests/expected/sched-burn.txt"), "");
    kassert!(d.is_empty() && code == 0, "abi-sched burn differs from CachyOS (exit {code}):\n{}", d.join("\n"));
    kassert!(user > 100_000_000 && system > 20_000_000, "sched saw user {} ms, system {} ms of a program that burns ~300 ms in each", user / 1_000_000, system / 1_000_000);
    Ok(())
}

// wait4's rusage, getrusage(RUSAGE_CHILDREN) and times()' children part after a real child that burned CPU
// time (a child of a held test process: no fork needed). Needs the proc side of SC-6 (integration patch).
ktest!("sc.sched.children_times", Safe, children_times);
fn children_times() -> TestResult {
    if !have("/bin/abi-sched") {
        return ktest::skip("/bin/abi-sched is not in this image (the ABI-diff programs need UL-1's musl sysroot)");
    }
    let mut t = T::new("sc6-parent", 1000)?;
    let out = proc::stdio::Capture::new(1 << 16);
    let env = ["HOME=/", "PATH=/bin"];
    let argv = ["abi-sched", "burn"];
    let parent = t.p.clone();
    let mut o = proc::SpawnOptions::new(&argv, &env, policy::ProgramId(1));
    o.parent = Some(&parent);
    o.stdout = Some(out.clone() as Arc<dyn vfs::File>);
    o.stderr = Some(out.clone() as Arc<dyn vfs::File>);
    let child = proc::spawn_with("/bin/abi-sched", &o).map_err(|e| format!("spawn: {e}"))?;
    let end = time::now_ns() + 60 * SEC;
    let got = loop {
        // wait4(-1, &status, WNOHANG, &rusage)
        let r = t.call(nr::WAIT4, &[u64::MAX, t.a(0), 1, t.a(64)]);
        if r != 0 {
            break r;
        }
        if time::now_ns() > end {
            child.kill(9);
            return Err(String::from("the child did not end within 60 s"));
        }
        sched::sleep_ms(10);
    };
    expect("wait4 collects the child", got, child.pid as i64)?;
    if !sched::cputime::user_marked() {
        return ktest::skip("proc does not keep CPU times of ended threads and children yet (SC-6 change request 1)");
    }
    let tv = |t: &T, at: u64| t.u64_at(at) * SEC + t.u64_at(at + 8) * 1000;
    let (u, s) = (tv(&t, 64), tv(&t, 80));
    klog::event!(Info, "sc", "sched.children_times", user_ms = u / 1_000_000, system_ms = s / 1_000_000);
    kassert!(u >= 250_000_000 && u + s >= 550_000_000, "wait4 rusage: user {} ms, system {} ms for a child that burned ~600 ms", u / 1_000_000, s / 1_000_000);
    expect("getrusage(RUSAGE_CHILDREN)", t.call(snr::GETRUSAGE, &[u64::MAX, t.a(256)]), 0)?;
    let (cu, cs) = (tv(&t, 256), tv(&t, 272));
    kassert!(cu == u && cs == s, "RUSAGE_CHILDREN {} / {} ms differs from wait4's {} / {} ms", cu / 1_000_000, cs / 1_000_000, u / 1_000_000, s / 1_000_000);
    t.call(snr::TIMES, &[t.a(512)]);
    kassert!(t.u64_at(528) == u / 10_000_000 && t.u64_at(536) == s / 10_000_000, "times(): cutime {} cstime {} ticks", t.u64_at(528), t.u64_at(536));
    Ok(())
}

// ---- FS-12: ioprio_set / ioprio_get (linux-6.6/block/ioprio.c)

ktest!("fs.ioprio.calls", Safe, ioprio_calls);
fn ioprio_calls() -> TestResult {
    use block::prio::{value, IOPRIO_CLASS_BE, IOPRIO_CLASS_IDLE, IOPRIO_CLASS_NONE, IOPRIO_CLASS_RT};
    let mut t = T::new("fs12-ioprio", TEST_UID)?;
    let (set, get) = (snr::IOPRIO_SET, snr::IOPRIO_GET);
    let idle = value(IOPRIO_CLASS_IDLE, 0) as u64;
    let kt = t.th.kthread();
    expect("get default", t.call(get, &[1, 0]), 0)?;
    expect("get which 0", t.call(get, &[0, 0]), -EINVAL)?;
    expect("get which 4", t.call(get, &[4, 0]), -EINVAL)?;
    expect("get no such pid", t.call(get, &[1, NOBODY]), -ESRCH)?;
    expect("set which 0", t.call(set, &[0, 0, idle]), -EINVAL)?;
    expect("set NONE level 4", t.call(set, &[1, 0, value(IOPRIO_CLASS_NONE, 4) as u64]), -EINVAL)?;
    expect("set class 4", t.call(set, &[1, 0, value(4, 0) as u64]), -EINVAL)?;
    expect("set class 7 checked before which", t.call(set, &[9, 0, u64::MAX]), -EINVAL)?;
    expect("set RT without CAP_SYS_ADMIN", t.call(set, &[1, 0, value(IOPRIO_CLASS_RT, 0) as u64]), -EPERM)?;
    expect("set no such pid", t.call(set, &[1, NOBODY, idle]), -ESRCH)?;
    expect("set pid -1", t.call(set, &[1, u64::MAX, idle]), -ESRCH)?;
    expect("set IDLE", t.call(set, &[1, 0, idle]), 0)?;
    expect("get IDLE", t.call(get, &[1, 0]), idle as i64)?;
    kassert!(block::prio::raw(kt) == idle as i32, "stored for the thread");
    kassert!(block::prio::map(sched::Class::Normal, block::prio::raw(kt)) == block::prio::IoPriority::Background, "IDLE requests go as Background");
    expect("get PGRP (the set value)", t.call(get, &[2, 0]), idle as i64)?;
    expect("get USER (the set value)", t.call(get, &[3, 0]), idle as i64)?;
    expect("set BE 8 (level 0, hint bits unchecked)", t.call(set, &[1, 0, value(IOPRIO_CLASS_BE, 8) as u64]), 0)?;
    expect("set NONE", t.call(set, &[1, 0, 0]), 0)?;
    expect("get NONE again", t.call(get, &[1, 0]), 0)?;
    // PGRP/USER give the effective value: BE with level (nice + 20) / 5.
    expect("get PGRP derived", t.call(get, &[2, 0]), value(IOPRIO_CLASS_BE, 4) as i64)?;
    expect("get USER derived", t.call(get, &[3, 0]), value(IOPRIO_CLASS_BE, 4) as i64)?;
    expect("get PGRP -1", t.call(get, &[2, u64::MAX]), -ESRCH)?;
    expect("set USER", t.call(set, &[3, 0, idle]), 0)?;
    expect("get after USER set", t.call(get, &[1, 0]), idle as i64)?;
    // Another owner: EPERM for a user; root (CAP_SYS_ADMIN) may set another user's process, and RT.
    let mut root = T::new("fs12-ioprio-root", 0)?;
    expect("set root's process", t.call(set, &[1, root.pid(), idle]), -EPERM)?;
    expect("root sets the user's process", root.call(set, &[1, t.pid(), 0]), 0)?;
    expect("root sets RT", root.call(set, &[1, 0, value(IOPRIO_CLASS_RT, 0) as u64]), 0)?;
    expect("root back to NONE", root.call(set, &[1, 0, 0]), 0)?;
    expect("set NONE again", t.call(set, &[1, 0, 0]), 0)?;
    kassert!(block::prio::raw(kt) == 0, "NONE removes the entry");
    Ok(())
}
