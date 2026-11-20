//! WY-SET: what the settings window shows now, built from the registry's rows (`page_schema::model`, with the
//! learned and the benchmark rows spliced in) and the page's state: one tab's cards (the important rows, then a
//! card's "Mehr" and, when open, what it holds), or the search's results over every key grouped by tab and card,
//! or the list of every key that differs from its default. Pure; `page_app` keeps the result per page.

use std::collections::BTreeSet;

use crate::page::{Flag, Heads, Kind, Model, Row, Section};
use crate::page_join::{self, Provider};
use crate::page_tabs::{self, CARDS, FAV_CARD, FOLD, Place, TABS};

/// The narrowest column the tabs lay out in two (the mock-up's two cards side by side).
pub const COL_MIN: i32 = 520;

/// What the view is built from besides the rows.
pub struct Ctx<'a> {
    pub tab: usize,
    pub query: &'a str,
    pub changed_only: bool,
    /// The cards whose "Mehr" Georg opened.
    pub open: &'a BTreeSet<String>,
    /// The key prefix the address points at: its card's "Mehr" opens with it.
    pub target: &'a str,
    /// What a row shows (`page_app::value_of`).
    pub value: &'a dyn Fn(&Row) -> String,
    /// settingsd has this key.
    pub known: &'a dyn Fn(&str) -> bool,
    pub providers: &'a [Provider],
    /// Programs the never-see list offers (running and known ones, `assistant.never_see`).
    pub programs: &'a [String],
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct View {
    pub model: Model,
    /// Per tab: how many rows it shows at once; while searching, how many hits it has.
    pub counts: [usize; 9],
    pub hits: Option<[usize; 9]>,
    /// Keys of settingsd that differ from their default, in all tabs.
    pub changed: usize,
    pub title: String,
    pub sub: String,
    /// Per row of `model`: the line under its name (settingsd's description when the name was shortened, the
    /// Rückfrage and restart notes); None: the row's own line.
    pub lines: Vec<Option<String>>,
    /// Per row: settingsd's description (DB1 names a row by it, as before the short names).
    pub names: Vec<String>,
}

/// Board 4's sentences under the rows of "Was er sieht" (the never-see list's programs are its "+ Programm
/// hinzufügen": the running ones are offered beside those on it).
pub const NEVER_SEE_LINE: &str = "Bei Win+F1 sieht er das Fenster, das vorne ist, ohne vorher zu fragen. Ein Klick nimmt ein Programm auf oder heraus.";
pub const SIDEBAR_SEES_LINE: &str = "Denselben Schalter gibt es direkt in der Seitenleiste.";

/// The line under a row the tabs show: its description when the name was shortened, what a change needs.
fn line_of(r: &Row, desc: &str) -> Option<String> {
    // EINS-B: the level's three rows carry their own sentences (board 4); the engine's description stays out.
    if r.key == crate::assist_level::LEVEL {
        return Some(crate::assist_views::LOOSEN_LINE.into());
    }
    if r.key == crate::assist_level::NEVER_SEE {
        return Some(NEVER_SEE_LINE.into());
    }
    if r.key == crate::assist_level::SIDEBAR_SEES {
        return Some(SIDEBAR_SEES_LINE.into());
    }
    if matches!(r.kind, Kind::Fold { .. }) || r.key.starts_with(crate::page_tabs::VIRTUAL) {
        return None;
    }
    let mut parts: Vec<&str> = Vec::new();
    if desc != r.label && !desc.trim().is_empty() {
        parts.push(desc);
    }
    if r.confirm {
        parts.push("ändern mit Rückfrage");
    }
    if !r.note.is_empty() {
        parts.push(&r.note);
    }
    let own = crate::gelernt::is_own(&r.key) || crate::leistung::is_own(&r.key);
    (!own).then(|| parts.join(" · "))
}

/// A row as the tabs show it: a plain short name, the providers' jobs as choices, a package source by its name,
/// the level as three rows with their sentences, the never-see list as programs to pick (board 4).
fn shaped(mut r: Row, providers: &[Provider], programs: &[String], value: &str) -> Row {
    use crate::assist_level::{self as lv, Level};
    if let Some(l) = crate::page_tabs::label_of(&r.key) {
        r.label = l.into();
    }
    if r.key == lv::LEVEL {
        let opts: Vec<(String, String)> = Level::ALL.iter().map(|l| (l.word().to_string(), l.name().to_string())).collect();
        let w = crate::page::pill_w(opts.iter().map(|(_, n)| n.as_str()));
        r.kind = Kind::Pills { opts, multi: false, w, notes: Level::ALL.iter().map(|l| l.sentence().to_string()).collect() };
    }
    if r.key == lv::NEVER_SEE {
        let mut words: Vec<String> = value.split(',').map(lv::program_word).filter(|w| !w.is_empty()).collect();
        words.extend(programs.iter().map(|p| lv::program_word(p)));
        let mut seen = BTreeSet::new();
        words.retain(|w| seen.insert(w.clone()));
        let opts: Vec<(String, String)> = words.into_iter().map(|w| (w.clone(), w)).collect();
        let w = crate::page::pill_w(opts.iter().map(|(_, n)| n.as_str()));
        r.kind = Kind::Pills { opts, multi: true, w, notes: Vec::new() };
    }
    if page_join::ROLES.contains(&r.key.as_str()) && matches!(r.kind, Kind::Field { .. }) {
        r.kind = page_join::role_kind(providers);
    }
    if let Some(src) = r.key.strip_prefix("pkg.sources.").and_then(|s| s.strip_suffix(".servers")) {
        r.short = format!("Quelle {src}");
        r.label = format!("{src}: Server");
    }
    r
}

