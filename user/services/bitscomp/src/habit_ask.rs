//! VORRANG: the question "Soll Blender immer mit hohem Vorrang starten?" with "Ja" and "Nein", under the address
//! bar like the habit hint (below it when both show), when a program whose learned Vorrang is hoch starts. A high
//! priority takes time from everything else, so BitsOS never sets it by itself: only Georg's own yes counts,
//! by the sidebar's rule (`assist_card::Hand`): the press and the pointer's last move from a real device, and
//! only `ARM` after the question appeared, moved (a notice or the hint above it came or went), or injected input
//! last touched it: a click meant for what was there a moment before never says yes. Injected input (the control socket, MCP, ui-act) can say neither yes
//! nor no for him: its "Ja" is refused with a notice, its "Nein" only closes the question for this start.

use std::time::Duration;

use crate::assist::{ARM, Hand};
use crate::canvas::{Box2, Canvas};
use crate::chrome_draw::{Px, font};
use crate::habit_vorrang::{Target, VRule};
use crate::layout::Rect;
use crate::render::{CompElement, CompRenderer};
use crate::state::App;
use crate::text::Weight;
use crate::theme::Theme;
use crate::win::Window;

pub const YES: &str = "Ja";
pub const NO: &str = "Nein";
/// What injected input gets instead of a yes.
pub const REAL_ONLY: &str = "Hohen Vorrang gibt es nur mit deiner echten Maus: nichts geändert.";
/// Unanswered, the question goes after this and comes again at the program's next start.
pub const SHOWN_FOR: Duration = Duration::from_secs(60);
const H: i32 = 34;

pub struct Ask {
    pub win: Window,
    pub id: String,
    pub app: String,
    pub name: String,
    pub text: String,
    pub target: Target,
    /// When it appeared, moved or injected input last touched it: "Ja" counts `ARM` later.
    pub since: Duration,
    /// Where card, "Ja" and "Nein" lay when last looked at (`settle`).
    pub at: Option<(Rect, Rect, Rect)>,
    pub until: Duration,
    /// 1: over "Ja", 2: over "Nein".
    pub hover: u8,
    image: Option<((String, Rect, u8, u64), smithay::backend::renderer::element::memory::MemoryRenderBuffer)>,
}

/// "Soll Blender immer mit hohem Vorrang starten? – weil du das 3 von 3 Mal so eingestellt hast".
pub fn text(name: &str, reason: &str) -> String {
    format!("Soll {name} immer mit hohem Vorrang starten? – {reason}")
}

/// The card, "Ja" and "Nein" for `text`, below `under`.
pub fn boxes(under: Rect, text_w: i32, yes_w: i32, no_w: i32, max_w: i32) -> (Rect, Rect, Rect) {
    let w = (12 + text_w + 16 + yes_w + 6 + no_w + 6).min(max_w);
    let card = Rect { x: under.x, y: under.y + under.h + 8, w, h: H };
    let no = Rect { x: card.x + w - 6 - no_w, y: card.y + 4, w: no_w, h: H - 8 };
    let yes = Rect { x: no.x - 6 - yes_w, w: yes_w, ..no };
    (card, yes, no)
}

/// Whether a press answers for Georg: his own hand, and the question was there long enough to be read.
pub fn counts(hand: Hand, since: Duration, now: Duration) -> bool {
    hand.real() && now >= since + ARM
}

/// The question's boxes now: when they differ from where it lay before, it moved and "Ja" is unarmed again
/// (true: it moved). The first look only notes them: the question appeared at `since`.
pub fn settle(at: &mut Option<(Rect, Rect, Rect)>, now_at: (Rect, Rect, Rect), since: &mut Duration, now: Duration) -> bool {
    let moved = at.is_some_and(|b| b != now_at);
    if moved {
        *since = now;
    }
    *at = Some(now_at);
    moved
}

/// What a press on "Ja" or "Nein" does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Answer {
    /// Georg's yes: hoch now and at every start (ctxd keeps the yes).
    Yes,
    /// Georg's no: never asked again until he deletes it on the page.
    No,
    /// His hand, but before the question could be read: nothing happens, the question stays.
    Wait,
    /// Injected input's yes: refused with a notice, the question stays.
    Refused,
    /// Injected input's no: the question goes for this start only, nothing is kept.
    Closed,
}

