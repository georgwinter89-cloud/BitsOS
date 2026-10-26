//! AMD/ATI HDMI codec set-up (codec 1002:AA01 on the Mini-PC), in the order Linux v6.6
//! `patch_hdmi.c` uses: `atihdmi_init`, `hdmi_init_pin`, `hdmi_pcm_open`, the channel map and
//! infoframe, `atihdmi_setup_stream`, `hdmi_setup_stream`. Taken from V1 `snd/hdmi.asm`
//! (`ati_verbs`, lines 183-206). The list is data; which entries apply depends only on the model
//! (revision 3 or later, digital converter, amps present).

use alloc::vec::Vec;

use crate::model::Codec;
use crate::path::Output;
use crate::setup::{Action, SetupError, Stream};
use crate::verb::*;

/// Converter channels the stereo stream uses, per HDMI slot 0..=7 (`None` = slot off).
pub const STEREO_SLOTS: [Option<u8>; 8] = [Some(0), Some(1), None, None, None, None, None, None];

/// The ATI channel-map verb for HDMI slot `slot` (`atihdmi_pin_set_slot_channel`):
/// `ATI_VERB_SET_MULTICHANNEL_01 + slot / 2 + (slot % 2) * 0xE`.
pub const fn slot_verb(slot: u8) -> u16 {
    ATI_SET_MULTICHANNEL_01 + (slot / 2) as u16 + (slot % 2) as u16 * 0xE
}

/// The set-up after [`crate::setup::power_up`] for a digital output of an AMD codec whose path is
/// pin -> converter.
pub fn setup(codec: &Codec, out: &Output, stream: &Stream) -> Result<Vec<Action>, SetupError> {
    let stream_verb = stream.verb()?;
    let [pin_hop, cvt_hop] = out.path[..] else {
        return Err(SetupError::AtiPathShape);
    };
    let group = codec
        .groups
        .iter()
        .find(|g| g.nid == out.group)
        .ok_or(SetupError::UnknownNode(out.group))?;
    let pin = group
        .widget(pin_hop.nid)
        .ok_or(SetupError::UnknownNode(pin_hop.nid))?;
    let cvt = group
        .widget(cvt_hop.nid)
        .ok_or(SetupError::UnknownNode(cvt_hop.nid))?;
    let index = pin_hop
        .input
        .and_then(|i| u8::try_from(i).ok())
        .ok_or(SetupError::IndexTooLarge(pin.nid))?;
    let (p, c) = (pin.nid, cvt.nid);
    let rev3 = codec.ati_rev3();
    let digital = cvt.caps.digital();

    let mut list = Vec::new();
    let mut put = |nid, verb| list.push(Action::Set { nid, verb });
    put(p, set(ATI_SET_DOWNMIX_INFO, 0));
    if rev3 {
        put(p, set(ATI_SET_MULTICHANNEL_MODE, 1)); // single-stream mode
    }
    if pin.caps.out_amp() {
        put(p, set_out_amp(pin.out_amp.zero_db_gain()));
    }
    put(p, set(SET_PIN_WIDGET_CONTROL, PINCTL_OUT_EN));
    put(p, set(SET_CONNECT_SEL, index));
    if digital {
        put(c, set(SET_DIGI_CONVERT_1, DIG1_ENABLE));
    }
    if cvt.caps.out_amp() {
        put(c, set_out_amp(cvt.out_amp.zero_db_gain()));
    }
    if digital {
        put(c, set(SET_CVT_CHAN_COUNT, 1)); // channels - 1
    }
    for (slot, ch) in STEREO_SLOTS.iter().enumerate() {
        let payload = ch.map_or(0, |ch| (ch << 4) | ATI_MULTICHANNEL_ENABLE);
        put(p, set(slot_verb(slot as u8), payload));
    }
    put(p, set(ATI_SET_CHANNEL_ALLOCATION, 0)); // CA 0: FL FR
    if rev3 {
        put(c, set(ATI_SET_RAMP_RATE, 180));
    }
    list.push(Action::UpdateBits {
        nid: p,
        get: ATI_GET_HBR_CONTROL,
        set: ATI_SET_HBR_CONTROL,
        clear: ATI_HBR_ENABLE,
        set_bits: 0,
    });
    list.push(Action::Set {
        nid: c,
        verb: stream_verb,
    });
    list.push(Action::Set {
        nid: c,
        verb: set_format(stream.format),
    });
    Ok(list)
}
