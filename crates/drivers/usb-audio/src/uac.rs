//! USB Audio Class 1.0 descriptors (UAC1 spec 4.3 control, 4.5/4.6 streaming; Linux `include/uapi/linux/usb/audio.h`,
//! `sound/usb/format.c`, `mixer.c`). Input is what `usb_core::Configuration::parse` kept raw: the AudioControl
//! interface's `extra` (header, terminals, units), a streaming alternate setting's `extra` (AS_GENERAL,
//! FORMAT_TYPE) and an endpoint's `ep_extra` (CS_ENDPOINT). Every descriptor is length-prefixed; a length below
//! 3, past the end or too short for its subtype is skipped and counted (`bad`), never trusted and never a panic.

use alloc::vec::Vec;

pub const CS_INTERFACE: u8 = 0x24;
pub const CS_ENDPOINT: u8 = 0x25;

pub mod ac {
    pub const HEADER: u8 = 1;
    pub const INPUT_TERMINAL: u8 = 2;
    pub const OUTPUT_TERMINAL: u8 = 3;
    pub const MIXER_UNIT: u8 = 4;
    pub const SELECTOR_UNIT: u8 = 5;
    pub const FEATURE_UNIT: u8 = 6;
    pub const PROCESSING_UNIT: u8 = 7;
    pub const EXTENSION_UNIT: u8 = 8;
}

pub mod as_ {
    pub const GENERAL: u8 = 1;
    pub const FORMAT_TYPE: u8 = 2;
}

/// wTerminalType 0x0101: USB streaming (the host side of a path).
pub const TT_USB_STREAMING: u16 = 0x0101;
/// wFormatTag PCM (Audio Data Formats 2.3).
pub const FORMAT_PCM: u16 = 0x0001;
pub const FORMAT_TYPE_I: u8 = 1;

/// Feature unit control bits (bmaControls, UAC1 4.3.2.5).
pub const FU_MUTE: u32 = 1 << 0;
pub const FU_VOLUME: u32 = 1 << 1;

