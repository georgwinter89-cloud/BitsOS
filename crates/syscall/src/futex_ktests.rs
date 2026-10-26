//! SC-5 ktests: the futex syscalls on a real process address space.
//!
//! Until AB-4 brings clone, a process has one thread; these tests play a process's **threads** with helper
//! kernel threads that load the process's address space (`AddressSpace::activate`, as the process's own thread
//! does) and call the syscall entry (`do_futex`, `do_futex_waitv`) the way the dispatcher would. So waits,
//! wakes, requeues and robust lists run on user memory with the real key, word and fault paths, several
//! "threads" of one address space at once. Real programs: `sc.futex.abi` (abi-futex transcript),
//! `sc.futex.procs64` (64 processes on a shared file mapping) and `sc.futex.pthread` (musl pthreads, once
//! clone exists).
//!
//! All `Safe` and rerunnable: each test makes its own processes and kills them at the end; nothing waits
//! without a deadline (a helper that does not finish fails the test after a generous limit).

use crate::futex::*;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicI64, AtomicU32, AtomicU64, Ordering};
use ksync::user_word::{self, WordOp};
use ktest::{TestResult, kassert, ktest};
use proc::Process;
use uapi::errno::*;

const SEC: u64 = 1_000_000_000;
const MS: u64 = 1_000_000;
const PENDING: i64 = i64::MIN;
/// `mov eax, 231; xor edi, edi; syscall` (never runs: the process stays held).
const EXIT0: [u8; 9] = [0xb8, 0xe7, 0, 0, 0, 0x31, 0xff, 0x0f, 0x05];
const PAGES: u64 = 16;
const PRIV: u64 = FUTEX_PRIVATE_FLAG as u64;
const ANY: u64 = u32::MAX as u64;

static RUN: AtomicU32 = AtomicU32::new(0);

/// A held process with `PAGES` pages of anonymous memory (filled) whose futex calls helper threads make.
struct Fx {
    p: Arc<Process>,
    mem: u64,
}

struct Helper {
    h: sched::JoinHandle,
    r: Arc<AtomicI64>,
}

impl Helper {
    fn id(&self) -> u64 {
        self.h.id()
    }
    /// The result once it finished (None after `ns`).
    fn result(&self, ns: u64) -> Option<i64> {
        if !self.h.join_timeout(ns) {
            return None;
        }
        Some(self.r.load(Ordering::Acquire))
    }
    fn running(&self) -> bool {
        !self.h.is_finished()
    }
}

impl Fx {
    fn new(name: &str) -> Result<Fx, String> {
        let image = crate::ktests::elf(&EXIT0);
        let argv = [name];
        let mut o = proc::SpawnOptions::new(&argv, &[], policy::ProgramId(1));
        o.held = true;
        let p = proc::spawn_image_with(name, &image, &o).map_err(|e| format!("spawn {name}: {e}"))?;
        let mem = p.mm.sys_mmap(0, PAGES * 4096, 3, 0x22, 0).map_err(|e| format!("mmap: {e:?}"))?;
        p.mm.copy_to_user(mem, &[0u8; (PAGES * 4096) as usize]).map_err(|e| format!("fill: {e:?}"))?;
        Ok(Fx { p, mem })
    }
    fn a(&self, off: u64) -> u64 {
        self.mem + off
    }
    fn put32(&self, off: u64, v: u32) {
        let _ = self.p.mm.copy_to_user(self.mem + off, &v.to_le_bytes());
    }
    fn put64(&self, off: u64, v: u64) {
        let _ = self.p.mm.copy_to_user(self.mem + off, &v.to_le_bytes());
    }
    fn get32(&self, off: u64) -> u32 {
        let mut b = [0u8; 4];
        let _ = self.p.mm.copy_from_user(&mut b, self.mem + off);
        u32::from_le_bytes(b)
    }
    /// A "thread" of the process: a kernel thread with the process's address space loaded runs `f`.
    fn thread(&self, name: &str, f: impl FnOnce(&Arc<Process>, u64) -> i64 + Send + 'static) -> Result<Helper, String> {
        let r = Arc::new(AtomicI64::new(PENDING));
        let (p, mem, r2) = (self.p.clone(), self.mem, r.clone());
        let h = sched::spawn(name, sched::Class::Normal, move || {
            p.mm.activate();
            let v = f(&p, mem);
            r2.store(v, Ordering::Release);
        })
        .map_err(|e| format!("spawn {name}: {e:?}"))?;
        Ok(Helper { h, r })
    }
    /// A helper that makes one futex call.
    fn call(&self, name: &str, args: [u64; 6]) -> Result<Helper, String> {
        self.thread(name, move |p, _| do_futex(p, args))
    }
    /// The futex call from the test thread itself (operations that touch no word: WAKE, REQUEUE).
    fn now(&self, args: [u64; 6]) -> i64 {
        do_futex(&self.p, args)
    }
}

impl Drop for Fx {
    fn drop(&mut self) {
        self.p.kill(9);
        let _ = self.p.wait(Some(10 * SEC));
    }
}

/// Waits until `n` more futex waiters than `base` are queued.
fn queued(base: usize, n: usize) -> TestResult {
    let end = time::now_ns() + 10 * SEC;
    while sched::futex::waiters() < base + n {
        kassert!(time::now_ns() < end, "only {} of {n} waiters queued after 10 s", sched::futex::waiters().saturating_sub(base));
        sched::sleep_ns(MS);
    }
    Ok(())
}

fn expect(what: &str, got: i64, want: i64) -> TestResult {
    kassert!(got == want, "{what}: got {got}, want {want}");
    Ok(())
}

fn done(h: &Helper, what: &str, want: i64) -> TestResult {
    match h.result(10 * SEC) {
        Some(v) => expect(what, v, want),
        None => Err(format!("{what}: still waiting after 10 s")),
    }
}

