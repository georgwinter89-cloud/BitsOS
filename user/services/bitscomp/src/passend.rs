//! DB4: "Gerade passend" (DECISIONS 24.10.): what fits now from the habits and the context (ctxd `suggest`: the
//! time of day, the programs open, programs started together), as a row of its own in the library (after the
//! favourites, which never move), as a group in the address bar's suggestions, and as a row of pills on the sofa
//! mode's start screen. Every entry says why (the library's foot line and the sofa row show the reason of the
//! focused one, the address bar its address). The shown entries change only while none of these shows them
//! (`passend_settle`: never while Georg looks). The address bar's programs are ranked by how often he picks them.

use crate::habit_app::Pass;
use crate::layout::Rect;
use crate::library::Index;
use crate::library_view::{self as view, Row};
use crate::state::App;

pub const TITLE: &str = "Gerade passend";
pub const GROUP: &str = "GERADE PASSEND";
/// The sofa row: at most this many pills, under Zuletzt gespielt (board px).
pub const SOFA_MAX: usize = 3;
const SOFA_Y: i32 = 716 + 228 + 18;
const SOFA_H: i32 = 40;

/// The library entry a suggestion starts (a program by its app id, the sofa mode by its tile). A pair started
/// together has none: one tile cannot start both, so it is a proposal on the page only (DECISIONS 24.10.: new
/// things only proposed); while one of the two runs the other comes as a program of its own.
pub fn entry_id(ix: &Index, p: &Pass) -> Option<String> {
    if p.kind == "sofa" {
        return ix.by_address(crate::address::SOFA).map(|e| e.id.clone());
    }
    if p.kind == "together" {
        return None;
    }
    let app = p.app.as_deref()?;
    ix.entries.iter().find(|e| e.owns(app)).or_else(|| view::entry_of(ix, &format!("programm/{app}"))).map(|e| e.id.clone())
}

/// The library's row: programs of the suggestions, each once, none that is a favourite already.
pub fn library_row(ix: &Index, favourites: &[String], shown: &[Pass]) -> Option<Row> {
    let mut ids: Vec<String> = Vec::new();
    for p in shown {
        if let Some(id) = entry_id(ix, p)
            && !ids.contains(&id)
            && !view::pinned(ix, favourites, &id)
        {
            ids.push(id);
        }
    }
    (!ids.is_empty()).then_some(Row { title: TITLE, ids })
}

/// The row goes after the favourites (or first without them); a search shows none.
pub fn insert_row(rows: &mut Vec<Row>, row: Option<Row>) {
    let Some(row) = row else { return };
    if rows.first().is_some_and(|r| r.title == view::HITS) {
        return;
    }
    let at = rows.iter().position(|r| r.title == view::FAVOURITES).map_or(0, |i| i + 1);
    rows.insert(at, row);
}

/// The reason of the suggestion behind library entry `id`.
pub fn reason_for(ix: &Index, shown: &[Pass], id: &str) -> Option<String> {
    shown.iter().find(|p| entry_id(ix, p).as_deref() == Some(id)).map(|p| p.reason.clone())
}

/// The address bar's rows: with no query a group after the favourites; the programs by how often they were picked
/// (stable: equal ranks keep the library's order). The favourites stay first and in their order.
pub fn address_rows(mut rows: Vec<crate::suggest::Row>, query: &str, ix: &Index, shown: &[Pass], rank: &dyn Fn(&str) -> f64) -> Vec<crate::suggest::Row> {
    let progs: Vec<usize> = (0..rows.len()).filter(|i| rows[*i].group == "PROGRAMME").collect();
    if let (Some(&a), Some(&b)) = (progs.first(), progs.last()) {
        rows[a..=b].sort_by(|x, y| rank(&y.addr).total_cmp(&rank(&x.addr)));
    }
    if !query.trim().is_empty() {
        return rows;
    }
    let mut add: Vec<crate::suggest::Row> = Vec::new();
    for p in shown {
        let Some(e) = entry_id(ix, p).and_then(|id| ix.entries.iter().find(|e| e.id == id)) else { continue };
        if rows.iter().any(|r| r.group == "FAVORITEN" && r.addr == e.address) || add.iter().any(|r| r.addr == e.address) {
            continue;
        }
        // The reason stands beside the name: the row says why it is there.
        add.push(crate::suggest::Row { group: GROUP, name: format!("{} · {}", e.name, p.reason), addr: e.address.clone() });
    }
    rows.retain(|r| r.group == "FAVORITEN" || !add.iter().any(|a| a.addr == r.addr));
    let at = rows.iter().rposition(|r| r.group == "FAVORITEN").map_or(0, |i| i + 1);
    rows.splice(at..at, add);
    rows
}

