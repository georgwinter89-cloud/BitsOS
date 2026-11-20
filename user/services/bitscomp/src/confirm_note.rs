//! VB-1 (Georg 17.11.): after the start of an unconfirmed version the desktop asks "Version 168 läuft.
//! Bestätigen?" with "Ja" and "Zurück zur vorherigen", as a card under the address bar (below the notice, the
//! habit hint and the Vorrang question). bitspkgd's `update_status` says whether the kernel asks (`question`) and
//! whether it still may (`asks`); a thread of its own asks every `POLL` until it never will, so the card goes as
//! soon as a proof (MCP, the desktop) confirmed first. Only Georg's own hand counts, by the Vorrang question's rule
//! (`habit_ask`): press, release and the pointer's last move from a real device, and only `ARM` after the card
//! appeared or moved; injected input (MCP, the control socket, ui-act) is refused with a notice and changes
//! nothing. "Ja" sends `update_confirm`; "Zurück zur vorherigen" sends `update_back`, says on the screen which
//! version comes, then asks init to restart. bitspkgd and the kernel check again (the grant, real input).

use std::sync::mpsc;
use std::time::Duration;

use mcpreg::Json;
use smithay::reexports::calloop::LoopHandle;
use smithay::reexports::calloop::channel::{self, Event};

use crate::assist::Hand;
use crate::canvas::{Box2, Canvas};
use crate::chrome_draw::{Px, font};
use crate::layout::Rect;
use crate::pkg_client::{Job, Msg};
use crate::render::{CompElement, CompRenderer};
use crate::state::App;
use crate::text::Weight;
use crate::theme::Theme;

/// The button words (the console's `bitsupdate frage` says the same).
pub const YES: &str = "Ja";
pub const BACK: &str = "Zurück zur vorherigen";
/// What injected input gets.
pub const REAL_ONLY: &str = "Bestätigen und Zurück gehen nur mit deiner echten Maus: nichts geändert.";
/// A press counts this long after the card appeared or moved (the console's 2 s, bitsinstall's rule).
pub const ARM: Duration = Duration::from_millis(2000);
/// How often the kernel's status is asked while the question may still come.
pub const POLL: Duration = Duration::from_secs(3);
/// "Zurück zu Version M" stays this long before the restart is asked.
pub const RESTART_AFTER: Duration = Duration::from_secs(3);
/// The longest wait between two status questions while bitspkgd does not answer.
pub const POLL_MAX: Duration = Duration::from_secs(60);
const SAID_FOR: Duration = Duration::from_secs(12);
const H: i32 = 34;

