//! DB1: the `desktop` window's elements: the top bar (address, buttons, workspaces, favourites, minimised
//! windows), the status words, the Bühne's own buttons, the notice, the start page, the address suggestions, the
//! circle menu, and every overlay and dialog that is open (library, Feld, rewind, workspace question, the list
//! behind "+N", the assistant's sidebar, the installer's and the access dialogs). Same layouts as `ctl hot`.

use uilist::{Action, Element, Role, State};

use crate::layout::Rect;
use crate::state::App;

/// The address field's key under the top bar (`ui_act` finds it by its id).
pub const ADDRESS_KEY: &str = "address";

/// The id the address field has in the desktop window.
pub fn address_id() -> String {
    uilist::Ids::new(crate::ui_reg::DESKTOP).id(&format!("/top/{ADDRESS_KEY}"))
}

fn press(role: Role, name: impl Into<String>, r: [i32; 4], key: impl Into<String>) -> Element {
    Element::new(role, name, r).key(key).acts(&[Action::Press])
}

impl App {
    pub(crate) fn ui_desktop(&self) -> Vec<Element> {
        // The bars are not drawn under a fullscreen window or a sofa screen (paint::order_with): not shown there.
        let chrome = !self.ui_chrome_covered();
        let mut v = vec![self.ui_top_bar().shown(chrome)];
        if let Some(b) = self.ui_buehne() {
            v.push(b.shown(chrome));
        }
        if let Some((n, _)) = &self.desk.notice {
            v.push(Element::new(Role::Text, "Hinweis", self.ui_px(self.chrome().address)).key("notice").value(n.clone()));
        }
        if let (Some(n), Some(b)) = (self.prog_note_shown(), self.prog_note_boxes()) {
            v.push(Element::new(Role::Text, "Programmfehler", self.ui_px(b.card)).key("prog-note").value(format!("{} {}", n.title, n.reason)));
        }
        if self.start_shown() {
            v.push(self.ui_start().shown(chrome));
        }
        v.extend(self.ui_suggestions());
        v.extend(self.ui_menu());
        v.extend(self.ui_library());
        v.extend(self.ui_feld());
        v.extend(self.ui_rewind());
        v.extend(self.ui_min_list());
        v.extend(self.ui_ws_ask());
        v.extend(self.ui_assist());
        v.extend(self.ui_dialogs());
        // DB3: Durchblick's panel.
        v.extend(self.ui_durchblick());
        v.extend(self.ui_durchblick_bar());
        v
    }

