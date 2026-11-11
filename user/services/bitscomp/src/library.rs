//! PK-L1a: the program library's index (plan/notes/desk-PK67-plan.md §3): every program BitsOS can start, from
//! three sources in this order of precedence for one id: packages (`/apps/<prog>/current`, read through the
//! program's own root tree, D8), stick launchers (`/boot/BITSOS2/bin/*.desktop`, an entry only while its launcher
//! is there and executable) and the built-in ones (Terminal, Einstellungen). It knows nothing of the views that show
//! it (the library overlay, PK-L1b; the sofa mode, GM-6: D9). Scans and icon decoding run on a thread of their own
//! (B4); inotify on the folders asks for a new scan, and the main loop swaps the new index in one step.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{Arc, mpsc};
use std::time::{Duration, SystemTime};

use smithay::reexports::calloop::channel::{self, Sender};
use smithay::reexports::calloop::generic::Generic;
use smithay::reexports::calloop::timer::{TimeoutAction, Timer};
use smithay::reexports::calloop::{Interest, LoopHandle, Mode, PostAction};

use crate::desktop_entry;
use crate::icon_png::{self, Argb};

/// The tile's icon side at 2560x1440 (board 96 px x 4/3, desktop.md §0 point 8).
pub const TILE: u32 = 128;
pub const STICK_BIN: &str = "/boot/BITSOS2/bin";
pub const APPS: &str = "/apps";
/// The scan's period where a folder cannot be watched (no inotify, or `/apps` not there yet).
pub const RESCAN: Duration = Duration::from_secs(5);
const MASK: u32 = libc::IN_CREATE | libc::IN_ATTRIB | libc::IN_DELETE | libc::IN_MOVED_TO | libc::IN_MOVED_FROM
    | libc::IN_CLOSE_WRITE | libc::IN_DELETE_SELF | libc::IN_ONLYDIR;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Source {
    Builtin,
    Stick,
    Package(String),
}

impl Source {
    pub fn word(&self) -> String {
        match self {
            Source::Builtin => "builtin".into(),
            Source::Stick => "stick".into(),
            Source::Package(p) => format!("package:{p}"),
        }
    }
}

#[derive(Clone, Debug)]
pub enum Icon {
    /// The name's first letter on an accent square (drawn by PK-L1b's look code).
    Fallback(char),
    /// A PNG found and not decoded yet: the fallback is shown meanwhile.
    Pending(PathBuf),
    Image(PathBuf, Arc<Argb>),
}

