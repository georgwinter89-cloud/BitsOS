//! GM-6s: Steam's installed games (sofa-mode.md 3.3) for the sofa mode and the program library. Reads only
//! `steamapps/libraryfolders.vdf` of Steam's root and `<library>/steamapps/appmanifest_<appid>.acf` of each library, each
//! file at most [`MAX_FILE`] bytes, at most [`MAX_MANIFESTS`] manifests; never Steam's config, login or user files. Every
//! open goes through [`Fs`], so a test can see what was opened. Errors are [`Problem`]s for the caller's events (B6).
//! No inotify here: [`watch_dirs`] names the folders, the caller watches them off its main loop (B4).

pub mod kv;
mod read;

use std::path::{Path, PathBuf};

pub use read::{Fs, RealFs};

/// Steam's root on BitsOS (the GM-2a launcher's STEAMROOT).
pub const STEAM_ROOT: &str = "/data/home/1000/.local/share/Steam";
pub const MAX_FILE: u64 = 1 << 20;
pub const MAX_MANIFESTS: usize = 4096;
/// Steam's runtime tools that are no games (sofa-mode.md 3.3); names starting with [`TOOL_PREFIXES`] go too.
pub const TOOL_IDS: [u32; 5] = [1070560, 1391110, 1628350, 1493710, 228980];
pub const TOOL_PREFIXES: [&str; 4] = ["Proton", "Steam Linux Runtime", "Legacy Steam Runtime", "Steamworks Common Redistributables"];
pub const BIG_PICTURE_URL: &str = "steam://open/bigpicture";

/// What a tile runs for a game (sofa-mode.md 3.2): `steam <this>`.
pub fn run_url(appid: u32) -> String {
    format!("steam://rungameid/{appid}")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// StateFlags has 4 (fully installed) and nothing of an update or install going on.
    Installed,
    /// Being downloaded, updated or checked: "wird geladen".
    Loading,
    /// Anything else (uninstalled, uninstalling, files missing): the views leave it out.
    Other(u32),
}