/// CS_ENDPOINT bmAttributes (UAC1 4.6.1.2).
pub const EP_SAMPLING_FREQ: u8 = 1 << 0;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InputTerminal {
    pub id: u8,
    pub ty: u16,
    pub channels: u8,
    pub channel_config: u16,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutputTerminal {
    pub id: u8,
    pub ty: u16,
    pub source: u8,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FeatureUnit {
    pub id: u8,
    pub source: u8,
    /// bmaControls: [0] the master channel, [1..] the logical channels.
    pub controls: Vec<u32>,
}

impl FeatureUnit {
    /// Channels (0 = master) whose controls have `bit`.
    pub fn channels_with(&self, bit: u32) -> impl Iterator<Item = u8> + '_ {
        self.controls.iter().enumerate().filter(move |(_, c)| *c & bit != 0).map(|(i, _)| i as u8)
    }
    pub fn has(&self, bit: u32) -> bool {
        self.controls.iter().any(|c| c & bit != 0)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Unit {
    It(InputTerminal),
    Ot(OutputTerminal),
    Fu(FeatureUnit),
    /// Mixer, selector, processing, extension: only their sources matter for the path search.
    Other { id: u8, subtype: u8, sources: Vec<u8> },
}

impl Unit {
    pub fn id(&self) -> u8 {
        match self {
            Unit::It(t) => t.id,
            Unit::Ot(t) => t.id,
            Unit::Fu(f) => f.id,
            Unit::Other { id, .. } => *id,
        }
    }
    fn sources(&self) -> &[u8] {
        match self {
            Unit::It(_) => &[],
            Unit::Ot(t) => core::slice::from_ref(&t.source),
            Unit::Fu(f) => core::slice::from_ref(&f.source),
            Unit::Other { sources, .. } => sources,
        }
    }
}

/// One AudioControl interface.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Control {
    /// bcdADC (0x0100 for UAC1); None without a header.
    pub adc: Option<u16>,
    /// baInterfaceNr: the streaming interfaces of this function.
    pub streams: Vec<u8>,
    pub units: Vec<Unit>,
    /// Descriptors skipped as malformed.
    pub bad: u32,
}

fn le16(d: &[u8], i: usize) -> u16 {
    u16::from_le_bytes([d[i], d[i + 1]])
}

fn le24(d: &[u8], i: usize) -> u32 {
    d[i] as u32 | (d[i + 1] as u32) << 8 | (d[i + 2] as u32) << 16
}

/// Walks length-prefixed descriptors; `f` sees each whole one. A length below 2 or past the end ends the walk
/// (the rest cannot be framed) and counts as bad.
fn walk(b: &[u8], bad: &mut u32, mut f: impl FnMut(&[u8])) {
    let mut i = 0;
    while i < b.len() {
        let len = b[i] as usize;
        if len < 2 || i + len > b.len() {
            *bad += 1;
            return;
        }
        f(&b[i..i + len]);
        i += len;
    }
}

/// Bytes `[at, at + n)` of `d`, or None when the descriptor is too short.
fn take(d: &[u8], at: usize, n: usize) -> Option<&[u8]> {
    d.get(at..at.checked_add(n)?)
}

fn parse_unit(d: &[u8]) -> Option<Unit> {
    let id = *d.get(3)?;
    if id == 0 {
        return None; // 0 is "no unit" in bSourceID; a unit with it could never be reached.
    }
    match d[2] {
        ac::INPUT_TERMINAL if d.len() >= 12 => Some(Unit::It(InputTerminal { id, ty: le16(d, 4), channels: d[7], channel_config: le16(d, 8) })),
        ac::OUTPUT_TERMINAL if d.len() >= 9 => Some(Unit::Ot(OutputTerminal { id, ty: le16(d, 4), source: d[7] })),
        ac::FEATURE_UNIT if d.len() >= 7 => {
            let size = d[5] as usize;
            if size == 0 || size > 4 {
                return None;
            }
            // bLength = 7 + (ch + 1) * bControlSize; the last byte is iFeature.
            let n = (d.len() - 7) / size;
            if n == 0 {
                return None;
            }
            let controls = (0..n)
                .map(|c| take(d, 6 + c * size, size).map(|b| b.iter().rev().fold(0u32, |a, &x| a << 8 | x as u32)))
                .collect::<Option<Vec<u32>>>()?;
            Some(Unit::Fu(FeatureUnit { id, source: d[4], controls }))
        }
        ac::MIXER_UNIT | ac::SELECTOR_UNIT if d.len() >= 5 => {
            let p = d[4] as usize;
            Some(Unit::Other { id, subtype: d[2], sources: take(d, 5, p)?.to_vec() })
        }
        ac::PROCESSING_UNIT | ac::EXTENSION_UNIT if d.len() >= 7 => {
            let p = d[6] as usize;
            Some(Unit::Other { id, subtype: d[2], sources: take(d, 7, p)?.to_vec() })
        }
        _ => None,
    }
}

impl Control {
    /// The class-specific descriptors of an AudioControl interface (its `extra`).
    pub fn parse(extra: &[u8]) -> Control {
        let mut c = Control::default();
        let mut bad = 0;
        walk(extra, &mut bad, |d| {
            if d.len() < 3 || d[1] != CS_INTERFACE {
                return; // a foreign descriptor (vendor, HID...) is not ours
            }
            if d[2] == ac::HEADER {
                if d.len() < 8 || c.adc.is_some() {
                    c.bad += 1;
                    return;
                }
                c.adc = Some(le16(d, 3));
                let n = d[7] as usize;
                match take(d, 8, n) {
                    Some(s) => c.streams = s.to_vec(),
                    None => c.bad += 1,
                }
                return;
            }
            match parse_unit(d) {
                // A second unit with one ID would make paths ambiguous: the first one counts.
                Some(u) if !c.units.iter().any(|x| x.id() == u.id()) => c.units.push(u),
                _ => c.bad += 1,
            }
        });
        c.bad += bad;
        c
    }

    pub fn unit(&self, id: u8) -> Option<&Unit> {
        self.units.iter().find(|u| u.id() == id)
    }

    /// The path from output terminal `ot` up to input terminal `it` (OT first), if one exists. Iterative
    /// depth-first search with a visited list: loops and dangling source IDs end a branch, never the kernel.
    pub fn path(&self, it: u8, ot: u8) -> Option<Vec<u8>> {
        let mut visited: Vec<u8> = Vec::new();
        // Each entry: the path so far (top = the unit being expanded).
        let mut stack: Vec<Vec<u8>> = alloc::vec![alloc::vec![ot]];
        while let Some(p) = stack.pop() {
            let Some(&id) = p.last() else { continue };
            if id == it {
                return Some(p);
            }
            if visited.contains(&id) {
                continue;
            }
            visited.push(id);
            let Some(u) = self.unit(id) else { continue };
            for &s in u.sources() {
                if !visited.contains(&s) && s != 0 {
                    let mut q = p.clone();
                    q.push(s);
                    stack.push(q);
                }
            }
        }
        None
    }

    /// The feature units on a path, nearest the output terminal first.
    pub fn features_on(&self, path: &[u8]) -> Vec<FeatureUnit> {
        path.iter().filter_map(|&id| match self.unit(id) {
            Some(Unit::Fu(f)) => Some(f.clone()),
            _ => None,
        }).collect()
    }

    /// For playback from input terminal `it` (USB streaming): the output terminals it reaches, with the path.
    pub fn outputs_from(&self, it: u8) -> Vec<(OutputTerminal, Vec<u8>)> {
        self.units.iter().filter_map(|u| match u {
            Unit::Ot(t) if t.ty != TT_USB_STREAMING => self.path(it, t.id).map(|p| (t.clone(), p)),
            _ => None,
        }).collect()
    }

    /// For capture into output terminal `ot` (USB streaming): the input terminal it comes from, with the path.
    pub fn input_to(&self, ot: u8) -> Option<(InputTerminal, Vec<u8>)> {
        self.units.iter().find_map(|u| match u {
            Unit::It(t) if t.ty != TT_USB_STREAMING => self.path(t.id, ot).map(|p| (t.clone(), p)),
            _ => None,
        })
    }

    pub fn input_terminal(&self, id: u8) -> Option<&InputTerminal> {
        match self.unit(id) {
            Some(Unit::It(t)) => Some(t),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Rates {
    Continuous { min: u32, max: u32 },
    Discrete(Vec<u32>),
}

impl Rates {
    pub fn has(&self, r: u32) -> bool {
        match self {
            Rates::Continuous { min, max } => (*min..=*max).contains(&r),
            Rates::Discrete(v) => v.contains(&r),
        }
    }
    /// Exactly this one rate: the device cannot play another, so a refused SET_CUR changes nothing.
    pub fn only(&self, r: u32) -> bool {
        matches!(self, Rates::Discrete(v) if v.iter().all(|&x| x == r) && !v.is_empty())
    }
}

/// A streaming alternate setting's class-specific part (AS_GENERAL + FORMAT_TYPE I).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StreamFormat {
    pub terminal: u8,
    pub delay: u8,
    pub tag: u16,
    pub format_type: u8,
    pub channels: u8,
    pub subframe: u8,
    pub bits: u8,
    pub rates: Rates,
}

/// Parses an AS alternate setting's `extra`; None when AS_GENERAL or FORMAT_TYPE is missing or malformed.
pub fn stream_format(extra: &[u8]) -> Option<StreamFormat> {
    let mut general: Option<(u8, u8, u16)> = None;
    let mut fmt: Option<(u8, u8, u8, u8, Rates)> = None;
    let mut bad = 0;
    walk(extra, &mut bad, |d| {
        if d.len() < 3 || d[1] != CS_INTERFACE {
            return;
        }
        match d[2] {
            as_::GENERAL if d.len() >= 7 && general.is_none() => general = Some((d[3], d[4], le16(d, 5))),
            as_::FORMAT_TYPE if d.len() >= 8 && fmt.is_none() => {
                let n = d[7] as usize;
                let rates = if n == 0 {
                    take(d, 8, 6).map(|b| Rates::Continuous { min: le24(b, 0), max: le24(b, 3) })
                } else {
                    n.checked_mul(3).and_then(|len| take(d, 8, len)).map(|b| Rates::Discrete(b.chunks_exact(3).map(|x| le24(x, 0)).collect()))
                };
                if let Some(r) = rates {
                    fmt = Some((d[3], d[4], d[5], d[6], r));
                }
            }
            _ => {}
        }
    });
    let (terminal, delay, tag) = general?;
    let (format_type, channels, subframe, bits, rates) = fmt?;
    Some(StreamFormat { terminal, delay, tag, format_type, channels, subframe, bits, rates })
}

/// CS_ENDPOINT EP_GENERAL (bmAttributes, bLockDelayUnits, wLockDelay); None if absent or malformed.
pub fn endpoint_general(extra: &[u8]) -> Option<(u8, u8, u16)> {
    let mut out = None;
    let mut bad = 0;
    walk(extra, &mut bad, |d| {
        if out.is_none() && d.len() >= 7 && d[1] == CS_ENDPOINT && d[2] == 1 {
            out = Some((d[3], d[4], le16(d, 5)));
        }
    });
    out
}
