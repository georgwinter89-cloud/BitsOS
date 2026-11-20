//! CL-3b: the assistant's conversation in bitscomp: the model (`assist`) between the socket's thread
//! (`assist_client`) and the sidebar. In the classic chrome the conversation is the sidebar below its ring; in
//! the Bühne it is the right column down to the stage's end once an assistant answered (desktop.md §0 answer 5:
//! nothing of the assistant shows while none is connected; "Übersicht" and "Als Nächstes" are empty boards yet). Keys go to the input line while it has
//! the focus (a click into the sidebar gives it, a click elsewhere or Esc takes it). R8c: a tool's question is
//! answered yes only by Georg's own hand; injected input (MCP, `bitscomp ctl`) can only say no. The rules are
//! `assist_card`'s; this file brings them the origins: of a click's press, release and the pointer's last
//! move, of a key (a repeated one keeps the origin of its press), and of the click that gave the focus.

use std::time::Duration;

use smithay::input::keyboard::{Keysym, ModifiersState};

use crate::address::Where;
use crate::assist::{Gate, Hand};
use crate::assist_level::{self as lv, Level};
use crate::assist_route::NewFor;
use crate::assist_client::Msg;
use crate::assist_draw::{PanelLook, panel_head};
use crate::assist_layout::{self as lay, CardKind, Look, Part};
use crate::desk::Spot;
use crate::input::Origin;
use crate::layout::Rect;
use crate::state::App;

/// The classic sidebar's head: the ring, the name, the state, "geht an", the running tool's line.
pub const SIDE_HEAD: i32 = 96;

/// Where the conversation is drawn: the Bühne's panel (its frame) or the classic bar, and the conversation's own
/// rectangle inside.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Place {
    pub panel: Option<Rect>,
    pub body: Rect,
}

impl App {
    /// The assistant's link started: without it the sidebar is as it was.
    pub fn assist_on(&self) -> bool {
        self.assist_client.is_some()
    }

    pub fn assist_place(&self) -> Option<Place> {
        if !self.assist_on() {
            return None;
        }
        if let Some(b) = self.buehne() {
            let (map, _) = b.column.filter(|_| self.assist.ever_up)?;
            let panel = Rect { h: b.stage.y + b.stage.h - map.y, ..map };
            let (m, head) = (self.theme.bl(12.0), panel_head(&self.theme));
            let body = Rect { x: panel.x + m, y: panel.y + head, w: panel.w - 2 * m, h: panel.h - head - m };
            return (body.h > 0 && body.w > 0).then_some(Place { panel: Some(panel), body });
        }
        let r = self.chrome().side?;
        Some(Place { panel: None, body: Rect { y: r.y + SIDE_HEAD, h: (r.h - SIDE_HEAD).max(1), ..r } })
    }

    fn assist_hover(&self) -> Option<Part> {
        match self.desk.hover {
            Some(Spot::Assist(p)) => Some(p),
            _ => None,
        }
    }

    /// The conversation's parts without the list's rows (hit testing, `ctl hot`).
    pub fn assist_parts(&self) -> Option<Look> {
        let p = self.assist_place()?;
        Some(lay::parts(&self.assist, &self.theme, p.body, self.assist_hover(), &self.assist_side()))
    }

    /// The window in front as the sidebar names it ("Brave · Georg – Suche"), cheap enough for every frame.
    pub(crate) fn assist_front_name(&self) -> String {
        match self.ws.focused() {
            Some(w) => {
                let (app, title) = crate::ctl::names(w);
                if lv::never_sees(&self.settings, &app) {
                    format!("{app} ({})", lv::NOT_SEEN)
                } else if title.is_empty() {
                    app
                } else {
                    format!("{app} · {title}")
                }
            }
            None => "die Arbeitsfläche".into(),
        }
    }

    /// What the sidebar shows beside the conversation (board 3).
    fn assist_side(&self) -> lay::Side {
        lay::Side {
            sees: Some((lv::sidebar_sees(&self.settings), self.assist_front_name())),
            level: Some(Level::of(&self.settings)),
            cloud: self.assist_cloud_line(true),
            // The host tests' clock never moves (`Status::fixed`).
            now: if self.status.fixed { crate::status::FIXED_SECS } else { crate::status::epoch_secs() },
            offset_min: if self.status.fixed { 0 } else { crate::status::offset_min() },
        }
    }