pub fn answer(yes: bool, hand: Hand, since: Duration, now: Duration) -> Answer {
    match (yes, counts(hand, since, now), hand.real()) {
        (true, true, _) => Answer::Yes,
        (false, true, _) => Answer::No,
        (_, false, true) => Answer::Wait,
        (true, false, false) => Answer::Refused,
        (false, false, false) => Answer::Closed,
    }
}

pub fn draw(text: &str, card: Rect, yes: Rect, no: Rect, hover: u8, t: &Theme) -> Canvas {
    let p = Px::new(card, t.scale);
    let (w, h) = p.size();
    let mut c = Canvas::new(w, h);
    let all = Box2 { x: 0.0, y: 0.0, w: w as f32, h: h as f32 };
    c.rrect(all, p.v(8.0), t.panel2);
    c.rrect_line(all, p.v(8.0), p.line(), t.accent);
    let band = p.r(Rect { x: card.x + 12, y: card.y, w: yes.x - card.x - 24, h: card.h });
    c.text_fit(&font(t, 13.0, Weight::Regular), band, text, t.text);
    for (i, (r, label)) in [(yes, YES), (no, NO)].into_iter().enumerate() {
        let b = p.b(r);
        if hover == i as u8 + 1 {
            c.rrect(b, p.v(6.0), t.chip);
        }
        c.rrect_line(b, p.v(6.0), p.line(), t.accent);
        c.text_mid(&font(t, 13.0, Weight::SemiBold), p.r(r), label, t.accent);
    }
    c
}

impl App {
    /// A program with a learned hoch starts: the question (one at a time; a second waits for its next start).
    pub fn vorrang_ask(&mut self, w: &Window, rule: &VRule, target: Target, name: &str) {
        if self.habit.ask.is_some() {
            eprintln!("bitscomp: habits: question for {} not shown (one is open)", rule.id);
            return;
        }
        let now = crate::pacer::now();
        let text = text(name, &rule.reason);
        eprintln!("bitscomp: habits: asked: {text}");
        self.habit.ask = Some(Ask { win: w.clone(), id: rule.id.clone(), app: rule.app.clone(), name: name.into(), text, target, since: now, at: None, until: now + SHOWN_FOR, hover: 0, image: None });
        // Where it lies from the start: any later move makes "Ja" wait again.
        self.vorrang_ask_settle();
        self.needs_frame = true;
    }

    pub fn vorrang_ask_boxes(&self) -> Option<(Rect, Rect, Rect)> {
        let a = self.habit.ask.as_ref()?;
        let t = &self.theme;
        let bar = self.chrome().address;
        let mut under = if self.desk.notice.is_some() { Rect { y: bar.y + bar.h + 8, h: 30, ..bar } } else { bar };
        if let Some((card, _)) = self.habit_hint_boxes() {
            under = Rect { y: card.y, h: card.h, ..under };
        }
        let bf = font(t, 13.0, Weight::SemiBold);
        let tw = crate::chrome::logical_width(&font(t, 13.0, Weight::Regular), &a.text, t.scale);
        let bw = |s: &str| crate::chrome::logical_width(&bf, s, t.scale) + 24;
        Some(boxes(under, tw, bw(YES), bw(NO), (self.area_full().w - bar.x - 16).max(240)))
    }

    /// Notes where the question lies now; when it moved, "Ja" waits `ARM` again.
    pub fn vorrang_ask_settle(&mut self) {
        let Some(b) = self.vorrang_ask_boxes() else { return };
        let now = crate::pacer::now();
        if let Some(a) = self.habit.ask.as_mut()
            && settle(&mut a.at, b, &mut a.since, now)
        {
            eprintln!("bitscomp: habits: question {} moved: Ja waits again", a.id);
        }
    }

