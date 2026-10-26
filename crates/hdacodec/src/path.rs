//! Output pins and their way to a converter (V1 `path.asm` `pins_setup`/`find_dac`,
//! `pins.asm` `pin_fits`/`pin_sense`).
//!
//! Outputs are pins whose default configuration says line out, speaker or headphone (or that are
//! digital: HDMI/DP pin caps, S/PDIF or "digital other" out) and that are wired to something.
//! The way to a converter goes through mixers and selectors only, at most [`DEFAULT_DEPTH`]
//! widgets behind the pin, first match in connection-list order (V1's depth-first search with its
//! "failed at this depth" memo). A way that runs through a loop is shortened afterwards, so every
//! returned path visits each node once.

use alloc::vec::Vec;

use crate::caps::{Connectivity, Device, WidgetType};
use crate::model::{Codec, FunctionGroup, Widget};
use crate::{NID_LAST, Nid};

/// Widgets behind the pin, as in V1 (`PATH_MAX`).
pub const DEFAULT_DEPTH: u8 = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputKind {
    LineOut,
    Speaker,
    Headphone,
    /// HDMI, DisplayPort or S/PDIF.
    Digital,
}

/// One step of a path: a node and, except for the converter at the end, the index in its
/// connection list of the next node.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hop {
    pub nid: Nid,
    pub input: Option<usize>,
}

/// How to ask a pin whether something is plugged in (V1 `pin_sense`, `jack_sense`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JackSense {
    pub pin: Nid,
    /// Send verb 709 (execute) before reading F09.
    pub trigger: bool,
    /// The pin can send unsolicited responses on a plug change.
    pub unsol: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Output {
    /// The audio function group the pin belongs to.
    pub group: Nid,
    pub pin: Nid,
    pub kind: OutputKind,
    pub hdmi: bool,
    pub dp: bool,
    /// AMD codecs: the audio endpoint behind this pin (`atihdmi_port2pin`: pin = endpoint * 2 + 3).
    pub endpoint: Option<u8>,
    /// Pin first, converter last.
    pub path: Vec<Hop>,
    pub jack: Option<JackSense>,
}

impl Output {
    /// The converter (DAC) at the end of the path.
    pub fn converter(&self) -> Nid {
        self.path.last().map_or(self.pin, |h| h.nid)
    }
}

/// Is pin `w` an output, and which kind? (V1 `pin_fits` without the slot split.)
pub fn output_kind(w: &Widget) -> Option<OutputKind> {
    if w.kind() != WidgetType::Pin || w.incomplete || w.config.connectivity() == Connectivity::None
    {
        return None;
    }
    // Some old codecs report no pin caps at all: trust the configuration then (V1, Linux).
    if w.pin_caps.0 != 0 && !w.pin_caps.output() {
        return None;
    }
    if w.pin_caps.digital() {
        return Some(OutputKind::Digital);
    }
    match w.config.device() {
        Device::LineOut => Some(OutputKind::LineOut),
        Device::Speaker => Some(OutputKind::Speaker),
        Device::Headphone => Some(OutputKind::Headphone),
        Device::SpdifOut | Device::DigitalOtherOut => Some(OutputKind::Digital),
        _ => None,
    }
}

/// Jack sense for pin `w`, if its jack can tell a plug: presence detect in the pin caps, not
/// switched off in the configuration's misc bits, and a jack (not fixed, not unconnected).
pub fn jack_sense(w: &Widget) -> Option<JackSense> {
    let jack = matches!(
        w.config.connectivity(),
        Connectivity::Jack | Connectivity::Both
    );
    (w.pin_caps.presence_detect() && !w.config.no_presence_detect() && jack).then_some(JackSense {
        pin: w.nid,
        trigger: w.pin_caps.trigger_required(),
        unsol: w.caps.unsol(),
    })
}

