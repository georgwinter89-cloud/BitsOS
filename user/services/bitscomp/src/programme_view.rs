//! PK-7a: what the "Programme" page shows, as a list of items built from the model, the page's own view state
//! and the library (pure: host tests). Tabs (plan/spec/program-library.md section 4): Bibliothek (the library's tiles as a full page
//! and the installed packages), Hinzufügen (search, the plan card, the first-start offer of §5), Aktualisierungen
//! (the daily check's result, D5), and one program (version, rollback, wanted permissions, space, origin,
//! removal). The layout (`programme_layout`) places the items, the look (`programme_draw`) draws them.

use crate::library::{Entry, Icon, Index, Source};
use crate::programme::{Action, Model, Phase, SUGGESTIONS};
use crate::programme_text::{self as text, Tone};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Tab {
    #[default]
    Library,
    Add,
    Updates,
    Program,
}

pub const TABS: [(Tab, &str, &str); 3] = [(Tab::Library, "Bibliothek", "bibliothek"), (Tab::Add, "Hinzufügen", "hinzufuegen"), (Tab::Updates, "Aktualisierungen", "aktualisierungen")];

/// The page's own state besides the generic part (page_app's PageState).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct View {
    pub tab: Tab,
    /// The program of the per-program view.
    pub program: Option<String>,
    pub keep_data: bool,
}

impl View {
    /// The view an address opens: `programme/`, `programme/hinzufuegen`, `programme/aktualisierungen`,
    /// `programme/<name>` (one program).
    pub fn of_address(a: &str) -> View {
        let rest = a.strip_prefix("programme").unwrap_or("").trim_matches('/');
        let first = rest.split('/').next().unwrap_or("");
        match first {
            "" | "bibliothek" | "library" => View::default(),
            "hinzufuegen" | "hinzufügen" | "add" | "suchen" => View { tab: Tab::Add, ..View::default() },
            "aktualisierungen" | "updates" => View { tab: Tab::Updates, ..View::default() },
            name => View { tab: Tab::Program, program: Some(name.to_string()), keep_data: false },
        }
    }

    /// LIB-ADD: the search an address hands to Hinzufügen (`programme/hinzufuegen/<text>`, the library's link
    /// when its search found nothing); the page's own address then is `programme/hinzufuegen` again.
    pub fn search_of(a: &str) -> Option<String> {
        let rest = a.strip_prefix("programme/")?;
        let q = ["hinzufuegen/", "hinzufügen/", "add/", "suchen/"].iter().find_map(|w| rest.strip_prefix(w))?.trim();
        (!q.is_empty()).then(|| q.to_string())
    }

    pub fn address(&self) -> String {
        match (self.tab, &self.program) {
            (Tab::Program, Some(p)) => format!("programme/{p}"),
            (t, _) => match TABS.iter().find(|(x, _, _)| *x == t) {
                Some((Tab::Library, _, _)) | None => "programme/".into(),
                Some((_, _, w)) => format!("programme/{w}"),
            },
        }
    }
}

/// Something on the page that reacts to the mouse; numbers index the model's lists (or the shown tiles).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProgPart {
    Tab(usize),
    Sources,
    Search,
    Install(usize),
    Suggest(usize),
    Manage(usize),
    Program(usize),
    CardGo,
    CardList,
    CardClose,
    Extra(usize),
    KeepData,
    Remove,
    Rollback,
    Clean,
    Start,
    UpdateAll,
    /// "Jetzt prüfen": the sources loaded again (bitspkgd's `refresh`).
    CheckNow,
    Update(usize),
    Tile(usize),
    More(usize),
}

#[derive(Clone, Debug, PartialEq)]
pub struct TileItem {
    pub id: String,
    pub name: String,
    pub icon: Icon,
    /// A package of /apps: it has a "…" (the per-program view).
    pub package: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Item {
    /// A section head (upper case label); a run of card items after it is one card.
    Head(String),
    Line(String, Tone),
    /// A result or list row: title, second line, a small line at the right (repository, size), the Rust mark,
    /// and its buttons.
    Row { title: String, sub: String, meta: String, rust: bool, buttons: Vec<(ProgPart, String)> },
    Buttons(Vec<(ProgPart, String, bool)>),
    Check(ProgPart, String, bool),
    /// The run's phases: reached rank, whether it is done, the download share (0..=1000).
    Progress(Option<usize>, bool, u32),
    /// The search field (bare, no card).
    Search,
    /// A row of library tiles (bare); the tiles are numbered across the page.
    Tiles(String, Vec<TileItem>, usize),
}

/// What the page needs of the library and the desktop.
pub struct Lib<'a> {
    pub index: &'a Index,
    pub rows: &'a [crate::library_view::Row],
}

