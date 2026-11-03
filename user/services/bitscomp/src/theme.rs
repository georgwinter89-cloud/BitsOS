//! The token table (WY-5, DS-1): every visual value bitscomp uses. Built from the `theme.*` keys of the settings
//! registry (`settings`, user/services/settingsd/src/builtin.rs) on top of the defaults, the design v2 tokens of
//! 28.10.2026 (`tokens`, plan/spec/desktop.md §0). The first chrome's sizes are CSS px of its mock-up at 100 % =
//! logical pixels, made physical by the output scale (`theme.scale`, 125 %); the design's are board px (`board`).

mod tokens;

pub use tokens::{ACCENT, BOARD_SCALE, DARK, LIGHT, Shadow, Style};

use crate::settings::Values;
use crate::text::{Families, Family, Font, Typeface, Weight};

pub type Rgba = [f32; 4];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Motion {
    Medium,
    Calm,
    Off,
}

/// theme.effects: the level the user chose. `Gpu` is accepted now and drawn like `Cheap` until a GPU renderer
/// exists (DS-2; the ambient motion of desktop.md §0 answer 7 comes with it).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Effects {
    Cheap,
    Gpu,
}

impl Effects {
    pub fn word(self) -> &'static str {
        match self {
            Effects::Cheap => "cheap",
            Effects::Gpu => "gpu",
        }
    }
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
    /// DS-3: theme.layout.preset, which chrome is drawn.
    pub preset: Preset,
}

/// DS-3: the chrome as a preset (Georg 28.10., desktop.md §0 answer 3): the design's Bühne (header with workspace
/// chips, the stage, the right column, the time strip and the Feld bar) is the default, today's chrome stays.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Preset {
    Buehne,
    Classic,
}

