//! The Gaming-PC's report page (plan/spec/gaming-pc.md §8, wave GP-ROUND): what the first stick round shows on the
//! firmware screen by itself, writes to the boot stick and answers over MCP. Pure logic, host-tested: the kernel
//! collects the rows from the drivers' getters (`kernel/src/gpreport.rs`), this crate words, groups, pages and names
//! them. Every row is plain German for Georg; the one English fragment a row may carry is a driver's own reason.
#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

pub mod ask;
pub mod file;
pub mod page;
pub mod rows;

use alloc::string::String;
use alloc::vec::Vec;

/// The page's groups, in the order Georg reads them (as on the round-1 page,
/// plan/hw/gaming-pc-round1-2026-11-15/GP000001.TXT).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Group {
    Start,
    Cpu,
    Guard,
    Devices,
    Disks,
    Picture,
    Usb,
    Network,
    Gpu,
    Watchdog,
    Sound,
}

impl Group {
    pub const ALL: [Group; 11] = [
        Group::Start,
        Group::Cpu,
        Group::Guard,
        Group::Devices,
        Group::Disks,
        Group::Picture,
        Group::Usb,
        Group::Network,
        Group::Gpu,
        Group::Watchdog,
        Group::Sound,
    ];

    pub fn title(self) -> &'static str {
        match self {
            Group::Start => "Startprogramm, Profil und Freigaben",
            Group::Cpu => "Prozessor, Uhr und Tabellen",
            Group::Guard => "Geräteschutz",
            Group::Devices => "Geräteliste (beide Anschluss-Stränge)",
            Group::Disks => "Platten",
            Group::Picture => "Bild",
            Group::Usb => "USB",
            Group::Network => "Netzwerk",
            Group::Gpu => "Grafikkarte",
            Group::Watchdog => "Wächter",
            Group::Sound => "Ton",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
    pub group: Group,
    pub text: String,
}

/// One boot's report: the rows in collection order (`text` and `page::layout` sort them by group).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Report {
    /// The first line: which boot and when ("BitsOS-Bericht, Version 5, 47 Sekunden nach dem Start").
    pub head: String,
    pub rows: Vec<Row>,
    /// VB-1: the question's rows at the foot of every page (`ask::lines`; empty: none). Screen only, not in the
    /// file or MCP's text.
    pub ask: Vec<String>,
}

impl Report {
    pub fn new(head: String) -> Report {
        Report { head, rows: Vec::new(), ask: Vec::new() }
    }

    pub fn push(&mut self, group: Group, text: impl Into<String>) {
        let text: String = text.into();
        // One row per line: a getter that hands over several lines in one string still gets one row each.
        for l in text.split('\n').map(|l| l.trim_end()).filter(|l| !l.is_empty()) {
            self.rows.push(Row { group, text: String::from(l) });
        }
    }

    pub fn extend<S: Into<String>>(&mut self, group: Group, lines: impl IntoIterator<Item = S>) {
        for l in lines {
            self.push(group, l);
        }
    }

    /// The rows of `g`, in the order they were added.
    pub fn group(&self, g: Group) -> impl Iterator<Item = &str> {
        self.rows.iter().filter(move |r| r.group == g).map(|r| r.text.as_str())
    }

    /// The whole report as lines: the head, then each group that has rows with its title and its rows indented.
    /// The file on the stick and the MCP answer are these lines; the screen pages them (`page::layout`).
    pub fn lines(&self) -> Vec<String> {
        let mut out = alloc::vec![self.head.clone()];
        for g in Group::ALL {
            let mut rows = self.group(g).peekable();
            if rows.peek().is_none() {
                continue;
            }
            out.push(String::new());
            out.push(String::from(g.title()));
            out.extend(rows.map(|r| alloc::format!("  {r}")));
        }
        out
    }

    /// Renews the file's row in place (`rows::FILE_ROW`); adds it to the start group when there is none.
    pub fn set_file(&mut self, st: &rows::FileState) {
        let text = rows::file(st);
        match self.rows.iter_mut().find(|r| r.text.starts_with(rows::FILE_ROW)) {
            Some(r) => r.text = text,
            None => self.push(Group::Start, text),
        }
    }

    /// The same rows as `other`, the head (its seconds) and the file's row aside: a new collection that says
    /// nothing new is neither redrawn nor written again.
    pub fn same_rows(&self, other: &Report) -> bool {
        let keep = |r: &&Row| !r.text.starts_with(rows::FILE_ROW);
        self.rows.iter().filter(keep).eq(other.rows.iter().filter(keep))
    }

    /// `lines` joined with CR LF: the stick's file is read on Windows too.
    pub fn file_text(&self) -> String {
        let mut s = String::new();
        for l in self.lines() {
            s.push_str(&l);
            s.push_str("\r\n");
        }
        s
    }
}

#[cfg(test)]
mod tests;