impl PartialEq for Icon {
    fn eq(&self, o: &Icon) -> bool {
        match (self, o) {
            (Icon::Fallback(a), Icon::Fallback(b)) => a == b,
            (Icon::Pending(a), Icon::Pending(b)) => a == b,
            (Icon::Image(a, x), Icon::Image(b, y)) => a == b && Arc::ptr_eq(x, y),
            _ => false,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    pub id: String,
    pub name: String,
    pub generic: String,
    pub comment: String,
    pub keywords: Vec<String>,
    pub categories: Vec<String>,
    /// What spawn starts; empty for bitscomp's own pages (Einstellungen: opened by its address).
    pub exec: Vec<String>,
    pub terminal: bool,
    pub icon: Icon,
    /// Why the fallback icon is shown (empty otherwise).
    pub icon_note: String,
    pub source: Source,
    /// `StartupWMClass`: the app_id of its windows where the address does not say it.
    pub wm_class: String,
    /// `programm/<id>` (Einstellungen: `einstellungen`), the one name favourites, recent list and address bar use.
    pub address: String,
}

impl Entry {
    /// The name of the program the Exec line really starts: `env NAME=value prog` counts as `prog`,
    /// `/bin/sh LAUNCHER` as the launcher, `flatpak run APP` as the app id. Empty where that name says nothing
    /// about the program (an interpreter or wrapper such as java, python3, `sh -c`), so the launcher rule never
    /// takes an unrelated window of that name for this program's.
    pub fn program_name(&self) -> &str {
        fn base(s: &str) -> &str {
            s.rsplit('/').next().unwrap_or(s)
        }
        let mut words = self.exec.iter().map(String::as_str);
        let mut after_env = false;
        while let Some(w) = words.next() {
            let name = base(w);
            if name == "env" {
                after_env = true;
                continue;
            }
            if after_env && (w.starts_with('-') || w.split_once('=').is_some_and(|(k, _)| !k.is_empty())) {
                // `-u NAME` and `-C DIR` take the next word along.
                if matches!(w, "-u" | "--unset" | "-C" | "--chdir") {
                    words.next();
                }
                continue;
            }
            return match name {
                "sh" | "bash" | "dash" => match words.next() {
                    Some(s) if !s.starts_with('-') => specific(base(s)),
                    _ => "",
                },
                "flatpak" => words.skip_while(|s| *s == "run" || s.starts_with('-')).next().map_or("", specific),
                _ => specific(name),
            };
        }
        ""
    }

    /// What spawn starts: `exec`, inside the terminal when the file says `Terminal=true` (foot runs a command
    /// given after its options). Its window carries the entry's id as app_id, so the launcher rule tells it
    /// apart from every other terminal window.
    pub fn argv(&self) -> Vec<String> {
        if !self.terminal || self.exec.is_empty() {
            return self.exec.clone();
        }
        let foot = crate::terminal::TERMINAL.iter().map(|s| s.to_string());
        foot.chain([format!("--app-id={}", self.id)]).chain(self.exec.iter().cloned()).collect()
    }

    /// `ctl library`'s icon column.
    pub fn icon_state(&self) -> String {
        match &self.icon {
            Icon::Fallback(c) if self.icon_note.is_empty() => format!("fallback:{c}"),
            Icon::Fallback(c) => format!("fallback:{c} ({})", self.icon_note),
            Icon::Pending(p) => format!("pending:{}", p.display()),
            Icon::Image(p, a) => format!("png:{}x{}:{}", a.w, a.h, p.display()),
        }
    }

    /// Whether a window with `app_id` is this program's (the launcher rule, beside minimize.rs's `launches`): by
    /// its address, its `StartupWMClass` or (PK-L1b) the name of the program its Exec starts.
    pub fn owns(&self, app_id: &str) -> bool {
        let id = app_id.trim();
        let same = |s: &str| !s.is_empty() && s.eq_ignore_ascii_case(id);
        crate::minimize::launches(&self.address, id) || same(&self.wm_class) || same(self.program_name())
    }
}

/// `name` unless it is an interpreter or wrapper whose windows belong to many programs.
fn specific(name: &str) -> &str {
    const GENERIC: &[&str] = &["env", "sh", "bash", "dash", "java", "node", "perl", "ruby", "mono", "dotnet", "flatpak",
        "foot", "footclient", "xdg-open", "gjs", "electron"];
    let generic = GENERIC.contains(&name) || ["python", "wine", "electron"].iter().any(|p| name.starts_with(p));
    if generic { "" } else { name }
}

/// The name's first letter, upper case the German way (`ä` -> `Ä`; `ß` stays: it has no one-letter capital).
pub fn letter(name: &str) -> char {
    let c = name.trim().chars().next().unwrap_or('?');
    let mut up = c.to_uppercase();
    match (up.next(), up.next()) {
        (Some(u), None) => u,
        _ => c,
    }
}

/// The German A-Z key (DIN 5007-1): case-blind, Ä with A, Ö with O, Ü with U, ß as ss, other accents dropped.
pub fn fold(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars().flat_map(char::to_lowercase) {
        match c {
            'ä' | 'à' | 'á' | 'â' | 'ã' | 'å' => out.push('a'),
            'ö' | 'ò' | 'ó' | 'ô' | 'õ' | 'ø' => out.push('o'),
            'ü' | 'ù' | 'ú' | 'û' => out.push('u'),
            'é' | 'è' | 'ê' | 'ë' => out.push('e'),
            'í' | 'ì' | 'î' | 'ï' => out.push('i'),
            'ç' => out.push('c'),
            'ñ' => out.push('n'),
            'ß' => out.push_str("ss"),
            o => out.push(o),
        }
    }
    out
}

/// The A-Z order of two names (ties by the plain text, so the order is total).
pub fn order(a: &str, b: &str) -> std::cmp::Ordering {
    fold(a).cmp(&fold(b)).then_with(|| a.cmp(b))
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Index {
    /// A-Z by name (German collation).
    pub entries: Vec<Entry>,
}

impl Index {
    /// The built-in programs alone (before the first scan, and the scan's third source).
    pub fn builtin() -> Index {
        let mut ix = Index { entries: builtin() };
        ix.entries.sort_by(|a, b| order(&a.name, &b.name).then_with(|| a.id.cmp(&b.id)));
        ix
    }

    pub fn by_address(&self, addr: &str) -> Option<&Entry> {
        let a = crate::address::normalise(addr);
        let word = a.strip_prefix("programm/").unwrap_or(&a);
        self.entries.iter().find(|e| e.address == a || e.id == word)
    }

    /// The entries `query` finds, best first: a word of the name starting with it, the name containing it, then
    /// the same in GenericName, a keyword, the program's file name; ties keep the A-Z order. Empty: all, A-Z.
    pub fn matches(&self, query: &str) -> Vec<&Entry> {
        let q = fold(query.trim());
        if q.is_empty() {
            return self.entries.iter().collect();
        }
        let word_start = |hay: &str| {
            let h = fold(hay);
            h.starts_with(&q) || h.match_indices(&q).any(|(i, _)| !h[..i].chars().last().is_some_and(char::is_alphanumeric))
        };
        let inside = |hay: &str| fold(hay).contains(&q);
        let rank = |e: &Entry| -> Option<u8> {
            if word_start(&e.name) {
                Some(0)
            } else if inside(&e.name) {
                Some(1)
            } else if word_start(&e.generic) {
                Some(2)
            } else if inside(&e.generic) {
                Some(3)
            } else if e.keywords.iter().any(|k| inside(k)) {
                Some(4)
            } else if inside(e.program_name()) {
                Some(5)
            } else {
                None
            }
        };
        let mut hits: Vec<(u8, usize, &Entry)> = self.entries.iter().enumerate().filter_map(|(i, e)| rank(e).map(|r| (r, i, e))).collect();
        hits.sort_by_key(|(r, i, _)| (*r, *i));
        hits.into_iter().map(|(_, _, e)| e).collect()
    }

    /// `ctl library` (and `ctl library search Q`): `id\tname\tsource\ticon-state\texec` per line.
    pub fn lines(entries: &[&Entry]) -> String {
        let mut out = String::new();
        for e in entries {
            let clean = |s: &str| s.replace(['\t', '\n'], " ");
            out.push_str(&format!("{}\t{}\t{}\t{}\t{}\n", e.id, clean(&e.name), e.source.word(), clean(&e.icon_state()), clean(&e.exec.join(" "))));
        }
        out
    }
}

fn builtin() -> Vec<Entry> {
    let base = |id: &str, name: &str, generic: &str, keywords: &[&str], exec: Vec<String>, address: &str, wm: &str| Entry {
        id: id.into(),
        name: name.into(),
        generic: generic.into(),
        comment: String::new(),
        keywords: keywords.iter().map(|k| k.to_string()).collect(),
        categories: Vec::new(),
        exec,
        terminal: false,
        icon: Icon::Fallback(letter(name)),
        icon_note: String::new(),
        source: Source::Builtin,
        wm_class: wm.into(),
        address: address.into(),
    };
    vec![
        base("terminal", "Terminal", "Befehlszeile", &["foot", "shell", "konsole"],
            crate::terminal::TERMINAL.iter().map(|s| s.to_string()).collect(), "programm/terminal", "foot"),
        base("einstellungen", "Einstellungen", "Einstellungen", &["settings", "optionen", "aussehen"], Vec::new(), "einstellungen", ""),
    ]
}

/// Where the sources are (tests and the headless scenario point them at fake folders).
#[derive(Clone, Debug)]
pub struct Roots {
    pub bin: PathBuf,
    pub apps: PathBuf,
    /// The PATH `TryExec` is looked up in.
    pub path: String,
}

impl Roots {
    /// The real places, or `BITSCOMP_LIBRARY_BIN` / `BITSCOMP_LIBRARY_APPS` (the scenario's fakes).
    pub fn from_env() -> Roots {
        let var = |k: &str, d: &str| std::env::var_os(k).map(PathBuf::from).unwrap_or_else(|| PathBuf::from(d));
        Roots { bin: var("BITSCOMP_LIBRARY_BIN", STICK_BIN), apps: var("BITSCOMP_LIBRARY_APPS", APPS), path: std::env::var("PATH").unwrap_or_default() }
    }
}

/// One scan's result: the entries (icons still `Pending`), what was refused or left out and why, and the folders
/// to watch.
#[derive(Debug, Default)]
pub struct Scan {
    pub index: Index,
    pub notes: Vec<String>,
    pub dirs: Vec<PathBuf>,
    /// Ids a source's `Hidden=true` file deletes (the spec's meaning): a lower source's entry of that id goes too.
    pub hidden: Vec<(String, Source)>,
}

/// A source's precedence for one id (lower wins).
fn rank(s: &Source) -> u8 {
    match s {
        Source::Package(_) => 0,
        Source::Stick => 1,
        Source::Builtin => 2,
    }
}

/// Reads a file of at most `max` bytes (`fs::metadata` first: a symlink loop or a folder is refused, not read).
fn read_bounded(p: &Path, max: u64) -> Result<Vec<u8>, String> {
    let m = std::fs::metadata(p).map_err(|e| e.to_string())?;
    if !m.is_file() {
        return Err("not a file".into());
    }
    if m.len() > max {
        return Err(format!("larger than {} KiB", max / 1024));
    }
    let mut v = Vec::new();
    std::fs::File::open(p).and_then(|f| f.take(max + 1).read_to_end(&mut v)).map_err(|e| e.to_string())?;
    if v.len() as u64 > max {
        return Err(format!("larger than {} KiB", max / 1024));
    }
    Ok(v)
}

/// The sorted `*.<ext>` files of `dir` (none if it is missing).
fn files_with(dir: &Path, ext: &str) -> Vec<PathBuf> {
    let Ok(rd) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut v: Vec<PathBuf> = rd.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == ext)).collect();
    v.sort();
    v
}

/// Looks for `icon` per the Icon Theme spec, simplified: an absolute path; hicolor's `<n>x<n>/apps/<icon>.png`
/// under each of `themes` (the smallest n >= `TILE`, else the largest); `pixmaps/<icon>.png`. Err: the fallback's
/// reason.
pub fn find_icon(icon: &str, abs_root: Option<&Path>, themes: &[PathBuf], pixmaps: &[PathBuf]) -> Result<PathBuf, String> {
    if icon.is_empty() {
        return Err("no Icon".into());
    }
    if icon.starts_with('/') {
        let p = abs_root.map_or_else(|| PathBuf::from(icon), |r| r.join(icon.trim_start_matches('/')));
        return match p.extension().and_then(|x| x.to_str()) {
            Some("svg") => Err("SVG only".into()),
            Some("xpm") => Err("XPM only (not read)".into()),
            _ => Ok(p),
        };
    }
    // A name with its extension (legacy, common for pixmaps): looked up by the bare name.
    let icon = [".png", ".svg", ".xpm"].iter().find_map(|x| icon.strip_suffix(x)).filter(|b| !b.is_empty()).unwrap_or(icon);
    if icon.contains('/') || icon == "." || icon == ".." {
        return Err(format!("Icon {icon:?} is neither a name nor an absolute path"));
    }
    let mut sizes: Vec<(u32, PathBuf)> = Vec::new();
    let (mut svg, mut xpm) = (false, false);
    for t in themes {
        let hicolor = t.join("hicolor");
        let Ok(rd) = std::fs::read_dir(&hicolor) else { continue };
        for d in rd.flatten() {
            let name = d.file_name().to_string_lossy().into_owned();
            if name == "scalable" {
                svg |= d.path().join("apps").join(format!("{icon}.svg")).exists();
                continue;
            }
            let Some((a, b)) = name.split_once('x') else { continue };
            let (Ok(a), Ok(b)) = (a.parse::<u32>(), b.parse::<u32>()) else { continue };
            let p = d.path().join("apps").join(format!("{icon}.png"));
            if a == b && p.exists() {
                sizes.push((a, p));
            }
        }
    }
    sizes.sort();
    if let Some((_, p)) = sizes.iter().find(|(n, _)| *n >= TILE).or(sizes.last()) {
        return Ok(p.clone());
    }
    for d in pixmaps {
        let p = d.join(format!("{icon}.png"));
        // symlink_metadata: a link loop is found here and refused by the decoder's metadata check, with its reason.
        if p.symlink_metadata().is_ok() {
            return Ok(p);
        }
        svg |= d.join(format!("{icon}.svg")).exists();
        xpm |= d.join(format!("{icon}.xpm")).exists();
    }
    Err(if svg {
        "SVG only".into()
    } else if xpm {
        "XPM only (not read)".into()
    } else {
        format!("no PNG for {icon}")
    })
}

/// Builds the index: packages, stick launchers, built-in; a later source's id that an earlier one has is dropped.
/// `exe` says whether a file is an executable (spawn.rs's `executable`; tests pass their own).
pub fn scan(roots: &Roots, exe: &dyn Fn(&Path) -> bool) -> Scan {
    let mut s = Scan::default();
    let mut all: Vec<Entry> = Vec::new();
    s.dirs.push(roots.apps.clone());
    packages(roots, exe, &mut all, &mut s);
    s.dirs.push(roots.bin.clone());
    stick(roots, exe, &mut all, &mut s);
    all.extend(builtin());
    let mut seen: HashMap<String, Source> = HashMap::new();
    for e in all {
        if let Some((_, by)) = s.hidden.iter().find(|(id, by)| *id == e.id && rank(by) < rank(&e.source)) {
            s.notes.push(format!("{} from {} left out: Hidden=true in {}", e.id, e.source.word(), by.word()));
            continue;
        }
        if let Some(winner) = seen.get(&e.id) {
            s.notes.push(format!("{} from {} wins over {}", e.id, winner.word(), e.source.word()));
            continue;
        }
        seen.insert(e.id.clone(), e.source.clone());
        s.index.entries.push(e);
    }
    s.index.entries.sort_by(|a, b| order(&a.name, &b.name).then_with(|| a.id.cmp(&b.id)));
    s
}

/// An entry of a parsed `.desktop` file, its icon looked up (not decoded yet).
fn entry_of(id: String, d: desktop_entry::DesktopEntry, exec: Vec<String>, source: Source, icon: Result<PathBuf, String>) -> Entry {
    let (icon, note) = match icon {
        Ok(p) => (Icon::Pending(p), String::new()),
        Err(why) => (Icon::Fallback(letter(&d.name)), why),
    };
    Entry {
        address: format!("programm/{id}"),
        id,
        generic: d.generic,
        comment: d.comment,
        keywords: d.keywords,
        categories: d.categories,
        exec,
        terminal: d.terminal,
        icon,
        icon_note: note,
        source,
        wm_class: d.wm_class,
        name: d.name,
    }
}

/// A desktop file id as an address word: lower case, `a-z0-9._-` only.
fn id_of(p: &Path) -> Option<String> {
    let stem = p.file_stem()?.to_string_lossy().to_lowercase();
    (!stem.is_empty() && stem.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))).then_some(stem)
}

