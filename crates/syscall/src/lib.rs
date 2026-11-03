//! Linux syscall dispatch (owner: AB; registry `syscall!` from G0, V2-PLAN §2.7).
//!
//! Lanes register syscalls in their own crates: `syscall::syscall!(WRITE, sys_write);`. The table is built
//! once from the `kreg` registry (a `Vec` indexed by number, sized by the highest registered number). Unknown
//! numbers return -ENOSYS and are logged once each (the to-do list, R6). Every number has counters (calls,
//! errors; unknown ones: ENOSYS hits) for `stats()`; `/proc/bitsos/syscalls` (0444, `stats_text`) shows them
//! (C1), so GM and YT can compare error counts per call without ptrace.
//!
//! AB-3 (wave A, `spec/rust-ecosystem.md` §3.5): what a static musl or Rust std program needs besides the file
//! calls (FS registers those in `vfs-sys`): memory and arch (`memory.rs`), signals basics (`signal.rs`),
//! clocks and sleeps (`clock.rs`), ids, uname, limits, randomness, robust list, scheduling (`info.rs`),
//! poll/ppoll (`poll.rs`) and ioctl (`ioctl.rs`). AB-6: after every syscall the way out delivers signals
//! and turns an interrupted call's answer into a restart or EINTR (`proc::deliver::syscall_exit`).
//!
//! AB-4: clone/clone3/fork/vfork (`fork.rs`), execve/execveat (`exec.rs`), exit/exit_group, wait4/waitid and
//! process groups and sessions (`process.rs`); the work itself is in `proc`.
#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

mod clock;
mod counts;
mod exec;
pub mod fcntl;
mod fork;
mod fsfreeze;
mod fsfreeze_ktests;
mod cputime_sys;
pub mod futex;
mod futex_ktests;
pub mod ntsync;
mod ntsync_ktests;
mod ab5_ktests;
mod ab6_ktests;
mod ab6_more_ktests;
mod ab12_ktests;
#[cfg(bitsos_fd_measure)]
mod ab15_ktests;
mod ab15_counts_ktests;
mod ab16_ktests;
mod ab17_ktests;
mod ab7_ktests;
mod ab7b_ktests;
mod caps;
mod cred_ktests;
mod info;
mod ioctl;
mod itimer;
mod ktests;
mod ldt;
mod ab10a_ktests;
mod memory;
mod prctl;
mod process;
mod pidfd;
mod procvm;
mod kcmp;
mod procfs;
// UL-7: reboot(2) (granted to UL, src/reboot.rs and its ktests).
pub mod reboot;
mod reboot_ktests;
mod sched_ktests;
mod sched_sys;
mod seccomp_sys;
mod sx1_ktests;
mod ns_sys;
mod sx2_ktests;
mod signal;
mod signalfd;
pub mod sysinfo;
pub mod timerfd;
mod socket;
mod socket_ktests;
mod uts;
mod wave_a_ktests;

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::fmt::Write;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use hal::entry::UserContext;
use ksync::SpinLock;
use proc::{Process, Thread};

/// What a handler sees: the calling thread's process image, the thread itself, its registers and the
/// arguments.
pub struct SysCtx<'a> {
    pub proc: &'a Arc<Process>,
    pub thread: &'a Arc<Thread>,
    pub user: &'a mut UserContext,
    pub args: [u64; 6],
}

