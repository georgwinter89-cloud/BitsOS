//! SN-9: moving recorded frames to the program: READI_FRAMES and read(2) (`__snd_pcm_lib_xfer` for capture,
//! `linux-6.6/sound/core/pcm_lib.c:2207`: the automatic start at :2261, `wait_for_avail` :1885), appl_ptr
//! moves (SYNC_PTR, FORWARD: frames dropped unread), and the conversion from the source's 16-bit stereo to
//! the program's format and channels.

use super::abi::*;
use super::cpcm::{CapFile, Phase, St};
use super::pcm::try_buf;
use crate::client::SndError;
use uapi::errno::*;
use vfs::sys::UserMem;
use vfs::Errno;

/// Frames per step (one source read, one conversion, one copy to the program).
const STEP: usize = 1024;
/// Waits look at signals and kills this often (B4).
const SLICE_NS: u64 = 50_000_000;
const ERESTARTSYS: i64 = uapi::ab::siginfo::ERESTARTSYS;

pub(crate) enum Dst<'a> {
    User(u64, &'a dyn UserMem),
    Kernel(&'a mut [u8]),
}

/// One 16-bit sample as the program's sample: S32 in the upper half, FLOAT_LE as v / 32768 (exact: built from
/// its bits, no floating point in the kernel).
pub fn from_s16(format: u32, v: i16, out: &mut [u8]) {
    match format {
        FORMAT_S32_LE => out[..4].copy_from_slice(&((v as i32) << 16).to_le_bytes()),
        FORMAT_FLOAT_LE => out[..4].copy_from_slice(&s16_to_f32(v).to_le_bytes()),
        _ => out[..2].copy_from_slice(&v.to_le_bytes()),
    }
}

/// IEEE 754 single bits of `v / 32768`.
pub fn s16_to_f32(v: i16) -> u32 {
    if v == 0 {
        return 0;
    }
    let sign = if v < 0 { 1u32 << 31 } else { 0 };
    let m = (v as i32).unsigned_abs();
    let e = 31 - m.leading_zeros();
    // m = 2^e * 1.f; value = m / 2^15.
    let frac = (m << (23 - e)) & 0x7f_ffff;
    sign | ((e + 127 - 15) << 23) | frac
}

/// Stereo frames to the program's layout (`channels` 1: (L + R) / 2).
fn convert(format: u32, channels: usize, src: &[[i16; 2]], out: &mut [u8]) {
    let w = if format == FORMAT_S16_LE { 2 } else { 4 };
    for (i, f) in src.iter().enumerate() {
        let at = i * w * channels;
        if channels == 1 {
            from_s16(format, ((f[0] as i32 + f[1] as i32) >> 1) as i16, &mut out[at..at + w]);
        } else {
            from_s16(format, f[0], &mut out[at..at + w]);
            from_s16(format, f[1], &mut out[at + w..at + 2 * w]);
        }
    }
}

fn interrupted(mem: Option<&dyn UserMem>) -> bool {
    match mem {
        Some(m) => m.interrupted() || m.signal_pending(),
        None => vfs::dentry::interrupted(),
    }
}

fn wait_ns(buffer: u64, rate: u32) -> u64 {
    (buffer * 1100 / rate.max(1) as u64).max(100) * 1_000_000
}

/// `pcm_accessible_state` (pcm_lib.c:2144): DRAINING is not (a capture read after drain is EBADFD on 6.6).
fn accessible(st: &St) -> Result<(), Errno> {
    match st.state() {
        STATE_PREPARED | STATE_RUNNING | STATE_PAUSED => Ok(()),
        STATE_XRUN => Err(EPIPE),
        STATE_DISCONNECTED => Err(ENODEV),
        _ => Err(EBADFD),
    }
}

/// `snd_pcm_xferi_frames_ioctl` (pcm_native.c:3204).
pub(super) fn xferi(f: &CapFile, arg: u64, mem: &dyn UserMem) -> Result<i64, Errno> {
    if f.st.lock().state() == STATE_OPEN {
        return Err(EBADFD);
    }
    if !mem.write(arg, &0u64.to_le_bytes()) {
        return Err(EFAULT);
    }
    let mut b = [0u8; XFERI_SIZE];
    if !mem.read(arg, &mut b) {
        return Err(EFAULT);
    }
    let (buf, frames) = (get_u64(&b, 8), get_u64(&b, 16));
    // pcm_lib.c:2237: a capture without a buffer is EINVAL (playback would write silence).
    if buf == 0 {
        return Err(EINVAL);
    }
    let r = read(f, Dst::User(buf, mem), frames, None, Some(mem));
    let v = match r {
        Ok(n) => n as i64,
        Err(e) => -e,
    };
    if !mem.write(arg, &v.to_le_bytes()) {
        return Err(EFAULT);
    }
    r.map(|_| 0)
}

/// Whether `len` bytes at `addr` can be written: one zero byte per page and the last byte (faults are per page;
/// the bytes are overwritten by the frames right after). No wrap past the top of the address space.
fn probe(m: &dyn UserMem, addr: u64, len: usize) -> bool {
    if len == 0 {
        return true;
    }
    let Some(last) = addr.checked_add(len as u64 - 1) else { return false };
    let mut a = addr;
    while a < last {
        if !m.write(a, &[0]) {
            return false;
        }
        a = (a & !0xfff) + 0x1000;
    }
    m.write(last, &[0])
}

/// The transfer loop. `nonblock`: read(2) knows it; None = look it up (ioctl).
pub(crate) fn read(f: &CapFile, mut dst: Dst<'_>, frames: u64, nonblock: Option<bool>, mem: Option<&dyn UserMem>) -> Result<u64, Errno> {
    // An automatic start goes through `trans` as the ioctls do; taken only when a start is due (not per read).
    let mut trans = None;
    let (cap, format, channels, fb, buffer, rate, start) = loop {
        let mut st = f.st.lock();
        if st.state() == STATE_OPEN {
            return Err(EBADFD);
        }
        if frames == 0 {
            return Ok(0);
        }
        if st.reconf {
            return Err(EBUSY);
        }
        accessible(&st)?;
        // Automatic start (pcm_lib.c:2261): a read of at least start_threshold frames starts a prepared stream.
        let due = st.state() == STATE_PREPARED && frames >= st.sw.start_threshold;
        if due && trans.is_none() {
            drop(st);
            trans = Some(f.trans.lock());
            continue;
        }
        let start = if due { Some(CapFile::start_locked(&mut st)?) } else { None };
        let Some(r) = &st.run else { return Err(EBADFD) };
        let v = (r.cap.clone(), r.format, r.channels as usize, r.frame_bytes, r.buffer, r.rate, start);
        st.busy += 1;
        break v;
    };
    let done_busy = |e: Errno| {
        f.st.lock().busy -= 1;
        e
    };
    if let Some(l) = start {
        f.poll.wake_all();
        if l.run().is_err() {
            let mut st = f.st.lock();
            st.phase = Phase::Xrun;
            st.busy -= 1;
            return Err(EPIPE);
        }
    }
    drop(trans);
    let step = STEP.min(frames as usize);
    let mut sbuf = alloc::vec::Vec::new();
    if sbuf.try_reserve_exact(step).is_err() {
        return Err(done_busy(ENOMEM));
    }
    sbuf.resize(step, [0i16; 2]);
    let mut kbuf = try_buf(step * fb).map_err(done_busy)?;
    let mut nb = nonblock;
    let (mut done, mut err) = (0u64, 0i64);
    'outer: while done < frames {
        let (avail, epoch) = {
            let mut st = f.st.lock();
            if let Err(e) = accessible(&st).and_then(|_| st.update()) {
                err = e;
                break;
            }
            (st.avail(), st.epoch)
        };
        if avail == 0 {
            if *nb.get_or_insert_with(|| mem.map_or_else(|| f.nonblock(), |m| f.nonblock_in(m))) {
                err = EAGAIN;
                break;
            }
            let twake = (frames - done).min(f.st.lock().sw.avail_min.max(1)).min(buffer) as usize;
            let (limit, mut waited, mut kicks) = (wait_ns(buffer, rate), 0u64, cap.kicks());
            loop {
                if interrupted(mem) {
                    err = ERESTARTSYS;
                    break 'outer;
                }
                if cap.wait(twake, SLICE_NS) {
                    break;
                }
                let mut st = f.st.lock();
                match st.state() {
                    STATE_XRUN => err = EPIPE,
                    STATE_DISCONNECTED => err = ENODEV,
                    STATE_OPEN | STATE_SETUP | STATE_DRAINING => err = EBADFD,
                    _ => {}
                }
                if err == 0
                    && let Err(e) = st.update()
                {
                    err = e;
                }
                if err != 0 {
                    break 'outer;
                }
                let k = cap.kicks();
                if st.state() == STATE_PAUSED || k != kicks || st.avail() > 0 {
                    (waited, kicks) = (0, k);
                    if st.avail() >= twake as u64 {
                        break;
                    }
                    continue;
                }
                waited += SLICE_NS;
                if waited >= limit {
                    klog::event!(Warn, "snd", "pcm.read_timeout", waited_ms = waited / 1_000_000, source = cap.source_name().as_str();
                        "capture read error (no frames from the device)");
                    err = EIO;
                    break 'outer;
                }
            }
            continue;
        }
        let n = ((frames - done).min(avail) as usize).min(step);
        // Linux leaves the frames in the ring when the copy faults (pcm_lib.c:2316): the destination is
        // probed before the source gives them up.
        if let Dst::User(addr, m) = &dst
            && !probe(*m, addr.wrapping_add(done.wrapping_mul(fb as u64)), n * fb)
        {
            err = EFAULT;
            break;
        }
        let got = match cap.read(&mut sbuf[..n]) {
            Ok(g) => g,
            Err(SndError::Closed) => {
                err = ENODEV;
                break;
            }
            Err(_) => {
                err = EIO;
                break;
            }
        };
        if got == 0 {
            continue;
        }
        let bytes = got * fb;
        convert(format, channels, &sbuf[..got], &mut kbuf[..bytes]);
        let at = done as usize * fb;
        match &mut dst {
            Dst::User(addr, m) => {
                if !m.write(addr.wrapping_add(at as u64), &kbuf[..bytes]) {
                    // The frames left the source: counted as read (dropped), so hw_ptr never goes back.
                    let mut st = f.st.lock();
                    if st.epoch == epoch {
                        st.appl += got as u64;
                    }
                    err = EFAULT;
                    break;
                }
            }
            Dst::Kernel(b) => b[at..at + bytes].copy_from_slice(&kbuf[..bytes]),
        }
        let mut st = f.st.lock();
        if st.epoch == epoch {
            st.appl += got as u64;
        }
        done += got as u64;
    }
    {
        let mut st = f.st.lock();
        st.busy -= 1;
        if done > 0 {
            let _ = st.update();
        }
    }
    if done > 0 { Ok(done) } else if err != 0 { Err(err) } else { Ok(0) }
}