fn args(uaddr: u64, op: u64, val: u64, utime: u64, uaddr2: u64, val3: u64) -> [u64; 6] {
    [uaddr, op, val, utime, uaddr2, val3]
}

// ---------------------------------------------------------------------------------------------------------

ktest!("sc.futex.wait_wake", Safe, wait_wake);
fn wait_wake() -> TestResult {
    let t = Fx::new("sc-fx-ww")?;
    let base = sched::futex::waiters();
    // One waiter, one wake.
    let w = t.call("sc-fx-w", args(t.a(0), FUTEX_WAIT as u64 | PRIV, 0, 0, 0, 0))?;
    queued(base, 1)?;
    expect("wake one", t.now(args(t.a(0), FUTEX_WAKE as u64 | PRIV, 1, 0, 0, 0)), 1)?;
    done(&w, "waiter", 0)?;
    // nr_wake 0 still wakes one (Linux's `++ret >= nr_wake`), INT_MAX the rest.
    let ws: Vec<Helper> = (0..3).map(|i| t.call(&format!("sc-fx-w{i}"), args(t.a(0), FUTEX_WAIT as u64 | PRIV, 0, 0, 0, 0))).collect::<Result<_, _>>()?;
    queued(base, 3)?;
    expect("wake 0", t.now(args(t.a(0), FUTEX_WAKE as u64 | PRIV, 0, 0, 0, 0)), 1)?;
    expect("wake INT_MAX", t.now(args(t.a(0), FUTEX_WAKE as u64 | PRIV, i32::MAX as u64, 0, 0, 0)), 2)?;
    for w in &ws {
        done(w, "waiter of 3", 0)?;
    }
    // Bitsets: a waiter on bit 1 is not woken by bit 0.
    let w = t.call("sc-fx-wb", args(t.a(4), FUTEX_WAIT_BITSET as u64 | PRIV, 0, 0, 0, 0b10))?;
    queued(base, 1)?;
    expect("wake bitset 01", t.now(args(t.a(4), FUTEX_WAKE_BITSET as u64 | PRIV, 9, 0, 0, 0b01)), 0)?;
    expect("wake bitset 10", t.now(args(t.a(4), FUTEX_WAKE_BITSET as u64 | PRIV, 9, 0, 0, 0b10)), 1)?;
    done(&w, "bitset waiter", 0)?;
    // A shared wait on anonymous memory is not met by a private wake (different key kinds), only by a shared one.
    let w = t.call("sc-fx-ws", args(t.a(8), FUTEX_WAIT as u64, 0, 0, 0, 0))?;
    queued(base, 1)?;
    expect("private wake of a shared waiter", t.now(args(t.a(8), FUTEX_WAKE as u64 | PRIV, 1, 0, 0, 0)), 0)?;
    expect("shared wake", t.now(args(t.a(8), FUTEX_WAKE as u64, 1, 0, 0, 0)), 1)?;
    done(&w, "shared waiter", 0)?;
    // Value mismatch: EAGAIN at once.
    t.put32(12, 3);
    let w = t.call("sc-fx-wm", args(t.a(12), FUTEX_WAIT as u64 | PRIV, 4, 0, 0, 0))?;
    done(&w, "mismatch", -EAGAIN)?;
    kassert!(sched::futex::waiters() == base, "waiters left behind");
    Ok(())
}

ktest!("sc.futex.timeouts", Safe, timeouts);
fn timeouts() -> TestResult {
    let t = Fx::new("sc-fx-to")?;
    // struct timespec at 0x100: 20 ms relative.
    t.put64(0x100, 0);
    t.put64(0x108, 20 * MS);
    let s = time::now_ns();
    let w = t.call("sc-fx-rel", args(t.a(0), FUTEX_WAIT as u64 | PRIV, 0, t.a(0x100), 0, 0))?;
    done(&w, "relative 20 ms", -ETIMEDOUT)?;
    kassert!(time::now_ns() - s >= 20 * MS, "returned before 20 ms");
    // Absolute monotonic 20 ms from now (WAIT_BITSET).
    let at = time::now_ns() + 20 * MS;
    t.put64(0x110, at / SEC);
    t.put64(0x118, at % SEC);
    let w = t.call("sc-fx-abs", args(t.a(0), FUTEX_WAIT_BITSET as u64 | PRIV, 0, t.a(0x110), 0, ANY))?;
    done(&w, "absolute monotonic", -ETIMEDOUT)?;
    kassert!(time::now_ns() >= at, "absolute monotonic returned early");
    // Absolute CLOCK_REALTIME.
    let real = time::wall::realtime_ns().unwrap_or_else(time::now_ns) + 20 * MS;
    t.put64(0x120, real / SEC);
    t.put64(0x128, real % SEC);
    let w = t.call("sc-fx-real", args(t.a(0), (FUTEX_WAIT_BITSET | FUTEX_CLOCK_REALTIME) as u64 | PRIV, 0, t.a(0x120), 0, ANY))?;
    done(&w, "absolute realtime", -ETIMEDOUT)?;
    // A wake before the timeout wins.
    t.put64(0x130, 10);
    t.put64(0x138, 0);
    let base = sched::futex::waiters();
    let w = t.call("sc-fx-early", args(t.a(0), FUTEX_WAIT as u64 | PRIV, 0, t.a(0x130), 0, 0))?;
    queued(base, 1)?;
    expect("wake", t.now(args(t.a(0), FUTEX_WAKE as u64 | PRIV, 1, 0, 0, 0)), 1)?;
    done(&w, "woken before its 10 s timeout", 0)?;
    // futex_waitv with a timeout.
    let at = time::now_ns() + 20 * MS;
    t.put64(0x140, at / SEC);
    t.put64(0x148, at % SEC);
    write_waitv(&t, 0x200, &[(0, t.a(0), FUTEX_32 | FUTEX_PRIVATE_FLAG), (0, t.a(4), FUTEX_32)]);
    let w = t.thread("sc-fx-wv", |p, mem| do_futex_waitv(p, [mem + 0x200, 2, 0, mem + 0x140, 1, 0]))?;
    done(&w, "waitv timeout", -ETIMEDOUT)?;
    Ok(())
}