    fn ui_top_bar(&self) -> Element {
        let c = self.chrome();
        let px = |r: Rect| self.ui_px(r);
        let full = self.area_full();
        // The Bühne keeps some of the classic bar's parts out of sight (no height, below the screen).
        let real = |r: Rect| r.w > 0 && r.h > 0 && r.y < full.y + full.h;
        let mut g = Element::new(Role::Group, "Obere Leiste", px(c.top)).key("top");
        for (r, name, key) in [(c.back, "Zurück", "back"), (c.forward, "Vor", "forward")] {
            if real(r) {
                g = g.child(press(Role::Button, name, px(r), key));
            }
        }
        g = g.child(press(Role::Button, "Programme", px(c.programs), "programs").state_if(State::Expanded, self.lib.open));
        let (shown, editing) = match &self.desk.edit {
            Some(e) => (e.text.clone(), true),
            None => (self.shown_address().unwrap_or_default(), false),
        };
        // Typing starts with Ctrl+Alt+L as Georg's does (`ui_act`); the Bühne draws the address read-only.
        let at = if real(self.edit_field()) { self.edit_field() } else { self.buehne().map_or(self.edit_field(), |b| b.address) };
        g = g.child(Element::new(Role::Field, "Adresszeile", px(at)).key(ADDRESS_KEY).value(shown).state_if(State::Focused, editing).acts(&[Action::Type]));
        if real(c.ring) {
            g = g.child(press(Role::Button, "Ring", px(c.ring), "ring"));
        }
        if real(c.home) {
            g = g.child(press(Role::Button, "Startseite", px(c.home), "home").state_if(State::Selected, self.start_shown()));
        }
        g = g.child(press(Role::Button, "Neuer Arbeitsraum", px(c.ws_new), "wsnew"));
        // The Bühne has no list frame (its chips stand in the header): the list spans its entries.
        let span = c.ws_items.iter().chain(&c.min_items).copied().filter(|r| real(*r)).reduce(|a, b| {
            let (x, y) = (a.x.min(b.x), a.y.min(b.y));
            Rect { x, y, w: (a.x + a.w).max(b.x + b.w) - x, h: (a.y + a.h).max(b.y + b.h) - y }
        });
        let frame = if real(c.list) { c.list } else { span.unwrap_or(c.top) };
        let mut list = Element::new(Role::List, "Arbeitsräume", px(frame)).key("workspaces").state_if(State::Expanded, c.list_shown);
        for (i, r) in c.ws_items.iter().enumerate().take(if c.list_shown { c.ws_shown() } else { 0 }) {
            let name = self.shown_names().get(i).cloned().unwrap_or_default();
            let mut it = press(Role::Item, name, px(*r), format!("ws:{i}")).state_if(State::Selected, i == self.desk.active);
            if let Some(x) = c.ws_close.get(i).filter(|_| self.ws_closable(i)) {
                it = it.child(press(Role::Button, "Arbeitsraum schließen", px(*x), "close").asks(true));
            }
            list = list.child(it);
        }
        for (i, r) in c.min_items.iter().enumerate().take(if c.list_shown { self.ws.minimized.len() } else { 0 }) {
            let w = &self.ws.minimized[i].win;
            let mut it = press(Role::Item, crate::minimize::label(w), px(*r), format!("min:{}", crate::ctl::win_id(w))).value("minimiert");
            if let Some(x) = c.min_close.get(i) {
                it = it.child(press(Role::Button, "Fenster schließen", px(*x), "close"));
            }
            list = list.child(it);
        }
        if let Some(r) = c.min_more {
            let n = self.ws.minimized.len() - c.min_items.len().min(self.ws.minimized.len());
            list = list.child(press(Role::Button, format!("{n} weitere minimierte Fenster"), px(r), "minmore").state_if(State::Expanded, self.desk.min_list.open));
        }
        g = g.child(list);
        if !c.fav_items.is_empty() {
            let mut favs = Element::new(Role::List, "Favoriten", px(c.favs.unwrap_or(c.top))).key("favs");
            for (i, r) in c.fav_items.iter().enumerate() {
                let a = self.desk.favourites.get(i).cloned().unwrap_or_default();
                favs = favs.child(press(Role::Link, crate::chrome::fav_label(&a).to_string(), px(*r), format!("fav:{a}")).value(a));
            }
            g = g.child(favs);
        }
        if let Some(s) = c.status {
            let mut st = Element::new(Role::Group, "Statusleiste", px(s)).key("status");
            for b in &self.status.blocks {
                st = st.child(Element::new(Role::Text, b.label.clone(), px(s)).key(format!("block:{}", b.label)).value(b.value.clone()));
            }
            st = st.child(Element::new(Role::Text, "Uhr", px(s)).key("clock").value(self.status.clock.clone()));
            g = g.child(st);
        }
        g
    }

    fn ui_buehne(&self) -> Option<Element> {
        let b = self.buehne()?;
        let px = |r: Rect| self.ui_px(r);
        let mut g = Element::new(Role::Group, "Bühne", px(b.header)).key("buehne");
        g = g.child(press(Role::Button, if self.theme.dark { "Hell" } else { "Dunkel" }, px(b.sun), "sun"));
        g = g.child(press(Role::Button, "Mitteilungen", px(b.bell), "bell"));
        if b.bottom.is_some() {
            g = g.child(press(Role::Button, "Feld", px(b.feld), "feld").state_if(State::Expanded, self.feld.open));
        }
        if let Some(r) = b.rewind {
            g = g.child(press(Role::Button, "Zurückspulen", px(r), "rewind").state_if(State::Expanded, self.rewind.open));
        }
        if let Some(r) = b.more {
            g = g.child(press(Role::Button, "Weitere Arbeitsräume", px(r), "more"));
        }
        Some(g)
    }

