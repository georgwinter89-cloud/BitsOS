//! FS-4's syscalls: poll, ppoll, select, pselect6 (linux-6.6/fs/select.c) and epoll_create, epoll_create1,
//! epoll_ctl, epoll_wait, epoll_pwait, epoll_pwait2 (fs/eventpoll.c), with Linux's argument checks and error
//! order, on the abstract user memory of `sys` (host tests run them unchanged; `vfs-sys` registers them).
//!
//! Waiting is the poll core's (`poll::PollTable`, `epoll::Epoll::wait`): wake-ups from every object's
//! `PollQueue`, deadlines on the monotonic clock (the scheduler's timed sleep, an hrtimer), the program's kill
//! state (`UserMem::interrupted`, Linux `fatal_signal_pending`) and a signal waiting for its handler
//! (`UserMem::signal_pending`) looked at between slices; as Linux, events found win over a pending signal and
//! only a kill ends an epoll harvest. A temporary signal mask (ppoll, pselect6, epoll_pwait, epoll_pwait2) is
//! set and put back through the calling program (`UserMem::set_sigmask`/`restore_sigmask`, AB's signal state;
//! put back only while it is still the one set). Until AB-6 delivers signals to handlers, `signal_pending` is
//! false (only a kill interrupts a wait) and the mask always goes back at once (Linux keeps it until the
//! handler ran when a signal interrupted the call: `restore_saved_sigmask_unless`).

use crate::epoll::{self, Epoll, EventSink};
use crate::fd::OpenFile;
use crate::poll::{self, Events, PollTable};
use crate::sys::{Args, Sys};
use crate::Errno;
use alloc::sync::Arc;
use alloc::vec::Vec;
use uapi::errno::*;

const NSEC: i64 = 1_000_000_000;
const SIGSET_SIZE: u64 = 8;

fn ret(r: Result<i64, Errno>) -> i64 {
    r.unwrap_or_else(|e| -e)
}

fn read_bytes<const N: usize>(s: &Sys<'_>, addr: u64) -> Result<[u8; N], Errno> {
    let mut b = [0u8; N];
    if s.mem.read(addr, &mut b) { Ok(b) } else { Err(EFAULT) }
}

/// `get_timespec64`: (sec, nsec) as the program wrote them.
fn read_timespec(s: &Sys<'_>, addr: u64) -> Result<(i64, i64), Errno> {
    let b: [u8; 16] = read_bytes(s, addr)?;
    Ok((i64::from_le_bytes(b[0..8].try_into().unwrap_or([0; 8])), i64::from_le_bytes(b[8..16].try_into().unwrap_or([0; 8]))))
}

/// A deadline: None = no end, Some(ZERO) = do not wait, Some(t) = monotonic ns.
type End = Option<u64>;
const ZERO: u64 = 0;

/// `poll_select_set_timeout`: EINVAL for an invalid timespec; zero stays zero (no wait), anything else is
/// now + ts (saturating, `timespec64_add_safe`).
fn set_timeout(sec: i64, nsec: i64) -> Result<u64, Errno> {
    if sec < 0 || !(0..NSEC).contains(&nsec) {
        return Err(EINVAL);
    }
    if sec == 0 && nsec == 0 {
        return Ok(ZERO);
    }
    let ns = (sec as u64).saturating_mul(NSEC as u64).saturating_add(nsec as u64);
    Ok(poll::now().saturating_add(ns).max(1))
}

/// How `poll_select_finish` writes the time left.
#[derive(Clone, Copy, PartialEq)]
enum TimeType {
    Timeval,
    Timespec,
}

/// `set_user_sigmask`: nothing without a mask; EINVAL for a size other than 8, EFAULT; (old, temporary) back.
fn set_user_sigmask(s: &Sys<'_>, umask: u64, size: u64) -> Result<Option<(u64, u64)>, Errno> {
    if umask == 0 {
        return Ok(None);
    }
    if size != SIGSET_SIZE {
        return Err(EINVAL);
    }
    let m = u64::from_le_bytes(read_bytes(s, umask)?);
    Ok(s.mem.set_sigmask(m))
}

