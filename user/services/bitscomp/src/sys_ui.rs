//! SYS-A for Durchblick (DB1, contract C1): the program "System" as elements, from the same model and layout the
//! drawing and the hit test use. Tabs and pills with their state, each card as a group of its rows, the task
//! manager's search, filters, sortable heads, rows and detail; the steps that ask first (Vorrang Hoch, anything on a
//! BitsOS service) carry `confirm`, so an agent knows Georg answers them; refused ones are disabled.

use uilist::{Action, Element, Role, State};

use crate::layout::Rect;
use crate::state::App;
use crate::sys_model::Item;
use crate::sys_rules::Gate;
use crate::sys_view::{SysPart, part_name};

impl App {
    fn ui_items(&self, items: &[Item], rects: &[Rect], shown: &dyn Fn(Rect) -> bool) -> Vec<Element> {
        let mut v = Vec::new();
        for (it, r) in items.iter().zip(rects) {
            let px = self.ui_px(*r);
            let e = match it {
                Item::Label(s) | Item::LabelRight(s, _) => Element::new(Role::Heading, s.clone(), px),
                Item::Big(a, b) => Element::new(Role::Text, if b.is_empty() { "Wert".to_string() } else { b.clone() }, px).value(a.clone()),
                Item::Mid(a) => Element::new(Role::Text, "Wert", px).value(a.clone()),
                Item::Sub(s) => Element::new(Role::Text, s.clone(), px),
                Item::Row(k, val) | Item::RowTone(k, val, _) => Element::new(Role::Text, k.clone(), px).value(val.clone()),
                Item::Grid(g) => {
                    let mut e = Element::new(Role::Group, "Werte", px);
                    for (k, val) in g {
                        e = e.child(Element::new(Role::Text, k.clone(), px).value(val.clone()));
                    }
                    e
                }
                Item::Chart(_) | Item::Threads(_) => Element::new(Role::Picture, "Verlauf", px),
                Item::Link(s, p) => Element::new(Role::Link, s.clone(), px).key(part_name(*p)).acts(&[Action::Press]),
                Item::Buttons(bs) => {
                    for (k, (label, p, on)) in bs.iter().enumerate() {
                        let n = bs.len().max(1) as i32;
                        let bw = (r.w - (n - 1) * 10) / n;
                        let br = Rect { x: r.x + k as i32 * (bw + 10), w: bw, ..*r };
                        v.push(Element::new(Role::Button, label.clone(), self.ui_px(br)).key(part_name(*p)).acts(&[Action::Press]).state_if(State::Disabled, !on).shown(shown(br)));
                    }
                    continue;
                }
                Item::Bar(_) | Item::Legend(_) | Item::Rule | Item::Fill => continue,
            };
            v.push(e.shown(shown(*r)));
        }
        v
    }