    fn ui_start(&self) -> Element {
        let p = self.start_page();
        let px = |r: Rect| self.ui_px(r);
        let can = self.session.offered && self.session.saved.is_some();
        let mut g = Element::new(Role::Group, "Startseite", px(self.chrome().work)).key("start");
        g = g.child(press(Role::Button, "Sitzung wiederherstellen", px(p.restore), "restore").state_if(State::Disabled, !can));
        g = g.child(press(Role::Button, "Neu beginnen", px(p.new), "new"));
        for (i, r) in p.favs.iter().enumerate().take(self.desk.favourites.len()) {
            let a = &self.desk.favourites[i];
            g = g.child(press(Role::Link, crate::suggest::short_name(a), px(*r), format!("fav:{a}")).value(a.clone()));
        }
        for (i, r) in p.recents.iter().enumerate().take(self.session.recent.len()) {
            let a = &self.session.recent[i].addr;
            g = g.child(press(Role::Link, self.recent_name(a), px(*r), format!("recent:{a}")).value(a.clone()));
        }
        g
    }

    fn ui_suggestions(&self) -> Option<Element> {
        let rows = self.suggestions();
        if rows.is_empty() {
            return None;
        }
        let (_, placed) = crate::suggest::layout(&rows, self.edit_field(), self.area_full().h - 8);
        let mut l = Element::new(Role::List, "Vorschläge", self.ui_px(self.edit_field())).key("suggest");
        for ((_, r), row) in placed.iter().zip(&rows) {
            l = l.child(press(Role::Item, row.name.clone(), self.ui_px(*r), format!("{}:{}", row.group, row.addr)).value(row.addr.clone()));
        }
        Some(l)
    }

    fn ui_menu(&self) -> Option<Element> {
        let m = self.menu.as_ref()?;
        let t = &self.theme;
        let (cx, cy) = (m.centre.0 as i32, m.centre.1 as i32);
        let mut g = Element::new(Role::Menu, "Kreismenü", self.ui_px(Rect { x: cx - 1, y: cy - 1, w: 2, h: 2 })).key("menu").state_if(State::Expanded, true);
        for (d, it) in m.items.iter().enumerate() {
            if let Some(it) = it {
                let r = crate::menu::item_rect(d, t);
                let at = self.ui_px(Rect { x: cx + r.x, y: cy + r.y, ..r });
                g = g.child(press(Role::Menuitem, it.label, at, format!("item:{}", it.label)).state_if(State::Disabled, !it.enabled));
            }
        }
        Some(g)
    }

    fn ui_library(&self) -> Option<Element> {
        if !self.lib.open {
            return None;
        }
        let g = self.library_geo();
        let px = |r: Rect| self.ui_px(r);
        let mut d = Element::new(Role::Group, "Programme", px(g.panel)).key("library");
        d = d.child(Element::new(Role::Field, "Suche", px(g.search)).key("search").value(self.lib.query.clone()).state_if(State::Focused, true).acts(&[Action::Type]));
        let h = self.library_head();
        d = d.child(press(Role::Button, "Hinzufügen", px(h.add), "add"));
        if let Some(r) = h.updates {
            d = d.child(press(Role::Button, "Aktualisierungen", px(r), "updates").value(self.library_updates_count().unwrap_or(0).to_string()));
        }
        d = d.child(press(Role::Button, "Schließen", px(h.esc), "esc"));
        if self.library_empty() {
            let link = crate::library_view::empty(&g, &self.theme).1;
            d = d.child(press(Role::Link, "Programm suchen und hinzufügen", px(link), "add-search").value(self.lib.query.trim().to_string()));
        }
        let (rows, heads, tiles) = self.library_tiles();
        for (r, title) in heads {
            d = d.child(Element::new(Role::Heading, title, px(r)).key(format!("head:{title}")));
        }
        for t in tiles {
            let name = self.library.entries.iter().find(|e| e.id == t.id).map_or_else(|| t.id.clone(), |e| e.name.clone());
            let row = rows.get(t.row).map_or("", |r| r.title);
            let focus = self.lib.focus.as_ref().is_some_and(|(_, id)| *id == t.id);
            d = d.child(press(Role::Item, name, px(t.rect), format!("tile:{row}:{}", t.id)).value(t.id.clone()).state_if(State::Focused, focus));
        }
        Some(d)
    }

