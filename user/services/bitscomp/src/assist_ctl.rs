//! CL-3b: the control socket's side of the conversation: `ctl assist state|refresh|ask TEXT` for tests and the
//! desk, and the `ctl hot` lines of its clickable parts. What comes through here is injected input (R8c): it
//! can ask (marked in the list as asked from outside) and read the state, never say yes to a tool's question.
//! What Georg and the assistant wrote is not handed to every program that reaches the control socket: the
//! conversation, the unsent input, the card's and the notes' texts never go out there; `state` gives states and
//! lengths.

use uilist::{Action, Element, Role, State};

use crate::assist::Gate;
use crate::assist_app::Place;
use crate::assist_layout::CardKind;
use crate::input::Origin;
use crate::layout::Rect;
use crate::state::App;

/// The sidebar's group name in the desktop's list.
pub const GROUP: &str = "KI-Seitenleiste";

fn press(role: Role, name: impl Into<String>, r: [i32; 4], key: impl Into<String>) -> Element {
    Element::new(role, name, r).key(key).acts(&[Action::Press])
}

impl App {
    /// The sidebar in DB1's list: the chats, the input, the switch, the pill, a question with Ja and Nein.
    pub(crate) fn ui_assist(&self) -> Option<Element> {
        let l = self.assist_parts()?;
        let px = |r: Rect| self.ui_px(r);
        let mut d = Element::new(Role::Group, GROUP, px(l.body)).key("assist");
        if let Some((shown, p)) = self.assist_menu_place() {
            let mut m = press(Role::Menu, "Modell", px(p.closed), "model").value(shown.current.clone()).state_if(State::Expanded, self.assist_menu.open);
            for (r, e) in p.rows.iter().zip(&shown.entries) {
                m = m.child(press(Role::Menuitem, e.label(), px(*r), format!("model:{}", e.label())));
            }
            d = d.child(m);
        }
        d = d.child(press(Role::Button, crate::assist_layout::NEW_CHAT, px(l.new), "new"));
        for (i, c) in l.chats.iter().enumerate() {
            let row = press(Role::Button, c.title.clone(), px(c.rect), format!("chat:{i}")).value(c.sub.clone()).state_if(State::Selected, c.current);
            d = d.child(row.child(press(Role::Button, "Chat entfernen", px(c.remove), format!("chat-remove:{i}"))));
        }
        d = d.child(Element::new(Role::Field, l.input.placeholder.clone(), px(l.input.rect)).key("input").value(l.input.lines.join("\n")).state_if(State::Focused, l.input.focus).acts(&[Action::Focus, Action::Type]));
        d = d.child(press(Role::Button, l.send_label, px(l.send), "send").state_if(State::Disabled, !l.live));
        if let Some(se) = &l.sees {
            d = d.child(Element::new(Role::Toggle, crate::assist_layout::SEES_TEXT, px(se.switch)).key("sees").value(format!("gerade: {}", se.now)).acts(&[Action::Toggle]).state_if(State::Checked, se.on));
        }
        if l.levels.is_empty() {
            d = d.child(press(Role::Button, l.pill_text.clone(), px(l.pill), "level"));
        }
        for (lv, r) in &l.levels {
            d = d.child(press(Role::Button, lv.name(), px(*r), format!("level:{}", lv.word())));
        }
        d = d.child(Element::new(Role::List, "Gespräch", px(l.list)).key("list").acts(&[Action::Scroll]));
        if let Some(c) = &l.card {
            let text = c.title.iter().cloned().chain(c.lines.iter().cloned()).collect::<Vec<_>>().join(" ");
            let mut card = Element::new(Role::Group, "Frage der KI", px(c.rect)).key("card").child(Element::new(Role::Text, "Text", px(c.rect)).key("text").value(text));
            match &c.kind {
                CardKind::Question { yes, no, armed, .. } => {
                    card = card.child(press(Role::Button, "Nein", px(*no), "no"));
                    card = card.child(press(Role::Button, "Ja", px(*yes), "yes").state_if(State::Disabled, !armed).settles());
                }
                CardKind::Info { link: Some(r) } => card = card.child(press(Role::Link, crate::assist_layout::LINK_TEXT, px(*r), "link").value(crate::assist::SETTINGS)),
                CardKind::Info { link: None } => {}
            }
            d = d.child(card);
        }
        if let Some((r, text)) = &l.trash {
            d = d.child(press(Role::Link, crate::assist_layout::TRASH_LINK, px(*r), "trash").value(text.clone()));
        }
        Some(d)
    }

