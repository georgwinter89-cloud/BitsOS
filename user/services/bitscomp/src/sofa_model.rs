//! GM-6b: what the sofa mode's Start and Bibliothek show (plan/spec/sofa-mode.md 2.1-2.3, 3.1-3.4), as pure
//! functions of the shared library index (with its Steam source), BitsOS's play time, the favourites and the music
//! player's cached answer: the collections of 3.1 as views, their sort orders (`sofa.sort`), the tiles with their
//! German lines ("Proton · 19:40", "Linux · Gestern"), the hero pick and its kontext colour by a stable hash, and
//! what each tile starts (3.2). Host-tested in sofa_model_tests.rs.

use crate::library::{Entry, Index, Source, order};
use crate::playtime::Book;
use crate::sofa_music::Music;

/// The collections of the left column (2.2), in its order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Coll {
    Spiele,
    Windows,
    YouTube,
    Brave,
    Musik,
    BigPicture,
}

impl Coll {
    pub const LIBRARY: [Coll; 6] = [Coll::Spiele, Coll::Windows, Coll::YouTube, Coll::Brave, Coll::Musik, Coll::BigPicture];
    /// Medien (2.3): the library on these alone, opened on YouTube.
    pub const MEDIEN: [Coll; 3] = [Coll::YouTube, Coll::Brave, Coll::Musik];

    pub fn title(self) -> &'static str {
        match self {
            Coll::Spiele => "Spiele",
            Coll::Windows => "Windows-Spiele",
            Coll::YouTube => "YouTube",
            Coll::Brave => "Brave",
            Coll::Musik => "Musik",
            Coll::BigPicture => "Steam Big Picture",
        }
    }

    pub fn word(self) -> &'static str {
        match self {
            Coll::Spiele => "spiele",
            Coll::Windows => "windows",
            Coll::YouTube => "youtube",
            Coll::Brave => "brave",
            Coll::Musik => "musik",
            Coll::BigPicture => "bigpicture",
        }
    }
}