#[derive(Clone, Debug, PartialEq)]
enum Cmd {
    Op(&'static str, String),
    Reboot,
}

#[derive(Clone, Debug)]
enum Back {
    Answer(&'static str, Json),
    Failed(&'static str, String),
    Power(String, bool),
}

/// The question as shown.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Q {
    pub version: u32,
    pub generation: u32,
    pub back: Option<u32>,
    pub text: String,
}

/// The question bitspkgd's `update_status` answer carries, and whether it may still come (pure).
pub fn parse(j: &Json) -> (Option<Q>, bool) {
    let asks = j.get("asks").and_then(Json::as_bool).unwrap_or(false);
    let q = j.get("question").filter(|q| !matches!(q, Json::Null)).and_then(|q| {
        let num = |k: &str| q.get(k).and_then(Json::as_u64).map(|v| v as u32);
        Some(Q { version: num("version")?, generation: num("generation")?, back: num("back"), text: q.get("text").and_then(Json::as_str)?.to_string() })
    });
    (q, asks)
}

/// The card, "Ja" and "Zurück" (when there is somewhere to go back to) for a text `text_w` wide, below `under`.
pub fn boxes(under: Rect, text_w: i32, yes_w: i32, back_w: Option<i32>, max_w: i32) -> (Rect, Rect, Option<Rect>) {
    let w = (12 + text_w + 16 + yes_w + 6 + back_w.map_or(0, |b| b + 6)).min(max_w);
    let card = Rect { x: under.x, y: under.y + under.h + 8, w, h: H };
    let right = card.x + w - 6;
    let back = back_w.map(|b| Rect { x: right - b, y: card.y + 4, w: b, h: H - 8 });
    let yes_x = back.map_or(right, |b| b.x - 6) - yes_w;
    (card, Rect { x: yes_x, y: card.y + 4, w: yes_w, h: H - 8 }, back)
}

/// What a press on a button does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Press {
    /// Georg's own hand, the card readable long enough: the answer goes out.
    Counts,
    /// His hand, but too early (a click meant for what lay there a moment before): nothing.
    Wait,
    /// Injected input: refused with a notice, nothing changes.
    Refused,
}

pub fn press(hand: Hand, since: Duration, now: Duration) -> Press {
    match (hand.real(), now >= since + ARM) {
        (true, true) => Press::Counts,
        (true, false) => Press::Wait,
        (false, _) => Press::Refused,
    }
}

/// The wait before the next status question after `misses` unanswered ones in a row (pure).
pub fn retry_after(misses: u32) -> Duration {
    POLL.saturating_mul(1u32 << misses.min(5)).min(POLL_MAX)
}

struct Shown {
    q: Q,
    since: Duration,
    at: Option<(Rect, Rect, Option<Rect>)>,
    /// 1: over "Ja", 2: over "Zurück".
    hover: u8,
    /// An answer is on its way: no buttons, the text says what happens.
    busy: Option<String>,
    image: Option<((String, Rect, u8, bool, u64), smithay::backend::renderer::element::memory::MemoryRenderBuffer)>,
}

#[derive(Default)]
pub struct Confirm {
    shown: Option<Shown>,
    ask: Option<mpsc::Sender<Cmd>>,
    /// The next status question (None: never again in this boot, or one is on its way).
    next: Option<Duration>,
    in_flight: bool,
    /// Status questions in a row that found no bitspkgd: the next waits longer (at most `POLL_MAX`).
    misses: u32,
    reboot_at: Option<Duration>,
    /// For `ctl bestaetigen`: the last thing said, and what injected input tried.
    said: String,
    refused: u32,
}

fn line(op: &str, extra: Vec<(&str, Json)>) -> String {
    let mut v = vec![("op", Json::str(op))];
    v.extend(extra);
    let mut s = String::new();
    Json::obj(v).write_to(&mut s);
    s
}

fn run(socket: &std::path::Path, op: &'static str, req: &str) -> Back {
    let wait = if op == "update_confirm" { Duration::from_secs(180) } else { Duration::from_secs(60) };
    let job = Job { id: 0, line: req.to_string(), stream: false, wait, idle: Duration::from_secs(300), local: None };
    match crate::pkg_client::run(socket, &job, &mut |_| true) {
        Some(Msg::End(_, j)) => Back::Answer(op, j),
        Some(Msg::Failed(_, why)) => Back::Failed(op, why),
        _ => Back::Failed(op, String::from("Die Programmverwaltung antwortet nicht.")),
    }
}

fn draw(text: &str, card: Rect, yes: Rect, back: Option<Rect>, hover: u8, buttons: bool, t: &Theme) -> Canvas {
    let p = Px::new(card, t.scale);
    let (w, h) = p.size();
    let mut c = Canvas::new(w, h);
    let all = Box2 { x: 0.0, y: 0.0, w: w as f32, h: h as f32 };
    c.rrect(all, p.v(8.0), t.panel2);
    c.rrect_line(all, p.v(8.0), p.line(), t.accent);
    let end = if buttons { yes.x - 12 } else { card.x + card.w - 12 };
    let band = p.r(Rect { x: card.x + 12, y: card.y, w: end - card.x - 12, h: card.h });
    c.text_fit(&font(t, 13.0, Weight::Regular), band, text, t.text);
    if buttons {
        let mut bs = vec![(yes, YES)];
        bs.extend(back.map(|b| (b, BACK)));
        for (i, (r, label)) in bs.into_iter().enumerate() {
            let b = p.b(r);
            if hover == i as u8 + 1 {
                c.rrect(b, p.v(6.0), t.chip);
            }
            c.rrect_line(b, p.v(6.0), p.line(), t.accent);
            c.text_mid(&font(t, 13.0, Weight::SemiBold), p.r(r), label, t.accent);
        }
    }
    c
}

impl App {
    /// At the desktop's start: the thread that talks to bitspkgd, and the first look.
    pub fn confirm_start(&mut self, handle: &LoopHandle<'static, App>, socket: String) {
        let (tx, rx) = channel::channel::<Back>();
        if let Err(e) = handle.insert_source(rx, |ev, _, app: &mut App| {
            if let Event::Msg(b) = ev {
                app.confirm_back(b);
            }
        }) {
            eprintln!("bitscomp: bestätigen: no channel ({e}); no question on this desktop");
            return;
        }
        let (atx, arx) = mpsc::channel::<Cmd>();
        let sock = std::path::PathBuf::from(socket);
        let spawned = std::thread::Builder::new().name("bitscomp-confirm".into()).spawn(move || {
            while let Ok(c) = arx.recv() {
                let b = match c {
                    Cmd::Op(op, req) => run(&sock, op, &req),
                    Cmd::Reboot => {
                        let (t, down) = crate::sofa_io::power_request(std::path::Path::new(crate::sofa_io::POWER_SOCKET), "reboot", true);
                        Back::Power(t, down)
                    }
                };
                if tx.send(b).is_err() {
                    return;
                }
            }
        });
        match spawned {
            Ok(_) => {
                self.confirm.ask = Some(atx);
                self.confirm.next = Some(crate::pacer::now());
            }
            Err(e) => eprintln!("bitscomp: bestätigen: no thread ({e}); no question on this desktop"),
        }
    }