/// The sofa row's pills (board px): `passend:N` with its rect, after the row's title.
pub fn sofa_items(names: &[String], m: crate::sofa_layout::Measure) -> Vec<crate::sofa_focus::Item> {
    use crate::sofa_layout::{PAD, Text};
    let mut x = PAD + m(TITLE, Text::Button) + 24;
    let mut v = Vec::new();
    for (i, n) in names.iter().take(SOFA_MAX).enumerate() {
        let w = m(n, Text::Button) + 40;
        v.push(crate::sofa_focus::Item::new(format!("passend:{i}"), Rect { x, y: SOFA_Y, w, h: SOFA_H }, crate::sofa_focus::Area::Content));
        x += w + 16;
    }
    v
}

/// The sofa row drawn on the start screen's body canvas (title, pills, the focused one's reason).
pub fn draw_sofa(c: &mut crate::canvas::Canvas, l: &crate::sofa_start_draw::StartLook, o: Rect, ink: &crate::sofa_draw::Ink) {
    use crate::sofa_draw::font;
    use crate::sofa_header_draw::{local, token_ring};
    use crate::sofa_layout::PAD;
    use crate::sofa_tile_draw::band;
    use crate::text::{Typeface, Weight};
    if l.passend.is_empty() {
        return;
    }
    let f = &l.frame;
    let k = f.k as f32;
    let txt = font(Typeface::Figtree, 18.0, Weight::SemiBold, k);
    let tw = crate::sofa_draw::measure(TITLE, crate::sofa_layout::Text::Button);
    c.text_fit(&txt, band(local(f, o, Rect { x: PAD, y: SOFA_Y, w: tw + 4, h: SOFA_H })), TITLE, ink.muted);
    let pills: Vec<(Rect, bool)> = (0..l.passend.len())
        .filter_map(|i| {
            let id = format!("passend:{i}");
            l.items.iter().find(|it| it.id == id).map(|it| (it.rect, l.focus.as_deref() == Some(id.as_str())))
        })
        .collect();
    let end = pills.iter().map(|(r, _)| r.x + r.w).max().unwrap_or(PAD + tw);
    for ((r, focused), (name, reason)) in pills.iter().zip(&l.passend) {
        let b = local(f, o, *r);
        c.rrect(b, 20.0 * k, ink.panel);
        c.rrect_line(b, 20.0 * k, 1.5 * k, ink.line);
        c.text_mid(&txt, band(b), name, ink.text);
        if *focused {
            token_ring(c, b, 20.0 * k, k, ink);
            let why = Rect { x: end + 24, y: SOFA_Y, w: (l.bw - PAD - end - 24).max(0), h: SOFA_H };
            c.text_fit(&font(Typeface::Figtree, 16.0, Weight::Medium, k), band(local(f, o, why)), reason, ink.muted);
        }
    }
}

impl App {
    /// The new suggestions (and pick ranks) become the shown ones while nothing shows them.
    pub fn passend_settle(&mut self) {
        if self.desk.edit.is_none() && self.habit.ranks != self.habit.ranks_new {
            self.habit.ranks = self.habit.ranks_new.clone();
        }
        let sofa_start = self.sofa.active && self.sofa.nav.screen == crate::sofa_focus::Screen::Start;
        if self.lib.open || self.desk.edit.is_some() || sofa_start {
            return;
        }
        if self.habit.shown != self.habit.passend {
            self.habit.shown = self.habit.passend.clone();
            self.needs_frame = true;
        }
    }

    /// The library's foot line for the focused (or pointed at) tile of the row: its reason.
    pub fn passend_library_note(&self) -> Option<String> {
        let rows = self.library_rows();
        let (r, id) = self.lib.hover.as_ref().or(self.lib.focus.as_ref())?;
        if rows.get(*r).is_none_or(|row| row.title != TITLE) {
            return None;
        }
        reason_for(&self.library, &self.habit.shown, id)
    }