    /// A press on the question (true: it was the question's).
    pub fn vorrang_ask_press(&mut self) -> bool {
        self.vorrang_ask_settle();
        let Some((card, yes, no)) = self.vorrang_ask_boxes() else { return false };
        let (x, y) = (self.pointer.x, self.pointer.y);
        let hand = Hand { press: self.input_origin, release: self.input_origin, moved: self.pointer_origin };
        if yes.contains(x, y) {
            self.vorrang_answer(true, hand);
        } else if no.contains(x, y) {
            self.vorrang_answer(false, hand);
        }
        let on = card.contains(x, y);
        // Injected input touched the question (its "Ja" refused, or the card itself): Georg's next click waits.
        if on && !hand.real()
            && let Some(a) = self.habit.ask.as_mut()
        {
            a.since = crate::pacer::now();
        }
        on
    }

    /// Georg's answer: yes sets hoch now and for every start after (ctxd keeps it), no declines for good.
    pub fn vorrang_answer(&mut self, yes: bool, hand: Hand) {
        let Some(a) = self.habit.ask.as_ref() else { return };
        let what = answer(yes, hand, a.since, crate::pacer::now());
        eprintln!("bitscomp: habits: {} {} ({hand:?}): {what:?}", a.id, if yes { YES } else { NO });
        match what {
            Answer::Wait => return,
            Answer::Refused => return self.notice(REAL_ONLY),
            _ => {}
        }
        let Some(a) = self.habit.ask.take() else { return };
        self.needs_frame = true;
        match what {
            Answer::Yes => {
                self.habit_event("vorrang_ja", vec![("app", mcpreg::Json::str(&a.app))]);
                let _ = self.vorrang_set(a.target, "hoch", &a.name);
                if let Some(r) = self.habit.vorrang.iter_mut().find(|r| r.id == a.id) {
                    (r.ask, r.hinted) = (false, true);
                }
            }
            Answer::No => {
                self.habit_event("undo", vec![("id", mcpreg::Json::str(&a.id)), ("nein", mcpreg::Json::Bool(true))]);
                self.habit.vorrang.retain(|r| r.id != a.id);
            }
            _ => return,
        }
        self.habit_refresh("vorrang answer");
    }

    pub fn vorrang_ask_motion(&mut self) {
        self.vorrang_ask_settle();
        let Some((_, yes, no)) = self.vorrang_ask_boxes() else { return };
        let (x, y) = (self.pointer.x, self.pointer.y);
        let on = if yes.contains(x, y) { 1 } else if no.contains(x, y) { 2 } else { 0 };
        if let Some(a) = self.habit.ask.as_mut()
            && a.hover != on
        {
            a.hover = on;
            self.needs_frame = true;
        }
    }

    /// Drops the question when its time is up or its window went; when it ends.
    pub fn vorrang_ask_tick(&mut self, now: Duration) -> Option<Duration> {
        let a = self.habit.ask.as_ref()?;
        if now >= a.until || !self.all_windows().contains(&a.win) {
            self.habit.ask = None;
            self.needs_frame = true;
            return None;
        }
        let until = a.until;
        self.vorrang_ask_settle();
        Some(until)
    }

    /// `ctl hot`'s line for the question.
    pub fn vorrang_ask_hot(&self) -> String {
        let (Some(a), Some((card, yes, no))) = (self.habit.ask.as_ref(), self.vorrang_ask_boxes()) else { return String::new() };
        let s = self.theme.scale;
        let pt = |r: Rect| format!("{},{}", ((r.x as f64 + r.w as f64 / 2.0) * s).round(), ((r.y as f64 + r.h as f64 / 2.0) * s).round());
        format!("ask id={} card={} yes={} no={} text={:?}\n", a.id, pt(card), pt(yes), pt(no), a.text)
    }

