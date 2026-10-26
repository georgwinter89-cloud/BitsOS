//! timerfd (AB-5), after linux-6.6/fs/timerfd.c: `timerfd_create` (283), `timerfd_settime` (286),
//! `timerfd_gettime` (287) and the file they make (read, poll, TFD_IOC_SET_TICKS through `ioctl.rs`).
//!
//! The state is Linux's `timerfd_ctx`: `expires` (in the clock's own time, ns), `interval`, `ticks` (expiries
//! not read yet), `expired` (the timer fired since it was last armed), the cancel-on-set fields. An expiry is a
//! CP-5 `HrTimer` callback (`timerfd_tmrproc`): it sets `expired`, adds one tick and wakes readers and pollers
//! with EPOLLIN. A periodic timer is **not** re-armed by its callback: read and gettime move it forward past
//! now and count the periods missed meanwhile (`hrtimer_forward_now`), so a 1 ns interval cannot keep a CPU in
//! interrupts and EPOLLET sees one edge per read, exactly as Linux.
//!
//! Clocks: CLOCK_MONOTONIC and CLOCK_BOOTTIME (= MONOTONIC: V2 does not suspend) count `time::now_ns()`,
//! CLOCK_REALTIME counts CP-5's wall clock; the deadline handed to the `HrTimer` is the expiry minus the
//! clock's offset from the monotonic clock at arming time. The alarm clocks (REALTIME_ALARM, BOOTTIME_ALARM)
//! need CAP_WAKE_ALARM (root) and run on their base clock (V2 never suspends, so there is nothing to wake
//! from). TFD_TIMER_CANCEL_ON_SET works through `clock_was_set`, which a future clock_settime/settimeofday
//! calls when the wall clock jumps; V2 has no such call yet, so such a timer is never cancelled today.
//!
//! The file is an anonymous inode (fstat as eventfd's), F_GETFL = O_RDWR plus O_NONBLOCK, lseek answers 0,
//! write EINVAL (no write operation), pread/pwrite ESPIPE, `/proc/<pid>/fd` shows `anon_inode:[timerfd]`.
//! Reads take the whole iovec (`read_iter`, as Linux since 6.8 and CachyOS, the ABI oracle; 6.6 read per
//! segment, so a readv whose first segment is shorter than 8 bytes was EINVAL there).
//!
//! The `HrTimer` callback finds its timer through `SLOTS` (a slab indexed by the callback argument): it works
//! on the slab's reference under the slab's IRQ-safe lock and never takes or drops a reference itself, so a
//! timer is only ever freed in thread context (the file's last close: the slot is emptied first, then the
//! timer cancelled and dropped). Locks: `ab.timerfd.slots` > `ab.timerfd` > `vfs.poll.*`, all IRQ-safe.

use crate::SysCtx;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use ksync::{IrqSpinLock, WaitQueue};
use time::HrTimer;
use uapi::ab::clock::*;
use uapi::ab::errno_more::ECANCELED;
use uapi::ab::timerfd::*;
use uapi::errno::*;
use vfs::abi::Attr;
use vfs::poll::{Events, PollQueue, Pollable};
use vfs::{Errno, File, OpenFile};

const NSEC: i64 = 1_000_000_000;
/// `KTIME_MAX`.
const KTIME_MAX: i64 = i64::MAX;
/// `KTIME_SEC_MAX`: a timespec with more seconds is KTIME_MAX (`timespec64_to_ktime`).
const KTIME_SEC_MAX: i64 = KTIME_MAX / NSEC;
/// A blocked read looks at the kill state this often (B4; expiries wake it at once).
const SLICE_NS: u64 = 50_000_000;
/// `struct itimerspec` (two timespecs: it_interval, then it_value).
const ITIMERSPEC_SIZE: usize = 32;
const O_RDWR: u64 = 2;

klog::counter!(TIMERFDS, "ab.timerfds");
klog::counter!(EXPIRIES, "ab.timerfd_expiries");

/// The clock a timerfd counts on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TfdClock {
    Monotonic,
    Realtime,
}