    fn confirm_send(&mut self, cmd: Cmd) -> bool {
        self.confirm.ask.as_ref().is_some_and(|a| a.send(cmd).is_ok())
    }

    pub fn confirm_due(&self) -> Option<Duration> {
        let c = &self.confirm;
        [if c.in_flight { None } else { c.next }, c.reboot_at].into_iter().flatten().min()
    }

    pub fn confirm_tick(&mut self, now: Duration) {
        if self.confirm.reboot_at.is_some_and(|t| now >= t) {
            self.confirm.reboot_at = None;
            eprintln!("bitscomp: bestätigen: restart asked of init");
            self.confirm_send(Cmd::Reboot);
        }
        if !self.confirm.in_flight && self.confirm.next.is_some_and(|t| now >= t) {
            self.confirm.next = None;
            self.confirm.in_flight = self.confirm_send(Cmd::Op("update_status", line("update_status", vec![])));
        }
        self.confirm_settle();
    }

    fn confirm_say(&mut self, t: String) {
        eprintln!("bitscomp: bestätigen: {t}");
        self.confirm.said = t.clone();
        self.notice_for(t, SAID_FOR);
    }

    fn confirm_back(&mut self, b: Back) {
        let now = crate::pacer::now();
        match b {
            Back::Answer("update_status", j) => {
                self.confirm.in_flight = false;
                self.confirm.misses = 0;
                let before = self.confirm.shown.as_ref().map(|s| s.q.clone());
                let (q, asks) = parse(&j);
                let busy = self.confirm.shown.as_ref().is_some_and(|s| s.busy.is_some());
                if !busy {
                    match (q, self.confirm.shown.as_ref().map(|s| s.q.clone())) {
                        (Some(q), Some(old)) if q == old => {}
                        (Some(q), _) => {
                            eprintln!("bitscomp: bestätigen: asked: {} (generation {}, back {:?})", q.text, q.generation, q.back);
                            self.confirm.shown = Some(Shown { q, since: now, at: None, hover: 0, busy: None, image: None });
                            self.confirm_settle();
                        }
                        (None, Some(_)) => {
                            eprintln!("bitscomp: bestätigen: the question went (confirmed meanwhile or not asked any more)");
                            self.confirm.shown = None;
                        }
                        (None, None) => {}
                    }
                }
                // Every few seconds while the kernel may still ask; never again once it never will.
                self.confirm.next = asks.then_some(now + POLL);
                if !asks && !busy {
                    self.confirm.shown = None;
                }
                // A frame only when the card changed: the look every few seconds must leave a still desktop still.
                if self.confirm.shown.as_ref().map(|s| s.q.clone()) != before {
                    self.needs_frame = true;
                }
            }
            Back::Answer("update_confirm", j) => {
                self.confirm.shown = None;
                let msg = j.get("message").and_then(Json::as_str).unwrap_or("").to_string();
                if j.get("ok").and_then(Json::as_bool) == Some(true) {
                    eprintln!("bitscomp: bestätigen: Ja written (by {})", j.get("by").and_then(Json::as_str).unwrap_or("?"));
                } else {
                    eprintln!("bitscomp: bestätigen: Ja refused: {}", j.get("detail").and_then(Json::as_str).unwrap_or(""));
                }
                self.confirm_say(msg);
                self.confirm.next = Some(now);
                self.needs_frame = true;
            }
            Back::Answer("update_back", j) => {
                self.confirm.shown = None;
                if j.get("ok").and_then(Json::as_bool) == Some(true) {
                    let next = j.get("next").and_then(Json::as_str).unwrap_or("?").to_string();
                    self.confirm_say(format!("Zurück zu Version {next}: BitsOS startet in wenigen Sekunden neu."));
                    self.confirm.reboot_at = Some(now + RESTART_AFTER);
                } else {
                    eprintln!("bitscomp: bestätigen: Zurück refused: {}", j.get("detail").and_then(Json::as_str).unwrap_or(""));
                    self.confirm_say(String::from("Zurück hat nicht geklappt; es bleibt bei dieser Version."));
                    self.confirm.next = Some(now);
                }
                self.needs_frame = true;
            }
            Back::Answer(_, _) => {}
            Back::Failed(op, why) => {
                eprintln!("bitscomp: bestätigen: {op}: {why}");
                if op == "update_status" {
                    // Nothing on the screen changes: no frame (a still desktop stays still).
                    self.confirm.in_flight = false;
                    self.confirm.misses = self.confirm.misses.saturating_add(1);
                    self.confirm.next = Some(now + retry_after(self.confirm.misses));
                } else {
                    self.confirm.shown = None;
                    self.confirm_say(format!("{why} Nichts geändert."));
                    self.confirm.next = Some(now);
                    self.needs_frame = true;
                }
            }
            Back::Power(t, down) => {
                if !down {
                    self.confirm_say(t);
                }
            }
        }
    }