fn entry_of_package<'a>(ix: &'a Index, pkg: &str) -> Option<&'a Entry> {
    ix.entries.iter().find(|e| e.source == Source::Package(pkg.to_string()))
}

/// The German short description of a hit: the installed program's `.desktop` Comment (German where the file has
/// `Comment[de]`), else the repository's `%DESC%`.
pub fn desc(ix: &Index, name: &str, repo_desc: &str) -> String {
    entry_of_package(ix, name).map(|e| e.comment.clone()).filter(|c| !c.is_empty()).unwrap_or_else(|| repo_desc.to_string())
}

/// Every tile id shown, in the order of `ProgPart::Tile` numbers.
pub fn tile_ids(items: &[Item]) -> Vec<(String, Option<String>)> {
    items.iter().filter_map(|i| if let Item::Tiles(_, t, _) = i { Some(t) } else { None }).flatten().map(|t| (t.id.clone(), t.package.clone())).collect()
}

fn card(m: &Model, action: Option<Action>, out: &mut Vec<Item>) {
    let Some(c) = m.card.as_ref().filter(|c| action.is_none_or(|a| a == c.action)) else { return };
    out.push(Item::Head("PLAN".into()));
    out.extend(text::card_lines(c).into_iter().map(|(t, tone)| Item::Line(t, tone)));
    for (i, (dep, why, on)) in c.extras.iter().enumerate() {
        let label = if why.is_empty() { dep.clone() } else { format!("{dep} – {why}") };
        // Without `with` in bitspkgd a box could only refuse after the click: the extras as plain lines.
        out.push(if m.can("with") { Item::Check(ProgPart::Extra(i), label, *on) } else { Item::Line(label, Tone::Item) });
    }
    if !c.extras.is_empty() && !m.can("with") {
        out.push(Item::Line("Zusätze kann die Programmverwaltung noch nicht mitnehmen: später einzeln installieren.".into(), Tone::Muted));
    }
    if c.phase == Phase::Running || c.rank.is_some() {
        let share = if c.bytes.1 > 0 { (c.bytes.0.min(c.bytes.1) * 1000 / c.bytes.1) as u32 } else { 0 };
        out.push(Item::Progress(c.rank, c.step_done, share));
    }
    out.extend(text::card_foot(c).into_iter().map(|(t, tone)| Item::Line(t, tone)));
    let mut b = Vec::new();
    if c.phase == Phase::Ready {
        let go = match c.action {
            Action::Install => "Jetzt installieren",
            Action::Remove => "Jetzt entfernen",
            Action::Update => "Jetzt aktualisieren",
        };
        b.push((ProgPart::CardGo, go.to_string(), true));
    }
    if c.action != Action::Remove && !c.packages.is_empty() {
        b.push((ProgPart::CardList, if c.show_list { "Liste verbergen" } else { "Liste zeigen" }.to_string(), true));
    }
    if c.phase != Phase::Running {
        b.push((ProgPart::CardClose, if matches!(c.phase, Phase::Done | Phase::Failed) { "Schließen" } else { "Abbrechen" }.to_string(), true));
    }
    if !b.is_empty() {
        out.push(Item::Buttons(b));
    }
}

fn down(m: &Model, out: &mut Vec<Item>) {
    if m.live == Some(false) {
        out.push(Item::Head("PROGRAMMVERWALTUNG".into()));
        out.push(Item::Line(format!("{}: was hier steht, ist der letzte bekannte Stand.", m.down), Tone::Bad));
    }
}

pub fn items(m: &Model, v: &View, lib: &Lib) -> Vec<Item> {
    let mut out = Vec::new();
    down(m, &mut out);
    match v.tab {
        Tab::Library => library(m, lib, &mut out),
        Tab::Add => add(m, lib, &mut out),
        Tab::Updates => updates(m, &mut out),
        Tab::Program => program(m, v, lib, &mut out),
    }
    out
}

