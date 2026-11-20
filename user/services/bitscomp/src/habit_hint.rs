//! DB4: the one-time hint when BitsOS imitated a habit ("Brave startet maximiert, weil du Brave 3 von 3 Mal
//! maximiert hast") with its "Rückgängig", under the address bar like the notice (below it when both show). The
//! window gets its old state back and ctxd is told; that habit is never imitated again until Georg deletes it on
//! the page "Was BitsOS über dich gelernt hat". Geometry in logical px, shared by the look, the hit and `ctl hot`.

use std::time::Duration;

use crate::canvas::{Box2, Canvas};
use crate::chrome_draw::{Px, font};
use crate::layout::{Place, Rect};
use crate::render::{CompElement, CompRenderer};
use crate::state::App;
use crate::text::Weight;
use crate::theme::Theme;
use crate::win::Window;

pub const UNDO: &str = "Rückgängig";
/// Long enough to read it and decide; it never comes again for this habit.
pub const SHOWN_FOR: Duration = Duration::from_secs(20);
const H: i32 = 34;

/// What "Rückgängig" gives back.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Undo {
    /// Maximised; it was a tile before.
    Max { tiled: bool },
    Full,
    Width(f32),
    /// VORRANG: the program got its learned priority; Rückgängig sets normal back.
    Vorrang(crate::habit_vorrang::Target),
}

/// A hint waiting behind the one shown.
pub struct Waiting<W> {
    pub win: W,
    pub rule: crate::habit_app::Rule,
    pub undo: Undo,
}

/// Queues `w` behind the shown hint, once per habit.
pub fn wait<W>(q: &mut Vec<Waiting<W>>, w: Waiting<W>) {
    if !q.iter().any(|x| x.rule.id == w.rule.id) {
        q.push(w);
    }
}

/// The next waiting hint whose window is still there (`alive`); those whose window went are dropped.
pub fn next<W>(q: &mut Vec<Waiting<W>>, alive: impl Fn(&W) -> bool) -> Option<Waiting<W>> {
    while !q.is_empty() {
        let x = q.remove(0);
        if alive(&x.win) {
            return Some(x);
        }
    }
    None
}

pub struct Hint {
    pub win: Window,
    pub id: String,
    pub text: String,
    pub undo: Undo,
    pub until: Duration,
    pub hover: bool,
    image: Option<((String, Rect, bool, u64), smithay::backend::renderer::element::memory::MemoryRenderBuffer)>,
}

/// "Brave startet maximiert – weil du Brave 3 von 3 Mal maximiert hast".
pub fn text(title: &str, reason: &str) -> String {
    format!("{title} – {reason}")
}

/// The card and its button for `text`, below `under` (the address bar, or the notice under it).
pub fn boxes(under: Rect, text_w: i32, button_w: i32, max_w: i32) -> (Rect, Rect) {
    let w = (12 + text_w + 16 + button_w + 6).min(max_w);
    let card = Rect { x: under.x, y: under.y + under.h + 8, w, h: H };
    let button = Rect { x: card.x + w - 6 - button_w, y: card.y + 4, w: button_w, h: H - 8 };
    (card, button)
}

pub fn draw(text: &str, card: Rect, button: Rect, hover: bool, t: &Theme) -> Canvas {
    let p = Px::new(card, t.scale);
    let (w, h) = p.size();
    let mut c = Canvas::new(w, h);
    let all = Box2 { x: 0.0, y: 0.0, w: w as f32, h: h as f32 };
    c.rrect(all, p.v(8.0), t.panel2);
    c.rrect_line(all, p.v(8.0), p.line(), t.accent);
    let f = font(t, 13.0, Weight::Regular);
    let text_band = p.r(Rect { x: card.x + 12, y: card.y, w: button.x - card.x - 24, h: card.h });
    c.text_fit(&f, text_band, text, t.text);
    let b = p.b(button);
    if hover {
        c.rrect(b, p.v(6.0), t.chip);
    }
    c.rrect_line(b, p.v(6.0), p.line(), t.accent);
    c.text_mid(&font(t, 13.0, Weight::SemiBold), p.r(button), UNDO, t.accent);
    c
}

impl App {
    /// An imitation's hint: shown now, or after the one on screen (it is told to ctxd as shown only once it is).
    pub fn habit_hint_offer(&mut self, w: &Window, rule: crate::habit_app::Rule, undo: Undo) {
        match &self.habit.hint {
            Some(h) if h.id == rule.id => {}
            Some(_) => {
                eprintln!("bitscomp: habits: hint for {} waits behind the shown one", rule.id);
                wait(&mut self.habit.waiting, Waiting { win: w.clone(), rule, undo });
            }
            None => self.habit_hint_show(w, &rule, undo),
        }
    }