/// Every output of every audio function group, in node order, each with a path.
pub fn outputs(codec: &Codec) -> Vec<Output> {
    let mut out = Vec::new();
    for g in codec.audio_groups() {
        let mut search = Search::new(g, codec.is_ati(), DEFAULT_DEPTH);
        for w in &g.widgets {
            let Some(kind) = output_kind(w) else { continue };
            let Some(path) = search.run(w.nid) else {
                continue;
            };
            let endpoint =
                (codec.is_ati() && w.nid >= 3 && (w.nid - 3) % 2 == 0).then(|| (w.nid - 3) / 2);
            out.push(Output {
                group: g.nid,
                pin: w.nid,
                kind,
                hdmi: w.pin_caps.hdmi(),
                dp: w.pin_caps.dp(),
                endpoint,
                path,
                jack: jack_sense(w),
            });
        }
    }
    out
}

/// The path from `pin` to a fitting converter within `depth` widgets, if any.
pub fn find_path(group: &FunctionGroup, pin: Nid, ati: bool, depth: u8) -> Option<Vec<Hop>> {
    Search::new(group, ati, depth).run(pin)
}

/// A converter V1 would play on: stereo, 48 kHz 16 bit. AMD's HDMI converters report less than
/// they take, so any converter fits there (V1 `dac_fits`, Linux `patch_hdmi.c`).
fn converter_fits(w: &Widget, ati: bool) -> bool {
    ati || (w.caps.stereo() && w.pcm.takes_48k_16())
}

struct Search<'a> {
    group: &'a FunctionGroup,
    ati: bool,
    depth: u8,
    /// Per node: the smallest depth at which no converter lay within reach (u8::MAX = never).
    failed: [u8; NID_LAST as usize + 1],
    path: Vec<Hop>,
}

impl<'a> Search<'a> {
    fn new(group: &'a FunctionGroup, ati: bool, depth: u8) -> Self {
        Search {
            group,
            ati,
            depth,
            failed: [u8::MAX; NID_LAST as usize + 1],
            path: Vec::new(),
        }
    }

    fn run(&mut self, pin: Nid) -> Option<Vec<Hop>> {
        self.path.clear();
        if !self.visit(pin, 0) {
            return None;
        }
        let mut path = core::mem::take(&mut self.path);
        cut_loops(&mut path);
        Some(path)
    }

    fn visit(&mut self, nid: Nid, d: u8) -> bool {
        if nid > NID_LAST || d >= self.failed[nid as usize] {
            return false;
        }
        let group = self.group;
        let found = match group.widget(nid) {
            Some(w) if !w.incomplete => self.try_widget(w, d),
            _ => false,
        };
        if !found {
            self.failed[nid as usize] = d;
        }
        found
    }

    fn try_widget(&mut self, w: &Widget, d: u8) -> bool {
        match w.kind() {
            WidgetType::Output if d > 0 => {
                let fits = converter_fits(w, self.ati);
                if fits {
                    self.path.push(Hop {
                        nid: w.nid,
                        input: None,
                    });
                }
                fits
            }
            WidgetType::Pin if d == 0 => self.try_inputs(w, d),
            WidgetType::Mixer | WidgetType::Selector if d > 0 && d < self.depth => {
                self.try_inputs(w, d)
            }
            _ => false,
        }
    }

    fn try_inputs(&mut self, w: &Widget, d: u8) -> bool {
        for (i, &next) in w.conns.nodes.iter().enumerate() {
            self.path.push(Hop {
                nid: w.nid,
                input: Some(i),
            });
            if self.visit(next, d + 1) {
                return true;
            }
            self.path.pop();
        }
        false
    }
}

/// Removes detours: if a node appears twice, keep its first place with the input of its last.
fn cut_loops(path: &mut Vec<Hop>) {
    let mut i = 0;
    while i < path.len() {
        if let Some(j) = path
            .iter()
            .rposition(|h| h.nid == path[i].nid)
            .filter(|&j| j > i)
        {
            path[i].input = path[j].input;
            path.drain(i + 1..=j);
        }
        i += 1;
    }
}
