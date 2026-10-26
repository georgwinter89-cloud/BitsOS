//! futex and futex_waitv (SC-5, glue granted to SC; the futex core is `sched::futex`). Linux:
//! linux-6.6/kernel/futex/syscalls.c (argument rules, `futex_init_timeout`, `do_futex`, `futex_waitv`),
//! core.c:221 `get_futex_key` (the keys, `ProcSpace::key`). set_robust_list/get_robust_list stay in `info.rs`
//! (AB-3, already Linux-exact); the robust-list walk and CLONE_CHILD_CLEARTID are `exit_release` here, called
//! by the process code at thread exit (AB-4 wires it, change request in plan/notes/sc-SC-5.md).
//!
//! Errors and their order are Linux's: the timeout is read and checked first (EFAULT, EINVAL), then
//! FUTEX_CLOCK_REALTIME on an operation that has no absolute timeout (ENOSYS), then the operation (an unknown
//! one ENOSYS). SC-7b: the PI operations are `sched::futex::pi` (LOCK_PI's timeout is CLOCK_REALTIME, LOCK_PI2's
//! monotonic unless FUTEX_CLOCK_REALTIME; an interrupted LOCK_PI answers ERESTARTNOINTR as Linux's
//! `futex_lock_pi`); WAIT_REQUEUE_PI/CMP_REQUEUE_PI check their arguments as Linux and then answer ENOSYS.

use crate::SysCtx;
use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec::Vec;
use ksync::user_word::{self, WordError, WordOp};
use proc::Process;
use sched::futex::{self, Deadline, Key, KeyKind, KeyRef, WaitvItem};
use uapi::errno::*;

pub const FUTEX_WAIT: u32 = 0;
pub const FUTEX_WAKE: u32 = 1;
pub const FUTEX_FD: u32 = 2;
pub const FUTEX_REQUEUE: u32 = 3;
pub const FUTEX_CMP_REQUEUE: u32 = 4;
pub const FUTEX_WAKE_OP: u32 = 5;
pub const FUTEX_LOCK_PI: u32 = 6;
pub const FUTEX_UNLOCK_PI: u32 = 7;
pub const FUTEX_TRYLOCK_PI: u32 = 8;
pub const FUTEX_WAIT_BITSET: u32 = 9;
pub const FUTEX_WAKE_BITSET: u32 = 10;
pub const FUTEX_WAIT_REQUEUE_PI: u32 = 11;
pub const FUTEX_CMP_REQUEUE_PI: u32 = 12;
pub const FUTEX_LOCK_PI2: u32 = 13;
pub const FUTEX_PRIVATE_FLAG: u32 = 128;
pub const FUTEX_CLOCK_REALTIME: u32 = 256;
/// futex_waitv: the only size flag (FUTEX_32) and the per-waiter mask.
pub const FUTEX_32: u32 = 2;
pub const FUTEX_WAITV_MAX: u64 = 128;
/// x86-64 syscall number of futex_waitv (uapi::nr does not name it yet).
pub const NR_FUTEX_WAITV: usize = 449;
const CLOCK_REALTIME: i32 = 0;
const CLOCK_MONOTONIC: i32 = 1;
const NSEC: u64 = 1_000_000_000;
/// KTIME_MAX: the end of time for a saturated timeout.
const KTIME_MAX: u64 = i64::MAX as u64;

/// The memory of process `p` as the futex core sees it. `restart`: what an interrupted wait answers when the
/// calling thread has a signal to handle (AB-6; Linux: ERESTARTSYS for untimed waits and futex_waitv,
/// ERESTART_RESTARTBLOCK for a timed FUTEX_WAIT/FUTEX_WAIT_BITSET, whose restart block keeps the absolute
/// deadline: EINTR with a handler, `restart_syscall` waits on to the same deadline without one).
pub struct ProcSpace<'a> {
    pub p: &'a Arc<Process>,
    pub restart: i64,
}

impl<'a> ProcSpace<'a> {
    pub fn new(p: &'a Arc<Process>) -> Self {
        ProcSpace { p, restart: uapi::ab::siginfo::ERESTARTSYS }
    }
}

