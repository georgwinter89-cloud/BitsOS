//! Moving frames: WRITEI_FRAMES and write(2) (`__snd_pcm_lib_xfer`, `linux-6.6/sound/core/pcm_lib.c:2207`,
//! its wait `wait_for_avail` :1885), DRAIN (pcm_native.c:2090), appl_ptr moves (SYNC_PTR, FORWARD;
//! `pcm_lib_apply_appl_ptr` pcm_lib.c:2163), and the conversion of S32_LE/FLOAT_LE to the mixer's 16 bit.

use super::abi::*;
use super::pcm::{try_buf, PcmFile, Phase, St};
use crate::client::SndError;
use uapi::errno::*;
use vfs::sys::UserMem;
use vfs::Errno;

/// Frames moved per step (one copy from the program, one conversion, one ring write).
const STEP: u64 = 4096;
/// Waits look at signals and kills this often (B4: never one unbounded wait).
const SLICE_NS: u64 = 50_000_000;
const ERESTARTSYS: i64 = uapi::ab::siginfo::ERESTARTSYS;

pub(super) enum Src<'a> {
    /// A program's address (WRITEI_FRAMES).
    User(u64, &'a dyn UserMem),
    /// Bytes already in the kernel (write(2)).
    Kernel(&'a [u8]),
}

/// One 16-bit sample from the program's sample: S32 keeps its upper half rounded, FLOAT_LE is decoded from its
/// bits (no floating point in the kernel, R10), clipped to [-1, 1) and rounded.
pub(crate) fn to_s16(format: u32, b: &[u8]) -> i16 {
    match format {
        FORMAT_S32_LE => {
            let v = i32::from_le_bytes([b[0], b[1], b[2], b[3]]) as i64;
            ((v + 0x8000) >> 16).clamp(-32768, 32767) as i16
        }
        FORMAT_FLOAT_LE => f32_to_s16(u32::from_le_bytes([b[0], b[1], b[2], b[3]])),
        _ => i16::from_le_bytes([b[0], b[1]]),
    }
}

/// IEEE 754 single (as bits) * 32768, rounded to nearest (ties away from zero), clamped; NaN = 0.
pub fn f32_to_s16(bits: u32) -> i16 {
    let neg = bits >> 31 != 0;
    let exp = ((bits >> 23) & 0xff) as i32;
    let man = (bits & 0x7f_ffff) as u64;
    if exp == 0xff {
        return if man != 0 { 0 } else if neg { -32768 } else { 32767 };
    }
    if exp == 0 {
        return 0; // subnormals: far below one 16-bit step
    }
    // value * 32768 = (1.man) * 2^(exp - 127 + 15); keep 24 fractional bits of the mantissa.
    let m = man | 0x80_0000; // 1.23 fixed point
    let shift = exp - 127 + 15 - 23; // value*32768 = m * 2^shift
    let mag: u64 = if shift >= 0 {
        if shift > 16 { 1 << 20 } else { m << shift }
    } else if -shift > 40 {
        0
    } else {
        let s = (-shift) as u32;
        (m + (1 << (s - 1))) >> s
    };
    let v = if neg { -(mag.min(1 << 20) as i64) } else { mag.min(1 << 20) as i64 };
    v.clamp(-32768, 32767) as i16
}

/// The program's frames as the client takes them (16-bit LE, its channels).
fn convert(format: u32, src: &[u8], out: &mut [u8]) {
    let w = if format == FORMAT_S16_LE { 2 } else { 4 };
    for (i, s) in src.chunks_exact(w).enumerate() {
        out[i * 2..i * 2 + 2].copy_from_slice(&to_s16(format, s).to_le_bytes());
    }
}

fn interrupted(mem: Option<&dyn UserMem>) -> bool {
    match mem {
        Some(m) => m.interrupted() || m.signal_pending(),
        None => vfs::dentry::interrupted(),
    }
}

/// `wait_time` of `wait_for_avail`/drain: max(100 ms, buffer * 1.1 s / rate).
fn wait_ns(buffer: u64, rate: u32) -> u64 {
    (buffer * 1100 / rate.max(1) as u64).max(100) * 1_000_000
}

/// `pcm_accessible_state` (pcm_lib.c:2144).
fn accessible(st: &St) -> Result<(), Errno> {
    match st.state() {
        STATE_PREPARED | STATE_RUNNING | STATE_PAUSED => Ok(()),
        STATE_XRUN => Err(EPIPE),
        _ => Err(EBADFD),
    }
}

/// `snd_pcm_xferi_frames_ioctl` (pcm_native.c:3204).
pub(super) fn xferi(f: &PcmFile, arg: u64, mem: &dyn UserMem) -> Result<i64, Errno> {
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
    let r = write(f, Src::User(buf, mem), frames, None, Some(mem));
    let v = match r {
        Ok(n) => n as i64,
        Err(e) => -e,
    };
    if !mem.write(arg, &v.to_le_bytes()) {
        return Err(EFAULT);
    }
    r.map(|_| 0)
}

/// The transfer loop. `nonblock`: write(2) knows it; None = look it up (ioctl).
pub(super) fn write(f: &PcmFile, src: Src<'_>, frames: u64, nonblock: Option<bool>, mem: Option<&dyn UserMem>) -> Result<u64, Errno> {
    let (pb, format, channels, fb, buffer, rate) = {
        let mut st = f.st.lock();
        if st.state() == STATE_OPEN {
            return Err(EBADFD);
        }
        if frames == 0 {
            return Ok(0);
        }
        // hw_params is replacing the client (Linux: buffer_accessing < 0, pcm_lib.c:2309).
        if st.reconf {
            return Err(EBUSY);
        }
        accessible(&st)?;
        let Some(r) = &st.run else { return Err(EBADFD) };
        let v = (r.pb.clone(), r.format, r.channels as usize, r.frame_bytes, r.buffer, r.rate);
        st.busy += 1;
        v
    };
    let bufs = try_buf((STEP.min(frames) as usize) * fb).and_then(|k| Ok((k, if format == FORMAT_S16_LE { alloc::vec::Vec::new() } else { try_buf(STEP.min(frames) as usize * channels * 2)? })));
    let (mut kbuf, mut sbuf) = match bufs {
        Ok(b) => b,
        Err(e) => {
            f.st.lock().busy -= 1;
            return Err(e);
        }
    };
    let mut nb = nonblock;
    let (mut done, mut err) = (0u64, 0i64);
    // The last copy took nothing: wait for ring space instead of trying again at once (B4).
    let mut stalled = false;
    'outer: while done < frames {
        // A concurrent PREPARE (or DROP + PREPARE) reset the pointers: go on from the new ones, as Linux does.
        let (avail, epoch) = {
            let mut st = f.st.lock();
            if let Err(e) = accessible(&st) {
                err = e;
                break;
            }
            // The ring is the truth for what fits; it can hold less than ALSA's avail while an older
            // epoch's copy is still in it.
            (if stalled { 0 } else { st.avail().min(pb.avail() as u64) }, st.epoch)
        };
        stalled = false;
        if avail == 0 {
            if *nb.get_or_insert_with(|| mem.map_or_else(|| f.nonblock(), |m| f.nonblock_in(m))) {
                err = EAGAIN;
                break;
            }
            let twake = (frames - done).min(f.st.lock().sw.avail_min.max(1));
            let (limit, mut waited, mut pos) = (wait_ns(buffer, rate), 0u64, pb.position());
            loop {
                if interrupted(mem) {
                    err = ERESTARTSYS;
                    break 'outer;
                }
                match pb.wait_space(twake as usize, SLICE_NS) {
                    Ok(true) => break,
                    Ok(false) => {}
                    Err(SndError::Xrun) => {
                        err = EPIPE;
                        break 'outer;
                    }
                    Err(_) => {
                        err = EBADFD;
                        break 'outer;
                    }
                }
                let st = f.st.lock();
                match st.state() {
                    STATE_XRUN => err = EPIPE,
                    STATE_DRAINING => err = EPIPE,
                    STATE_OPEN | STATE_SETUP | STATE_DISCONNECTED => err = EBADFD,
                    _ => {}
                }
                if err != 0 {
                    break 'outer;
                }
                let now = pb.position();
                if st.state() == STATE_PAUSED || now != pos {
                    (waited, pos) = (0, now);
                    continue;
                }
                waited += SLICE_NS;
                if waited >= limit {
                    klog::event!(Warn, "snd", "pcm.write_timeout", waited_ms = waited / 1_000_000; "playback write error (no progress)");
                    err = EIO;
                    break 'outer;
                }
            }
            continue;
        }
        let n = (frames - done).min(avail).min(STEP);
        let bytes = n as usize * fb;
        let data: &[u8] = match &src {
            Src::User(addr, m) => {
                if !m.read(addr.wrapping_add(done.wrapping_mul(fb as u64)), &mut kbuf[..bytes]) {
                    err = EFAULT;
                    break;
                }
                &kbuf[..bytes]
            }
            Src::Kernel(b) => &b[done as usize * fb..done as usize * fb + bytes],
        };
        let out: &[u8] = if format == FORMAT_S16_LE {
            data
        } else {
            convert(format, data, &mut sbuf[..n as usize * channels * 2]);
            &sbuf[..n as usize * channels * 2]
        };
        let took = match pb.write(out) {
            Ok(b) => (b / (channels * 2)) as u64,
            Err(SndError::Xrun) => {
                err = EPIPE;
                break;
            }
            Err(_) => {
                err = EBADFD;
                break;
            }
        };
        let mut st = f.st.lock();
        if st.epoch == epoch {
            st.appl += took;
        }
        done += took;
        stalled = took == 0;
        // Automatic start (pcm_lib.c:2337).
        if st.state() == STATE_PREPARED && st.hw_avail() >= st.sw.start_threshold {
            pb.start();
            f.trigger(&mut st);
            drop(st);
            f.poll.wake_all();
        }
    }
    f.st.lock().busy -= 1;
    if done > 0 { Ok(done) } else if err != 0 { Err(err) } else { Ok(0) }
}

