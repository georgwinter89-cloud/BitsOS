//! WY-19: the full effect level of `theme.effects` (plan/notes/wy-design-v2-scout.md §6b). At `gpu`, on a renderer
//! with shaders, the frame gets extra render elements: live backdrop blur behind glass, soft drop shadows and glows
//! (19a), the ambient motion (19b) and the transitions (19c). `cheap` draws none of them: the rescue build, Pixman and
//! a software rasteriser stay on the images the level has always drawn, and an idle desktop draws no frames.
//!
//! This module is renderer-neutral and in every build: which effects a frame shows, as data (`Kind`), in one element
//! type (`FxElement`) that hands its drawing to the renderer (`CompRenderer::fx_draw`). The shaders are in `fx_gl`.
//!
//! Damage. The tracker draws one element at a time and only inside the damage, so a blur cannot read the frame it is
//! part of. `seal` gives every glass a stamp of what lies under it (each element's id, commit, place and alpha);
//! when the stamp changes the glass is damaged whole, and the renderer redraws the blur from those elements before the
//! frame (`CompRenderer::fx_prepare`). A desktop whose wallpaper is all that lies under its glass blurs once.

pub mod ctl;
pub mod motion;
pub mod plan;
pub mod trans;

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::rc::Rc;
use std::time::Duration;

use smithay::backend::renderer::element::{Element, Id, Kind as ElementKind, RenderElement};
use smithay::backend::renderer::utils::CommitCounter;
use smithay::utils::{Buffer, Physical, Rectangle, Scale};

use crate::render::{CompElement, CompRenderer};
use crate::theme::{Effects, Motion, Rgba};
use trans::{Look, Play, Playing, Title};

/// An effect's name and instance (a panel's part, a window's surface): stays the same over the frames.
pub type Key = (&'static str, u64);

/// What one element draws. Coordinates are physical px relative to the element's rectangle; colours premultiplied.
#[derive(Clone, Debug, PartialEq)]
pub enum Kind {
    /// CSS box-shadow: the box `bx` (x, y, w, h) blurred by `sigma`, not drawn inside `hole` (the panel itself).
    Shadow { bx: [f32; 4], radius: f32, sigma: f32, color: Rgba, hole: [f32; 4], hole_radius: f32 },
    /// CSS backdrop-filter: blur(sigma) saturate(saturate), clipped to the rounded rectangle.
    Glass { radius: f32, sigma: f32, saturate: f32 },
    /// The element above this one, blurred under itself (text-shadow, the stroke glow): `rect` reaches 3 sigma around it.
    Halo { sigma: f32, gain: f32 },
    /// `.beam`: the band's gradient, `peak` its strongest alpha, `color` straight.
    Beam { color: [f32; 3], peak: f32, mid: f32 },
    /// `.scan`: one row every `period` rows.
    Scan { period: f32, color: Rgba },
    /// The map's disc with its rings and cross, and `.radar`'s conic sweep turned by `angle`.
    Radar { angle: f32, accent: [f32; 3], ground: Rgba },
    /// A dashed ring around the rectangle's middle: on/off along the arc, `phase` turns, with its stroke glow.
    Ring { r: f32, width: f32, on: f32, off: f32, phase: f32, color: Rgba, glow: f32 },
    /// A disc (`stroke` 0) or a circle line around the rectangle's middle, with a glow.
    Dot { r: f32, stroke: f32, color: Rgba, glow: f32 },
    /// One stretch of a flowing dashed thread.
    Flow { a: [f32; 2], b: [f32; 2], width: f32, on: f32, off: f32, offset: f32, color: Rgba, glow: f32 },
    /// A transition: the last frame on the screen under `look`.
    Snapshot { serial: u64, look: Look },
    /// A picture coming out of a blur (the title's reveal).
    Reveal { image: Rc<trans::Image>, sigma: f32, alpha: f32 },
}

/// One effect in the frame's element list.
#[derive(Clone, Debug)]
pub struct FxElement {
    id: Id,
    commit: CommitCounter,
    pub key: Key,
    pub rect: Rectangle<i32, Physical>,
    pub kind: Kind,
    /// What lies under a glass or in a halo's source (`seal`): the renderer's cached blur is good while it stays.
    pub under: u64,
}

impl Element for FxElement {
    fn id(&self) -> &Id {
        &self.id
    }
    fn current_commit(&self) -> CommitCounter {
        self.commit
    }
    fn src(&self) -> Rectangle<f64, Buffer> {
        Rectangle::from_size((self.rect.size.w as f64, self.rect.size.h as f64).into())
    }
    fn geometry(&self, _scale: Scale<f64>) -> Rectangle<i32, Physical> {
        self.rect
    }
    fn kind(&self) -> ElementKind {
        ElementKind::Unspecified
    }
}

impl<R: CompRenderer> RenderElement<R> for FxElement {
    fn draw(
        &self,
        frame: &mut R::Frame<'_, '_>,
        _src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        _opaque: &[Rectangle<i32, Physical>],
    ) -> Result<(), R::Error> {
        R::fx_draw(frame, self, dst, damage)
    }
}

/// An effect's identity over the frames.
struct Slot {
    id: Id,
    commit: CommitCounter,
    kind: Kind,
    size: (i32, i32),
    under: u64,
    frame: u64,
}

/// `ctl fx wait MS`: cuts the renderer's bounded wait for a frame's GPU work to this many ms (a test makes frames late
/// with it, on purpose; no wait is ever longer than the renderer's own bound). A relaxed load per frame.
pub static GPU_WAIT_CUT_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(u64::MAX);

/// The test and measuring hook `ctl fx demo`: the primitives no screen uses yet (threads, a pinging or pulsing dot).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Demo {
    Pulse([i32; 4]),
    Ping([i32; 4]),
    Flow([i32; 4]),
}

