//! Text of the interface (WY-5, DS-1): the design v2 families Bricolage Grotesque (display), Figtree (interface) and
//! JetBrains Mono (labels, addresses), and Exo 2 (the first look's font, still selectable), all OFL 1.1 (fonts/),
//! rendered by fontdue with grey-level antialiasing. The faces are built into the binary (no font files on V2, no
//! fontconfig) and parsed on first use, so a family nobody shows costs nothing at boot. A character the font lacks
//! comes from the next family that has it; the glyphs none has are drawn as paths (`icon`). One glyph cache for all
//! sizes (key: face or icon, pixel size, character); it is a cache, emptied when it grows large.

mod icon;

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::OnceLock;

use fontdue::{Font as Face, FontSettings};

use crate::canvas::Canvas;

pub const LICENCE: &str = concat!(
    "Exo 2:\n\n",
    include_str!("../fonts/OFL.txt"),
    "\n\nBricolage Grotesque:\n\n",
    include_str!("../fonts/OFL-BricolageGrotesque.txt"),
    "\n\nFigtree:\n\n",
    include_str!("../fonts/OFL-Figtree.txt"),
    "\n\nJetBrains Mono:\n\n",
    include_str!("../fonts/OFL-JetBrainsMono.txt"),
);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Weight {
    Light,
    Regular,
    Medium,
    SemiBold,
    /// Figtree 700 and Exo 2 700 (the boards' strong text, DS-2..5).
    Bold,
}

impl Weight {
    fn css(self) -> i32 {
        match self {
            Weight::Light => 300,
            Weight::Regular => 400,
            Weight::Medium => 500,
            Weight::SemiBold => 600,
            Weight::Bold => 700,
        }
    }
}

/// What a text is for (the design's absicht-display, -text and -mono); `theme.font.*` picks a typeface for each.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Family {
    Display,
    Ui,
    Mono,
}

/// A typeface built into bitscomp.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Typeface {
    Bricolage,
    Figtree,
    JetBrainsMono,
    Exo2,
}

impl Typeface {
    pub fn name(self) -> &'static str {
        match self {
            Typeface::Bricolage => "Bricolage Grotesque",
            Typeface::Figtree => "Figtree",
            Typeface::JetBrainsMono => "JetBrains Mono",
            Typeface::Exo2 => "Exo 2",
        }
    }

    /// A `theme.font.*` value: the name in any case, spaces, dashes and underscores ignored.
    pub fn by_name(s: &str) -> Option<Typeface> {
        let k: String = s.chars().filter(|c| !matches!(c, ' ' | '-' | '_')).flat_map(char::to_lowercase).collect();
        Some(match k.as_str() {
            "bricolagegrotesque" | "bricolage" => Typeface::Bricolage,
            "figtree" => Typeface::Figtree,
            "jetbrainsmono" | "jetbrains" => Typeface::JetBrainsMono,
            "exo2" => Typeface::Exo2,
            _ => return None,
        })
    }
}

/// The typeface of each family (the design's by default).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Families {
    pub display: Typeface,
    pub ui: Typeface,
    pub mono: Typeface,
}

impl Default for Families {
    fn default() -> Self {
        Families { display: Typeface::Bricolage, ui: Typeface::Figtree, mono: Typeface::JetBrainsMono }
    }
}

impl Families {
    pub fn get(&self, f: Family) -> Typeface {
        match f {
            Family::Display => self.display,
            Family::Ui => self.ui,
            Family::Mono => self.mono,
        }
    }
}

/// The static instances in fonts/ (README: the weights the boards use).
const FILES: &[(Typeface, i32, &[u8])] = &[
    (Typeface::Bricolage, 500, include_bytes!("../fonts/BricolageGrotesque-500.ttf")),
    (Typeface::Bricolage, 600, include_bytes!("../fonts/BricolageGrotesque-600.ttf")),
    (Typeface::Figtree, 400, include_bytes!("../fonts/Figtree-400.ttf")),
    (Typeface::Figtree, 500, include_bytes!("../fonts/Figtree-500.ttf")),
    (Typeface::Figtree, 600, include_bytes!("../fonts/Figtree-600.ttf")),
    (Typeface::Figtree, 700, include_bytes!("../fonts/Figtree-700.ttf")),
    (Typeface::JetBrainsMono, 400, include_bytes!("../fonts/JetBrainsMono-400.ttf")),
    (Typeface::JetBrainsMono, 500, include_bytes!("../fonts/JetBrainsMono-500.ttf")),
    (Typeface::JetBrainsMono, 600, include_bytes!("../fonts/JetBrainsMono-600.ttf")),
    (Typeface::Exo2, 300, include_bytes!("../fonts/Exo2-300.ttf")),
    (Typeface::Exo2, 400, include_bytes!("../fonts/Exo2-400.ttf")),
    (Typeface::Exo2, 500, include_bytes!("../fonts/Exo2-500.ttf")),
    (Typeface::Exo2, 600, include_bytes!("../fonts/Exo2-600.ttf")),
    (Typeface::Exo2, 700, include_bytes!("../fonts/Exo2-700.ttf")),
];

