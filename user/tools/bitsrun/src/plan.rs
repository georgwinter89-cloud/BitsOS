//! The view plan (PK-3, D8): which binds, links and folders make a program's view, in the order they are made.
//! Pure: the tree, the system and the wishes come in as facts, so host tests check the order without mounting.
//!
//! The view: a fresh root that ends read-only; `/usr` = the tree's `usr` (one read-only bind), `/bin`, `/sbin`
//! -> usr/bin and `/lib`, `/lib64` -> usr/lib (PT_INTERP finds the tree's own ld-linux-x86-64.so.2, no
//! LD_LIBRARY_PATH); `/etc` = the tree's `etc` read-only with the system's resolv.conf, CA bundle, passwd, group
//! and localtime bound over it; `/dev`, `/proc`, `/sys`; a private `/tmp`; the user's runtime folder (Wayland,
//! X11); the system's fonts read-only at `$HOME/.local/share/fonts`; the data folder at `/data/appdata/<name>`
//! and `$HOME/.local/share/<name>`; Downloads, Documents and Pictures only when the manifest wants them.

use bitspkg::perm::{Permission, Want};
use std::collections::BTreeMap;
use std::ffi::OsString;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Missing,
    Dir,
    File,
    Link,
    Other,
}

/// The program's root tree, by paths relative to its root (`/`-separated, no leading `/`).
pub trait Tree {
    /// What `rel` is, never following a final symlink.
    fn kind(&self, rel: &str) -> Kind;
    /// The names in the folder `rel`, sorted (empty when it is no folder).
    fn list(&self, rel: &str) -> Vec<String>;
    /// The target of the symlink `rel`.
    fn link(&self, rel: &str) -> Option<String>;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Op {
    RoBind { src: String, dst: String },
    Bind { src: String, dst: String },
    /// Device nodes stay usable (no `nodev`).
    DevBind { src: String, dst: String },
    Symlink { target: String, dst: String },
    Dir { dst: String, mode: u32 },
    Tmpfs { dst: String, mode: u32 },
    /// A minimal /dev (null, zero, full, random, urandom, tty, pts, shm).
    Dev { dst: String },
    Proc { dst: String },
    RemountRo { dst: String },
    /// A network namespace of its own with only a loopback that is down (PK-6d's triggers: no network, plan 1.7).
    UnshareNet,
}

impl std::fmt::Display for Op {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Op::RoBind { src, dst } => write!(f, "ro-bind {src} -> {dst}"),
            Op::Bind { src, dst } => write!(f, "bind {src} -> {dst}"),
            Op::DevBind { src, dst } => write!(f, "dev-bind {src} -> {dst}"),
            Op::Symlink { target, dst } => write!(f, "symlink {dst} -> {target}"),
            Op::Dir { dst, mode } => write!(f, "dir {dst} {mode:04o}"),
            Op::Tmpfs { dst, mode } => write!(f, "tmpfs {dst} {mode:04o}"),
            Op::Dev { dst } => write!(f, "dev {dst}"),
            Op::Proc { dst } => write!(f, "proc {dst}"),
            Op::RemountRo { dst } => write!(f, "remount-ro {dst}"),
            Op::UnshareNet => write!(f, "unshare-net"),
        }
    }
}

/// The system files bound over the tree's /etc, by their path under /etc.
pub const SYSTEM_ETC: [&str; 5] = ["resolv.conf", "ssl/certs/ca-certificates.crt", "passwd", "group", "localtime"];
/// The user folders a manifest may want (`path` permission on `<home>/<folder>`).
pub const USER_FOLDERS: [&str; 3] = ["Downloads", "Documents", "Pictures"];
/// Device classes the view can give (crates/policy `DeviceClass`) and their folders under /dev.
pub const DEVICES: [(&str, &str); 2] = [("gpu", "/dev/dri"), ("audio", "/dev/snd")];
/// Top-level folders of the tree besides usr and etc that the view shows read-only at their place.
pub const TOP_RO: [&str; 2] = ["opt", "srv"];
/// The system's font folder (the image's fonts; whatever the system adds below it comes along).
pub const SYSTEM_FONTS: &str = "/usr/share/fonts";
/// Where the view shows it, below the home folder: fontconfig's `<dir prefix="xdg">fonts</dir>`, which the
/// closure's own fonts.conf has and fontconfig's built-in configuration too (a tree without /etc/fonts). The
/// tree's own /usr/share/fonts stays the tree's: /usr is one read-only bind and takes no mount point.
pub const FONTS_IN_HOME: &str = ".local/share/fonts";
/// The same for a program whose name is `fonts` (its data folder has that place): fontconfig's older home folder.
pub const FONTS_IN_HOME_OLD: &str = ".fonts";
/// The caller's environment that reaches the program; everything else is left out.
const ENV_PASS: [&str; 10] =
    ["LANG", "LANGUAGE", "LC_ALL", "TERM", "COLORTERM", "TZ", "WAYLAND_DISPLAY", "DISPLAY", "XDG_SESSION_TYPE", "XDG_CURRENT_DESKTOP"];