/// `restore_saved_sigmask_unless(ret == -ERESTARTNOHAND)`: `r` is the call's answer.
fn restore_sigmask(s: &Sys<'_>, saved: Option<(u64, u64)>, r: i64) {
    if let Some((old, temp)) = saved {
        s.mem.restore_sigmask(old, temp, r == -EINTR && s.mem.signal_pending());
    }
}

/// A signal ends a wait that found nothing (`signal_pending`; a kill counts too).
fn stop(s: &Sys<'_>) -> bool {
    s.mem.interrupted() || s.mem.signal_pending()
}

/// `poll_select_finish` (the mask was put back by the caller): the time left goes back to `p` unless there is
/// no `p` or the timeout was zero; a fault there is ignored (the timespec may be read-only).
fn finish(s: &Sys<'_>, end: End, p: u64, ty: TimeType, r: i64) -> i64 {
    let Some(end) = end else { return r };
    if p == 0 || end == ZERO {
        return r;
    }
    let left = end.saturating_sub(poll::now());
    let (sec, nsec) = ((left / NSEC as u64) as i64, (left % NSEC as u64) as i64);
    let sub = if ty == TimeType::Timeval { nsec / 1000 } else { nsec };
    let mut b = [0u8; 16];
    b[0..8].copy_from_slice(&sec.to_le_bytes());
    b[8..16].copy_from_slice(&sub.to_le_bytes());
    let _ = s.mem.write(p, &b);
    r
}

/// An fd as `fdget` gives it: O_PATH files and reserved numbers are not files.
fn get_file(s: &Sys<'_>, fd: i64) -> Option<Arc<OpenFile>> {
    if fd < 0 || fd > i32::MAX as i64 {
        return None;
    }
    s.fds.get(fd as usize).ok().filter(|f| !f.is_path_only())
}

// ---- poll / ppoll ----

/// `do_sys_poll`.
fn do_sys_poll(s: &Sys<'_>, ufds: u64, nfds: u64, end: End) -> Result<i64, Errno> {
    if nfds > s.fds.limit() as u64 {
        return Err(EINVAL);
    }
    let n = nfds as usize;
    let mut raw: Vec<u8> = Vec::new();
    raw.try_reserve_exact(n * 8).map_err(|_| ENOMEM)?;
    raw.resize(n * 8, 0);
    if n > 0 && !s.mem.read(ufds, &mut raw) {
        return Err(EFAULT);
    }
    let mut table = PollTable::new(end != Some(ZERO));
    let mut timed_out = end == Some(ZERO);
    let count = loop {
        table.arm();
        let mut count: i64 = 0;
        for e in raw.chunks_exact_mut(8) {
            let fd = i32::from_le_bytes([e[0], e[1], e[2], e[3]]);
            let events = u16::from_le_bytes([e[4], e[5]]) as u32;
            // do_pollfd
            let mask = if fd < 0 {
                0
            } else {
                match get_file(s, fd as i64) {
                    Some(f) => {
                        let filter = events | Events::ERR | Events::HUP;
                        table.poll(&f.file, filter) & filter
                    }
                    None => Events::NVAL,
                }
            };
            e[6..8].copy_from_slice(&(mask as u16).to_le_bytes());
            if mask != 0 {
                count += 1;
                table.stop_registering();
            }
        }
        table.stop_registering();
        if count == 0 {
            if let Some(err) = table.error {
                count = -err;
            }
            if stop(s) {
                count = -EINTR;
            }
        }
        if count != 0 || timed_out {
            break count;
        }
        timed_out = table.sleep(end, &|| stop(s));
    };
    drop(table);
    for (i, e) in raw.chunks_exact(8).enumerate() {
        if !s.mem.write(ufds + i as u64 * 8 + 6, &e[6..8]) {
            return Err(EFAULT);
        }
    }
    Ok(count)
}

pub fn poll(s: &Sys<'_>, a: Args) -> i64 {
    let ms = a[2] as i32;
    let end = (ms >= 0).then(|| set_timeout(ms as i64 / 1000, (ms as i64 % 1000) * 1_000_000).unwrap_or(ZERO));
    ret(do_sys_poll(s, a[0], a[1] as u32 as u64, end))
}