    pub(crate) fn ui_system(&self, pid: u32, content: Rect) -> Vec<Element> {
        let Some((m, l)) = self.sys_layout(pid, content) else { return Vec::new() };
        let Some(s) = self.pages.by_pid.get(&pid) else { return Vec::new() };
        let mut v = vec![Element::new(Role::Heading, "System", self.ui_px(l.title)).key("title").value(s.address.clone())];
        if let Some(d) = &s.confirm {
            let parts = crate::page_confirm::parts(content, d);
            let at = |p: crate::page::Part| parts.iter().find(|(q, _)| *q == p).map_or(content, |(_, r)| *r);
            v.push(self.ui_question(&d.title, &d.lines, content, ("Abbrechen", at(crate::page::Part::DialogCancel)), (&d.ok, at(crate::page::Part::DialogOk)), d.on_ok, "dialog"));
            return v;
        }
        let body = l.body;
        let shown = move |r: Rect| r.y >= body.y && r.y + r.h <= body.y + body.h;
        for (t, r) in &l.tabs {
            v.push(Element::new(Role::Tab, t.name(), self.ui_px(*r)).key(part_name(SysPart::Tab(*t))).acts(&[Action::Press]).state_if(State::Selected, *t == s.sys.tab));
        }
        for (rg, r) in &l.ranges {
            v.push(Element::new(Role::Choice, rg.name(), self.ui_px(*r)).key(part_name(SysPart::Range(*rg))).acts(&[Action::Press]).state_if(State::Checked, *rg == s.sys.range));
        }
        if let Some((st, _)) = &m.status {
            v.push(Element::new(Role::Text, "Zustand", self.ui_px(l.status.unwrap_or(l.sub))).key("status").value(st.clone()));
        }
        if let Some((msg, err)) = &s.msg {
            v.push(Element::new(Role::Text, if *err { "Fehler" } else { "Meldung" }, self.ui_px(l.msg)).key("msg").value(msg.clone()));
        }
        let cards: Vec<&crate::sys_model::Card> = m.rows.iter().flat_map(|r| r.cards.iter()).collect();
        for (k, (card, rect)) in cards.iter().zip(&l.cards).enumerate() {
            let name = card.items.iter().find_map(|i| if let Item::Label(s) | Item::LabelRight(s, _) = i { Some(s.clone()) } else { None }).unwrap_or_else(|| format!("Karte {}", k + 1));
            let mut g = Element::new(Role::Group, name, self.ui_px(*rect)).key(format!("card:{k}")).shown(shown(*rect));
            for e in self.ui_items(&card.items, l.items.get(k).map_or(&[][..], |v| v.as_slice()), &shown) {
                g = g.child(e);
            }
            v.push(g);
        }
        if let (Some(pl), Some(pm)) = (&l.procs, &m.procs) {
            v.push(Element::new(Role::Field, "Prozess suchen", self.ui_px(pl.search)).key("search").value(pm.query.clone()).acts(&[Action::Type, Action::Focus]));
            for ((f, r), n) in pl.filters.iter().zip(pm.counts) {
                v.push(Element::new(Role::Choice, format!("{} {n}", f.name()), self.ui_px(*r)).key(part_name(SysPart::Filter(*f))).acts(&[Action::Press]).state_if(State::Checked, *f == pm.filter));
            }
            for (c, r) in &pl.heads {
                v.push(Element::new(Role::Button, format!("Sortieren nach {}", c.head()), self.ui_px(*r)).key(part_name(SysPart::Sort(*c))).acts(&[Action::Press]).state_if(State::Selected, *c == pm.sort));
            }
            let mut list = Element::new(Role::List, "Prozesse", self.ui_px(pl.rows_area)).key("procs").acts(&[Action::Scroll]);
            for (at, r) in &pl.lines {
                let crate::sys_layout::LineAt::Line(i, j) = at else { continue };
                let ln = &pm.sections[*i].lines[*j];
                let val = format!("{} · CPU {} · Speicher {} · Grafik {} · Platte {}", ln.state, ln.cells[0], ln.cells[1], ln.cells[2], ln.cells[3]);
                list = list.child(Element::new(Role::Item, ln.name.clone(), self.ui_px(*r)).key(part_name(SysPart::Row(ln.pid))).value(val).acts(&[Action::Press]).state_if(State::Selected, ln.selected).shown(shown(*r)));
            }
            v.push(list);
            if let Some(d) = &pm.detail {
                let mut g = Element::new(Role::Group, format!("Ausgewählt: {}", d.name), self.ui_px(pl.detail)).key("detail").value(d.line.clone());
                for ((k, val), r) in d.rows.iter().zip(&pl.d_rows) {
                    g = g.child(Element::new(Role::Text, k.clone(), self.ui_px(*r)).value(val.clone()));
                }
                for (k, (vr, r)) in pl.d_prio.1.iter().enumerate() {
                    let gate = &d.prio[k];
                    g = g.child(
                        Element::new(Role::Choice, format!("Vorrang {}", vr.name()), self.ui_px(*r))
                            .key(part_name(SysPart::Prio(*vr)))
                            .acts(&[Action::Press])
                            .state_if(State::Checked, *vr == d.vorrang)
                            .state_if(State::Disabled, matches!(gate, Gate::No(_)))
                            .asks(*gate == Gate::Ask),
                    );
                }
                g = g.child(Element::new(Role::Link, "Was hat es gelesen und verbunden?", self.ui_px(pl.d_who)).key(part_name(SysPart::WhoDid)).acts(&[Action::Press]));
                g = g.child(Element::new(Role::Button, "Fenster zeigen", self.ui_px(pl.d_buttons.0)).key(part_name(SysPart::Show)).acts(&[Action::Press]).state_if(State::Disabled, !d.show));
                g = g.child(
                    Element::new(Role::Button, "Beenden", self.ui_px(pl.d_buttons.1))
                        .key(part_name(SysPart::End))
                        .acts(&[Action::Press])
                        .state_if(State::Disabled, matches!(d.end, Gate::No(_)))
                        .asks(d.end == Gate::Ask),
                );
                g = g.child(Element::new(Role::Text, d.note.clone(), self.ui_px(pl.d_note)));
                v.push(g);
            }
        }
        if let (Some(dl), Some(dm)) = (&l.devs, &m.devices) {
            let mut list = Element::new(Role::List, "Geräte", self.ui_px(dl.list)).key("devices").acts(&[Action::Scroll]);
            let all: Vec<&(String, String, crate::sys_model::Tone)> = dm.groups.iter().flat_map(|g| g.1.iter()).collect();
            for (dev, _, r) in &dl.lines {
                let Some(k) = dev else { continue };
                if let Some((n, st, _)) = all.get(*k) {
                    list = list.child(Element::new(Role::Item, n.clone(), self.ui_px(*r)).key(part_name(SysPart::Device(*k))).value(st.clone()).acts(&[Action::Press]).state_if(State::Selected, *k == dm.selected));
                }
            }
            v.push(list);
            let mut g = Element::new(Role::Group, "Gerät", self.ui_px(dl.detail)).key("device");
            for e in self.ui_items(&dm.detail, &dl.d_items, &shown) {
                g = g.child(e);
            }
            v.push(g);
            let mut f = Element::new(Role::Group, "BitsOS", self.ui_px(dl.facts)).key("facts");
            for e in self.ui_items(&dm.facts, &dl.f_items, &shown) {
                f = f.child(e);
            }
            v.push(f);
        }
        v
    }
}
