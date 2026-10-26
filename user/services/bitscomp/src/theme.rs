//! The token table (WY-5): every visual value bitscomp uses. Built from the `theme.*` keys of the settings registry
//! (`settings`, user/services/settingsd/src/builtin.rs) on top of the defaults, which are the values of the mock-up
//! Georg approved on 21.10.2026 (plan/spec/desktop.md §6, plan/spec/desktop-mockup/). Sizes are CSS px of the
//! mock-up at 100 % = logical pixels; the output scale (`theme.scale`, 125 %) makes them physical.

use crate::settings::Values;

pub type Rgba = [f32; 4];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Motion {
    Medium,
    Calm,
    Off,
}

/// What `theme.layout.*` switches on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Layout {
    /// `top`: one address bar at the top; `tile`: one per tile above its tabs, the top bar becomes a search field.
    pub address_top: bool,
    pub favourites: bool,
    /// WY-5c: the workspace list docked on the left (false: folded away).
    pub workspaces: bool,
    /// Claude's sidebar docked on the right (false: folded away, the ring brings it back).
    pub sidebar: bool,
    pub statusbar: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Theme {
    pub dark: bool,
    /// Colours (premultiplied, what pixman blends with). Names as in the mock-up.
    pub bg: Rgba,
    pub panel: Rgba,
    pub panel2: Rgba,
    /// A shown tab: it runs into the content below it.
    pub paper: Rgba,
    pub line: Rgba,
    pub text: Rgba,
    pub muted: Rgba,
    pub accent: Rgba,
    /// The accent at glow strength (14-18 %) and half of it: the cheap level's glow lines.
    pub glow: Rgba,
    pub glow_soft: Rgba,
    /// The accent behind selected things (workspace, suggestion, hovered menu item).
    pub chip: Rgba,
    pub danger: Rgba,
    pub danger_chip: Rgba,
    /// The ring's "waiting for Georg" colour.
    pub warm: Rgba,
    pub veil: Rgba,
    /// Effect level `gpu`: wider glows (still drawn by the CPU until the Radeon driver).
    pub effects_gpu: bool,
    pub motion: Motion,
    pub layout: Layout,
    /// `theme.font.ui`; only Exo 2 is shipped, another name is noted once and Exo 2 drawn.
    pub font_ui: String,
    pub font_mono: String,
    /// Factor on every text size of the mock-up (theme.font.size / 15).
    pub text_k: f32,

    /// Chrome (logical pixels).
    pub top_h: i32,
    pub fav_h: i32,
    pub list_w: i32,
    pub side_w: i32,
    pub status_h: i32,
    /// Room around and between the tiles.
    pub gap: i32,
    pub border: i32,
    /// Tile corner radius and the small radius (tabs, buttons).
    pub radius: i32,
    pub radius_small: i32,
    /// The address bar of a tile (`layout.address_bar = tile`), 0 otherwise.
    pub tile_bar: i32,
    /// The tab row (tabs sit at its bottom) and the whole header: tile bar + tab row.
    pub tab_row: i32,
    pub header_height: i32,
    pub tab_max: i32,
    pub tab_min: i32,
    pub tab_gap: i32,
    /// Distance from the header's top to the tabs; tabs not shown stop `tab_lift` above its bottom.
    pub tab_top: i32,
    pub tab_lift: i32,
    /// Strip padding before the first tab, and inside a tab before its dot.
    pub tab_pad: i32,
    pub tab_inner: i32,
    pub tab_dot: i32,
    pub tab_close: i32,
    pub more_width: i32,
    /// Header buttons (float, close), square.
    pub button_size: i32,
    pub resize_margin: i32,
    /// A header press becomes a drag only after this much movement, so a click never moves a window.
    pub drag_threshold: f64,
    /// Share of a tile's width at each side that means "beside it" when a header is dropped there.
    pub edge_zone: f32,
    pub min_tile_width: i32,
    pub min_float: (i32, i32),
    pub menu_radius: i32,
    pub menu_item_size: (i32, i32),
    /// Inside this radius the circle menu picks nothing, so opening and releasing in place keeps it open.
    pub menu_dead: i32,
    pub hung_box_size: (i32, i32),
    pub kill_button_size: (i32, i32),
    /// Output scale (plan/spec/desktop.md §2: 125 % on the Mini-PC's 2560x1440).
    pub scale: f64,
    /// WY-11 theme.cursor.size: the pointer's size in logical px (32: 40 px at 125 %, Adwaita's 48 px image).
    pub cursor_size: u32,
}

/// `#rrggbb` as an opaque colour.
pub fn hex(s: &str) -> Option<Rgba> {
    let h = s.strip_prefix('#')?;
    if h.len() != 6 || !h.is_ascii() {
        return None;
    }
    let c = |i: usize| u8::from_str_radix(&h[i..i + 2], 16).ok().map(|v| v as f32 / 255.0);
    Some([c(0)?, c(2)?, c(4)?, 1.0])
}

