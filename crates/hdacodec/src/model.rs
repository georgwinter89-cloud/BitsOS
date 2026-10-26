//! The codec model, built from read-only verbs only (V1 `codec.asm` `codec_setup`,
//! `widget.asm` `widget_read`). Building never powers anything up; that is in the setup lists.
//!
//! A read that gets no answer is noted in [`Codec::missing`]; the widget it belongs to is then
//! marked incomplete and the path finder ignores it, so a codec that is half silent still gives
//! the outputs that answered fully.

use alloc::vec::Vec;

use crate::caps::{AmpCaps, PcmCaps, PinCaps, PinConfig, WidgetCaps, WidgetType};
use crate::conn::{self, ConnList};
use crate::verb::*;
use crate::{NID_LAST, Nid, NoResponse, VerbSource};

/// One widget of an audio function group.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Widget {
    pub nid: Nid,
    pub caps: WidgetCaps,
    /// Pins only.
    pub pin_caps: PinCaps,
    /// Pins only.
    pub config: PinConfig,
    /// The widget's own amp caps if it overrides, else the group's.
    pub out_amp: AmpCaps,
    pub in_amp: AmpCaps,
    /// The converter's own PCM caps if it overrides, else the group's.
    pub pcm: PcmCaps,
    pub stream_formats: u32,
    pub conns: ConnList,
    /// A read for this widget got no answer; its other fields cannot be trusted.
    pub incomplete: bool,
}

impl Widget {
    pub fn kind(&self) -> WidgetType {
        self.caps.kind()
    }
}

/// A function group (parameter 0x05); widgets are read for audio groups only.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FunctionGroup {
    pub nid: Nid,
    /// Parameter 0x05 as read; type in bits 7:0 (1 = audio, 2 = modem).
    pub type_param: u32,
    pub pcm: PcmCaps,
    pub stream_formats: u32,
    pub out_amp: AmpCaps,
    pub in_amp: AmpCaps,
    /// Sorted by node ID, no duplicates.
    pub widgets: Vec<Widget>,
}

impl FunctionGroup {
    pub fn is_audio(&self) -> bool {
        self.type_param & 0xFF == 1
    }
    pub fn widget(&self, nid: Nid) -> Option<&Widget> {
        self.widgets
            .binary_search_by_key(&nid, |w| w.nid)
            .ok()
            .map(|i| &self.widgets[i])
    }
}

/// One codec on the link.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Codec {
    pub cad: u8,
    pub vendor_id: u32,
    pub revision_id: u32,
    pub groups: Vec<FunctionGroup>,
    /// Reads that got no answer, as (node, verb).
    pub missing: Vec<(Nid, u32)>,
}

/// The node IDs a "subordinate node count" answer (parameter 0x04) names: start in bits 23:16,
/// count in bits 7:0. Nodes above [`NID_LAST`] cannot be addressed and node 0 is the root, so
/// both are left out.
pub fn sub_nodes(param: u32) -> impl Iterator<Item = Nid> {
    let start = (param >> 16) & 0xFF;
    let count = param & 0xFF;
    (start..start + count)
        .filter(|&n| n >= 1 && n <= NID_LAST as u32)
        .map(|n| n as Nid)
}

struct Reader<'a, S> {
    src: &'a mut S,
    missing: &'a mut Vec<(Nid, u32)>,
}

impl<S: VerbSource> Reader<'_, S> {
    fn ask(&mut self, nid: Nid, verb: u32) -> Option<u32> {
        let r = self.src.get(nid, verb).ok();
        if r.is_none() {
            self.missing.push((nid, verb));
        }
        r
    }
}

impl Codec {
    /// Reads codec `cad` from `src`. Only a silent root (no vendor ID) is an error.
    pub fn read<S: VerbSource>(src: &mut S, cad: u8) -> Result<Codec, NoResponse> {
        let vendor_id = src.get(0, get_param(PAR_VENDOR_ID))?;
        let mut codec = Codec {
            cad,
            vendor_id,
            ..Codec::default()
        };
        let mut missing = Vec::new();
        let mut r = Reader {
            src,
            missing: &mut missing,
        };
        codec.revision_id = r.ask(0, get_param(PAR_REVISION_ID)).unwrap_or(0);
        if let Some(nodes) = r.ask(0, get_param(PAR_NODE_COUNT)) {
            // Each node belongs to one place only: a hostile codec whose ranges overlap (or name
            // the groups themselves) cannot make the model bigger than the node space.
            let mut claimed = [false; NID_LAST as usize + 1];
            sub_nodes(nodes).for_each(|n| claimed[n as usize] = true);
            for nid in sub_nodes(nodes) {
                let Some(type_param) = r.ask(nid, get_param(PAR_FG_TYPE)) else {
                    continue;
                };
                let mut group = FunctionGroup {
                    nid,
                    type_param,
                    ..FunctionGroup::default()
                };
                if group.is_audio() {
                    read_audio_group(&mut r, &mut group, &mut claimed);
                }
                codec.groups.push(group);
            }
        }
        codec.missing = missing;
        Ok(codec)
    }

