//! DB1: a settings page's elements (and the page "KI", the programme page): from the page's model (page.rs Row and
//! Kind) and its layout, the same the drawing and the hit test use. A row's element carries its label, value, kind
//! and whether a change asks first (`confirm`); a Secret row never carries its value, nor what is typed into it.

use uilist::{Action, Element, Role, State};

use crate::layout::Rect;
use crate::page::{FAVOURITES, Kind, Layout, Part, Row, SWATCHES};
use crate::page_kind::PageKind;
use crate::state::App;

fn on(v: &str) -> bool {
    matches!(v, "true" | "1" | "on" | "yes")
}

impl App {
    pub(crate) fn ui_page(&self, pid: u32, content: Rect) -> Vec<Element> {
        let Some(s) = self.pages.by_pid.get(&pid) else { return Vec::new() };
        if s.kind == PageKind::Programme {
            return self.ui_programme(pid, content);
        }
        if s.kind == PageKind::System {
            return self.ui_system(pid, content);
        }
        let Some(lay) = self.page_layout(pid, content) else { return Vec::new() };
        let ki = s.kind == PageKind::Ki;
        let rows = &self.page_rows_of(pid).rows;
        let values: Vec<String> = if ki { self.ki.built.values.clone() } else { rows.iter().map(|r| crate::page_app::value_of(self, r)).collect() };
        // The text of the field being edited, whichever part it is (a row's field, the accent's hex, a new favourite).
        let editing = |p: Part| match &s.edit {
            Some((q, ed)) if *q == p => Some(ed.text.clone()),
            _ => None,
        };
        let dialog = s.confirm.is_some();
        let shown = |r: Rect| r.y >= lay.head.y + lay.head.h && r.y + r.h <= lay.content.y + lay.content.h;
        let mut v = vec![Element::new(Role::Heading, if ki { "KI" } else { "Einstellungen" }, self.ui_px(lay.title)).key("title").value(s.address.clone())];
        if let Some(r) = lay.part(Part::Undo) {
            v.push(self.ui_part(Element::new(Role::Button, "Rückgängig", [0; 4]).key("undo").acts(&[Action::Press]), r, dialog, true));
        }
        if let Some(r) = &lay.rail {
            v.extend(self.ui_rail(r, editing(Part::Search), dialog));
        }
        if let Some((m, err)) = &s.msg {
            v.push(Element::new(Role::Text, if *err { "Fehler" } else { "Meldung" }, self.ui_px(lay.msg)).key("msg").value(m.clone()));
        }
        for (title, r) in &lay.heads {
            v.push(Element::new(Role::Heading, title.clone(), self.ui_px(*r)).key(format!("head:{title}")).shown(shown(*r)));
        }
        for (i, row) in rows.iter().enumerate() {
            let Some(rr) = lay.rows.get(i).copied() else { continue };
            let val = values.get(i).cloned().unwrap_or_default();
            // WY-SET: a row of the tabs is named by settingsd's description, as before its short name.
            let named = s.shown.names.get(i).filter(|_| s.kind == PageKind::Settings).map(|n| Row { label: n.clone(), ..row.clone() });
            let e = self.ui_row(&lay, i, named.as_ref().unwrap_or(row), &val, &editing, dialog).shown(shown(rr));
            v.push(e);
        }
        if !ki {
            let favs = self.ui_favs(&lay, dialog, &shown, editing(Part::FavField));
            v.push(favs);
        }
        if let Some(d) = &s.confirm {
            let parts = crate::page_confirm::parts(lay.content, d);
            let at = |p: Part| parts.iter().find(|(q, _)| *q == p).map_or(lay.content, |(_, r)| *r);
            v.push(self.ui_question(&d.title, &d.lines, lay.content, ("Abbrechen", at(Part::DialogCancel)), (&d.ok, at(Part::DialogOk)), d.on_ok, "dialog"));
        }
        v
    }