    pub fn confirm_boxes(&self) -> Option<(Rect, Rect, Option<Rect>)> {
        let s = self.confirm.shown.as_ref()?;
        let t = &self.theme;
        let bar = self.chrome().address;
        let mut under = if self.desk.notice.is_some() { Rect { y: bar.y + bar.h + 8, h: 30, ..bar } } else { bar };
        if let Some((card, _)) = self.habit_hint_boxes() {
            under = Rect { y: card.y, h: card.h, ..under };
        }
        if let Some((card, _, _)) = self.vorrang_ask_boxes() {
            under = Rect { y: card.y, h: card.h, ..under };
        }
        let bf = font(t, 13.0, Weight::SemiBold);
        let text = s.busy.as_deref().unwrap_or(&s.q.text);
        let tw = crate::chrome::logical_width(&font(t, 13.0, Weight::Regular), text, t.scale);
        let bw = |l: &str| crate::chrome::logical_width(&bf, l, t.scale) + 24;
        Some(boxes(under, tw, bw(YES), s.q.back.map(|_| bw(BACK)), (self.area_full().w - bar.x - 16).max(320)))
    }

    /// Notes where the card lies; when it moved, a press waits `ARM` again.
    fn confirm_settle(&mut self) {
        let Some(b) = self.confirm_boxes() else { return };
        let now = crate::pacer::now();
        if let Some(s) = self.confirm.shown.as_mut() {
            if s.at.is_some_and(|a| a != b) {
                s.since = now;
            }
            s.at = Some(b);
        }
    }