fn library(m: &Model, lib: &Lib, out: &mut Vec<Item>) {
    let mut n = 0;
    for r in lib.rows {
        let tiles: Vec<TileItem> = r
            .ids
            .iter()
            .filter_map(|id| lib.index.entries.iter().find(|e| e.id == *id))
            .map(|e| TileItem {
                id: e.id.clone(),
                name: e.name.clone(),
                icon: e.icon.clone(),
                package: if let Source::Package(p) = &e.source { Some(p.clone()) } else { None },
            })
            .collect();
        let k = tiles.len();
        out.push(Item::Tiles(r.title.to_string(), tiles, n));
        n += k;
    }
    out.push(Item::Head("INSTALLIERTE PAKETE".into()));
    if m.installed.is_empty() {
        let t = if m.listed { "Noch kein Paket installiert: unter „Hinzufügen“ gibt es Vorschläge." } else { "Liste wird geladen …" };
        out.push(Item::Line(t.into(), Tone::Muted));
    }
    for (i, p) in m.installed.iter().enumerate() {
        let sub = match (&p.current, &p.previous) {
            (Some(c), Some(pr)) => format!("Fassung {c} · frühere {pr}"),
            (Some(c), None) => format!("Fassung {c}"),
            _ => "keine gültige Fassung".into(),
        };
        out.push(Item::Row { title: p.name.clone(), sub, meta: String::new(), rust: false, buttons: vec![(ProgPart::Program(i), "Verwalten".into())] });
    }
}

fn sources(m: &Model, out: &mut Vec<Item>) {
    if let Some(l) = &m.sources_line {
        out.push(Item::Head("PAKETQUELLEN".into()));
        out.push(Item::Line(l.clone(), Tone::Bad));
    }
}

fn add(m: &Model, lib: &Lib, out: &mut Vec<Item>) {
    out.push(Item::Search);
    if m.search_line != m.sources_line.clone().unwrap_or_default() {
        sources(m, out);
    }
    card(m, Some(Action::Install), out);
    if !m.search_line.is_empty() {
        out.push(Item::Head("SUCHE".into()));
        out.push(Item::Line(m.search_line.clone(), Tone::Muted));
    }
    if m.query.trim().is_empty() {
        // Each offer stays until its own program is installed (CR-BR9-1): Brave installed keeps LibreOffice's.
        let open: Vec<_> = SUGGESTIONS.iter().enumerate().filter(|(_, (pkg, _, _))| !m.installed.iter().any(|i| i.name == *pkg)).collect();
        if m.listed && !open.is_empty() {
            out.push(Item::Head("VORSCHLÄGE".into()));
            for (i, (_, name, d)) in open {
                out.push(Item::Row { title: name.to_string(), sub: d.to_string(), meta: "aus den Paketquellen".into(), rust: false, buttons: vec![(ProgPart::Suggest(i), "Installieren".into())] });
            }
            out.push(Item::Line("Wird erst beim Klick heruntergeladen; vorher zeigt der Plan Platz und Rechte.".into(), Tone::Muted));
        }
        return;
    }
    if m.hits.is_empty() {
        return;
    }
    out.push(Item::Head("TREFFER".into()));
    for (i, h) in m.hits.iter().enumerate() {
        let mut meta = h.repo.clone();
        if let Some(sz) = h.size {
            meta = format!("{meta} · {}", text::bytes(sz));
        }
        let b = if m.is_installed(&h.name) { (ProgPart::Manage(i), "Verwalten".to_string()) } else { (ProgPart::Install(i), "Installieren".to_string()) };
        out.push(Item::Row { title: format!("{} {}", h.name, h.version), sub: desc(lib.index, &h.name, &h.desc), meta, rust: h.rust, buttons: vec![b] });
    }
    if m.more > 0 {
        out.push(Item::Line(format!("… und {} weitere: genauer suchen.", m.more), Tone::Muted));
    }
    if !m.search_note.is_empty() {
        out.push(Item::Line(m.search_note.clone(), Tone::Muted));
    }
}