    /// Where Ja is now; when it is somewhere else than at the last look, it waits anew (`Model::card_moved`):
    /// whatever shifts the card (the input line grows, a note comes, the scale changes) must not put Ja under
    /// a click that was aimed at something else. True: it moved.
    fn assist_card_seen(&mut self, look: &Look) -> bool {
        let at = match (&self.assist.card, look.card.as_ref().map(|c| &c.kind)) {
            (Some(c), Some(CardKind::Question { yes, .. })) => Some((c.ask_id, *yes)),
            _ => None,
        };
        let moved = matches!((self.assist_yes_at, at), (Some((a, r)), Some((b, q))) if a == b && r != q);
        self.assist_yes_at = at;
        if moved {
            self.assist.card_moved(crate::pacer::now());
        }
        moved
    }

    /// The whole look for this frame; the scroll offset is cut to what is there.
    pub fn assist_look(&mut self) -> Option<Look> {
        let p = self.assist_place()?;
        let t = self.theme.clone();
        let side = self.assist_side();
        let mut look = lay::parts(&self.assist, &t, p.body, self.assist_hover(), &side);
        if self.assist_card_seen(&look) {
            look = lay::parts(&self.assist, &t, p.body, self.assist_hover(), &side);
        }
        self.assist.scroll = lay::rows(&mut look, &mut self.assist, &mut self.assist_wraps, &t, self.theme_gen);
        Some(look)
    }

    pub fn assist_panel_look(&mut self) -> Option<PanelLook> {
        let rect = self.assist_place()?.panel?;
        let conv = self.assist_look()?;
        let a = &self.assist;
        Some(PanelLook {
            rect,
            theme: self.theme_gen,
            name: a.name.clone(),
            word: a.phase.word(),
            waiting: a.phase == crate::assist::Phase::Waiting,
            notice: a.notice.clone(),
            tool: a.tool.clone(),
            conv,
            menu: self.assist_menu_look(),
        })
    }

    /// The part of the conversation under (x, y): the whole panel is the sidebar's, so no click falls through.
    pub fn assist_at(&self, x: f64, y: f64) -> Option<Part> {
        let p = self.assist_place()?;
        let all = p.panel.or(self.chrome().side)?;
        if !all.contains(x, y) {
            return None;
        }
        if let Some(p) = self.assist_menu_at(x, y) {
            return Some(p);
        }
        Some(self.assist_parts().map_or(Part::Body, |l| l.hit(x, y)))
    }

    /// After every change of the model: its request lines go out, its log lines too, the ring follows.
    pub(crate) fn assist_sync(&mut self, lines: Vec<String>) {
        for e in std::mem::take(&mut self.assist.events) {
            eprintln!("bitscomp: {e}");
        }
        self.assist_out(lines, NewFor::Sidebar);
        if let Some((of, yes)) = self.assist.decided.take() {
            self.assist_decided(of, yes);
        }
        self.bits_card_show();
        if self.assist_on() {
            self.set_ring(self.assist.phase.ring());
            self.assist_menu_follow();
        }
        self.chrome_dirty();
    }

    pub fn assist_msg(&mut self, m: Msg) {
        let now = crate::pacer::now();
        let lines = match m {
            Msg::Up => {
                eprintln!("bitscomp: assist: connected");
                // A new connection: nothing of the old one is answered on it.
                self.assist_route.reset();
                self.assist.link_up(now)
            }
            Msg::Down(why) => {
                self.assist.link_down(&why, now);
                self.assist_route.reset();
                self.db_link_down();
                Vec::new()
            }
            Msg::Stranger(why) => {
                self.assist.link_stranger(&why, now);
                self.assist_route.reset();
                self.db_link_down();
                Vec::new()
            }
            Msg::Line(j) => {
                let out = self.assist_line_in(&j, now);
                self.db_setup_ready();
                out
            }
            Msg::Paste(text, origin) => {
                self.assist.paste(&text, origin != Origin::Physical);
                Vec::new()
            }
        };
        self.assist_sync(lines);
    }

    pub fn assist_due(&self) -> Option<Duration> {
        // The popup's wait for its chat is looked at once a second.
        let popup = self.db.waits_for_chat().then(|| crate::pacer::now() + Duration::from_secs(1));
        self.assist_client.as_ref().and_then(|_| self.assist.due()).into_iter().chain(popup).min()
    }

