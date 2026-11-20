//! CL-3b: a tool's question to Georg and the rules of its answer (R8c; the installer's question has the same,
//! `install_dialog`). Only Georg's own hand says yes:
//! - a click counts when the press, the release and the pointer's last move all came from a real device
//!   (`Hand`); injected input that puts the pointer on Ja, or presses for him, makes his release worth nothing;
//! - Enter counts when it is real, the pick was moved to Ja by a real Tab, and a real click gave the input
//!   line the keys (keys that injected input steered into the sidebar are not Georg answering);
//! - Ja is armed only `ARM` after the card appeared, moved, or injected input last touched the sidebar: a click
//!   or key meant for what was there before never confirms;
//! - injected input never moves the pick; whenever it reaches the sidebar the pick goes back to Nein;
//! - Nein needs none of this: anyone may refuse.

use std::time::Duration;

use mcpreg::Json;

use super::{Model, Phase, line};
use crate::input::Origin;

/// How long Ja stays unarmed (the installer's time).
pub use crate::install_dialog::ARM;

/// What a yes that was not Georg's own gets instead.
pub const REAL_INPUT: &str = "Ja geht nur mit deiner echten Maus oder Tastatur.";
/// Under a question in the list that injected input asked (MCP, `bitscomp ctl`).
pub const REMOTE_ASK: &str = "über die Fernsteuerung gefragt";
/// On the card of a tool's question that belongs to such an ask.
pub const REMOTE_CARD: &str = "Diese Frage kam über die Fernsteuerung, nicht von dir.";

/// Whose question a card is: the engine's (answered with `answer{ask_id}`), a desktop step of `handeln` (the
/// desktop's gate answers it), the sidebar's own question before a chat is removed, or a question of `bits`
/// (`ctl_bits_card`, by its number).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CardFor {
    Tool,
    Step,
    Remove(String),
    Bits(u64),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Card {
    pub ask_id: u64,
    pub of: CardFor,
    /// A line above the text (board 5: "Löschen fragt immer").
    pub title: Option<String>,
    pub text: String,
    /// The button Enter would press (Nein until Georg's Tab moves it).
    pub yes_picked: bool,
    /// Said under the buttons: a yes that did not count.
    pub note: Option<String>,
    /// The ask it belongs to came from injected input.
    pub remote: bool,
    /// When it appeared, moved or was last touched by injected input (`pacer` time): Ja counts `ARM` later.
    pub since: Duration,
    /// The drawing shows Ja as armed (`Model::tick` sets it at `since + ARM`).
    pub armed: bool,
}

/// Who did each part of a click.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hand {
    pub press: Origin,
    pub release: Origin,
    /// The pointer's last move (a warp by the control socket or a client counts as injected).
    pub moved: Origin,
}

impl Hand {
    pub fn all(o: Origin) -> Hand {
        Hand { press: o, release: o, moved: o }
    }

    pub fn real(self) -> bool {
        self == Hand::all(Origin::Physical)
    }

    fn show(self) -> String {
        format!("press={} release={} pointer={}", self.press.name(), self.release.name(), self.moved.name())
    }
}

impl Model {
    fn rearm(&mut self, now: Duration) {
        if let Some(c) = self.card.as_mut() {
            (c.since, c.armed) = (now, false);
        }
    }

    /// Injected input reached the sidebar (a key, a click, a control command): whatever it prepared, the pick
    /// is on Nein again and Ja waits anew.
    pub fn touched(&mut self, now: Duration) {
        if let Some(c) = self.card.as_mut() {
            c.yes_picked = false;
        }
        self.rearm(now);
    }

    /// The buttons are somewhere else than a moment ago (the input line grew, a note came): a click on its
    /// way to the old place must not land on Ja.
    pub fn card_moved(&mut self, now: Duration) {
        self.rearm(now);
    }

    /// Tab on the card: Georg's own moves the pick; injected input does not (the caller logs and `touched`).
    pub fn pick(&mut self, key: Origin) {
        match self.card.as_mut() {
            Some(c) if key == Origin::Physical => c.yes_picked = !c.yes_picked,
            Some(c) => self.events.push(format!("assist: injected input tried to move the pick of question {}: ignored (R8c)", c.ask_id)),
            None => {}
        }
    }

    /// Enter on the card: the picked button.
    pub fn enter(&mut self, key: Origin, now: Duration) -> Vec<String> {
        let yes = self.card.as_ref().is_some_and(|c| c.yes_picked);
        // Georg is typing his next question: his Enter is meant for it, never for a question that just came.
        if yes && !self.edit.text.trim().is_empty() {
            self.events.push("assist: Enter with text in the input line while a question is open: not an answer".into());
            return Vec::new();
        }
        let real = key == Origin::Physical && (!yes || self.focus_physical);
        let how = format!("key={} focus={}", key.name(), if self.focus_physical { "physical" } else { "injected" });
        self.say(yes, real, &how, now)
    }

    /// A click on Ja or Nein.
    pub fn click(&mut self, yes: bool, hand: Hand, now: Duration) -> Vec<String> {
        self.say(yes, hand.real(), &hand.show(), now)
    }

    fn say(&mut self, yes: bool, real: bool, how: &str, now: Duration) -> Vec<String> {
        let Some(c) = self.card.as_mut() else { return Vec::new() };
        let id = c.ask_id;
        if yes && !real {
            c.note = Some(REAL_INPUT.into());
            self.events.push(format!("assist: yes to question {id} from injected input: refused (R8c) [{how}]"));
            self.touched(now);
            return Vec::new();
        }
        if yes && now < c.since + ARM {
            self.events.push(format!("assist: yes to question {id} before Ja was armed: ignored"));
            return Vec::new();
        }
        self.events.push(format!("assist: question {id} answered {} by {} input [{how}]", if yes { "yes" } else { "no" }, if real { "physical" } else { "injected" }));
        let of = c.of.clone();
        self.card = None;
        if of != CardFor::Tool {
            // The desktop's own question: the App acts on it (`decided`); the engine hears nothing here.
            self.decided = Some((of, yes));
            return Vec::new();
        }
        self.phase = Phase::Thinking;
        if let Some(a) = self.ask.as_mut() {
            // Georg may have taken longer than an answer may pause: the pause counts from his answer.
            a.last = now;
        }
        vec![line(vec![("op", Json::str("answer")), ("ask_id", Json::Num(id as f64)), ("yes", Json::Bool(yes))])]
    }
}