/// `c` at alpha `a`, premultiplied.
pub fn alpha(c: Rgba, a: f32) -> Rgba {
    [c[0] * a, c[1] * a, c[2] * a, c[3] * a]
}

pub struct Palette {
    pub bg: &'static str,
    pub panel: &'static str,
    pub panel2: &'static str,
    pub paper: &'static str,
    pub line: &'static str,
    pub text: &'static str,
    pub muted: &'static str,
}

/// The mock-up's values (Main.dc.html `renderVals`); the settings keys theme.{dark,light}.* default to them.
pub const DARK: Palette = Palette {
    bg: "#0b0f14",
    panel: "#111821",
    panel2: "#17212c",
    paper: "#131c26",
    line: "#26364a",
    text: "#e6edf3",
    muted: "#93a4b8",
};
pub const LIGHT: Palette = Palette {
    bg: "#e9eef3",
    panel: "#ffffff",
    panel2: "#f2f5f8",
    paper: "#ffffff",
    line: "#d3dce6",
    text: "#101820",
    muted: "#4d5b6b",
};
pub const ACCENT: &str = "#00e5ff";
const DANGER: &str = "#ff6b6b";
const WARM: &str = "#ffb020";

/// The default theme for unit tests (one per process).
#[cfg(test)]
pub fn test_theme() -> &'static Theme {
    static T: std::sync::LazyLock<Theme> = std::sync::LazyLock::new(Theme::default);
    &T
}

impl Default for Theme {
    fn default() -> Self {
        Theme::build(&Values::default())
    }
}

impl Theme {
    /// The theme of the settings `v`; every missing or malformed key takes its default.
    pub fn build(v: &Values) -> Theme {
        let text = |k: &str, d: &str| v.text(k).unwrap_or(d).to_string();
        let int = |k: &str, d: i64, lo: i64, hi: i64| v.int(k).unwrap_or(d).clamp(lo, hi);
        let flag = |k: &str, d: bool| v.flag(k).unwrap_or(d);
        let dark = text("theme.mode", "dark") != "light";
        let (pal, mode) = if dark { (&DARK, "dark") } else { (&LIGHT, "light") };
        let col = |name: &str, d: &str| {
            v.text(&format!("theme.{mode}.{name}")).and_then(hex).unwrap_or_else(|| hex(d).unwrap())
        };
        let accent = v.text("theme.accent").and_then(hex).unwrap_or_else(|| hex(ACCENT).unwrap());
        // theme.glow 60 % (the default) is the mock-up's glow: 18 % alpha dark, 14 % light.
        let glow_k = int("theme.glow", 60, 0, 100) as f32 / 60.0;
        let glow_a = (if dark { 0.18 } else { 0.14 } * glow_k).min(0.4);
        let chip_a = if dark { 0.14 } else { 0.12 };
        let bg = col("bg", pal.bg);
        let spacing = int("theme.spacing", 8, 0, 64) as f32 / 8.0;
        let sp = |px: i32| (px as f32 * spacing).round() as i32;
        let radius = int("theme.radius", 8, 0, 32) as f32 / 8.0;
        let rad = |px: i32| (px as f32 * radius).round() as i32;
        let layout = Layout {
            address_top: text("theme.layout.address_bar", "top") != "tile",
            favourites: flag("theme.layout.favourites_bar", true),
            workspaces: flag("theme.layout.workspaces_bar", true),
            sidebar: flag("theme.layout.claude_sidebar", true),
            statusbar: flag("theme.layout.statusbar", true),
        };
        let tile_bar = if layout.address_top { 0 } else { 34 };
        let tab_row = 38;
        let danger = hex(DANGER).unwrap();
        Theme {
            dark,
            bg,
            panel: col("surface", pal.panel),
            panel2: col("surface2", pal.panel2),
            paper: col("paper", pal.paper),
            line: col("border", pal.line),
            text: col("text", pal.text),
            muted: col("text_dim", pal.muted),
            accent,
            glow: alpha(accent, glow_a),
            glow_soft: alpha(accent, glow_a / 2.0),
            chip: alpha(accent, chip_a),
            danger,
            danger_chip: alpha(danger, 0.14),
            warm: hex(WARM).unwrap(),
            veil: alpha(bg, 0.8),
            effects_gpu: text("theme.effects", "cheap") == "gpu",
            motion: match text("theme.motion", "medium").as_str() {
                "off" => Motion::Off,
                "calm" => Motion::Calm,
                _ => Motion::Medium,
            },
            layout,
            font_ui: text("theme.font.ui", "Exo 2"),
            font_mono: text("theme.font.mono", "Noto Sans Mono"),
            text_k: int("theme.font.size", 15, 8, 48) as f32 / 15.0,
            top_h: 56,
            fav_h: 34,
            list_w: 212,
            side_w: 300,
            status_h: 32,
            gap: sp(12),
            border: 1,
            radius: rad(12),
            radius_small: rad(8),
            tile_bar,
            tab_row,
            header_height: tile_bar + tab_row,
            tab_max: 190,
            tab_min: 96,
            tab_gap: 2,
            tab_top: tile_bar + 8,
            tab_lift: 0,
            tab_pad: 8,
            tab_inner: 12,
            tab_dot: 8,
            tab_close: 12,
            more_width: 36,
            button_size: 28,
            resize_margin: 6,
            drag_threshold: 6.0,
            edge_zone: 0.25,
            min_tile_width: 120,
            min_float: (160, 100),
            menu_radius: 120,
            menu_item_size: (160, 28),
            menu_dead: 24,
            hung_box_size: (360, 190),
            kill_button_size: (120, 44),
            scale: int("theme.scale", 125, 100, 300) as f64 / 100.0,
            cursor_size: int("theme.cursor.size", 32, 16, 128) as u32,
        }
    }