fn write_waitv(t: &Fx, off: u64, items: &[(u64, u64, u32)]) {
    for (i, (val, addr, flags)) in items.iter().enumerate() {
        let o = off + i as u64 * 24;
        t.put64(o, *val);
        t.put64(o + 8, *addr);
        t.put64(o + 16, *flags as u64);
    }
}

ktest!("sc.futex.requeue", Safe, requeue);
fn requeue() -> TestResult {
    let t = Fx::new("sc-fx-rq")?;
    let base = sched::futex::waiters();
    t.put32(0, 7);
    let ws: Vec<Helper> = (0..6).map(|i| t.call(&format!("sc-fx-rq{i}"), args(t.a(0), FUTEX_WAIT as u64 | PRIV, 7, 0, 0, 0))).collect::<Result<_, _>>()?;
    queued(base, 6)?;
    // CMP_REQUEUE reads the word under both bucket locks: from a "thread" of the process.
    let r = t.call("sc-fx-cmp", args(t.a(0), FUTEX_CMP_REQUEUE as u64 | PRIV, 1, 3, t.a(64), 8))?;
    done(&r, "cmp_requeue mismatch", -EAGAIN)?;
    let r = t.call("sc-fx-cmp2", args(t.a(0), FUTEX_CMP_REQUEUE as u64 | PRIV, 1, 3, t.a(64), 7))?;
    done(&r, "cmp_requeue: 1 woken + 3 moved", 4)?;
    sched::sleep_ns(20 * MS);
    let finished = ws.iter().filter(|w| !w.running()).count();
    kassert!(finished == 1, "{finished} waiters finished after the requeue, want 1");
    // The moved ones wait on the second word now, the rest on the first.
    expect("wake the first word", t.now(args(t.a(0), FUTEX_WAKE as u64 | PRIV, i32::MAX as u64, 0, 0, 0)), 2)?;
    expect("wake the second word", t.now(args(t.a(64), FUTEX_WAKE as u64 | PRIV, i32::MAX as u64, 0, 0, 0)), 3)?;
    for w in &ws {
        done(w, "requeued waiter", 0)?;
    }
    // Plain REQUEUE (no word read): all moved, then woken there.
    let ws: Vec<Helper> = (0..4).map(|i| t.call(&format!("sc-fx-rp{i}"), args(t.a(0), FUTEX_WAIT as u64 | PRIV, 7, 0, 0, 0))).collect::<Result<_, _>>()?;
    queued(base, 4)?;
    expect("requeue all", t.now(args(t.a(0), FUTEX_REQUEUE as u64 | PRIV, 0, i32::MAX as u64, t.a(4), 0)), 4)?;
    expect("nothing left on the first", t.now(args(t.a(0), FUTEX_WAKE as u64 | PRIV, 9, 0, 0, 0)), 0)?;
    expect("all on the second", t.now(args(t.a(4), FUTEX_WAKE as u64 | PRIV, 9, 0, 0, 0)), 4)?;
    for w in &ws {
        done(w, "requeued waiter", 0)?;
    }
    expect("requeue negative", t.now(args(t.a(0), FUTEX_REQUEUE as u64 | PRIV, 1, u32::MAX as u64, t.a(4), 0)), -EINVAL)?;
    kassert!(sched::futex::waiters() == base, "waiters left behind");
    Ok(())
}

ktest!("sc.futex.wake_op", Safe, wake_op);
fn wake_op() -> TestResult {
    let t = Fx::new("sc-fx-wo")?;
    let base = sched::futex::waiters();
    t.put32(64, 5);
    let w1: Vec<Helper> = (0..2).map(|i| t.call(&format!("sc-fx-wo{i}"), args(t.a(0), FUTEX_WAIT as u64 | PRIV, 0, 0, 0, 0))).collect::<Result<_, _>>()?;
    let w2 = t.call("sc-fx-wo2", args(t.a(64), FUTEX_WAIT as u64 | PRIV, 5, 0, 0, 0))?;
    queued(base, 3)?;
    // FUTEX_OP(ADD, 1, CMP_EQ, 5): *a2 += 1 (5 -> 6), the old value 5 equals 5: wake 1 of a1 and 1 of a2.
    let enc: u64 = (1 << 28) | (1 << 12) | 5;
    let r = t.call("sc-fx-op", args(t.a(0), FUTEX_WAKE_OP as u64 | PRIV, 1, 1, t.a(64), enc))?;
    done(&r, "wake_op", 2)?;
    kassert!(t.get32(64) == 6, "the word is {}, want 6", t.get32(64));
    done(&w2, "a2 waiter", 0)?;
    // CMP_NE 6 against old 6: false, only a1.
    let enc: u64 = (1 << 28) | (1 << 24) | (1 << 12) | 6;
    let r = t.call("sc-fx-op2", args(t.a(0), FUTEX_WAKE_OP as u64 | PRIV, 1, 1, t.a(64), enc))?;
    done(&r, "wake_op without the second", 1)?;
    for w in &w1 {
        done(w, "a1 waiter", 0)?;
    }
    // The word on a page not yet touched (lazy fill through the fault path), then a read-only page: EFAULT.
    let fresh = t.p.mm.sys_mmap(0, 4096, 3, 0x22, 0).map_err(|e| format!("{e:?}"))?;
    let r = t.thread("sc-fx-opf", move |p, mem| do_futex(p, args(mem, FUTEX_WAKE_OP as u64 | PRIV, 1, 1, fresh, 3 << 12)))?;
    done(&r, "wake_op on an untouched page", 0)?;
    let mut b = [0u8; 4];
    let _ = t.p.mm.copy_from_user(&mut b, fresh);
    kassert!(u32::from_le_bytes(b) == 3, "untouched page word {}", u32::from_le_bytes(b));
    let ro = t.p.mm.sys_mmap(0, 4096, 1, 0x22, 0).map_err(|e| format!("{e:?}"))?;
    let r = t.thread("sc-fx-opr", move |p, mem| do_futex(p, args(mem, FUTEX_WAKE_OP as u64 | PRIV, 1, 1, ro, 3 << 12)))?;
    done(&r, "wake_op on a read-only page", -EFAULT)?;
    Ok(())
}

