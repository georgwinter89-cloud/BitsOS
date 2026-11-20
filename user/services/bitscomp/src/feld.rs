//! The Feld (Feld.dc.html, desktop.md §0 point 6): one field for everything, opened by a click on the Bühne's Feld
//! bar or by Win+Space (in every preset). What is typed is resolved in a fixed order, and the rows under the field
//! show it, the first row being what Enter does: an address BitsOS knows opens its page, a program of the library
//! starts, everything else is a question to the assistant in the sidebar ("Frage an den Assistenten", always the
//! last row, so a text that names a program can still be asked). Pure: the rows from their inputs; the App side
//! (feld_app.rs) gathers them, the look is feld_draw.rs.

use crate::address::Edit;

/// At most this many program rows: the field answers one wish, the library shows them all.
pub const PROGRAMS_MAX: usize = 4;

pub const PLACEHOLDER: &str = "Sag, was du vorhast, oder gib eine Adresse ein";
pub const ASK: &str = "Frage an den Assistenten";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Address,
    Program,
    Ask,
}

impl Kind {
    /// Its name in `ctl hot` and `ctl feld state`.
    pub fn name(self) -> &'static str {
        match self {
            Kind::Address => "address",
            Kind::Program => "program",
            Kind::Ask => "ask",
        }
    }

    /// The group label above its rows (the design's "Adressen", "Programme").
    pub fn group(self) -> &'static str {
        match self {
            Kind::Address => "Adressen",
            Kind::Program => "Programme",
            Kind::Ask => "Frage",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
    pub kind: Kind,
    pub title: String,
    /// What it acts on: the address to open, the library entry's id, or the question.
    pub target: String,
    /// The quiet text at its right: the address, or the assistant's name.
    pub detail: String,
}

/// A program the library's search found: (id, name, address).
pub type Hit = (String, String, String);

/// The rows for `text`: the address BitsOS knows (title, address), then the library's hits best first (not the one
/// the address names), then the question to `assistant` (None: no assistant in this session). Empty text: none.
pub fn rows(text: &str, address: Option<(String, String)>, programs: &[Hit], assistant: Option<&str>) -> Vec<Row> {
    let q = text.trim();
    if q.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    if let Some((title, addr)) = address {
        out.push(Row { kind: Kind::Address, title, target: q.to_string(), detail: addr });
    }
    for (id, name, addr) in programs.iter().filter(|(_, _, a)| !out.iter().any(|r: &Row| r.detail == *a)).take(PROGRAMS_MAX).collect::<Vec<_>>() {
        out.push(Row { kind: Kind::Program, title: name.clone(), target: id.clone(), detail: addr.clone() });
    }
    if let Some(name) = assistant {
        out.push(Row { kind: Kind::Ask, title: format!("{ASK}: {q}"), target: q.to_string(), detail: name.to_string() });
    }
    out
}

/// The title of an address row: the library's name for it, else its last part in words.
pub fn address_title(addr: &str, library_name: Option<&str>) -> String {
    if let Some(n) = library_name {
        return n.to_string();
    }
    let a = addr.trim_end_matches('/');
    match a {
        "einstellungen" => "Einstellungen".into(),
        "programme" => "Programme".into(),
        "programme/hinzufuegen" | "programme/hinzufügen" => "Programme hinzufügen".into(),
        "programme/aktualisierungen" => "Aktualisierungen".into(),
        _ => crate::suggest::short_name(a),
    }
}

/// What Enter does with `row`, in the foot of the panel.
pub fn says(row: Option<&Row>) -> String {
    match row {
        None => "Tippe eine Adresse, einen Programmnamen oder eine Frage.".into(),
        Some(r) => match r.kind {
            Kind::Address => format!("Enter öffnet {}.", r.detail),
            Kind::Program => format!("Enter startet „{}“.", r.title),
            Kind::Ask => format!("Enter stellt die Frage in der Seitenleiste an {}.", r.detail),
        },
    }
}

/// The Feld's state while it is open.
#[derive(Default)]
pub struct Feld {
    pub open: bool,
    /// The typed text, with the address bar's editing (`pick` is the chosen row; None: the first).
    pub edit: Edit,
    pub hover: Option<usize>,
    pub hover_esc: bool,
    /// Injected input typed into it: a question from it is marked as asked from outside (R8c), as the
    /// sidebar's own line does.
    pub remote: bool,
    /// The rows for the text typed now (`feld_refresh`).
    pub rows: Vec<Row>,
    pub images: crate::feld_draw::Images,
}

impl Feld {
    /// The chosen row's index among `n` rows.
    pub fn pick(&self, n: usize) -> Option<usize> {
        (n > 0).then(|| self.edit.pick.unwrap_or(0).min(n - 1))
    }
}

#[cfg(test)]
#[path = "feld_tests.rs"]
mod tests;