    pub fn assist_tick(&mut self, now: Duration) {
        self.db_tick();
        if self.assist_client.as_ref().and_then(|_| self.assist.due()).is_some_and(|t| t <= now) {
            let lines = self.assist.tick(now);
            self.assist_sync(lines);
        }
    }

    /// A setting of the assistant changed (`ai.*`, the name): what it can do is asked again.
    pub fn assist_setting(&mut self, key: &str) {
        if !self.assist_on() || !(key.starts_with("ai.") || key.starts_with("claude.")) {
            return;
        }
        if self.assist.status.is_none() {
            self.assist.name = self.desk.claude_name.clone();
        }
        let lines = self.assist.refresh(crate::pacer::now());
        self.assist_sync(lines);
    }

    /// The input line takes the keys. `real`: Georg's own click asked for it (only such a focus lets his
    /// Enter say yes to a question; a focus that injected input gave stays what it is until he clicks).
    pub fn assist_focus(&mut self, real: bool) {
        self.address_end();
        self.page_edit_end();
        if real || !self.assist.focus {
            self.assist.focus_physical = real;
        }
        if !self.assist.focus {
            self.assist.focus = true;
            // The settings may have changed since the last look.
            let lines = self.assist.refresh(crate::pacer::now());
            self.assist_sync(lines);
        }
    }

    pub fn assist_blur(&mut self) {
        if self.assist.focus {
            self.assist.focus = false;
            self.desk.held = None;
            self.chrome_dirty();
        }
    }

    /// The keys are the input line's: it has the focus and its sidebar is on the screen.
    pub fn assist_takes_keys(&self) -> bool {
        self.assist.focus && self.assist_place().is_some()
    }

    /// A click on a part of the conversation (the release over the spot that was pressed).
    pub fn assist_act(&mut self, part: Part) {
        match part {
            Part::Model => return self.assist_menu_toggle("click"),
            Part::ModelRow(i) => return self.assist_menu_pick(i, self.input_origin),
            _ => {}
        }
        let now = crate::pacer::now();
        let hand = Hand { press: self.desk.press_origin, release: self.input_origin, moved: self.pointer_origin };
        let real = hand.real();
        // The buttons may have moved since the last frame.
        if let Some(l) = self.assist_parts() {
            self.assist_card_seen(&l);
        }
        if !real {
            self.assist.touched(now);
        }
        // While a question is open, injected input does not steer Georg's keys into the sidebar.
        if real || self.assist.card.is_none() {
            self.assist_focus(real);
        } else {
            eprintln!("bitscomp: assist: an injected click gives the input line no keys while a question is open (R8c)");
        }
        if let Some(c) = self.assist_client.as_ref().filter(|_| !self.assist.up) {
            // Georg is here: no waiting for the next try of a missing socket.
            c.poke();
        }
        let lines = match part {
            // A click while the assistant cannot answer asks it again (a provider was added meanwhile).
            Part::Body | Part::Input if self.assist.gate() != Gate::Ready => self.assist.refresh(now),
            Part::Body | Part::Input => Vec::new(),
            Part::Send if self.assist.ask_id().is_some() => self.assist.cancel(now),
            Part::Send => {
                let context = self.assist_context();
                self.assist.submit_with(now, !real, context)
            }
            Part::New => self.assist.new_conversation(now),
            Part::Chat(i) => match self.assist.chats.get(i).map(|c| c.chat.clone()) {
                Some(c) => self.assist.open_chat(&c, now),
                None => Vec::new(),
            },
            Part::ChatRemove(i) => {
                if let Some(c) = self.assist.chats.get(i).map(|c| c.chat.clone()) {
                    self.assist_ask_remove(&c, now);
                }
                Vec::new()
            }
            // Seeing the window in front is more freedom: only Georg's own click switches it on (off: anyone).
            Part::Sees => {
                let on = !lv::sidebar_sees(&self.settings);
                if on && !real {
                    eprintln!("bitscomp: assist: \"Fenster vorne mitsehen\" from injected input refused");
                    self.assist.note = Some((crate::assist_views::SEES_REFUSED.into(), true));
                } else {
                    self.assist_set(lv::SIDEBAR_SEES, mcpreg::Json::Bool(on), on);
                }
                Vec::new()
            }
            Part::Level => {
                self.assist.level_open = !self.assist.level_open;
                Vec::new()
            }
            Part::LevelPick(l) => {
                self.assist_level_pick(l, if real { crate::input::Origin::Physical } else { crate::input::Origin::Injected }, true);
                self.assist.level_open = false;
                Vec::new()
            }
            Part::Trash => {
                self.assist_trash_list();
                Vec::new()
            }
            Part::Yes => self.assist.click(true, hand, now),
            Part::No => self.assist.click(false, hand, now),
            Part::Link => {
                self.assist_sync(Vec::new());
                return self.open_address(crate::assist::SETTINGS, Where::Tab, None);
            }
            Part::Model | Part::ModelRow(_) => Vec::new(),
        };
        self.assist_sync(lines);
    }

