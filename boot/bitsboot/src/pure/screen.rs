//! The screen mode bitsboot sets before the kernel (V2-PLAN §7 "Picture"): the native 2560x1440 by default,
//! `screen=` in CONFIG.TXT overrides it. A wanted mode the firmware does not offer keeps the firmware's mode.

use crate::pixel::FB_LARGE;
use alloc::format;
use alloc::string::String;
use bootinfo::{Mode, PixelFormat};

/// The Mini-PC's monitor (iiyama via HDMI, HW-MINIPC round 1).
pub const NATIVE: (u32, u32) = (2560, 1440);
pub const KEY: &str = "screen";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Want {
    /// Exactly this size (default: [`NATIVE`]).
    Size(u32, u32),
    /// `screen=largest`: the largest mode (V1's `screen=native` meaning).
    Largest,
    /// `screen=firmware`: leave the firmware's mode alone.
    Keep,
}

/// The value of `screen=` (`None` = no line). An unreadable value keeps the firmware's mode (a switch that
/// does not parse means "off", spec core.md 2.6) and says why.
pub fn parse(value: Option<&str>) -> (Want, Option<String>) {
    let Some(v) = value else { return (Want::Size(NATIVE.0, NATIVE.1), None) };
    let v = v.trim();
    if v.eq_ignore_ascii_case("native") {
        return (Want::Size(NATIVE.0, NATIVE.1), None);
    }
    if v.eq_ignore_ascii_case("largest") {
        return (Want::Largest, None);
    }
    if v.eq_ignore_ascii_case("firmware") || v.eq_ignore_ascii_case("keep") {
        return (Want::Keep, None);
    }
    let size = v.split_once(['x', 'X']).and_then(|(w, h)| {
        let num = |s: &str| (!s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())).then(|| s.parse::<u32>().ok()).flatten();
        Some((num(w)?, num(h)?))
    });
    match size {
        Some((w, h)) if w > 0 && h > 0 => (Want::Size(w, h), None),
        _ => (Want::Keep, Some(format!("screen={v} is not WxH, native, largest or firmware: keeping the firmware's mode"))),
    }
}

