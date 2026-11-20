//! SYS-A: the program "System" (`system`, `system/<tab>`) as one page window per address: its ten tabs, what a
//! window shows now (tab, time range, the chosen process or device, the task manager's search, filter and sort)
//! and the controls that react to the mouse (`SysPart`).

use crate::sys_group::{Column, Filter};
use crate::sys_ring::Range;

pub const ADDRESS: &str = "system";
pub const APP_ID: &str = "bitscomp.system";
pub const TITLE: &str = "System";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Tab {
    #[default]
    Overview,
    Cpu,
    Memory,
    Gpu,
    Disks,
    Net,
    Power,
    Procs,
    Devices,
    History,
}

impl Tab {
    pub const ALL: [Tab; 10] = [Tab::Overview, Tab::Cpu, Tab::Memory, Tab::Gpu, Tab::Disks, Tab::Net, Tab::Power, Tab::Procs, Tab::Devices, Tab::History];

    pub fn name(self) -> &'static str {
        match self {
            Tab::Overview => "Überblick",
            Tab::Cpu => "Prozessor",
            Tab::Memory => "Speicher",
            Tab::Gpu => "Grafik",
            Tab::Disks => "Datenträger",
            Tab::Net => "Netz",
            Tab::Power => "Strom und Wärme",
            Tab::Procs => "Prozesse",
            Tab::Devices => "Geräte",
            Tab::History => "Verlauf",
        }
    }

    /// The address word (`system/<word>`), lower case German.
    pub fn word(self) -> &'static str {
        match self {
            Tab::Overview => "überblick",
            Tab::Cpu => "prozessor",
            Tab::Memory => "speicher",
            Tab::Gpu => "grafik",
            Tab::Disks => "datenträger",
            Tab::Net => "netz",
            Tab::Power => "strom",
            Tab::Procs => "prozesse",
            Tab::Devices => "geräte",
            Tab::History => "verlauf",
        }
    }

    /// A word of an address: the German one, its ASCII spelling, a few English ones.
    pub fn of_word(w: &str) -> Option<Tab> {
        let w = w.trim().trim_matches('/').to_lowercase();
        let w = w.replace("ue", "ü").replace("ae", "ä");
        Some(match w.as_str() {
            "" | "überblick" | "overview" => Tab::Overview,
            "prozessor" | "cpu" => Tab::Cpu,
            "speicher" | "memory" | "arbeitsspeicher" => Tab::Memory,
            "grafik" | "gpu" => Tab::Gpu,
            "datenträger" | "disks" | "platten" => Tab::Disks,
            "netz" | "net" | "netzwerk" => Tab::Net,
            "strom" | "strom-und-wärme" | "wärme" | "power" => Tab::Power,
            "prozesse" | "processes" | "taskmanager" => Tab::Procs,
            "geräte" | "devices" => Tab::Devices,
            "verlauf" | "history" => Tab::History,
            _ => return None,
        })
    }

    /// Whether the tab has charts (the time range pills show then).
    pub fn has_range(self) -> bool {
        !matches!(self, Tab::Procs | Tab::Devices)
    }

    /// Whether its sampler needs the process list.
    pub fn needs_procs(self) -> bool {
        matches!(self, Tab::Overview | Tab::Memory | Tab::Procs | Tab::Gpu)
    }
}

/// Whether `a` (normalised) is the program's address, and its tab.
pub fn tab_of(a: &str) -> Option<Tab> {
    let a = a.trim().trim_end_matches('/').to_lowercase();
    if a == ADDRESS {
        return Some(Tab::Overview);
    }
    Tab::of_word(a.strip_prefix("system/")?)
}

pub fn address(t: Tab) -> String {
    format!("{ADDRESS}/{}", t.word())
}

/// Something on the page that reacts to the mouse.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SysPart {
    Tab(Tab),
    Range(Range),
    Search,
    Filter(Filter),
    Sort(Column),
    /// A process row by its main pid.
    Row(u32),
    /// "… N weitere": the filter "BitsOS selbst".
    More,
    Show,
    End,
    Prio(crate::sys_rules::Vorrang),
    WhoDid,
    /// A device in the list of the tab Geräte.
    Device(usize),
    Copy,
    /// A link to another tab or address (by its number in the tab's link list).
    Link(usize),
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct View {
    pub tab: Tab,
    pub range: Range,
    pub filter: Filter,
    pub sort: Column,
    pub up: bool,
    pub query: String,
    /// The chosen process row (main pid, its start ticks so a reused pid is not taken for it).
    pub selected: Option<(u32, u64)>,
    pub device: usize,
    /// Test aid (`ctl system mask on`): values drawn as blocks, so a reference picture does not depend on them.
    pub mask: bool,
}

impl View {
    pub fn of_address(a: &str) -> View {
        View { tab: tab_of(a).unwrap_or_default(), ..View::default() }
    }
}

/// `ctl hot` and the element list name parts so.
pub fn part_name(p: SysPart) -> String {
    match p {
        SysPart::Tab(t) => format!("tab={}", t.word()),
        SysPart::Range(r) => format!("range={}", r.word()),
        SysPart::Search => "search".into(),
        SysPart::Filter(f) => format!("filter={}", f.word()),
        SysPart::Sort(c) => format!("sort={}", c.word()),
        SysPart::Row(pid) => format!("row={pid}"),
        SysPart::More => "more".into(),
        SysPart::Show => "show".into(),
        SysPart::End => "end".into(),
        SysPart::Prio(v) => format!("vorrang={}", v.word()),
        SysPart::WhoDid => "werhatwas".into(),
        SysPart::Device(i) => format!("device={i}"),
        SysPart::Copy => "copy".into(),
        SysPart::Link(i) => format!("link={i}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses_name_the_tabs() {
        assert_eq!(tab_of("system"), Some(Tab::Overview));
        assert_eq!(tab_of("system/"), Some(Tab::Overview));
        assert_eq!(tab_of("system/prozesse"), Some(Tab::Procs));
        assert_eq!(tab_of("System/Datentraeger"), Some(Tab::Disks));
        assert_eq!(tab_of("system/geraete"), Some(Tab::Devices));
        assert_eq!(tab_of("system/strom"), Some(Tab::Power));
        assert_eq!(tab_of("system/unsinn"), None);
        assert_eq!(tab_of("systeme"), None);
        assert_eq!(tab_of("einstellungen/system"), None);
        for t in Tab::ALL {
            assert_eq!(tab_of(&address(t)), Some(t), "{t:?}");
        }
        assert!(!Tab::Procs.has_range() && Tab::Cpu.has_range());
    }

    #[test]
    fn every_tab_name_has_glyphs() {
        for t in Tab::ALL {
            assert!(t.name().chars().all(crate::text::Font::has), "{}", t.name());
        }
    }
}