impl TfdClock {
    /// The clock's time now (ns, as ktime).
    fn now(self) -> i64 {
        match self {
            TfdClock::Monotonic => time::now_ns() as i64,
            TfdClock::Realtime => crate::clock::realtime_ns() as i64,
        }
    }
    /// This clock minus the monotonic clock (0 for MONOTONIC/BOOTTIME).
    fn offset(self) -> i64 {
        match self {
            TfdClock::Monotonic => 0,
            TfdClock::Realtime => crate::clock::realtime_offset_ns(),
        }
    }
}

/// `timespec64_to_ktime`: KTIME_MAX for seconds at or above KTIME_SEC_MAX.
pub(crate) fn ktime(sec: i64, nsec: i64) -> i64 {
    if sec >= KTIME_SEC_MAX { KTIME_MAX } else { sec * NSEC + nsec }
}

/// `timespec64_valid` (tv_sec >= 0, 0 <= tv_nsec < 1e9).
fn ts_valid(sec: i64, nsec: i64) -> bool {
    sec >= 0 && (0..NSEC).contains(&nsec)
}

/// `hrtimer_forward(expires, now, interval)`: moves `expires` past `now` by whole intervals; returns the new
/// expiry and the overrun count (0 when `now` is before `expires`). `interval` is at least 1 ns (the hrtimer
/// resolution). Additions saturate at KTIME_MAX (`ktime_add_safe`).
pub(crate) fn forward(expires: i64, now: i64, interval: i64) -> (i64, u64) {
    let delta = now.saturating_sub(expires);
    if delta < 0 {
        return (expires, 0);
    }
    let interval = interval.max(1);
    let mut orun: u64 = 1;
    let mut e = expires;
    if delta >= interval {
        let n = (delta / interval) as u64;
        e = e.saturating_add((interval as i128 * n as i128).min(KTIME_MAX as i128) as i64);
        if e > now {
            return (e, n);
        }
        orun = n + 1;
    }
    (e.saturating_add(interval), orun)
}

/// `timerfd_ctx` (under `Shared::st`).
struct Ctx {
    /// The expiry in `base`'s time (Linux `hrtimer_get_expires`; 0 when set to "disarmed").
    expires: i64,
    /// The clock the current setting runs on: CLOCK_REALTIME only for an absolute REALTIME setting; a
    /// relative one runs on the monotonic clock, as `__hrtimer_init` turns CLOCK_REALTIME + HRTIMER_MODE_REL
    /// into CLOCK_MONOTONIC (linux-6.6/kernel/time/hrtimer.c), so a wall-clock jump never moves it.
    base: TfdClock,
    interval: i64,
    ticks: u64,
    expired: bool,
    /// The HrTimer is queued for the current setting (cleared when it fires or is cancelled).
    active: bool,
    /// The monotonic deadline the HrTimer was armed for.
    deadline_mono: u64,
    settime_flags: u64,
    might_cancel: bool,
    /// `moffs`: the wall clock's offset when last looked at; None = KTIME_MAX (a jump was seen).
    moffs: Option<i64>,
}

impl Ctx {
    /// `timerfd_canceled`: a cancel-on-set timer whose clock jumped reports it once.
    fn canceled(&mut self, clock: TfdClock) -> bool {
        if !self.might_cancel || self.moffs.is_some() {
            return false;
        }
        self.moffs = Some(clock.offset());
        true
    }
    /// `timerfd_get_remaining`: the time to the expiry, 0 once past.
    fn remaining(&self) -> i64 {
        self.expires.saturating_sub(self.base.now()).max(0)
    }
    /// `hrtimer_forward_now` of an expired periodic timer, then `hrtimer_restart`: the overruns.
    fn forward_and_restart(&mut self, sh: &Shared) -> u64 {
        let (e, n) = forward(self.expires, self.base.now(), self.interval);
        self.expires = e;
        self.arm(sh);
        n
    }
    /// Queues the HrTimer at `expires` (turned into a monotonic deadline now).
    fn arm(&mut self, sh: &Shared) {
        let mono = (self.expires as i128 - self.base.offset() as i128).clamp(0, u64::MAX as i128) as u64;
        self.deadline_mono = mono;
        self.active = true;
        sh.timer.arm(mono);
    }
}