fn updates(m: &Model, out: &mut Vec<Item>) {
    card(m, Some(Action::Update), out);
    sources(m, out);
    out.push(Item::Head("AKTUALISIERUNGEN".into()));
    let checked = match m.last_check.max(m.checked_at) {
        Some(t) if t > 0 => format!("Zuletzt geprüft: {}", text::when(t)),
        _ => "Noch nicht geprüft: die tägliche Prüfung läuft, sobald die Quellen einmal geladen sind.".into(),
    };
    out.push(Item::Line(checked, Tone::Text));
    let n = m.updates.len();
    // "Alles ist aktuell" only when every check answered and the sources are there.
    let complete = m.listed && !m.checking() && m.check_failed.is_empty() && m.sources_line.is_none();
    out.push(Item::Line(
        match n {
            0 if complete => "Alles ist aktuell.".to_string(),
            0 if m.checking() || !m.listed => "Wird geprüft …".to_string(),
            0 => "Keine Aktualisierung gefunden, aber die Prüfung ist nicht vollständig.".to_string(),
            1 => "1 Aktualisierung".to_string(),
            n => format!("{n} Aktualisierungen"),
        },
        Tone::Text,
    ));
    for (name, why) in &m.check_failed {
        out.push(Item::Line(format!("Prüfung unvollständig: {name}: {why}"), Tone::Bad));
    }
    let mut b = Vec::new();
    if n > 0 {
        b.push((ProgPart::UpdateAll, "Alle aktualisieren".into(), !m.busy()));
    }
    b.push((ProgPart::CheckNow, if m.refreshing() { "Wird geprüft …" } else { "Jetzt prüfen" }.into(), !m.busy() && !m.refreshing()));
    out.push(Item::Buttons(b));
    out.push(Item::Line("BitsOS sieht täglich nach und aktualisiert nur auf deinen Klick; die alte Fassung bleibt zum Zurückgehen.".into(), Tone::Muted));
    if n > 0 {
        out.push(Item::Head("PROGRAMME".into()));
    }
    for (i, u) in m.updates.iter().enumerate() {
        let parts = if u.parts == 1 { "1 Teil neu".to_string() } else { format!("{} Teile neu", u.parts) };
        out.push(Item::Row { title: format!("{} → {}", u.name, u.version), sub: u.summary.clone(), meta: parts, rust: false, buttons: vec![(ProgPart::Update(i), "Aktualisieren".into())] });
    }
}

fn date_of(d: Option<&crate::programme::Detail>, v: &str) -> String {
    d.and_then(|d| d.dates.iter().find(|(x, _)| x == v)).filter(|(_, t)| *t > 0).map_or(String::new(), |(_, t)| format!(" vom {}", text::day(*t)))
}

