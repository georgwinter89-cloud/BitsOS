//! SYS-A: what a tab of the program "System" shows, as cards of items (pure: host tests). Values come from the
//! sampler's snapshot and the history; a missing source shows "–" and, once per card, the reason. The tabs
//! Überblick, Prozessor and Speicher are here; Grafik to Verlauf in sys_model2, Prozesse and Geräte in sys_model3.

use crate::sys_group::Row;
use crate::sys_ring::{History, Range};
use crate::sys_snap::Snapshot;
use crate::sys_text as tx;
use crate::sys_view::{SysPart, Tab, View};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    Muted,
    Accent,
    Blue,
    Amber,
    Danger,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shape {
    Line,
    Bars,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Chart {
    /// (points oldest first, colour, area under the line).
    pub series: Vec<(Vec<Option<f64>>, Tone, bool)>,
    /// A fixed top of the scale (percent: 100); None: the data's peak.
    pub max: Option<f64>,
    pub shape: Shape,
    pub axis: Option<[&'static str; 5]>,
    pub h: i32,
    /// A dashed line at this value with its label (the refresh's frame time).
    pub mark: Option<(f64, String)>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Item {
    Label(String),
    /// A caption with a small text or legend on its right.
    LabelRight(String, Vec<(Tone, String)>),
    Big(String, String),
    Mid(String),
    Sub(String),
    Row(String, String),
    /// A row whose value has a colour (a state).
    RowTone(String, String, Tone),
    Bar(Vec<(f64, Tone)>),
    Legend(Vec<(Tone, String)>),
    Chart(Chart),
    /// Each thread's busy share, None while it had no ticks to measure.
    Threads(Vec<Option<f64>>),
    Grid(Vec<(String, String)>),
    Link(String, SysPart),
    Buttons(Vec<(String, SysPart, bool)>),
    Rule,
    /// Pushes the items after it to the card's bottom.
    Fill,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Card {
    pub items: Vec<Item>,
    /// Width weight in its row.
    pub span: u32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct CardRow {
    pub cards: Vec<Card>,
    /// Takes the height left over.
    pub grow: bool,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct TabModel {
    /// The text right of the address in the head.
    pub sub: String,
    /// Überblick's state pill.
    pub status: Option<(String, Tone)>,
    pub rows: Vec<CardRow>,
    /// Addresses the tab's `SysPart::Link(i)` open.
    pub links: Vec<String>,
    pub procs: Option<crate::sys_model3::Procs>,
    pub devices: Option<crate::sys_model3::Devices>,
}

/// The desktop's own numbers of the last second (bitscomp's frame counters).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Desk {
    pub fps: Option<f64>,
    pub build_ms: Option<f64>,
    pub render_ms: Option<f64>,
    pub present_ms: Option<f64>,
    pub input_ms: Option<f64>,
    pub frames: u64,
    pub size: Option<(i32, i32)>,
    pub refresh_hz: Option<f64>,
    pub hung: usize,
}

/// Everything a tab is built from.
pub struct Ctx<'a> {
    pub s: &'a Snapshot,
    pub h: &'a History,
    pub v: &'a View,
    pub rows: &'a [Row],
    pub desk: &'a Desk,
    pub off_min: i32,
    /// Where the 24 h points come from: `Some(true)` the journal, `Some(false)` the program's own ring.
    pub journal: Option<bool>,
    pub own_uid: u32,
    pub real_root: bool,
}

impl Ctx<'_> {
    /// A value as shown (masked for reference pictures).
    pub fn m(&self, s: String) -> String {
        // "–" too: whether a value is there this second can vary (no frame drawn in the last second).
        if self.v.mask { MASK.into() } else { s }
    }

    pub fn pts(&self, name: &str) -> Vec<Option<f64>> {
        if self.v.mask { Vec::new() } else { self.h.points(name, self.v.range) }
    }

    pub fn chart(&self, series: Vec<(&str, Tone, bool)>, max: Option<f64>, h: i32) -> Item {
        Item::Chart(Chart { series: series.into_iter().map(|(n, t, a)| (self.pts(n), t, a)).collect(), max, shape: Shape::Line, axis: Some(self.v.range.axis()), h, mark: None })
    }

    /// A small line chart without axis (the Überblick cards).
    pub fn spark(&self, name: &str, t: Tone, max: Option<f64>) -> Item {
        Item::Chart(Chart { series: vec![(self.pts(name), t, false)], max, shape: Shape::Line, axis: None, h: 60, mark: None })
    }
}

pub const MASK: &str = "••••";

pub fn card(span: u32, items: Vec<Item>) -> Card {
    Card { items, span }
}

pub fn row(cards: Vec<Card>, grow: bool) -> CardRow {
    CardRow { cards, grow }
}

fn share(a: u64, total: u64) -> f64 {
    if total == 0 { 0.0 } else { (a as f64 / total as f64).clamp(0.0, 1.0) }
}

pub fn build(c: &Ctx) -> TabModel {
    match c.v.tab {
        Tab::Overview => overview(c),
        Tab::Cpu => cpu(c),
        Tab::Memory => memory(c),
        Tab::Gpu => crate::sys_model2::gpu(c),
        Tab::Disks => crate::sys_model2::disks(c),
        Tab::Net => crate::sys_model2::net(c),
        Tab::Power => crate::sys_model2::power(c),
        Tab::History => crate::sys_model2::history(c),
        Tab::Procs => crate::sys_model3::procs(c),
        Tab::Devices => crate::sys_model3::devices(c),
    }
}

pub const MEASURING: &str = "Misst noch …";

/// The state pill: the first thing that wants attention, else "Alles in Ordnung" (only once processor and memory
/// were measured: no verdict without data).
pub fn status(c: &Ctx) -> (String, Tone) {
    let s = c.s;
    if c.desk.hung > 0 {
        return (if c.desk.hung == 1 { "Ein Programm antwortet nicht".into() } else { format!("{} Programme antworten nicht", c.desk.hung) }, Tone::Danger);
    }
    if s.cpu.busy.is_none() || s.mem.mem.is_none() {
        return (MEASURING.into(), Tone::Muted);
    }
    if let Some(m) = s.mem.mem.filter(|m| m.available * 10 < m.total) {
        let _ = m;
        return ("Der Speicher wird knapp".into(), Tone::Amber);
    }
    if s.heat.cpu.is_some_and(|t| t >= 90.0) || s.heat.gpu.is_some_and(|t| t >= 90.0) {
        return ("Sehr warm".into(), Tone::Amber);
    }
    if s.kernel.crashes.is_some_and(|n| n > 0.0) {
        return ("Ein Absturz seit dem Start".into(), Tone::Amber);
    }
    ("Alles in Ordnung".into(), Tone::Accent)
}

fn up_line(c: &Ctx) -> String {
    let gn = c.s.kernel.generation.as_ref().map_or_else(|| tx::NONE.to_string(), |g| g.0.to_string());
    format!("Läuft seit {} · Generation {}", c.m(tx::duration(c.s.kernel.uptime)), c.m(gn))
}

fn mem_parts(c: &Ctx) -> (Vec<(f64, Tone)>, String) {
    let Some(m) = c.s.mem.mem else { return (Vec::new(), tx::NONE.into()) };
    if c.v.mask {
        return (Vec::new(), MASK.into());
    }
    let gpu = c.s.gpu.gtt.map(|g| g.0).unwrap_or(0).min(m.used());
    (vec![(share(m.used() - gpu, m.total), Tone::Accent), (share(m.cache(), m.total), Tone::Blue), (share(gpu, m.total), Tone::Amber)], tx::bytes(Some(m.available)))
}

pub fn net_now(c: &Ctx) -> (Option<f64>, Option<f64>, u64, String) {
    let (mut rx, mut tx_, mut tot) = (None::<f64>, None::<f64>, 0u64);
    for n in &c.s.nets {
        // Only rates already measured: the first sample after opening has none yet ("–", not 0).
        rx = n.rx.map(|v| rx.unwrap_or(0.0) + v).or(rx);
        tx_ = n.tx.map(|v| tx_.unwrap_or(0.0) + v).or(tx_);
        tot += n.rx_total;
    }
    let main = c.s.nets.iter().filter(|n| n.up != Some(false)).max_by_key(|n| n.rx_total).map(|n| {
        let kind = if n.wifi.is_some() || n.name.starts_with("wl") { "WLAN" } else { "Kabel" };
        format!("{kind} {}", n.name)
    });
    (rx, tx_, tot, main.unwrap_or_default())
}

fn mb_s(v: Option<f64>) -> String {
    v.map_or_else(|| tx::NONE.into(), |b| tx::dec(b / 1_048_576.0, 1))
}

/// The biggest program rows by CPU (Überblick's "Was gerade arbeitet").
pub fn busiest(rows: &[Row], n: usize) -> Vec<Row> {
    let mut v: Vec<Row> = rows.to_vec();
    crate::sys_group::sort(&mut v, crate::sys_group::Column::Cpu, false);
    v.truncate(n);
    v
}

fn overview(c: &Ctx) -> TabModel {
    let s = c.s;
    let threads = s.facts.threads.map_or_else(|| tx::NONE.into(), |t| format!("{t} Threads"));
    let load = s.cpu.load.map_or_else(|| tx::NONE.to_string(), |l| format!("{} · {} · {}", tx::dec(l.0, 2), tx::dec(l.1, 2), tx::dec(l.2, 2)));
    let cpu = card(1, vec![Item::Label("Prozessor".into()), Item::Big(c.m(tx::pct0(s.cpu.busy)), threads), c.spark("cpu", Tone::Accent, Some(100.0)), Item::Sub(format!("Last {}", c.m(load)))]);
    let (bar, free) = mem_parts(c);
    let total = s.mem.mem.map(|m| format!("von {}", tx::bytes(Some(m.total)))).unwrap_or_default();
    let mem = card(1, vec![
        Item::Label("Arbeitsspeicher".into()),
        Item::Big(c.m(tx::bytes(s.mem.mem.map(|m| m.used()))), total),
        Item::Bar(bar),
        Item::Legend(vec![(Tone::Accent, "Programme".into()), (Tone::Blue, "Zwischenspeicher".into()), (Tone::Amber, "Grafik".into())]),
        Item::Sub(format!("{} frei verfügbar", c.m(free))),
    ]);
    let size = c.desk.size.map_or_else(String::new, |(w, h)| format!("{w} × {h}"));
    let gname = s.facts.gpu_name.clone().map_or_else(|| "Grafik".to_string(), |n| format!("Grafik · {n}"));
    let mut g = vec![Item::Label(gname), Item::Big(c.m(tx::pct0(s.gpu.busy)), size.clone()), c.spark("gpu", Tone::Blue, Some(100.0))];
    g.push(Item::Sub(if s.gpu.busy.is_none() { tx::NO_SOURCE.into() } else { format!("Bild zeichnen {} · zeigen {}", c.m(tx::ms(c.desk.render_ms)), c.m(tx::ms(c.desk.present_ms))) }));
    let (rx, txr, tot, main) = net_now(c);
    let net = card(1, vec![
        Item::Label(if main.is_empty() { "Netz".into() } else { format!("Netz · {main}") }),
        Item::Big(format!("↓ {}", c.m(mb_s(rx))), format!("MB/s · ↑ {}", c.m(mb_s(txr)))),
        c.spark("net.rx", Tone::Amber, None),
        Item::Sub(format!("Seit dem Start {} empfangen", c.m(tx::bytes(Some(tot))))),
    ]);
    let mut disk = vec![Item::Label("Datenträger".into())];
    for m in c.s.mounts.iter().take(3) {
        disk.push(Item::Row(mount_name(&m.path), c.m(format!("{} / {}", tx::bytes(Some(m.used)), tx::bytes(Some(m.total))))));
        disk.push(Item::Bar(if c.v.mask { Vec::new() } else { vec![(share(m.used, m.total), Tone::Accent)] }));
    }
    if c.s.mounts.is_empty() {
        disk.push(Item::Sub("Keine Datenträger gefunden.".into()));
    }
    let (r, w) = disk_rates(c);
    disk.push(Item::Sub(format!("Lesen {} · Schreiben {}", c.m(tx::rate(r)), c.m(tx::rate(w)))));
    disk.push(Item::Sub(format!("NVMe {}", c.m(tx::celsius(s.heat.nvme)))));
    let heat = card(1, vec![
        Item::Label("Strom und Wärme".into()),
        Item::Grid(vec![
            ("Prozessor".into(), c.m(tx::celsius(s.heat.cpu))),
            ("Grafik".into(), c.m(tx::celsius(s.heat.gpu))),
            ("Prozessor-Paket".into(), c.m(tx::watt(s.heat.cpu_power))),
            ("Grafik".into(), c.m(tx::watt(s.heat.gpu_power))),
        ]),
        Item::Sub(heat_words(c)),
    ]);
    let mut work = vec![Item::LabelRight("Was gerade arbeitet".into(), Vec::new()), Item::Link(format!("Alle {} Prozesse", if c.v.mask { MASK.into() } else { s.proc_count.map_or_else(|| tx::NONE.into(), |n| n.to_string()) }), SysPart::Tab(Tab::Procs))];
    if c.v.mask {
        work.extend((0..5).map(|_| Item::Row(MASK.into(), MASK.into())));
    } else {
        for r in busiest(c.rows, 5) {
            work.push(Item::Row(r.name.clone(), format!("{} · {}", tx::pct(r.cpu), tx::bytes(r.mem))));
        }
    }
    let crash = match (s.kernel.crashes, s.kernel.crash) {
        (Some(n), _) if n > 0.0 => format!("{} seit dem Start", tx::int(n as u64)),
        (Some(_), _) => "keiner seit dem Start".into(),
        (None, Some((wall, g))) => format!("zuletzt {} · Gen {g}", tx::date_time(wall, c.off_min)),
        (None, None) => "keiner gespeichert".into(),
    };
    let this = card(1, vec![
        Item::Label("Dieser Rechner".into()),
        Item::Row("BitsOS".into(), c.m(format!("V2 · Generation {}", s.kernel.generation.as_ref().map_or_else(|| tx::NONE.to_string(), |g| g.0.to_string())))),
        // Masked too: a QEMU guest shows the host's processor.
        Item::Row("Prozessor".into(), c.m(s.facts.cpu_model.clone().unwrap_or_else(|| tx::NONE.into()))),
        Item::Row("Grafik".into(), c.m(s.facts.gpu_name.clone().unwrap_or_else(|| tx::NONE.into()))),
        Item::Row("Speicher".into(), c.m(tx::bytes(s.facts.mem_total))),
        Item::Row("Bildschirm".into(), if size.is_empty() { tx::NONE.into() } else { size.clone() }),
        Item::Row("Abstürze".into(), c.m(crash)),
    ]);
    TabModel {
        sub: up_line(c),
        status: Some(if c.v.mask { ("Alles in Ordnung".into(), Tone::Accent) } else { status(c) }),
        rows: vec![row(vec![cpu, mem, card(1, g), net], false), row(vec![card(1, disk), heat, card(1, work), this], true)],
        ..TabModel::default()
    }
}

pub fn mount_name(p: &str) -> String {
    match p {
        "/" => "System (/)".into(),
        "/data" => "Daten (/data)".into(),
        "/boot" => "Startteil (/boot)".into(),
        p => p.to_string(),
    }
}

pub fn disk_rates(c: &Ctx) -> (Option<f64>, Option<f64>) {
    let sum = |f: fn(&crate::sys_snap::Disk) -> Option<f64>| c.s.disks.iter().filter_map(f).fold(None, |a: Option<f64>, x| Some(a.unwrap_or(0.0) + x));
    (sum(|d| d.read), sum(|d| d.write))
}

pub fn heat_words(c: &Ctx) -> String {
    match c.s.heat.cpu.or(c.s.heat.gpu) {
        None => tx::NO_SOURCE.into(),
        Some(t) if t >= 90.0 => "Sehr warm: der Prozessor bremst sich vielleicht.".into(),
        Some(t) if t >= 75.0 => "Warm, aber im erlaubten Bereich.".into(),
        Some(_) => "Kühl, alles im grünen Bereich.".into(),
    }
}

fn cpu(c: &Ctx) -> TabModel {
    let s = c.s;
    let model = s.facts.cpu_model.clone().unwrap_or_else(|| "Prozessor".into());
    let cores = s.facts.cores.map(|n| format!(" · {n} Kerne")).unwrap_or_default();
    let threads = s.facts.threads.map(|n| format!(" · {n} Threads")).unwrap_or_default();
    let pts = c.h.points("cpu", c.v.range);
    let peak = History::summary(&pts).map_or_else(
        || "noch keine Messwerte".to_string(),
        |(mean, peak, _)| format!("im Schnitt {} · Spitze {}", tx::pct0(Some(mean)), tx::pct0(Some(peak))),
    );
    let range = match c.v.range {
        Range::Minute => "letzte Minute",
        Range::Hour => "letzte Stunde",
        Range::Day => "letzte 24 Stunden",
    };
    let chart = card(2, vec![
        Item::LabelRight(format!("Auslastung · {range}"), vec![(Tone::Accent, "Programme".into()), (Tone::Blue, "Kern".into())]),
        Item::Big(c.m(tx::pct0(s.cpu.busy)), c.m(peak)),
        c.chart(vec![("cpu.programs", Tone::Accent, true), ("cpu.kernel", Tone::Blue, false)], Some(100.0), 220),
    ]);
    let per = card(1, vec![
        Item::Label("Je Thread · jetzt".into()),
        Item::Threads(if c.v.mask { vec![Some(0.0); s.cpu.threads.len()] } else { s.cpu.threads.clone() }),
        Item::Fill,
        Item::Sub("Zwei Threads teilen sich einen Kern: 0 und 1 sind Kern 1, und so weiter.".into()),
    ]);
    let mid = |label: &str, v: String, sub: String| card(1, vec![Item::Label(label.into()), Item::Mid(c.m(v)), Item::Sub(sub)]);
    let peak_t = History::summary(&c.h.points("temp.cpu", Range::Day)).map(|(_, p, _)| format!("Spitze {}", tx::celsius(Some(p))));
    let load = s.cpu.load;
    let small = vec![
        mid("Takt", tx::khz(s.cpu.cur_khz), s.cpu.max_khz.map_or_else(|| tx::NO_SOURCE.into(), |m| format!("höchstens {}", tx::khz(Some(m))))),
        mid("Temperatur", tx::celsius(s.heat.cpu), if s.heat.cpu.is_none() { tx::NO_SOURCE.into() } else { c.m(peak_t.unwrap_or_default()) }),
        mid("Strom", tx::watt(s.heat.cpu_power), if s.heat.cpu_power.is_none() { tx::NO_SOURCE.into() } else { "ganzes Paket".into() }),
        mid("Wartende", load.map_or_else(|| tx::NONE.into(), |l| tx::dec(l.0, 2)), load.map_or_else(String::new, |l| c.m(format!("1 · 5 · 15 Min: {} · {} · {}", tx::dec(l.0, 2), tx::dec(l.1, 2), tx::dec(l.2, 2))))),
        mid("Wechsel", s.cpu.switches.map_or_else(|| tx::NONE.into(), |w| format!("{}/s", tx::int(w as u64))), "zwischen Threads".into()),
        mid("Laufend", format!("{} / {}", s.proc_count.map_or_else(|| tx::NONE.into(), |n| n.to_string()), s.cpu.all_threads.map_or_else(|| tx::NONE.into(), |n| n.to_string())), "Prozesse / Threads".into()),
    ];
    let sleep = card(1, vec![
        Item::Label("Schlaf der Kerne".into()),
        Item::Sub("Untätige Kerne schlafen tief, wo der Rechner es kann: das spart Strom und kostet beim Aufwachen etwas Zeit.".into()),
    ]);
    TabModel {
        sub: format!("{model}{cores}{threads}"),
        rows: vec![row(vec![chart, per], false), row(small, false), row(vec![sleep], false)],
        ..TabModel::default()
    }
}

fn memory(c: &Ctx) -> TabModel {
    let s = c.s;
    let m = s.mem.mem;
    let (bar, free) = mem_parts(c);
    let chart = card(2, vec![
        Item::LabelRight("Belegt".into(), vec![(Tone::Accent, "Programme".into()), (Tone::Blue, "Zwischenspeicher".into())]),
        Item::Big(c.m(tx::bytes(m.map(|m| m.used()))), m.map_or_else(String::new, |m| format!("von {}", tx::bytes(Some(m.total))))),
        c.chart(vec![("mem.used", Tone::Accent, true), ("mem.cache", Tone::Blue, false)], m.map(|m| m.total as f64), 220),
    ]);
    let parts = card(1, vec![
        Item::Label("Aufteilung".into()),
        Item::Bar(bar),
        Item::Row("Programme".into(), c.m(tx::bytes(m.map(|m| m.used())))),
        Item::Row("Zwischenspeicher".into(), c.m(tx::bytes(m.map(|m| m.cache())))),
        Item::Row("Grafik (aus dem Arbeitsspeicher)".into(), c.m(tx::bytes(s.gpu.gtt.map(|g| g.0)))),
        Item::Row("Geteilt".into(), c.m(tx::bytes(m.map(|m| m.shmem)))),
        Item::Row("Frei verfügbar".into(), c.m(free)),
        Item::Rule,
        Item::Row("Auslagerung".into(), m.map_or_else(|| tx::NONE.into(), |m| if m.swap_total == 0 { "keine".into() } else { c.m(format!("{} / {}", tx::bytes(Some(m.swap_total - m.swap_free)), tx::bytes(Some(m.swap_total)))) })),
    ]);
    let mut top = vec![Item::Label("Die größten Verbraucher".into())];
    let mut big: Vec<Row> = c.rows.to_vec();
    crate::sys_group::sort(&mut big, crate::sys_group::Column::Mem, false);
    if c.v.mask {
        top.extend((0..5).map(|_| Item::Row(MASK.into(), MASK.into())));
    } else {
        top.extend(big.iter().take(6).map(|r| Item::Row(r.name.clone(), tx::bytes(r.mem))));
    }
    top.push(Item::Link("Alle Prozesse".into(), SysPart::Tab(Tab::Procs)));
    let k = &s.kernel;
    let press = card(1, vec![
        Item::Label("Engpässe".into()),
        Item::Row("Speicher freigeräumt".into(), k.reclaim.map_or_else(|| tx::NONE.into(), |n| c.m(format!("{}×", tx::int(n as u64))))),
        Item::Row("Programme beendet wegen Speicher".into(), k.oom.map_or_else(|| tx::NONE.into(), |n| c.m(tx::int(n as u64)))),
        Item::Sub(if k.oom.is_none() && k.reclaim.is_none() { tx::NO_SOURCE.into() } else { "Seit dem Start.".into() }),
    ]);
    TabModel {
        sub: m.map_or_else(String::new, |m| format!("{} eingebaut", tx::bytes(Some(m.total)))),
        rows: vec![row(vec![chart, parts], false), row(vec![card(1, top), press], true)],
        ..TabModel::default()
    }
}

#[cfg(test)]
#[path = "sys_model_tests.rs"]
mod tests;