/// `pcm_lib_apply_appl_ptr` with INFO_NO_REWINDS (pcm_lib.c:2163): a forward move within the free space is
/// filled with silence (the ring has no frames the program could have written there); back is EINVAL.
pub(super) fn apply_appl(st: &mut St, appl: u64) -> Result<(), Errno> {
    let Some(r) = &st.run else { return Ok(()) };
    let (old, boundary, buffer) = (st.appl % r.boundary, r.boundary, r.buffer);
    if appl == old {
        return Ok(());
    }
    if appl >= boundary {
        return Err(EINVAL);
    }
    let diff = if appl > old { appl - old } else { boundary - old + appl };
    if diff > buffer {
        return Err(EINVAL);
    }
    if st.phase != Phase::Active {
        return Ok(());
    }
    forward(st, diff).and_then(|n| if n == diff { Ok(()) } else { Err(EINVAL) })
}

/// `forward_appl_ptr`: at most the free space, as silence.
pub(super) fn forward(st: &mut St, frames: u64) -> Result<u64, Errno> {
    let avail = st.avail();
    let n = frames.min(avail);
    let Some(r) = &st.run else { return Ok(0) };
    if n == 0 || st.phase != Phase::Active {
        return Ok(0);
    }
    let zeros = try_buf(n as usize * r.channels as usize * 2)?;
    let took = r.pb.write(&zeros).map_err(|e| if e == SndError::Xrun { EPIPE } else { EBADFD })? / (r.channels as usize * 2);
    st.appl += took as u64;
    Ok(took as u64)
}