impl ProcSpace<'_> {
    fn page_key(&self, kind: KeyKind, addr: u64) -> Key {
        Key { kind, object: self.p.mm.root(), page: addr & !0xfff, offset: (addr & 0xfff) as u32 }
    }
}

impl futex::Space for ProcSpace<'_> {
    /// `get_futex_key`: private = (address space, address) after the alignment and `access_ok` checks only.
    /// Shared: the VMA decides as Linux's `get_user_pages_fast(FOLL_WRITE)` and the page kind would: a
    /// MAP_SHARED file (or shmem) page is (inode, page index, offset), read-only allowed for the reading
    /// operations; a private file mapping that is writable becomes an anonymous page by the write (COW), so it
    /// keys like anonymous memory; anonymous memory needs a writable mapping (a read-only anonymous page never
    /// changes: EFAULT); nothing mapped or PROT_NONE: EFAULT.
    fn key(&self, addr: u64, shared: bool, write: bool) -> Result<KeyRef, i64> {
        if addr % 4 != 0 {
            return Err(EINVAL);
        }
        // `access_ok` for 4 bytes. Linux 6.6 accepted any address with bit 63 clear; the CachyOS kernel the ABI
        // diff compares with (7.x) accepts up to USER_PTR_MAX, the end of user space: a non-canonical
        // "user" address is EFAULT there, and so here. The bound is inclusive as Linux's `valid_user_address`
        // (addr <= USER_PTR_MAX = 0x7ffffffff000, measured on CachyOS 7.2: FUTEX_WAKE_PRIVATE on 0x7ffffffff000
        // gives 0, on 0x7ffffffff004 EFAULT; abi-futex "wake.user_top.*"), although no 4 bytes there are user
        // memory: the operations that touch the word still answer EFAULT through `user_word`.
        if addr > hal::USER_TOP {
            return Err(EFAULT);
        }
        if !shared {
            return Ok(KeyRef::plain(self.page_key(KeyKind::Private, addr)));
        }
        let vmas = self.p.mm.vmas();
        let i = vmas.partition_point(|v| v.end <= addr);
        let v = vmas.get(i).filter(|v| v.start <= addr).ok_or(EFAULT)?;
        if !v.prot.accessible() {
            return Err(EFAULT);
        }
        match &v.file {
            Some(f) => {
                let may_share = f.shared || f.backing.file().is_some_and(|m| m.may_share());
                if !may_share && v.prot.write {
                    return Ok(KeyRef::plain(self.page_key(KeyKind::MmShared, addr)));
                }
                if write && !v.prot.write {
                    return Err(EFAULT);
                }
                let object = match f.backing.file() {
                    Some(m) => Arc::as_ptr(m.inode()) as *const u8 as u64,
                    None => Arc::as_ptr(&f.backing) as *const u8 as u64,
                };
                let page = v.pgoff_at(addr).unwrap_or(0);
                let hold: Box<dyn core::any::Any + Send + Sync> = Box::new(f.backing.clone());
                Ok(KeyRef { key: Key { kind: KeyKind::Inode, object, page, offset: (addr & 0xfff) as u32 }, hold: Some(hold) })
            }
            None => {
                if !v.prot.write {
                    return Err(EFAULT);
                }
                Ok(KeyRef::plain(self.page_key(KeyKind::MmShared, addr)))
            }
        }
    }
    fn word(&self, addr: u64, op: WordOp) -> Result<u32, WordError> {
        user_word::access(self.p.mm.root(), addr, op)
    }
    fn fault_in(&self, addr: u64, write: bool) -> Result<(), i64> {
        if write {
            self.p.mm.handle_user_fault(addr, mm::fault::PF_USER | mm::fault::PF_WRITE).map_err(|_| EFAULT)
        } else {
            self.read_u32(addr).map(|_| ())
        }
    }
    fn read_u32(&self, addr: u64) -> Result<u32, i64> {
        let mut b = [0u8; 4];
        self.p.mm.copy_from_user(&mut b, addr).map_err(|_| EFAULT)?;
        Ok(u32::from_le_bytes(b))
    }
    fn read_u64(&self, addr: u64) -> Result<u64, i64> {
        self.p.mm.read_u64(addr).map_err(|_| EFAULT)
    }
    fn write_u32(&self, addr: u64, v: u32) -> Result<(), i64> {
        self.p.mm.copy_to_user(addr, &v.to_le_bytes()).map_err(|_| EFAULT)
    }
    /// The process was told to end (kill, exit_group, a fatal signal): EINTR; AB-6: the calling thread has a
    /// signal to handle: the restart answer.
    fn interrupted(&self) -> Option<i64> {
        if self.p.has_exited() {
            return Some(EINTR);
        }
        proc::current_signal_pending().then_some(self.restart)
    }
    /// SC-7b: the TID programs see (the PI futex word's owner field).
    fn tid(&self) -> Option<u32> {
        proc::current_thread().map(|t| t.tid())
    }
    /// SC-7b: any live user thread with this TID (Linux `find_get_task_by_vpid`; one namespace).
    fn thread_of(&self, tid: u32) -> Option<u64> {
        proc::find_thread(tid).map(|t| t.kthread()).filter(|&k| k != 0)
    }
}