pub fn ppoll(s: &Sys<'_>, a: Args) -> i64 {
    let [ufds, nfds, tsp, sigmask, sigsetsize, _] = a;
    let mut end = None;
    if tsp != 0 {
        let (sec, nsec) = match read_timespec(s, tsp) {
            Ok(t) => t,
            Err(e) => return -e,
        };
        end = match set_timeout(sec, nsec) {
            Ok(t) => Some(t),
            Err(e) => return -e,
        };
    }
    let old = match set_user_sigmask(s, sigmask, sigsetsize) {
        Ok(o) => o,
        Err(e) => return -e,
    };
    let r = ret(do_sys_poll(s, ufds, nfds as u32 as u64, end));
    restore_sigmask(s, old, r);
    finish(s, end, tsp, TimeType::Timespec, r)
}

// ---- select / pselect6 ----

const POLLIN_SET: u32 = Events::RDNORM | Events::RDBAND | Events::IN | Events::HUP | Events::ERR | Events::NVAL;
const POLLOUT_SET: u32 = Events::WRBAND | Events::WRNORM | Events::OUT | Events::ERR | Events::NVAL;
const POLLEX_SET: u32 = Events::PRI | Events::NVAL;

fn bit(set: &[u64], i: usize) -> bool {
    set.get(i / 64).is_some_and(|w| w & (1 << (i % 64)) != 0)
}

/// `get_fd_set`: `words` longs from the program, or zeros for NULL.
fn get_fd_set(s: &Sys<'_>, addr: u64, words: usize) -> Result<Vec<u64>, Errno> {
    let mut v: Vec<u64> = Vec::new();
    v.try_reserve_exact(words).map_err(|_| ENOMEM)?;
    v.resize(words, 0);
    if addr != 0 && words > 0 {
        let mut b: Vec<u8> = Vec::new();
        b.try_reserve_exact(words * 8).map_err(|_| ENOMEM)?;
        b.resize(words * 8, 0);
        if !s.mem.read(addr, &mut b) {
            return Err(EFAULT);
        }
        for (w, c) in v.iter_mut().zip(b.chunks_exact(8)) {
            *w = u64::from_le_bytes(c.try_into().unwrap_or([0; 8]));
        }
    }
    Ok(v)
}

fn set_fd_set(s: &Sys<'_>, addr: u64, set: &[u64]) -> bool {
    if addr == 0 || set.is_empty() {
        return true;
    }
    let mut b: Vec<u8> = Vec::new();
    if b.try_reserve_exact(set.len() * 8).is_err() {
        return false;
    }
    for w in set {
        b.extend_from_slice(&w.to_le_bytes());
    }
    s.mem.write(addr, &b)
}

/// `core_sys_select` + `do_select`.
fn core_sys_select(s: &Sys<'_>, n: i64, inp: u64, outp: u64, exp: u64, end: End) -> Result<i64, Errno> {
    if n < 0 {
        return Err(EINVAL);
    }
    // `max_fds` can only grow; read once (Linux caps n there: bits above it are neither read nor written).
    let n = (n as usize).min(s.fds.max_fds());
    let words = n.div_ceil(64);
    let fin = get_fd_set(s, inp, words)?;
    let fout = get_fd_set(s, outp, words)?;
    let fex = get_fd_set(s, exp, words)?;
    let mut rin = alloc::vec![0u64; words];
    let mut rout = alloc::vec![0u64; words];
    let mut rex = alloc::vec![0u64; words];
    // max_select_fd: every fd named must be open; the scan ends after the highest one.
    let mut max = 0usize;
    for i in 0..n {
        if bit(&fin, i) || bit(&fout, i) || bit(&fex, i) {
            if !s.fds.is_open(i) {
                return Err(EBADF);
            }
            max = i + 1;
        }
    }
    let mut table = PollTable::new(end != Some(ZERO));
    let mut timed_out = end == Some(ZERO);
    let mut retval: i64;
    loop {
        table.arm();
        retval = 0;
        rin.fill(0);
        rout.fill(0);
        rex.fill(0);
        for i in 0..max {
            let (bi, bo, be) = (bit(&fin, i), bit(&fout, i), bit(&fex, i));
            if !(bi || bo || be) {
                continue;
            }
            let mask = match get_file(s, i as i64) {
                Some(f) => {
                    let mut key = POLLEX_SET;
                    if bi {
                        key |= POLLIN_SET;
                    }
                    if bo {
                        key |= POLLOUT_SET;
                    }
                    table.poll(&f.file, key)
                }
                None => Events::NVAL,
            };
            let w = i / 64;
            let b = 1u64 << (i % 64);
            if mask & POLLIN_SET != 0 && bi {
                rin[w] |= b;
                retval += 1;
                table.stop_registering();
            }
            if mask & POLLOUT_SET != 0 && bo {
                rout[w] |= b;
                retval += 1;
                table.stop_registering();
            }
            if mask & POLLEX_SET != 0 && be {
                rex[w] |= b;
                retval += 1;
                table.stop_registering();
            }
        }
        table.stop_registering();
        if retval != 0 || timed_out || stop(s) {
            break;
        }
        if let Some(err) = table.error {
            return Err(err);
        }
        timed_out = table.sleep(end, &|| stop(s));
    }
    drop(table);
    if retval == 0 && stop(s) {
        return Err(EINTR);
    }
    if !set_fd_set(s, inp, &rin) || !set_fd_set(s, outp, &rout) || !set_fd_set(s, exp, &rex) {
        return Err(EFAULT);
    }
    Ok(retval)
}