/// `pcm_lib_apply_appl_ptr` with INFO_NO_REWINDS: forward within what waits (dropped unread); back is EINVAL.
pub(super) fn apply_appl(f: &CapFile, appl: u64) -> Result<(), Errno> {
    let diff = {
        let st = f.st.lock();
        let Some(r) = &st.run else { return Ok(()) };
        let old = st.appl % r.boundary;
        if appl == old {
            return Ok(());
        }
        if appl >= r.boundary {
            return Err(EINVAL);
        }
        let diff = if appl > old { appl - old } else { r.boundary - old + appl };
        if diff > r.buffer {
            return Err(EINVAL);
        }
        if matches!(st.phase, Phase::Open | Phase::Setup) {
            return Ok(());
        }
        diff
    };
    forward(f, diff).and_then(|n| if n == diff { Ok(()) } else { Err(EINVAL) })
}

/// `forward_appl_ptr`: at most what waits, dropped unread (the source read happens outside `snd.pcm.c`).
pub(super) fn forward(f: &CapFile, frames: u64) -> Result<u64, Errno> {
    let (cap, n, epoch) = {
        let mut st = f.st.lock();
        let n = frames.min(st.avail());
        let Some(r) = &st.run else { return Ok(0) };
        if n == 0 || matches!(st.phase, Phase::Open | Phase::Setup) {
            return Ok(0);
        }
        (r.cap.clone(), n, st.epoch)
    };
    let took = cap.discard(n as usize) as u64;
    let mut st = f.st.lock();
    if st.epoch == epoch {
        st.appl += took;
    }
    Ok(took)
}
