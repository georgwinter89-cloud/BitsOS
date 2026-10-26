//! Verb encoding. A verb here is the low 20 bits of a command word: either a 12-bit verb ID with
//! an 8-bit payload (`0x7xx`/`0xFxx`) or a 4-bit verb ID with a 16-bit payload (format, amp).
//! Names follow Linux `include/sound/hda_verbs.h` (`AC_*`) and `patch_hdmi.c` (`ATI_*`).

use crate::{CAD_LAST, NID_LAST, Nid};

// Parameter IDs for verb F00.
pub const PAR_VENDOR_ID: u8 = 0x00;
pub const PAR_REVISION_ID: u8 = 0x02;
pub const PAR_NODE_COUNT: u8 = 0x04;
pub const PAR_FG_TYPE: u8 = 0x05;
pub const PAR_AUDIO_WIDGET_CAP: u8 = 0x09;
pub const PAR_PCM: u8 = 0x0A;
pub const PAR_STREAM: u8 = 0x0B;
pub const PAR_PIN_CAP: u8 = 0x0C;
pub const PAR_AMP_IN_CAP: u8 = 0x0D;
pub const PAR_CONNLIST_LEN: u8 = 0x0E;
pub const PAR_POWER_STATE: u8 = 0x0F;
pub const PAR_GPIO_CAP: u8 = 0x11;
pub const PAR_AMP_OUT_CAP: u8 = 0x12;

/// Get parameter `p` (verb F00).
pub const fn get_param(p: u8) -> u32 {
    0xF0000 | p as u32
}

/// Get the connection list entries starting at `index` (verb F02).
pub const fn get_conn_list(index: u8) -> u32 {
    0xF0200 | index as u32
}

pub const GET_CONNECT_SEL: u32 = 0xF0100;
pub const GET_POWER_STATE: u32 = 0xF0500;
pub const GET_CONV: u32 = 0xF0600;
pub const GET_PIN_WIDGET_CONTROL: u32 = 0xF0700;
pub const GET_UNSOLICITED_RESPONSE: u32 = 0xF0800;
pub const GET_PIN_SENSE: u32 = 0xF0900;
pub const GET_EAPD_BTLENABLE: u32 = 0xF0C00;
pub const GET_CONFIG_DEFAULT: u32 = 0xF1C00;
pub const GET_SUBSYSTEM_ID: u32 = 0xF2000;
pub const GET_STREAM_FORMAT: u32 = 0xA0000;
pub const ATI_GET_HBR_CONTROL: u32 = 0xF7C00;

/// Get amplifier gain/mute (4-bit verb B): output or input amp, left or right, input `index`.
pub const fn get_amp(output: bool, left: bool, index: u8) -> u32 {
    0xB0000 | if output { 0x8000 } else { 0 } | if left { 0x2000 } else { 0 } | (index as u32 & 0xF)
}

// 12-bit "set" verb IDs (payload 8 bits).
pub const SET_CONNECT_SEL: u16 = 0x701;
pub const SET_POWER_STATE: u16 = 0x705;
pub const SET_CHANNEL_STREAMID: u16 = 0x706;
pub const SET_PIN_WIDGET_CONTROL: u16 = 0x707;
pub const SET_UNSOLICITED_ENABLE: u16 = 0x708;
pub const SET_PIN_SENSE: u16 = 0x709;
pub const SET_EAPD_BTLENABLE: u16 = 0x70C;
pub const SET_DIGI_CONVERT_1: u16 = 0x70D;
pub const SET_CVT_CHAN_COUNT: u16 = 0x72D;
pub const ATI_SET_RAMP_RATE: u16 = 0x770;
pub const ATI_SET_CHANNEL_ALLOCATION: u16 = 0x771;
pub const ATI_SET_DOWNMIX_INFO: u16 = 0x772;
pub const ATI_SET_MULTICHANNEL_01: u16 = 0x777;
pub const ATI_SET_HBR_CONTROL: u16 = 0x77C;
pub const ATI_SET_MULTICHANNEL_1: u16 = 0x785;
pub const ATI_SET_MULTICHANNEL_MODE: u16 = 0x789;

// Payload bits.
pub const PWRST_D0: u8 = 0x00;
pub const PINCTL_OUT_EN: u8 = 0x40;
pub const PINCTL_HP_EN: u8 = 0x80;
pub const EAPD_ENABLE: u8 = 0x02;
pub const DIG1_ENABLE: u8 = 0x01;
pub const USRSP_EN: u8 = 0x80;
pub const ATI_HBR_ENABLE: u8 = 0x10;
pub const ATI_MULTICHANNEL_ENABLE: u8 = 0x01;

/// A 12-bit verb with its 8-bit payload.
pub const fn set(verb: u16, payload: u8) -> u32 {
    ((verb as u32 & 0xFFF) << 8) | payload as u32
}

/// Set amplifier gain/mute (4-bit verb 3) for the output amp, both sides, unmuted.
pub const fn set_out_amp(gain: u8) -> u32 {
    0x3B000 | (gain as u32 & 0x7F)
}

/// Set amplifier gain/mute for input `index` of the input amp, both sides, unmuted.
pub const fn set_in_amp(index: u8, gain: u8) -> u32 {
    0x37000 | ((index as u32 & 0xF) << 8) | (gain as u32 & 0x7F)
}

/// Set converter format (4-bit verb 2).
pub const fn set_format(format: u16) -> u32 {
    0x20000 | format as u32
}

/// True for verbs that only read: 12-bit verbs `Fxx` and the 4-bit gets A (format),
/// B (amp), C (processing coefficient), D (coefficient index).
pub const fn is_read_only(verb: u32) -> bool {
    verb <= 0xFFFFF && matches!(verb >> 16, 0xA | 0xB | 0xC | 0xD | 0xF)
}

/// The 32-bit command word for codec `cad`, node `nid`; `None` if either is out of range or
/// the verb has more than 20 bits.
pub const fn command(cad: u8, nid: Nid, verb: u32) -> Option<u32> {
    if cad > CAD_LAST || nid > NID_LAST || verb > 0xFFFFF {
        return None;
    }
    Some(((cad as u32) << 28) | ((nid as u32) << 20) | verb)
}
