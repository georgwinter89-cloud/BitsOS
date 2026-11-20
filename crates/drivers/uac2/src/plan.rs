//! From a jack and a format to an alternate setting and a clock (`stream.c` `__snd_usb_parse_audio_interface`, narrowed
//! to what this board has): the path from the jack through feature units to the USB side, the first alternate setting
//! that carries the format, and the rate checked against the clock's own answer.

use crate::desc::{AltSetting, Endpoint, FORMAT_PCM, FORMAT_TYPE_I, Function, Sync, terminal};
use crate::range::Ranges;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Want {
    /// wTerminalType of the jack.
    pub jack: u16,
    pub rate: u32,
    pub channels: u8,
    /// S16 is 16 bits in 2 bytes, S24_3 is 24 in 3, S32 is 32 in 4.
    pub bits: u8,
    /// The device runs at full speed: its interval counts 1 ms frames, not 125 us microframes.
    pub full_speed: bool,
}

/// What the V2 mixer would first ask of the board's speaker jack.
pub const SPEAKER_S16_48K: Want = Want { jack: terminal::SPEAKER, rate: 48_000, channels: 2, bits: 16, full_speed: false };

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlanError {
    /// The device has no terminal of this type.
    NoSuchJack(u16),
    /// The path from the jack leads into something other than a feature unit or terminal: a selector or mixer, whose
    /// routing this crate does not understand, or a loop.
    PathUnsupported {
        unit: u8,
    },
    /// No terminal on the USB side connects to this jack.
    NoUsbSide(u16),
    NoFormat {
        channels: u8,
        bits: u8,
    },
    /// The linked terminal's clock is not a clock source. A selector or multiplier could be walked; nothing here has
    /// one.
    ClockNotASource(u8),
    /// A clock whose rate cannot be set: there is no rate to choose.
    ClockFixed(u8),
    RateNotOffered(u32),
    /// The endpoint is not isochronous, or its interval is outside what the USB spec allows.
    BadEndpoint(u8),
    /// Not enough room per interval for this rate and format.
    PacketTooSmall {
        need: u32,
        have: u32,
    },
    /// An asynchronous OUT endpoint wants a feedback endpoint read, which is not built. This board's are adaptive.
    NeedsFeedback(u8),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Plan {
    pub control_iface: u8,
    pub iface: u8,
    pub alt: u8,
    pub endpoint: Endpoint,
    pub clock: u8,
    pub playback: bool,
    pub channels: u8,
    pub subslot: u8,
    pub bits: u8,
    pub rate: u32,
    /// The feature unit on the path, for mute and volume.
    pub feature: Option<u8>,
    /// Bytes in the largest packet this rate needs.
    pub max_packet_bytes: u32,
}

fn is_input_jack(jack: u16) -> bool {
    jack >> 8 == 0x02
}

/// Walks from a terminal's source back to an input terminal, through feature units only. Returns that terminal's id and
/// the last feature unit passed. Bounded by the units in the tree, so a loop ends.
fn trace(f: &Function, mut unit: u8) -> Result<(u8, Option<u8>), PlanError> {
    let mut feature = None;
    for _ in 0..=f.features.len() {
        if f.input(unit).is_some() {
            return Ok((unit, feature));
        }
        match f.feature(unit) {
            Some(fu) => {
                feature = Some(fu.id);
                unit = fu.source;
            }
            None => return Err(PlanError::PathUnsupported { unit }),
        }
    }
    Err(PlanError::PathUnsupported { unit })
}

/// The terminal the stream links to, and the feature unit on the way.
fn route(f: &Function, jack: u16) -> Result<(u8, Option<u8>), PlanError> {
    if is_input_jack(jack) {
        let input = f.inputs.iter().find(|t| t.ty == jack).ok_or(PlanError::NoSuchJack(jack))?;
        let mut usb_side = None;
        for out in f.outputs.iter().filter(|o| o.ty == terminal::USB_STREAMING) {
            if let Ok((from, feature)) = trace(f, out.source)
                && from == input.id
            {
                usb_side = Some((out.id, feature));
            }
        }
        usb_side.ok_or(PlanError::NoUsbSide(jack))
    } else {
        let out = f.outputs.iter().find(|t| t.ty == jack).ok_or(PlanError::NoSuchJack(jack))?;
        let (input, feature) = trace(f, out.source)?;
        if f.input(input).is_some_and(|t| t.ty == terminal::USB_STREAMING) {
            Ok((input, feature))
        } else {
            Err(PlanError::NoUsbSide(jack))
        }
    }
}

fn subslot_of(bits: u8) -> Option<u8> {
    match bits {
        16 => Some(2),
        24 => Some(3),
        32 => Some(4),
        _ => None,
    }
}

/// Interface 5's alternate setting 11 here is Type III with formats 0x1381, for something more than plain PCM. Linux
/// gives it a stream of its own; a plain stream never needs it.
fn carries(alt: &AltSetting, link: u8, want: &Want, subslot: u8) -> bool {
    alt.link == link
        && alt.format_type == FORMAT_TYPE_I
        && alt.formats == FORMAT_PCM
        && alt.channels == want.channels
        && alt.subslot == subslot
        && alt.bits == want.bits
}

/// The most a packet can hold of this stream: frames per interval rounded up, plus one for a clock running a little
/// fast. That margin is this crate's choice and not Linux's formula.
fn packet_bytes(ep: &Endpoint, want: &Want, subslot: u8) -> Result<u32, PlanError> {
    let interval = ep.interval_us_at(want.full_speed).ok_or(PlanError::BadEndpoint(ep.address))?;
    let frames = (want.rate as u64 * interval as u64).div_ceil(1_000_000) + 1;
    let bytes = frames * want.channels as u64 * subslot as u64;
    Ok(bytes.min(u32::MAX as u64) as u32)
}

/// The clock source a jack's stream runs on: the one whose RANGE `plan` needs. Refused as `plan` refuses it.
pub fn clock_for(f: &Function, jack: u16) -> Result<u8, PlanError> {
    let (link, _) = route(f, jack)?;
    let id = link_clock(f, link, jack)?;
    f.clock(id).map(|_| id).ok_or(PlanError::ClockNotASource(id))
}

fn link_clock(f: &Function, link: u8, jack: u16) -> Result<u8, PlanError> {
    match f.input(link) {
        Some(t) => Ok(t.clock),
        None => f.output(link).map(|t| t.clock).ok_or(PlanError::NoUsbSide(jack)),
    }
}

pub fn plan(f: &Function, want: &Want, rates: &Ranges<'_>) -> Result<Plan, PlanError> {
    let (link, feature) = route(f, want.jack)?;
    let no_format = PlanError::NoFormat { channels: want.channels, bits: want.bits };
    let subslot = subslot_of(want.bits).ok_or(no_format)?;
    let playback = !is_input_jack(want.jack);
    // The first one: on this board later ones repeat the format and Linux lists them all alike; what tells them apart
    // is unverified.
    let alt = f
        .alts
        .iter()
        .filter(|a| a.endpoint.is_in() != playback)
        .find(|a| carries(a, link, want, subslot))
        .ok_or(no_format)?;
    let clock_id = link_clock(f, link, want.jack)?;
    let clock = f.clock(clock_id).ok_or(PlanError::ClockNotASource(clock_id))?;
    if !clock.frequency_writable() {
        return Err(PlanError::ClockFixed(clock_id));
    }
    if !rates.contains(want.rate) {
        return Err(PlanError::RateNotOffered(want.rate));
    }
    let ep = alt.endpoint;
    if !ep.is_isochronous() {
        return Err(PlanError::BadEndpoint(ep.address));
    }
    if playback && ep.sync() == Sync::Async {
        return Err(PlanError::NeedsFeedback(ep.address));
    }
    let need = packet_bytes(&ep, want, subslot)?;
    let have = if want.full_speed { (ep.max_packet & 0x7FF) as u32 } else { ep.bytes_per_interval() };
    if need > have {
        return Err(PlanError::PacketTooSmall { need, have });
    }
    Ok(Plan {
        control_iface: f.control_iface,
        iface: alt.iface,
        alt: alt.alt,
        endpoint: ep,
        clock: clock_id,
        playback,
        channels: want.channels,
        subslot,
        bits: want.bits,
        rate: want.rate,
        feature,
        max_packet_bytes: need,
    })
}