ktest!("sc.futex.waitv", Safe, waitv);
fn waitv() -> TestResult {
    let t = Fx::new("sc-fx-wv")?;
    let base = sched::futex::waiters();
    t.put32(8, 2);
    write_waitv(&t, 0x200, &[(0, t.a(0), FUTEX_32 | FUTEX_PRIVATE_FLAG), (0, t.a(4), FUTEX_32), (2, t.a(8), FUTEX_32 | FUTEX_PRIVATE_FLAG)]);
    let w = t.thread("sc-fx-wv1", |p, mem| do_futex_waitv(p, [mem + 0x200, 3, 0, 0, 0, 0]))?;
    queued(base, 3)?;
    expect("wake index 2", t.now(args(t.a(8), FUTEX_WAKE as u64 | PRIV, 1, 0, 0, 0)), 1)?;
    done(&w, "waitv woken by 2", 2)?;
    kassert!(sched::futex::waiters() == base, "waitv left entries queued");
    // The shared entry (index 1) needs a shared wake.
    let w = t.thread("sc-fx-wv2", |p, mem| do_futex_waitv(p, [mem + 0x200, 3, 0, 0, 0, 0]))?;
    queued(base, 3)?;
    expect("private wake of the shared entry", t.now(args(t.a(4), FUTEX_WAKE as u64 | PRIV, 1, 0, 0, 0)), 0)?;
    expect("shared wake", t.now(args(t.a(4), FUTEX_WAKE as u64, 1, 0, 0, 0)), 1)?;
    done(&w, "waitv woken by 1", 1)?;
    // A requeued entry: index 0 moved to another word, woken there.
    let w = t.thread("sc-fx-wv3", |p, mem| do_futex_waitv(p, [mem + 0x200, 3, 0, 0, 0, 0]))?;
    queued(base, 3)?;
    expect("requeue index 0", t.now(args(t.a(0), FUTEX_REQUEUE as u64 | PRIV, 0, 1, t.a(128), 0)), 1)?;
    expect("wake the target", t.now(args(t.a(128), FUTEX_WAKE as u64 | PRIV, 1, 0, 0, 0)), 1)?;
    done(&w, "waitv woken through a requeue", 0)?;
    // Mismatch at setup.
    t.put32(8, 3);
    let w = t.thread("sc-fx-wv4", |p, mem| do_futex_waitv(p, [mem + 0x200, 3, 0, 0, 0, 0]))?;
    done(&w, "waitv mismatch", -EAGAIN)?;
    kassert!(sched::futex::waiters() == base, "waiters left behind");
    Ok(())
}

ktest!("sc.futex.kill_interrupts", Safe, kill_interrupts);
fn kill_interrupts() -> TestResult {
    let t = Fx::new("sc-fx-kill")?;
    let base = sched::futex::waiters();
    let w = t.call("sc-fx-k", args(t.a(0), FUTEX_WAIT as u64 | PRIV, 0, 0, 0, 0))?;
    write_waitv(&t, 0x200, &[(0, t.a(16), FUTEX_32)]);
    let wv = t.thread("sc-fx-kv", |p, mem| do_futex_waitv(p, [mem + 0x200, 1, 0, 0, 0, 0]))?;
    queued(base, 2)?;
    t.p.kill(9);
    // What AB-4's kill will do for every thread of the process (its main thread is kicked by `kill` already).
    sched::kick_thread(w.id());
    sched::kick_thread(wv.id());
    done(&w, "futex wait of a killed process", -EINTR)?;
    done(&wv, "futex_waitv of a killed process", -EINTR)?;
    kassert!(sched::futex::waiters() == base, "waiters left behind");
    Ok(())
}