/// Where a character the font lacks is looked for next.
const FALLBACK: [Typeface; 4] = [Typeface::Figtree, Typeface::JetBrainsMono, Typeface::Exo2, Typeface::Bricolage];

fn face(i: usize) -> &'static Face {
    static FACES: [OnceLock<Face>; FILES.len()] = [const { OnceLock::new() }; FILES.len()];
    FACES[i].get_or_init(|| Face::from_bytes(FILES[i].2, FontSettings::default()).expect("built-in font parses"))
}

/// The parsed face of `tf` nearest to weight `w`, for a drawing that places glyphs itself: DS-5's boot scene must
/// match the kernel's boot screen, whose atlases crates/console/build.rs makes from these same files with fontdue.
pub fn face_of(tf: Typeface, w: Weight) -> &'static Face {
    face(file(tf, w))
}

/// The file of `tf` nearest to weight `w` (the heavier one on a tie: Bricolage has no 400).
fn file(tf: Typeface, w: Weight) -> usize {
    let want = w.css();
    let mut best: Option<usize> = None;
    for (i, &(t, fw, _)) in FILES.iter().enumerate() {
        if t != tf {
            continue;
        }
        let better = best.is_none_or(|b| {
            let (d, bd) = ((fw - want).abs(), (FILES[b].1 - want).abs());
            d < bd || (d == bd && fw > FILES[b].1)
        });
        if better {
            best = Some(i);
        }
    }
    best.expect("every typeface has a file")
}

/// What draws a character.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Src {
    Face(usize),
    Icon(icon::Icon),
}

struct Glyph {
    xmin: i32,
    ymin: i32,
    width: usize,
    height: usize,
    cov: Vec<u8>,
}

/// Entries kept before the cache starts over (a cache bound, not a limit on anything shown).
const CACHE_KEEP: usize = 8192;

thread_local! {
    static CACHE: RefCell<HashMap<(Src, u32, char), Rc<Glyph>>> = RefCell::new(HashMap::new());
    /// (own face, character) -> what draws it, for the characters the own face lacks (same bound as `CACHE`).
    static FALLBACK_OF: RefCell<HashMap<(usize, char), Src>> = RefCell::new(HashMap::new());
}

fn glyph(src: Src, px: f32, c: char) -> Rc<Glyph> {
    CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        let key = (src, px.to_bits(), c);
        if let Some(g) = cache.get(&key) {
            return g.clone();
        }
        if cache.len() >= CACHE_KEEP {
            cache.clear();
        }
        let g = Rc::new(match src {
            Src::Face(i) => {
                let (m, cov) = face(i).rasterize(c, px);
                Glyph { xmin: m.xmin, ymin: m.ymin, width: m.width, height: m.height, cov }
            }
            Src::Icon(ic) => {
                let (xmin, ymin, width, height, cov) = icon::raster(ic, px);
                Glyph { xmin, ymin, width, height, cov }
            }
        });
        cache.insert(key, g.clone());
        g
    })
}

/// Pixel sizes a font is drawn at: a size of 0, NaN or a runaway scale must not ask the rasteriser for nothing or for
/// a glyph bitmap the size of a screen (the settings clamp theme.scale and theme.font.size; the largest style there
/// is about 450 px).
const MIN_PX: f32 = 1.0;
const MAX_PX: f32 = 1024.0;

/// A text style at output resolution.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Font {
    typeface: Typeface,
    weight: Weight,
    /// Size in physical pixels.
    px: f32,
    /// Letter spacing in physical pixels.
    track: f32,
}

impl Font {
    /// `logical` px at output scale `scale`, in the default interface typeface (Figtree); `Theme::font` gives the
    /// one the settings chose. Only the tests need the default face; the program always asks the theme.
    #[cfg(test)]
    pub fn new(logical: f32, weight: Weight, scale: f64) -> Font {
        Font::of(Families::default().ui, logical, weight, scale)
    }

    pub fn of(typeface: Typeface, logical: f32, weight: Weight, scale: f64) -> Font {
        let px = (logical as f64 * scale) as f32;
        let px = if px.is_finite() { px.clamp(MIN_PX, MAX_PX) } else { MIN_PX };
        Font { typeface, weight, px, track: 0.0 }
    }

    /// Letter spacing in em (the mock-up's `letter-spacing: 0.12em`; negative tightens).
    pub fn spaced(self, em: f32) -> Font {
        Font { track: em * self.px, ..self }
    }

    #[cfg(test)]
    pub fn typeface(&self) -> Typeface {
        self.typeface
    }

    fn own(&self) -> usize {
        file(self.typeface, self.weight)
    }

