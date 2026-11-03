//! Design v2 tokens (DS-1): the two palettes of Georg's final design of 28.10.2026 ("Absicht dunkel", the default,
//! and "Absicht", the light mode), the type styles, spacing, radii and shadows, as Rust values. Sizes are px of the
//! design boards (1920x1080); `BOARD_SCALE` maps them onto the Mini-PC's 2560x1440.

use super::Rgba;
use crate::text::{Family, Weight};

/// Board px to physical px at 2560x1440 (plan/spec/desktop.md §0 answer 8). It holds at the default
/// `theme.scale` 125 %, so the old chrome (mock-up px at 125 %) keeps its size; another scale moves both alike.
pub const BOARD_SCALE: f64 = 4.0 / 3.0;
pub const BOARD_AT_SCALE: f64 = 1.25;

/// One mode's colours. The first seven are the WY-5 keys `theme.{dark,light}.*` (names kept, so a stored value keeps
/// working); the rest are the design's new tokens under the same prefix.
pub struct Palette {
    pub bg: &'static str,
    pub panel: &'static str,
    pub panel2: &'static str,
    pub paper: &'static str,
    pub line: &'static str,
    pub text: &'static str,
    pub muted: &'static str,
    pub line_strong: &'static str,
    pub accent: &'static str,
    pub accent_soft: &'static str,
    pub on_accent: &'static str,
    pub claude: &'static str,
    pub warnung: &'static str,
    pub terminal: &'static str,
    pub on_terminal: &'static str,
    pub kontext: [&'static str; 3],
    pub on_kontext: &'static str,
    /// The lifted shadow's 1 px ring (dark mode; not a key, the design gives no name for it).
    pub lifted_ring: &'static str,
}

#[cfg(test)]
impl Palette {
    /// (key below `theme.<mode>.`, default) for every colour key.
    pub fn keys(&self) -> [(&'static str, &'static str); 19] {
        [
            ("bg", self.bg),
            ("surface", self.panel),
            ("surface2", self.panel2),
            ("paper", self.paper),
            ("border", self.line),
            ("text", self.text),
            ("text_dim", self.muted),
            ("line_strong", self.line_strong),
            ("accent", self.accent),
            ("accent_soft", self.accent_soft),
            ("on_accent", self.on_accent),
            ("claude", self.claude),
            ("warnung", self.warnung),
            ("terminal", self.terminal),
            ("on_terminal", self.on_terminal),
            ("kontext_1", self.kontext[0]),
            ("kontext_2", self.kontext[1]),
            ("kontext_3", self.kontext[2]),
            ("on_kontext", self.on_kontext),
        ]
    }
}

/// bg = surface, surface = surface-raised, surface2 = surface-sunken (fields, wells), paper = surface-raised (a shown
/// tab runs into its raised tile), border = line, text = ink, text_dim = ink-muted.
pub const DARK: Palette = Palette {
    bg: "#121a16",
    panel: "#1b2520",
    panel2: "#0c120f",
    paper: "#1b2520",
    line: "#2b3731",
    text: "#e8eee9",
    muted: "#a3b0a8",
    line_strong: "#6b7a71",
    accent: "#7fcfa0",
    accent_soft: "#1f3a2c",
    on_accent: "#0f1712",
    claude: "#7fcfa0",
    warnung: "#ff7b6b",
    terminal: "#0a0f0c",
    on_terminal: "#d5e2da",
    kontext: ["#d9765b", "#e3bd5f", "#86afd9"],
    on_kontext: "#121a16",
    lifted_ring: "#3a4741",
};
pub const LIGHT: Palette = Palette {
    bg: "#e3e7e3",
    panel: "#fbfcfa",
    panel2: "#d5dbd5",
    paper: "#fbfcfa",
    line: "#c3cbc3",
    text: "#15201a",
    muted: "#4e5b52",
    line_strong: "#6f7b71",
    accent: "#2f6b4f",
    accent_soft: "#cfe3d6",
    on_accent: "#fbfcfa",
    claude: "#2f6b4f",
    warnung: "#b3261e",
    terminal: "#15201a",
    on_terminal: "#e8eee9",
    kontext: ["#e2694b", "#f2c14e", "#8db9e6"],
    on_kontext: "#15201a",
    lifted_ring: "#c3cbc3",
};

/// settingsd's default of `theme.accent`: this value (or none, or a malformed one) means "the mode's own accent";
/// any other colour is the user's accent in both modes, and the soft and on-accent tones are derived from it.
pub const ACCENT: &str = DARK.accent;

/// The 4 px grid (space-1..8), board px at `theme.spacing` 8.
pub const SPACE: [i32; 8] = [4, 8, 12, 16, 24, 32, 48, 64];
/// radius-a (cards), board px at `theme.radius` 8. radius-a-sm (14) and radius-voll (half the height) come with
/// their first drawn part (C1: nothing drew them yet; tokens.json in plan/spec/desktop-v2 keeps the values).
pub const RADIUS_CARD: i32 = 28;

/// One layer of a CSS box shadow, board px; the colour premultiplied. A ring is `blur` 0 with `spread` its width.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ShadowLayer {
    pub dy: f32,
    pub blur: f32,
    pub spread: f32,
    pub color: Rgba,
}