const ENV_DROP_NOTED: [&str; 3] = ["LD_LIBRARY_PATH", "LD_PRELOAD", "LD_AUDIT"];

/// What the system has, as the caller sees it.
#[derive(Clone, Debug, Default)]
pub struct System {
    /// (path under /etc, source) of the [`SYSTEM_ETC`] files that exist.
    pub etc: Vec<(String, String)>,
    /// The user's runtime folder (`/run/user/<uid>`: Wayland and bitscomp sockets) when it exists.
    pub runtime_dir: Option<String>,
    /// `/tmp/.X11-unix` when it exists (Xwayland).
    pub x11: Option<String>,
    /// `/sys` when it exists.
    pub sys: bool,
    /// (class, folder) of the [`DEVICES`] that exist.
    pub devices: Vec<(String, String)>,
    /// The [`USER_FOLDERS`] that exist under the home folder.
    pub user_folders: Vec<String>,
    /// The system's font folder ([`SYSTEM_FONTS`]) when it exists.
    pub fonts: Option<String>,
}

pub struct Input<'a> {
    pub tree: &'a dyn Tree,
    /// The tree's root as the caller sees it (absolute, no trailing `/`).
    pub tree_dir: String,
    /// The package name (None for `--root` runs without `--name`).
    pub name: Option<String>,
    /// The data folder as the caller sees it.
    pub data: Option<String>,
    pub home: String,
    pub user: String,
    pub system: System,
    pub permissions: &'a [Permission],
    /// The caller's environment.
    pub env: Vec<(String, String)>,
    /// The program's path in the view (absolute).
    pub program: String,
    /// The program's arguments as the caller gave them (a file name need not be UTF-8).
    pub args: Vec<OsString>,
}

#[derive(Clone, Debug, Default)]
pub struct Plan {
    pub ops: Vec<Op>,
    /// The program's whole environment, sorted.
    pub env: Vec<(String, String)>,
    pub chdir: String,
    pub argv: Vec<OsString>,
    /// Choices and what was left out, in German (the report).
    pub notes: Vec<String>,
}

/// Where the manifest's `program` lies in the view. Format 1 names it inside the version folder (`root/usr/bin/x`
/// for a root tree); format 2 (PK-6d) inside the tree (`usr/bin/x`). Only the folders the view shows can hold it.
pub fn program_in_view(program: &str, root_tree: bool) -> Result<String, String> {
    let rel = if root_tree { program.strip_prefix("root/").unwrap_or(program) } else { program };
    let top = rel.split('/').next().unwrap_or("");
    if rel.is_empty() || rel.split('/').any(|c| c.is_empty() || c == "." || c == "..") {
        return Err(format!("Programm {program:?}: kein gültiger Pfad im Baum"));
    }
    if top != "usr" && !TOP_RO.contains(&top) {
        return Err(format!("Programm {program:?} liegt nicht in usr/ (oder opt/, srv/) des Baums: die Sicht zeigt es nicht"));
    }
    Ok(format!("/{rel}"))
}