    pub fn vorrang_ask_elements<R: CompRenderer>(&mut self, renderer: &mut R) -> Vec<CompElement<R>> {
        self.vorrang_ask_settle();
        let Some((card, yes, no)) = self.vorrang_ask_boxes() else { return Vec::new() };
        let t = self.theme.clone();
        let tg = self.theme_gen;
        let Some(a) = self.habit.ask.as_mut() else { return Vec::new() };
        let look = (a.text.clone(), card, a.hover, tg);
        if a.image.as_ref().is_none_or(|(l, _)| *l != look) {
            a.image = Some((look, draw(&a.text, card, yes, no, a.hover, &t).into_buffer()));
        }
        let buf = &a.image.as_ref().unwrap().1;
        let px = crate::header::px_rect(card, t.scale);
        crate::deco::image(renderer, buf, card, (px.w, px.h), t.scale).into_iter().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::Origin::{Injected, Physical};

    #[test]
    fn yes_and_no_sit_inside_the_card_at_its_right_end() {
        let (card, yes, no) = boxes(Rect { x: 100, y: 10, w: 800, h: 40 }, 400, 50, 70, 2000);
        assert_eq!((card.y, card.h), (58, H));
        assert_eq!(no.x + no.w + 6, card.x + card.w);
        assert!(yes.x + yes.w < no.x && yes.x > card.x);
        assert_eq!(text("Blender", "weil du das 3 von 3 Mal so eingestellt hast"), "Soll Blender immer mit hohem Vorrang starten? – weil du das 3 von 3 Mal so eingestellt hast");
    }

    #[test]
    fn only_georgs_own_hand_after_the_question_was_readable_says_yes() {
        let at = Duration::from_secs(10);
        let late = at + ARM;
        assert!(counts(Hand::all(Physical), at, late));
        assert!(!counts(Hand::all(Physical), at, at + ARM / 2), "a click already on its way when it appeared");
        assert!(!counts(Hand::all(Injected), at, late), "the control socket, MCP, ui-act");
        assert!(!counts(Hand { press: Physical, release: Physical, moved: Injected }, at, late), "a warped pointer");
        assert_eq!(answer(true, Hand::all(Physical), at, late), Answer::Yes);
        assert_eq!(answer(false, Hand::all(Physical), at, late), Answer::No);
        assert_eq!(answer(true, Hand::all(Physical), at, at), Answer::Wait, "too early: the question stays");
        assert_eq!(answer(true, Hand::all(Injected), at, late), Answer::Refused, "ctl, MCP and ui-act cannot say yes");
        assert_eq!(answer(false, Hand::all(Injected), at, late), Answer::Closed, "nor no: nothing kept");
    }

    #[test]
    fn a_question_that_moved_waits_again_before_a_yes_counts() {
        let at = Duration::from_secs(10);
        let under = Rect { x: 100, y: 10, w: 800, h: 40 };
        let first = boxes(under, 400, 50, 70, 2000);
        let (mut seen, mut since) = (None, at);
        assert!(!settle(&mut seen, first, &mut since, at + ARM), "the first look only notes where it lies");
        assert_eq!(since, at);
        assert_eq!(answer(true, Hand::all(Physical), since, at + ARM), Answer::Yes, "readable since it appeared");
        // A notice came: the card went down by the notice's height, just before Georg's click.
        let moved = at + ARM * 2;
        let down = boxes(Rect { y: under.y + under.h + 8, h: 30, ..under }, 400, 50, 70, 2000);
        assert!(settle(&mut seen, down, &mut since, moved));
        assert_eq!(answer(true, Hand::all(Physical), since, moved + ARM / 2), Answer::Wait, "a click meant for what was there before");
        assert_eq!(answer(true, Hand::all(Physical), since, moved + ARM), Answer::Yes);
        assert!(!settle(&mut seen, down, &mut since, moved + ARM), "not moved again");
        assert_eq!(since, moved);
    }

    #[test]
    fn the_card_draws_its_outline_in_the_accent() {
        let t = crate::theme::test_theme();
        let (card, yes, no) = boxes(Rect { x: 0, y: -48, w: 600, h: 40 }, 300, 50, 70, 600);
        let c = draw("Soll Blender immer mit hohem Vorrang starten?", card, yes, no, 1, t);
        let accent = crate::canvas::pack(t.accent);
        let (w, h) = Px::new(card, t.scale).size();
        assert!((0..w).any(|x| c.pixel(x, h / 2) == accent) || (0..h).any(|y| c.pixel(1, y) == accent));
    }
}