/// The effect level's state in the App.
#[derive(Default)]
pub struct Fx {
    slots: HashMap<Key, Slot>,
    frame: u64,
    /// The next ambient step's frame; set by a frame that draws moving effects, taken by `tick`.
    next: Option<Duration>,
    epoch: Option<Duration>,
    /// `--fx-on-software` / `ctl fx software on`: draw the level on a software rasteriser too (tests, measuring).
    pub on_software: bool,
    /// `ctl fx freeze T`: the ambient clocks stand at T seconds, no frame is asked for them.
    pub frozen: Option<f64>,
    /// Why the chosen gpu level is not drawn, as last said (once per reason).
    pub why_cheap: Option<String>,
    pub playing: Option<Playing>,
    serial: u64,
    pub title: Option<Title>,
    pub demo: Vec<Demo>,
    /// How many effect elements the last frame had.
    pub elements: usize,
}

impl Fx {
    /// Seconds on the ambient clocks.
    pub fn time(&mut self, now: Duration) -> f64 {
        if let Some(t) = self.frozen {
            return t;
        }
        now.saturating_sub(*self.epoch.get_or_insert(now)).as_secs_f64()
    }

    /// The element for `key`: a new commit when what it draws changed.
    pub fn element(&mut self, key: Key, rect: [i32; 4], kind: Kind) -> FxElement {
        let frame = self.frame;
        let size = (rect[2], rect[3]);
        let slot = self.slots.entry(key).or_insert_with(|| Slot {
            id: Id::new(),
            commit: CommitCounter::default(),
            kind: kind.clone(),
            size,
            under: 0,
            frame,
        });
        if slot.kind != kind || slot.size != size {
            slot.commit.increment();
            slot.kind = kind.clone();
            slot.size = size;
        }
        slot.frame = frame;
        FxElement {
            id: slot.id.clone(),
            commit: slot.commit,
            key,
            rect: Rectangle::new((rect[0], rect[1]).into(), (rect[2], rect[3]).into()),
            kind,
            under: 0,
        }
    }

    /// A frame drew moving effects: the next step is due after `step`.
    pub fn shown(&mut self, now: Duration, step: Duration) {
        self.next = Some(now + step);
    }

    /// The next step's time, for the loop's sleep.
    pub fn due(&self) -> Option<Duration> {
        self.next
    }

    /// True once when the next step is due; the frame it asks for calls `shown` again if anything still moves.
    pub fn tick(&mut self, now: Duration) -> bool {
        if self.next.is_some_and(|n| now >= n) {
            self.next = None;
            return true;
        }
        false
    }

    /// Stops asking for frames (level off, frozen, nothing moves).
    pub fn still(&mut self) {
        self.next = None;
    }

    /// Starts a transition; the renderer takes the snapshot before the next frame.
    pub fn start(&mut self, what: Play, now: Duration) {
        self.serial += 1;
        self.playing = Some(Playing { what, since: now, serial: self.serial, hold: None });
    }

    /// The next serial number for a picture (`trans::Image`).
    pub fn next_serial(&mut self) -> u64 {
        self.serial += 1;
        self.serial
    }

    /// Whether any effect is on screen or was on the last frame (`seal`'s quick way out).
    fn idle(&self) -> bool {
        self.slots.is_empty()
    }
}