pub fn build(i: &Input) -> Result<Plan, String> {
    let t = i.tree;
    let mut p = Plan::default();
    if t.kind("usr") != Kind::Dir {
        return Err(format!("{}: kein Programmbaum (usr/ fehlt)", i.tree_dir));
    }
    let prog_rel = i.program.trim_start_matches('/');
    if !matches!(t.kind(prog_rel), Kind::File | Kind::Link) {
        return Err(format!("{}: das Programm {} fehlt im Baum", i.tree_dir, i.program));
    }
    let src = |rel: &str| format!("{}/{rel}", i.tree_dir);
    p.ops.push(Op::RoBind { src: src("usr"), dst: "/usr".into() });
    for (l, target) in [("/bin", "usr/bin"), ("/sbin", "usr/bin"), ("/lib", "usr/lib"), ("/lib64", "usr/lib")] {
        p.ops.push(Op::Symlink { target: target.into(), dst: l.into() });
    }
    // A multilib tree (Arch's lib32-*): its 32-bit loader is found through /lib (usr/lib/ld-linux.so.2 links to
    // ../lib32), the libraries through /usr/lib32 and, for programs that name it, /lib32.
    if t.kind("usr/lib32") == Kind::Dir {
        p.ops.push(Op::Symlink { target: "usr/lib32".into(), dst: "/lib32".into() });
    }
    for d in TOP_RO {
        if t.kind(d) == Kind::Dir {
            p.ops.push(Op::RoBind { src: src(d), dst: format!("/{d}") });
        }
    }
    let shown = ["usr", "etc", "bin", "sbin", "lib", "lib64", "lib32"];
    let left: Vec<String> =
        t.list("").into_iter().filter(|n| !shown.contains(&n.as_str()) && !(TOP_RO.contains(&n.as_str()) && t.kind(n) == Kind::Dir)).collect();
    if !left.is_empty() {
        p.notes.push(format!("nicht in der Sicht (Ordner des Baums außer usr, etc, opt, srv): {}", left.join(", ")));
    }
    etc(i, &mut p);
    p.ops.push(Op::Dev { dst: "/dev".into() });
    let wants = wanted(i, &mut p.notes);
    for class in &wants.devices {
        match i.system.devices.iter().find(|(c, _)| c == class) {
            Some((_, d)) => p.ops.push(Op::DevBind { src: d.clone(), dst: d.clone() }),
            None => p.notes.push(format!("Gerät {class}: gewünscht, aber auf diesem System nicht vorhanden")),
        }
    }
    p.ops.push(Op::Proc { dst: "/proc".into() });
    if i.system.sys {
        p.ops.push(Op::RoBind { src: "/sys".into(), dst: "/sys".into() });
    }
    p.ops.push(Op::Tmpfs { dst: "/tmp".into(), mode: 0o1777 });
    if let Some(x) = &i.system.x11 {
        p.ops.push(Op::RoBind { src: x.clone(), dst: x.clone() });
    }
    p.ops.push(Op::Dir { dst: "/run".into(), mode: 0o755 });
    if let Some(r) = &i.system.runtime_dir {
        p.ops.push(Op::Bind { src: r.clone(), dst: r.clone() });
    }
    p.ops.push(Op::Dir { dst: i.home.clone(), mode: 0o700 });
    fonts(i, &mut p);
    let share = match (&i.name, &i.data) {
        (Some(n), Some(d)) => {
            let share = format!("{}/.local/share/{n}", i.home);
            p.ops.push(Op::Bind { src: d.clone(), dst: format!("/data/appdata/{n}") });
            p.ops.push(Op::Bind { src: d.clone(), dst: share.clone() });
            Some(share)
        }
        _ => {
            p.notes.push("kein Datenordner: das Programm kann nur nach /tmp schreiben".into());
            None
        }
    };
    for (f, write) in &wants.folders {
        let d = format!("{}/{f}", i.home);
        p.ops.push(if *write { Op::Bind { src: d.clone(), dst: d.clone() } } else { Op::RoBind { src: d.clone(), dst: d } });
    }
    p.ops.push(Op::RemountRo { dst: "/".into() });
    p.notes.push("Rechte: nur Unix-Rechte und diese Sicht; bis PM-3 fragt niemand nach (PACKAGES.md 6.3)".into());
    p.env = environment(i, share.as_deref(), &mut p.notes);
    p.chdir = i.home.clone();
    p.argv = std::iter::once(OsString::from(&i.program)).chain(i.args.iter().cloned()).collect();
    Ok(p)
}

/// The system's fonts, read-only, where every fontconfig looks by itself: no copy per program, no file written
/// into the program's folders, and the fonts of the tree (/usr/share/fonts there) stay first in fontconfig's list.
fn fonts(i: &Input, p: &mut Plan) {
    let Some(src) = &i.system.fonts else { return };
    let place = if i.name.as_deref() == Some("fonts") {
        p.notes.push(format!("Schriften: der Datenordner von fonts liegt an ihrem Platz; die Systemschriften stehen in ~/{FONTS_IN_HOME_OLD}"));
        FONTS_IN_HOME_OLD
    } else {
        FONTS_IN_HOME
    };
    p.ops.push(Op::RoBind { src: src.clone(), dst: format!("{}/{place}", i.home) });
}