pub type Handler = fn(&mut SysCtx<'_>) -> i64;

pub struct SyscallEntry {
    pub nr: usize,
    pub name: &'static str,
    pub handler: Handler,
}

kreg::declare!(pub SYSCALLS: SyscallEntry = "syscall");

#[doc(hidden)]
pub use kreg as __kreg;

/// `syscall::syscall!(WRITE, sys_write);` (names from `uapi::nr`).
#[macro_export]
macro_rules! syscall {
    ($nr:ident, $h:path) => {
        $crate::__kreg::register!("syscall", $crate::SyscallEntry, $crate::SyscallEntry { nr: uapi::nr::$nr, name: stringify!($nr), handler: $h });
    };
}

/// A number `uapi::nr` (desk) does not name yet, registered with its own name (`uapi::ab::nr_more`).
macro_rules! syscall_nr {
    ($nr:expr, $name:literal, $h:path) => {
        $crate::__kreg::register!("syscall", $crate::SyscallEntry, $crate::SyscallEntry { nr: $nr, name: $name, handler: $h });
    };
}
pub(crate) use syscall_nr;

struct Slot {
    handler: Option<Handler>,
    name: &'static str,
}

/// Unknown numbers below this are counted and logged one by one; the rest (x32 numbers with bit 30, garbage)
/// share one bucket. A bound against hostile input: a program trying every u64 must not grow a map without end.
const ENOSYS_TRACKED_BELOW: usize = 4096;

static TABLE: ksync::Once<Vec<Slot>> = ksync::Once::new();
static ENOSYS: SpinLock<BTreeMap<usize, u64>> = SpinLock::named("syscall.enosys", BTreeMap::new());
static ENOSYS_OTHER: AtomicU64 = AtomicU64::new(0);
static ENOSYS_OTHER_LOGGED: AtomicBool = AtomicBool::new(false);
klog::counter!(ENOSYS_TOTAL, "syscall.enosys");

fn table() -> &'static Vec<Slot> {
    TABLE.get_or_init(|| {
        let max = SYSCALLS.entries().iter().map(|e| e.nr).max().unwrap_or(0);
        let mut t: Vec<Slot> = (0..=max).map(|_| Slot { handler: None, name: "" }).collect();
        for e in SYSCALLS.entries() {
            t[e.nr].handler = Some(e.handler);
            t[e.nr].name = uapi::nr::name(e.nr).unwrap_or(e.name);
        }
        t
    })
}

pub(crate) fn dispatch(p: &Arc<Process>, thread: &Arc<Thread>, user: &mut UserContext) {
    let nr = user.regs.rax as usize;
    let args = user.syscall_args();
    // SX-1: seccomp. Without a mode this is one load and one branch; a call the filters stop never reaches
    // its handler (the hook set its answer and ran the way out, or the thread is ending).
    if thread.prctl().seccomp().mode() != 0 && seccomp_sys::entry(p, thread, user, nr, &args) {
        return;
    }
    #[cfg(bitsos_fd_measure)]
    hal::entry::steps::mark(hal::entry::steps::SECCOMP);
    // RB-7a: slow-syscall trace (no-op unless CONFIG trace names it); a call that blocked is not "slow".
    let t0 = klog::trace::syscall_start();
    let sw0 = if t0 != 0 { switches(thread) } else { None };
    #[cfg(bitsos_fd_measure)]
    hal::entry::steps::mark(hal::entry::steps::TRACE_START);
    // Spectre v1: the handler is read with the number masked into range, as Linux's do_syscall_x64 does.
    let tbl = table();
    let slot = if nr < tbl.len() { tbl.get(hal::entry::index_nospec(nr, tbl.len())) } else { None };
    let ret = match slot.and_then(|s| s.handler) {
        Some(h) => {
            counts::call(nr);
            #[cfg(bitsos_fd_measure)]
            hal::entry::steps::mark(hal::entry::steps::LOOKUP);
            let r = h(&mut SysCtx { proc: p, thread, user, args });
            #[cfg(bitsos_fd_measure)]
            hal::entry::steps::mark(hal::entry::steps::HANDLER);
            if (-4095..0).contains(&r) {
                counts::error(nr);
            }
            r
        }
        None => {
            enosys(p, nr);
            -uapi::errno::ENOSYS
        }
    };
    if t0 != 0 {
        let blocked = switches(thread) != sw0;
        klog::trace::syscall_done(nr as u64, t0, ret as i64, blocked);
    }
    user.set_syscall_return(ret);
    #[cfg(bitsos_fd_measure)]
    hal::entry::steps::mark(hal::entry::steps::TRACE_DONE);
    // AB-6: Linux's `syscall_exit_to_user_mode`: an interrupted call's restart answer and the signals to
    // deliver (handlers, default actions, stops) before the program runs on.
    proc::deliver::syscall_exit(thread, user, nr, ret);
    #[cfg(bitsos_fd_measure)]
    hal::entry::steps::mark(hal::entry::steps::SYS_EXIT);
}