impl Preset {
    /// `classic` (also `klassisch`) picks today's chrome; anything else is the Bühne.
    pub fn parse(s: &str) -> Preset {
        match s.trim().to_ascii_lowercase().as_str() {
            "classic" | "klassisch" => Preset::Classic,
            _ => Preset::Buehne,
        }
    }
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
    /// warnung (errors, data loss) and behind it.
    pub danger: Rgba,
    pub danger_chip: Rgba,
    /// The ring's "waiting for Georg" colour (kontext-2, the amber).
    pub warm: Rgba,
    pub veil: Rgba,
    /// Design v2 (DS-1): the strong line (focus edges, rulers), the accent's soft tone (behind selected things) and
    /// the text on it, Claude's colour, the terminal's ground and text, the three context colours and their text.
    pub line_strong: Rgba,
    pub accent_soft: Rgba,
    pub on_accent: Rgba,
    pub claude: Rgba,
    pub terminal: Rgba,
    pub on_terminal: Rgba,
    pub kontext: [Rgba; 3],
    pub on_kontext: Rgba,
    /// shadow-raised, shadow-lifted (board px).
    pub shadow_raised: Shadow,
    pub shadow_lifted: Shadow,
    /// space-1..8 and radius-a, board px after theme.spacing and theme.radius.
    pub space: [i32; 8],
    pub radius_card: i32,
    /// Board px to physical px: `BOARD_SCALE` at the default scale, following theme.scale.
    pub board: f64,
    /// The typeface of each family (theme.font.display/ui/mono; an unknown name keeps the design's).
    pub fonts: Families,
    /// The chosen effect level (theme.effects).
    pub effects: Effects,
    /// Whether the GPU level is drawn: false until a GPU renderer exists, so `gpu` draws exactly like `cheap`
    /// (DS-2; every `if t.effects_gpu` picks the cheap branch).
    pub effects_gpu: bool,
    /// theme.scanlines: the design's scanlines, baked only into bitscomp's own ground (dark mode), never over windows.
    pub scanlines: bool,
    pub motion: Motion,
    pub layout: Layout,
    /// The values of theme.font.display/ui/mono as set (`fonts` holds what is drawn; `font_notes` says why not).
    pub font_display: String,
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
        let bg = col("bg", pal.bg);
        let panel = col("surface", pal.panel);
        let ink = col("text", pal.text);
        let line = col("border", pal.line);
        // The user's accent, if any (`tokens::ACCENT` is settingsd's default and means the mode's own).
        let user = v.text("theme.accent").and_then(hex).filter(|c| Some(*c) != hex(ACCENT));
        let (accent, accent_soft, on_accent) = match user {
            None => (col("accent", pal.accent), col("accent_soft", pal.accent_soft), col("on_accent", pal.on_accent)),
            Some(a) => (a, soft(a, if dark { bg } else { panel }, dark), on(a)),
        };
        // theme.glow 60 % (the default) is the mock-up's glow: 18 % alpha dark, 14 % light.
        let glow_k = int("theme.glow", 60, 0, 100) as f32 / 60.0;
        let glow_a = (if dark { 0.18 } else { 0.14 } * glow_k).min(0.4);
        let kontext =
            [col("kontext_1", pal.kontext[0]), col("kontext_2", pal.kontext[1]), col("kontext_3", pal.kontext[2])];
        let (shadow_raised, shadow_lifted) = tokens::shadows(dark, line, ink, hex(pal.lifted_ring).unwrap());
        let family = |k: &str, d: Typeface| v.text(k).and_then(Typeface::by_name).unwrap_or(d);
        let design = Families::default();
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
            preset: Preset::parse(&text("theme.layout.preset", "buehne")),
        };
        // DS-3: the Bühne lays its tiles out as the boards' blocks: 24 board px apart, radius-a (28) corners. Board
        // px in logical px do not depend on theme.scale (`board` / `scale`), so the Bühne keeps its proportions.
        let buehne = layout.preset == Preset::Buehne;
        let lb = |px: f32| (px as f64 * BOARD_SCALE / tokens::BOARD_AT_SCALE).round() as i32;
        let tile_bar = if layout.address_top { 0 } else { 34 };
        let tab_row = 38;
        let danger = col("warnung", pal.warnung);
        let scale = int("theme.scale", 125, 100, 300) as f64 / 100.0;
        Theme {
            dark,
            bg,
            panel,
            panel2: col("surface2", pal.panel2),
            paper: col("paper", pal.paper),
            line,
            text: ink,
            muted: col("text_dim", pal.muted),
            accent,
            glow: alpha(accent, glow_a),
            glow_soft: alpha(accent, glow_a / 2.0),
            chip: accent_soft,
            danger,
            danger_chip: alpha(danger, 0.14),
            warm: kontext[1],
            veil: alpha(bg, 0.8),
            line_strong: col("line_strong", pal.line_strong),
            accent_soft,
            on_accent,
            claude: col("claude", pal.claude),
            terminal: col("terminal", pal.terminal),
            on_terminal: col("on_terminal", pal.on_terminal),
            kontext,
            on_kontext: col("on_kontext", pal.on_kontext),
            shadow_raised,
            shadow_lifted,
            space: tokens::SPACE.map(sp),
            radius_card: rad(tokens::RADIUS_CARD),
            board: BOARD_SCALE * scale / tokens::BOARD_AT_SCALE,
            fonts: Families {
                display: family("theme.font.display", design.display),
                ui: family("theme.font.ui", design.ui),
                mono: family("theme.font.mono", design.mono),
            },
            effects: if text("theme.effects", "cheap") == "gpu" { Effects::Gpu } else { Effects::Cheap },
            effects_gpu: false,
            scanlines: flag("theme.scanlines", true),
            motion: match text("theme.motion", "medium").as_str() {
                "off" => Motion::Off,
                "calm" => Motion::Calm,
                _ => Motion::Medium,
            },
            layout,
            font_display: text("theme.font.display", design.display.name()),
            font_ui: text("theme.font.ui", design.ui.name()),
            font_mono: text("theme.font.mono", design.mono.name()),
            text_k: int("theme.font.size", 15, 8, 48) as f32 / 15.0,
            top_h: 56,
            fav_h: 34,
            list_w: 212,
            side_w: 300,
            status_h: 32,
            gap: if buehne { lb(24.0 * spacing) } else { sp(12) },
            border: 1,
            radius: if buehne { lb(tokens::RADIUS_CARD as f32 * radius) } else { rad(12) },
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
            scale,
            cursor_size: int("theme.cursor.size", 32, 16, 128) as u32,
        }
    }

    /// A mock-up text size (CSS px) after theme.font.size.
    pub fn fs(&self, px: f32) -> f32 {
        px * self.text_k
    }

    /// The first chrome's text: mock-up px `px` in the typeface the settings chose for `f`.
    pub fn font(&self, f: Family, px: f32, w: Weight) -> Font {
        Font::of(self.fonts.get(f), self.fs(px), w, self.scale)
    }

    /// A design v2 type style at this output (board px, theme.font.size applies too). This and what follows are
    /// for the design v2 parts (DS-2..5) and the apply log (CR DS-1-3).
    pub fn style(&self, s: Style) -> Font {
        let sp = tokens::spec(s);
        Font::of(self.fonts.get(sp.family), sp.px * self.text_k, sp.weight, self.board).spaced(sp.track)
    }

    /// `s` as a style shows it (the labels are uppercase).
    pub fn label(s: Style, text: &str) -> String {
        if tokens::spec(s).upper { text.to_uppercase() } else { text.to_string() }
    }

    /// Board px in logical px (DS-3: the Bühne's geometry; the same at every theme.scale).
    pub fn bl(&self, px: f32) -> i32 {
        (px as f64 * self.board / self.scale).round() as i32
    }

    /// A font at a board px size (DS-3), theme.font.size applied.
    pub fn bfont(&self, f: Family, px: f32, w: Weight) -> Font {
        Font::of(self.fonts.get(f), px * self.text_k, w, self.board)
    }

    /// For the log (B6): each theme.font.* value that names no shipped typeface, and what is drawn instead.
    pub fn font_notes(&self) -> Vec<String> {
        [
            ("display", &self.font_display, self.fonts.display),
            ("ui", &self.font_ui, self.fonts.ui),
            ("mono", &self.font_mono, self.fonts.mono),
        ]
        .into_iter()
        .filter(|(_, set, _)| Typeface::by_name(set).is_none())
        .map(|(k, set, tf)| format!("theme.font.{k} {set:?} is not built in, {} is drawn", tf.name()))
        .collect()
    }
}

/// The soft tone of a user's accent: it over the ground, as the design's accent-soft is to its accent.
fn soft(a: Rgba, ground: Rgba, dark: bool) -> Rgba {
    let k = if dark { 0.15 } else { 0.2 };
    [0, 1, 2, 3].map(|i| if i == 3 { 1.0 } else { ground[i] + (a[i] - ground[i]) * k })
}

/// Text on a user's accent: the dark mode's on-accent on a bright accent, the light mode's on a deep one.
fn on(a: Rgba) -> Rgba {
    let lin = |c: f32| if c <= 0.04045 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) };
    let y = 0.2126 * lin(a[0]) + 0.7152 * lin(a[1]) + 0.0722 * lin(a[2]);
    hex(if y > 0.3 { DARK.on_accent } else { LIGHT.on_accent }).unwrap()
}

#[cfg(test)]
mod tests;