/// A row counts as changed: a key of settingsd (not a secret) whose value is not its default.
fn changed(r: &Row, cx: &Ctx) -> bool {
    let secret = matches!(&r.kind, Kind::Fixed(why) if why == crate::page_schema::SECRET_NOTE);
    !secret && (cx.known)(&r.key) && (cx.value)(r) != r.default
}

fn hit(r: &Row, desc: &str, card: &str, tab: &str, q: &str) -> bool {
    let q = q.trim().to_lowercase();
    if q.is_empty() {
        return true;
    }
    let subs = match &r.kind {
        Kind::Subs(s) => s.iter().map(|s| format!("{} {}", s.label, s.key)).collect::<Vec<_>>().join(" "),
        _ => String::new(),
    };
    [r.label.as_str(), desc, r.key.as_str(), r.short.as_str(), card, tab, subs.as_str()].iter().any(|t| t.to_lowercase().contains(&q))
}

fn under(key: &str, prefix: &str) -> bool {
    !prefix.is_empty() && (key == prefix || key.starts_with(&format!("{prefix}.")))
}

fn fold_row(card: usize, open: bool, count: usize, changed: usize) -> Row {
    let c = &CARDS[card];
    Row {
        key: format!("{FOLD}{}", c.id),
        kind: Kind::Fold { open, count, changed },
        default: String::new(),
        label: c.fold.into(),
        short: c.fold.into(),
        note: String::new(),
        confirm: false,
        stack: false,
    }
}