/// Voluntary switches of the kernel thread running the call (the slow-call trace's "it blocked"). The run loop
/// calls on the program thread's own kernel thread, whose handle the thread keeps: no clone of the current
/// thread's `Arc` (two per call before AB-15). Tests that play a program's thread on their own thread use the
/// current one, as before.
fn switches(thread: &Thread) -> Option<u64> {
    match thread.sched_thread() {
        Some(k) if thread.kthread() == sched::current_id() => Some(k.nvcsw()),
        _ => sched::current_thread().map(|t| t.nvcsw()),
    }
}

fn enosys(p: &Process, nr: usize) {
    ENOSYS_TOTAL.inc();
    if nr >= ENOSYS_TRACKED_BELOW {
        ENOSYS_OTHER.fetch_add(1, Ordering::Relaxed);
        if !ENOSYS_OTHER_LOGGED.swap(true, Ordering::Relaxed) {
            klog::event!(Info, "syscall", "nr.invalid", nr = klog::Hex(nr as u64), program = p.name.as_str(); "numbers from 4096 on are counted together and logged once");
        }
        return;
    }
    let first = {
        let mut m = ENOSYS.lock();
        let c = m.entry(nr).or_insert(0);
        *c += 1;
        *c == 1
    };
    if first {
        klog::event!(Info, "syscall", "enosys", nr = nr, name = uapi::nr::name(nr).unwrap_or("?"), program = p.name.as_str());
    }
}

/// Keys of `ENOTTY` (ioctl command) and `ENOPROTOOPT` ((level << 32) | optname) answers already logged once.
static GAPS: SpinLock<BTreeMap<(u8, u64), u64>> = SpinLock::named("syscall.gaps", BTreeMap::new());
klog::counter!(GAPS_TOTAL, "syscall.gaps");

/// YT-4a CR 11: an ioctl that ends in ENOTTY or a socket option that ends in ENOPROTOOPT is logged once per key
/// (as `enosys` does for numbers), so an interface gap is visible without a failing test.
pub(crate) fn note_gap(kind: u8, key: u64, ret: i64, program: &str) {
    let errno = if kind == 0 { uapi::errno::ENOTTY } else { uapi::errno::ENOPROTOOPT };
    if ret != -errno {
        return;
    }
    GAPS_TOTAL.inc();
    let first = {
        let mut m = GAPS.lock();
        let c = m.entry((kind, key)).or_insert(0);
        *c += 1;
        *c == 1
    };
    if !first {
        return;
    }
    if kind == 0 {
        klog::event!(Info, "syscall", "enotty", cmd = klog::Hex(key), program = program);
    } else {
        klog::event!(Info, "syscall", "enoprotoopt", level = key >> 32, optname = key as u32, program = program);
    }
}

/// Kernel init: the user-copy fast path (its #PF fixup), proc's lazy-fill resolver for it, and the dispatcher
/// into `proc`.
pub fn init() {
    hal::usercopy::install_fixup();
    proc::init();
    proc::set_syscall_handler(dispatch);
    // AB-10a: int 0x80's TLS/LDT calls.
    proc::set_int80_handler(ldt::int80);
    // SC-5 CR 1: every thread's end and every execve run the futex exit protocol (robust list, tid word).
    proc::set_exit_release(futex::exit_release);
    // AB-5: the load average sysinfo reports.
    sysinfo::start_sampler();
    klog::event!(
        Info,
        "syscall",
        "table",
        registered = SYSCALLS.len(),
        highest = table().len().saturating_sub(1),
        usercopy_fast = hal::usercopy::stats().fast_path,
        user_fsgsbase = hal::entry::user_fsgsbase(),
        smap = hal::entry::smap_all().unwrap_or(false)
    );
}

/// One line of `stats()`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SyscallStat {
    pub nr: usize,
    pub name: &'static str,
    pub calls: u64,
    /// Calls that returned -errno.
    pub errors: u64,
    /// Not implemented: `calls` are ENOSYS answers.
    pub enosys: bool,
}