pub(crate) struct Shared {
    clock: TfdClock,
    /// AB-7a: the clock id as created (`ctx->clockid`, /proc/<pid>/fdinfo's `clockid`).
    clockid: i32,
    /// The clock id as created (alarm clocks keep their id for the CAP_WAKE_ALARM check at settime).
    alarm: bool,
    st: IrqSpinLock<Ctx>,
    /// `ctx->wqh`: readers and poll/epoll subscribers.
    wq: PollQueue,
    timer: HrTimer,
}

impl Shared {
    /// `timerfd_tmrproc` (hard-IRQ context, under `SLOTS`): the expiry of the current setting.
    fn fired(&self) {
        let woke = {
            let mut c = self.st.lock();
            if !c.active {
                // A callback of an earlier setting that ran late (settime replaced it meanwhile).
                false
            } else if time::now_ns() < c.deadline_mono {
                // Early (should not happen): the HrTimer goes again at the deadline.
                self.timer.arm(c.deadline_mono);
                false
            } else {
                c.active = false;
                c.expired = true;
                c.ticks = c.ticks.wrapping_add(1);
                true
            }
        };
        if woke {
            EXPIRIES.inc();
            self.wq.wake_poll(Events::IN);
        }
    }
}

enum Slot {
    Free,
    Reserved,
    Live(Arc<Shared>),
}

struct Slab {
    v: Vec<Slot>,
    free: Vec<usize>,
}

static SLOTS: IrqSpinLock<Slab> = IrqSpinLock::named("ab.timerfd.slots", Slab { v: Vec::new(), free: Vec::new() });

/// A slot for a new timerfd (grown outside the lock: the callback side never allocates).
fn reserve_slot() -> Result<usize, Errno> {
    loop {
        let len = {
            let mut s = SLOTS.lock();
            if let Some(i) = s.free.pop() {
                s.v[i] = Slot::Reserved;
                return Ok(i);
            }
            s.v.len()
        };
        let want = (len * 2).max(16);
        let mut nv: Vec<Slot> = Vec::new();
        let mut nf: Vec<usize> = Vec::new();
        nv.try_reserve_exact(want).map_err(|_| ENOMEM)?;
        nf.try_reserve_exact(want).map_err(|_| ENOMEM)?;
        let old = {
            let mut s = SLOTS.lock();
            if s.v.len() != len {
                continue;
            }
            nv.extend(s.v.drain(..));
            nv.resize_with(want, || Slot::Free);
            nf.extend(s.free.drain(..));
            nf.extend((len..want).rev());
            (core::mem::replace(&mut s.v, nv), core::mem::replace(&mut s.free, nf))
        };
        drop(old);
    }
}

fn set_slot(i: usize, sh: Arc<Shared>) {
    SLOTS.lock().v[i] = Slot::Live(sh);
}

/// Empties slot `i`; the reference it held is dropped by the caller, outside the lock.
fn free_slot(i: usize) -> Option<Arc<Shared>> {
    let mut s = SLOTS.lock();
    let old = core::mem::replace(&mut s.v[i], Slot::Free);
    // `free` has room for every slot (reserved at growth).
    s.free.push(i);
    match old {
        Slot::Live(a) => Some(a),
        _ => None,
    }
}

/// The HrTimer callback (argument = slot).
fn timer_fired(slot: usize) {
    let s = SLOTS.lock();
    if let Some(Slot::Live(sh)) = s.v.get(slot) {
        sh.fired();
    }
}

/// The open object behind a timerfd's fd.
pub(crate) struct TimerFd {
    sh: Arc<Shared>,
    slot: usize,
}

impl Drop for TimerFd {
    /// `timerfd_release`: no callback can reach the timer after this (slot emptied first, then cancelled);
    /// the last reference (and the HrTimer) goes here, in thread context.
    fn drop(&mut self) {
        let held = free_slot(self.slot);
        self.sh.timer.cancel();
        drop(held);
        forget_object(self as *const TimerFd as usize);
    }
}

/// Every live timerfd object by its address (settime/gettime/ioctl find theirs from an fd's `File`).
static OBJECTS: ksync::SpinLock<alloc::collections::BTreeMap<usize, (Arc<Shared>, usize)>> = ksync::SpinLock::named("ab.timerfd.objects", alloc::collections::BTreeMap::new());