    pub fn habit_hint_show(&mut self, w: &Window, rule: &crate::habit_app::Rule, undo: Undo) {
        let text = text(&rule.title, &rule.reason);
        eprintln!("bitscomp: habits: hint shown for {}: {text}", rule.id);
        self.habit.hint = Some(Hint { win: w.clone(), id: rule.id.clone(), text, undo, until: crate::pacer::now() + SHOWN_FOR, hover: false, image: None });
        self.needs_frame = true;
        self.habit_event("hint_shown", vec![("id", mcpreg::Json::str(&rule.id))]);
        self.habit.hinted(&rule.id);
    }

    /// The shown hint ended: the next waiting one comes.
    pub fn habit_hint_next(&mut self) {
        let all = self.all_windows();
        if let Some(x) = next(&mut self.habit.waiting, |w| all.contains(w)) {
            self.habit_hint_show(&x.win, &x.rule, x.undo);
        }
    }

    /// Where the card and its button lie now (None: no hint).
    pub fn habit_hint_boxes(&self) -> Option<(Rect, Rect)> {
        let h = self.habit.hint.as_ref()?;
        let t = &self.theme;
        let a = self.chrome().address;
        let under = if self.desk.notice.is_some() { Rect { y: a.y + a.h + 8, h: 30, ..a } } else { a };
        let f = font(t, 13.0, Weight::Regular);
        let bf = font(t, 13.0, Weight::SemiBold);
        let tw = crate::chrome::logical_width(&f, &h.text, t.scale);
        let bw = crate::chrome::logical_width(&bf, UNDO, t.scale) + 24;
        Some(boxes(under, tw, bw, (self.area_full().w - a.x - 16).max(200)))
    }

    /// Drops the hint (and the Vorrang question) when its time is up; when the next one ends.
    pub fn habit_hint_tick(&mut self, now: Duration) -> Option<Duration> {
        let ask = self.vorrang_ask_tick(now);
        let hint = self.habit_hint_tick_own(now);
        hint.into_iter().chain(ask).min()
    }

    fn habit_hint_tick_own(&mut self, now: Duration) -> Option<Duration> {
        let until = self.habit.hint.as_ref()?.until;
        if now >= until {
            self.habit.hint = None;
            self.needs_frame = true;
            self.habit_hint_next();
            return self.habit.hint.as_ref().map(|h| h.until);
        }
        Some(until)
    }

    /// A press: on the button it undoes, anywhere on the card it is kept (true: the press was the hint's).
    pub fn habit_hint_press(&mut self) -> bool {
        if self.vorrang_ask_press() {
            return true;
        }
        let Some((card, button)) = self.habit_hint_boxes() else { return false };
        let (x, y) = (self.pointer.x, self.pointer.y);
        if button.contains(x, y) {
            self.habit_hint_undo();
            return true;
        }
        card.contains(x, y)
    }

    pub fn habit_hint_motion(&mut self) {
        self.vorrang_ask_motion();
        let Some((_, button)) = self.habit_hint_boxes() else { return };
        let on = button.contains(self.pointer.x, self.pointer.y);
        if let Some(h) = self.habit.hint.as_mut()
            && h.hover != on
        {
            h.hover = on;
            self.needs_frame = true;
        }
    }

    /// "Rückgängig": the window as it would have been, ctxd told, the hint gone.
    pub fn habit_hint_undo(&mut self) -> bool {
        let Some(h) = self.habit.hint.take() else { return false };
        let w = h.win.clone();
        if self.ws.find(&w).is_some() {
            match h.undo {
                Undo::Max { tiled } => {
                    if self.ws.is_max(&w) {
                        self.ws.toggle_max(&w, self.area_full());
                    }
                    if tiled && matches!(self.ws.find(&w), Some(Place::Floating { .. })) {
                        let area = self.area();
                        self.ws.toggle_float(&w, area);
                    }
                }
                Undo::Full => {
                    self.window_fullscreen_why(&w, false, "habit undone");
                }
                Undo::Width(old) => {
                    if let Some(Place::Tiled { tile, .. }) = self.ws.find(&w) {
                        self.ws.tiles[tile].weight = old;
                    }
                }
                Undo::Vorrang(t) => self.vorrang_undo(t, &h.id),
            }
            self.layout_dirty = true;
        }
        eprintln!("bitscomp: habits: {} undone (Rückgängig): not imitated again", h.id);
        self.habit_event("undo", vec![("id", mcpreg::Json::str(&h.id))]);
        self.habit.undone(&h.id);
        self.needs_frame = true;
        self.habit_hint_next();
        true
    }

