//! Verb lists that set an output up, as data for the controller driver (V1 `path.asm`
//! `path_setup`, `codec.asm` power-up, `switch.asm` jack sense). The driver sends them in order
//! to the codec the output belongs to.

use alloc::vec::Vec;

use crate::caps::WidgetType;
use crate::model::Codec;
use crate::path::{JackSense, Output, OutputKind};
use crate::verb::*;
use crate::{Nid, ati};

/// How long V1 waits for a function group to report D0 before trying anyway.
pub const D0_WAIT_MS: u32 = 200;

/// One step of a setup list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Send `verb` to `nid`; the answer does not matter.
    Set { nid: Nid, verb: u32 },
    /// Read `verb` until `answer & mask == value`; after `timeout_ms` go on anyway.
    WaitFor {
        nid: Nid,
        verb: u32,
        mask: u32,
        value: u32,
        timeout_ms: u32,
    },
    /// Read `get`; if the payload changes by clearing `clear` and setting `set_bits`, send the
    /// 12-bit verb `set` with the new payload (see [`Action::update_verb`]).
    UpdateBits {
        nid: Nid,
        get: u32,
        set: u16,
        clear: u8,
        set_bits: u8,
    },
}

impl Action {
    pub fn nid(&self) -> Nid {
        match *self {
            Action::Set { nid, .. }
            | Action::WaitFor { nid, .. }
            | Action::UpdateBits { nid, .. } => nid,
        }
    }

    /// For [`Action::UpdateBits`]: the verb to send after `get` answered `answer`, or `None` if
    /// nothing changes (or for the other actions).
    pub fn update_verb(&self, answer: u32) -> Option<u32> {
        let Action::UpdateBits {
            set,
            clear,
            set_bits,
            ..
        } = *self
        else {
            return None;
        };
        let old = (answer & 0xFF) as u8;
        let new = (old & !clear) | set_bits;
        (new != old).then(|| crate::verb::set(set, new))
    }
}

/// The stream a converter is put on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stream {
    /// Stream tag, 1..=15, assigned by the controller driver.
    pub tag: u8,
    /// First channel, 0..=15.
    pub channel: u8,
    /// Converter format (V1: 0x0011 = 48 kHz, 16 bit, 2 channels).
    pub format: u16,
}