/// Reads and parses one `.desktop` file; None (with a note) when it is refused or not shown.
fn read_entry(p: &Path, source: &Source, s: &mut Scan) -> Option<(String, desktop_entry::DesktopEntry)> {
    let notes = &mut s.notes;
    let Some(id) = id_of(p) else {
        notes.push(format!("{}: refused: a file name that is no address word", p.display()));
        return None;
    };
    let d = read_bounded(p, desktop_entry::MAX_BYTES as u64).and_then(|b| desktop_entry::parse(&b, &p.to_string_lossy()));
    if let Ok(d) = &d {
        notes.extend(d.warnings.iter().map(|w| format!("{}: {w}", p.display())));
    }
    match d {
        Err(e) => {
            notes.push(format!("{}: refused: {e}", p.display()));
            None
        }
        Ok(d) => match d.not_shown() {
            Some(why) => {
                notes.push(format!("{}: not shown: {why}", p.display()));
                if d.hidden {
                    s.hidden.push((id, source.clone()));
                }
                None
            }
            None => Some((id, d)),
        },
    }
}

fn stick(roots: &Roots, exe: &dyn Fn(&Path) -> bool, out: &mut Vec<Entry>, s: &mut Scan) {
    for p in files_with(&roots.bin, "desktop") {
        let Some((id, d)) = read_entry(&p, &Source::Stick, s) else { continue };
        // A stick file names its launcher without a path: it is started through sh from the bin folder (LO-1).
        let prog = d.exec[0].clone();
        let launcher = if prog.contains('/') { PathBuf::from(&prog) } else { roots.bin.join(&prog) };
        if !exe(&launcher) {
            s.notes.push(format!("{}: left out: its launcher {} is not there", p.display(), launcher.display()));
            continue;
        }
        if !d.try_exec.is_empty() && !crate::spawn::found(&d.try_exec, &format!("{}:{}", roots.path, roots.bin.display()), exe) {
            s.notes.push(format!("{}: left out: TryExec {} not found", p.display(), d.try_exec));
            continue;
        }
        let mut exec = vec!["/bin/sh".to_string(), launcher.to_string_lossy().into_owned()];
        exec.extend(d.exec[1..].iter().cloned());
        let icon = find_icon(&d.icon, None, &[], std::slice::from_ref(&roots.bin));
        out.push(entry_of(id, d, exec, Source::Stick, icon));
    }
}