/// shadow-raised or shadow-lifted: two layers, the inner first. The cheap level draws them as stepped
/// lines or a cached sprite (no blur); the GPU level as written.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Shadow(pub [ShadowLayer; 2]);

fn ring(w: f32, color: Rgba) -> ShadowLayer {
    ShadowLayer { dy: 0.0, blur: 0.0, spread: w, color }
}

fn drop(dy: f32, blur: f32, spread: f32, color: Rgba) -> ShadowLayer {
    ShadowLayer { dy, blur, spread, color }
}

/// (raised, lifted) of a mode from its resolved colours. The focus-ring token (2 px ground, 4 px accent) comes with
/// the first part that draws keyboard focus (C1: nothing drew it).
pub fn shadows(dark: bool, line: Rgba, ink: Rgba, lifted_ring: Rgba) -> (Shadow, Shadow) {
    use super::alpha;
    let black = [0.0, 0.0, 0.0, 1.0];
    if dark {
        (
            Shadow([ring(1.0, line), drop(12.0, 32.0, -12.0, alpha(black, 0.70))]),
            Shadow([ring(1.0, lifted_ring), drop(24.0, 48.0, -12.0, alpha(black, 0.80))]),
        )
    } else {
        (
            Shadow([drop(1.0, 2.0, 0.0, alpha(ink, 0.06)), drop(12.0, 32.0, -12.0, alpha(ink, 0.18))]),
            Shadow([drop(2.0, 4.0, 0.0, alpha(ink, 0.08)), drop(24.0, 48.0, -12.0, alpha(ink, 0.25))]),
        )
    }
}

/// The design's type styles that something draws (tokens "3 · Absicht", plus the uppercase mono labels of the
/// boards). a-titel, a-zahl, a-text and a-klein (and the line heights) come with their first drawn part (C1: DS-2..5
/// drew none of them; tokens.json in plan/spec/desktop-v2 keeps the values).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Style {
    /// The workspace's headline on the stage.
    Absicht,
    /// Uppercase mono labels (`.lbl`): show them through `Theme::label`.
    Label,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct StyleSpec {
    pub family: Family,
    /// Size in board px.
    pub px: f32,
    pub weight: Weight,
    /// Letter spacing in em.
    pub track: f32,
    pub upper: bool,
}

pub fn spec(s: Style) -> StyleSpec {
    let st = |family, px, weight, track| StyleSpec { family, px, weight, track, upper: false };
    match s {
        Style::Absicht => st(Family::Display, 44.0, Weight::SemiBold, -0.02),
        Style::Label => StyleSpec { upper: true, ..st(Family::Mono, 11.0, Weight::Medium, 0.08) },
    }
}