fn program(m: &Model, v: &View, lib: &Lib, out: &mut Vec<Item>) {
    let Some(name) = v.program.as_deref() else { return };
    if !crate::programme::valid_name(name) {
        out.push(Item::Head("FASSUNG".into()));
        out.push(Item::Line(format!("Unbekanntes Programm: „{name}“ ist kein Programmname."), Tone::Bad));
        return;
    }
    card(m, None, out);
    let inst = m.installed.iter().find(|i| i.name == name);
    let d = m.details.get(name);
    out.push(Item::Head("FASSUNG".into()));
    let Some(inst) = inst.filter(|i| i.current.is_some()) else {
        let t = if m.listed { format!("{name} ist nicht installiert.") } else { "Wird geladen …".into() };
        out.push(Item::Line(t, Tone::Muted));
        return;
    };
    let cur = inst.current.clone().unwrap_or_default();
    out.push(Item::Line(format!("Fassung {cur}{}", date_of(d, &cur)), Tone::Text));
    match &inst.previous {
        Some(p) => out.push(Item::Row {
            title: format!("Frühere Fassung {p}{}", date_of(d, p)),
            sub: "Zurückgehen lässt sich wieder umkehren: dann steht hier die jetzige.".into(),
            meta: String::new(),
            rust: false,
            buttons: vec![(ProgPart::Rollback, "Zurück zu dieser Version".into())],
        }),
        None => out.push(Item::Line("Keine frühere Fassung.".into(), Tone::Muted)),
    }
    if entry_of_package(lib.index, name).is_some() {
        out.push(Item::Buttons(vec![(ProgPart::Start, "Starten".into(), true)]));
    }
    out.push(Item::Head("WAS DAS PROGRAMM DARF".into()));
    let perms = d.map(|d| d.permissions.clone()).unwrap_or_default();
    if perms.is_empty() {
        out.push(Item::Line("Das Programm nennt keine Wünsche.".into(), Tone::Muted));
    }
    out.extend(perms.into_iter().map(|p| Item::Line(p, Tone::Item)));
    out.push(Item::Line("Das wünscht sich das Programm. Deine eigenen Regeln (erlauben, fragen, verbieten) kommen mit dem Rechtedienst permd (PM-3).".into(), Tone::Muted));
    out.push(Item::Head("PLATZ".into()));
    let b = |x: Option<u64>| x.map_or("?".to_string(), text::bytes);
    out.push(Item::Line(format!("Eigene Teile {} · mit anderen geteilt {} · Daten {}", b(d.and_then(|d| d.own)), b(d.and_then(|d| d.shared)), b(d.and_then(|d| d.data))), Tone::Text));
    out.push(Item::Buttons(vec![(ProgPart::Clean, "Cache leeren".into(), m.can("clean"))]));
    if !m.can("clean") {
        out.push(Item::Line(text::NO_CLEAN.into(), Tone::Muted));
    }
    out.push(Item::Head("HERKUNFT".into()));
    out.push(Item::Line(format!("Quelle: {}", d.and_then(|d| d.repo.clone()).unwrap_or_else(|| "unbekannt".into())), Tone::Text));
    let key = d.and_then(|d| d.key.clone()).map(|k| format!(" (Schlüssel …{})", &k[k.len().saturating_sub(16)..])).unwrap_or_default();
    let signer = d.and_then(|d| d.signer.clone()).map_or("Signiert von: noch nicht bekannt".to_string(), |s| format!("Signiert von: {s}{key}"));
    out.push(Item::Line(signer, Tone::Text));
    out.push(Item::Head("ENTFERNEN".into()));
    if m.can("keep_data") {
        out.push(Item::Check(ProgPart::KeepData, "Meine Daten behalten".into(), v.keep_data));
    } else {
        out.push(Item::Line("„Meine Daten behalten“ kann die Programmverwaltung noch nicht: beim Entfernen geht der Datenordner mit.".into(), Tone::Muted));
    }
    out.push(Item::Buttons(vec![(ProgPart::Remove, "Entfernen …".into(), !m.busy())]));
}

/// The name of `p` in `ctl hot` (scenarios click by it).
pub fn part_name(m: &Model, items: &[Item], p: ProgPart) -> String {
    let tiles = tile_ids(items);
    match p {
        ProgPart::Tab(i) => format!("tab={}", TABS.get(i).map_or("programm", |t| t.2)),
        ProgPart::Sources => "sources".into(),
        ProgPart::Search => "search".into(),
        ProgPart::Install(i) => format!("install={}", m.hits.get(i).map_or("?", |h| h.name.as_str())),
        ProgPart::Manage(i) => format!("manage={}", m.hits.get(i).map_or("?", |h| h.name.as_str())),
        ProgPart::Suggest(i) => format!("suggest={}", SUGGESTIONS.get(i).map_or("?", |s| s.0)),
        ProgPart::Program(i) => format!("program={}", m.installed.get(i).map_or("?", |p| p.name.as_str())),
        ProgPart::CardGo => "card-go".into(),
        ProgPart::CardList => "card-list".into(),
        ProgPart::CardClose => "card-close".into(),
        ProgPart::Extra(i) => format!("extra={}", m.card.as_ref().and_then(|c| c.extras.get(i)).map_or("?", |e| e.0.as_str())),
        ProgPart::KeepData => "keep-data".into(),
        ProgPart::Remove => "remove".into(),
        ProgPart::Rollback => "rollback".into(),
        ProgPart::Clean => "clean".into(),
        ProgPart::Start => "start".into(),
        ProgPart::UpdateAll => "update-all".into(),
        ProgPart::CheckNow => "check-now".into(),
        ProgPart::Update(i) => format!("update={}", m.updates.get(i).map_or("?", |u| u.name.as_str())),
        ProgPart::Tile(i) => format!("tile={}", tiles.get(i).map_or("?", |t| t.0.as_str())),
        ProgPart::More(i) => format!("more={}", tiles.get(i).map_or("?", |t| t.0.as_str())),
    }
}