    /// A press of the pointer (true: it was on the card).
    pub fn confirm_press(&mut self) -> bool {
        self.confirm_settle();
        let Some((card, yes, back)) = self.confirm_boxes() else { return false };
        let (x, y) = (self.pointer.x, self.pointer.y);
        if !card.contains(x, y) {
            return false;
        }
        let Some(s) = self.confirm.shown.as_ref() else { return false };
        if s.busy.is_some() {
            return true;
        }
        let which = if yes.contains(x, y) { Some(true) } else if back.is_some_and(|b| b.contains(x, y)) { Some(false) } else { None };
        let hand = Hand { press: self.input_origin, release: self.input_origin, moved: self.pointer_origin };
        let now = crate::pacer::now();
        if !hand.real() {
            // Injected input touched the card: Georg's next click waits again.
            if let Some(s) = self.confirm.shown.as_mut() {
                s.since = now;
            }
            if which.is_some() {
                self.confirm.refused += 1;
                eprintln!("bitscomp: bestätigen: {} by injected input refused ({hand:?})", if which == Some(true) { YES } else { BACK });
                self.notice(REAL_ONLY);
            }
            return true;
        }
        let Some(yes_pressed) = which else { return true };
        match press(hand, s.since, now) {
            Press::Wait | Press::Refused => return true,
            Press::Counts => {}
        }
        let (g, v, to) = (s.q.generation, s.q.version, s.q.back);
        let (busy, cmd) = if yes_pressed {
            (format!("Version {v} wird bestätigt …"), Cmd::Op("update_confirm", line("update_confirm", vec![("generation", Json::Num(g as f64))])))
        } else {
            (format!("Zurück zu Version {} wird vorbereitet …", to.unwrap_or(0)), Cmd::Op("update_back", line("update_back", vec![])))
        };
        eprintln!("bitscomp: bestätigen: Georg pressed {} for generation {g}", if yes_pressed { YES } else { BACK });
        if self.confirm_send(cmd) {
            if let Some(s) = self.confirm.shown.as_mut() {
                s.busy = Some(busy);
            }
        } else {
            self.confirm_say(String::from("Die Programmverwaltung ist nicht verbunden: nichts geändert."));
        }
        self.needs_frame = true;
        true
    }

    pub fn confirm_motion(&mut self) {
        self.confirm_settle();
        let Some((_, yes, back)) = self.confirm_boxes() else { return };
        let (x, y) = (self.pointer.x, self.pointer.y);
        let on = if yes.contains(x, y) { 1 } else if back.is_some_and(|b| b.contains(x, y)) { 2 } else { 0 };
        if let Some(s) = self.confirm.shown.as_mut()
            && s.hover != on
        {
            s.hover = on;
            self.needs_frame = true;
        }
    }

    /// `ctl hot`'s line for the card (physical output pixels, as the other hot lines).
    pub fn confirm_hot(&self) -> String {
        let (Some(s), Some((card, yes, back))) = (self.confirm.shown.as_ref(), self.confirm_boxes()) else { return String::new() };
        let sc = self.theme.scale;
        let pt = |r: Rect| format!("{},{}", ((r.x as f64 + r.w as f64 / 2.0) * sc).round(), ((r.y as f64 + r.h as f64 / 2.0) * sc).round());
        format!("confirm version={} card={} ja={} zurueck={} busy={} text={:?}\n", s.q.version, pt(card), pt(yes), back.map_or(String::from("-"), pt), u8::from(s.busy.is_some()), s.busy.as_deref().unwrap_or(&s.q.text))
    }