    /// AMD/ATI codec (vendor 1002), set up its own way (`ati`).
    pub fn is_ati(&self) -> bool {
        self.vendor_id >> 16 == 0x1002
    }

    /// AMD codec revision 3 or later: only the AA01 codec counts (`linux-6.6/sound/pci/hda/patch_hdmi.c:4082`,
    /// `is_amdhdmi_rev3_or_later`; checked in SN-2).
    pub fn ati_rev3(&self) -> bool {
        self.vendor_id == 0x1002_AA01 && self.revision_id & 0xFF00 >= 0x0300
    }

    pub fn audio_groups(&self) -> impl Iterator<Item = &FunctionGroup> {
        self.groups.iter().filter(|g| g.is_audio())
    }
}

fn read_audio_group<S: VerbSource>(
    r: &mut Reader<'_, S>,
    g: &mut FunctionGroup,
    claimed: &mut [bool],
) {
    let n = g.nid;
    g.pcm = PcmCaps(r.ask(n, get_param(PAR_PCM)).unwrap_or(0));
    g.stream_formats = r.ask(n, get_param(PAR_STREAM)).unwrap_or(0);
    g.out_amp = AmpCaps(r.ask(n, get_param(PAR_AMP_OUT_CAP)).unwrap_or(0));
    g.in_amp = AmpCaps(r.ask(n, get_param(PAR_AMP_IN_CAP)).unwrap_or(0));
    let Some(nodes) = r.ask(n, get_param(PAR_NODE_COUNT)) else {
        return;
    };
    for nid in sub_nodes(nodes) {
        if core::mem::replace(&mut claimed[nid as usize], true) {
            continue;
        }
        if let Some(w) = read_widget(r, g, nid) {
            g.widgets.push(w);
        }
    }
}

fn read_widget<S: VerbSource>(
    r: &mut Reader<'_, S>,
    g: &FunctionGroup,
    nid: Nid,
) -> Option<Widget> {
    let caps = WidgetCaps(r.ask(nid, get_param(PAR_AUDIO_WIDGET_CAP))?);
    let mut w = Widget {
        nid,
        caps,
        out_amp: g.out_amp,
        in_amp: g.in_amp,
        pcm: g.pcm,
        stream_formats: g.stream_formats,
        ..Widget::default()
    };
    let ask = |r: &mut Reader<'_, S>, verb: u32, w: &mut Widget| {
        let v = r.ask(nid, verb);
        w.incomplete |= v.is_none();
        v.unwrap_or(0)
    };
    if caps.amp_override() {
        w.out_amp = AmpCaps(ask(r, get_param(PAR_AMP_OUT_CAP), &mut w));
        w.in_amp = AmpCaps(ask(r, get_param(PAR_AMP_IN_CAP), &mut w));
    }
    if caps.kind() == WidgetType::Pin {
        w.pin_caps = PinCaps(ask(r, get_param(PAR_PIN_CAP), &mut w));
        w.config = PinConfig(ask(r, GET_CONFIG_DEFAULT, &mut w));
    }
    if caps.conn_list() {
        let len = r.ask(nid, get_param(PAR_CONNLIST_LEN));
        w.incomplete |= len.is_none();
        let len = len.unwrap_or(0);
        let mut answers = Vec::new();
        for index in conn::reads(len) {
            match r.ask(nid, get_conn_list(index)) {
                Some(a) => answers.push(a),
                None => {
                    w.incomplete = true;
                    break;
                }
            }
        }
        w.conns = conn::decode(len, &answers);
    }
    if matches!(caps.kind(), WidgetType::Output | WidgetType::Input) && caps.format_override() {
        w.pcm = PcmCaps(ask(r, get_param(PAR_PCM), &mut w));
        w.stream_formats = ask(r, get_param(PAR_STREAM), &mut w);
    }
    Some(w)
}
