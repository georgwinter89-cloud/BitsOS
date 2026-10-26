//! What a test needs to talk to a PCM or control file as libasound does: parameter structs, a transfer, the
//! status fields (host tests and ktests).

use super::abi::*;
use super::params::{self, HwParams, Interval, Mask};
use super::umem::{Arena, BASE};
use super::{CapFile, PcmFile};
use vfs::sys::UserMem;
use alloc::vec::Vec;
use vfs::Errno;

/// Scratch addresses in the arena: the struct goes at `ARG`, sample data at `DATA`.
pub const ARG: u64 = BASE;
pub const DATA: u64 = BASE + 0x1000;

/// hw_params with access RW_INTERLEAVED, `format`, `rate`, `channels` fixed; `period`/`buffer` frames fixed
/// when non-zero (`snd_pcm_hw_params_set_*` leave the parameter single).
pub fn hw(format: u32, rate: u32, channels: u32, period: u32, buffer: u32) -> [u8; HW_PARAMS_SIZE] {
    let mut p = HwParams::any();
    p.masks[params::ACCESS] = Mask::of(&[ACCESS_RW_INTERLEAVED]);
    p.masks[params::FORMAT] = Mask::of(&[format]);
    for (k, v) in [(params::RATE, rate), (params::CHANNELS, channels), (params::PERIOD_SIZE, period), (params::BUFFER_SIZE, buffer)] {
        if v != 0 {
            *p.iv_mut(k) = Interval { integer: true, ..Interval::range(v, v) };
        }
    }
    p.encode()
}

pub fn sw(start: u64, stop: u64, avail_min: u64) -> [u8; SW_PARAMS_SIZE] {
    let mut b = [0u8; SW_PARAMS_SIZE];
    put_u64(&mut b, sw::AVAIL_MIN, avail_min);
    put_u64(&mut b, sw::START_THRESHOLD, start);
    put_u64(&mut b, sw::STOP_THRESHOLD, stop);
    put_u32(&mut b, sw::PROTO, SNDRV_PCM_VERSION);
    b
}

/// The fields of `struct snd_pcm_status` a test looks at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Status {
    pub state: u32,
    pub appl: u64,
    pub hw: u64,
    pub avail: u64,
    pub delay: i64,
}

/// A PCM file's ioctl: the playback or (SN-9) the capture substream.
pub trait Ioctl {
    fn pcm_ioctl(&self, cmd: u64, arg: u64, mem: &dyn UserMem) -> Result<i64, Errno>;
}

impl Ioctl for PcmFile {
    fn pcm_ioctl(&self, cmd: u64, arg: u64, mem: &dyn UserMem) -> Result<i64, Errno> {
        self.ioctl(cmd, arg, mem)
    }
}

impl Ioctl for CapFile {
    fn pcm_ioctl(&self, cmd: u64, arg: u64, mem: &dyn UserMem) -> Result<i64, Errno> {
        self.ioctl(cmd, arg, mem)
    }
}

impl<X: Ioctl + ?Sized> Ioctl for alloc::sync::Arc<X> {
    fn pcm_ioctl(&self, cmd: u64, arg: u64, mem: &dyn UserMem) -> Result<i64, Errno> {
        (**self).pcm_ioctl(cmd, arg, mem)
    }
}

pub struct T<'a> {
    pub f: &'a dyn Ioctl,
    pub m: &'a Arena,
}

impl T<'_> {
    pub fn call(&self, cmd: u64, arg: u64) -> Result<i64, Errno> {
        self.f.pcm_ioctl(cmd, arg, self.m)
    }
    /// An ioctl on `data` at ARG; the struct as it came back.
    pub fn with(&self, cmd: u64, data: &[u8]) -> (Result<i64, Errno>, Vec<u8>) {
        self.m.set(ARG, data);
        let r = self.call(cmd, ARG);
        (r, self.m.get(ARG, data.len()))
    }
    pub fn hw_params(&self, b: &[u8; HW_PARAMS_SIZE]) -> Result<HwParams, Errno> {
        let (r, out) = self.with(PCM_HW_PARAMS, b);
        r?;
        let mut raw = [0u8; HW_PARAMS_SIZE];
        raw.copy_from_slice(&out);
        Ok(HwParams::decode(&raw))
    }
    /// WRITEI_FRAMES of `frames` frames already at DATA: the ioctl's result and xferi.result.
    pub fn writei(&self, frames: u64) -> (Result<i64, Errno>, i64) {
        let mut x = [0u8; XFERI_SIZE];
        put_u64(&mut x, 8, DATA);
        put_u64(&mut x, 16, frames);
        let (r, out) = self.with(PCM_WRITEI_FRAMES, &x);
        (r, get_u64(&out, 0) as i64)
    }
    /// READI_FRAMES of `frames` frames into DATA: the ioctl's result and xferi.result.
    pub fn readi(&self, frames: u64) -> (Result<i64, Errno>, i64) {
        let mut x = [0u8; XFERI_SIZE];
        put_u64(&mut x, 8, DATA);
        put_u64(&mut x, 16, frames);
        let (r, out) = self.with(PCM_READI_FRAMES, &x);
        (r, get_u64(&out, 0) as i64)
    }
    pub fn status(&self) -> Status {
        let (_, b) = self.with(PCM_STATUS, &[0u8; STATUS_SIZE]);
        Status { state: get_u32(&b, status::STATE), appl: get_u64(&b, status::APPL_PTR), hw: get_u64(&b, status::HW_PTR), avail: get_u64(&b, status::AVAIL), delay: get_u64(&b, status::DELAY) as i64 }
    }
    /// SYNC_PTR with `flags` (APPL|AVAIL_MIN = only read): (result, state, hw_ptr, appl_ptr).
    pub fn sync_ptr(&self, flags: u32) -> (Result<i64, Errno>, u32, u64, u64) {
        let mut b = [0u8; SYNC_PTR_SIZE];
        put_u32(&mut b, sync::FLAGS, flags);
        let (r, o) = self.with(PCM_SYNC_PTR, &b);
        (r, get_u32(&o, sync::STATE), get_u64(&o, sync::HW_PTR), get_u64(&o, sync::APPL_PTR))
    }
}
