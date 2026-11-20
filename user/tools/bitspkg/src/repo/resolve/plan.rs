//! The plan of one install as the page shows it (plan 1.5): packages in install order, sizes, every provider
//! choice, the Zusätze, notes and refusals; German, one fact per line (report.rs style).

use crate::space::human;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rule {
    /// data/providers.txt.
    Table,
    /// The provider the program's previous version used.
    Previous,
    /// The first provider in source order (pacman --noconfirm).
    First,
}

impl Rule {
    pub fn name(self) -> &'static str {
        match self {
            Rule::Table => "table",
            Rule::Previous => "previous",
            Rule::First => "first",
        }
    }
    fn de(self) -> &'static str {
        match self {
            Rule::Table => "Vorgabe von BitsOS",
            Rule::Previous => "wie in der vorigen Version",
            Rule::First => "erster Anbieter in der Reihenfolge der Quellen",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Choice {
    pub dep: String,
    pub needed_by: String,
    pub chosen: String,
    pub repo: String,
    pub rule: Rule,
    /// The other candidates, `repo/name`, in source order.
    pub others: Vec<String>,
    /// pacman --noconfirm's pick, `repo/name` (the first candidate).
    pub first: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Optional {
    pub dep: String,
    /// As the package says it (English; the page translates known ones).
    pub reason: String,
    /// Already inside the closure.
    pub present: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Planned {
    pub repo: String,
    pub name: String,
    pub version: String,
    pub csize: u64,
    pub isize: u64,
    pub why: String,
    pub rust: bool,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Plan {
    pub target: String,
    /// In install order (each after its dependencies, cycles aside); empty when refused.
    pub packages: Vec<Planned>,
    pub choices: Vec<Choice>,
    pub optional: Vec<Optional>,
    /// (name, reason): anything here refuses the whole install.
    pub refused: Vec<(String, String)>,
    pub notes: Vec<String>,
    pub download: u64,
    pub installed: u64,
}

impl Plan {
    pub fn ok(&self) -> bool {
        self.refused.is_empty() && !self.packages.is_empty()
    }

    pub fn render(&self) -> String {
        let mut o = String::new();
        if !self.ok() {
            o.push_str(&format!("{} kann nicht installiert werden:\n", self.target));
            for (n, r) in &self.refused {
                o.push_str(&format!("  abgelehnt: {n}: {r}\n"));
            }
            return o;
        }
        let top = self.packages.iter().find(|p| p.why == "Programm");
        if let Some(t) = top {
            o.push_str(&format!("Programm: {} {} aus {}{}\n", t.name, t.version, t.repo, if t.rust { " [Rust]" } else { "" }));
        }
        o.push_str(&format!("{} Pakete, Download {}, installiert {}\n", self.packages.len(), human(self.download), human(self.installed)));
        o.push_str("Pakete (in Installationsreihenfolge):\n");
        for p in &self.packages {
            o.push_str(&format!("  {}/{} {} ({}; {})\n", p.repo, p.name, p.version, human(p.csize), p.why));
        }
        if !self.choices.is_empty() {
            o.push_str("Auswahl:\n");
            for c in &self.choices {
                let others = if c.others.is_empty() { String::new() } else { format!("; auch möglich: {}", c.others.join(", ")) };
                let pacman = if c.rule == Rule::First || c.first == format!("{}/{}", c.repo, c.chosen) { String::new() } else { format!(" (pacman nähme {})", c.first) };
                o.push_str(&format!("  {} (für {}): {}/{} gewählt, {}{pacman}{others}\n", c.dep, c.needed_by, c.repo, c.chosen, c.rule.de()));
            }
        }
        let add: Vec<&Optional> = self.optional.iter().filter(|x| !x.present).collect();
        if !add.is_empty() {
            o.push_str("Zusätze (nicht dabei, einzeln hinzufügbar):\n");
            for x in add {
                o.push_str(&format!("  {}{}\n", x.dep, if x.reason.is_empty() { String::new() } else { format!(": {}", x.reason) }));
            }
        }
        for n in &self.notes {
            o.push_str(&format!("Hinweis: {n}\n"));
        }
        o
    }
}