// Waits that end by their timeout leave nothing queued, also after a requeue moved them; a wake racing the
// timeout is counted exactly when the waiter reports it (Linux `futex_wait`: a dequeued waiter returns 0 even
// when its timer fired). Consistency only, no timing judged.
ktest!("sc.futex.no_stale_waiters", Safe, no_stale_waiters);
fn no_stale_waiters() -> TestResult {
    let t = Fx::new("sc-fx-stale")?;
    let base = sched::futex::waiters();
    // 8 timed waits on one word, all time out.
    t.put64(0x100, 0);
    t.put64(0x108, 10 * MS);
    let ws: Vec<Helper> =
        (0..8).map(|i| t.call(&format!("sc-fx-st{i}"), args(t.a(0), FUTEX_WAIT as u64 | PRIV, 0, t.a(0x100), 0, 0))).collect::<Result<_, _>>()?;
    for w in &ws {
        done(w, "timed wait", -ETIMEDOUT)?;
    }
    kassert!(sched::futex::waiters() == base, "{} waiters left after timeouts", sched::futex::waiters() - base);
    expect("wake after the timeouts", t.now(args(t.a(0), FUTEX_WAKE as u64 | PRIV, i32::MAX as u64, 0, 0, 0)), 0)?;
    // Requeued, then timed out where they were moved to (the waiter finds its entry in the second bucket).
    t.put64(0x110, 0);
    t.put64(0x118, 300 * MS);
    let ws: Vec<Helper> =
        (0..4).map(|i| t.call(&format!("sc-fx-sr{i}"), args(t.a(0), FUTEX_WAIT as u64 | PRIV, 0, t.a(0x110), 0, 0))).collect::<Result<_, _>>()?;
    queued(base, 4)?;
    let moved = t.now(args(t.a(0), FUTEX_REQUEUE as u64 | PRIV, 0, i32::MAX as u64, t.a(64), 0));
    for w in &ws {
        match w.result(10 * SEC) {
            // A waiter whose timer fired before the requeue reached it is not moved: both are fine.
            Some(v) => expect("requeued timed wait", v, -ETIMEDOUT)?,
            None => return Err("requeued timed wait: still waiting after 10 s".into()),
        }
    }
    kassert!((0..=4).contains(&moved), "requeue moved {moved}");
    kassert!(sched::futex::waiters() == base, "{} waiters left after requeue + timeout", sched::futex::waiters() - base);
    expect("wake the first word", t.now(args(t.a(0), FUTEX_WAKE as u64 | PRIV, i32::MAX as u64, 0, 0, 0)), 0)?;
    expect("wake the second word", t.now(args(t.a(64), FUTEX_WAKE as u64 | PRIV, i32::MAX as u64, 0, 0, 0)), 0)?;
    // A wake racing the timeout, 200 rounds: the wake's count equals the waiters that saw 0.
    t.put64(0x120, 0);
    let (mut woken, mut timed) = (0, 0);
    for round in 0..200u64 {
        t.put64(0x128, (round % 5) * 100_000);
        let w = t.call("sc-fx-race", args(t.a(128), FUTEX_WAIT as u64 | PRIV, 0, t.a(0x120), 0, 0))?;
        // Spin a little so the wake lands before, during or after the expiry.
        let until = time::now_ns() + (round % 7) * 50_000;
        while time::now_ns() < until {
            core::hint::spin_loop();
        }
        let n = t.now(args(t.a(128), FUTEX_WAKE as u64 | PRIV, 1, 0, 0, 0));
        let r = match w.result(10 * SEC) {
            Some(r) => r,
            None => return Err(format!("round {round}: waiter still waiting after 10 s")),
        };
        match (n, r) {
            (1, 0) => woken += 1,
            (0, r) if r == -ETIMEDOUT => timed += 1,
            // (0, 0) would be a wait that returned "woken" although no wake took it; (1, ETIMEDOUT) a wake
            // counted for a waiter that reports a timeout.
            _ => return Err(format!("round {round}: wake gave {n}, the waiter {r}")),
        }
        kassert!(sched::futex::waiters() == base, "round {round}: {} waiters left", sched::futex::waiters() - base);
    }
    klog::event!(Info, "sc", "futex.stale.race", woken = woken, timed_out = timed);
    Ok(())
}

// A robust list in process memory: head at 0, entries at 0x200.., lock words 0x100 below their entries.
ktest!("sc.futex.robust_list", Safe, robust_list);
fn robust_list() -> TestResult {
    use sched::futex::{FUTEX_OWNER_DIED, FUTEX_WAITERS};
    let t = Fx::new("sc-fx-rb")?;
    let base = sched::futex::waiters();
    let tid: u32 = 4321;
    let off = (-0x100i64) as u64;
    let (e1, e2, e3, e4) = (0x200u64, 0x240u64, 0x280u64, 0x2c0u64);
    // head: next = e1, futex_offset, list_op_pending = e3; e1 -> e2 -> e4 -> head.
    t.put64(0, t.a(e1));
    t.put64(8, off);
    t.put64(16, t.a(e3));
    t.put64(e1, t.a(e2));
    t.put64(e2, t.a(e4));
    t.put64(e4, t.a(0));
    t.put32(e1 - 0x100, tid | FUTEX_WAITERS);
    t.put32(e2 - 0x100, tid);
    t.put32(e3 - 0x100, 0);
    t.put32(e4 - 0x100, tid + 1);
    let w1 = t.call("sc-fx-rbw1", args(t.a(e1 - 0x100), FUTEX_WAIT as u64, (tid | FUTEX_WAITERS) as u64, 0, 0, 0))?;
    let w3 = t.call("sc-fx-rbw3", args(t.a(e3 - 0x100), FUTEX_WAIT as u64, 0, 0, 0, 0))?;
    queued(base, 2)?;
    let head = t.a(0);
    let o = t.thread("sc-fx-owner", move |p, _| {
        exit_release(p, tid, head, 0, false);
        0
    })?;
    done(&o, "owner exit", 0)?;
    done(&w1, "waiter of the dead owner's lock", 0)?;
    done(&w3, "waiter of the pending unowned lock", 0)?;
    let (v1, v2, v3, v4) = (t.get32(e1 - 0x100), t.get32(e2 - 0x100), t.get32(e3 - 0x100), t.get32(e4 - 0x100));
    kassert!(v1 == FUTEX_WAITERS | FUTEX_OWNER_DIED, "lock 1: {v1:#x}");
    kassert!(v2 == FUTEX_OWNER_DIED, "lock 2: {v2:#x}");
    kassert!(v3 == 0, "pending unowned lock changed: {v3:#x}");
    kassert!(v4 == tid + 1, "another thread's lock changed: {v4:#x}");
    // A circular list ends at the limit; neither hangs nor faults.
    t.put64(0, t.a(e1));
    t.put64(16, 0);
    t.put64(e1, t.a(e1));
    t.put32(e1 - 0x100, tid);
    let o = t.thread("sc-fx-loop", move |p, _| {
        exit_release(p, tid, head, 0, false);
        0
    })?;
    done(&o, "circular list", 0)?;
    kassert!(t.get32(e1 - 0x100) == FUTEX_OWNER_DIED, "circular list lock: {:#x}", t.get32(e1 - 0x100));
    // A misaligned futex offset stops the walk; a head in unmapped memory does nothing.
    t.put64(8, 2);
    let o = t.thread("sc-fx-mis", move |p, _| {
        exit_release(p, tid, head, 0, false);
        0
    })?;
    done(&o, "misaligned futex offset", 0)?;
    let o = t.thread("sc-fx-bad", move |p, _| {
        exit_release(p, tid, 0x10, 0, false);
        0
    })?;
    done(&o, "unmapped head", 0)?;
    kassert!(sched::futex::waiters() == base, "waiters left behind");
    Ok(())
}