    /// A mock-up text size (CSS px) after theme.font.size.
    pub fn fs(&self, px: f32) -> f32 {
        px * self.text_k
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_the_mockup_values() {
        let t = Theme::default();
        assert!(t.dark);
        assert_eq!(t.bg, hex("#0b0f14").unwrap());
        assert_eq!(t.accent, hex("#00e5ff").unwrap());
        assert!((t.glow[3] - 0.18).abs() < 1e-6);
        assert_eq!(t.scale, 1.25);
        assert_eq!((t.gap, t.radius, t.header_height), (12, 12, 38));
    }

    #[test]
    fn settings_change_mode_accent_and_layout() {
        let mut v = Values::default();
        v.set("theme.mode", "light");
        v.set("theme.accent", "#ffb020");
        v.set("theme.layout.address_bar", "tile");
        v.set("theme.scale", "100");
        let t = Theme::build(&v);
        assert!(!t.dark);
        assert_eq!(t.bg, hex(LIGHT.bg).unwrap());
        assert!((t.glow[3] - 0.14).abs() < 1e-6);
        assert_eq!(t.accent, hex("#ffb020").unwrap());
        assert_eq!(t.header_height, 72);
        assert_eq!(t.scale, 1.0);
        v.set("theme.accent", "cyan");
        assert_eq!(Theme::build(&v).accent, hex(ACCENT).unwrap(), "a malformed value falls back to the default");
    }

    #[test]
    fn garbage_values_take_the_default_or_are_clamped() {
        let d = Theme::default();
        let mut v = Values::default();
        for (k, x) in [
            ("theme.accent", "zzz"),
            ("theme.mode", "foo"),
            ("theme.scale", "0"),
            ("theme.font.size", "-5"),
            ("theme.font.ui", ""),
            ("theme.spacing", "99999999999999999999"),
            ("theme.radius", "-1"),
            ("theme.glow", "1000"),
            ("theme.motion", "wild"),
            ("theme.dark.bg", "#12345"),
            ("theme.dark.text", "#ggg000"),
            ("theme.dark.border", "#ü1234"),
            ("theme.layout.address_bar", "sideways"),
            ("theme.layout.statusbar", "maybe"),
        ] {
            v.set(k, x);
        }
        let t = Theme::build(&v);
        assert!(t.dark, "an unknown mode is dark (the default)");
        assert_eq!((t.accent, t.bg, t.text, t.line), (d.accent, d.bg, d.text, d.line));
        assert_eq!(t.scale, 1.0, "scale 0 is clamped to 100 %");
        assert_eq!(t.text_k, 8.0 / 15.0, "font size -5 is clamped to 8");
        assert_eq!(t.gap, d.gap, "an unreadable number takes the default");
        assert_eq!(t.radius, 0, "radius -1 is clamped to 0");
        assert!(t.glow[3] <= 0.4 + 1e-6);
        assert_eq!(t.motion, Motion::Medium);
        assert!(t.layout.address_top && t.layout.statusbar);
        assert_eq!(t.cursor_size, 32, "WY-11: theme.cursor.size defaults to 32 logical px");
        v.set("theme.scale", "99999");
        v.set("theme.font.size", "99999");
        v.set("theme.cursor.size", "99999");
        let t = Theme::build(&v);
        assert_eq!((t.scale, t.text_k), (3.0, 48.0 / 15.0), "huge values are clamped");
        assert_eq!(t.cursor_size, 128);
    }

    #[test]
    fn settingsd_defaults_match_the_built_in_ones() {
        // Without settingsd (nested on CachyOS) and with it the screen must look the same.
        let src = include_str!("../../settingsd/src/builtin.rs");
        for (p, mode) in [(&DARK, "dark"), (&LIGHT, "light")] {
            for c in [p.bg, p.panel, p.panel2, p.paper, p.line, p.text, p.muted] {
                assert!(src.contains(&format!("\"{c}\"")), "settingsd lacks {mode} colour {c}");
            }
        }
        for k in [
            "theme.layout.address_bar",
            "theme.layout.favourites_bar",
            "theme.layout.workspaces_bar",
            "theme.layout.claude_sidebar",
            "theme.layout.statusbar",
            "desktop.favourites",
            "claude.name",
        ] {
            assert!(src.contains(k), "settingsd lacks {k}");
        }
        assert!(src.contains(&format!("text(\"{ACCENT}\")")));
    }
}