/// A `struct __kernel_timespec` from user memory, checked as `timespec64_valid` (EFAULT, EINVAL); in ns,
/// saturated at KTIME_MAX as `timespec64_to_ktime`.
pub fn read_timespec(p: &Process, addr: u64) -> Result<u64, i64> {
    let mut b = [0u8; 16];
    p.mm.copy_from_user(&mut b, addr).map_err(|_| EFAULT)?;
    let sec = i64::from_le_bytes(b[..8].try_into().unwrap_or([0; 8]));
    let nsec = i64::from_le_bytes(b[8..].try_into().unwrap_or([0; 8]));
    if sec < 0 || !(0..NSEC as i64).contains(&nsec) {
        return Err(EINVAL);
    }
    Ok((sec as u64).checked_mul(NSEC).and_then(|s| s.checked_add(nsec as u64)).map_or(KTIME_MAX, |t| t.min(KTIME_MAX)))
}

fn has_timeout(cmd: u32) -> bool {
    matches!(cmd, FUTEX_WAIT | FUTEX_LOCK_PI | FUTEX_LOCK_PI2 | FUTEX_WAIT_BITSET | FUTEX_WAIT_REQUEUE_PI)
}

/// The futex syscall on process `p` (also used by the ktests).
pub fn do_futex(p: &Arc<Process>, args: [u64; 6]) -> i64 {
    let [uaddr, op, val, utime, uaddr2, val3] = args;
    let (op, val, val3) = (op as u32, val as u32, val3 as u32);
    let cmd = op & !(FUTEX_PRIVATE_FLAG | FUTEX_CLOCK_REALTIME);
    let mut deadline = None;
    if utime != 0 && has_timeout(cmd) {
        let t = match read_timespec(p, utime) {
            Ok(t) => t,
            Err(e) => return -e,
        };
        deadline = Some(if cmd == FUTEX_WAIT {
            Deadline::Monotonic(time::now_ns().saturating_add(t).min(KTIME_MAX))
        } else if cmd != FUTEX_LOCK_PI && op & FUTEX_CLOCK_REALTIME == 0 {
            Deadline::Monotonic(t)
        } else {
            Deadline::Realtime(t)
        });
    }
    let shared = op & FUTEX_PRIVATE_FLAG == 0;
    if op & FUTEX_CLOCK_REALTIME != 0 && !matches!(cmd, FUTEX_WAIT_BITSET | FUTEX_WAIT_REQUEUE_PI | FUTEX_LOCK_PI2) {
        return -ENOSYS;
    }
    // val2 of do_futex: the timeout argument's low 32 bits, an int.
    let val2 = utime as u32 as i32;
    let r = match cmd {
        FUTEX_WAIT => return timed_wait(p, uaddr, shared, val, futex::BITSET_MATCH_ANY, deadline),
        FUTEX_WAIT_BITSET => return timed_wait(p, uaddr, shared, val, val3, deadline),
        FUTEX_WAKE => futex::wake(&ProcSpace::new(p), uaddr, shared, val as i32, futex::BITSET_MATCH_ANY),
        FUTEX_WAKE_BITSET => futex::wake(&ProcSpace::new(p), uaddr, shared, val as i32, val3),
        FUTEX_REQUEUE => futex::requeue(&ProcSpace::new(p), uaddr, uaddr2, shared, val as i32, val2, None),
        FUTEX_CMP_REQUEUE => futex::requeue(&ProcSpace::new(p), uaddr, uaddr2, shared, val as i32, val2, Some(val3)),
        FUTEX_WAKE_OP => futex::wake_op(&ProcSpace::new(p), uaddr, uaddr2, shared, val as i32, val2, val3),
        FUTEX_LOCK_PI | FUTEX_LOCK_PI2 => return lock_pi(p, uaddr, shared, deadline),
        FUTEX_TRYLOCK_PI => futex::pi::lock_pi(&ProcSpace::new(p), uaddr, shared, None, true).map(|_| 0),
        FUTEX_UNLOCK_PI => futex::pi::unlock_pi(&ProcSpace::new(p), uaddr, shared).map(|_| 0),
        // linux-6.6/kernel/futex/requeue.c:762 and :364: the argument checks, then (not built here) ENOSYS.
        FUTEX_WAIT_REQUEUE_PI if uaddr == uaddr2 => Err(EINVAL),
        FUTEX_CMP_REQUEUE_PI if (val as i32) < 0 || val2 < 0 || uaddr == uaddr2 || val as i32 != 1 => Err(EINVAL),
        FUTEX_WAIT_REQUEUE_PI | FUTEX_CMP_REQUEUE_PI => {
            note_pi(cmd);
            Err(ENOSYS)
        }
        _ => Err(ENOSYS),
    };
    match r {
        Ok(n) => n,
        Err(e) => -e,
    }
}

