//! BitsOS's rules on top of pacman's resolution (plan 1.5): the provider table (data/providers.txt), the system
//! packages (data/system.txt), and what a program is.

use crate::repo::syncdb::Pkg;

const PROVIDERS: &str = include_str!("../../../data/providers.txt");
const SYSTEM: &str = include_str!("../../../data/system.txt");

#[derive(Clone, Debug, PartialEq)]
pub enum Want {
    Name(String),
    /// Among the providers whose name matches the glob, the one providing the highest version.
    Newest(String),
}

#[derive(Clone, Debug, Default)]
pub struct Prefs {
    pub rules: Vec<(String, Want)>,
}

impl Prefs {
    /// The table shipped with bitspkg.
    pub fn builtin() -> Prefs {
        // The file is ours and checked by the tests; a broken line would be a build defect, said here.
        Prefs::parse(PROVIDERS).unwrap_or_else(|e| {
            crate::repo::refresh::event("pkg.resolve.table_broken", &[("reason", &e)]);
            Prefs::default()
        })
    }
    /// No table: pacman --noconfirm's choices (the comparison test).
    pub fn none() -> Prefs {
        Prefs::default()
    }
    pub fn parse(text: &str) -> Result<Prefs, String> {
        let mut rules = Vec::new();
        for (n, l) in text.lines().enumerate() {
            let l = l.trim();
            if l.is_empty() || l.starts_with('#') {
                continue;
            }
            let (dep, want) = l.split_once("->").ok_or_else(|| format!("providers.txt Zeile {}: '->' fehlt", n + 1))?;
            let want = want.trim();
            let w = match want.strip_prefix("newest ") {
                Some(g) => Want::Newest(g.trim().to_string()),
                None if !want.is_empty() && !want.contains(char::is_whitespace) => Want::Name(want.to_string()),
                None => return Err(format!("providers.txt Zeile {}: Paketname fehlt", n + 1)),
            };
            rules.push((dep.trim().to_string(), w));
        }
        Ok(Prefs { rules })
    }
    pub fn rule(&self, dep: &str) -> Option<&Want> {
        self.rules.iter().find(|(d, _)| d == dep).map(|(_, w)| w)
    }
}

/// `*` matches any text (also none).
pub fn glob(pat: &str, s: &str) -> bool {
    let parts: Vec<&str> = pat.split('*').collect();
    if parts.len() == 1 {
        return pat == s;
    }
    let (first, last) = (parts[0], parts[parts.len() - 1]);
    if !s.starts_with(first) || s.len() < first.len() + last.len() || !s.ends_with(last) {
        return false;
    }
    let mut rest = &s[first.len()..s.len() - last.len()];
    for p in &parts[1..parts.len() - 1] {
        match rest.find(p) {
            Some(i) => rest = &rest[i + p.len()..],
            None => return false,
        }
    }
    true
}

pub struct SystemRule {
    pub pattern: String,
    /// Also refused inside a closure.
    pub all: bool,
    pub reason: String,
}

pub fn system_rules() -> Vec<SystemRule> {
    SYSTEM
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter_map(|l| {
            let mut it = l.splitn(3, ' ');
            let (p, s, r) = (it.next()?, it.next()?, it.next()?);
            Some(SystemRule { pattern: p.to_string(), all: s == "all", reason: r.trim().to_string() })
        })
        .collect()
}

/// The reason a package is a system part: as the program (`top`) or inside a closure.
pub fn system(rules: &[SystemRule], name: &str, top: bool) -> Option<String> {
    rules.iter().find(|r| (top || r.all) && glob(&r.pattern, name)).map(|r| r.reason.clone())
}

/// The keyring and mirror list packages bitspkg itself uses are allowed as a top level (plan 1.5).
pub fn own_part(name: &str) -> bool {
    name.ends_with("-keyring") || name.ends_with("-mirrorlist")
}

const TOOL_WORDS: [&str; 10] = ["tool", "utilit", "program", "command", "player", "editor", "viewer", "browser", "game", "terminal"];