    /// WY-SET: the settings' rail: the search field, the tabs (the shown one selected, its count or hits as the
    /// value), "Zeigen" of the changed keys, and the head's filter while a list is shown.
    fn ui_rail(&self, r: &crate::page_rail::Rail, typed: Option<String>, dialog: bool) -> Vec<Element> {
        let mut v = Vec::new();
        let search = Element::new(Role::Field, "Einstellungen durchsuchen", [0; 4]).key("search").acts(&[Action::Type, Action::Focus]);
        let search = match typed {
            Some(t) => search.value(t).with_state(State::Focused),
            None => search.value(r.query.clone()),
        };
        v.push(self.ui_part(search, r.search, dialog, true));
        if let Some(b) = r.clear {
            v.push(self.ui_part(Element::new(Role::Button, "Suche leeren", [0; 4]).key("search-clear").acts(&[Action::Press]), b, dialog, true));
        }
        for (i, b) in r.tabs.iter().enumerate() {
            let t = &crate::page_tabs::TABS[i];
            let n = r.hits.map_or(r.counts[i], |h| h[i]);
            let value = if r.hits.is_some() { format!("{n} Treffer") } else { format!("{n} Einstellungen") };
            let e = Element::new(Role::Tab, t.title, [0; 4]).key(format!("tab:{}", t.word)).value(value).state_if(State::Selected, r.tab == Some(i)).acts(&[Action::Press]);
            v.push(self.ui_part(e, *b, dialog, true));
        }
        if let Some(b) = r.show {
            let n = r.changed;
            let e = Element::new(Role::Button, "Geänderte zeigen", [0; 4]).key("changed").value(format!("{n} weichen vom Standard ab")).acts(&[Action::Press]);
            v.push(self.ui_part(e, b, dialog, true));
        }
        if let Some([a, b]) = r.filter {
            let mut e = Element::new(Role::Choice, "Filter", self.ui_px(Rect { w: b.x + b.w - a.x, ..a })).key("filter").value(if r.changed_only { "Nur geänderte" } else { "Alle" });
            for (on, rect, name) in [(false, a, "Alle"), (true, b, "Nur geänderte")] {
                let item = Element::new(Role::Item, name, [0; 4]).key(if on { "filter:geaendert" } else { "filter:alle" }).state_if(State::Selected, r.changed_only == on).acts(&[Action::Choose, Action::Press]);
                e = e.child(self.ui_part(item, rect, dialog, true));
            }
            v.push(e);
        }
        v
    }

    /// A control at its rect; while the page's dialog is open it takes no click (shown as disabled).
    fn ui_part(&self, e: Element, r: Rect, dialog: bool, visible: bool) -> Element {
        let mut e = e.state_if(State::Disabled, dialog).shown(visible);
        e.rect = self.ui_px(r);
        e
    }