fn stamp_element<R: CompRenderer>(e: &CompElement<R>, s: Scale<f64>, h: &mut std::collections::hash_map::DefaultHasher, place: bool) {
    e.id().hash(h);
    e.current_commit().distance(Some(CommitCounter::default())).hash(h);
    let g = e.geometry(s);
    (g.size.w, g.size.h).hash(h);
    if place {
        (g.loc.x, g.loc.y).hash(h);
    }
    e.alpha().to_bits().hash(h);
}

/// After all elements are built: what lies under each glass (and in each halo) goes into its stamp, the bottom ones
/// first so a glass's new commit reaches the glass above it; a changed stamp damages the glass whole. Effects no
/// element uses any more are forgotten.
pub fn seal<R: CompRenderer>(fx: &mut Fx, out: &mut [CompElement<R>], s: Scale<f64>) {
    if fx.idle() {
        fx.elements = 0;
        return;
    }
    let mut n = 0;
    for i in (0..out.len()).rev() {
        let (head, tail) = out.split_at_mut(i);
        let Some((CompElement::Fx(f), below)) = tail.split_first_mut() else { continue };
        n += 1;
        let mut h = std::collections::hash_map::DefaultHasher::new();
        match f.kind {
            Kind::Glass { .. } => {
                (f.rect.loc.x, f.rect.loc.y, f.rect.size.w, f.rect.size.h).hash(&mut h);
                // The beam goes behind a glass unseen: its every step would be a new blur.
                for e in below.iter().filter(|e| e.geometry(s).overlaps(f.rect) && !e.is_beam()) {
                    stamp_element(e, s, &mut h, true);
                }
            }
            Kind::Halo { .. } => {
                if let Some(e) = head.last() {
                    stamp_element(e, s, &mut h, false);
                }
            }
            _ => continue,
        }
        let stamp = h.finish();
        f.under = stamp;
        if let Some(slot) = fx.slots.get_mut(&f.key) {
            if slot.under != stamp {
                slot.under = stamp;
                slot.commit.increment();
            }
            f.commit = slot.commit;
        }
    }
    fx.elements = n;
    let frame = fx.frame;
    fx.slots.retain(|_, slot| slot.frame == frame);
    fx.frame += 1;
}

impl crate::state::App {
    /// Follows what the renderer can do, before each frame while the level is chosen or still drawn: `can` is why it
    /// cannot (said once per reason). The level is live: the cached images follow the new theme generation.
    pub fn fx_follow(&mut self, can: Result<(), String>) {
        let chosen = self.theme.effects == Effects::Gpu;
        let draw = chosen && can.is_ok();
        match can {
            Err(why) if chosen && self.fx.why_cheap.as_deref() != Some(&why) => {
                eprintln!("bitscomp: effects: gpu level chosen, drawing the cheap level: {why}");
                self.fx.why_cheap = Some(why);
            }
            Ok(()) => self.fx.why_cheap = None,
            Err(_) => {}
        }
        if self.theme.effects_gpu == draw {
            return;
        }
        eprintln!("bitscomp: effects: drawing the {} level", if draw { "gpu" } else { "cheap" });
        let mut t = (*self.theme).clone();
        t.effects_gpu = draw;
        self.theme = Rc::new(t);
        self.theme_gen += 1;
        self.deco_img.clear();
        self.corner_img.clear();
        self.menu_img = None;
        self.hung_img.clear();
        if !draw {
            self.fx.playing = None;
            self.fx.title = None;
            self.fx.still();
        }
        self.chrome_dirty();
    }

