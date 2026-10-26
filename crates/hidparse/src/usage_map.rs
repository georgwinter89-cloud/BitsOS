//! One usage to one Linux input code: the page switch of `hidinput_configure_usage` in
//! linux-6.6/drivers/hid/hid-input.c:687-1460, checked line by line against the source in IN-2a.
//! Pure: whether a non-"clear" code is a duplicate is decided in `map.rs`. Left out on purpose
//! (plan/notes/in-IN-2a.md): the Digitizer page (pens and touch screens, a tool state machine in
//! `hidinput_hid_event`), battery reporting (EV_PWR, a power-supply class in Linux) and the
//! per-driver `input_mapping` hooks of hid-generic's siblings.

use crate::codes::*;
use crate::descriptor::{Field, ReportKind, collection};
use crate::keyboard::HID_KEYBOARD;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Special {
    None,
    /// Hat switch: the value counts clockwise from up starting at `min`; reported as code, code + 1.
    Hat {
        min: i32,
        max: i32,
    },
    /// D-pad usage (Generic Desktop 0x90-0x93): a fixed hat direction 1-8.
    HatDir(u8),
    /// Relative wheel: `code` is the 1/120-notch axis, `low` the whole-notch axis.
    HiRes {
        low: u16,
    },
    /// A key whose release the device never sends: press, frame, release (Linux sets the field's
    /// RELATIVE flag for the RF-kill button, hid-input.c:907-910).
    Pulse,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Target {
    pub ev: u16,
    pub code: u16,
    pub special: Special,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Raw {
    Ignore,
    /// `clear`: Linux `map_*_clear`, the code is taken even if another usage has it already.
    Map {
        target: Target,
        clear: bool,
    },
}

pub(crate) const GD_JOYSTICK: u32 = 0x0001_0004;
pub(crate) const GD_GAMEPAD: u32 = 0x0001_0005;
const GD_MOUSE: u32 = 0x0001_0002;
const GD_POINTER: u32 = 0x0001_0001;
const GD_WIRELESS_RADIO_CTLS: u32 = 0x0001_000C;
const GD_SYSTEM_CONTROL: u32 = 0x0001_0080;
const CP_CONSUMER_CONTROL: u32 = 0x000C_0001;
const CP_PROGRAMMABLE_BUTTONS: u32 = 0x000C_0003;

/// Pages with their own case in Linux's switch, and the highest usage id handled one by one
/// (above it the page's default applies). Other pages take the "unknown" default.
pub(crate) const KNOWN_PAGES: &[(u32, u32)] = &[
    (0x01, 0xFF),
    (0x02, 0xFF),
    (0x06, 0xFF),
    (0x07, 0xFF),
    (0x08, 0xFF),
    (0x09, 0x200),
    (0x0B, 0xFF),
    (0x0C, 0x3FF),
    (0x0D, 0),
    (0x0F, 0xFF),
    (0x85, 0xFF),
    (0x90, 0xFF),
    (0xFF, 0),
    (0xFF00, 0),
    (0xFF01, 0xFF),
    (0xFF09, 0),
    (0xFF43, 0),
    (0xFF7F, 0xFF),
    (0xFFBC, 0),
];

/// Pages whose fields turn on key repeat (Linux sets EV_REP for them).
pub(crate) const REPEAT_PAGES: &[u32] = &[0x07, 0xFF, 0xFF01, 0xFF7F];

fn max_code(ev: u16) -> u16 {
    match ev {
        EV_KEY => KEY_MAX,
        EV_REL => REL_MAX,
        EV_ABS => ABS_MAX,
        EV_LED => LED_MAX,
        _ => 0,
    }
}

fn make(ev: u16, code: u32, clear: bool, special: Special) -> Raw {
    if code > max_code(ev) as u32 {
        return Raw::Ignore; // Linux hid_map_usage: "Invalid code"
    }
    Raw::Map {
        target: Target {
            ev,
            code: code as u16,
            special,
        },
        clear,
    }
}

fn key(code: u16) -> Raw {
    make(EV_KEY, code as u32, false, Special::None)
}
fn key_clear(code: u16) -> Raw {
    make(EV_KEY, code as u32, true, Special::None)
}
fn abs(code: u16) -> Raw {
    make(EV_ABS, code as u32, false, Special::None)
}
fn rel(code: u16) -> Raw {
    make(EV_REL, code as u32, false, Special::None)
}

/// Linux's `unknown:` label.
fn unknown(field: &Field) -> Raw {
    if field.size == 1 {
        if field.kind == ReportKind::Output {
            return make(EV_LED, LED_MISC as u32, false, Special::None);
        }
        return key(BTN_MISC);
    }
    if field.is_relative() {
        rel(REL_MISC)
    } else {
        abs(ABS_MISC)
    }
}

/// Where `usage` of `field` goes, before duplicates are looked at.
pub(crate) fn map_usage(field: &Field, usage: u32) -> Raw {
    if field.kind == ReportKind::Feature || field.is_constant() || field.count < 1 {
        return Raw::Ignore;
    }
    let page = usage >> 16;
    let id = usage & 0xFFFF;
    if field.kind == ReportKind::Output && page != 0x08 {
        return Raw::Ignore;
    }
    match page {
        0x00 => Raw::Ignore,
        0x01 => generic_desktop(field, id),
        0x02 => match id {
            0xBA => abs(ABS_RUDDER),
            0xBB => abs(ABS_THROTTLE),
            0xC4 => abs(ABS_GAS),
            0xC5 => abs(ABS_BRAKE),
            0xC8 => abs(ABS_WHEEL),
            _ => Raw::Ignore,
        },
        // Battery strength becomes a power-supply value in Linux, not an input event.
        0x06 if id == 0x20 => Raw::Ignore,
        0x07 => match HID_KEYBOARD.get(id as usize) {
            Some(0) => Raw::Ignore,
            Some(&code) => key_clear(code),
            None => key(KEY_UNKNOWN),
        },
        0x08 => led(id),
        0x09 => button(field, id),
        0x0B => match id {
            0x2F => key_clear(KEY_MICMUTE),
            0xB0..=0xB9 => key_clear(KEY_NUMERIC_0 + (id - 0xB0) as u16),
            0xBA => key_clear(KEY_NUMERIC_STAR),
            0xBB => key_clear(KEY_NUMERIC_POUND),
            0xBC => key_clear(KEY_NUMERIC_A),
            0xBD => key_clear(KEY_NUMERIC_B),
            0xBE => key_clear(KEY_NUMERIC_C),
            0xBF => key_clear(KEY_NUMERIC_D),
            _ => Raw::Ignore,
        },
        0x0C => consumer(id),
        0x0F if id == 0xA4 => key_clear(BTN_DEAD),
        // Battery: state of charge and charging become power-supply values in Linux (EV_PWR, no
        // input event); every other usage of the page takes the unknown default.
        0x85 if matches!(id, 0x44 | 0x65) => Raw::Ignore,
        0x90 => match id {
            0x20 => key_clear(KEY_CAMERA_FOCUS),
            0x21 => key_clear(KEY_CAMERA),
            _ => Raw::Ignore,
        },
        0xFF7F => match id {
            0x21 => key_clear(KEY_PRINT),
            0x70 => key_clear(KEY_HP),
            0x71 => key_clear(KEY_CAMERA),
            0x72 => key_clear(KEY_SOUND),
            0x73 => key_clear(KEY_QUESTION),
            0x80 => key_clear(KEY_EMAIL),
            0x81 => key_clear(KEY_CHAT),
            0x82 => key_clear(KEY_SEARCH),
            0x83 => key_clear(KEY_CONNECT),
            0x84 => key_clear(KEY_FINANCE),
            0x85 => key_clear(KEY_SPORT),
            0x86 => key_clear(KEY_SHOP),
            _ => Raw::Ignore,
        },
        0xFF01 => match id {
            0x01 => key_clear(KEY_MICMUTE),
            0x03 => key_clear(KEY_BRIGHTNESSDOWN),
            0x04 => key_clear(KEY_BRIGHTNESSUP),
            _ => Raw::Ignore,
        },
        0x0D | 0x0F | 0xFF | 0xFF00 | 0xFF09 | 0xFF43 | 0xFFBC => Raw::Ignore,
        _ => unknown(field),
    }
}

fn generic_desktop(field: &Field, id: u32) -> Raw {
    match id & 0xF0 {
        0x80 => {
            let code = [
                0,
                KEY_POWER,
                KEY_SLEEP,
                KEY_WAKEUP,
                KEY_CONTEXT_MENU,
                KEY_MENU,
                KEY_PROG1,
                KEY_HELP,
                KEY_EXIT,
                KEY_SELECT,
                KEY_RIGHT,
                KEY_LEFT,
                KEY_UP,
                KEY_DOWN,
                KEY_POWER2,
                KEY_RESTART,
            ][(id & 0xF) as usize];
            return if code == 0 {
                unknown(field)
            } else {
                key_clear(code)
            };
        }
        0xA0 => {
            return if id & 0xF == 0x9 {
                key_clear(KEY_MICMUTE)
            } else {
                Raw::Ignore
            };
        }
        0xB0 => {
            return if id & 0xF == 5 {
                key_clear(KEY_SWITCHVIDEOMODE)
            } else {
                Raw::Ignore
            };
        }
        _ => {}
    }
    // "Some lazy vendors declare 255 usages for System Control" (hid-input.c:850-857).
    if field.application == GD_SYSTEM_CONTROL {
        return Raw::Ignore;
    }
    if id & 0xF0 == 0x90 {
        // D-pad: the first takes ABS_HAT0X with its direction, the field's others are ignored
        // (field->dpad, hid-input.c:859-873; here as duplicates of the non-clear code).
        let dir = match id {
            0x90 => 1,
            0x91 => 5,
            0x92 => 3,
            0x93 => 7,
            _ => return unknown(field),
        };
        return make(EV_ABS, ABS_HAT0X as u32, false, Special::HatDir(dir));
    }
    match id {
        // X, Y, Z, Rx, Ry, Rz map straight to codes 0-5; absolute ones "clear", so a repeated X
        // (the 0079:0011 SNES clones declare it four times) keeps every copy, and the last one wins.
        0x30..=0x35 if field.is_relative() => rel((id & 0xF) as u16),
        0x30..=0x35 => make(EV_ABS, id & 0xF, true, Special::None),
        0x38 if field.is_relative() => make(
            EV_REL,
            REL_WHEEL_HI_RES as u32,
            false,
            Special::HiRes { low: REL_WHEEL },
        ),
        0x36..=0x38 if field.is_relative() => rel((id & 0xF) as u16),
        0x36..=0x38 => abs((id & 0xF) as u16),
        0x39 => make(
            EV_ABS,
            ABS_HAT0X as u32,
            false,
            Special::Hat {
                min: field.logical_min,
                max: field.logical_max,
            },
        ),
        0x3D => key_clear(BTN_START),
        0x3E => key_clear(BTN_SELECT),
        0xC6 if field.application == GD_WIRELESS_RADIO_CTLS => {
            make(EV_KEY, KEY_RFKILL as u32, true, Special::Pulse)
        }
        _ => unknown(field),
    }
}

fn led(id: u32) -> Raw {
    let code = match id {
        0x01 => LED_NUML,
        0x02 => LED_CAPSL,
        0x03 => LED_SCROLLL,
        0x04 => LED_COMPOSE,
        0x05 => LED_KANA,
        0x09 => LED_MUTE,
        0x19 => LED_MAIL,
        0x27 => LED_SLEEP,
        0x4B => LED_MISC,
        0x4C => LED_SUSPEND,
        0x4D => LED_CHARGING,
        _ => return Raw::Ignore,
    };
    make(EV_LED, code as u32, false, Special::None)
}

fn button(field: &Field, id: u32) -> Raw {
    // Button 0 wraps to 0xFFFF here and lands past KEY_MAX: ignored, as in Linux.
    let n = id.wrapping_sub(1) & 0xFFFF;
    let base = |physical: u32| match physical {
        GD_MOUSE | GD_POINTER => BTN_MOUSE,
        GD_JOYSTICK => BTN_JOYSTICK,
        GD_GAMEPAD => BTN_GAMEPAD,
        _ => BTN_MISC,
    } as u32;
    let programmable = field.innermost == Some((collection::NAMED_ARRAY, CP_PROGRAMMABLE_BUTTONS));
    let code = match field.application {
        GD_MOUSE | GD_POINTER => BTN_MOUSE as u32 + n,
        GD_JOYSTICK | GD_GAMEPAD if n > 0xF => BTN_TRIGGER_HAPPY as u32 - 0x10 + n,
        GD_JOYSTICK => BTN_JOYSTICK as u32 + n,
        GD_GAMEPAD => BTN_GAMEPAD as u32 + n,
        CP_CONSUMER_CONTROL if programmable && n <= 0x1D => KEY_MACRO1 as u32 + n,
        CP_CONSUMER_CONTROL if programmable => BTN_TRIGGER_HAPPY as u32 - 0x1E + n,
        _ => base(field.physical) + n,
    };
    make(EV_KEY, code, false, Special::None)
}

/// The Consumer page, hid-input.c:1069-1262 entry for entry.
fn consumer(id: u32) -> Raw {
    let code = match id {
        0x000 => return Raw::Ignore,
        0x030 => KEY_POWER,
        0x031 => KEY_RESTART,
        0x032 | 0x034 => KEY_SLEEP,
        0x035 => KEY_KBDILLUMTOGGLE,
        0x036 => BTN_MISC,
        0x040 => KEY_MENU,
        0x041 => KEY_SELECT,
        0x042 => KEY_UP,
        0x043 => KEY_DOWN,
        0x044 => KEY_LEFT,
        0x045 => KEY_RIGHT,
        0x046 => KEY_ESC,
        0x047 => KEY_KPPLUS,
        0x048 => KEY_KPMINUS,
        0x060 => KEY_INFO,
        0x061 => KEY_SUBTITLE,
        0x063 => KEY_VCR,
        0x065 => KEY_CAMERA,
        0x069 => KEY_RED,
        0x06A => KEY_GREEN,
        0x06B => KEY_BLUE,
        0x06C => KEY_YELLOW,
        0x06D => KEY_ASPECT_RATIO,
        0x06F => KEY_BRIGHTNESSUP,
        0x070 => KEY_BRIGHTNESSDOWN,
        0x072 => KEY_BRIGHTNESS_TOGGLE,
        0x073 => KEY_BRIGHTNESS_MIN,
        0x074 => KEY_BRIGHTNESS_MAX,
        0x075 => KEY_BRIGHTNESS_AUTO,
        0x076 => KEY_CAMERA_ACCESS_ENABLE,
        0x077 => KEY_CAMERA_ACCESS_DISABLE,
        0x078 => KEY_CAMERA_ACCESS_TOGGLE,
        0x079 => KEY_KBDILLUMUP,
        0x07A => KEY_KBDILLUMDOWN,
        0x07C => KEY_KBDILLUMTOGGLE,
        0x082 => KEY_VIDEO_NEXT,
        0x083 => KEY_LAST,
        0x084 => KEY_ENTER,
        0x088 => KEY_PC,
        0x089 => KEY_TV,
        0x08A => KEY_WWW,
        0x08B => KEY_DVD,
        0x08C => KEY_PHONE,
        0x08D => KEY_PROGRAM,
        0x08E => KEY_VIDEOPHONE,
        0x08F => KEY_GAMES,
        0x090 => KEY_MEMO,
        0x091 => KEY_CD,
        0x092 => KEY_VCR,
        0x093 => KEY_TUNER,
        0x094 => KEY_EXIT,
        0x095 => KEY_HELP,
        0x096 => KEY_TAPE,
        0x097 => KEY_TV2,
        0x098 => KEY_SAT,
        0x09A => KEY_PVR,
        0x09C => KEY_CHANNELUP,
        0x09D => KEY_CHANNELDOWN,
        0x0A0 => KEY_VCR2,
        0x0B0 => KEY_PLAY,
        0x0B1 => KEY_PAUSE,
        0x0B2 => KEY_RECORD,
        0x0B3 => KEY_FASTFORWARD,
        0x0B4 => KEY_REWIND,
        0x0B5 => KEY_NEXTSONG,
        0x0B6 => KEY_PREVIOUSSONG,
        0x0B7 => KEY_STOPCD,
        0x0B8 => KEY_EJECTCD,
        0x0BC => KEY_MEDIA_REPEAT,
        0x0B9 => KEY_SHUFFLE,
        0x0BF => KEY_SLOW,
        0x0CD => KEY_PLAYPAUSE,
        0x0CF => KEY_VOICECOMMAND,
        0x0D8 => KEY_DICTATE,
        0x0D9 => KEY_EMOJI_PICKER,
        // A volume knob; a relative one becomes VOLUMEUP/VOLUMEDOWN presses (`input.rs`).
        0x0E0 => return make(EV_ABS, ABS_VOLUME as u32, true, Special::None),
        0x0E2 => KEY_MUTE,
        0x0E5 => KEY_BASSBOOST,
        0x0E9 => KEY_VOLUMEUP,
        0x0EA => KEY_VOLUMEDOWN,
        0x0F5 => KEY_SLOW,
        0x181 => KEY_BUTTONCONFIG,
        0x182 => KEY_BOOKMARKS,
        0x183 => KEY_CONFIG,
        0x184 => KEY_WORDPROCESSOR,
        0x185 => KEY_EDITOR,
        0x186 => KEY_SPREADSHEET,
        0x187 => KEY_GRAPHICSEDITOR,
        0x188 => KEY_PRESENTATION,
        0x189 => KEY_DATABASE,
        0x18A => KEY_MAIL,
        0x18B => KEY_NEWS,
        0x18C => KEY_VOICEMAIL,
        0x18D => KEY_ADDRESSBOOK,
        0x18E => KEY_CALENDAR,
        0x18F => KEY_TASKMANAGER,
        0x190 => KEY_JOURNAL,
        0x191 => KEY_FINANCE,
        0x192 => KEY_CALC,
        0x193 => KEY_PLAYER,
        0x194 => KEY_FILE,
        0x196 => KEY_WWW,
        0x199 => KEY_CHAT,
        0x19C => KEY_LOGOFF,
        0x19E => KEY_COFFEE,
        0x19F => KEY_CONTROLPANEL,
        0x1A2 => KEY_APPSELECT,
        0x1A3 => KEY_NEXT,
        0x1A4 => KEY_PREVIOUS,
        0x1A6 => KEY_HELP,
        0x1A7 => KEY_DOCUMENTS,
        0x1AB => KEY_SPELLCHECK,
        0x1AE => KEY_KEYBOARD,
        0x1B1 => KEY_SCREENSAVER,
        0x1B4 => KEY_FILE,
        0x1B6 => KEY_IMAGES,
        0x1B7 => KEY_AUDIO,
        0x1B8 => KEY_VIDEO,
        0x1BC => KEY_MESSENGER,
        0x1BD => KEY_INFO,
        0x1CB => KEY_ASSISTANT,
        0x201 => KEY_NEW,
        0x202 => KEY_OPEN,
        0x203 => KEY_CLOSE,
        0x204 => KEY_EXIT,
        0x207 => KEY_SAVE,
        0x208 => KEY_PRINT,
        0x209 => KEY_PROPS,
        0x21A => KEY_UNDO,
        0x21B => KEY_COPY,
        0x21C => KEY_CUT,
        0x21D => KEY_PASTE,
        0x21F => KEY_FIND,
        0x221 => KEY_SEARCH,
        0x222 => KEY_GOTO,
        0x223 => KEY_HOMEPAGE,
        0x224 => KEY_BACK,
        0x225 => KEY_FORWARD,
        0x226 => KEY_STOP,
        0x227 => KEY_REFRESH,
        0x22A => KEY_BOOKMARKS,
        0x22D => KEY_ZOOMIN,
        0x22E => KEY_ZOOMOUT,
        0x22F => KEY_ZOOMRESET,
        0x232 => KEY_FULL_SCREEN,
        0x233 => KEY_SCROLLUP,
        0x234 => KEY_SCROLLDOWN,
        // AC Pan: the horizontal wheel.
        0x238 => {
            return make(
                EV_REL,
                REL_HWHEEL_HI_RES as u32,
                false,
                Special::HiRes { low: REL_HWHEEL },
            );
        }
        0x23D => KEY_EDIT,
        0x25F => KEY_CANCEL,
        0x269 => KEY_INSERT,
        0x26A => KEY_DELETE,
        0x279 => KEY_REDO,
        0x289 => KEY_REPLY,
        0x28B => KEY_FORWARDMAIL,
        0x28C => KEY_SEND,
        0x29D => KEY_KBD_LAYOUT_NEXT,
        0x2A2 => KEY_ALL_APPLICATIONS,
        0x2C7 => KEY_KBDINPUTASSIST_PREV,
        0x2C8 => KEY_KBDINPUTASSIST_NEXT,
        0x2C9 => KEY_KBDINPUTASSIST_PREVGROUP,
        0x2CA => KEY_KBDINPUTASSIST_NEXTGROUP,
        0x2CB => KEY_KBDINPUTASSIST_ACCEPT,
        0x2CC => KEY_KBDINPUTASSIST_CANCEL,
        0x29F => KEY_SCALE,
        _ => KEY_UNKNOWN,
    };
    key_clear(code)
}
