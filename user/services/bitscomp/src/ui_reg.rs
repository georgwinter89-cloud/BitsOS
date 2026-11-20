//! DB1 (Durchblick, "See"): the desktop as a list (contract C1, user/lib/uilist). bitscomp draws everything
//! itself and keeps no widget tree, so the list is built when asked, from the same layouts the drawing and the hit
//! tests use (chrome, page layout, sofa items, dialog parts): what the list says is where a click lands. Windows:
//! `desktop` (bars, overlays, dialogs), `sofa` while a sofa screen is up, `w<id>` per program window (its frame,
//! and its content: a settings page from its model, an own program from its protocol file, a foreign one from C3
//! or only its picture). Rects are physical output pixels, the unit of the pointer commands.

use uilist::{Action, Element, Role, Source, State, Window as UiWindow};

use crate::layout::{Place, Placement, Rect};
use crate::state::App;
use crate::win::Window;
use crate::{header, tabs, zones};

pub const DESKTOP: &str = "desktop";
pub const SOFA: &str = "sofa";

/// Which windows get their elements.
#[derive(Clone, Debug, PartialEq)]
pub enum Full {
    /// Only the focused one (`ui`).
    Focused,
    /// This one (`ui <window>`).
    One(String),
    /// This one, never asking a foreign program's provider first (C3, `ui_fetch`): for Georg's own clicks.
    Local(String),
}

pub fn wid(w: &Window) -> String {
    format!("w{}", crate::ctl::win_id(w))
}

impl App {
    /// A logical rect in physical output pixels.
    pub(crate) fn ui_px(&self, r: Rect) -> [i32; 4] {
        let p = header::px_rect(r, self.theme.scale);
        [p.x, p.y, p.w, p.h]
    }

    /// Whether a desktop overlay or dialog has the keyboard (then the desktop is the focused window).
    fn ui_overlay(&self) -> bool {
        self.lib.open
            || self.feld.open
            || self.rewind.open
            || self.menu.is_some()
            || self.install.shown()
            || self.desk.ws_ask.is_some()
            || self.desk.edit.is_some()
            || self.desk.min_list.open
    }

    /// The id of the window that has the keyboard now.
    pub fn ui_focused_id(&self) -> String {
        if self.sofa.listens() {
            return SOFA.into();
        }
        // The start page put over the workspace hides its windows: the desktop has the pointer and the keys.
        if self.ui_overlay() || self.desk.start_forced {
            return DESKTOP.into();
        }
        self.ws.focused().map_or_else(|| DESKTOP.to_string(), wid)
    }

    /// Every window, with elements where `full` asks for them.
    pub fn ui_windows(&self, full: &Full) -> Vec<UiWindow> {
        let focused = self.ui_focused_id();
        let wants = |id: &str| match full {
            Full::Focused => id == focused,
            Full::One(w) | Full::Local(w) => id == w,
        };
        let mut out = Vec::new();
        let area = self.ui_px(self.area_full());
        let mut desk = UiWindow { window: DESKTOP.into(), app: "bitscomp".into(), title: "Arbeitsfläche".into(), pid: std::process::id() as i64, source: Source::Desktop, focused: focused == DESKTOP, rect: area, elements: Vec::new() };
        if wants(DESKTOP) {
            desk.elements = self.ui_desktop();
        }
        out.push(desk);
        if self.sofa.show() != crate::sofa::Show::None {
            let mut s = UiWindow { window: SOFA.into(), app: "bitscomp".into(), title: "Sofa-Modus".into(), pid: std::process::id() as i64, source: Source::Desktop, focused: focused == SOFA, rect: area, elements: Vec::new() };
            if wants(SOFA) {
                s.elements = self.ui_sofa();
            }
            out.push(s);
        }
        for p in self.ui_drawn() {
            let id = wid(&p.win);
            out.push(self.ui_client(&p, id == focused, wants(&id)));
        }
        for m in &self.ws.minimized {
            let id = wid(&m.win);
            let (app, title) = crate::ctl::names(&m.win);
            let mut w = UiWindow { window: id.clone(), app, title, pid: self.pid_of(&m.win) as i64, source: Source::Picture, focused: false, rect: [0, 0, 0, 0], elements: Vec::new() };
            if wants(&id) {
                w.elements = vec![Element::new(Role::Text, "Minimiert", [0, 0, 0, 0]).key("minimised").value("Das Fenster ist minimiert; es zeigt nichts.")];
            }
            out.push(w);
        }
        for w in &mut out {
            w.number();
            uilist::scrub(&mut w.elements);
            hide_below(&mut w.elements, false);
        }
        out
    }

    /// Whether the bars are not drawn now: a sofa screen or a fullscreen window covers them (paint::order_with).
    pub(crate) fn ui_chrome_covered(&self) -> bool {
        self.sofa.show() == crate::sofa::Show::Shell || self.drawn_placements().iter().any(|p| p.fullscreen && p.visible)
    }

