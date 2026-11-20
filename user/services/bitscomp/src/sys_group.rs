//! SYS-A: the task manager's rows. Every process lands in one of three parts, as in the design: programs with a
//! window (their helper processes, found down the process tree, under them), Georg's other programs in the
//! background (one row per process group), and BitsOS itself (root's processes, the known services by
//! name, the desktop). A row sums its processes; its state is "antwortet nicht" when a window is hung (bitscomp's
//! ping), "läuft" when it works, else "wartet".

use std::collections::{BTreeMap, HashSet};

use crate::sys_snap::Proc;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Part {
    Window,
    Background,
    System,
}

impl Part {
    pub fn head(self) -> &'static str {
        match self {
            Part::Window => "Mit Fenster",
            Part::Background => "Im Hintergrund",
            Part::System => "BitsOS selbst",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum State {
    Runs,
    Waits,
    Hung,
    Stopped,
    Ended,
}

impl State {
    pub fn word(self) -> &'static str {
        match self {
            State::Runs => "läuft",
            State::Waits => "wartet",
            State::Hung => "antwortet nicht",
            State::Stopped => "angehalten",
            State::Ended => "beendet",
        }
    }
}

/// A window as bitscomp knows it (`ctl list`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Win {
    pub id: u64,
    pub pid: u32,
    pub app_id: String,
    pub title: String,
    pub hung: bool,
    pub x11: bool,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Row {
    pub part: Option<Part>,
    pub name: String,
    /// The small text after the name ("über Steam", "4 Prozesse").
    pub note: String,
    /// The main process first, then its helpers.
    pub pids: Vec<u32>,
    pub wins: Vec<Win>,
    pub app: String,
    pub comm: String,
    pub argv: Vec<String>,
    pub uid: Option<u32>,
    pub pgrp: u32,
    pub nice: i32,
    pub threads: u32,
    pub start: u64,
    pub state: Option<State>,
    pub cpu: Option<f64>,
    pub mem: Option<u64>,
    pub shared: Option<u64>,
    pub gpu: Option<u64>,
    pub disk: Option<f64>,
    pub files: Option<u32>,
    pub service: bool,
    /// The desktop itself or init: never ended from here.
    pub core: bool,
    pub steam: bool,
}

impl Row {
    pub fn main(&self) -> u32 {
        self.pids.first().copied().unwrap_or(0)
    }
}

/// BitsOS's own programs by their command name, with the name the task manager shows.
pub const SERVICES: [(&str, &str); 19] = [
    ("init", "Start und Dienste"),
    ("bitscomp", "Desktop"),
    ("settingsd", "Einstellungsdienst"),
    ("ctxd", "Kontextdienst"),
    ("assist", "Assistent"),
    ("bitsaccess", "Wer hat was gemacht"),
    ("bitsjournal", "Protokoll"),
    ("bitsmcp", "Fernsteuerung"),
    ("bitspkgd", "Programmverwaltung"),
    ("bitspkg", "Programmverwaltung"),
    ("atspid", "Bedienhilfen"),
    ("bitssnap", "Sicherungen"),
    ("bitsinstall", "Installation"),
    ("bitsmusik", "Musik"),
    ("bitsbench", "Leistungsmessung"),
    ("Xwayland", "Brücke für X11-Programme"),
    ("klogd", "Kernprotokoll"),
    ("bitsnetd", "Netzdienst"),
    ("wpa_supplicant", "WLAN-Anmeldung"),
];

pub fn service_name(comm: &str) -> Option<&'static str> {
    SERVICES.iter().find(|(c, _)| *c == comm).map(|(_, n)| *n)
}

/// What the task manager needs besides the processes and windows.
pub struct Ctx<'a> {
    pub own_pid: u32,
    pub own_uid: u32,
    /// The library's name of an app id or a command, if it knows one.
    pub name_of: &'a dyn Fn(&str) -> Option<String>,
}

fn is_service(p: &Proc, c: &Ctx) -> bool {
    p.uid == Some(0) || p.pid == c.own_pid || p.pid == 1 || service_name(&p.comm).is_some() || p.uid.is_some_and(|u| u != c.own_uid)
}