/// /etc: one read-only bind of the tree's etc when every system file lands on a plain file of it; otherwise a
/// folder of the (read-only) root with the tree's entries bound one by one, so a missing file or a symlink in
/// the way never needs a write into the tree.
fn etc(i: &Input, p: &mut Plan) {
    let t = i.tree;
    let have: Vec<&str> = i.system.etc.iter().map(|(n, _)| n.as_str()).collect();
    let missing: Vec<&str> = SYSTEM_ETC.iter().copied().filter(|n| !have.contains(n)).collect();
    if !missing.is_empty() {
        p.notes.push(format!("Systemdateien fehlen und bleiben weg: /etc/{}", missing.join(", /etc/")));
    }
    let ovs: Vec<(String, String)> = i.system.etc.clone();
    if t.kind("etc") == Kind::Dir && ovs.iter().all(|(r, _)| fits(t, "etc", r)) {
        p.ops.push(Op::RoBind { src: format!("{}/etc", i.tree_dir), dst: "/etc".into() });
        for (r, s) in ovs {
            p.ops.push(Op::RoBind { src: s, dst: format!("/etc/{r}") });
        }
        return;
    }
    let rel = (t.kind("etc") == Kind::Dir).then(|| "etc".to_string());
    if rel.is_some() {
        p.notes.push("/etc: Einträge des Baums einzeln eingebunden (eine Systemdatei fehlt im Baum oder ein Link steht im Weg)".into());
    }
    merge(i, rel, "/etc".into(), ovs, p);
}

/// Every folder on the way to `sub` below `rel` is a real folder of the tree and `sub` a plain file.
fn fits(t: &dyn Tree, rel: &str, sub: &str) -> bool {
    let parts: Vec<&str> = sub.split('/').collect();
    let mut at = rel.to_string();
    for (k, c) in parts.iter().enumerate() {
        at = format!("{at}/{c}");
        let want = if k + 1 == parts.len() { Kind::File } else { Kind::Dir };
        if t.kind(&at) != want {
            return false;
        }
    }
    true
}

fn merge(i: &Input, rel: Option<String>, dst: String, ovs: Vec<(String, String)>, p: &mut Plan) {
    let t = i.tree;
    p.ops.push(Op::Dir { dst: dst.clone(), mode: 0o755 });
    let mut groups: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
    for (sub, s) in ovs {
        let (first, rest) = sub.split_once('/').unwrap_or((sub.as_str(), ""));
        groups.entry(first.to_string()).or_default().push((rest.to_string(), s.clone()));
    }
    for e in rel.as_ref().map(|r| t.list(r)).unwrap_or_default() {
        let tp = format!("{}/{e}", rel.as_deref().unwrap_or(""));
        let d = format!("{dst}/{e}");
        match groups.remove(&e) {
            Some(g) => place(i, Some(tp), d, g, p),
            None => match t.kind(&tp) {
                Kind::Dir | Kind::File => p.ops.push(Op::RoBind { src: format!("{}/{tp}", i.tree_dir), dst: d }),
                Kind::Link => match t.link(&tp) {
                    Some(target) => p.ops.push(Op::Symlink { target, dst: d }),
                    None => p.notes.push(format!("{d}: Link nicht lesbar, weggelassen")),
                },
                Kind::Other => p.notes.push(format!("{d}: weder Datei, Ordner noch Link, weggelassen")),
                Kind::Missing => {}
            },
        }
    }
    for (first, g) in groups {
        place(i, None, format!("{dst}/{first}"), g, p);
    }
}

fn place(i: &Input, tp: Option<String>, d: String, g: Vec<(String, String)>, p: &mut Plan) {
    let t = i.tree;
    if let Some((_, s)) = g.iter().find(|(rest, _)| rest.is_empty()) {
        p.ops.push(Op::RoBind { src: s.clone(), dst: d });
        return;
    }
    let kind = tp.as_deref().map(|r| t.kind(r)).unwrap_or(Kind::Missing);
    match tp {
        Some(r) if kind == Kind::Dir && g.iter().all(|(rest, _)| fits(t, &r, rest)) => {
            p.ops.push(Op::RoBind { src: format!("{}/{r}", i.tree_dir), dst: d.clone() });
            for (rest, s) in g {
                p.ops.push(Op::RoBind { src: s, dst: format!("{d}/{rest}") });
            }
        }
        Some(r) if kind == Kind::Dir => merge(i, Some(r), d, g, p),
        _ => {
            if kind != Kind::Missing {
                p.notes.push(format!("{d}: Eintrag des Baums durch einen Ordner für Systemdateien ersetzt"));
            }
            merge(i, None, d, g, p)
        }
    }
}