/// The plan step's guess that a top level is not a program, from the database alone (the index has no file
/// list). It refuses only what is clearly a part: fonts, documentation, headers, data and icon themes, and
/// `lib*` packages that call themselves a library and nothing that runs. Everything else is decided on the
/// downloaded package's file list (`is_program`, PK-6d).
pub fn not_program(p: &Pkg) -> Option<&'static str> {
    let n = p.name.as_str();
    let d = p.desc.to_lowercase();
    if own_part(n) {
        return None;
    }
    if n.starts_with("ttf-") || n.starts_with("otf-") || n.starts_with("noto-fonts") || n.ends_with("-fonts") || n.ends_with("-font") {
        return Some("eine Schrift");
    }
    if n.ends_with("-docs") || n.ends_with("-doc") || n.ends_with("-headers") {
        return Some("Dokumentation oder Header-Dateien");
    }
    if n.ends_with("-data") || n.ends_with("-icon-theme") {
        return Some("nur Daten");
    }
    if n.starts_with("lib") && !n.starts_with("libre") && (d.contains("librar") || d.contains("bindings")) && !TOOL_WORDS.iter().any(|w| d.contains(w)) {
        return Some("eine Bibliothek");
    }
    None
}

fn norm(p: &str) -> &str {
    p.trim_start_matches("./").trim_start_matches('/')
}

/// Plan 1.5 on a package's own file list (.MTREE or .files): an executable under usr/bin, or a .desktop file in
/// usr/share/applications that is not NoDisplay/Hidden (`desktop` gives a .desktop file's text).
pub fn is_program<'a>(paths: impl IntoIterator<Item = &'a str>, desktop: impl Fn(&str) -> Option<String>) -> bool {
    paths.into_iter().any(|p| {
        let q = norm(p);
        if let Some(b) = q.strip_prefix("usr/bin/") {
            return !b.is_empty() && !b.contains('/');
        }
        if let Some(f) = q.strip_prefix("usr/share/applications/")
            && f.ends_with(".desktop")
            && !f.contains('/')
        {
            return desktop(p).is_some_and(|t| shown(&t));
        }
        false
    })
}

/// NoDisplay=true or Hidden=true in the [Desktop Entry] group hides an entry from menus.
fn shown(text: &str) -> bool {
    let mut main = false;
    for l in text.lines().map(str::trim) {
        if l.starts_with('[') {
            main = l == "[Desktop Entry]";
        } else if main && let Some((k, v)) = l.split_once('=') {
            if matches!(k.trim(), "NoDisplay" | "Hidden") && v.trim().eq_ignore_ascii_case("true") {
                return false;
            }
        }
    }
    true
}

/// Plan 1.5's file rule: kernel modules, firmware or boot files, or nothing but hooks, units, sysusers.d and
/// tmpfiles.d entries (besides licenses and documentation).
pub fn system_by_files<'a>(paths: impl IntoIterator<Item = &'a str>) -> Option<String> {
    let mut any = false;
    let mut only_system = true;
    for p in paths {
        let q = norm(p);
        if q.is_empty() || q.ends_with('/') || q.starts_with('.') {
            continue;
        }
        for (pre, why) in [("usr/lib/modules/", "Linux-Kernmodule"), ("lib/modules/", "Linux-Kernmodule"), ("usr/lib/firmware/", "Firmware"), ("lib/firmware/", "Firmware"), ("boot/", "Dateien für den Start (boot/)")] {
            if q.starts_with(pre) {
                return Some(format!("enthält {why}: BitsOS bringt das selbst mit"));
            }
        }
        if q.starts_with("usr/share/licenses/") || q.starts_with("usr/share/doc/") || q.starts_with("usr/share/man/") {
            continue;
        }
        any = true;
        let sys = q.starts_with("usr/share/libalpm/hooks/")
            || q.starts_with("usr/lib/systemd/")
            || q.starts_with("etc/systemd/")
            || q.starts_with("usr/lib/sysusers.d/")
            || q.starts_with("usr/lib/tmpfiles.d/")
            || q.starts_with("usr/lib/udev/");
        only_system &= sys;
    }
    (any && only_system).then(|| "besteht nur aus Systemdiensten, Hooks und Systemeinträgen".to_string())
}