pub fn select(s: &Sys<'_>, a: Args) -> i64 {
    let [n, inp, outp, exp, tvp, _] = a;
    let mut end = None;
    if tvp != 0 {
        let (sec, usec) = match read_timespec(s, tvp) {
            Ok(t) => t,
            Err(e) => return -e,
        };
        // tv_sec + tv_usec / USEC_PER_SEC, (tv_usec % USEC_PER_SEC) * NSEC_PER_USEC (C division).
        end = match set_timeout(sec.wrapping_add(usec / 1_000_000), (usec % 1_000_000) * 1000) {
            Ok(t) => Some(t),
            Err(e) => return -e,
        };
    }
    let r = ret(core_sys_select(s, n as i32 as i64, inp, outp, exp, end));
    finish(s, end, tvp, TimeType::Timeval, r)
}

pub fn pselect6(s: &Sys<'_>, a: Args) -> i64 {
    let [n, inp, outp, exp, tsp, sig] = a;
    // get_sigset_argpack: { const sigset_t *p; size_t size; }
    let (mask_p, mask_size) = if sig != 0 {
        match read_bytes::<16>(s, sig) {
            Ok(b) => (u64::from_le_bytes(b[0..8].try_into().unwrap_or([0; 8])), u64::from_le_bytes(b[8..16].try_into().unwrap_or([0; 8]))),
            Err(e) => return -e,
        }
    } else {
        (0, 0)
    };
    let mut end = None;
    if tsp != 0 {
        let (sec, nsec) = match read_timespec(s, tsp) {
            Ok(t) => t,
            Err(e) => return -e,
        };
        end = match set_timeout(sec, nsec) {
            Ok(t) => Some(t),
            Err(e) => return -e,
        };
    }
    let old = match set_user_sigmask(s, mask_p, mask_size) {
        Ok(o) => o,
        Err(e) => return -e,
    };
    let r = ret(core_sys_select(s, n as i32 as i64, inp, outp, exp, end));
    restore_sigmask(s, old, r);
    finish(s, end, tsp, TimeType::Timespec, r)
}

// ---- epoll ----

fn do_epoll_create(s: &Sys<'_>, flags: u64) -> Result<i64, Errno> {
    if flags & !epoll::EPOLL_CLOEXEC != 0 {
        return Err(EINVAL);
    }
    let f = epoll::new_epoll();
    s.fds.install_flags(f, 0, flags & epoll::EPOLL_CLOEXEC != 0).map(|fd| fd as i64)
}

pub fn epoll_create(s: &Sys<'_>, a: Args) -> i64 {
    if (a[0] as i32) <= 0 {
        return -EINVAL;
    }
    ret(do_epoll_create(s, 0))
}

pub fn epoll_create1(s: &Sys<'_>, a: Args) -> i64 {
    ret(do_epoll_create(s, a[0] as i32 as u32 as u64))
}