/// Packages: each `/apps/<prog>/current`, read through its root tree (`current/root` from format 2 on, else
/// `current`): its `usr/share/applications/*.desktop`, else the manifest's top-level `program`.
fn packages(roots: &Roots, exe: &dyn Fn(&Path) -> bool, out: &mut Vec<Entry>, s: &mut Scan) {
    let Ok(rd) = std::fs::read_dir(&roots.apps) else { return };
    let mut progs: Vec<PathBuf> = rd.flatten().filter(|e| !e.file_name().to_string_lossy().starts_with('.')).map(|e| e.path()).filter(|p| p.is_dir()).collect();
    progs.sort();
    for dir in progs {
        s.dirs.push(dir.clone());
        let pkg = dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let cur = dir.join("current");
        if !cur.is_dir() {
            continue;
        }
        let root = if cur.join("root").is_dir() { cur.join("root") } else { cur.clone() };
        let share = root.join("usr/share");
        let mut any = false;
        for p in files_with(&share.join("applications"), "desktop") {
            let Some((id, d)) = read_entry(&p, &Source::Package(pkg.clone()), s) else { continue };
            let Some(prog) = in_root(&root, &d.exec[0], exe) else {
                s.notes.push(format!("{}: left out: {} is not in the package", p.display(), d.exec[0]));
                continue;
            };
            if !d.try_exec.is_empty() && in_root(&root, &d.try_exec, exe).is_none() {
                s.notes.push(format!("{}: left out: TryExec {} not in the package", p.display(), d.try_exec));
                continue;
            }
            let mut exec = vec![prog.to_string_lossy().into_owned()];
            exec.extend(d.exec[1..].iter().cloned());
            let icon = find_icon(&d.icon, Some(&root), &[share.join("icons")], &[share.join("pixmaps")]);
            out.push(entry_of(id, d, exec, Source::Package(pkg.clone()), icon));
            any = true;
        }
        if !any && let Some(prog) = manifest_program(&cur.join("manifest.toml")) {
            let p = cur.join(&prog);
            if exe(&p) && !prog.starts_with('/') && !prog.split('/').any(|c| c == "..") {
                let d = desktop_entry::DesktopEntry { name: capitalised(&pkg), ..Default::default() };
                out.push(entry_of(pkg.clone(), d, vec![p.to_string_lossy().into_owned()], Source::Package(pkg.clone()), Err("no .desktop file".into())));
            } else {
                s.notes.push(format!("{}: left out: program {prog} is not an executable of the package", cur.display()));
            }
        }
    }
}

