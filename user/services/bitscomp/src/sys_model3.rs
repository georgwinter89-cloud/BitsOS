//! SYS-A: the tabs Prozesse (the task manager) and Geräte of the program "System" (see sys_model).

use crate::sys_group::{self as grp, Column, Filter, Part, Row, State};
use crate::sys_model::{Ctx, Item, MASK, TabModel, Tone};
use crate::sys_rules::{self as rules, Act, Gate, Vorrang};
use crate::sys_text as tx;
use crate::sys_view::SysPart;

/// Rows of BitsOS's part shown under "Alle" before "… N weitere".
pub const SYSTEM_TOP: usize = 3;

#[derive(Clone, Debug, PartialEq)]
pub struct Line {
    pub pid: u32,
    pub name: String,
    pub note: String,
    pub state: String,
    pub tone: Tone,
    /// CPU, Speicher, Grafik, Platte, Netz.
    pub cells: [String; 5],
    pub selected: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Section {
    pub head: String,
    pub lines: Vec<Line>,
    pub more: usize,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Detail {
    pub pid: u32,
    pub name: String,
    pub line: String,
    pub rows: Vec<(String, String)>,
    pub vorrang: Vorrang,
    pub show: bool,
    pub end: Gate,
    pub prio: [Gate; 3],
    /// The line under the buttons.
    pub note: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Procs {
    pub counts: [usize; 4],
    pub filter: Filter,
    pub sort: Column,
    pub up: bool,
    pub query: String,
    pub sections: Vec<Section>,
    pub detail: Option<Detail>,
    pub loaded: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Devices {
    /// (group head, devices: (name, state, tone)).
    pub groups: Vec<(String, Vec<(String, String, Tone)>)>,
    pub selected: usize,
    pub detail: Vec<Item>,
    pub facts: Vec<Item>,
    pub pci: Option<usize>,
}

fn tone(s: State) -> Tone {
    match s {
        State::Runs => Tone::Accent,
        State::Hung => Tone::Danger,
        _ => Tone::Muted,
    }
}

/// The row the detail shows: the chosen one while it is still there (same pid and start); with nothing chosen yet the
/// first shown, which the page pins as its choice when it draws it (`pin`). A chosen row that went is not replaced:
/// the buttons must never move on to a program Georg did not pick.
pub fn chosen<'a>(rows: &'a [Row], sections: &[(Part, Vec<Row>, usize)], sel: Option<(u32, u64)>) -> Option<&'a Row> {
    match sel {
        Some(sel) => selected(rows, Some(sel)),
        None => {
            let first = sections.first()?.1.first()?;
            rows.iter().find(|r| r.main() == first.main())
        }
    }
}

/// The row an action acts on: only the explicit choice, and only while it still runs.
pub fn selected(rows: &[Row], sel: Option<(u32, u64)>) -> Option<&Row> {
    let (pid, start) = sel?;
    rows.iter().find(|r| r.main() == pid && r.start == start)
}

/// What the page pins as its choice when it shows the first row without one.
pub fn pin(rows: &[Row], sections: &[(Part, Vec<Row>, usize)], sel: Option<(u32, u64)>) -> Option<(u32, u64)> {
    if sel.is_some() {
        return None;
    }
    chosen(rows, sections, None).map(|r| (r.main(), r.start))
}

pub const GONE: &str = "Läuft nicht mehr: wähle ein Programm in der Liste.";
/// The first row shown before the page pinned it (its order still settles): nothing to press yet.
pub const PICK: &str = "Wähle ein Programm in der Liste.";

/// The detail of a chosen program that ended: nothing to press.
fn gone(pid: u32) -> Detail {
    let no = || Gate::No(GONE.into());
    let rows = ["Threads", "Speicher, davon geteilt", "Grafikspeicher", "Offene Dateien", "Läuft mit", "Gehört"].map(|k| (k.to_string(), tx::NONE.to_string())).to_vec();
    Detail { pid, name: "Läuft nicht mehr".into(), line: format!("Prozess {pid}"), rows, vorrang: Vorrang::Normal, show: false, end: no(), prio: [no(), no(), no()], note: GONE.into() }
}

/// What a program runs with, from its command line and window (Proton and Wine by their paths).
pub fn runs_with(r: &Row) -> String {
    let all = r.argv.join(" ");
    let low = all.to_lowercase();
    if low.contains("proton") {
        let ver = all.split('/').find(|p| p.starts_with("Proton")).map(|p| p.trim().to_string());
        return format!("{} · Windows-Spiel", ver.unwrap_or_else(|| "Proton".into()));
    }
    if low.contains("wine") || r.comm.ends_with(".exe") {
        return "Wine · Windows-Programm".into();
    }
    match r.wins.first() {
        Some(w) if w.x11 => "X11, über die Brücke".into(),
        Some(_) => "Wayland".into(),
        None => r.argv.first().cloned().unwrap_or_else(|| r.comm.clone()),
    }
}

fn detail(c: &Ctx, r: &Row) -> Detail {
    let s = c.s;
    let start = s.kernel.boot_wall.map(|b| b + (r.start / s.hz.max(1)) as i64);
    let mut line = vec![];
    if !r.app.is_empty() {
        line.push(r.app.clone());
    } else if r.comm != r.name {
        line.push(r.comm.clone());
    }
    line.push(format!("Prozess {}", r.main()));
    if let Some(w) = start {
        line.push(format!("seit {}", tx::clock(w, c.off_min)));
    }
    if let Some(w) = r.wins.first() {
        line.push(format!("Fenster w{}", w.id));
    }
    let mem = match (r.mem, r.shared) {
        (Some(m), Some(sh)) => format!("{} · {}", tx::bytes(Some(m)), tx::bytes(Some(sh))),
        (m, _) => tx::bytes(m),
    };
    let owner = match r.uid {
        Some(0) => "System (root)".to_string(),
        Some(u) if u == c.own_uid => "du".into(),
        Some(u) => format!("Benutzer {u}"),
        None => tx::NONE.into(),
    };
    let rows = vec![
        ("Threads".to_string(), r.threads.to_string()),
        ("Speicher, davon geteilt".into(), mem),
        ("Grafikspeicher".into(), tx::bytes(r.gpu)),
        ("Offene Dateien".into(), r.files.map_or_else(|| tx::NONE.into(), |f| f.to_string())),
        ("Läuft mit".into(), runs_with(r)),
        ("Gehört".into(), owner),
    ];
    let g = |a| rules::gate(r, a, c.own_uid, c.real_root);
    let end = g(Act::End);
    let prio = Vorrang::ALL.map(|v| g(Act::Prio(v)));
    let note = match (&end, r.service) {
        (Gate::No(why), _) => why.clone(),
        (_, true) => "Ein Dienst von BitsOS: beenden oder Vorrang ändern nur nach deiner Bestätigung.".into(),
        _ => "Beenden fragt das Programm zuerst höflich. Reagiert es nicht, bietet BitsOS „Sofort beenden“ an.".into(),
    };
    Detail { pid: r.main(), name: r.name.clone(), line: line.join(" · "), rows, vorrang: Vorrang::of_nice(r.nice), show: !r.wins.is_empty(), end, prio, note }
}

pub fn procs(c: &Ctx) -> TabModel {
    let v = c.v;
    let shown = grp::shown(c.rows, v.filter, &v.query, v.sort, v.up, SYSTEM_TOP);
    let sel = chosen(c.rows, &shown, v.selected);
    let mut sections = Vec::new();
    if v.mask {
        for p in [Part::Window, Part::Background, Part::System] {
            let l = Line { pid: 0, name: MASK.into(), note: String::new(), state: MASK.into(), tone: Tone::Muted, cells: [MASK.into(), MASK.into(), MASK.into(), MASK.into(), tx::NONE.into()], selected: false };
            sections.push(Section { head: p.head().into(), lines: vec![l], more: 0 });
        }
    } else {
        for (part, rows, more) in &shown {
            let head = if *part == Part::System && *more > 0 { format!("{} · die größten zuerst", part.head()) } else { part.head().into() };
            let lines = rows
                .iter()
                .map(|r| Line {
                    pid: r.main(),
                    name: r.name.clone(),
                    note: r.note.clone(),
                    state: r.state.map_or("", |s| s.word()).into(),
                    tone: r.state.map_or(Tone::Muted, tone),
                    cells: [tx::pct(r.cpu), tx::bytes(r.mem), tx::bytes(r.gpu), tx::rate_cell(r.disk), tx::NONE.into()],
                    selected: sel.is_some_and(|s| s.main() == r.main()),
                })
                .collect();
            sections.push(Section { head, lines, more: *more });
        }
    }
    let mut d = match (sel, v.selected) {
        (Some(r), Some(_)) => Some(detail(c, r)),
        (Some(r), None) => {
            let no = || Gate::No(PICK.into());
            Some(Detail { end: no(), prio: [no(), no(), no()], show: false, ..detail(c, r) })
        }
        (None, Some((pid, _))) => Some(gone(pid)),
        (None, None) => None,
    };
    if v.mask {
        // A fixed detail: which row comes first, and so its buttons, depends on the machine.
        d = d.map(|d| Detail {
            pid: 0,
            name: MASK.into(),
            line: MASK.into(),
            rows: d.rows.into_iter().map(|r| (r.0, MASK.into())).collect(),
            vorrang: Vorrang::Normal,
            show: true,
            end: Gate::Free,
            prio: [Gate::Free, Gate::Free, Gate::Ask],
            note: "Beenden fragt das Programm zuerst höflich. Reagiert es nicht, bietet BitsOS „Sofort beenden“ an.".into(),
        });
    }
    let counts = if v.mask { [0; 4] } else { grp::counts(c.rows) };
    TabModel {
        sub: "Strg+Umschalt+Esc öffnet diese Seite".into(),
        procs: Some(Procs { counts, filter: v.filter, sort: v.sort, up: v.up, query: v.query.clone(), sections, detail: d, loaded: c.s.procs_read || v.mask }),
        ..TabModel::default()
    }
}

/// One device of the list with what its detail card shows.
struct Dev {
    group: &'static str,
    name: String,
    state: String,
    tone: Tone,
    detail: Vec<Item>,
}

fn devs(c: &Ctx) -> Vec<Dev> {
    let s = c.s;
    let f = &s.facts;
    let mut v = Vec::new();
    let none = || tx::NONE.to_string();
    v.push(Dev {
        group: "Rechnen",
        name: f.cpu_model.clone().unwrap_or_else(|| "Prozessor".into()),
        state: "Prozessor".into(),
        tone: Tone::Muted,
        detail: vec![
            Item::Label("Prozessor".into()),
            Item::Row("Kerne".into(), f.cores.map_or_else(none, |n| n.to_string())),
            Item::Row("Threads".into(), f.threads.map_or_else(none, |n| n.to_string())),
            Item::Row("Höchster Takt".into(), tx::khz(s.cpu.max_khz)),
            Item::Row("Temperatur".into(), c.m(tx::celsius(s.heat.cpu))),
        ],
    });
    v.push(Dev {
        group: "Rechnen",
        name: f.gpu_name.clone().unwrap_or_else(|| "Grafik".into()),
        state: "Grafik".into(),
        tone: Tone::Muted,
        detail: vec![
            Item::Label("Grafik".into()),
            Item::Row("Fest zugeteilter Speicher".into(), tx::bytes(s.gpu.vram.map(|g| g.1))),
            Item::Row("Aus dem Arbeitsspeicher".into(), tx::bytes(s.gpu.gtt.map(|g| g.1))),
            Item::Row("Temperatur".into(), c.m(tx::celsius(s.heat.gpu))),
        ],
    });
    v.push(Dev {
        group: "Rechnen",
        name: tx::bytes(f.mem_total),
        state: "Speicher".into(),
        tone: Tone::Muted,
        detail: vec![Item::Label("Arbeitsspeicher".into()), Item::Row("Eingebaut".into(), tx::bytes(f.mem_total)), Item::Row("Frei verfügbar".into(), c.m(tx::bytes(s.mem.mem.map(|m| m.available))))],
    });
    for d in &s.disks {
        let mounted: Vec<&crate::sys_snap::Mount> = s.mounts.iter().filter(|m| m.dev.trim_start_matches("/dev/").starts_with(&d.name)).collect();
        let state = if mounted.iter().any(|m| m.path == "/data") {
            "BitsOS-Daten".to_string()
        } else if mounted.iter().any(|m| m.path == "/boot") {
            "Startteil".into()
        } else if mounted.is_empty() {
            "nicht benutzt".into()
        } else {
            mounted[0].path.clone()
        };
        let total = d.size.unwrap_or(0).max(1) as f64;
        let tones = [Tone::Blue, Tone::Accent, Tone::Amber, Tone::Muted];
        let mut detail = vec![Item::Label(format!("{} · {}", d.name, tx::bytes(d.size))), Item::Big("Aufteilung".into(), String::new())];
        detail.push(Item::Bar(d.parts.iter().enumerate().map(|(i, (_, sz))| (*sz as f64 / total, tones[i % tones.len()])).collect()));
        detail.push(Item::Legend(d.parts.iter().enumerate().map(|(i, (n, sz))| (tones[i % tones.len()], format!("{n} {}", tx::bytes(Some(*sz))))).collect()));
        for m in &mounted {
            detail.push(Item::Row(crate::sys_model::mount_name(&m.path), c.m(format!("{} / {}", tx::bytes(Some(m.used)), tx::bytes(Some(m.total))))));
        }
        detail.push(Item::Row("Gelesen seit dem Start".into(), c.m(tx::bytes(d.read_total))));
        detail.push(Item::Row("Geschrieben seit dem Start".into(), c.m(tx::bytes(d.write_total))));
        let name = d.model.clone().map_or_else(|| format!("{} · {}", d.name, tx::bytes(d.size)), |m| format!("{m} · {}", tx::bytes(d.size)));
        v.push(Dev { group: "Speichern", name, tone: if state == "BitsOS-Daten" { Tone::Accent } else { Tone::Muted }, state, detail });
    }
    for n in &s.nets {
        let wifi = n.wifi.is_some() || n.name.starts_with("wl");
        let (state, tone) = match n.up {
            Some(true) => ("verbunden", Tone::Accent),
            Some(false) if wifi => ("bereit", Tone::Muted),
            Some(false) => ("kein Kabel", Tone::Muted),
            None => ("–", Tone::Muted),
        };
        let name = if wifi { format!("WLAN ({})", n.name) } else { format!("Netzwerk ({})", n.name) };
        let detail = vec![
            Item::Label(name.clone()),
            Item::RowTone("Zustand".into(), state.into(), tone),
            Item::Row("Geschwindigkeit".into(), n.speed_mbps.map_or_else(none, |s| format!("{s} Mbit/s"))),
            Item::Row("Empfangen seit dem Start".into(), c.m(tx::bytes(Some(n.rx_total)))),
            Item::Row("Gesendet seit dem Start".into(), c.m(tx::bytes(Some(n.tx_total)))),
        ];
        v.push(Dev { group: "Verbinden", name, state: state.into(), tone, detail });
    }
    for (name, speed) in &f.usb {
        v.push(Dev { group: "Ein- und Ausgabe", name: name.clone(), state: speed.clone(), tone: Tone::Muted, detail: vec![Item::Label(name.clone()), Item::Row("Anschluss".into(), speed.clone())] });
    }
    v
}

pub fn devices(c: &Ctx) -> TabModel {
    let s = c.s;
    let list = devs(c);
    let selected = c.v.device.min(list.len().saturating_sub(1));
    let mut groups: Vec<(String, Vec<(String, String, Tone)>)> = Vec::new();
    for d in &list {
        match groups.last_mut() {
            Some(g) if g.0 == d.group => g.1.push((d.name.clone(), d.state.clone(), d.tone)),
            _ => groups.push((d.group.to_string(), vec![(d.name.clone(), d.state.clone(), d.tone)])),
        }
    }
    let k = &s.kernel;
    let (gn, ver) = k.generation.clone().map_or((tx::NONE.to_string(), String::new()), |(g, v)| (g.to_string(), v));
    let mut facts = vec![
        Item::Label("BitsOS".into()),
        Item::Row("Fassung".into(), if ver.is_empty() { "V2".into() } else { format!("V2 · Kern {ver}") }),
        Item::Row("Generation".into(), c.m(gn)),
        Item::Row("Gestartet".into(), c.m(k.boot_wall.map_or_else(|| tx::NONE.into(), |w| tx::date_time(w, c.off_min)))),
        Item::Row("Hochfahren".into(), c.m(k.boot_ms.map_or_else(|| tx::NONE.into(), |ms| format!("{} s", tx::dec(ms as f64 / 1000.0, 1))))),
        Item::Row("Läuft seit".into(), c.m(tx::duration(k.uptime))),
        Item::Rule,
        Item::Label("Abstürze".into()),
        Item::Row("Letzter".into(), c.m(k.crash.map_or_else(|| "keiner gespeichert".into(), |(w, g)| format!("{} · Gen {g}", tx::date_time(w, c.off_min))))),
    ];
    if k.crash.is_some() {
        facts.push(Item::Link("Bericht ansehen".into(), SysPart::Link(0)));
    }
    facts.extend([
        Item::Rule,
        Item::Label("Kern-Arbeit".into()),
        Item::Row("Aufrufe je Sekunde".into(), c.m(k.calls.map_or_else(|| tx::NONE.into(), |n| tx::int(n as u64)))),
        Item::Row("davon abgelehnt".into(), c.m(tx::pct(k.refused))),
        Item::Row("Programme beendet wegen Speicher".into(), c.m(k.oom.map_or_else(|| tx::NONE.into(), |n| tx::int(n as u64)))),
    ]);
    if !s.facts.dmi.is_empty() {
        facts.push(Item::Rule);
        facts.extend(s.facts.dmi.iter().take(4).map(|(k, v)| Item::Row(k.clone(), v.clone())));
    }
    facts.push(Item::Fill);
    facts.push(Item::Buttons(vec![("Bericht für Claude kopieren".into(), SysPart::Copy, true)]));
    let product = s.facts.dmi.iter().find(|(k, _)| k == "Modell").map(|(_, v)| v.clone());
    TabModel {
        sub: product.unwrap_or_else(|| "Dieser Rechner".into()),
        links: vec!["absturz".into()],
        devices: Some(Devices { groups, selected, detail: list.into_iter().nth(selected).map(|d| d.detail).unwrap_or_default(), facts, pci: s.facts.pci }),
        ..TabModel::default()
    }
}
