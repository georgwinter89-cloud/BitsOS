//! Input pins (microphones, line in) and their way to an ADC, and the verbs that set one up for recording
//! (SN-9; Linux `hda_generic.c` `parse_capture_source`/`set_pin_target`, `hda_codec.c:3905`
//! `snd_hda_get_default_vref`).
//!
//! An input is a pin whose default configuration says mic or line in, that can take input and is wired to
//! something. Its path runs from an ADC (an input converter, the start of the list) through mixers and
//! selectors to the pin (the end), at most [`crate::path::DEFAULT_DEPTH`] widgets, first match in node and
//! connection-list order; the first ADC (lowest node) that reaches the pin is taken.

use alloc::vec::Vec;

use crate::caps::{Connectivity, Device, WidgetType};
use crate::model::{Codec, FunctionGroup, Widget};
use crate::path::{DEFAULT_DEPTH, Hop, JackSense, jack_sense};
use crate::setup::{Action, D0_WAIT_MS, SetupError, Stream};
use crate::verb::*;
use crate::Nid;

/// Pin widget control: input enable (`AC_PINCTL_IN_EN`).
pub const PINCTL_IN_EN: u8 = 0x20;
/// Pin widget control VREF values (`AC_PINCTL_VREF_*`).
pub const VREF_HIZ: u8 = 0;
pub const VREF_50: u8 = 1;
pub const VREF_GRD: u8 = 2;
pub const VREF_80: u8 = 4;
pub const VREF_100: u8 = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputKind {
    Mic,
    LineIn,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Input {
    pub group: Nid,
    pub pin: Nid,
    pub kind: InputKind,
    /// Fixed (built in, e.g. an internal microphone), not a jack.
    pub fixed: bool,
    /// ADC first, pin last; every hop but the pin names the input index it takes.
    pub path: Vec<Hop>,
    pub jack: Option<JackSense>,
}

impl Input {
    /// The ADC at the start of the path.
    pub fn converter(&self) -> Nid {
        self.path.first().map_or(self.pin, |h| h.nid)
    }
}

/// Is pin `w` an input, and which kind?
pub fn input_kind(w: &Widget) -> Option<InputKind> {
    if w.kind() != WidgetType::Pin || w.incomplete || w.config.connectivity() == Connectivity::None {
        return None;
    }
    if w.pin_caps.0 != 0 && !w.pin_caps.input() {
        return None;
    }
    match w.config.device() {
        Device::Mic => Some(InputKind::Mic),
        Device::LineIn => Some(InputKind::LineIn),
        _ => None,
    }
}

/// An ADC we record from: 48 kHz 16 bit (mono or stereo).
fn adc_fits(w: &Widget) -> bool {
    w.kind() == WidgetType::Input && !w.incomplete && w.pcm.takes_48k_16()
}

/// Every input of every audio function group, in pin order, each with a path from an ADC.
pub fn inputs(codec: &Codec) -> Vec<Input> {
    let mut v = Vec::new();
    for g in codec.audio_groups() {
        for w in &g.widgets {
            let Some(kind) = input_kind(w) else { continue };
            let Some(path) = g.widgets.iter().filter(|a| adc_fits(a)).find_map(|a| path_to(g, a.nid, w.nid, DEFAULT_DEPTH)) else {
                continue;
            };
            v.push(Input {
                group: g.nid,
                pin: w.nid,
                kind,
                fixed: w.config.connectivity() == Connectivity::Fixed,
                path,
                jack: jack_sense(w),
            });
        }
    }
    v
}

/// The path from ADC `adc` to `pin` within `depth` widgets.
pub fn path_to(group: &FunctionGroup, adc: Nid, pin: Nid, depth: u8) -> Option<Vec<Hop>> {
    let mut path = Vec::new();
    walk(group, adc, pin, 0, depth, &mut path).then_some(path)
}

fn walk(g: &FunctionGroup, nid: Nid, pin: Nid, d: u8, depth: u8, path: &mut Vec<Hop>) -> bool {
    if path.iter().any(|h| h.nid == nid) {
        return false;
    }
    let Some(w) = g.widget(nid).filter(|w| !w.incomplete) else { return false };
    if nid == pin {
        path.push(Hop { nid, input: None });
        return true;
    }
    let pass = match w.kind() {
        WidgetType::Input => d == 0,
        WidgetType::Mixer | WidgetType::Selector => d > 0 && d < depth,
        _ => false,
    };
    if !pass {
        return false;
    }
    for (i, &next) in w.conns.nodes.iter().enumerate() {
        path.push(Hop { nid, input: Some(i) });
        if walk(g, next, pin, d + 1, depth, path) {
            return true;
        }
        path.pop();
    }
    false
}

/// The VREF a microphone pin gets (Linux `snd_hda_get_default_vref` without its "was 50 %" exception): 80 %,
/// else 50 %, 100 %, ground, Hi-Z by the pin caps (bits 15:8). Line in: none.
pub fn vref(w: &Widget, kind: InputKind) -> u8 {
    if kind != InputKind::Mic {
        return VREF_HIZ;
    }
    let caps = (w.pin_caps.0 >> 8) & 0x37;
    if caps & 0x10 != 0 {
        VREF_80
    } else if caps & 0x02 != 0 {
        VREF_50
    } else if caps & 0x20 != 0 {
        VREF_100
    } else if caps & 0x04 != 0 {
        VREF_GRD
    } else {
        VREF_HIZ
    }
}

/// Where the capture gain sits: the first input amp with gain steps from the ADC on (the ADC's own on the
/// ALC269), as (node, input index, caps). Linux names it "Capture Volume"/"Capture Switch".
pub fn capture_amp(codec: &Codec, inp: &Input) -> Option<(Nid, u8, crate::caps::AmpCaps)> {
    let g = codec.groups.iter().find(|g| g.nid == inp.group)?;
    inp.path.iter().filter_map(|h| Some((g.widget(h.nid)?, h.input?))).find_map(|(w, i)| {
        (w.caps.in_amp() && w.in_amp.steps() > 0 && i <= 15).then_some((w.nid, i as u8, w.in_amp))
    })
}

/// Everything to record from `inp` on `stream`: the group and the path's widgets in D0; selectors and ADCs
/// switched to the path's input; the used input amps unmuted at 0 dB (mixers: only the path's input, the
/// others stay as they are); output amps of mixers/selectors at 0 dB; the pin's boost at 0 dB; the ADC's
/// format and stream; the pin on for input with its VREF (mics).
pub fn input_setup(codec: &Codec, inp: &Input, stream: &Stream) -> Result<Vec<Action>, SetupError> {
    let stream_verb = stream.verb()?;
    let g = codec.groups.iter().find(|g| g.nid == inp.group).ok_or(SetupError::UnknownNode(inp.group))?;
    let mut list = Vec::new();
    list.push(Action::Set { nid: g.nid, verb: set(SET_POWER_STATE, PWRST_D0) });
    list.push(Action::WaitFor { nid: g.nid, verb: GET_POWER_STATE, mask: 0xF0, value: 0, timeout_ms: D0_WAIT_MS });
    for hop in &inp.path {
        let w = g.widget(hop.nid).ok_or(SetupError::UnknownNode(hop.nid))?;
        if w.caps.power_control() {
            list.push(Action::Set { nid: hop.nid, verb: set(SET_POWER_STATE, PWRST_D0) });
        }
    }
    for hop in &inp.path {
        let w = g.widget(hop.nid).ok_or(SetupError::UnknownNode(hop.nid))?;
        let Some(input) = hop.input else { continue };
        if w.kind() != WidgetType::Mixer && w.conns.nodes.len() > 1 {
            let index = u8::try_from(input).map_err(|_| SetupError::IndexTooLarge(hop.nid))?;
            list.push(Action::Set { nid: hop.nid, verb: set(SET_CONNECT_SEL, index) });
        }
        if w.caps.in_amp() && input <= 15 {
            list.push(Action::Set { nid: hop.nid, verb: set_in_amp(input as u8, w.in_amp.zero_db_gain()) });
        }
        if w.kind() != WidgetType::Input && w.caps.out_amp() {
            list.push(Action::Set { nid: hop.nid, verb: set_out_amp(w.out_amp.zero_db_gain()) });
        }
    }
    let adc = inp.converter();
    list.push(Action::Set { nid: adc, verb: set_format(stream.format) });
    list.push(Action::Set { nid: adc, verb: stream_verb });
    let pin = g.widget(inp.pin).ok_or(SetupError::UnknownNode(inp.pin))?;
    if pin.caps.in_amp() {
        list.push(Action::Set { nid: inp.pin, verb: set_in_amp(0, pin.in_amp.zero_db_gain()) });
    }
    list.push(Action::Set { nid: inp.pin, verb: set(SET_PIN_WIDGET_CONTROL, PINCTL_IN_EN | vref(pin, inp.kind)) });
    Ok(list)
}