impl State {
    pub fn from_flags(f: u32) -> State {
        // EAppState: update running/paused/started, validating, adding files, preallocating, downloading, staging, committing.
        const BUSY: u32 = 0x100 | 0x200 | 0x400 | 0x2_0000 | 0x4_0000 | 0x8_0000 | 0x10_0000 | 0x20_0000 | 0x40_0000;
        const GOING: u32 = 0x800; // uninstalling
        if f & GOING != 0 {
            State::Other(f)
        } else if f & BUSY != 0 {
            State::Loading
        } else if f & 4 != 0 {
            State::Installed
        } else {
            State::Other(f)
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Game {
    pub appid: u32,
    pub name: String,
    pub installdir: String,
    /// Bytes.
    pub size_on_disk: u64,
    /// Unix time, 0 when never played.
    pub last_played: u64,
    /// A Proton game: UserConfig or MountedConfig `platform_override_source` is `windows`, or Steam made a Proton
    /// prefix for it (`scan_with`).
    pub proton: bool,
    pub state: State,
    /// The library folder its manifest is in.
    pub library: PathBuf,
}

impl Game {
    pub fn is_tool(&self) -> bool {
        is_tool(self.appid, &self.name)
    }
}

pub fn is_tool(appid: u32, name: &str) -> bool {
    TOOL_IDS.contains(&appid) || TOOL_PREFIXES.iter().any(|p| name.starts_with(p))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Problem {
    pub path: PathBuf,
    pub what: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Scan {
    pub folders: Vec<PathBuf>,
    /// Games only (tools left out), in folder order and then by appid; an appid seen twice counts once (the first).
    pub games: Vec<Game>,
    pub tools: usize,
    pub problems: Vec<Problem>,
}

fn steamapps(lib: &Path) -> PathBuf {
    lib.join("steamapps")
}

/// Steam's root first (it always is a library), then each `path` of libraryfolders.vdf: the numbered blocks of today's
/// format and the plain `"1" "path"` strings of the old one. A missing or broken file leaves the root alone.
pub fn library_folders_with(fs: &dyn Fs, root: &Path, problems: &mut Vec<Problem>) -> Vec<PathBuf> {
    let mut out = vec![root.to_path_buf()];
    let file = steamapps(root).join("libraryfolders.vdf");
    let text = match fs.read(&file, MAX_FILE) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return out,
        Err(e) => {
            problems.push(Problem { path: file, what: e.to_string() });
            return out;
        }
    };
    let pairs = match kv::parse(&text) {
        Ok(p) => p,
        Err(e) => {
            problems.push(Problem { path: file, what: e.to_string() });
            return out;
        }
    };
    let Some(top) = kv::get(&pairs, "libraryfolders").and_then(kv::Value::as_block) else {
        problems.push(Problem { path: file, what: "no libraryfolders block".into() });
        return out;
    };
    for (k, v) in top {
        if k.is_empty() || !k.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        let p = match v {
            kv::Value::Str(s) => Some(s.as_str()),
            kv::Value::Block(b) => kv::get(b, "path").and_then(kv::Value::as_str),
        };
        let Some(p) = p else { continue };
        let p = PathBuf::from(p);
        if !p.is_absolute() {
            problems.push(Problem { path: file.clone(), what: format!("library {k}: not an absolute path: {}", p.display()) });
        } else if !out.contains(&p) {
            out.push(p);
        }
    }
    out
}

pub fn library_folders(root: &Path) -> Vec<PathBuf> {
    library_folders_with(&RealFs, root, &mut Vec::new())
}

/// The folders to watch with inotify: each library's steamapps/ (the root's holds libraryfolders.vdf too).
pub fn watch_dirs(root: &Path) -> Vec<PathBuf> {
    library_folders(root).iter().map(|l| steamapps(l)).collect()
}

/// `appmanifest_<digits>.acf` -> the digits as an appid.
fn manifest_id(name: &str) -> Option<u32> {
    let id = name.strip_prefix("appmanifest_")?.strip_suffix(".acf")?;
    if id.is_empty() || !id.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    id.parse().ok()
}

fn clean(s: &str) -> String {
    s.chars().map(|c| if c.is_control() { ' ' } else { c }).collect::<String>().trim().to_string()
}

/// One manifest's game. The appid of the file name counts when the file has none.
pub fn parse_manifest(text: &[u8], file_id: u32, library: &Path) -> Result<Game, String> {
    let pairs = kv::parse(text).map_err(|e| e.to_string())?;
    let st = kv::get(&pairs, "AppState").and_then(kv::Value::as_block).ok_or("no AppState block")?;
    let str_of = |k: &str| kv::get(st, k).and_then(kv::Value::as_str).map(str::trim);
    let num = |k: &str| str_of(k).and_then(|v| v.parse::<u64>().ok()).unwrap_or(0);
    let appid = match str_of("appid") {
        Some(v) => v.parse::<u32>().map_err(|_| format!("appid is no number: {v}"))?,
        None => file_id,
    };
    if appid == 0 {
        return Err("appid 0".into());
    }
    let name = clean(str_of("name").unwrap_or(""));
    let name = if name.is_empty() { format!("App {appid}") } else { name };
    let flags = str_of("StateFlags").and_then(|v| v.parse::<u32>().ok()).unwrap_or(0);
    let proton = ["UserConfig", "MountedConfig"].iter().any(|c| {
        kv::get(st, c)
            .and_then(kv::Value::as_block)
            .and_then(|b| kv::get(b, "platform_override_source"))
            .and_then(kv::Value::as_str)
            .is_some_and(|v| v.trim().eq_ignore_ascii_case("windows"))
    });
    Ok(Game {
        appid,
        name,
        installdir: clean(str_of("installdir").unwrap_or("")),
        size_on_disk: num("SizeOnDisk"),
        last_played: num("LastPlayed"),
        proton,
        state: State::from_flags(flags),
        library: library.to_path_buf(),
    })
}

pub fn scan_with(fs: &dyn Fs, root: &Path) -> Scan {
    let mut scan = Scan::default();
    scan.folders = library_folders_with(fs, root, &mut scan.problems);
    let mut budget = MAX_MANIFESTS;
    for lib in &scan.folders {
        let dir = steamapps(lib);
        // One more than the budget: so it shows that there were more.
        let names = match fs.list(&dir, &|n| manifest_id(n).is_some(), budget + 1) {
            Ok(n) => n,
            Err(e) => {
                if e.kind() != std::io::ErrorKind::NotFound || lib == root {
                    scan.problems.push(Problem { path: dir, what: e.to_string() });
                }
                continue;
            }
        };
        let mut ids: Vec<(u32, String)> = names.into_iter().filter_map(|n| manifest_id(&n).map(|id| (id, n))).collect();
        ids.sort();
        let over = ids.len() > budget;
        ids.truncate(budget);
        for (id, n) in ids {
            budget -= 1;
            let path = dir.join(&n);
            let game = fs.read(&path, MAX_FILE).map_err(|e| e.to_string()).and_then(|t| parse_manifest(&t, id, lib)).map(|mut g| {
                // Steam made a Proton prefix for it (`compatdata/<appid>/pfx`): it ran as a Windows game. On the Mini-PC
                // (11.11.2026) the manifests of games Steam starts through Proton by itself carry no platform_override,
                // so every game was "Linux" and the Windows collection was empty. Only the folder's name is looked at.
                if !g.proton && !g.is_tool() {
                    g.proton = fs.list(&dir.join("compatdata").join(g.appid.to_string()), &|n| n == "pfx", 1).is_ok_and(|v| !v.is_empty());
                }
                g
            });
            match game {
                Err(what) => scan.problems.push(Problem { path, what }),
                Ok(g) if g.is_tool() => scan.tools += 1,
                Ok(g) if scan.games.iter().any(|o| o.appid == g.appid) => {}
                Ok(g) => scan.games.push(g),
            }
        }
        if over {
            scan.problems.push(Problem { path: dir, what: format!("more than {MAX_MANIFESTS} manifests: the rest left out") });
            break;
        }
    }
    scan
}

pub fn scan(root: &Path) -> Scan {
    scan_with(&RealFs, root)
}

/// Steam's games of `root` (tools left out); problems dropped. [`scan`] keeps them.
pub fn games(root: &Path) -> Vec<Game> {
    scan(root).games
}