/// `snd_pcm_drain` (pcm_native.c:2090).
pub(super) fn drain(f: &PcmFile, mem: &dyn UserMem) -> Result<(), Errno> {
    let (pb, limit) = {
        let st = f.st.lock();
        if matches!(st.state(), STATE_OPEN | STATE_DISCONNECTED) {
            return Err(EBADFD);
        }
        let Some(r) = &st.run else { return Err(EBADFD) };
        (r.pb.clone(), wait_ns(r.buffer, r.rate))
    };
    // A paused stream is released first (outside `snd.pcm`: the client wakes the poll queue).
    let _ = pb.pause(false);
    let later = {
        let mut st = f.st.lock();
        match st.state() {
            STATE_PREPARED if st.hw_avail() == 0 => {
                st.phase = Phase::Setup;
                None
            }
            STATE_PREPARED | STATE_RUNNING => {
                pb.start_drain();
                f.trigger(&mut st);
                None
            }
            STATE_XRUN => {
                let l = st.reset();
                st.phase = Phase::Setup;
                Some(l)
            }
            _ => None,
        }
    };
    if let Some(l) = later {
        l.run();
    }
    f.poll.wake_all();
    if f.nonblock_in(mem) {
        return Err(EAGAIN);
    }
    let (mut waited, mut pos) = (0u64, pb.position());
    loop {
        if f.st.lock().state() != STATE_DRAINING {
            return Ok(());
        }
        if mem.interrupted() || mem.signal_pending() {
            return Err(ERESTARTSYS);
        }
        if pb.drain(SLICE_NS) {
            continue;
        }
        let now = pb.position();
        if now != pos {
            (waited, pos) = (0, now);
            continue;
        }
        waited += SLICE_NS;
        if waited >= limit {
            klog::event!(Warn, "snd", "pcm.drain_timeout", waited_ms = waited / 1_000_000; "playback drain timeout (no progress)");
            let _ = f.drop_stream();
            return Err(EIO);
        }
    }
}