fn forget_object(addr: usize) {
    let gone = OBJECTS.lock().remove(&addr);
    drop(gone);
}

/// The timerfd behind `f`, if it is one (`timerfd_fget`'s `f_op == &timerfd_fops`).
pub(crate) fn of(f: &OpenFile) -> Option<Arc<Shared>> {
    let addr = Arc::as_ptr(&f.file) as *const u8 as usize;
    OBJECTS.lock().get(&addr).map(|(sh, _)| sh.clone())
}

/// AB-7a: `timerfd_show` (linux-6.6/fs/timerfd.c :318) for `/proc/<pid>/fdinfo`: None for other files.
pub(crate) fn fdinfo(f: &OpenFile) -> Option<String> {
    let sh = of(f)?;
    // Copied under the IRQ-safe lock, formatted after it.
    let (ticks, flags, value, interval) = {
        let c = sh.st.lock();
        (c.ticks, c.settime_flags, c.remaining(), c.interval)
    };
    let ts = |ns: i64| (ns.max(0) / NSEC, ns.max(0) % NSEC);
    let (vs, vn) = ts(value);
    let (is, inn) = ts(interval);
    Some(alloc::format!(
        "clockid: {}\nticks: {ticks}\nsettime flags: 0{flags:o}\nit_value: ({vs}, {vn})\nit_interval: ({is}, {inn})\n",
        sh.clockid
    ))
}

/// The anonymous inode every anon file shares (the same answer eventfd's fstat gives).
pub(crate) fn anon_attr() -> Attr {
    static ANON: ksync::Once<Attr> = ksync::Once::new();
    *ANON.get_or_init(|| File::attr(&*vfs::eventfd::EventFd::new(0, 0)).unwrap_or_default())
}

impl TimerFd {
    /// `timerfd_read` (8-byte reads of the expiry count).
    fn read_ticks(&self, len: usize, nonblock: bool) -> Result<u64, Errno> {
        if len < 8 {
            return Err(EINVAL);
        }
        let sh = &self.sh;
        loop {
            {
                let mut c = sh.st.lock();
                if c.ticks != 0 || nonblock {
                    if c.canceled(sh.clock) {
                        c.ticks = 0;
                        c.expired = false;
                        return Err(ECANCELED);
                    }
                    if c.ticks == 0 {
                        return Err(EAGAIN);
                    }
                    let mut ticks = c.ticks;
                    if c.expired && c.interval != 0 {
                        ticks = ticks.wrapping_add(c.forward_and_restart(sh)).wrapping_sub(1);
                    }
                    c.expired = false;
                    c.ticks = 0;
                    return Ok(ticks);
                }
            }
            if vfs::dentry::interrupted() {
                return Err(EINTR);
            }
            sh.wq.wait_until_timeout(|| sh.st.lock().ticks != 0 || vfs::dentry::interrupted(), SLICE_NS);
        }
    }
}

impl File for TimerFd {
    fn read_at(&self, _pos: u64, buf: &mut [u8]) -> Result<usize, Errno> {
        self.read_stream(buf, true)
    }
    fn write_at(&self, _pos: u64, _buf: &[u8]) -> Result<usize, Errno> {
        Err(EINVAL)
    }
    fn read_stream(&self, buf: &mut [u8], nonblock: bool) -> Result<usize, Errno> {
        let t = self.read_ticks(buf.len(), nonblock)?;
        buf[..8].copy_from_slice(&t.to_le_bytes());
        Ok(8)
    }
    /// `timerfd_read_iter` (Linux 6.8+, CachyOS; 6.6 had a plain `read`): the whole iovec counts, fewer than 8
    /// bytes is EINVAL, a copy that faults after the expiries were taken is EFAULT (they are lost, as Linux).
    fn read_iter(&self, to: &mut dyn vfs::node::IoSink, status: u64) -> Option<Result<usize, Errno>> {
        let r = self.read_ticks(to.left(), status & uapi::ab::timerfd::TFD_NONBLOCK != 0);
        Some(r.and_then(|t| if to.copy(&t.to_le_bytes()) == 8 { Ok(8) } else { Err(EFAULT) }))
    }
    /// No write operation: `vfs_write` answers EINVAL (FMODE_CAN_WRITE is not set).
    fn write_stream(&self, _buf: &[u8], _nonblock: bool) -> Result<usize, Errno> {
        Err(EINVAL)
    }
    fn mode(&self) -> u32 {
        0o600
    }
    /// `timerfd_poll`: EPOLLIN while expiries wait to be read.
    fn events(&self) -> Events {
        Events(if self.sh.st.lock().ticks != 0 { Events::IN } else { 0 })
    }
    fn lseek(&self, _off: i64, _whence: u32) -> Option<Result<u64, Errno>> {
        Some(Ok(0))
    }
    fn attr(&self) -> Option<Attr> {
        Some(anon_attr())
    }
    fn poll_head(&self) -> Option<&PollQueue> {
        Some(&self.sh.wq)
    }
    fn can_poll(&self) -> bool {
        true
    }
    fn poll_exact(&self) -> bool {
        true
    }
    fn link_name(&self) -> Option<String> {
        Some(String::from("anon_inode:[timerfd]"))
    }
}

