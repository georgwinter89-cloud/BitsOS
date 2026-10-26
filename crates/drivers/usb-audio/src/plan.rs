//! Which alternate settings of an audio function the driver uses (Linux `stream.c` `snd_usb_parse_audio_interface`
//! + `format.c` `parse_audio_format_i`, narrowed to what the V2 mixer plays: 48 kHz, PCM, 16 or 24 bit in 2 or 3
//! byte subframes, 1 or 2 channels). Pure: built from the parsed configuration, host-tested on Georg's dump.

use crate::uac::{self, Control, FeatureUnit, Rates, StreamFormat};
use alloc::vec::Vec;
use usb_core::{Configuration, Endpoint, EpKind};

pub const RATE: u32 = 48_000;

/// Endpoint synchronisation type (bmAttributes bits 3:2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sync {
    None,
    Async,
    Adaptive,
    Sync,
}

impl Sync {
    pub fn of(ep: &Endpoint) -> Sync {
        match (ep.attributes >> 2) & 3 {
            0 => Sync::None,
            1 => Sync::Async,
            2 => Sync::Adaptive,
            _ => Sync::Sync,
        }
    }
    /// Playback needs a feedback endpoint the driver does not read (an asynchronous sink, UAC1 3.7.2.2).
    pub fn wants_feedback(self) -> bool {
        self == Sync::Async
    }
}

/// One usable streaming alternate setting.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StreamPlan {
    /// The AudioControl interface of the function (wIndex of unit requests).
    pub control: u8,
    pub iface: u8,
    pub alt: u8,
    pub ep: Endpoint,
    pub playback: bool,
    pub channels: u8,
    /// Bytes per sample (2 or 3).
    pub subframe: u8,
    pub bits: u8,
    pub sync: Sync,
    /// The endpoint takes SET_CUR sampling frequency (CS_ENDPOINT bit 0).
    pub rate_control: bool,
    /// 48 kHz is the only rate the setting offers.
    pub rate_only: bool,
    /// The far terminal's type (0x0301 speaker, 0x0402 headset, 0x0201 microphone ...).
    pub terminal_type: u16,
    /// Feature units on the path, nearest the far terminal first.
    pub features: Vec<FeatureUnit>,
}

impl StreamPlan {
    pub fn frame_bytes(&self) -> usize {
        self.channels as usize * self.subframe as usize
    }
}

/// Why an alternate setting was not taken (logged once at probe).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Skipped {
    pub iface: u8,
    pub alt: u8,
    pub why: &'static str,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FunctionPlan {
    pub control: u8,
    pub playback: Vec<StreamPlan>,
    pub capture: Vec<StreamPlan>,
    pub skipped: Vec<Skipped>,
    pub bad_descriptors: u32,
}

fn check(f: &StreamFormat, ep: &Endpoint) -> Result<(), &'static str> {
    if f.tag != uac::FORMAT_PCM || f.format_type != uac::FORMAT_TYPE_I {
        return Err("not PCM format type I");
    }
    if !(1..=2).contains(&f.channels) {
        return Err("not 1 or 2 channels");
    }
    if !matches!((f.subframe, f.bits), (2, 16) | (3, 24) | (3, 16..=23)) {
        return Err("not 16 or 24 bit in 2 or 3 bytes");
    }
    if !f.rates.has(RATE) {
        return Err("no 48 kHz");
    }
    if ep.max_packet() == 0 {
        return Err("max packet 0");
    }
    Ok(())
}

/// The streaming settings of the function whose AudioControl interface is `control` (default setting).
pub fn plan(cfg: &Configuration, control: u8) -> FunctionPlan {
    let mut out = FunctionPlan { control, ..Default::default() };
    let Some(acf) = cfg.interface(control, 0) else { return out };
    let ac = Control::parse(&acf.extra);
    out.bad_descriptors = ac.bad;
    for &n in &ac.streams {
        for f in cfg.interfaces.iter().filter(|f| f.number == n && f.class == 1 && f.subclass == 2) {
            let skip = |why| Skipped { iface: f.number, alt: f.alt, why };
            let Some(ep) = f.endpoints.iter().find(|e| e.kind() == EpKind::Isoch && (e.attributes >> 4) & 3 != 1).copied() else {
                if f.alt != 0 {
                    out.skipped.push(skip("no isochronous data endpoint"));
                }
                continue;
            };
            let Some(fmt) = uac::stream_format(&f.extra) else {
                out.skipped.push(skip("no AS_GENERAL/FORMAT_TYPE"));
                continue;
            };
            if let Err(why) = check(&fmt, &ep) {
                out.skipped.push(skip(why));
                continue;
            }
            let playback = !ep.is_in();
            // Playback: AS terminal link = the USB streaming input terminal; capture: the USB streaming output terminal.
            let (terminal_type, path) = if playback {
                match ac.outputs_from(fmt.terminal).into_iter().next() {
                    Some((t, p)) => (t.ty, p),
                    None => {
                        out.skipped.push(skip("terminal link reaches no output terminal"));
                        continue;
                    }
                }
            } else {
                match ac.input_to(fmt.terminal) {
                    Some((t, p)) => (t.ty, p),
                    None => {
                        out.skipped.push(skip("terminal link reached from no input terminal"));
                        continue;
                    }
                }
            };
            let cs = uac::endpoint_general(f.endpoint_extra(ep.address));
            out.push(StreamPlan {
                control,
                iface: f.number,
                alt: f.alt,
                ep,
                playback,
                channels: fmt.channels,
                subframe: fmt.subframe,
                bits: fmt.bits,
                sync: Sync::of(&ep),
                rate_control: cs.is_some_and(|c| c.0 & uac::EP_SAMPLING_FREQ != 0),
                rate_only: fmt.rates.only(RATE) || matches!(fmt.rates, Rates::Continuous { min, max } if min == RATE && max == RATE),
                terminal_type,
                features: ac.features_on(&path),
            });
        }
    }
    out
}

impl FunctionPlan {
    fn push(&mut self, s: StreamPlan) {
        if s.playback { self.playback.push(s) } else { self.capture.push(s) }
    }

    /// The setting a stream of this direction uses: per interface, stereo before mono, 16 before 24 bit,
    /// then the lowest alternate setting (Linux picks by the requested format; the mixer gives 16-bit stereo).
    pub fn best(list: &[StreamPlan]) -> Vec<StreamPlan> {
        let mut ifaces: Vec<u8> = list.iter().map(|s| s.iface).collect();
        ifaces.sort_unstable();
        ifaces.dedup();
        ifaces.iter().filter_map(|&n| {
            list.iter().filter(|s| s.iface == n).min_by_key(|s| (s.channels != 2, s.subframe != 2, s.alt)).cloned()
        }).collect()
    }
}