    /// A key while the input line has the focus; every key stays with bitscomp then. `origin` is the key
    /// press's own (a repeat brings the one of its press, not of whatever event came last).
    pub fn assist_key(&mut self, sym: Keysym, mods: &ModifiersState, origin: Origin) {
        let now = crate::pacer::now();
        let real = origin == Origin::Physical;
        let page = self.assist_parts().map_or(200, |l| (l.list.h - lay::LINE).max(lay::LINE));
        if matches!(sym, Keysym::Return | Keysym::KP_Enter) && self.assist.card.is_none() && !mods.shift {
            if !real {
                self.assist.touched(now);
            }
            let context = self.assist_context();
            let lines = self.assist.submit_with(now, !real, context);
            return self.assist_sync(lines);
        }
        let a = &mut self.assist;
        let card = a.card.is_some();
        if !real {
            a.touched(now);
        }
        let lines = match sym {
            // Board 2/5: Esc = Nein to a question; the next Esc cancels the answer.
            Keysym::Escape if card => a.click(false, Hand::all(origin), now),
            Keysym::Escape if a.ask_id().is_some() => a.cancel(now),
            Keysym::Escape => return self.assist_blur(),
            Keysym::Return | Keysym::KP_Enter if card => a.enter(origin, now),
            // Only Tab moves the pick: the arrows are the input line's, where Georg may be typing his next
            // question when a card comes.
            Keysym::Tab | Keysym::ISO_Left_Tab if card => {
                a.pick(origin);
                Vec::new()
            }
            // Injected input does nothing else while a question is open (it could shift the card under a click).
            _ if card && !real => {
                eprintln!("bitscomp: assist: an injected key while a question is open: ignored (R8c)");
                Vec::new()
            }
            Keysym::Return | Keysym::KP_Enter if mods.shift => {
                a.insert('\n', !real);
                Vec::new()
            }
            Keysym::Return | Keysym::KP_Enter => unreachable!("sent above"),
            Keysym::Prior | Keysym::KP_Prior => {
                a.scroll += page;
                Vec::new()
            }
            Keysym::Next | Keysym::KP_Next => {
                a.scroll = (a.scroll - page).max(0);
                Vec::new()
            }
            Keysym::v | Keysym::V if mods.ctrl => return self.assist_paste(origin),
            Keysym::Insert | Keysym::KP_Insert if mods.shift => return self.assist_paste(origin),
            _ => {
                a.note = None;
                match sym {
                    Keysym::BackSpace => a.edit.backspace(),
                    Keysym::Delete | Keysym::KP_Delete => a.edit.delete(),
                    Keysym::Left | Keysym::KP_Left => a.edit.left(),
                    Keysym::Right | Keysym::KP_Right => a.edit.right(),
                    Keysym::Home | Keysym::KP_Home => a.edit.home(),
                    Keysym::End | Keysym::KP_End => a.edit.end(),
                    _ => match sym.key_char() {
                        Some(c) if !c.is_control() && !mods.ctrl && !mods.logo => {
                            a.insert(c, !real);
                        }
                        _ => return,
                    },
                }
                if a.edit.text.is_empty() {
                    a.typed_remote = false;
                }
                Vec::new()
            }
        };
        self.assist_sync(lines);
    }

    /// The wheel over the conversation: up shows older lines.
    pub fn assist_wheel(&mut self, steps: f64) {
        self.assist.scroll = (self.assist.scroll - (steps * 3.0 * lay::LINE as f64).round() as i32).max(0);
        self.chrome_dirty();
    }
}

#[cfg(test)]
#[path = "assist_app_tests.rs"]
mod tests;