impl Default for Stream {
    fn default() -> Self {
        Stream {
            tag: 1,
            channel: 0,
            format: 0x0011,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetupError {
    BadStream,
    /// The output's group or a node of its path is not in the codec model.
    UnknownNode(Nid),
    /// The path needs a connection index the select verb cannot carry (above 255).
    IndexTooLarge(Nid),
    /// AMD codec: the path is not pin -> converter (V1 sets only that shape).
    AtiPathShape,
}

impl Stream {
    pub(crate) fn verb(&self) -> Result<u32, SetupError> {
        if self.tag == 0 || self.tag > 15 || self.channel > 15 {
            return Err(SetupError::BadStream);
        }
        Ok(set(SET_CHANNEL_STREAMID, (self.tag << 4) | self.channel))
    }
}

/// The function group in D0 (and waited for), then every widget of the path that has power
/// control in D0.
pub fn power_up(codec: &Codec, out: &Output) -> Result<Vec<Action>, SetupError> {
    let group = codec
        .groups
        .iter()
        .find(|g| g.nid == out.group)
        .ok_or(SetupError::UnknownNode(out.group))?;
    let mut list = Vec::new();
    list.push(Action::Set {
        nid: group.nid,
        verb: set(SET_POWER_STATE, PWRST_D0),
    });
    list.push(Action::WaitFor {
        nid: group.nid,
        verb: GET_POWER_STATE,
        mask: 0xF0,
        value: 0,
        timeout_ms: D0_WAIT_MS,
    });
    for hop in &out.path {
        let w = group
            .widget(hop.nid)
            .ok_or(SetupError::UnknownNode(hop.nid))?;
        if w.caps.power_control() {
            list.push(Action::Set {
                nid: hop.nid,
                verb: set(SET_POWER_STATE, PWRST_D0),
            });
        }
    }
    Ok(list)
}

/// Everything to make `out` play `stream`: [`power_up`], then the AMD list ([`ati::setup`]) for
/// a digital output on an AMD codec, else the generic list ([`path_setup`]).
pub fn output_setup(
    codec: &Codec,
    out: &Output,
    stream: &Stream,
) -> Result<Vec<Action>, SetupError> {
    let mut list = power_up(codec, out)?;
    if codec.is_ati() && out.kind == OutputKind::Digital {
        list.extend(ati::setup(codec, out, stream)?);
    } else {
        list.extend(path_setup(codec, out, stream)?);
    }
    Ok(list)
}

/// V1's generic path set-up, in V1's order: for each node from the pin to the converter its
/// output amp at 0 dB unmuted, the input selected (not on mixers, which add their inputs), the
/// used input's amp at 0 dB unmuted (not on the pin: that one is for recording); then the
/// converter's format and stream; the pin on (headphone drive for headphones); EAPD on.
pub fn path_setup(codec: &Codec, out: &Output, stream: &Stream) -> Result<Vec<Action>, SetupError> {
    let stream_verb = stream.verb()?;
    let group = codec
        .groups
        .iter()
        .find(|g| g.nid == out.group)
        .ok_or(SetupError::UnknownNode(out.group))?;
    let pin = group
        .widget(out.pin)
        .ok_or(SetupError::UnknownNode(out.pin))?;
    let mut list = Vec::new();
    for (depth, hop) in out.path.iter().enumerate() {
        let w = group
            .widget(hop.nid)
            .ok_or(SetupError::UnknownNode(hop.nid))?;
        if w.caps.out_amp() {
            list.push(Action::Set {
                nid: hop.nid,
                verb: set_out_amp(w.out_amp.zero_db_gain()),
            });
        }
        let Some(input) = hop.input else { continue };
        if w.kind() != WidgetType::Mixer && w.conns.nodes.len() > 1 {
            let index = u8::try_from(input).map_err(|_| SetupError::IndexTooLarge(hop.nid))?;
            list.push(Action::Set {
                nid: hop.nid,
                verb: set(SET_CONNECT_SEL, index),
            });
        }
        // The amp verb has 4 bits for the input index; V1 leaves higher inputs alone.
        if depth > 0 && w.caps.in_amp() && input <= 15 {
            list.push(Action::Set {
                nid: hop.nid,
                verb: set_in_amp(input as u8, w.in_amp.zero_db_gain()),
            });
        }
    }
    let cvt = out.converter();
    list.push(Action::Set {
        nid: cvt,
        verb: set_format(stream.format),
    });
    list.push(Action::Set {
        nid: cvt,
        verb: stream_verb,
    });
    let hp = out.kind == OutputKind::Headphone && (pin.pin_caps.hp_drive() || pin.pin_caps.0 == 0);
    let ctl = PINCTL_OUT_EN | if hp { PINCTL_HP_EN } else { 0 };
    list.push(Action::Set {
        nid: out.pin,
        verb: set(SET_PIN_WIDGET_CONTROL, ctl),
    });
    if pin.pin_caps.eapd() {
        list.push(Action::Set {
            nid: out.pin,
            verb: set(SET_EAPD_BTLENABLE, EAPD_ENABLE),
        });
    }
    Ok(list)
}

impl JackSense {
    /// What to send before reading [`GET_PIN_SENSE`]: the execute verb where the pin needs it.
    pub fn before_read(&self) -> Option<Action> {
        self.trigger.then_some(Action::Set {
            nid: self.pin,
            verb: set(SET_PIN_SENSE, 0),
        })
    }

    /// The verb whose answer tells the plug state.
    pub fn read_verb(&self) -> u32 {
        GET_PIN_SENSE
    }

    /// Something is plugged in (F09 bit 31).
    pub fn plugged(answer: u32) -> bool {
        answer & (1 << 31) != 0
    }

    /// HDMI/DP pins: the ELD is valid (F09 bit 30).
    pub fn eld_valid(answer: u32) -> bool {
        answer & (1 << 30) != 0
    }

    /// Turn on unsolicited responses with `tag` (6 bits) for plug changes, if the pin can.
    pub fn unsol_enable(&self, tag: u8) -> Option<Action> {
        (self.unsol && tag <= 0x3F).then_some(Action::Set {
            nid: self.pin,
            verb: set(SET_UNSOLICITED_ENABLE, USRSP_EN | tag),
        })
    }
}