    /// `typed`: the text of the field being edited, by its part (focused, and its value instead of the stored one).
    pub(crate) fn ui_row(&self, lay: &Layout, i: usize, row: &Row, val: &str, typed: &dyn Fn(Part) -> Option<String>, dialog: bool) -> Element {
        let part = |p: Part| lay.part(p);
        let rect = self.ui_px(lay.rows[i]);
        // settingsd's secret key: only its service sees the value, the list neither (whatever settingsd sends).
        if matches!(&row.kind, Kind::Fixed(why) if why == crate::page_schema::SECRET_NOTE) {
            return Element::secret(row.label.clone(), rect).key(format!("row:{}", row.key)).asks(row.confirm);
        }
        let field = |e: Element, p: Part| match typed(p) {
            Some(t) => e.value(t).with_state(State::Focused),
            None => e.value(val),
        };
        let ctl = |e: Element, p: Part| -> Element {
            match part(p) {
                Some(r) => self.ui_part(e, r, dialog, true),
                // Scrolled out: the layout places no control; `ui-act` scrolls to it first.
                None => e.state_if(State::Disabled, dialog).shown(false),
            }
        };
        let base = |role: Role| Element::new(role, row.label.clone(), rect).key(format!("row:{}", row.key)).asks(row.confirm);
        let mut e = match &row.kind {
            Kind::Switch => ctl(base(Role::Toggle).value(if on(val) { "an" } else { "aus" }).state_if(State::Checked, on(val)).acts(&[Action::Toggle, Action::Press]), Part::Switch(i)),
            Kind::Choice(opts) => {
                let now = opts.iter().find(|(v, _)| v == val).map_or(val.to_string(), |(_, n)| n.clone());
                let mut e = base(Role::Choice).value(now);
                for (o, (v, n)) in opts.iter().enumerate() {
                    e = e.child(ctl(Element::new(Role::Item, n.clone(), rect).key(format!("opt:{v}")).state_if(State::Selected, v == val).acts(&[Action::Choose, Action::Press]).asks(row.confirm), Part::Choice(i, o)));
                }
                e
            }
            Kind::Accent => {
                let mut e = base(Role::Choice).value(val);
                for (o, c) in SWATCHES.iter().enumerate() {
                    e = e.child(ctl(Element::new(Role::Item, *c, rect).key(format!("swatch:{c}")).state_if(State::Selected, *c == val).acts(&[Action::Choose, Action::Press]), Part::Swatch(o)));
                }
                e.child(ctl(field(Element::new(Role::Field, "Farbe als Hex", rect).key("hex").acts(&[Action::Type, Action::Focus]), Part::Hex), Part::Hex))
            }
            Kind::Number { unit, word, .. } => {
                let shown = match word {
                    Some((w, name)) if w == val => name.clone(),
                    _ if unit.is_empty() => val.to_string(),
                    _ => format!("{val} {unit}"),
                };
                let mut e = base(Role::Slider).value(shown);
                e = e.child(ctl(Element::new(Role::Button, "weniger", rect).key("less").acts(&[Action::Press]).asks(row.confirm), Part::Less(i)));
                e = e.child(ctl(field(Element::new(Role::Field, "Zahl", rect).key("field").acts(&[Action::Type, Action::Focus]).asks(row.confirm), Part::Field(i)), Part::Field(i)));
                e = e.child(ctl(Element::new(Role::Button, "mehr", rect).key("more").acts(&[Action::Press]).asks(row.confirm), Part::More(i)));
                if let Some((w, name)) = word {
                    e = e.child(ctl(Element::new(Role::Toggle, name.clone(), rect).key("word").state_if(State::Checked, w == val).acts(&[Action::Toggle, Action::Press]).asks(row.confirm), Part::Word(i)));
                }
                e
            }
            Kind::Field { .. } => ctl(field(base(Role::Field).acts(&[Action::Type, Action::Focus]), Part::Field(i)), Part::Field(i)),
            Kind::Fixed(why) => base(Role::Text).value(if why.is_empty() { val.to_string() } else { format!("{val} ({why})") }),
            Kind::Pills { opts, multi, .. } => {
                let chosen: Vec<&str> = if *multi { val.split(',').map(str::trim).collect() } else { vec![val] };
                let mut e = base(if *multi { Role::List } else { Role::Choice }).value(val);
                for (o, (v, n)) in opts.iter().enumerate() {
                    let item = Element::new(if *multi { Role::Toggle } else { Role::Item }, n.clone(), rect).key(format!("opt:{v}"));
                    let item = if *multi { item.state_if(State::Checked, chosen.contains(&v.as_str())).acts(&[Action::Toggle, Action::Press]) } else { item.state_if(State::Selected, v == val).acts(&[Action::Choose, Action::Press]) };
                    e = e.child(ctl(item.asks(row.confirm), Part::Pill(i, o)));
                }
                e
            }
            Kind::Secret { clear } => {
                // Never the stored value, never what is typed: only the field and its button.
                let mut e = ctl(Element::secret(row.label.clone(), rect).key(format!("row:{}", row.key)).acts(&[Action::Type, Action::Focus]).asks(row.confirm), Part::Field(i));
                if *clear {
                    e = e.child(ctl(Element::new(Role::Button, "Löschen", rect).key("clear").acts(&[Action::Press]).asks(true), Part::Button(i, 0)));
                }
                e
            }
            Kind::Buttons(b) => {
                let mut e = base(Role::Group);
                for (o, (name, text)) in b.iter().enumerate() {
                    e = e.child(ctl(Element::new(Role::Button, text.clone(), rect).key(format!("button:{name}")).acts(&[Action::Press]).asks(row.confirm), Part::Button(i, o)));
                }
                e
            }
            Kind::Link(addr) => ctl(base(Role::Link).value(addr.clone()).acts(&[Action::Press]), Part::Link(i)),
            Kind::Chart => base(Role::Image).value(val),
            Kind::Fold { open, count, .. } => ctl(base(Role::Button).value(format!("{count} Einstellungen")).state_if(State::Expanded, *open).acts(&[Action::Press]), Part::Fold(i)),
            Kind::Subs(subs) => {
                let vals: Vec<&str> = val.split(';').collect();
                let mut e = base(Role::Group).value(val);
                for (k, s) in subs.iter().enumerate() {
                    let v = vals.get(k).copied().unwrap_or("");
                    let item = if s.switch {
                        Element::new(Role::Toggle, s.label.clone(), rect).key(format!("sub:{}", s.key)).value(if on(v) { "an" } else { "aus" }).state_if(State::Checked, on(v)).acts(&[Action::Toggle, Action::Press])
                    } else {
                        let shown = if s.unit.is_empty() { v.to_string() } else { format!("{v} {}", s.unit) };
                        field(Element::new(Role::Field, s.label.clone(), rect).key(format!("sub:{}", s.key)).acts(&[Action::Type, Action::Focus]), Part::Sub(i, k)).value(typed(Part::Sub(i, k)).unwrap_or(shown))
                    };
                    e = e.child(ctl(item, Part::Sub(i, k)));
                }
                e
            }
        };
        // WY-SET: the list of changed keys and the search offer "Zurücksetzen".
        if lay.extra.get(i).is_some_and(|x| *x > 0) {
            e = e.child(ctl(Element::new(Role::Button, "Zurücksetzen", rect).key("reset").acts(&[Action::Press]).asks(row.confirm), Part::Reset(i)));
        }
        e
    }