pub fn build(full: &Model, cx: &Ctx) -> View {
    let keys: Vec<&str> = full.rows.iter().map(|r| r.key.as_str()).collect();
    let mut all: Vec<Row> = page_join::rows(cx.known, &keys, cx.providers);
    // PLAN K8: what the one assistant took away is never shown, whoever still registers it.
    all.extend(full.rows.iter().filter(|r| !page_tabs::gone(&r.key)).cloned());
    let all: Vec<(Place, Row)> = all.into_iter().map(|r| (page_tabs::place(&r.key), r)).collect();
    // settingsd's description of each key, before the tabs' short names replace it.
    let descs: std::collections::HashMap<String, String> = all.iter().map(|(_, r)| (r.key.clone(), r.label.clone())).collect();
    let all: Vec<(Place, Row)> = all
        .into_iter()
        .map(|(p, r)| {
            let now = if r.key == crate::assist_level::NEVER_SEE { (cx.value)(&r) } else { String::new() };
            (p, shaped(r, cx.providers, cx.programs, &now))
        })
        .collect();
    let desc = |r: &Row| descs.get(&r.key).cloned().unwrap_or_default();
    // Per card: (main, more), each in rule order, then in the registry's order.
    let mut cards: Vec<(Vec<&Row>, Vec<&Row>)> = vec![(Vec::new(), Vec::new()); CARDS.len()];
    let mut order: Vec<usize> = (0..all.len()).collect();
    order.sort_by_key(|&i| (all[i].0.rank, i));
    for i in order {
        let (p, r) = &all[i];
        if p.more { cards[p.card].1.push(r) } else { cards[p.card].0.push(r) }
    }
    let has_favs = full.sections.iter().any(|s| matches!(s, Section::Favourites));
    let mut v = View::default();
    for (c, (main, _)) in cards.iter().enumerate() {
        v.counts[CARDS[c].tab] += main.len() + usize::from(CARDS[c].id == FAV_CARD && has_favs);
    }
    v.changed = all.iter().filter(|(_, r)| changed(r, cx)).count();
    let mut m = Model { col_min: COL_MIN, ..Model::default() };
    let mut lines = Vec::new();
    let mut names = Vec::new();
    let mut push = |m: &mut Model, r: &Row, reset: bool| {
        let ch = changed(r, cx);
        let d = desc(r);
        lines.push(line_of(r, &d));
        names.push(if d.trim().is_empty() { r.label.clone() } else { d });
        m.rows.push(r.clone());
        m.flags.push(Flag { changed: ch, reset: reset && ch });
    };
    let listing = !cx.query.trim().is_empty() || cx.changed_only;
    if !listing {
        let tab = cx.tab.min(TABS.len() - 1);
        m.heads = Heads::Title;
        // A target a row shown at once already names (a namespace like `theme`) opens no "Mehr".
        let target_shown = cards.iter().enumerate().any(|(c, (main, _))| CARDS[c].tab == tab && main.iter().any(|r| under(&r.key, cx.target)));
        for (c, (main, more)) in cards.iter().enumerate().filter(|(c, _)| CARDS[*c].tab == tab) {
            if CARDS[c].id == FAV_CARD {
                if has_favs {
                    m.sections.push(Section::Favourites);
                }
                continue;
            }
            if main.is_empty() && more.is_empty() {
                continue;
            }
            let from = m.rows.len();
            for r in main {
                push(&mut m, r, false);
            }
            if !more.is_empty() {
                let open = cx.open.contains(CARDS[c].id) || (!target_shown && more.iter().any(|r| under(&r.key, cx.target)));
                let ch = more.iter().filter(|r| changed(r, cx)).count();
                push(&mut m, &fold_row(c, open, more.len(), ch), false);
                if open {
                    for r in more {
                        push(&mut m, r, false);
                    }
                }
            }
            m.sections.push(Section::Rows(CARDS[c].title.to_string(), from..m.rows.len()));
        }
        if m.sections.is_empty() {
            let from = m.rows.len();
            let note = Row {
                key: page_join::LEER.into(),
                kind: Kind::Fixed(String::new()),
                default: String::new(),
                label: "Hier ist noch nichts: der Dienst, dem diese Einstellungen gehören, hat sich noch nicht gemeldet.".into(),
                short: "Noch nichts".into(),
                note: String::new(),
                confirm: false,
                stack: true,
            };
            push(&mut m, &note, false);
            m.sections.push(Section::Rows(String::new(), from..m.rows.len()));
        }
        v.title = TABS[tab].title.into();
        v.sub = TABS[tab].sub.into();
    } else {
        m.heads = Heads::Crumb;
        let mut hits = [0; 9];
        let found_row = |r: &Row, card: &str, tab: &str| hit(r, &desc(r), card, tab, cx.query) && (!cx.changed_only || changed(r, cx));
        // A key a consolidated hit already shows is not listed a second time on its own.
        let mut joined_hits = BTreeSet::new();
        for (c, (main, more)) in cards.iter().enumerate() {
            let (card, tab) = (CARDS[c].title, TABS[CARDS[c].tab].title);
            joined_hits.extend(main.iter().chain(more.iter()).filter(|r| found_row(r, card, tab)).map(|r| r.key.clone()));
        }
        let shown_twice = |r: &Row| page_join::joined(&r.key).is_some_and(|j| joined_hits.contains(&j));
        for (t, tab) in TABS.iter().enumerate() {
            for (c, (main, more)) in cards.iter().enumerate().filter(|(c, _)| CARDS[*c].tab == t) {
                let card = &CARDS[c];
                let found: Vec<&&Row> = main
                    .iter()
                    .chain(more.iter())
                    .filter(|r| found_row(r, card.title, tab.title) && !shown_twice(r))
                    .collect();
                if found.is_empty() {
                    continue;
                }
                hits[t] += found.len();
                let from = m.rows.len();
                for r in found {
                    push(&mut m, r, true);
                }
                let crumb = if card.title.is_empty() || card.title == tab.title { tab.title.to_string() } else { format!("{} › {}", tab.title, card.title) };
                m.sections.push(Section::Rows(crumb, from..m.rows.len()));
            }
        }
        let n: usize = hits.iter().sum();
        let q = cx.query.trim();
        (v.title, v.sub) = match (q.is_empty(), n) {
            (true, n) => (format!("{n} {} vom Standard ab", if n == 1 { "Einstellung weicht" } else { "Einstellungen weichen" }), "Jede lässt sich hier einzeln zurücksetzen.".into()),
            (false, 0) => (format!("Keine Treffer für „{q}“"), "Die Suche geht durch alle Tabs, auch durch das, was unter „Mehr“ eingeklappt ist.".into()),
            (false, 1) => (format!("1 Treffer für „{q}“"), "Die Suche geht durch alle Tabs, auch durch das, was unter „Mehr“ eingeklappt ist.".into()),
            (false, n) => (format!("{n} Treffer für „{q}“"), "Die Suche geht durch alle Tabs, auch durch das, was unter „Mehr“ eingeklappt ist.".into()),
        };
        if !q.is_empty() {
            let from = m.rows.len();
            let ask = Row {
                key: page_join::FRAG.into(),
                kind: Kind::Fixed(String::new()),
                default: String::new(),
                label: "Nicht gefunden, was du suchst? Frag einfach den Assistenten: „Wie stelle ich ein, dass …“".into(),
                short: "Frag den Assistenten".into(),
                note: String::new(),
                confirm: false,
                stack: true,
            };
            push(&mut m, &ask, false);
            m.sections.push(Section::Rows(String::new(), from..m.rows.len()));
        }
        v.hits = Some(hits);
    }
    v.model = m;
    v.lines = lines;
    v.names = names;
    v
}

/// The tab that shows the row of `prefix` (a key or a namespace) among `rows`; None: no such row.
pub fn tab_of_prefix(rows: &[Row], prefix: &str) -> Option<usize> {
    if prefix == crate::page::FAVOURITES {
        return Some(page_tabs::tab_of(prefix));
    }
    rows.iter().find(|r| under(&r.key, prefix)).map(|r| page_tabs::tab_of(&r.key))
}

#[cfg(test)]
#[path = "page_view_tests.rs"]
mod tests;