    /// The placements as drawn: hidden under the start page put over them (`drawn_placements`), under a sofa
    /// screen (drawn alone), and beside a fullscreen window (all there is).
    pub(crate) fn ui_drawn(&self) -> Vec<Placement<Window>> {
        let mut v = self.drawn_placements();
        let shell = self.sofa.show() == crate::sofa::Show::Shell;
        let full = v.iter().position(|p| p.fullscreen && p.visible);
        for (i, p) in v.iter_mut().enumerate() {
            if shell || full.is_some_and(|f| f != i) {
                p.visible = false;
            }
        }
        v
    }

    /// One program window: its frame and, when asked, its content.
    fn ui_client(&self, p: &Placement<Window>, focused: bool, full: bool) -> UiWindow {
        let (app, title) = crate::ctl::names(&p.win);
        let content = if p.fullscreen { p.frame } else { self.content_rect(p.frame) };
        let rect = self.ui_px(if p.visible { content } else { Rect { x: 0, y: 0, w: 0, h: 0 } });
        let pid = self.pid_of(&p.win);
        let mut w = UiWindow { window: wid(&p.win), app, title: title.clone(), pid: pid as i64, source: Source::Picture, focused, rect, elements: Vec::new() };
        if let Some(page) = self.page_pid(&p.win) {
            w.source = Source::Desktop;
            if full && p.visible {
                w.elements = self.ui_page(page, content);
            }
        } else if full && p.visible {
            let (source, elements) = self.ui_foreign(&p.win, pid, content);
            w.source = source;
            w.elements = elements;
        } else {
            w.source = self.ui_source_of(pid);
        }
        if full && p.visible && !p.fullscreen {
            w.elements.insert(0, self.ui_frame(p, &title));
        }
        w
    }

    /// The window's frame: title, its buttons, its tabs.
    fn ui_frame(&self, p: &Placement<Window>, title: &str) -> Element {
        let t = &self.theme;
        let f = p.frame;
        let floating = matches!(p.place, Place::Floating { .. });
        let (n, shown) = zones::pages(p.place);
        let strip = tabs::strip(zones::tab_frame(f, floating, t), n, shown, t);
        let mut g = Element::new(Role::Group, "Fensterrahmen", self.ui_px(Rect { h: t.header_height, ..f })).key("frame");
        g = g.child(Element::new(Role::Heading, title, self.ui_px(strip.tab(shown).unwrap_or(Rect { h: t.header_height, ..f }))).key("title").acts(&[Action::Focus]));
        g = g.child(Element::new(Role::Button, "Schließen", self.ui_px(zones::close_button(f, t))).key("close").acts(&[Action::Press]));
        g = g.child(Element::new(Role::Button, "Minimieren", self.ui_px(zones::min_button(f, floating, t))).key("min").acts(&[Action::Press]));
        g = g.child(Element::new(Role::Toggle, "Schwebend", self.ui_px(zones::float_button(f, t))).key("float").state_if(State::Checked, floating).acts(&[Action::Toggle, Action::Press]));
        if let Place::Floating { index } = p.place {
            let max = self.ws.floating.get(index).is_some_and(|fl| fl.restore.is_some());
            g = g.child(Element::new(Role::Toggle, "Maximieren", self.ui_px(zones::max_button(f, t))).key("max").state_if(State::Checked, max).acts(&[Action::Toggle, Action::Press]));
        }
        if crate::veil::shown(self, &p.win) {
            let c = self.content_rect(f);
            g = g.child(Element::new(Role::Text, "Das Programm antwortet nicht", self.ui_px(c)).key("hung"));
            g = g.child(Element::new(Role::Button, "Beenden", self.ui_px(crate::veil::kill_button(c, t))).key("kill").acts(&[Action::Press]).asks(false));
            g = g.child(Element::new(Role::Button, "Warten", self.ui_px(crate::veil::wait_button(c, t))).key("wait").acts(&[Action::Press]));
        }
        let labels = header::labels(self, &p.win, p.place);
        for (i, &r) in strip.tabs.iter().enumerate() {
            let page = strip.first + i;
            let Some(tw) = self.tab_window(&p.win, p.place, page) else { continue };
            let label = labels.get(page).cloned().unwrap_or_default();
            let tab = Element::new(Role::Tab, label, self.ui_px(r)).key(format!("tab:{}", crate::ctl::win_id(&tw))).state_if(State::Selected, page == shown).acts(&[Action::Press]);
            g = g.child(tab.child(Element::new(Role::Button, "Tab schließen", self.ui_px(tabs::close_rect(r, t))).key("close").acts(&[Action::Press])));
        }
        if let Some((r, count)) = strip.more {
            g = g.child(Element::new(Role::Button, format!("{count} weitere Tabs"), self.ui_px(r)).key("more").acts(&[Action::Press]));
        }
        g
    }
}

/// What lies in a group that is not shown is not shown either.
fn hide_below(v: &mut [Element], hidden: bool) {
    for e in v {
        e.visible &= !hidden;
        let h = !e.visible;
        hide_below(&mut e.children, h);
    }
}

#[cfg(test)]
#[path = "ui_reg_tests.rs"]
mod tests;