ktest!("sc.futex.clear_child_tid", Safe, clear_child_tid);
fn clear_child_tid() -> TestResult {
    let t = Fx::new("sc-fx-ct")?;
    let base = sched::futex::waiters();
    t.put32(0, 777);
    // pthread_join: a shared wait on the tid word.
    let j = t.call("sc-fx-join", args(t.a(0), FUTEX_WAIT as u64, 777, 0, 0, 0))?;
    queued(base, 1)?;
    // An only thread (mm not shared) leaves the word alone, as Linux.
    let at = t.a(0);
    let o = t.thread("sc-fx-x1", move |p, _| {
        exit_release(p, 777, 0, at, false);
        0
    })?;
    done(&o, "exit (single)", 0)?;
    kassert!(t.get32(0) == 777 && j.running(), "the tid word changed for a single-threaded exit");
    let o = t.thread("sc-fx-x2", move |p, _| {
        exit_release(p, 777, 0, at, true);
        0
    })?;
    done(&o, "exit (shared mm)", 0)?;
    done(&j, "joiner", 0)?;
    kassert!(t.get32(0) == 0, "tid word {}", t.get32(0));
    Ok(())
}

/// Drepper's "mutex2" on a user word through the syscall entry, as a program's lock would be.
fn user_lock(p: &Arc<Process>, m: u64) {
    let root = p.mm.root();
    let mut c = user_word::access(root, m, WordOp::CmpXchg { old: 0, new: 1 }).unwrap_or(1);
    if c == 0 {
        return;
    }
    if c != 2 {
        c = user_word::access(root, m, WordOp::Set(2)).unwrap_or(2);
    }
    while c != 0 {
        do_futex(p, args(m, FUTEX_WAIT as u64 | PRIV, 2, 0, 0, 0));
        c = user_word::access(root, m, WordOp::Set(2)).unwrap_or(2);
    }
}

fn user_unlock(p: &Arc<Process>, m: u64) {
    let root = p.mm.root();
    if user_word::access(root, m, WordOp::Add(u32::MAX)).unwrap_or(2) != 1 {
        let _ = user_word::access(root, m, WordOp::Set(0));
        do_futex(p, args(m, FUTEX_WAKE as u64 | PRIV, 1, 0, 0, 0));
    }
}

// 64 "threads" of one process (helper threads in its address space) contend for a futex mutex in its memory
// and count, with busy work between: under `-smp 16` every CPU is busy. Functional: every thread makes
// progress, the counter changed only under the lock is exact, nothing hangs.
ktest!("sc.futex.threads64", Safe, threads64);
fn threads64() -> TestResult {
    const N: u64 = 64;
    const ITERS: u32 = 3000;
    let t = Fx::new("sc-fx-64")?;
    let base = sched::futex::waiters();
    let start = Arc::new(AtomicU64::new(0));
    let mut hs = Vec::new();
    for i in 0..N {
        let s = start.clone();
        hs.push(t.thread(&format!("sc-fx-t{i}"), move |p, mem| {
            while s.load(Ordering::Acquire) == 0 {
                sched::sleep_ns(MS);
            }
            let root = p.mm.root();
            for _ in 0..ITERS {
                user_lock(p, mem);
                // Read-modify-write without an atomic RMW: a broken mutex loses counts.
                let c = user_word::access(root, mem + 64, WordOp::Load).unwrap_or(0);
                for _ in 0..200 {
                    core::hint::spin_loop();
                }
                let _ = user_word::access(root, mem + 64, WordOp::Set(c + 1));
                user_unlock(p, mem);
                let _ = user_word::access(root, mem + 128 + i * 4, WordOp::Add(1));
                for _ in 0..5_000 {
                    core::hint::spin_loop();
                }
                sched::cond_resched();
            }
            0
        })?);
    }
    start.store(1, Ordering::Release);
    // Progress while it runs: soon every thread has counted.
    let end = time::now_ns() + 60 * SEC;
    loop {
        let moving = (0..N).filter(|i| t.get32(128 + i * 4) > 0).count();
        if moving == N as usize {
            break;
        }
        kassert!(time::now_ns() < end, "only {moving} of {N} threads made progress in 60 s");
        sched::sleep_ns(5 * MS);
    }
    for h in &hs {
        kassert!(h.result(120 * SEC).is_some(), "a thread did not finish within 120 s (counter {})", t.get32(64));
    }
    let total = t.get32(64) as u64;
    kassert!(total == N * ITERS as u64, "counter {total}, want {} (the mutex let two threads in)", N * ITERS as u64);
    kassert!((0..N).all(|i| t.get32(128 + i * 4) == ITERS), "a thread did not count to the end");
    kassert!(t.get32(0) == 0, "mutex word {} at the end", t.get32(0));
    kassert!(sched::futex::waiters() == base, "waiters left behind");
    klog::event!(Info, "sc", "futex.threads64", threads = N, iterations = ITERS, cpus = sched::online_cpus());
    Ok(())
}

