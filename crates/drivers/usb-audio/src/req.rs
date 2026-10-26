//! UAC1 class requests (UAC1 5.2): SET_CUR/GET_CUR of the endpoint's sampling frequency (Linux `clock.c:400`
//! `set_sample_rate_v1`), feature unit mute and volume (`mixer.c` `snd_usb_mixer_set_ctl_value`,
//! `get_min_max_with_quirks`). Over the `Ctl` trait so host tests answer them with a fake device.

use crate::plan::{RATE, StreamPlan};
use crate::uac::{FU_MUTE, FU_VOLUME, FeatureUnit};
use alloc::sync::Arc;
use usb_core::{Device, Setup, UsbError};

pub const SET_CUR: u8 = 0x01;
pub const GET_CUR: u8 = 0x81;
pub const GET_MIN: u8 = 0x82;
pub const GET_MAX: u8 = 0x83;

/// bmRequestType: class, interface / endpoint recipient, OUT or IN.
const IF_OUT: u8 = 0x21;
const IF_IN: u8 = 0xA1;
const EP_OUT: u8 = 0x22;
const EP_IN: u8 = 0xA2;

const MUTE_CONTROL: u16 = 1;
const VOLUME_CONTROL: u16 = 2;
const SAMPLING_FREQ_CONTROL: u16 = 1;

/// Volume in 1/256 dB; 0x8000 is "silence" (UAC1 5.2.2.4.3.2).
pub const VOL_SILENCE: i16 = i16::MIN;

/// What the driver needs of a device for control: the default pipe and SET_INTERFACE. `Device` is one; host
/// tests implement it.
pub trait Ctl: Send + Sync {
    fn out(&self, setup: Setup, data: &[u8]) -> Result<usize, UsbError>;
    fn input(&self, setup: Setup, buf: &mut [u8]) -> Result<usize, UsbError>;
    fn set_interface(&self, iface: u8, alt: u8) -> Result<(), UsbError>;
}

impl Ctl for Arc<Device> {
    fn out(&self, setup: Setup, data: &[u8]) -> Result<usize, UsbError> {
        self.control_out(setup, data)
    }
    fn input(&self, setup: Setup, buf: &mut [u8]) -> Result<usize, UsbError> {
        self.control_in(setup, buf)
    }
    fn set_interface(&self, iface: u8, alt: u8) -> Result<(), UsbError> {
        Device::set_interface(self, iface, alt)
    }
}

pub fn ep_rate(request: u8, ep: u8) -> Setup {
    let rt = if request & 0x80 != 0 { EP_IN } else { EP_OUT };
    Setup { request_type: rt, request, value: SAMPLING_FREQ_CONTROL << 8, index: ep as u16, length: 3 }
}

pub fn fu(request: u8, control: u16, channel: u8, unit: u8, iface: u8, length: u16) -> Setup {
    let rt = if request & 0x80 != 0 { IF_IN } else { IF_OUT };
    Setup { request_type: rt, request, value: control << 8 | channel as u16, index: (unit as u16) << 8 | iface as u16, length }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RateResult {
    /// SET_CUR taken and GET_CUR read this back.
    Set(u32),
    /// SET_CUR taken, the read-back failed (many devices cannot answer it; Linux ignores it too).
    Unverified,
    /// No sampling-frequency control on the endpoint: the device runs at its one rate.
    NoControl,
}

/// SET_CUR 48 kHz on the stream's endpoint, verified with GET_CUR as Linux does.
pub fn set_rate(c: &dyn Ctl, s: &StreamPlan) -> Result<RateResult, UsbError> {
    if !s.rate_control {
        return Ok(RateResult::NoControl);
    }
    let b = RATE.to_le_bytes();
    c.out(ep_rate(SET_CUR, s.ep.address), &b[..3])?;
    let mut r = [0u8; 3];
    match c.input(ep_rate(GET_CUR, s.ep.address), &mut r) {
        Ok(3) => Ok(RateResult::Set(r[0] as u32 | (r[1] as u32) << 8 | (r[2] as u32) << 16)),
        _ => Ok(RateResult::Unverified),
    }
}

fn get_i16(c: &dyn Ctl, request: u8, f: &FeatureUnit, ch: u8, iface: u8) -> Option<i16> {
    let mut b = [0u8; 2];
    match c.input(fu(request, VOLUME_CONTROL, ch, f.id, iface, 2), &mut b) {
        Ok(2) => Some(i16::from_le_bytes(b)),
        _ => None,
    }
}

/// The level the driver sets: 0 dB inside [min, max] (the mixer's master and client volumes do the rest, so the
/// headset plays at the level the jack does). Min/max unreadable: 0 dB.
pub fn unity(min: Option<i16>, max: Option<i16>) -> i16 {
    let (lo, hi) = match (min, max) {
        (Some(a), Some(b)) if a <= b => (a, b),
        _ => return 0,
    };
    // A minimum of 0x8000 means "silence" there, not the lowest step.
    let lo = if lo == VOL_SILENCE { lo + 1 } else { lo };
    if lo > hi {
        return 0; // a range of silence only: ignored like min above max
    }
    0i16.clamp(lo, hi)
}

/// What `apply_features` did, for the probe's log line.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FeatureResult {
    pub mutes: u32,
    pub volumes: u32,
    pub failed: u32,
    /// The volume set (1/256 dB) on the last volume control.
    pub level: i16,
}

/// Every mute control of the path to `mute`, every volume control to 0 dB (clamped).
pub fn apply_features(c: &dyn Ctl, s: &StreamPlan, mute: bool) -> FeatureResult {
    let mut r = FeatureResult::default();
    for f in &s.features {
        for ch in f.channels_with(FU_MUTE) {
            match c.out(fu(SET_CUR, MUTE_CONTROL, ch, f.id, s.control, 1), &[mute as u8]) {
                Ok(_) => r.mutes += 1,
                Err(_) => r.failed += 1,
            }
        }
        for ch in f.channels_with(FU_VOLUME) {
            let min = get_i16(c, GET_MIN, f, ch, s.control);
            let max = get_i16(c, GET_MAX, f, ch, s.control);
            let v = unity(min, max);
            match c.out(fu(SET_CUR, VOLUME_CONTROL, ch, f.id, s.control, 2), &v.to_le_bytes()) {
                Ok(_) => {
                    r.volumes += 1;
                    r.level = v;
                }
                Err(_) => r.failed += 1,
            }
        }
    }
    r
}

/// Only the mute controls (the master switch changed).
pub fn set_mute(c: &dyn Ctl, s: &StreamPlan, mute: bool) -> Result<u32, UsbError> {
    let mut n = 0;
    let mut last = Ok(());
    for f in s.features.iter().filter(|f| f.has(FU_MUTE)) {
        for ch in f.channels_with(FU_MUTE) {
            match c.out(fu(SET_CUR, MUTE_CONTROL, ch, f.id, s.control, 1), &[mute as u8]) {
                Ok(_) => n += 1,
                Err(e) => last = Err(e),
            }
        }
    }
    last.map(|_| n)
}

/// Mute state read back (GET_CUR, the first mute control), for tests and the status line.
pub fn get_mute(c: &dyn Ctl, s: &StreamPlan) -> Option<bool> {
    let f = s.features.iter().find(|f| f.has(FU_MUTE))?;
    let ch = f.channels_with(FU_MUTE).next()?;
    let mut b = [0u8; 1];
    match c.input(fu(GET_CUR, MUTE_CONTROL, ch, f.id, s.control, 1), &mut b) {
        Ok(1) => Some(b[0] != 0),
        _ => None,
    }
}
