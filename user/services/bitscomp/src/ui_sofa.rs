//! DB1: the sofa mode's `sofa` window: its focusable elements (sofa_items, the same the pad, the keys and the
//! pointer use) with their names, the footer's hints, the search field (a WLAN key typed there never shows: while
//! it asks for a secret its on-screen keys are secret too, never focused in the list and never noted).

use uilist::{Action, Element, Role, State};

use crate::sofa_focus::{Item, Screen};
use crate::sofa_layout::SECTIONS;
use crate::state::App;

impl App {
    pub(crate) fn ui_sofa(&self) -> Vec<Element> {
        let f = self.sofa_frame();
        let px = |r: crate::layout::Rect| {
            let p = f.r(r);
            [p.x, p.y, p.w, p.h]
        };
        let screen = self.sofa.nav.screen;
        let mut v = vec![Element::new(Role::Heading, screen.title(), px(crate::layout::Rect { x: 0, y: 0, w: f.board().0, h: 80 })).key("screen").value(screen.word())];
        let typing = screen == Screen::Suche && self.sofa.show() == crate::sofa::Show::Shell;
        let secret = screen == Screen::Suche && self.sofa.search.target.secret();
        for it in self.sofa_items() {
            let key = it.id.starts_with("key:");
            let focused = self.sofa.focus.as_deref() == Some(it.id.as_str()) && !(secret && key);
            v.push(self.ui_sofa_item(&it, px(it.rect)).state_if(State::Focused, focused).state_if(State::Secret, secret && key));
        }
        if typing {
            let s = &self.sofa.search;
            let at = px(crate::sofa_layout::FIELD);
            let field = if secret { Element::secret("Eingabe", at) } else { Element::new(Role::Field, "Eingabe", at).value(s.field.text.clone()) };
            v.push(field.key("search-field"));
        }
        for p in self.sofa_hints() {
            v.push(Element::new(Role::Button, p.hint.word, px(p.rect)).key(format!("hint:{}", p.hint.id)).value(format!("{} ({})", p.hint.sign, p.hint.ways)).acts(&[Action::Press]));
        }
        v
    }

    fn ui_sofa_item(&self, it: &Item, rect: [i32; 4]) -> Element {
        let id = it.id.as_str();
        let n = |p: &str| id.strip_prefix(p).and_then(|n| n.parse::<usize>().ok());
        let screen = self.sofa.nav.screen;
        let mut role = Role::Button;
        let mut value = None;
        let mut asks = false;
        let name: String = if let Some(w) = id.strip_prefix("pill:") {
            role = Role::Tab;
            Screen::of_word(w).map_or(w, |s| s.title()).to_string()
        } else if let Some(i) = n("tile:") {
            role = Role::Item;
            if screen == Screen::Start {
                let (h, r) = self.sofa_start_model();
                r.get(i).map(|t| t.name.clone()).unwrap_or_else(|| h.name.clone())
            } else {
                self.sofa_tiles().get(i).map(|t| t.name.clone()).unwrap_or_else(|| id.to_string())
            }
        } else if let Some(i) = n("coll:") {
            role = Role::Tab;
            self.sofa_colls().get(i).map_or(id, |c| c.title()).to_string()
        } else if let Some(i) = n("section:") {
            role = Role::Tab;
            SECTIONS.get(i).map_or(id, |s| s.0).to_string()
        } else if let Some(j) = n("row:") {
            role = Role::Item;
            let rows = self.sofa_set_rows();
            value = rows.get(j).map(|r| r.line.clone());
            rows.get(j).map_or_else(|| id.to_string(), |r| r.title.clone())
        } else if let Some(i) = n("hit:") {
            role = Role::Item;
            let hits = &self.sofa.search.hits;
            match i {
                0 => hits.first().map_or_else(|| "Programm".into(), |h| h.tile.name.clone()),
                1 => "YouTube".into(),
                2 => "Brave-Suche".into(),
                i => hits.get(i - 2).map_or_else(|| id.to_string(), |h| h.tile.name.clone()),
            }
        } else if let Some(k) = id.strip_prefix("key:") {
            k.to_string()
        } else if let Some(i) = n("q:out:") {
            role = Role::Choice;
            self.sofa_outputs().get(i).map_or(id, |o| o.1).to_string()
        } else {
            match id {
                "search" => "Suche",
                "menu" => "Schnellmenü",
                "hero" => {
                    role = Role::Item;
                    return Element::new(role, self.sofa_start_model().0.name, rect).key(id).acts(&[Action::Press, Action::Focus]);
                }
                "hero:details" => "Details",
                "card:youtube" => "YouTube",
                "card:musik" => "Musik",
                "alle" => "Alle ansehen",
                "steam" => "Steam",
                "sort" => "Sortieren",
                "play" => "Spielen",
                "steam-open" => "In Steam öffnen",
                "q:resume" => "Weiterspielen",
                "q:end" => {
                    asks = true;
                    "Spiel beenden"
                }
                "q:volume" => {
                    role = Role::Slider;
                    value = Some(crate::sofa_quick::header_word(self.sofa.quick.io.volume));
                    "Lautstärke"
                }
                "q:wlan" => "WLAN",
                "q:pad" => "Controller",
                "q:screen" => "Bildschirm",
                "q:power" => "Energie",
                "q:desktop" => "Zum Desktop",
                "q:reboot" => {
                    asks = true;
                    "Neu starten"
                }
                "q:off" => {
                    asks = true;
                    "Ausschalten"
                }
                "c:yes" => {
                    return Element::new(role, "Ja", rect).key(id).acts(&[Action::Press, Action::Focus]).settles();
                }
                "c:no" => "Nein",
                other => other,
            }
            .to_string()
        };
        let mut e = Element::new(role, name, rect).key(id).acts(&[Action::Press, Action::Focus]).asks(asks);
        if let Some(v) = value {
            e = e.value(v);
        }
        e
    }
}