impl Pollable for TimerFd {
    fn events(&self) -> Events {
        File::events(self)
    }
    fn wait_queue(&self) -> &WaitQueue {
        &self.sh.wq
    }
}

/// `timerfd_create(clockid, flags)`.
fn sys_timerfd_create(c: &mut SysCtx<'_>) -> i64 {
    let clockid = c.args[0] as i32;
    let flags = c.args[1] as u32 as u64;
    let (clock, alarm) = match clockid {
        CLOCK_MONOTONIC | CLOCK_BOOTTIME => (TfdClock::Monotonic, false),
        CLOCK_REALTIME => (TfdClock::Realtime, false),
        CLOCK_REALTIME_ALARM => (TfdClock::Realtime, true),
        CLOCK_BOOTTIME_ALARM => (TfdClock::Monotonic, true),
        _ => return -EINVAL,
    };
    if flags & !TFD_CREATE_FLAGS != 0 {
        return -EINVAL;
    }
    if alarm && !c.proc.capable(proc::cred::Cap::WakeAlarm) {
        return -EPERM;
    }
    match create(clockid, clock, alarm, flags) {
        Ok(f) => c.proc.fds.install_flags(f, 0, flags & TFD_CLOEXEC != 0).map(|fd| fd as i64).unwrap_or_else(|e| -e),
        Err(e) => -e,
    }
}
crate::syscall_nr!(uapi::ab::nr_more::TIMERFD_CREATE, "timerfd_create", sys_timerfd_create);

/// A new timerfd's open file (also for kernel tests).
pub(crate) fn create(clockid: i32, clock: TfdClock, alarm: bool, flags: u64) -> Result<Arc<OpenFile>, Errno> {
    let slot = reserve_slot()?;
    let sh = Arc::new(Shared {
        clock,
        clockid,
        alarm,
        st: IrqSpinLock::named(
            "ab.timerfd",
            Ctx { expires: 0, base: TfdClock::Monotonic, interval: 0, ticks: 0, expired: false, active: false, deadline_mono: 0, settime_flags: 0, might_cancel: false, moffs: Some(clock.offset()) },
        ),
        wq: PollQueue::new(),
        timer: HrTimer::new(timer_fired, slot),
    });
    let obj = Arc::new(TimerFd { sh: sh.clone(), slot });
    OBJECTS.lock().insert(Arc::as_ptr(&obj) as *const u8 as usize, (sh.clone(), slot));
    set_slot(slot, sh);
    TIMERFDS.inc();
    Ok(OpenFile::anon(obj, O_RDWR | (flags & TFD_NONBLOCK)))
}

/// The timerfd behind fd `fd` (`timerfd_fget`: EBADF without an fd, EINVAL for another file).
fn fget(c: &SysCtx<'_>, fd: u64) -> Result<Arc<Shared>, i64> {
    let fd = fd as i32;
    if fd < 0 {
        return Err(-EBADF);
    }
    match c.proc.fds.get(fd as usize) {
        Ok(f) if !f.is_path_only() => of(&f).ok_or(-EINVAL),
        _ => Err(-EBADF),
    }
}