    fn ui_feld(&self) -> Option<Element> {
        if !self.feld.open {
            return None;
        }
        let (g, rows, placed) = self.feld_placed();
        let px = |r: Rect| self.ui_px(r);
        let mut d = Element::new(Role::Group, "Feld", px(g.panel)).key("feld");
        d = d.child(Element::new(Role::Field, "Eingabe", px(g.text)).key("text").value(self.feld.edit.text.clone()).state_if(State::Focused, true).acts(&[Action::Type]));
        d = d.child(press(Role::Button, "Schließen", px(g.esc), "esc"));
        let pick = self.feld.pick(rows.len());
        for (i, (r, (_, at))) in rows.iter().zip(&placed).enumerate() {
            d = d.child(press(Role::Item, r.title.clone(), px(*at), format!("row:{}:{}", r.kind.name(), r.target)).value(r.detail.clone()).state_if(State::Selected, pick == Some(i)));
        }
        Some(d)
    }

    fn ui_rewind(&self) -> Option<Element> {
        use crate::rewind::Hot;
        if !self.rewind.open {
            return None;
        }
        let r = &self.rewind;
        let mut d = Element::new(Role::Group, "Zurückspulen", self.ui_px(self.area_full())).key("rewind");
        for (h, rect) in self.rewind_parts() {
            let at = self.ui_px(rect);
            let e = match h {
                Hot::Back => press(Role::Button, "Zurück", at, "back"),
                Hot::Now => press(Role::Button, "Jetzt", at, "now"),
                Hot::Older => press(Role::Button, "Älter", at, "older"),
                Hot::Newer => press(Role::Button, "Neuer", at, "newer"),
                Hot::Chip(i) => press(Role::Item, format!("Stand {}", r.snaps[i].id), at, format!("chip:{}", r.snaps[i].id)).state_if(State::Selected, i == r.pick),
                Hot::Way(t) => press(Role::Tab, t.name(), at, format!("way:{}", t.name())).state_if(State::Selected, t == r.tab),
                Hot::Up => press(Role::Button, "Eine Ebene höher", at, "up"),
                Hot::Row(i) if r.tab == crate::rewind::Tab::Path => press(Role::Item, r.browse.entries[i].name.clone(), at, format!("row:{}", r.browse.entries[i].name)).state_if(State::Selected, r.browse.pick == Some(i)),
                Hot::Row(i) => press(Role::Item, r.progs.list[i].name.clone(), at, format!("prog:{}", r.progs.list[i].name)).state_if(State::Selected, r.progs.pick == Some(i)),
                Hot::Do => press(Role::Button, "Zurückholen", at, "do").asks(true),
                Hot::Restart => press(Role::Button, "Neu starten", at, "restart").asks(true),
                Hot::Undo => press(Role::Button, "Rückgängig", at, "undo"),
                Hot::DialogCancel => press(Role::Button, "Abbrechen", at, "dialog-cancel"),
                Hot::DialogOk => press(Role::Button, "Bestätigen", at, "dialog-ok").settles(),
            };
            d = d.child(e);
        }
        Some(d)
    }

