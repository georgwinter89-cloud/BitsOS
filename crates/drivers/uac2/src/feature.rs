//! Mute and volume of a UAC2 feature unit, as values; nothing here sends. Linux 7.3.1 `sound/usb/mixer.c`: a write is
//! CUR to the interface with the control in the high byte of wValue, the channel in the low one and the unit above
//! the interface in wIndex (mixer.c:466-498); the limits come from one RANGE request of a 2-byte count and one triplet
//! of 2-byte values, which is all Linux reads (mixer.c:350-391). Two bits per control, bit 1 writable (V:39).

use crate::clock::{CS_CUR, CS_RANGE, Setup};
use crate::desc::FeatureUnit;

pub const MUTE: u8 = 0x01;
pub const VOLUME: u8 = 0x02;

/// 0x8000 in a CUR is silence, not the lowest step (UAC2 5.2.5.7.2).
pub const VOL_SILENCE: i16 = i16::MIN;

const IF_OUT: u8 = 0x21;
const IF_IN: u8 = 0xA1;
/// Count and one triplet of min, max and resolution.
pub const VOLUME_RANGE_BYTES: u16 = 2 + 3 * 2;

const fn request(request_type: u8, request: u8, control: u8, channel: u8, unit: u8, iface: u8, length: u16) -> Setup {
    Setup { request_type, request, value: (control as u16) << 8 | channel as u16, index: (unit as u16) << 8 | iface as u16, length }
}

pub const fn set_mute(iface: u8, unit: u8, channel: u8, on: bool) -> (Setup, [u8; 1]) {
    (request(IF_OUT, CS_CUR, MUTE, channel, unit, iface, 1), [on as u8])
}

pub const fn get_mute(iface: u8, unit: u8, channel: u8) -> Setup {
    request(IF_IN, CS_CUR, MUTE, channel, unit, iface, 1)
}

pub const fn volume_range(iface: u8, unit: u8, channel: u8) -> Setup {
    request(IF_IN, CS_RANGE, VOLUME, channel, unit, iface, VOLUME_RANGE_BYTES)
}

pub const fn set_volume(iface: u8, unit: u8, channel: u8, db256: i16) -> (Setup, [u8; 2]) {
    (request(IF_OUT, CS_CUR, VOLUME, channel, unit, iface, 2), db256.to_le_bytes())
}

/// The first triplet (min, max, res in 1/256 dB) of a RANGE reply; `None` if it is short, empty or min above max.
pub fn decode_volume_range(reply: &[u8]) -> Option<(i16, i16, i16)> {
    let r = reply.get(..VOLUME_RANGE_BYTES as usize)?;
    if u16::from_le_bytes([r[0], r[1]]) == 0 {
        return None;
    }
    let v = |i: usize| i16::from_le_bytes([r[i], r[i + 1]]);
    let (min, max, res) = (v(2), v(4), v(6));
    (min <= max).then_some((min, max, res))
}

/// 0 dB inside the device's range, the level `usb-audio` gives UAC1 units too (the mixer's volumes do the rest).
/// Without a range: 0 dB, which a device clamps or refuses.
pub fn unity(range: Option<(i16, i16, i16)>) -> i16 {
    let Some((min, max, _)) = range else { return 0 };
    let min = if min == VOL_SILENCE { min + 1 } else { min };
    if min > max {
        return 0;
    }
    0i16.clamp(min, max)
}

/// Channels (0 = master) on which `control` can be written.
pub fn writable(f: &FeatureUnit, control: u8) -> impl Iterator<Item = u8> + '_ {
    let bit = 2u32 << ((control as u32 - 1) * 2);
    f.controls.iter().enumerate().filter(move |(_, c)| *c & bit != 0).map(|(i, _)| i as u8)
}