fn itimerspec_bytes(interval: i64, value: i64) -> [u8; ITIMERSPEC_SIZE] {
    let mut b = [0u8; ITIMERSPEC_SIZE];
    for (i, v) in [interval / NSEC, interval % NSEC, value / NSEC, value % NSEC].into_iter().enumerate() {
        b[i * 8..i * 8 + 8].copy_from_slice(&v.to_le_bytes());
    }
    b
}

fn word(b: &[u8], i: usize) -> i64 {
    i64::from_le_bytes(b[i * 8..i * 8 + 8].try_into().unwrap_or([0; 8]))
}

/// `do_timerfd_settime` once the arguments are checked: the old (remaining, interval) or ECANCELED.
pub(crate) fn settime(sh: &Shared, flags: u64, interval: i64, value: i64) -> (i64, i64, Result<(), Errno>) {
    let mut c = sh.st.lock();
    // `timerfd_setup_cancel`.
    c.might_cancel = sh.clock == TfdClock::Realtime && flags & TFD_TIMER_ABSTIME != 0 && flags & TFD_TIMER_CANCEL_ON_SET != 0;
    // The running setting stops (a callback still on its way finds `active` false).
    sh.timer.cancel();
    c.active = false;
    if c.expired && c.interval != 0 {
        let (e, _) = forward(c.expires, c.base.now(), c.interval);
        c.expires = e;
    }
    let old = (c.remaining(), c.interval);
    // `timerfd_setup`.
    c.expired = false;
    c.ticks = 0;
    c.interval = interval;
    c.expires = 0;
    let mut r = Ok(());
    if value != 0 {
        c.base = if flags & TFD_TIMER_ABSTIME != 0 { sh.clock } else { TfdClock::Monotonic };
        c.expires = if flags & TFD_TIMER_ABSTIME != 0 { value } else { c.base.now().saturating_add(value) };
        c.arm(sh);
        if c.canceled(sh.clock) {
            r = Err(ECANCELED);
        }
    }
    if r.is_ok() {
        c.settime_flags = flags & TFD_SETTIME_FLAGS;
    }
    (old.0, old.1, r)
}

/// `timerfd_settime(fd, flags, new, old)`.
fn sys_timerfd_settime(c: &mut SysCtx<'_>) -> i64 {
    let [fd, flags, new, old, ..] = c.args;
    let flags = flags as u32 as u64;
    let mut b = [0u8; ITIMERSPEC_SIZE];
    if c.proc.mm.copy_from_user(&mut b, new).is_err() {
        return -EFAULT;
    }
    let (isec, insec, vsec, vnsec) = (word(&b, 0), word(&b, 1), word(&b, 2), word(&b, 3));
    if flags & !TFD_SETTIME_FLAGS != 0 || !ts_valid(isec, insec) || !ts_valid(vsec, vnsec) {
        return -EINVAL;
    }
    let sh = match fget(c, fd) {
        Ok(s) => s,
        Err(e) => return e,
    };
    if sh.alarm && !c.proc.capable(proc::cred::Cap::WakeAlarm) {
        return -EPERM;
    }
    let (rem, ival, r) = settime(&sh, flags, ktime(isec, insec), ktime(vsec, vnsec));
    if let Err(e) = r {
        return -e;
    }
    if old != 0 && c.proc.mm.copy_to_user(old, &itimerspec_bytes(ival, rem)).is_err() {
        return -EFAULT;
    }
    0
}
crate::syscall_nr!(uapi::ab::nr_more::TIMERFD_SETTIME, "timerfd_settime", sys_timerfd_settime);

/// `do_timerfd_gettime`: an expired periodic timer moves on first (its missed periods become ticks).
pub(crate) fn gettime(sh: &Shared) -> (i64, i64) {
    let mut c = sh.st.lock();
    if c.expired && c.interval != 0 {
        c.expired = false;
        let n = c.forward_and_restart(sh);
        c.ticks = c.ticks.wrapping_add(n).wrapping_sub(1);
    }
    (c.remaining(), c.interval)
}