/// A program of a `.desktop` file inside the package's root: absolute paths under the root, bare names in its
/// usr/bin or bin (bitsrun, PK-3, will start it inside its own view later).
fn in_root(root: &Path, prog: &str, exe: &dyn Fn(&Path) -> bool) -> Option<PathBuf> {
    if prog.split('/').any(|c| c == "..") {
        return None;
    }
    let cands = if prog.starts_with('/') {
        vec![root.join(prog.trim_start_matches('/'))]
    } else if prog.contains('/') {
        return None;
    } else {
        vec![root.join("usr/bin").join(prog), root.join("bin").join(prog)]
    };
    cands.into_iter().find(|p| exe(p))
}

/// The top-level `program = "..."` of a manifest.toml (PACKAGES.md §2; before the first `[[` header).
fn manifest_program(p: &Path) -> Option<String> {
    let text = read_bounded(p, 1024 * 1024).ok()?;
    let text = String::from_utf8(text).ok()?;
    for l in text.lines() {
        let l = l.trim();
        if l.starts_with("[[") {
            break;
        }
        if let Some((k, v)) = l.split_once('=')
            && k.trim() == "program"
        {
            let v = v.trim();
            return v.strip_prefix('"').and_then(|v| v.strip_suffix('"')).or_else(|| v.strip_prefix('\'').and_then(|v| v.strip_suffix('\''))).map(String::from);
        }
    }
    None
}