    pub fn confirm_elements<R: CompRenderer>(&mut self, renderer: &mut R) -> Vec<CompElement<R>> {
        self.confirm_settle();
        let Some((card, yes, back)) = self.confirm_boxes() else { return Vec::new() };
        let t = self.theme.clone();
        let tg = self.theme_gen;
        let Some(s) = self.confirm.shown.as_mut() else { return Vec::new() };
        let text = s.busy.clone().unwrap_or_else(|| s.q.text.clone());
        let buttons = s.busy.is_none();
        let look = (text.clone(), card, s.hover, buttons, tg);
        if s.image.as_ref().is_none_or(|(l, _)| *l != look) {
            s.image = Some((look, draw(&text, card, yes, back, s.hover, buttons, &t).into_buffer()));
        }
        let buf = &s.image.as_ref().unwrap().1;
        let px = crate::header::px_rect(card, t.scale);
        crate::deco::image(renderer, buf, card, (px.w, px.h), t.scale).into_iter().collect()
    }
}

/// `ctl bestaetigen`: the card's state for the scenarios.
pub fn ctl(app: &mut App, words: &[&str]) -> Option<Result<String, String>> {
    if words.first().copied()? != "bestaetigen" {
        return None;
    }
    let c = &app.confirm;
    let s = c.shown.as_ref();
    Some(Ok(format!(
        "bestaetigen shown={} version={} back={} busy={} asking={} refused={} said={:?}\n{}end",
        u8::from(s.is_some()),
        s.map_or(0, |s| s.q.version),
        s.and_then(|s| s.q.back).map_or(String::from("-"), |b| b.to_string()),
        u8::from(s.is_some_and(|s| s.busy.is_some())),
        u8::from(c.next.is_some() || c.in_flight),
        c.refused,
        c.said,
        app.confirm_hot()
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::Origin::{Injected, Physical};

    #[test]
    fn the_question_comes_from_bitspkgds_status() {
        let j = Json::parse(r#"{"ok":true,"question":{"version":168,"generation":168,"back":167,"text":"Version 168 läuft. Bestätigen?"},"asks":true}"#).unwrap();
        assert_eq!(parse(&j), (Some(Q { version: 168, generation: 168, back: Some(167), text: "Version 168 läuft. Bestätigen?".into() }), true));
        let none = Json::parse(r#"{"ok":true,"question":{"version":168,"generation":168,"back":null,"text":"x"},"asks":true}"#).unwrap();
        assert_eq!(parse(&none).0.unwrap().back, None, "no older confirmed version: only Ja");
        let gone = Json::parse(r#"{"ok":true,"question":null,"asks":false}"#).unwrap();
        assert_eq!(parse(&gone), (None, false), "confirmed meanwhile: the card goes, the asking ends");
        let older = Json::parse(r#"{"ok":true,"installed":{}}"#).unwrap();
        assert_eq!(parse(&older), (None, false), "a bitspkgd from before VB-1 never asks");
    }

    #[test]
    fn a_missing_bitspkgd_is_asked_less_and_less_often() {
        assert_eq!(retry_after(1), POLL * 2);
        assert_eq!(retry_after(3), POLL * 8);
        assert_eq!(retry_after(40), POLL_MAX);
    }

    #[test]
    fn the_buttons_sit_inside_the_card_at_its_right_end() {
        let (card, yes, back) = boxes(Rect { x: 100, y: 10, w: 800, h: 40 }, 300, 40, Some(180), 2000);
        let back = back.unwrap();
        assert_eq!(back.x + back.w + 6, card.x + card.w);
        assert!(yes.x + yes.w + 6 == back.x && yes.x > card.x + 300);
        let (card, yes, none) = boxes(Rect { x: 100, y: 10, w: 800, h: 40 }, 300, 40, None, 2000);
        assert!(none.is_none() && yes.x + yes.w + 6 == card.x + card.w);
    }

    #[test]
    fn only_georgs_own_hand_after_the_arming_answers() {
        let at = Duration::from_secs(10);
        assert_eq!(press(Hand::all(Physical), at, at + ARM), Press::Counts);
        assert_eq!(press(Hand::all(Physical), at, at + ARM / 2), Press::Wait);
        assert_eq!(press(Hand::all(Injected), at, at + ARM * 3), Press::Refused, "MCP, the control socket, ui-act");
        assert_eq!(press(Hand { press: Physical, release: Physical, moved: Injected }, at, at + ARM), Press::Refused, "a warped pointer");
    }
}