struct Wants {
    /// (folder name, write)
    folders: Vec<(String, bool)>,
    devices: Vec<String>,
}

/// The manifest's wanted permissions the view can give before PM-3: the user folders and devices; the rest is
/// listed as not given.
fn wanted(i: &Input, notes: &mut Vec<String>) -> Wants {
    let mut w = Wants { folders: Vec::new(), devices: Vec::new() };
    let mut net = false;
    for perm in i.permissions {
        match &perm.want {
            Want::Path { path, write, .. } => {
                let (parent, leaf) = path.rsplit_once('/').unwrap_or(("", path.as_str()));
                let home_like = parent == i.home || (parent.starts_with("/home/") && parent.matches('/').count() == 2);
                if !home_like || !USER_FOLDERS.contains(&leaf) {
                    notes.push(format!("Pfad {path}: nicht in der Sicht (andere Pfade als Downloads, Documents, Pictures erst mit PM-3)"));
                } else if let Some(e) = w.folders.iter_mut().find(|(f, _)| f == leaf) {
                    e.1 |= *write;
                } else {
                    w.folders.push((leaf.to_string(), *write));
                }
            }
            Want::Device { class, .. } if DEVICES.iter().any(|(c, _)| c == class) => {
                if !w.devices.contains(class) {
                    w.devices.push(class.clone());
                }
            }
            Want::Device { class, .. } => notes.push(format!("Gerät {class}: nicht in der Sicht (erst mit PM-3)")),
            Want::NetConnect { .. } | Want::NetBind { .. } | Want::NetListen { .. } => net = true,
            _ => {}
        }
    }
    // One note per folder, after all wishes are merged: a later write wish makes an earlier read wish writable.
    w.folders.retain(|(leaf, write)| {
        let have = i.system.user_folders.iter().any(|f| f == leaf);
        let how = if *write { "lesen und schreiben" } else { "nur lesen" };
        notes.push(if have {
            format!("{}/{leaf}: {how} (gewünscht; bis PM-3 ohne Rückfrage, nur Unix-Rechte)", i.home)
        } else {
            format!("{}/{leaf}: gewünscht, aber nicht vorhanden, nicht eingebunden", i.home)
        });
        have
    });
    w.folders.sort();
    w.devices.sort();
    if net {
        notes.push("Netz: wie das System, keine Regeln bis PM-3".into());
    }
    w
}

fn environment(i: &Input, share: Option<&str>, notes: &mut Vec<String>) -> Vec<(String, String)> {
    let mut env: BTreeMap<String, String> = BTreeMap::new();
    for (k, v) in &i.env {
        if ENV_PASS.contains(&k.as_str()) || k.starts_with("LC_") {
            env.insert(k.clone(), v.clone());
        } else if ENV_DROP_NOTED.contains(&k.as_str()) {
            notes.push(format!("{k} weggelassen: die Bibliotheken kommen aus dem Baum"));
        }
    }
    env.insert("PATH".into(), "/usr/bin".into());
    env.insert("HOME".into(), i.home.clone());
    env.insert("USER".into(), i.user.clone());
    env.insert("LOGNAME".into(), i.user.clone());
    if let Some(n) = &i.name {
        env.insert("BITSOS_PROGRAM".into(), n.clone());
    }
    match &i.system.runtime_dir {
        Some(r) => {
            env.insert("XDG_RUNTIME_DIR".into(), r.clone());
        }
        None => {
            env.remove("WAYLAND_DISPLAY");
        }
    }
    if i.system.x11.is_none() {
        env.remove("DISPLAY");
    }
    if let Some(s) = share {
        env.insert("XDG_CONFIG_HOME".into(), format!("{s}/config"));
        env.insert("XDG_CACHE_HOME".into(), format!("{s}/cache"));
    }
    env.into_iter().collect()
}