/// `futex_wait` (kernel/futex/waitwake.c:666): an untimed wait answers ERESTARTSYS when a signal interrupts
/// it; a timed one keeps a restart block with its absolute deadline (`futex_wait_restart`) and answers
/// ERESTART_RESTARTBLOCK, so a handler sees EINTR and a stop/continue waits on to the same end.
fn timed_wait(p: &Arc<Process>, uaddr: u64, shared: bool, val: u32, bitset: u32, deadline: Option<Deadline>) -> i64 {
    use uapi::ab::siginfo::{ERESTART_RESTARTBLOCK, ERESTARTSYS};
    let restart = if deadline.is_some() { ERESTART_RESTARTBLOCK } else { ERESTARTSYS };
    let sp = ProcSpace { p, restart };
    match futex::wait(&sp, uaddr, shared, val, bitset, deadline) {
        Ok(_) => 0,
        Err(e) if e == ERESTART_RESTARTBLOCK => {
            let (at, realtime) = match deadline {
                Some(Deadline::Realtime(t)) => (t, true),
                Some(Deadline::Monotonic(t)) => (t, false),
                None => (0, false),
            };
            match proc::current_thread() {
                Some(t) => {
                    t.set_restart_block(proc::Restart::Futex { uaddr, shared, val, bitset, deadline: at, realtime });
                    -ERESTART_RESTARTBLOCK
                }
                // Kernel callers (ktests) never have a signal; be safe anyway.
                None => -EINTR,
            }
        }
        Err(e) => -e,
    }
}

/// FUTEX_LOCK_PI/LOCK_PI2 (pi.c:930): a signal ends the wait with ERESTARTNOINTR (no restart block: the
/// deadline is absolute), so the call is always restarted.
fn lock_pi(p: &Arc<Process>, uaddr: u64, shared: bool, deadline: Option<Deadline>) -> i64 {
    use uapi::ab::siginfo::ERESTARTNOINTR;
    let sp = ProcSpace { p, restart: ERESTARTNOINTR };
    match futex::pi::lock_pi(&sp, uaddr, shared, deadline, false) {
        Ok(()) => 0,
        Err(EINTR) => -ERESTARTNOINTR,
        Err(e) => -e,
    }
}

/// `futex_wait_restart`: the restart block's wait again.
pub(crate) fn restart_wait(p: &Arc<Process>, uaddr: u64, shared: bool, val: u32, bitset: u32, deadline: u64, realtime: bool) -> i64 {
    let d = if realtime { Deadline::Realtime(deadline) } else { Deadline::Monotonic(deadline) };
    timed_wait(p, uaddr, shared, val, bitset, Some(d))
}