    /// The sofa row's entries (programs only: the sofa mode is there already), name and reason.
    pub fn passend_sofa(&self) -> Vec<(String, String)> {
        self.habit.shown.iter().filter(|p| p.kind != "sofa" && entry_id(&self.library, p).is_some()).take(SOFA_MAX).map(|p| (p.name.clone(), p.reason.clone())).collect()
    }

    /// A on a sofa pill: its program by the launcher rule, counted as a pick.
    pub fn passend_sofa_run(&mut self, id: &str) -> bool {
        let Some(n) = id.strip_prefix("passend:").and_then(|n| n.parse::<usize>().ok()) else { return false };
        let shown: Vec<Pass> = self.habit.shown.iter().filter(|p| p.kind != "sofa" && entry_id(&self.library, p).is_some()).cloned().collect();
        let Some(e) = shown.get(n).and_then(|p| entry_id(&self.library, p)) else { return true };
        let addr = self.library.entries.iter().find(|x| x.id == e).map(|x| x.address.clone()).unwrap_or_default();
        self.habit_pick("passend", &addr);
        let t = crate::sofa_model::Tile { act: crate::sofa_model::Act::Start { id: e.clone(), extra: Vec::new(), restore: true }, ..crate::sofa_body::model_tile(&e, &e) };
        self.sofa_run(&t);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pass(kind: &str, app: Option<&str>, action: Option<&str>) -> Pass {
        Pass { kind: kind.into(), app: app.map(str::to_string), action: action.map(str::to_string), name: "N".into(), reason: "weil".into() }
    }

    #[test]
    fn the_row_comes_after_the_favourites_and_never_holds_a_favourite() {
        let ix = Index::builtin();
        let term = pass("program", Some("foot"), None);
        let row = library_row(&ix, &[], std::slice::from_ref(&term)).expect("Terminal owns foot");
        assert_eq!(row.title, TITLE);
        assert!(library_row(&ix, &["programm/terminal".into()], std::slice::from_ref(&term)).is_none(), "a favourite stays where it is");
        let fav = Row { title: view::FAVOURITES, ids: vec!["x".into()] };
        let all = Row { title: view::ALL, ids: vec!["y".into()] };
        let mut rows = vec![fav.clone(), all.clone()];
        insert_row(&mut rows, Some(row.clone()));
        assert_eq!(rows.iter().map(|r| r.title).collect::<Vec<_>>(), [view::FAVOURITES, TITLE, view::ALL]);
        let mut hits = vec![Row { title: view::HITS, ids: vec!["x".into()] }];
        insert_row(&mut hits, Some(row));
        assert_eq!(hits.len(), 1, "a search shows its hits only");
        let id = entry_id(&ix, &term).unwrap();
        assert_eq!(reason_for(&ix, &[term], &id).as_deref(), Some("weil"));
        assert_eq!(entry_id(&ix, &pass("together", None, Some("start:foot+steam"))), None, "one tile cannot start both");
        assert!(library_row(&ix, &[], &[pass("together", None, Some("start:foot+steam"))]).is_none());
    }

    #[test]
    fn the_address_bar_ranks_programs_and_keeps_favourites_first() {
        use crate::suggest::Row as S;
        let r = |g: &'static str, a: &str| S { group: g, name: a.into(), addr: a.into() };
        let rows = vec![r("FAVORITEN", "einstellungen/aussehen"), r("PROGRAMME", "programm/a"), r("PROGRAMME", "programm/b"), r("ZULETZT BENUTZT", "x/y")];
        let rank = |a: &str| if a == "programm/b" { 3.0 } else { 0.0 };
        let ix = Index::builtin();
        let out = address_rows(rows.clone(), "", &ix, &[], &rank);
        assert_eq!(out.iter().map(|x| x.addr.as_str()).collect::<Vec<_>>(), ["einstellungen/aussehen", "programm/b", "programm/a", "x/y"]);
        let out = address_rows(rows, "", &ix, &[pass("program", Some("foot"), None)], &|_| 0.0);
        assert_eq!(out[1].group, GROUP, "after the favourites");
        assert_eq!(out[1].addr, "programm/terminal");
        assert!(out[1].name.ends_with(" · weil"), "with its reason: {}", out[1].name);
    }
}
