//! GM-6b: the library index's fourth source, Steam's installed games (plan/spec/sofa-mode.md 3.1, 3.3), read by
//! steamlib (GM-6s: only libraryfolders.vdf and the appmanifests, never a login file) on the library's scan thread
//! (B4). Each game is an entry `steam-<appid>` (address `programm/steam-<appid>`, Categories Game) that starts
//! `steam steam://rungameid/<appid>` through the Steam launcher; tools are left out by steamlib, games that are
//! neither installed nor loading are left out here. Its windows are known by the X11 class `steam_app_<appid>`
//! (Proton games); Steam's own windows are never a game's (library.rs `Entry::owns`).

use std::path::{Path, PathBuf};

use crate::library::{Entry, Icon, Scan, Source, letter};

/// What the sofa mode shows of a game besides its name (sofa-mode.md 2.2, 3.3, 3.4).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Steam {
    pub appid: u32,
    /// A Proton override (`platform_override_source` windows).
    pub proton: bool,
    /// Steam's `LastPlayed`, Unix seconds (0: never).
    pub last_played: u64,
    /// Steam's `SizeOnDisk`, bytes.
    pub size: u64,
    /// Being downloaded or updated: "wird geladen".
    pub loading: bool,
}

/// How far up from a missing Steam folder a watch may go (the folder's parent sees it come).
const UP: usize = 3;

/// The folders to watch for `dir`: itself, else its nearest existing parent (at most `UP` up), so a Steam installed
/// or a library added later is seen at once.
fn watched(dir: &Path) -> Option<PathBuf> {
    dir.ancestors().take(UP + 1).find(|d| d.is_dir()).map(Path::to_path_buf)
}

/// The games of Steam's `root` as entries; `launcher` is the argv that starts Steam (the URL comes after it).
/// Problems of Steam's files become notes; the folders to watch go into `s.dirs`.
pub fn entries(root: &Path, launcher: &[String], s: &mut Scan) -> Vec<Entry> {
    for d in steamlib::watch_dirs(root) {
        if let Some(w) = watched(&d)
            && !s.dirs.contains(&w)
        {
            s.dirs.push(w);
        }
    }
    let scan = steamlib::scan(root);
    for p in &scan.problems {
        // A Steam that is not installed is no problem worth a line each scan.
        if p.what.contains("No such file") {
            continue;
        }
        s.notes.push(format!("steam: {}: {}", p.path.display(), p.what));
    }
    let mut out = Vec::new();
    for g in scan.games {
        let loading = match g.state {
            steamlib::State::Installed => false,
            steamlib::State::Loading => true,
            steamlib::State::Other(f) => {
                s.notes.push(format!("steam-{} ({}) left out: not installed (StateFlags {f})", g.appid, g.name));
                continue;
            }
        };
        let id = format!("steam-{}", g.appid);
        let mut exec = launcher.to_vec();
        exec.push(steamlib::run_url(g.appid));
        out.push(Entry {
            address: format!("programm/{id}"),
            id,
            generic: if g.proton { "Windows-Spiel".into() } else { "Spiel".into() },
            comment: String::new(),
            keywords: vec!["steam".into()],
            categories: vec!["Game".into()],
            exec,
            terminal: false,
            icon: Icon::Fallback(letter(&g.name)),
            icon_note: "Steam game: no cover pictures in v1".into(),
            source: Source::Steam(Steam { appid: g.appid, proton: g.proton, last_played: g.last_played, size: g.size_on_disk, loading }),
            wm_class: format!("steam_app_{}", g.appid),
            name: g.name,
        });
    }
    out
}