// Lost wake-ups: 32 pairs pass a token back and forth through a futex word (wait for my value, write the
// other's, wake), 3000 rounds each. A lost wake-up stops a pair for good (the limit catches it).
ktest!("sc.futex.lost_wakeup", Safe, lost_wakeup);
fn lost_wakeup() -> TestResult {
    const PAIRS: u64 = 32;
    const ROUNDS: u32 = 3000;
    let t = Fx::new("sc-fx-lw")?;
    let mut hs = Vec::new();
    for pair in 0..PAIRS {
        for side in 0..2u32 {
            hs.push(t.thread(&format!("sc-fx-p{pair}{side}"), move |p, mem| {
                let w = mem + pair * 64;
                let root = p.mm.root();
                for r in 0..ROUNDS {
                    let mine = r * 2 + side;
                    loop {
                        let v = user_word::access(root, w, WordOp::Load).unwrap_or(u32::MAX);
                        if v == mine {
                            break;
                        }
                        if v == u32::MAX {
                            return -1;
                        }
                        let e = do_futex(p, args(w, FUTEX_WAIT as u64 | PRIV, v as u64, 0, 0, 0));
                        if e != 0 && e != -EAGAIN {
                            return e;
                        }
                    }
                    let _ = user_word::access(root, w, WordOp::Set(mine + 1));
                    do_futex(p, args(w, FUTEX_WAKE as u64 | PRIV, 1, 0, 0, 0));
                }
                0
            })?);
        }
    }
    for (i, h) in hs.iter().enumerate() {
        match h.result(120 * SEC) {
            Some(0) => {}
            Some(e) => return Err(format!("pair {} side {}: error {e}", i / 2, i % 2)),
            None => return Err(format!("pair {} side {} stuck at {} (a lost wake-up)", i / 2, i % 2, t.get32((i as u64 / 2) * 64))),
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------------------
// Real programs

fn have(path: &str) -> bool {
    vfs::root_ns().lookup(path).is_ok()
}

fn run(path: &str, argv: &[&str], held: bool) -> Result<(Arc<Process>, Arc<proc::stdio::Capture>), String> {
    let out = proc::stdio::Capture::new(1 << 20);
    let env = ["HOME=/", "PATH=/bin", "ABI_TEST=1"];
    let mut o = proc::SpawnOptions::new(argv, &env, policy::ProgramId(1));
    o.stdin = Some(proc::stdio::Input::new(Vec::new()) as Arc<dyn vfs::File>);
    o.stdout = Some(out.clone() as Arc<dyn vfs::File>);
    o.stderr = Some(out.clone() as Arc<dyn vfs::File>);
    o.held = held;
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

// abi-futex's transcript equals CachyOS's (expected/futex.txt, known differences in futex.known).
ktest!("sc.futex.abi", Safe, abi);
fn abi() -> TestResult {
    if !have("/bin/abi-futex") {
        return ktest::skip("/bin/abi-futex is not in this image (the ABI-diff programs need UL-1's musl sysroot)");
    }
    let (p, out) = run("/bin/abi-futex", &["abi-futex"], false)?;
    let code = finish(&p, &out, 60 * SEC)?;
    let got = text(&out);
    let want = include_str!("../../../user/abi-tests/expected/futex.txt");
    let known = include_str!("../../../user/abi-tests/expected/futex.known");
    let known: Vec<&str> = known.lines().filter(|l| !l.starts_with('#') && !l.trim().is_empty()).filter_map(|l| l.split(": ").next()).collect();
    let mut diffs = Vec::new();
    let g: Vec<&str> = got.lines().collect();
    for (i, w) in want.lines().enumerate() {
        let gl = g.get(i).copied().unwrap_or("<missing>");
        if gl != w && !known.iter().any(|k| w.starts_with(k)) {
            diffs.push(format!("line {}: linux '{w}' got '{gl}'", i + 1));
        }
    }
    if g.len() > want.lines().count() {
        diffs.push(format!("{} extra lines", g.len() - want.lines().count()));
    }
    kassert!(diffs.is_empty() && code == 0, "abi-futex differs from CachyOS (exit {code}):\n{}", diffs.join("\n"));
    Ok(())
}

// 64 processes of `abi-futex worker` share a futex mutex in one /dev/shm file (shared keys by inode and page).
ktest!("sc.futex.procs64", Safe, procs64);
fn procs64() -> TestResult {
    const N: usize = 64;
    const ITERS: &str = "2000";
    if !have("/bin/abi-futex") {
        return ktest::skip("/bin/abi-futex is not in this image (the ABI-diff programs need UL-1's musl sysroot)");
    }
    let path = format!("/dev/shm/sc-fx-{}-{}", RUN.fetch_add(1, Ordering::Relaxed), time::now_ns() % 1_000_000);
    let mut ps = Vec::new();
    for i in 0..N {
        let idx = i.to_string();
        ps.push(run("/bin/abi-futex", &["abi-futex", "worker", &path, &idx, ITERS], true)?);
    }
    for (p, _) in &ps {
        p.release();
    }
    let mut failures = Vec::new();
    for (i, (p, out)) in ps.iter().enumerate() {
        match finish(p, out, 120 * SEC) {
            Ok(0) => {}
            Ok(c) => failures.push(format!("worker {i}: exit {c}: {}", text(out))),
            Err(e) => failures.push(e),
        }
    }
    let (c, out) = run("/bin/abi-futex", &["abi-futex", "check", &path, &N.to_string(), ITERS], false)?;
    let code = finish(&c, &out, 30 * SEC)?;
    let summary = text(&out);
    klog::event!(Info, "sc", "futex.procs64", procs = N, code = code, summary = summary.trim());
    kassert!(failures.is_empty(), "{}", failures.join("\n"));
    kassert!(code == 0, "check: {summary}");
    Ok(())
}

// abi-futex multi: wake/requeue/cmp_requeue/wake_op/wake_bitset counts with real waiter threads, equal to
// CachyOS's (expected/futex-multi.txt); skipped while clone is missing (AB-4).
ktest!("sc.futex.abi_multi", Safe, abi_multi);
fn abi_multi() -> TestResult {
    if !have("/bin/abi-futex") {
        return ktest::skip("/bin/abi-futex is not in this image (the ABI-diff programs need UL-1's musl sysroot)");
    }
    let (p, out) = run("/bin/abi-futex", &["abi-futex", "multi"], false)?;
    let code = finish(&p, &out, 60 * SEC)?;
    let got = text(&out);
    if got.lines().nth(1).is_some_and(|l| l.starts_with("create") && l.contains("EAGAIN")) {
        return ktest::skip("clone with the thread flags is not there yet (AB-4): pthread_create gives EAGAIN");
    }
    let want = include_str!("../../../user/abi-tests/expected/futex-multi.txt");
    kassert!(got == want && code == 0, "abi-futex multi differs from CachyOS (exit {code}):\ngot:\n{got}want:\n{want}");
    Ok(())
}

// SC-7b: abi-futex pi: the PI operations on one thread and with helper threads (hand-over, contended trylock,
// plain operations meeting a PI waiter, the owner's exit with and without a robust list, timeouts, EDEADLK,
// ESRCH), equal to CachyOS's (expected/futex-pi.txt).
ktest!("sc.futex.abi_pi", Safe, abi_pi);
fn abi_pi() -> TestResult {
    if !have("/bin/abi-futex") {
        return ktest::skip("/bin/abi-futex is not in this image (the ABI-diff programs need UL-1's musl sysroot)");
    }
    let (p, out) = run("/bin/abi-futex", &["abi-futex", "pi"], false)?;
    let code = finish(&p, &out, 60 * SEC)?;
    let got = text(&out);
    let want = include_str!("../../../user/abi-tests/expected/futex-pi.txt");
    let diffs: Vec<String> = want
        .lines()
        .zip(got.lines().chain(core::iter::repeat("<missing>")))
        .enumerate()
        .filter(|(_, (w, g))| w != g)
        .map(|(i, (w, g))| format!("line {}: linux '{w}' got '{g}'", i + 1))
        .collect();
    kassert!(diffs.is_empty() && code == 0 && got.lines().count() == want.lines().count(), "abi-futex pi differs from CachyOS (exit {code}):\n{}", diffs.join("\n"));
    Ok(())
}

// A process that ends holding a robust lock in a shared file: the robust-list walk at its end marks the word
// OWNER_DIED (CachyOS: "owner died: yes, waiters kept: yes, tid cleared: yes"). Skips only while nothing walked
// the list at the end (`robust_list` still registered: AB-4 has not wired `exit_release`, SC-5 CR 1); once
// something does, a wrong word fails the test.
ktest!("sc.futex.exit_robust", Safe, exit_robust);
fn exit_robust() -> TestResult {
    if !have("/bin/abi-futex") {
        return ktest::skip("/bin/abi-futex is not in this image (the ABI-diff programs need UL-1's musl sysroot)");
    }
    let path = format!("/dev/shm/sc-fx-rob-{}-{}", RUN.fetch_add(1, Ordering::Relaxed), time::now_ns() % 1_000_000);
    let (p, out) = run("/bin/abi-futex", &["abi-futex", "robust-die", &path], false)?;
    let code = finish(&p, &out, 30 * SEC)?;
    let die = text(&out);
    kassert!(die == "robust-die.set_robust_list = 0\n" && code == 0, "robust-die (exit {code}): {die}");
    // AB-4: every thread's end calls `exit_release` (proc's exit hook, installed by `syscall::init`), so the list
    // was walked; a wrong word is a failure.
    let walked = true;
    let (c, out) = run("/bin/abi-futex", &["abi-futex", "robust-check", &path], false)?;
    let code = finish(&c, &out, 30 * SEC)?;
    let got = text(&out);
    let want = "robust-check: owner died: yes, waiters kept: yes, tid cleared: yes\n";
    if got == want && code == 0 {
        return Ok(());
    }
    if !walked {
        return ktest::skip("the robust list was not walked at process end: AB-4 does not call syscall::futex::exit_release yet (SC-5 CR 1)");
    }
    Err(format!("robust list walked at the end, but the word is wrong (exit {code}): {got}"))
}

// musl pthreads (abi-pthread) with CachyOS's answers; skipped while clone is missing (AB-4).
ktest!("sc.futex.pthread", Safe, pthread);
fn pthread() -> TestResult {
    if !have("/bin/abi-pthread") {
        return ktest::skip("/bin/abi-pthread is not in this image (the ABI-diff programs need UL-1's musl sysroot)");
    }
    let cases: [(&str, &str); 8] = [
        ("mutex", "mutex: 8 threads x 20000 = 160000\n"),
        ("condvar", "condvar: consumed 20000 items, sum right: yes\n"),
        ("barrier", "barrier: 16 threads x 100 rounds, one serial thread per round: yes, every round complete: yes\n"),
        ("many", "many: round 0: 256 joined, results right: yes, all ran: yes\nmany: round 1: 256 joined, results right: yes, all ran: yes\n"),
        ("robust", "robust: lock after the owner died: EOWNERDEAD\nrobust: consistent: errno 0\nrobust: unlock: 0\nrobust: lock again: 0\n"),
        ("timed", "timed: no signal: errno 110\ntimed: signalled: 0, flag 1\ntimed: mutex_timedlock free: 0\ntimed: mutex_timedlock held: errno 110\n"),
        ("rwlock", "rwlock: value 10000, readers saw a half write: 0\n"),
        ("sem", "sem: 4000 handed through 4 threads, left 0\n"),
    ];
    let mut bad = Vec::new();
    for (mode, want) in cases {
        let (p, out) = run("/bin/abi-pthread", &["abi-pthread", mode], false)?;
        let code = finish(&p, &out, 120 * SEC)?;
        let got = text(&out);
        if got.starts_with("create") && got.contains("EAGAIN") {
            return ktest::skip("clone with the thread flags is not there yet (AB-4): pthread_create gives EAGAIN");
        }
        if got != want || code != 0 {
            bad.push(format!("{mode}: exit {code}, got:\n{got}want:\n{want}"));
        }
    }
    kassert!(bad.is_empty(), "{}", bad.join("\n"));
    Ok(())
}