static PI_NOTED: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

fn note_pi(cmd: u32) {
    if !PI_NOTED.swap(true, core::sync::atomic::Ordering::Relaxed) {
        klog::event!(Info, "futex", "pi.requeue_unsupported", cmd = cmd; "FUTEX_WAIT_REQUEUE_PI/CMP_REQUEUE_PI answer ENOSYS after Linux's argument checks (SC-7b)");
    }
}

fn sys_futex(c: &mut SysCtx<'_>) -> i64 {
    do_futex(c.proc, c.args)
}
crate::syscall!(FUTEX, sys_futex);

/// futex_waitv(waiters, nr_futexes, flags, timeout, clockid) (syscalls.c:273, :213 `futex_parse_waitv`).
pub fn do_futex_waitv(p: &Arc<Process>, args: [u64; 6]) -> i64 {
    let [waiters, nr, flags, timeout, clockid, _] = args;
    let (nr, flags, clockid) = (nr as u32 as u64, flags as u32, clockid as i32);
    if flags != 0 || nr == 0 || nr > FUTEX_WAITV_MAX || waiters == 0 {
        return -EINVAL;
    }
    let mut deadline = None;
    if timeout != 0 {
        if clockid != CLOCK_REALTIME && clockid != CLOCK_MONOTONIC {
            return -EINVAL;
        }
        let t = match read_timespec(p, timeout) {
            Ok(t) => t,
            Err(e) => return -e,
        };
        deadline = Some(if clockid == CLOCK_REALTIME { Deadline::Realtime(t) } else { Deadline::Monotonic(t) });
    }
    let mut items: Vec<WaitvItem> = Vec::new();
    if items.try_reserve_exact(nr as usize).is_err() {
        return -ENOMEM;
    }
    for i in 0..nr {
        let mut b = [0u8; 24];
        let Some(at) = waiters.checked_add(i * 24) else { return -EFAULT };
        if p.mm.copy_from_user(&mut b, at).is_err() {
            return -EFAULT;
        }
        let val = u64::from_le_bytes(b[0..8].try_into().unwrap_or([0; 8]));
        let uaddr = u64::from_le_bytes(b[8..16].try_into().unwrap_or([0; 8]));
        let wflags = u32::from_le_bytes(b[16..20].try_into().unwrap_or([0; 4]));
        let reserved = u32::from_le_bytes(b[20..24].try_into().unwrap_or([0; 4]));
        // Linux 6.6 compared `(u32)val`; newer kernels (CachyOS's 7.x, the ABI diff's oracle) refuse a value that
        // does not fit the futex size (`futex_validate_input`): EINVAL, as here.
        if wflags & !(FUTEX_32 | FUTEX_PRIVATE_FLAG) != 0 || reserved != 0 || wflags & FUTEX_32 == 0 || val > u32::MAX as u64 {
            return -EINVAL;
        }
        items.push(WaitvItem { addr: uaddr, val: val as u32, shared: wflags & FUTEX_PRIVATE_FLAG == 0 });
    }
    match futex::wait_multiple(&ProcSpace::new(p), &items, deadline) {
        Ok(n) => n,
        Err(e) => -e,
    }
}

fn sys_futex_waitv(c: &mut SysCtx<'_>) -> i64 {
    do_futex_waitv(c.proc, c.args)
}
crate::syscall_nr!(NR_FUTEX_WAITV, "futex_waitv", sys_futex_waitv);

/// What the futex code does when a thread of `p` ends (Linux `exit_mm_release`): walk its robust list
/// (OWNER_DIED, wake one), and when other threads share the address space (`mm_shared`) zero its
/// CLONE_CHILD_CLEARTID word and wake one waiter. Must run in the ending thread itself, with `p`'s address
/// space still loaded (before the thread's reference to it goes): the atomic word update needs it.
pub fn exit_release(p: &Arc<Process>, tid: u32, robust_head: u64, clear_child_tid: u64, mm_shared: bool) {
    futex::robust::thread_exit(&ProcSpace::new(p), tid, robust_head, clear_child_tid, mm_shared);
}