    /// A transition, if this level plays them: `theme.motion` off has none, calm only the fade.
    pub fn fx_play(&mut self, what: Play) {
        if !self.theme.effects_gpu || self.theme.motion == Motion::Off {
            return;
        }
        let what = if self.theme.motion == Motion::Calm { Play::Fade } else { what };
        self.fx.start(what, crate::pacer::now());
        self.needs_frame = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use smithay::backend::renderer::element::Kind as K;
    use smithay::backend::renderer::element::solid::{SolidColorBuffer, SolidColorRenderElement};
    use smithay::backend::renderer::pixman::PixmanRenderer;

    type E = CompElement<PixmanRenderer>;

    fn solid(buf: &SolidColorBuffer, x: i32) -> E {
        SolidColorRenderElement::from_buffer(buf, (x, 0), 1.0, 1.0, K::Unspecified).into()
    }

    fn glass(fx: &mut Fx) -> E {
        fx.element(("glass", 0), [0, 0, 10, 10], Kind::Glass { radius: 2.0, sigma: 4.0, saturate: 1.25 }).into()
    }

    fn sealed(fx: &mut Fx, mut v: Vec<E>) -> (u64, CommitCounter) {
        seal(fx, &mut v, Scale::from(1.0));
        let CompElement::Fx(f) = &v[0] else { panic!() };
        (f.under, f.commit)
    }

    #[test]
    fn a_glass_follows_only_what_lies_under_it() {
        let mut fx = Fx::default();
        let mut under = SolidColorBuffer::new((10, 10), [1.0, 0.0, 0.0, 1.0]);
        let beside = SolidColorBuffer::new((10, 10), [0.0, 1.0, 0.0, 1.0]);
        let g0 = glass(&mut fx);
        let (s0, c0) = sealed(&mut fx, vec![g0, solid(&under, 0), solid(&beside, 50)]);
        assert_ne!(s0, 0);
        let g = glass(&mut fx);
        let (s1, c1) = sealed(&mut fx, vec![g, solid(&under, 0), solid(&beside, 50)]);
        assert_eq!((s1, c1), (s0, c0), "nothing changed: no new commit, the kept blur is good");
        let mut other = beside.clone();
        other.set_color([0.0, 0.0, 1.0, 1.0]);
        let g = glass(&mut fx);
        let (s2, c2) = sealed(&mut fx, vec![g, solid(&under, 0), solid(&other, 50)]);
        assert_eq!((s2, c2), (s1, c1), "a change beside the glass is none of its business");
        under.set_color([0.0, 1.0, 1.0, 1.0]);
        let g = glass(&mut fx);
        let (s3, c3) = sealed(&mut fx, vec![g, solid(&under, 0), solid(&other, 50)]);
        assert_ne!(s3, s2);
        assert_ne!(c3, c2, "a change under the glass damages it");
        let g = glass(&mut fx);
        let (s4, c4) = sealed(&mut fx, vec![g, solid(&under, 3), solid(&other, 50)]);
        assert!(s4 != s3 && c4 != c3, "so does a window moving under it");
    }

    #[test]
    fn what_changes_gets_a_new_commit_and_old_effects_are_forgotten() {
        let mut fx = Fx::default();
        let dot = |r: f32| Kind::Dot { r, stroke: 0.0, color: [1.0; 4], glow: 0.0 };
        let a = fx.element(("dot", 1), [0, 0, 8, 8], dot(3.0));
        let id = a.id().clone();
        let c0 = a.current_commit();
        let mut v: Vec<E> = vec![a.into()];
        seal(&mut fx, &mut v, Scale::from(1.0));
        assert_eq!(fx.elements, 1);
        let moved = fx.element(("dot", 1), [5, 5, 8, 8], dot(3.0));
        assert_eq!((moved.id(), moved.current_commit()), (&id, c0), "moving is the tracker's: same element, no commit");
        let mut v: Vec<E> = vec![moved.into()];
        seal(&mut fx, &mut v, Scale::from(1.0));
        let grown = fx.element(("dot", 1), [5, 5, 8, 8], dot(4.0));
        assert_ne!(grown.current_commit(), c0, "another picture: a new commit");
        let mut v: Vec<E> = vec![grown.into()];
        seal(&mut fx, &mut v, Scale::from(1.0));
        let mut none: Vec<E> = Vec::new();
        seal(&mut fx, &mut none, Scale::from(1.0));
        assert!(fx.idle(), "no element any more: nothing kept");
        assert_eq!(fx.element(("dot", 1), [0, 0, 8, 8], dot(3.0)).id() == &id, false, "a later one is a new element");
    }

    #[test]
    fn the_clock_asks_for_one_frame_at_a_time_and_nothing_when_still() {
        let mut fx = Fx::default();
        let ms = Duration::from_millis;
        assert_eq!(fx.due(), None);
        fx.shown(ms(100), ms(16));
        assert_eq!(fx.due(), Some(ms(116)));
        assert!(!fx.tick(ms(110)));
        assert!(fx.tick(ms(116)));
        assert_eq!(fx.due(), None, "until the frame it asked for shows moving effects again");
        assert!(!fx.tick(ms(120)));
        fx.shown(ms(120), ms(16));
        fx.still();
        assert_eq!(fx.due(), None);
    }

    #[test]
    fn frozen_clocks_stand_still() {
        let mut fx = Fx::default();
        assert_eq!(fx.time(Duration::from_secs(5)), 0.0);
        assert_eq!(fx.time(Duration::from_secs(7)), 2.0);
        fx.frozen = Some(1.5);
        assert_eq!(fx.time(Duration::from_secs(9)), 1.5);
    }
}