    fn ui_min_list(&self) -> Option<Element> {
        let b = self.chrome();
        if !(b.list_shown && self.desk.min_list.open) {
            return None;
        }
        let mut l = Element::new(Role::List, "Minimierte Fenster", self.ui_px(b.min_more.unwrap_or(b.list))).key("minlist").state_if(State::Expanded, true);
        for ((r, x), m) in b.min_list.iter().zip(&b.min_list_close).zip(&self.ws.minimized).filter(|((r, _), _)| r.w > 0) {
            let id = crate::ctl::win_id(&m.win);
            l = l.child(press(Role::Item, crate::minimize::label(&m.win), self.ui_px(*r), format!("min:{id}")).child(press(Role::Button, "Fenster schließen", self.ui_px(*x), "close")));
        }
        Some(l)
    }

    fn ui_ws_ask(&self) -> Option<Element> {
        let a = self.desk.ws_ask.as_ref()?;
        let (f, cancel, ok) = crate::page_confirm::boxes(self.area_full(), a.dialog.lines.len());
        Some(self.ui_question(&a.dialog.title, &a.dialog.lines, f, ("Abbrechen", cancel), (&a.dialog.ok, ok), a.dialog.on_ok, "wsask"))
    }

    /// A question with two buttons; `on_ok`: the keyboard's Enter would press the second.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn ui_question(&self, title: &str, lines: &[String], frame: Rect, no: (&str, Rect), yes: (&str, Rect), on_ok: bool, key: &str) -> Element {
        let mut d = Element::new(Role::Group, title, self.ui_px(frame)).key(key).state_if(State::Focused, true);
        if !lines.is_empty() {
            d = d.child(Element::new(Role::Text, "Text", self.ui_px(frame)).key("text").value(lines.join(" ")));
        }
        d = d.child(press(Role::Button, no.0, self.ui_px(no.1), "no").state_if(State::Focused, !on_ok));
        d.child(press(Role::Button, yes.0, self.ui_px(yes.1), "yes").state_if(State::Focused, on_ok).settles())
    }

    /// The installer's question, its grant dialog and the access dialog (only real input answers them).
    fn ui_dialogs(&self) -> Vec<Element> {
        let full = self.area_full();
        let i = &self.install;
        let mut v = Vec::new();
        if let Some(a) = &i.ask {
            let (no, yes) = crate::install_draw::ask_buttons(full, a);
            let mut q = self.ui_question(&a.title, std::slice::from_ref(&a.text), full, ("Nein", no), ("Ja", yes), a.on_yes, "install-ask");
            q.confirm = true;
            v.push(q);
        }
        if let Some(g) = &i.grant {
            let mut d = Element::new(Role::Group, "Erlaubnis", self.ui_px(full)).key("grant").state_if(State::Focused, true);
            for (stop, r) in crate::install_draw::grant_parts(full, g) {
                d = d.child(press(Role::Button, stop.label(), self.ui_px(r), format!("{stop:?}")).state_if(State::Focused, g.focus == stop));
            }
            v.push(d);
        }
        if let Some(a) = &i.access {
            use crate::access_dialog::{AStop, CHOICES};
            let mut d = Element::new(Role::Group, self.access_title(a), self.ui_px(full)).key("access").state_if(State::Focused, true).asks(true);
            let lines = self.access_lines();
            d = d.child(Element::new(Role::Text, "Text", self.ui_px(full)).key("text").value(lines.join(" ")));
            for (stop, r) in crate::access_dialog::parts(full, a, lines.len()) {
                let at = self.ui_px(r);
                d = d.child(match stop {
                    AStop::Choice(n) => press(Role::Choice, CHOICES[n.min(CHOICES.len() - 1)].1, at, format!("choice:{n}")).state_if(State::Selected, a.choice == n),
                    AStop::Nvme => press(Role::Toggle, "Interne SSD beschreibbar", at, "nvme").state_if(State::Checked, a.nvme),
                    AStop::Revoke => press(Role::Button, "Fernzugriff beenden", at, "revoke"),
                    AStop::No => press(Role::Button, "Ablehnen", at, "no"),
                    AStop::Yes => press(Role::Button, "Erlauben", at, "yes").settles(),
                });
            }
            v.push(d);
        }
        v
    }
}