fn capitalised(s: &str) -> String {
    let mut c = s.chars();
    c.next().map_or_else(String::new, |f| f.to_uppercase().chain(c).collect())
}

/// Icons decoded so far, by file and its change time and size (a rescan decodes only what changed).
#[derive(Default)]
pub struct IconCache {
    map: HashMap<(PathBuf, Option<SystemTime>, u64), Result<Arc<Argb>, String>>,
}

impl IconCache {
    pub fn load(&mut self, p: &Path) -> Result<Arc<Argb>, String> {
        let m = std::fs::metadata(p).map_err(|e| e.to_string())?;
        let key = (p.to_path_buf(), m.modified().ok(), m.len());
        if let Some(r) = self.map.get(&key) {
            return r.clone();
        }
        let r = read_bounded(p, icon_png::MAX_FILE).and_then(|b| icon_png::decode(&b)).map(|img| Arc::new(icon_png::to_tile(&img, TILE)));
        self.map.retain(|k, _| k.0 != key.0);
        self.map.insert(key, r.clone());
        r
    }

    /// Forgets the icons no entry of `ix` uses any more (a removed program's icon is not kept for the session).
    pub fn keep_used(&mut self, ix: &Index) {
        let used: HashSet<&Path> = ix.entries.iter().filter_map(|e| match &e.icon {
            Icon::Pending(p) | Icon::Image(p, _) => Some(p.as_path()),
            Icon::Fallback(_) => None,
        }).collect();
        self.map.retain(|k, _| used.contains(k.0.as_path()));
    }
}