/// A mode the kernel can draw into whose framebuffer stays within the 64 MiB rule (spec core.md 2.1).
pub fn usable(m: &Mode) -> bool {
    m.format != PixelFormat::None
        && m.width > 0
        && m.height > 0
        && m.stride >= m.width
        && (m.stride as u64) * (m.height as u64) * 4 <= FB_LARGE
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Pick {
    /// Stay in the current mode.
    Keep(String),
    /// SetMode to this index.
    Switch(u32),
    /// The wanted mode is not offered: keep the firmware's mode and say so (an event in the loader log).
    Missing(String),
}

/// Ranks equal-size modes: the current one first, then the two fast pixel layouts, then the lowest index.
fn rank(m: &Mode, current: u32) -> (bool, bool, core::cmp::Reverse<u32>) {
    (m.index == current, m.format != PixelFormat::Bitmask, core::cmp::Reverse(m.index))
}

pub fn pick(modes: &[Mode], current: u32, want: Want) -> Pick {
    let best = match want {
        Want::Keep => return Pick::Keep(String::from("screen=firmware: the firmware's mode stays")),
        Want::Size(w, h) => modes
            .iter()
            .filter(|m| usable(m) && m.width == w && m.height == h)
            .max_by_key(|m| rank(m, current)),
        Want::Largest => modes
            .iter()
            .filter(|m| usable(m))
            .max_by_key(|m| ((m.width as u64) * (m.height as u64), rank(m, current))),
    };
    match (best, want) {
        (Some(m), _) if m.index == current => Pick::Keep(format!("already in {}x{} (mode {})", m.width, m.height, m.index)),
        (Some(m), _) => Pick::Switch(m.index),
        (None, Want::Size(w, h)) => {
            Pick::Missing(format!("the firmware offers no usable {w}x{h} mode: keeping the firmware's mode"))
        }
        (None, _) => Pick::Missing(String::from("the firmware offers no usable mode: keeping the firmware's mode")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::vec::Vec;

    fn m(index: u32, width: u32, height: u32, format: PixelFormat) -> Mode {
        Mode { index, width, height, format, stride: width }
    }

    fn ovmf() -> Vec<Mode> {
        let b = PixelFormat::Bgrx;
        [(640, 480), (800, 600), (1024, 768), (1280, 800), (1920, 1080), (2560, 1440), (2560, 1600)]
            .iter()
            .enumerate()
            .map(|(i, &(w, h))| m(i as u32, w, h, b))
            .collect()
    }

    #[test]
    fn config_values() {
        assert_eq!(parse(None), (Want::Size(2560, 1440), None));
        assert_eq!(parse(Some("native")).0, Want::Size(2560, 1440));
        assert_eq!(parse(Some("1024x768")).0, Want::Size(1024, 768));
        assert_eq!(parse(Some(" 1920X1080 ")).0, Want::Size(1920, 1080));
        assert_eq!(parse(Some("largest")).0, Want::Largest);
        assert_eq!(parse(Some("Firmware")).0, Want::Keep);
        for bad in ["", "1024", "x768", "0x768", "1024x", "+1024x768", "1024x768x2", "99999999999x1", "big"] {
            let (w, why) = parse(Some(bad));
            assert_eq!(w, Want::Keep, "{bad}");
            assert!(why.is_some(), "{bad}");
        }
    }

    #[test]
    fn native_is_chosen() {
        assert_eq!(pick(&ovmf(), 3, Want::Size(2560, 1440)), Pick::Switch(5));
        assert!(matches!(pick(&ovmf(), 5, Want::Size(2560, 1440)), Pick::Keep(_)));
        assert_eq!(pick(&ovmf(), 3, Want::Size(1024, 768)), Pick::Switch(2));
        assert!(matches!(pick(&ovmf(), 3, Want::Keep), Pick::Keep(_)));
    }

    #[test]
    fn missing_mode_keeps_the_firmware_mode() {
        let modes = &ovmf()[..5];
        match pick(modes, 3, Want::Size(2560, 1440)) {
            Pick::Missing(why) => assert!(why.contains("2560x1440"), "{why}"),
            p => panic!("{p:?}"),
        }
        assert!(matches!(pick(&[], 0, Want::Largest), Pick::Missing(_)));
    }

    #[test]
    fn unusable_modes_are_never_chosen() {
        let modes = [
            m(0, 1280, 800, PixelFormat::Bgrx),
            m(1, 2560, 1440, PixelFormat::None),
            Mode { index: 2, width: 2560, height: 1440, format: PixelFormat::Bgrx, stride: 2000 },
            m(3, 8192, 4096, PixelFormat::Bgrx), // 128 MiB
        ];
        assert!(matches!(pick(&modes, 0, Want::Size(2560, 1440)), Pick::Missing(_)));
        assert!(matches!(pick(&modes, 0, Want::Size(8192, 4096)), Pick::Missing(_)));
        assert!(matches!(pick(&modes, 0, Want::Largest), Pick::Keep(_)));
    }

    #[test]
    fn equal_sizes_prefer_current_then_fast_formats() {
        let modes = [
            m(0, 1280, 800, PixelFormat::Bgrx),
            m(1, 1920, 1080, PixelFormat::Bitmask),
            m(2, 1920, 1080, PixelFormat::Rgbx),
            m(3, 1920, 1080, PixelFormat::Bgrx),
        ];
        assert_eq!(pick(&modes, 0, Want::Size(1920, 1080)), Pick::Switch(2));
        assert!(matches!(pick(&modes, 3, Want::Size(1920, 1080)), Pick::Keep(_)));
        assert_eq!(pick(&modes, 0, Want::Largest), Pick::Switch(2));
        assert_eq!(pick(&ovmf(), 0, Want::Largest), Pick::Switch(6));
    }
}