fn do_epoll_ctl(s: &Sys<'_>, epfd: i64, op: i32, fd: i64, event: u64) -> Result<i64, Errno> {
    let mut ev = (0u32, 0u64);
    if op != epoll::EPOLL_CTL_DEL {
        let b: [u8; 12] = read_bytes(s, event)?;
        ev = (u32::from_le_bytes(b[0..4].try_into().unwrap_or([0; 4])), u64::from_le_bytes(b[4..12].try_into().unwrap_or([0; 8])));
    }
    let f = get_file(s, epfd).ok_or(EBADF)?;
    let tf = get_file(s, fd).ok_or(EBADF)?;
    Epoll::ctl(&f, op, &tf, fd as i32, ev, s.cred.uid == 0).map(|_| 0)
}

pub fn epoll_ctl(s: &Sys<'_>, a: Args) -> i64 {
    ret(do_epoll_ctl(s, a[0] as i32 as i64, a[1] as i32, a[2] as i32 as i64, a[3]))
}

/// The program's `struct epoll_event` array (packed, 12 bytes each).
struct UserEvents<'a> {
    s: &'a Sys<'a>,
    at: u64,
}

impl EventSink for UserEvents<'_> {
    fn put(&mut self, events: u32, data: u64) -> bool {
        let mut b = [0u8; 12];
        b[0..4].copy_from_slice(&events.to_le_bytes());
        b[4..12].copy_from_slice(&data.to_le_bytes());
        if !self.s.mem.write(self.at, &b) {
            return false;
        }
        self.at += 12;
        true
    }
}

/// `do_epoll_wait`.
fn do_epoll_wait(s: &Sys<'_>, epfd: i64, events: u64, maxevents: i64, end: End) -> Result<i64, Errno> {
    if maxevents <= 0 || maxevents > epoll::EP_MAX_EVENTS {
        return Err(EINVAL);
    }
    if !crate::sys::access_ok(events, maxevents as u64 * 12) {
        return Err(EFAULT);
    }
    let f = get_file(s, epfd).ok_or(EBADF)?;
    let Some(ep) = f.file.as_epoll() else { return Err(EINVAL) };
    let mut sink = UserEvents { s, at: events };
    ep.wait_sig(&mut sink, maxevents as usize, end, &|| s.mem.interrupted(), &|| s.mem.signal_pending()).map(|n| n as i64)
}

/// `ep_timeout_to_timespec`: a negative timeout waits without end.
fn ms_end(ms: i32) -> End {
    (ms >= 0).then(|| set_timeout(ms as i64 / 1000, (ms as i64 % 1000) * 1_000_000).unwrap_or(ZERO))
}

pub fn epoll_wait(s: &Sys<'_>, a: Args) -> i64 {
    ret(do_epoll_wait(s, a[0] as i32 as i64, a[1], a[2] as i32 as i64, ms_end(a[3] as i32)))
}

/// `do_epoll_pwait`: the mask first, then the wait.
fn do_epoll_pwait(s: &Sys<'_>, a: Args, end: End) -> i64 {
    let old = match set_user_sigmask(s, a[4], a[5]) {
        Ok(o) => o,
        Err(e) => return -e,
    };
    let r = ret(do_epoll_wait(s, a[0] as i32 as i64, a[1], a[2] as i32 as i64, end));
    restore_sigmask(s, old, r);
    r
}

pub fn epoll_pwait(s: &Sys<'_>, a: Args) -> i64 {
    do_epoll_pwait(s, a, ms_end(a[3] as i32))
}

pub fn epoll_pwait2(s: &Sys<'_>, a: Args) -> i64 {
    let mut end = None;
    if a[3] != 0 {
        let (sec, nsec) = match read_timespec(s, a[3]) {
            Ok(t) => t,
            Err(e) => return -e,
        };
        end = match set_timeout(sec, nsec) {
            Ok(t) => Some(t),
            Err(e) => return -e,
        };
    }
    do_epoll_pwait(s, a, end)
}

/// FS-4's calls with their x86-64 numbers (the glue registers exactly these; `sys::call` dispatches them).
pub const CALLS: &[(usize, &str, crate::sys::SysFn)] = &[
    (7, "poll", poll),
    (23, "select", select),
    (213, "epoll_create", epoll_create),
    (232, "epoll_wait", epoll_wait),
    (233, "epoll_ctl", epoll_ctl),
    (270, "pselect6", pselect6),
    (271, "ppoll", ppoll),
    (281, "epoll_pwait", epoll_pwait),
    (291, "epoll_create1", epoll_create1),
    (441, "epoll_pwait2", epoll_pwait2),
];