/// Decodes every `Pending` icon; a failed one becomes the fallback with its reason.
pub fn decode_icons(ix: &mut Index, cache: &mut IconCache, notes: &mut Vec<String>) {
    for e in &mut ix.entries {
        if let Icon::Pending(p) = &e.icon {
            let p = p.clone();
            match cache.load(&p) {
                Ok(img) => e.icon = Icon::Image(p, img),
                Err(why) => {
                    notes.push(format!("{}: icon {}: fallback: {why}", e.id, p.display()));
                    e.icon = Icon::Fallback(letter(&e.name));
                    e.icon_note = why;
                }
            }
        }
    }
}

struct Msg {
    /// None: the index did not change.
    index: Option<Arc<Index>>,
    /// The folders to watch, after every scan: the main loop watches each one it does not watch yet (a folder whose
    /// watch ended, or a root folder that came after the polling began, is watched again).
    dirs: Vec<PathBuf>,
}

/// The worker: a scan per request (requests queued meanwhile count once), the index first with its icons
/// pending, then again with them decoded; only a changed index is sent (the folders always are).
fn worker(roots: Roots, jobs: mpsc::Receiver<()>, tx: Sender<Msg>) {
    if let Err(e) = crate::spawn::background_class() {
        eprintln!("bitscomp: library: the scan thread keeps bitscomp's class (no Background: {e})");
    }
    let mut cache = IconCache::default();
    let mut last: Option<Index> = None;
    let exe = |p: &Path| crate::spawn::executable(p);
    while jobs.recv().is_ok() {
        while jobs.try_recv().is_ok() {}
        let mut s = scan(&roots, &exe);
        let mut decoded = s.index.clone();
        decode_icons(&mut decoded, &mut cache, &mut s.notes);
        cache.keep_used(&decoded);
        if last.as_ref() == Some(&decoded) {
            if tx.send(Msg { index: None, dirs: s.dirs }).is_err() {
                return;
            }
            continue;
        }
        let counts = |src: fn(&Source) -> bool| decoded.entries.iter().filter(|e| src(&e.source)).count();
        eprintln!(
            "bitscomp: library: {} programs (built-in {}, stick {}, packages {}){}",
            decoded.entries.len(),
            counts(|s| *s == Source::Builtin),
            counts(|s| *s == Source::Stick),
            counts(|s| matches!(s, Source::Package(_))),
            if last.is_none() { "" } else { " after a change" }
        );
        for n in &s.notes {
            eprintln!("bitscomp: library: {n}");
        }
        let pending = s.index.entries.iter().any(|e| matches!(e.icon, Icon::Pending(_)));
        if last.is_none() && pending && tx.send(Msg { index: Some(Arc::new(s.index)), dirs: s.dirs.clone() }).is_err() {
            return;
        }
        if tx.send(Msg { index: Some(Arc::new(decoded.clone())), dirs: s.dirs }).is_err() {
            return;
        }
        last = Some(decoded);
    }
}

