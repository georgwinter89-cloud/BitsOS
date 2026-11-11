//! WY-19c: the transitions between two pictures: the circle wipe (Wechsel.dc.html `.wipe`), the blur and hue glitch
//! between snapshots (Verlauf.dc.html `sA`), the carousel (Wechsel's turned cards), the layout morph (WerkbankTeile's
//! `.blk`) and the title's blur reveal (Boot.dc.html `tA`). The old picture is the last frame on the screen, taken
//! by the renderer before the first frame of the transition (`Kind::Snapshot`), so a transition never asks the state
//! that changed for anything. Pure: from the progress to what the shader gets.

use std::rc::Rc;
use std::time::Duration;

use super::motion;
use crate::theme::Rgba;

/// What plays. `Fade` is what `theme.motion` calm gets instead of the others ("calm keeps only the fades").
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Play {
    Wipe,
    Fade,
    Glitch,
    Carousel,
    /// The old picture's `from` flies to `to` (physical px x, y, w, h) and fades as it lands.
    Morph { from: [i32; 4], to: [i32; 4] },
}

impl Play {
    /// The design's length.
    pub fn len(self) -> Duration {
        Duration::from_millis(match self {
            Play::Wipe => 1000,
            Play::Fade => 300,
            Play::Glitch => 800,
            Play::Carousel | Play::Morph { .. } => 800,
        })
    }

    pub fn word(self) -> &'static str {
        match self {
            Play::Wipe => "wipe",
            Play::Fade => "fade",
            Play::Glitch => "glitch",
            Play::Carousel => "carousel",
            Play::Morph { .. } => "morph",
        }
    }
}

/// A transition under way.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Playing {
    pub what: Play,
    pub since: Duration,
    /// Which snapshot it shows (a new transition takes a new one).
    pub serial: u64,
    /// `ctl fx hold K`: stands at this progress (the screendump tests' mid-transition picture).
    pub hold: Option<f32>,
}

impl Playing {
    /// 0..1; None once it is over.
    pub fn progress(&self, now: Duration) -> Option<f32> {
        if let Some(k) = self.hold {
            return Some(k);
        }
        let k = now.saturating_sub(self.since).as_secs_f32() / self.what.len().as_secs_f32();
        (k < 1.0).then_some(k)
    }
}

/// What the shader of a transition gets.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Look {
    /// The old picture with a hole of radius `r` around (`cx`, `cy`), the new scene under it.
    Wipe { cx: f32, cy: f32, r: f32 },
    Fade { alpha: f32 },
    /// The old picture, and over it the new scene blurred, shifted, skewed and hue-turned.
    Glitch { alpha: f32, blur: f32, dx: f32, skew: f32, hue: f32 },
    /// Both pictures as turned cards on the ground.
    Carousel { off: f32, ground: Rgba, persp: f32, radius: f32 },
    /// The part `src` (x, y, w, h in the old picture) into the element's rectangle.
    Morph { src: [f32; 4], radius: f32, alpha: f32 },
}

impl Look {
    /// Whether the shader needs the new scene as a texture (the others let it show through).
    #[cfg(feature = "gpu")]
    pub fn wants_live(&self) -> bool {
        matches!(self, Look::Glitch { .. } | Look::Carousel { .. })
    }
}

fn mix(a: f32, b: f32, k: f32) -> f32 {
    a + (b - a) * k
}