    /// `ctl hot`'s line for the hint.
    pub fn habit_hint_hot(&self) -> String {
        let ask = self.vorrang_ask_hot();
        let (Some(h), Some((card, button))) = (self.habit.hint.as_ref(), self.habit_hint_boxes()) else { return ask };
        let s = self.theme.scale;
        let pt = |r: Rect| format!("{},{}", ((r.x as f64 + r.w as f64 / 2.0) * s).round(), ((r.y as f64 + r.h as f64 / 2.0) * s).round());
        format!("hint id={} card={} undo={} text={:?}\n{ask}", h.id, pt(card), pt(button), h.text)
    }

    pub fn habit_hint_elements<R: CompRenderer>(&mut self, renderer: &mut R) -> Vec<CompElement<R>> {
        let mut out = self.vorrang_ask_elements(renderer);
        out.extend(self.habit_hint_elements_own(renderer));
        out
    }

    fn habit_hint_elements_own<R: CompRenderer>(&mut self, renderer: &mut R) -> Vec<CompElement<R>> {
        let Some((card, button)) = self.habit_hint_boxes() else { return Vec::new() };
        let t = self.theme.clone();
        let tg = self.theme_gen;
        let Some(h) = self.habit.hint.as_mut() else { return Vec::new() };
        let look = (h.text.clone(), card, h.hover, tg);
        if h.image.as_ref().is_none_or(|(l, _)| *l != look) {
            h.image = Some((look, draw(&h.text, card, button, h.hover, &t).into_buffer()));
        }
        let buf = &h.image.as_ref().unwrap().1;
        let px = crate::header::px_rect(card, t.scale);
        crate::deco::image(renderer, buf, card, (px.w, px.h), t.scale).into_iter().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_button_sits_inside_the_card_at_its_right_end() {
        let a = Rect { x: 100, y: 10, w: 800, h: 40 };
        let (card, button) = boxes(a, 300, 100, 2000);
        assert_eq!((card.x, card.y, card.h), (100, 58, H));
        assert_eq!(button.x + button.w + 6, card.x + card.w);
        assert!(button.y > card.y && button.y + button.h < card.y + card.h);
        let (narrow, b) = boxes(a, 3000, 100, 600);
        assert_eq!(narrow.w, 600, "a long text is cut, the button stays");
        assert!(b.x > narrow.x);
        assert_eq!(text("A", "weil B"), "A – weil B");
    }

    #[test]
    fn a_second_hint_waits_once_and_comes_while_its_window_is_there() {
        let rule = |id: &str| crate::habit_app::Rule { id: id.into(), app: id.into(), state: None, width: None, hinted: false, title: String::new(), reason: String::new() };
        let mut q: Vec<Waiting<u32>> = Vec::new();
        wait(&mut q, Waiting { win: 1, rule: rule("fenster:steam"), undo: Undo::Full });
        wait(&mut q, Waiting { win: 2, rule: rule("fenster:steam"), undo: Undo::Full });
        wait(&mut q, Waiting { win: 3, rule: rule("fenster:discord"), undo: Undo::Max { tiled: true } });
        assert_eq!(q.len(), 2, "once per habit");
        let n = next(&mut q, |w| *w != 1).expect("the window of the first one went, the second comes");
        assert_eq!((n.win, n.rule.id.as_str()), (3, "fenster:discord"));
        assert!(next(&mut q, |_| true).is_none());
    }

    #[test]
    fn the_card_draws_its_outline_in_the_accent() {
        let t = crate::theme::test_theme();
        let card = Rect { x: 0, y: 0, w: 400, h: H };
        let (_, button) = boxes(Rect { x: 0, y: -48, w: 400, h: 40 }, 200, 100, 400);
        let c = draw("Brave startet maximiert – weil du Brave 3 von 3 Mal maximiert hast", card, button, false, t);
        let accent = crate::canvas::pack(t.accent);
        let p = Px::new(card, t.scale);
        let (w, h) = p.size();
        assert!((0..w).any(|x| c.pixel(x, h / 2) == accent) || (0..h).any(|y| c.pixel(1, y) == accent));
    }
}