    fn ui_favs(&self, lay: &Layout, dialog: bool, shown: &dyn Fn(Rect) -> bool, typed: Option<String>) -> Element {
        let mut g = Element::new(Role::List, "Favoriten", self.ui_px(lay.fav_add_row)).key(format!("row:{FAVOURITES}")).shown(shown(lay.fav_add_row));
        for (i, r) in lay.favs.iter().enumerate() {
            let a = self.desk.favourites.get(i).cloned().unwrap_or_default();
            let mut it = Element::new(Role::Item, crate::suggest::short_name(&a), self.ui_px(*r)).key(format!("fav:{a}")).value(a).shown(shown(*r));
            if let Some(b) = lay.part(Part::FavUp(i)) {
                it = it.child(self.ui_part(Element::new(Role::Button, "Nach oben", [0; 4]).key("up").acts(&[Action::Press]), b, dialog, true));
            }
            if let Some(b) = lay.part(Part::FavRemove(i)) {
                it = it.child(self.ui_part(Element::new(Role::Button, "Entfernen", [0; 4]).key("remove").acts(&[Action::Press]), b, dialog, true));
            }
            g = g.child(it);
        }
        if let Some(b) = lay.part(Part::FavField) {
            let mut f = Element::new(Role::Field, "Neuer Favorit", [0; 4]).key("fav-field").acts(&[Action::Type, Action::Focus]);
            if let Some(t) = typed {
                f = f.value(t).with_state(State::Focused);
            }
            g = g.child(self.ui_part(f, b, dialog, true));
        }
        if let Some(b) = lay.part(Part::FavAdd) {
            g = g.child(self.ui_part(Element::new(Role::Button, "Hinzufügen", [0; 4]).key("fav-add").acts(&[Action::Press]), b, dialog, true));
        }
        g
    }

    /// The programme page: each control by the name `ctl hot` gives it.
    fn ui_programme(&self, pid: u32, content: Rect) -> Vec<Element> {
        use crate::programme_view::ProgPart;
        let Some((items, lay)) = self.prog_layout(pid, content) else { return Vec::new() };
        let dialog = self.pages.by_pid.get(&pid).and_then(|s| s.confirm.as_ref());
        let mut v = Vec::new();
        if let Some(d) = dialog {
            let parts = crate::page_confirm::parts(lay.content, d);
            let at = |p: Part| parts.iter().find(|(q, _)| *q == p).map_or(lay.content, |(_, r)| *r);
            v.push(self.ui_question(&d.title, &d.lines, lay.content, ("Abbrechen", at(Part::DialogCancel)), (&d.ok, at(Part::DialogOk)), d.on_ok, "dialog"));
            return v;
        }
        for (part, r) in &lay.parts {
            let Part::Prog(p) = part else { continue };
            let name = crate::programme_view::part_name(&self.prog, &items, *p);
            let (role, acts): (Role, &[Action]) = match p {
                ProgPart::Search => (Role::Field, &[Action::Type, Action::Focus]),
                ProgPart::Tab(_) => (Role::Tab, &[Action::Press]),
                ProgPart::Tile(_) | ProgPart::Program(_) => (Role::Item, &[Action::Press]),
                _ => (Role::Button, &[Action::Press]),
            };
            let asks = matches!(p, ProgPart::Remove | ProgPart::Rollback | ProgPart::Clean);
            v.push(Element::new(role, name.clone(), self.ui_px(*r)).key(name).acts(acts).asks(asks));
        }
        v
    }
}