    /// `ctl assist ...`: the state, the status asked again, a question. The control socket never shows or writes
    /// the conversation's texts.
    pub fn assist_ctl(&mut self, words: &[&str], cmd: &str) -> Option<Result<String, String>> {
        if words.first() != Some(&"assist") {
            return None;
        }
        let rest = |verb: &str| cmd.split_once(verb).map_or("", |(_, r)| r).trim().to_string();
        // The control socket's input is injected (a unit test may set `ctl_origin` to Georg's own hand).
        let injected = self.ctl_origin != Origin::Physical;
        let now = crate::pacer::now();
        if injected && words.get(1) == Some(&"ask") {
            // What these change may shift an open card: its Ja waits anew, the pick is Nein.
            self.assist.touched(now);
        }
        Some(match words.get(1).copied() {
            Some("state") | None => Ok(self.assist_state()),
            Some("ask") => {
                if !self.assist_on() {
                    return Some(Err("assist: not connected".into()));
                }
                // Georg's unsent line stays his.
                let lines = self.assist.ask(&rest(" ask "), now, injected);
                let sent = !lines.is_empty();
                self.assist_sync(lines);
                if sent { Ok(self.assist_state()) } else { Err(self.assist.note.as_ref().map_or("assist: nothing to ask".into(), |(n, _)| n.clone())) }
            }
            Some("refresh") => {
                let lines = self.assist.refresh(now);
                self.assist_sync(lines);
                Ok(self.assist_state())
            }
            Some("model") => self.assist_menu_ctl(&words[2..]),
            _ => Err("usage: assist state|refresh|ask TEXT|model open|close|state|pick I".into()),
        })
    }

    fn assist_state(&self) -> String {
        let a = &self.assist;
        let s = a.status.as_ref();
        let gate = match a.gate() {
            Gate::Ready => "ready",
            Gate::Starting => "starting",
            Gate::Silent => "silent",
            Gate::Stranger => "stranger",
            Gate::NotReady(_) => "notready",
        };
        let card = a.card.as_ref().map_or("none".to_string(), |c| c.ask_id.to_string());
        let pick = a.card.as_ref().map_or("-", |c| if c.yes_picked { "ja" } else { "nein" });
        let mut out = format!(
            "assist on={} link={} gate={gate} ready={} busy={} turns={} ring={} ask={} card={card} pick={pick} armed={} remote={}",
            self.assist_on(),
            if a.up { "up" } else { "down" },
            s.is_some_and(|s| s.ready),
            a.busy(),
            s.map_or(0, |s| s.turns),
            a.phase.name(),
            a.ask_id().map_or("none".into(), |i| i.to_string()),
            a.card.as_ref().is_some_and(|c| c.armed),
            a.card.as_ref().is_some_and(|c| c.remote),
        );
        out.push_str(&format!(
            " service_busy={} chat={} chats={} level={} sees={} trash={}",
            s.is_some_and(|s| s.busy),
            a.chat.as_deref().unwrap_or("-"),
            a.chats.len(),
            crate::assist_level::Level::of(&self.settings).word(),
            crate::assist_level::sidebar_sees(&self.settings),
            a.trash.is_some(),
        ));
        out.push_str(&format!(
            " input_len={} focus={} scroll={} items={} pending={} provider={} place={} name={:?} shown={}",
            a.edit.text.chars().count(),
            a.focus,
            a.scroll,
            a.items.len(),
            a.pending(),
            s.and_then(|s| s.provider.as_deref()).unwrap_or("-"),
            s.and_then(|s| s.place.as_deref()).unwrap_or("-"),
            a.name,
            match self.assist_place() {
                Some(Place { panel: Some(_), .. }) => "panel",
                Some(_) => "sidebar",
                None => "no",
            },
        ));
        out
    }

    /// The `ctl hot` lines of the conversation's clickable parts.
    pub fn assist_hot(&self) -> String {
        let Some(l) = self.assist_parts() else { return String::new() };
        let s = self.theme.scale;
        let mid = |r: Rect| format!("{},{}", ((r.x + r.w / 2) as f64 * s).round(), ((r.y + r.h / 2) as f64 * s).round());
        let mut out = self.assist_menu_hot();
        out += &format!("assist-input at={}\nassist-send at={} label={}\nassist-new at={}\nassist-list at={}\n", mid(l.input.rect), mid(l.send), l.send_label, mid(l.new), mid(l.list));
        for (i, c) in l.chats.iter().enumerate() {
            out += &format!("assist-chat i={i} at={} remove={} current={}\n", mid(c.rect), mid(c.remove), c.current);
        }
        if let Some(se) = &l.sees {
            out += &format!("assist-sees at={} on={}\n", mid(se.switch), se.on);
        }
        if l.levels.is_empty() {
            out += &format!("assist-level at={}\n", mid(l.pill));
        }
        for (lv, r) in &l.levels {
            out += &format!("assist-level-pick level={} at={}\n", lv.word(), mid(*r));
        }
        if let Some((r, _)) = &l.trash {
            out += &format!("assist-trash at={}\n", mid(*r));
        }
        match l.card.as_ref().map(|c| &c.kind) {
            Some(CardKind::Question { yes, no, .. }) => out.push_str(&format!("assist-yes at={}\nassist-no at={}\n", mid(*yes), mid(*no))),
            Some(CardKind::Info { link: Some(r) }) => out.push_str(&format!("assist-link at={} addr={}\n", mid(*r), crate::assist::SETTINGS)),
            _ => {}
        }
        out
    }
}