/// (element rectangle, look) of `what` at progress `k`, on an output `w` x `h`, `board` physical px per board px.
pub fn look(what: Play, k: f32, (w, h): (i32, i32), board: f32, ground: Rgba, card_r: f32) -> ([i32; 4], Look) {
    let full = [0, 0, w, h];
    let (fw, fh) = (w as f32, h as f32);
    match what {
        // clip-path: circle(0% at 50% 56%) to circle(150% ...); a percentage of sqrt(w² + h²) / sqrt(2).
        Play::Wipe => {
            let e = motion::bezier(0.7, 0.0, 0.2, 1.0, k);
            let r = e * 1.5 * ((fw * fw + fh * fh) / 2.0).sqrt();
            (full, Look::Wipe { cx: fw * 0.5, cy: fh * 0.56, r })
        }
        Play::Fade => (full, Look::Fade { alpha: 1.0 - motion::ease_in_out(k) }),
        // sA: 0% opacity 0, blur 10, translateX(-40) skewX(6deg); 30% opacity .6, blur 4, hue 40deg; 100% none.
        // Each keyframe interval runs its own curve; the transform has no 30% keyframe.
        Play::Glitch => {
            let (first, second) = if k < 0.3 { (motion::rise(k / 0.3), 0.0) } else { (1.0, motion::rise((k - 0.3) / 0.7)) };
            let whole = motion::rise(k);
            (
                full,
                Look::Glitch {
                    alpha: if k < 0.3 { mix(0.0, 0.6, first) } else { mix(0.6, 1.0, second) },
                    blur: if k < 0.3 { mix(10.0, 4.0, first) } else { mix(4.0, 0.0, second) } * board,
                    dx: mix(-40.0, 0.0, whole) * board,
                    skew: mix(6.0, 0.0, whole).to_radians().tan(),
                    hue: if k < 0.3 { mix(0.0, 40.0, first) } else { mix(40.0, 0.0, second) }.to_radians(),
                },
            )
        }
        Play::Carousel => (full, Look::Carousel { off: motion::rise(k), ground, persp: 1600.0 * board, radius: card_r }),
        Play::Morph { from, to } => {
            let e = motion::rise(k);
            let at = [0, 1, 2, 3].map(|i| mix(from[i] as f32, to[i] as f32, e).round() as i32);
            let fade = ((k - 0.5) / 0.5).clamp(0.0, 1.0);
            (at, Look::Morph { src: from.map(|v| v as f32), radius: mix(0.0, card_r, e), alpha: 1.0 - fade })
        }
    }
}

/// A card's offset from the middle to its picture: (x shift as a share of the width, scale, turn in radians): the
/// Wechsel cards' `translateX(-off * 30px) rotateY(-off * 18deg) scale(.88 / 1.06)`, spread so one card leaves as the
/// next arrives.
#[cfg(feature = "gpu")]
pub fn card(off: f32) -> (f32, f32, f32) {
    let a = off.abs().min(1.0);
    (off * 0.94, mix(1.0, 0.88, a), (-off * 18.0).to_radians())
}

/// A picture for `Kind::Reveal`: premultiplied BGRA bytes, compared by `serial`.
#[derive(Debug)]
pub struct Image {
    pub serial: u64,
    pub w: i32,
    pub h: i32,
    pub bytes: Vec<u8>,
}

impl PartialEq for Image {
    fn eq(&self, o: &Image) -> bool {
        self.serial == o.serial && self.bytes.len() == o.bytes.len()
    }
}

/// The title's reveal under way (Boot's `tA`, 1.8 s): letter-spacing .7em to -.01em, opacity, blur(14px).
#[derive(Clone, Debug)]
pub struct Title {
    pub image: Rc<Image>,
    pub since: Duration,
    pub hold: Option<f32>,
}

pub const TITLE_LEN: Duration = Duration::from_millis(1800);

impl Title {
    pub fn progress(&self, now: Duration) -> Option<f32> {
        if let Some(k) = self.hold {
            return Some(k);
        }
        let k = now.saturating_sub(self.since).as_secs_f32() / TITLE_LEN.as_secs_f32();
        (k < 1.0).then_some(k)
    }
}

/// (sigma px, alpha, horizontal stretch) at progress `k`. The word is one picture, so its letter spacing is a stretch:
/// under the blur of the first half nobody can tell them apart, and in the second half it has nearly ended.
pub fn title_look(k: f32, board: f32) -> (f32, f32, f32) {
    let e = motion::rise(k);
    (14.0 * board * (1.0 - e), e, mix(1.35, 1.0, e))
}