/// The rows of every process, by part (programs with windows, background, BitsOS), unsorted inside a part.
pub fn group(procs: &[Proc], wins: &[Win], c: &Ctx) -> Vec<Row> {
    let by_pid: BTreeMap<u32, &Proc> = procs.iter().map(|p| (p.pid, p)).collect();
    let mut kids: BTreeMap<u32, Vec<u32>> = BTreeMap::new();
    for p in procs {
        kids.entry(p.ppid).or_default().push(p.pid);
    }
    let mut taken: HashSet<u32> = HashSet::new();
    let mut rows = Vec::new();
    // Programs with a window: the window's process and its non-window, non-service descendants.
    let mut owners: Vec<u32> = Vec::new();
    for w in wins {
        if by_pid.contains_key(&w.pid) && w.pid != c.own_pid && !owners.contains(&w.pid) {
            owners.push(w.pid);
        }
    }
    taken.extend(owners.iter().copied());
    for &o in &owners {
        let mut pids = vec![o];
        let mut stack = kids.get(&o).cloned().unwrap_or_default();
        while let Some(k) = stack.pop() {
            let Some(p) = by_pid.get(&k) else { continue };
            if taken.contains(&k) || is_service(p, c) {
                continue;
            }
            taken.insert(k);
            pids.push(k);
            stack.extend(kids.get(&k).cloned().unwrap_or_default());
        }
        let ws: Vec<Win> = wins.iter().filter(|w| w.pid == o).cloned().collect();
        rows.push(row(Part::Window, pids, ws, &by_pid, c));
    }
    // BitsOS itself: one row per process.
    let services: Vec<u32> = procs.iter().filter(|p| !taken.contains(&p.pid) && is_service(p, c)).map(|p| p.pid).collect();
    for pid in services {
        taken.insert(pid);
        rows.push(row(Part::System, vec![pid], Vec::new(), &by_pid, c));
    }
    // The rest: one row per process group (a program and the helpers it did not put into a group of their own,
    // as a shell's job is one group), led by the group's leader when it is among them.
    let mut groups: BTreeMap<u32, Vec<u32>> = BTreeMap::new();
    for p in procs.iter().filter(|p| !taken.contains(&p.pid)) {
        groups.entry(p.pgrp).or_default().push(p.pid);
    }
    for (pgrp, mut pids) in groups {
        let lead = if pids.contains(&pgrp) { pgrp } else { pids[0] };
        pids.retain(|p| *p != lead);
        pids.insert(0, lead);
        rows.push(row(Part::Background, pids, Vec::new(), &by_pid, c));
    }
    rows
}

fn steam_above(pid: u32, by_pid: &BTreeMap<u32, &Proc>) -> bool {
    let mut at = pid;
    for _ in 0..64 {
        let Some(p) = by_pid.get(&at) else { return false };
        if p.comm == "steam" || p.comm == "steamwebhelper" || p.comm.starts_with("reaper") {
            return at != pid;
        }
        if p.ppid == at {
            return false;
        }
        at = p.ppid;
    }
    false
}

fn sum_f(v: impl Iterator<Item = Option<f64>>) -> Option<f64> {
    v.fold(None, |a, x| match (a, x) {
        (Some(a), Some(x)) => Some(a + x),
        (a, None) => a,
        (None, x) => x,
    })
}

fn sum_u(v: impl Iterator<Item = Option<u64>>) -> Option<u64> {
    sum_f(v.map(|x| x.map(|x| x as f64))).map(|x| x as u64)
}