/// Starts the index: the worker thread, the watches on its folders, and `apply` with every new index on the main
/// loop (the swap). Generic over the loop's data so the host test runs it without a compositor.
pub fn start<D: 'static>(handle: &LoopHandle<'static, D>, roots: Roots, apply: impl Fn(&mut D, Arc<Index>) + 'static) -> Result<(), String> {
    let (tx, rx) = channel::channel::<Msg>();
    let (jtx, jrx) = mpsc::channel::<()>();
    let r = roots.clone();
    std::thread::Builder::new().name("bitscomp-library".into()).spawn(move || worker(r, jrx, tx)).map_err(|e| format!("library thread: {e}"))?;
    let watched: Rc<RefCell<HashSet<PathBuf>>> = Rc::default();
    let polling = Rc::new(std::cell::Cell::new(false));
    let (h, j) = (handle.clone(), jtx.clone());
    handle
        .insert_source(rx, move |ev, _, data| {
            let channel::Event::Msg(m) = ev else { return };
            if let Some(ix) = m.index {
                apply(data, ix);
            }
            for d in m.dirs {
                if !watched.borrow().contains(&d) {
                    watch_dir(&h, &d, &j, &watched, &polling, &roots);
                }
            }
        })
        .map_err(|e| format!("library channel: {e}"))?;
    jtx.send(()).map_err(|_| "library thread ended".to_string())
}

fn watch_dir<D: 'static>(h: &LoopHandle<'static, D>, d: &Path, jobs: &mpsc::Sender<()>, watched: &Rc<RefCell<HashSet<PathBuf>>>, polling: &Rc<std::cell::Cell<bool>>, roots: &Roots) {
    let root = *d == roots.apps || *d == roots.bin;
    match crate::backend::hotplug::inotify(&d.to_string_lossy(), MASK) {
        Ok(file) => {
            let fd = file.as_raw_fd();
            let (j, w, dd) = (jobs.clone(), watched.clone(), d.to_path_buf());
            let r = h.insert_source(Generic::new(file, Interest::READ, Mode::Level), move |_, _, _| {
                let gone = crate::backend::hotplug::drain(fd);
                let _ = j.send(());
                if gone {
                    w.borrow_mut().remove(&dd);
                    return Ok(PostAction::Remove);
                }
                Ok(PostAction::Continue)
            });
            match r {
                Ok(_) => {
                    watched.borrow_mut().insert(d.to_path_buf());
                    // Once more now the watch is in place: a change since the scan is not missed.
                    let _ = jobs.send(());
                }
                Err(e) => eprintln!("bitscomp: library: no watch on {}: {e}", d.display()),
            }
        }
        // A missing folder is no error (no /apps before PK-0); its parent's watch or the rescan sees it come.
        Err(e) => {
            if root || e.raw_os_error() != Some(libc::ENOENT) {
                if !polling.replace(true) {
                    eprintln!("bitscomp: library: no watch on {} ({e}); looking every {} s instead", d.display(), RESCAN.as_secs());
                    let j = jobs.clone();
                    let r = h.insert_source(Timer::from_duration(RESCAN), move |_, _, _| {
                        if j.send(()).is_err() { TimeoutAction::Drop } else { TimeoutAction::ToDuration(RESCAN) }
                    });
                    if let Err(e) = r {
                        eprintln!("bitscomp: library: no rescan timer either: {e}");
                    }
                }
            }
        }
    }
}

#[cfg(test)]
#[path = "library_tests.rs"]
mod tests;