/// What A on a tile does (3.2).
#[derive(Clone, Debug, PartialEq)]
pub enum Act {
    /// The library entry `id` by the launcher rule (`restore`: its newest minimised window first), started with
    /// `extra` after its Exec.
    Start { id: String, extra: Vec<String>, restore: bool },
    /// A command of its own (Steam without a stick entry: `steam <url>`).
    Argv(Vec<String>),
    /// A request to the music player.
    Music { kind: &'static str, name: String },
    /// Nothing; the German reason (the YouTube tile while other windows exist, decision S1).
    Refused(&'static str),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Tile {
    /// The library entry's id, or `musik:<kind>:<name>`, `web:<address>`, `bigpicture`.
    pub key: String,
    pub name: String,
    /// The muted line under the name ("Proton · 19:40").
    pub meta: String,
    /// Index into the kontext colours (kontext 1-3, accent); None: panel.
    pub colour: Option<usize>,
    pub greyed: bool,
    pub act: Act,
    /// Zuletzt gespielt (the newest of Steam's LastPlayed and BitsOS's own record), Unix seconds; 0: never.
    pub last: i64,
    pub size: u64,
    pub steam: Option<u32>,
    pub proton: bool,
    pub loading: bool,
}

/// What the model is made of.
pub struct Input<'a> {
    pub index: &'a Index,
    pub play: &'a Book,
    pub favourites: &'a [String],
    pub music: Option<&'a Music>,
    /// Windows exist (anywhere): the YouTube tile is greyed until Cobalt runs in a window (S1).
    pub windows: bool,
    pub sort: &'a str,
    pub now: i64,
    /// The local broken-down time of a Unix time (status.rs `tm_of`).
    pub tm: &'a dyn Fn(i64) -> libc::tm,
}

/// The kontext colours of a stable hash of `id` (FNV-1a), one of four (§1).
pub fn colour(id: &str) -> usize {
    let mut h: u32 = 0x811c_9dc5;
    for b in id.bytes() {
        h = (h ^ b as u32).wrapping_mul(0x0100_0193);
    }
    (h % 4) as usize
}

/// Days since 1970-01-01 of a broken-down date (the civil calendar).
fn day_number(t: &libc::tm) -> i64 {
    let (y, m, d) = (t.tm_year as i64 + 1900, t.tm_mon as i64 + 1, t.tm_mday as i64);
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * ((m + 9) % 12) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Whole days between the two dates (0: the same day; later times than now count as today).
pub fn days(then: &libc::tm, now: &libc::tm) -> i64 {
    (day_number(now) - day_number(then)).max(0)
}

/// "Heute", "Gestern", "Vor 2 Tagen", "Letzte Woche", "Vor 3 Wochen", else the date (2.1).
pub fn days_text(then: &libc::tm, now: &libc::tm) -> String {
    match days(then, now) {
        0 => "Heute".into(),
        1 => "Gestern".into(),
        n @ 2..=6 => format!("Vor {n} Tagen"),
        7..=13 => "Letzte Woche".into(),
        n @ 14..=55 => format!("Vor {} Wochen", n / 7),
        _ => format!("{:02}.{:02}.{}", then.tm_mday, then.tm_mon + 1, then.tm_year + 1900),
    }
}

/// The library tile's time: today the clock time, else the relative day.
fn when_short(then: &libc::tm, now: &libc::tm) -> String {
    if days(then, now) == 0 { format!("{:02}:{:02}", then.tm_hour, then.tm_min) } else { days_text(then, now) }
}

/// The hero's "heute um 19:40", "gestern um 22:10", "vor 2 Tagen", "am 03.11.2026".
pub fn hero_when(then: &libc::tm, now: &libc::tm) -> String {
    let hm = format!("{:02}:{:02}", then.tm_hour, then.tm_min);
    match days(then, now) {
        0 => format!("heute um {hm}"),
        1 => format!("gestern um {hm}"),
        2..=55 => {
            let t = days_text(then, now);
            let mut c = t.chars();
            c.next().map_or_else(String::new, |f| f.to_lowercase().chain(c).collect())
        }
        _ => format!("am {}", days_text(then, now)),
    }
}

/// "2 Std. 15 Min.", "45 Min.", "unter 1 Min."; None for nothing.
pub fn duration(secs: u64) -> Option<String> {
    let (h, m) = (secs / 3600, secs % 3600 / 60);
    match (h, m) {
        (0, 0) if secs == 0 => None,
        (0, 0) => Some("unter 1 Min.".into()),
        (0, m) => Some(format!("{m} Min.")),
        (h, 0) => Some(format!("{h} Std.")),
        (h, m) => Some(format!("{h} Std. {m} Min.")),
    }
}

/// Bytes as the board writes them: "238 GB", "1,2 GB", "850 MB".
pub fn size_text(b: u64) -> String {
    let gb = b as f64 / 1e9;
    if gb >= 10.0 {
        format!("{} GB", gb.round() as u64)
    } else if gb >= 1.0 {
        format!("{:.1} GB", gb).replace('.', ",")
    } else {
        format!("{} MB", (b as f64 / 1e6).round() as u64)
    }
}

pub(crate) fn is_game(e: &Entry) -> bool {
    e.id != "steam" && (matches!(e.source, Source::Steam(_)) || e.categories.iter().any(|c| c.eq_ignore_ascii_case("game")))
}

/// The tile of a game entry.
fn game_tile(e: &Entry, i: &Input) -> Tile {
    let (steam, proton, loading, steam_last, size) = match &e.source {
        Source::Steam(s) => (Some(s.appid), s.proton, s.loading, s.last_played as i64, s.size),
        _ => (None, false, false, 0, 0),
    };
    let own = i.play.recs.get(&e.id).map_or(0, |r| r.last);
    let last = steam_last.max(own);
    let kind = if proton { "Proton" } else { "Linux" };
    let when = if loading {
        "wird geladen".to_string()
    } else if last > 0 {
        when_short(&(i.tm)(last), &(i.tm)(i.now))
    } else {
        "Noch nie gespielt".to_string()
    };
    Tile {
        key: e.id.clone(),
        name: e.name.clone(),
        meta: format!("{kind} · {when}"),
        colour: steam.map(|_| colour(&e.id)),
        // A game Steam still loads does not start (In Steam öffnen still shows its download).
        greyed: loading,
        act: if loading { Act::Refused("Das Spiel wird noch geladen.") } else { Act::Start { id: e.id.clone(), extra: Vec::new(), restore: true } },
        last,
        size,
        steam,
        proton,
        loading,
    }
}

fn plain(key: &str, name: &str, meta: &str, act: Act) -> Tile {
    Tile { key: key.into(), name: name.into(), meta: meta.into(), colour: None, greyed: false, act, last: 0, size: 0, steam: None, proton: false, loading: false }
}

/// The Steam launcher's argv with `url` (the stick entry's, else `steam`).
pub fn steam_argv(ix: &Index, url: &str) -> Vec<String> {
    let mut v = ix.entries.iter().find(|e| e.id == "steam" && !e.exec.is_empty()).map_or_else(|| vec!["steam".to_string()], |e| e.argv());
    v.push(url.to_string());
    v
}

/// The sort orders of `sofa.sort`: zuletzt (newest first), az, groesse (largest first); ties A-Z.
pub fn sort(tiles: &mut [Tile], how: &str) {
    tiles.sort_by(|a, b| {
        let first = match how {
            "az" => std::cmp::Ordering::Equal,
            "groesse" => b.size.cmp(&a.size),
            _ => b.last.cmp(&a.last),
        };
        first.then_with(|| order(&a.name, &b.name)).then_with(|| a.key.cmp(&b.key))
    });
}

/// The tiles of collection `c`, sorted by `sofa.sort`.
pub fn tiles(c: Coll, i: &Input) -> Vec<Tile> {
    let ix = i.index;
    let mut v: Vec<Tile> = match c {
        Coll::Spiele => ix.entries.iter().filter(|e| is_game(e)).map(|e| game_tile(e, i)).collect(),
        Coll::Windows => ix.entries.iter().filter(|e| matches!(&e.source, Source::Steam(s) if s.proton)).map(|e| game_tile(e, i)).collect(),
        Coll::YouTube => vec![youtube(i)],
        Coll::Brave => brave(i),
        Coll::Musik => music(i.music),
        Coll::BigPicture => vec![plain("bigpicture", "Steam Big Picture", "Steam auf dem Fernseher", Act::Argv(steam_argv(ix, steamlib::BIG_PICTURE_URL)))],
    };
    if matches!(c, Coll::Spiele | Coll::Windows) {
        sort(&mut v, i.sort);
    }
    v
}

/// YouTube (Cobalt): greyed "kommt bald" while other windows exist (S1: today's Cobalt ends the desktop).
pub fn youtube(i: &Input) -> Tile {
    let there = i.index.entries.iter().any(|e| e.id == "cobalt");
    let (meta, act) = if !there {
        ("nicht installiert", Act::Refused("YouTube (Cobalt) ist nicht installiert."))
    } else if i.windows {
        ("kommt bald", Act::Refused("YouTube startet erst im Fenster richtig; bis dahin nur, wenn sonst nichts offen ist."))
    } else {
        ("Cobalt", Act::Start { id: "cobalt".into(), extra: Vec::new(), restore: true })
    };
    Tile { greyed: !matches!(act, Act::Start { .. }), ..plain("cobalt", "YouTube", meta, act) }
}

/// Brave fullscreen and the favourites that are web addresses (3.2).
fn brave(i: &Input) -> Vec<Tile> {
    let full = || vec!["--".to_string(), "--start-fullscreen".to_string()];
    if !i.index.entries.iter().any(|e| e.id == "brave") {
        return vec![Tile { greyed: true, ..plain("brave", "Brave", "nicht installiert", Act::Refused("Brave ist nicht installiert.")) }];
    }
    let mut v = vec![plain("brave", "Brave", "Browser", Act::Start { id: "brave".into(), extra: full(), restore: true })];
    for f in i.favourites {
        if !matches!(crate::address::resolve(f), crate::address::Open::Web(_)) {
            continue;
        }
        let url = if f.starts_with("http://") || f.starts_with("https://") { f.clone() } else { format!("https://{f}") };
        let name = f.trim_start_matches("https://").trim_start_matches("http://").trim_start_matches("www.").trim_end_matches('/');
        let mut extra = vec![url];
        extra.extend(full());
        v.push(plain(&format!("web:{f}"), name, "Webseite", Act::Start { id: "brave".into(), extra, restore: false }));
    }
    v
}

/// GM-6d: a program match of the search (2.6): its tile and whether it is a game (the card's word SPIEL).
#[derive(Clone, Debug, PartialEq)]
pub struct Hit {
    pub tile: Tile,
    pub game: bool,
}

/// The program matches of `text`, best first (library.rs's ranked search). Each starts as its tile does in the
/// sofa mode: a game by the launcher rule, Brave fullscreen, YouTube only as decision S1 allows.
pub fn search_hits(i: &Input, text: &str) -> Vec<Hit> {
    if text.trim().is_empty() {
        return Vec::new();
    }
    let full = || vec!["--".to_string(), "--start-fullscreen".to_string()];
    i.index
        .matches(text)
        .into_iter()
        .map(|e| {
            let game = is_game(e);
            let tile = match e.id.as_str() {
                _ if game => game_tile(e, i),
                "cobalt" => youtube(i),
                "brave" => plain("brave", &e.name, "Browser", Act::Start { id: "brave".into(), extra: full(), restore: true }),
                id => plain(id, &e.name, &e.generic, Act::Start { id: id.into(), extra: Vec::new(), restore: true }),
            };
            Hit { tile, game }
        })
        .collect()
}

/// GM-6d: the search's web hit: Brave fullscreen on Brave's search for `text` (3.2), refused without Brave.
pub fn brave_search(i: &Input, text: &str) -> Tile {
    if !i.index.entries.iter().any(|e| e.id == "brave") {
        return Tile { greyed: true, ..plain("brave", "Im Netz suchen", "Brave ist nicht installiert", Act::Refused("Brave ist nicht installiert.")) };
    }
    let extra = vec![crate::sofa_search::brave_url(text), "--".to_string(), "--start-fullscreen".to_string()];
    plain("web:search", "Im Netz suchen", "Brave", Act::Start { id: "brave".into(), extra, restore: false })
}

fn music(m: Option<&Music>) -> Vec<Tile> {
    let Some(m) = m else { return Vec::new() };
    let one = |kind: &'static str, (name, n): &(String, usize)| {
        plain(&format!("musik:{kind}:{name}"), name, &format!("{n} Titel"), Act::Music { kind, name: name.clone() })
    };
    m.lists.iter().map(|l| one("list", l)).chain(m.albums.iter().map(|a| one("album", a))).collect()
}

/// The left column's count of `c` (2.2): Spiele, Windows-Spiele, Musik (tracks); none for the others.
pub fn count(c: Coll, i: &Input) -> Option<usize> {
    match c {
        Coll::Spiele | Coll::Windows => Some(tiles(c, i).len()),
        Coll::Musik => i.music.map(|m| m.tracks),
        _ => None,
    }
}

/// The line beside the title: "12 installiert · 238 GB auf /data", "412 Titel · 3 Listen · 5 Alben".
pub fn info(c: Coll, t: &[Tile], i: &Input) -> String {
    match c {
        Coll::Spiele | Coll::Windows => {
            let size: u64 = t.iter().map(|t| t.size).sum();
            if size > 0 { format!("{} installiert · {} auf /data", t.len(), size_text(size)) } else { format!("{} installiert", t.len()) }
        }
        Coll::Musik => match i.music {
            Some(m) => format!("{} Titel · {} Listen · {} Alben", m.tracks, m.lists.len(), m.albums.len()),
            None => "Musik: noch nicht da".into(),
        },
        _ => String::new(),
    }
}

/// The selection bar's facts (2.2): "über Proton · 1,2 GB", "Linux · 850 MB", the tile's line otherwise.
pub fn facts(t: &Tile) -> String {
    if t.steam.is_none() {
        return t.meta.clone();
    }
    let kind = if t.proton { "über Proton" } else { "Linux" };
    if t.size > 0 { format!("{kind} · {}", size_text(t.size)) } else { kind.into() }
}

/// The start screen's hero (2.1).
#[derive(Clone, Debug, PartialEq)]
pub struct Hero {
    /// None: no game was ever started (the empty state: "Bibliothek" with the count of programs).
    pub key: Option<String>,
    pub label: String,
    pub name: String,
    pub line: String,
    pub colour: usize,
}

pub fn hero(i: &Input) -> Hero {
    let games = tiles(Coll::Spiele, i);
    let best = games.iter().filter(|t| t.last > 0).max_by(|a, b| a.last.cmp(&b.last).then_with(|| order(&b.name, &a.name)));
    let Some(t) = best else {
        let n = i.index.entries.len();
        return Hero {
            key: None,
            label: "BIBLIOTHEK".into(),
            name: "Bibliothek".into(),
            line: format!("{n} {} · {} {}", if n == 1 { "Programm" } else { "Programme" }, games.len(), if games.len() == 1 { "Spiel" } else { "Spiele" }),
            colour: 3,
        };
    };
    let kind = match (t.steam, t.proton) {
        (Some(_), true) => "WINDOWS-SPIEL ÜBER PROTON",
        (Some(_), false) => "LINUX-SPIEL",
        _ => "SPIEL",
    };
    let mut line = format!("Zuletzt gespielt {}", hero_when(&(i.tm)(t.last), &(i.tm)(i.now)));
    if let Some(d) = i.play.recs.get(&t.key).and_then(|r| duration(r.total)) {
        line.push_str(&format!(" · {d}"));
    }
    Hero { key: Some(t.key.clone()), label: format!("WEITERSPIELEN · {kind}"), name: t.name.clone(), line, colour: colour(&t.key) }
}

/// Zuletzt gespielt (2.1): up to `max` games newest first, the hero's left out; with none ever played the first
/// installed games.
pub fn recent(i: &Input, hero: &Hero, max: usize) -> Vec<Tile> {
    let mut games = tiles(Coll::Spiele, i);
    let picked: Vec<Tile> = if hero.key.is_none() {
        sort(&mut games, "az");
        games.into_iter().take(max).collect()
    } else {
        sort(&mut games, "zuletzt");
        games.into_iter().filter(|t| t.last > 0 && Some(&t.key) != hero.key.as_ref()).take(max).collect()
    };
    // The start row's line is the day alone (board Main: "Gestern", "Vor 2 Tagen").
    let day = |t: &Tile| if t.last > 0 { days_text(&(i.tm)(t.last), &(i.tm)(i.now)) } else { "Noch nie gespielt".into() };
    picked.into_iter().map(|t| Tile { meta: day(&t), ..t }).collect()
}

#[cfg(test)]
#[path = "sofa_model_tests.rs"]
pub(crate) mod tests;