fn row(part: Part, pids: Vec<u32>, wins: Vec<Win>, by_pid: &BTreeMap<u32, &Proc>, c: &Ctx) -> Row {
    let main = by_pid[&pids[0]];
    let members: Vec<&Proc> = pids.iter().filter_map(|p| by_pid.get(p).copied()).collect();
    let app = wins.first().map(|w| w.app_id.clone()).unwrap_or_default();
    let steam = app.starts_with("steam_app_") || steam_above(main.pid, by_pid);
    let exe = main.argv.first().and_then(|a| a.rsplit('/').next()).unwrap_or("").to_string();
    let name = match part {
        Part::System => service_name(&main.comm).map(String::from).unwrap_or_else(|| main.comm.clone()),
        Part::Window => {
            let title = wins.first().map(|w| w.title.trim().to_string()).unwrap_or_default();
            (c.name_of)(&app)
                .filter(|_| !app.starts_with("steam_app_"))
                .or_else(|| (!title.is_empty() && title.chars().count() <= 40).then_some(title))
                .or_else(|| (!app.is_empty()).then(|| app.clone()))
                .unwrap_or_else(|| main.comm.clone())
        }
        Part::Background => (c.name_of)(&exe).or_else(|| (c.name_of)(&main.comm)).unwrap_or_else(|| main.comm.clone()),
    };
    let hung = wins.iter().any(|w| w.hung);
    let cpu = sum_f(members.iter().map(|p| p.cpu));
    let state = if hung {
        State::Hung
    } else if main.state == 'Z' || main.state == 'X' {
        State::Ended
    } else if main.state == 'T' || main.state == 't' {
        State::Stopped
    } else if main.state == 'R' || cpu.is_some_and(|c| c >= 0.05) {
        State::Runs
    } else {
        State::Waits
    };
    let note = match (part, pids.len()) {
        (Part::Window, _) if steam && !app.is_empty() => "über Steam".to_string(),
        (_, n) if n > 1 => format!("{n} Prozesse"),
        _ => String::new(),
    };
    Row {
        part: Some(part),
        note,
        wins,
        app,
        comm: main.comm.clone(),
        argv: main.argv.clone(),
        uid: main.uid,
        pgrp: main.pgrp,
        nice: main.nice,
        threads: members.iter().map(|p| p.threads).sum(),
        start: main.start,
        state: Some(state),
        cpu,
        mem: sum_u(members.iter().map(|p| p.rss)),
        shared: sum_u(members.iter().map(|p| p.shared)),
        gpu: sum_u(members.iter().map(|p| p.gpu_mem)),
        disk: sum_f(members.iter().map(|p| p.disk)),
        files: members.iter().map(|p| p.files).fold(None, |a, x| match (a, x) {
            (Some(a), Some(x)) => Some(a + x),
            (a, None) => a,
            (None, x) => x,
        }),
        // A window of BitsOS's own (Installation, Musik, the assistant) is a service too: it asks first and teaches no habit.
        service: part == Part::System || is_service(main, c),
        core: main.pid == c.own_pid || main.pid == 1,
        steam,
        name,
        pids,
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Filter {
    #[default]
    All,
    Window,
    Background,
    System,
}

impl Filter {
    pub const ALL: [Filter; 4] = [Filter::All, Filter::Window, Filter::Background, Filter::System];
    pub fn name(self) -> &'static str {
        match self {
            Filter::All => "Alle",
            Filter::Window => "Mit Fenster",
            Filter::Background => "Hintergrund",
            Filter::System => "BitsOS selbst",
        }
    }
    pub fn word(self) -> &'static str {
        match self {
            Filter::All => "alle",
            Filter::Window => "fenster",
            Filter::Background => "hintergrund",
            Filter::System => "bitsos",
        }
    }
    pub fn keeps(self, p: Option<Part>) -> bool {
        match self {
            Filter::All => true,
            Filter::Window => p == Some(Part::Window),
            Filter::Background => p == Some(Part::Background),
            Filter::System => p == Some(Part::System),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Column {
    Name,
    State,
    #[default]
    Cpu,
    Mem,
    Gpu,
    Disk,
    Net,
}

impl Column {
    pub const ALL: [Column; 7] = [Column::Name, Column::State, Column::Cpu, Column::Mem, Column::Gpu, Column::Disk, Column::Net];
    pub fn head(self) -> &'static str {
        match self {
            Column::Name => "Name",
            Column::State => "Zustand",
            Column::Cpu => "CPU",
            Column::Mem => "Speicher",
            Column::Gpu => "Grafik",
            Column::Disk => "Platte",
            Column::Net => "Netz",
        }
    }
    pub fn word(self) -> &'static str {
        match self {
            Column::Name => "name",
            Column::State => "zustand",
            Column::Cpu => "cpu",
            Column::Mem => "speicher",
            Column::Gpu => "grafik",
            Column::Disk => "platte",
            Column::Net => "netz",
        }
    }
}

fn matches(r: &Row, q: &str) -> bool {
    let q = q.trim().to_lowercase();
    q.is_empty() || [&r.name, &r.comm, &r.app].iter().any(|s| s.to_lowercase().contains(&q)) || r.pids.iter().any(|p| p.to_string() == q)
}

/// Sorts in place: by the column (numbers biggest first unless `up`), the name breaking ties.
pub fn sort(rows: &mut [Row], col: Column, up: bool) {
    let key = |r: &Row| -> f64 {
        match col {
            Column::Cpu => r.cpu.unwrap_or(-1.0),
            Column::Mem => r.mem.map_or(-1.0, |m| m as f64),
            Column::Gpu => r.gpu.map_or(-1.0, |m| m as f64),
            Column::Disk => r.disk.unwrap_or(-1.0),
            Column::State => r.state.map_or(9.0, |s| s as u8 as f64),
            Column::Name | Column::Net => 0.0,
        }
    };
    rows.sort_by(|a, b| {
        let by_name = a.name.to_lowercase().cmp(&b.name.to_lowercase()).then(a.main().cmp(&b.main()));
        let o = match col {
            Column::Name | Column::Net => by_name,
            Column::State => key(a).total_cmp(&key(b)).then(by_name),
            _ => key(b).total_cmp(&key(a)).then(by_name),
        };
        if up { o.reverse() } else { o }
    });
}

/// What the table shows: the parts in order, each sorted, filtered and searched. With the filter "Alle" and no
/// search, BitsOS's part shows its `top` biggest rows and says how many more there are.
pub fn shown(rows: &[Row], f: Filter, query: &str, col: Column, up: bool, top: usize) -> Vec<(Part, Vec<Row>, usize)> {
    let mut out = Vec::new();
    for part in [Part::Window, Part::Background, Part::System] {
        if !f.keeps(Some(part)) {
            continue;
        }
        let mut v: Vec<Row> = rows.iter().filter(|r| r.part == Some(part) && matches(r, query)).cloned().collect();
        sort(&mut v, col, up);
        let mut more = 0;
        if part == Part::System && f == Filter::All && query.trim().is_empty() && v.len() > top {
            more = v.len() - top;
            v.truncate(top);
        }
        if !v.is_empty() || more > 0 {
            out.push((part, v, more));
        }
    }
    out
}

/// The counts on the filter pills: the rows each filter shows.
pub fn counts(rows: &[Row]) -> [usize; 4] {
    let n = |p: Part| rows.iter().filter(|r| r.part == Some(p)).count();
    let (w, b, s) = (n(Part::Window), n(Part::Background), n(Part::System));
    [w + b + s, w, b, s]
}

#[cfg(test)]
#[path = "sys_group_tests.rs"]
mod tests;