fn sys_timerfd_gettime(c: &mut SysCtx<'_>) -> i64 {
    let [fd, out, ..] = c.args;
    let sh = match fget(c, fd) {
        Ok(s) => s,
        Err(e) => return e,
    };
    let (rem, ival) = gettime(&sh);
    c.proc.mm.copy_to_user(out, &itimerspec_bytes(ival, rem)).map(|_| 0).unwrap_or(-EFAULT)
}
crate::syscall_nr!(uapi::ab::nr_more::TIMERFD_GETTIME, "timerfd_gettime", sys_timerfd_gettime);

/// TFD_IOC_SET_TICKS (`timerfd_ioctl`, CONFIG_CHECKPOINT_RESTORE): sets the expiry count.
pub(crate) fn set_ticks(c: &SysCtx<'_>, sh: &Shared, arg: u64) -> i64 {
    let mut b = [0u8; 8];
    if c.proc.mm.copy_from_user(&mut b, arg).is_err() {
        return -EFAULT;
    }
    let ticks = u64::from_le_bytes(b);
    if ticks == 0 {
        return -EINVAL;
    }
    let ok = {
        let mut st = sh.st.lock();
        if st.canceled(sh.clock) {
            false
        } else {
            st.ticks = ticks;
            true
        }
    };
    if !ok {
        return -ECANCELED;
    }
    sh.wq.wake_poll(Events::IN);
    0
}

/// `timerfd_clock_was_set` + `clock_was_set` for timerfds: the wall clock jumped. Cancel-on-set timers whose
/// offset changed report it (one tick, ECANCELED on the next read); REALTIME timers are queued again for their
/// expiry at the new offset. For a future clock_settime/settimeofday (V2 has neither yet); the ktest
/// `ab.ab5.timerfd_cancel_on_set` moves the wall clock itself.
pub fn clock_was_set() {
    let all: Vec<Arc<Shared>> = OBJECTS.lock().values().map(|(s, _)| s.clone()).collect();
    for sh in all {
        if sh.clock != TfdClock::Realtime {
            continue;
        }
        let moffs = sh.clock.offset();
        let woke = {
            let mut c = sh.st.lock();
            if c.active && c.base == TfdClock::Realtime {
                // An absolute REALTIME setting: same expiry, new monotonic deadline (relative ones run on the
                // monotonic clock and stay).
                c.arm(&sh);
            }
            if c.might_cancel && c.moffs != Some(moffs) {
                c.moffs = None;
                c.ticks = c.ticks.wrapping_add(1);
                true
            } else {
                false
            }
        };
        if woke {
            sh.wq.wake_poll(Events::IN);
        }
    }
}

/// How many timerfds exist (tests).
pub(crate) fn count() -> usize {
    OBJECTS.lock().len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forward_as_hrtimer_forward() {
        // Not expired yet: nothing moves.
        assert_eq!(forward(100, 50, 10), (100, 0));
        // Exactly at the expiry: one period.
        assert_eq!(forward(100, 100, 10), (110, 1));
        // Three periods missed: 100, 110, 120 passed at now = 125.
        assert_eq!(forward(100, 125, 10), (130, 3));
        // now on a period edge: the expiry at now counts, the next is later.
        assert_eq!(forward(100, 130, 10), (140, 4));
        // Interval 0 is 1 ns (the resolution).
        assert_eq!(forward(5, 7, 0), (8, 3));
        // Saturation at KTIME_MAX.
        assert_eq!(forward(KTIME_MAX - 5, KTIME_MAX, 10).0, KTIME_MAX);
    }

    #[test]
    fn ktime_of_timespec() {
        assert_eq!(ktime(1, 5), NSEC + 5);
        assert_eq!(ktime(KTIME_SEC_MAX, 0), KTIME_MAX);
        assert_eq!(ktime(KTIME_SEC_MAX - 1, 999_999_999), (KTIME_SEC_MAX - 1) * NSEC + 999_999_999);
        assert!(ts_valid(0, 0) && ts_valid(5, 999_999_999));
        assert!(!ts_valid(-1, 0) && !ts_valid(0, NSEC) && !ts_valid(0, -1));
    }

    #[test]
    fn itimerspec_layout() {
        let b = itimerspec_bytes(2 * NSEC + 3, 7 * NSEC + 9);
        assert_eq!([word(&b, 0), word(&b, 1), word(&b, 2), word(&b, 3)], [2, 3, 7, 9], "it_interval first, then it_value");
    }
}