    fn src(&self, c: char) -> Src {
        if let Some(i) = icon::of(c) {
            return Src::Icon(i);
        }
        let own = self.own();
        if c.is_ascii() || face(own).has_glyph(c) {
            return Src::Face(own);
        }
        // A character the own face lacks: the probe through the other families is remembered, so a long title in a
        // script no family has costs the probe once per character, not once per character per frame.
        FALLBACK_OF.with(|m| {
            let mut m = m.borrow_mut();
            if let Some(&s) = m.get(&(own, c)) {
                return s;
            }
            if m.len() >= CACHE_KEEP {
                m.clear();
            }
            let s = FALLBACK
                .iter()
                .filter(|&&tf| tf != self.typeface)
                .map(|&tf| file(tf, self.weight))
                .find(|&i| face(i).has_glyph(c))
                .map_or(Src::Face(own), Src::Face);
            m.insert((own, c), s);
            s
        })
    }

    fn advance(&self, src: Src, c: char) -> f32 {
        match src {
            Src::Face(i) => face(i).metrics(c, self.px).advance_width,
            Src::Icon(i) => icon::advance(i) * self.px,
        }
    }

    fn line(&self) -> (f32, f32) {
        face(self.own())
            .horizontal_line_metrics(self.px)
            .map_or((self.px * 0.8, -self.px * 0.2), |l| (l.ascent, l.descent))
    }

    /// The line's height in pixels (ascent to descent).
    pub fn height(&self) -> i32 {
        let (a, d) = self.line();
        (a - d).round() as i32
    }

    /// Pen positions: each character with what draws it, its x (pixels from the start) and advance; the total.
    fn layout(&self, s: &str) -> (Vec<(char, Src, f32, f32)>, f32) {
        let mut x = 0.0;
        let mut out = Vec::with_capacity(s.len());
        let mut prev: Option<(char, Src)> = None;
        for c in s.chars() {
            let src = self.src(c);
            if let (Some((p, Src::Face(pi))), Src::Face(i)) = (prev, src)
                && pi == i
            {
                x += face(i).horizontal_kern(p, c, self.px).unwrap_or(0.0);
            }
            let adv = self.advance(src, c);
            out.push((c, src, x, adv));
            x += adv + self.track;
            prev = Some((c, src));
        }
        (out, (x - if prev.is_some() { self.track } else { 0.0 }).max(0.0))
    }

    pub fn width(&self, s: &str) -> i32 {
        self.layout(s).1.ceil() as i32
    }

    /// Each character's box (x, width) in pixels from the start of the text (for `ctl hot`).
    pub fn boxes(&self, s: &str) -> Vec<(i32, i32)> {
        self.layout(s).0.iter().map(|&(_, _, x, adv)| (x.round() as i32, adv.round() as i32)).collect()
    }

    /// `s`, cut with an ellipsis when wider than `max` pixels. One layout pass gives each prefix's end; only the
    /// cuts that this estimate lets through are measured exactly (kerning before the ellipsis), so a long title costs
    /// O(n), not O(n^2).
    pub fn fit(&self, s: &str, max: i32) -> String {
        let (chars, total) = self.layout(s);
        if total.ceil() as i32 <= max {
            return s.to_string();
        }
        let ell = self.layout("…").1;
        for n in (0..chars.len()).rev() {
            let end = if n == 0 { 0.0 } else { chars[n - 1].2 + chars[n - 1].3 + self.track };
            // Slack for a negative kern before the ellipsis; trimmed spaces give a cut a shorter prefix gives too.
            if end + ell - 0.1 * self.px > max as f32 {
                continue;
            }
            let cut: String = s.chars().take(n).collect::<String>().trim_end().to_string() + "…";
            if self.width(&cut) <= max {
                return cut;
            }
        }
        if self.width("…") <= max { "…".into() } else { String::new() }
    }

    /// Whether this font draws `c` with a real glyph: its own, a fallback family's or an icon (else the empty box).
    #[cfg(test)]
    pub fn draws(&self, c: char) -> bool {
        match self.src(c) {
            Src::Icon(_) => true,
            Src::Face(i) => face(i).has_glyph(c),
        }
    }

    /// Whether the default interface font draws `c`.
    #[cfg(test)]
    pub fn has(c: char) -> bool {
        Font::new(15.0, Weight::Regular, 1.0).draws(c)
    }

    /// Draws `s` with its line's top at (x, y) in premultiplied colour `c`; returns the width.
    pub fn draw(&self, canvas: &mut Canvas, x: i32, y: i32, s: &str, c: u32) -> i32 {
        let (ascent, _) = self.line();
        let base = (y as f32 + ascent).round() as i32;
        let (chars, w) = self.layout(s);
        for (ch, src, pen, _) in chars {
            if ch == ' ' {
                continue;
            }
            let g = glyph(src, self.px, ch);
            let gx = x + (pen.round() as i32) + g.xmin;
            let gy = base - g.ymin - g.height as i32;
            for row in 0..g.height {
                for col in 0..g.width {
                    canvas.put_cov(gx + col as i32, gy + row as i32, c, g.cov[row * g.width + col]);
                }
            }
        }
        w.ceil() as i32
    }
}

#[cfg(test)]
mod tests;