#[cfg(test)]
mod tests {
    use super::*;

    const G: Rgba = [0.0, 0.0, 0.0, 1.0];

    #[test]
    fn the_wipe_starts_closed_and_ends_past_every_corner() {
        let (_, l0) = look(Play::Wipe, 0.0, (2560, 1440), 4.0 / 3.0, G, 37.0);
        let (_, l1) = look(Play::Wipe, 1.0, (2560, 1440), 4.0 / 3.0, G, 37.0);
        let Look::Wipe { r, cx, cy } = l0 else { panic!() };
        assert!(r < 0.5 && cx == 1280.0 && (cy - 806.4).abs() < 0.1);
        let Look::Wipe { r, cx, cy } = l1 else { panic!() };
        let far = (cx * cx + cy * cy).sqrt();
        assert!(r > far, "the last frame of the wipe hides nothing of the new scene ({r} > {far})");
    }

    #[test]
    fn the_glitch_settles_on_the_plain_picture() {
        let (_, l) = look(Play::Glitch, 0.0, (100, 100), 1.0, G, 0.0);
        let Look::Glitch { alpha, blur, dx, .. } = l else { panic!() };
        assert!(alpha < 1e-3 && (blur - 10.0).abs() < 1e-3 && (dx + 40.0).abs() < 1e-3);
        let (_, l) = look(Play::Glitch, 0.3, (100, 100), 1.0, G, 0.0);
        let Look::Glitch { alpha, hue, .. } = l else { panic!() };
        assert!((alpha - 0.6).abs() < 1e-3 && (hue - 40f32.to_radians()).abs() < 1e-3, "the 30 % keyframe");
        let (_, l) = look(Play::Glitch, 1.0, (100, 100), 1.0, G, 0.0);
        let Look::Glitch { alpha, blur, dx, skew, hue } = l else { panic!() };
        assert!(alpha > 0.999 && blur < 1e-2 && dx.abs() < 1e-2 && skew.abs() < 1e-3 && hue.abs() < 1e-3);
    }

    #[test]
    fn the_morph_flies_from_its_rectangle_to_the_other_and_fades_late() {
        let m = Play::Morph { from: [0, 0, 100, 100], to: [200, 100, 400, 300] };
        let (at, l) = look(m, 0.0, (800, 600), 1.0, G, 20.0);
        assert_eq!(at, [0, 0, 100, 100]);
        let Look::Morph { alpha, .. } = l else { panic!() };
        assert_eq!(alpha, 1.0);
        let (at, l) = look(m, 1.0, (800, 600), 1.0, G, 20.0);
        assert_eq!(at, [200, 100, 400, 300]);
        let Look::Morph { alpha, radius, .. } = l else { panic!() };
        assert!(alpha.abs() < 1e-3 && (radius - 20.0).abs() < 1e-2);
    }

    #[test]
    fn progress_ends_and_a_hold_stands_still() {
        let mut p = Playing { what: Play::Wipe, since: Duration::from_secs(1), serial: 1, hold: None };
        assert_eq!(p.progress(Duration::from_millis(1500)), Some(0.5));
        assert_eq!(p.progress(Duration::from_secs(2)), None);
        p.hold = Some(0.4);
        assert_eq!(p.progress(Duration::from_secs(99)), Some(0.4));
    }

    #[test]
    fn a_title_comes_out_of_its_blur() {
        let (s, a, st) = title_look(0.0, 1.0);
        assert!((s - 14.0).abs() < 1e-3 && a.abs() < 1e-3 && (st - 1.35).abs() < 1e-3);
        let (s, a, st) = title_look(1.0, 1.0);
        assert!(s.abs() < 1e-2 && (a - 1.0).abs() < 1e-3 && (st - 1.0).abs() < 1e-3);
    }
}