/// Every number that was called at least once, by number: registered ones with calls and errors, unknown ones
/// with their ENOSYS count. Unknown numbers from 4096 on appear as one line with `nr` = 4096 and name "other".
pub fn stats() -> Vec<SyscallStat> {
    let mut v: Vec<SyscallStat> = table()
        .iter()
        .enumerate()
        .filter(|(_, s)| s.handler.is_some())
        .filter_map(|(nr, s)| {
            let calls = counts::calls(nr);
            (calls > 0).then(|| SyscallStat { nr, name: s.name, calls, errors: counts::errors(nr), enosys: false })
        })
        .collect();
    let unknown: Vec<(usize, u64)> = ENOSYS.lock().iter().map(|(k, c)| (*k, *c)).collect();
    for (nr, calls) in unknown {
        v.push(SyscallStat { nr, name: uapi::nr::name(nr).unwrap_or("?"), calls, errors: calls, enosys: true });
    }
    let other = ENOSYS_OTHER.load(Ordering::Relaxed);
    if other > 0 {
        v.push(SyscallStat { nr: ENOSYS_TRACKED_BELOW, name: "other", calls: other, errors: other, enosys: true });
    }
    v.sort_by_key(|s| s.nr);
    v
}

/// Whether fcntl is AB-5's `fcntl::sys_fcntl` (after the integration patch moved its registration from vfs-sys).
pub(crate) fn fcntl_is_ab5() -> bool {
    is_handler(uapi::nr::FCNTL, fcntl::sys_fcntl)
}

/// Whether close, dup2 and dup3 are AB-5's lock-releasing wrappers (the integration patch, with fcntl).
pub(crate) fn close_dup_are_ab5() -> bool {
    is_handler(uapi::nr::CLOSE, fcntl::sys_close) && is_handler(uapi::nr::DUP2, fcntl::sys_dup2) && is_handler(uapi::nr::DUP3, fcntl::sys_dup3)
}

fn is_handler(nr: usize, f: Handler) -> bool {
    table().get(nr).and_then(|s| s.handler).is_some_and(|h| core::ptr::fn_addr_eq(h, f))
}

/// Calls of a registered number (0 for unknown ones).
pub fn calls(nr: usize) -> u64 {
    if table().get(nr).is_some_and(|s| s.handler.is_some()) { counts::calls(nr) } else { 0 }
}

/// ENOSYS answers for `nr` (numbers from 4096 on share one count).
pub fn enosys_count(nr: usize) -> u64 {
    if nr >= ENOSYS_TRACKED_BELOW {
        return ENOSYS_OTHER.load(Ordering::Relaxed);
    }
    ENOSYS.lock().get(&nr).copied().unwrap_or(0)
}

/// `stats()` and the user-copy counters as text lines: the content of `/proc/bitsos/syscalls` (C1).
pub fn stats_text() -> String {
    let mut s = String::new();
    for st in stats() {
        let _ = writeln!(s, "{:>4} {:<20} calls={} errors={}{}", st.nr, st.name, st.calls, st.errors, if st.enosys { " ENOSYS" } else { "" });
    }
    let u = hal::usercopy::stats();
    let _ = writeln!(
        s,
        "usercopy fast_path={} fast_faults={} resolved={} walks={} hook_replaced={} nofault_short={}; user interrupts={}",
        u.fast_path,
        u.fast_faults,
        u.resolved,
        u.walks,
        u.hook_replaced,
        u.nofault_short,
        hal::entry::user_interrupts()
    );
    s
}
vfs::procfile!("/proc/bitsos/syscalls", stats_text);

/// No two registrations for one number (the registry contract).
fn unique() -> ktest::TestResult {
    let mut nrs: Vec<usize> = SYSCALLS.entries().iter().map(|e| e.nr).collect();
    nrs.sort();
    for w in nrs.windows(2) {
        ktest::kassert!(w[0] != w[1], "syscall {} registered twice", w[0]);
    }
    Ok(())
}
ktest::ktest!("kreg.syscalls_unique", Safe, unique);
