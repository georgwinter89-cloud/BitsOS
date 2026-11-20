//! The per-program root tree (plan 1.6, D8): `apps/<prog>/<version>/root/` is the merged closure, hard links into
//! the content store again (no copy, no overlay), with the outputs of the emulated hooks (trigger.rs) and
//! `manifest.toml` in format 2 beside it. A path two packages of the closure both own (other than a folder) is a
//! file conflict and refuses the program, as pacman's conflict.c:_alpm_db_find_fileconflicts does; never last-wins.

use super::pkgs::{Linker, List};
use super::trigger::{self, Outcome, Result1};
use crate::fsx::{self, err};
use crate::manifest::{FILE_NAME, FORMAT_REPO, FileEntry, Launch, Manifest, Origin, Symlink, TriggerOut, Uses};
use crate::perm::{Permission, Want};
use crate::store::Store;
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

/// A trigger's output file is bounded like a manifest's input (R1: one input's bound).
const OUTPUT_MAX: u64 = 512 << 20;
const O_NOFOLLOW: i32 = 0o400000;
const O_NONBLOCK: i32 = 0o4000;

pub struct Part {
    pub pkg: String,
    /// The `.pkgs` folder name.
    pub folder: String,
    pub list: List,
    pub origin: Origin,
}

pub struct Built {
    pub manifest: Manifest,
    pub triggers: Vec<Result1>,
}

#[derive(Clone, Copy, PartialEq)]
enum K {
    Dir,
    File(usize),
    Link(usize),
}

/// `[Desktop Entry]` keys of a .desktop file (the library's reader is PK-L1a's; this one only for the manifest).
fn desktop(text: &str) -> Option<(String, String, String, String)> {
    let mut main = false;
    let (mut name, mut exec, mut icon, mut cats, mut hidden, mut app) = (String::new(), String::new(), String::new(), String::new(), false, true);
    for l in text.lines().map(str::trim) {
        if l.starts_with('[') {
            main = l == "[Desktop Entry]";
            continue;
        }
        if !main {
            continue;
        }
        let Some((k, v)) = l.split_once('=') else { continue };
        let v = v.trim().to_string();
        match k.trim() {
            "Name" => name = v,
            "Exec" => exec = v,
            "Icon" => icon = v,
            "Categories" => cats = v,
            "NoDisplay" | "Hidden" if v.eq_ignore_ascii_case("true") => hidden = true,
            "Type" => app = v == "Application",
            _ => {}
        }
    }
    (!hidden && app && !exec.is_empty()).then_some((name, exec, icon, cats))
}

/// The program word of an Exec line (desktop entry spec: the first word, quotes allowed, field codes later).
fn exec_word(exec: &str) -> String {
    let w = exec.trim_start();
    if let Some(r) = w.strip_prefix('"') {
        return r.split('"').next().unwrap_or("").to_string();
    }
    w.split_whitespace().next().unwrap_or("").to_string()
}

/// Plan 1.6's wanted permissions by kind: the user folders for every program (ask), devices and the web for the
/// categories that need them (ask). PM-3 turns "ask" into Georg's answers; until then only Unix rights hold.
pub fn wanted(launch: &[Launch]) -> Vec<Permission> {
    let why = "Voreinstellung für Programme aus den Quellen: fragen (PM-3)".to_string();
    let mut v: Vec<Permission> = ["Downloads", "Documents", "Pictures"]
        .iter()
        .map(|f| Permission { want: Want::Path { path: format!("/data/home/1000/{f}"), write: true, create: false }, why: why.clone() })
        .collect();
    let net = launch.iter().any(|l| l.categories.split(';').any(|c| matches!(c, "Network" | "WebBrowser" | "AudioVideo" | "Game")));
    if net {
        for class in ["audio", "gpu"] {
            v.push(Permission { want: Want::Device { class: class.into(), name: None }, why: why.clone() });
        }
        for port in [80, 443] {
            v.push(Permission { want: Want::NetConnect { proto: "tcp".into(), host: None, port: Some(port) }, why: why.clone() });
        }
    }
    v
}

fn parents(p: &str) -> impl Iterator<Item = &str> {
    p.match_indices('/').map(move |(i, _)| &p[..i])
}

/// Builds the version in `apps/<prog>/.new-<version>` and renames it into place. `parts` in install order.
pub fn build(st: &Store, lk: &mut Linker, prog: &str, version: &str, parts: &[Part], de: &str, en: &str) -> Result<Built, String> {
    // The merged tree, checked before anything is written.
    let mut at: BTreeMap<String, (K, usize)> = BTreeMap::new();
    for (pi, p) in parts.iter().enumerate() {
        let mut put = |path: &str, k: K| -> Result<(), String> {
            match at.get(path) {
                Some((K::Dir, _)) if k == K::Dir => return Ok(()),
                Some((_, other)) => {
                    return Err(format!("Dateikonflikt: {path} steht in {} und in {}: abgelehnt (wie pacman)", parts[*other].pkg, p.pkg));
                }
                None => {}
            }
            at.insert(path.to_string(), (k, pi));
            Ok(())
        };
        for d in &p.list.dirs {
            put(d, K::Dir)?;
        }
        for (i, f) in p.list.files.iter().enumerate() {
            put(&f.path, K::File(i))?;
        }
        for (i, l) in p.list.symlinks.iter().enumerate() {
            put(&l.path, K::Link(i))?;
        }
    }
    for (path, (_, pi)) in &at {
        for pre in parents(path) {
            match at.get(pre) {
                Some((K::Dir, _)) | None => {}
                Some((_, other)) => {
                    return Err(format!("Dateikonflikt: {pre} ist in {} eine Datei oder ein Verweis, {} legt {path} darunter: abgelehnt", parts[*other].pkg, parts[*pi].pkg));
                }
            }
        }
    }
    let pkg_dir = st.pkg(prog);
    fsx::mkdir(&pkg_dir, 0o755)?;
    let staging = pkg_dir.join(format!(".new-{version}"));
    fsx::remove_tree(&staging)?;
    crate::cut::step("create staging");
    fsx::mkdir(&staging, 0o755)?;
    let r = (|| {
        let root = staging.join("root");
        fsx::mkdir(&root, 0o755)?;
        let mut files: BTreeMap<String, FileEntry> = BTreeMap::new();
        let mut links: BTreeMap<String, String> = BTreeMap::new();
        let mut dirs: Vec<String> = Vec::new();
        for (path, (k, pi)) in &at {
            let dst = root.join(path);
            match k {
                K::Dir => {
                    fsx::mkdir(&dst, 0o755)?;
                    dirs.push(path.clone());
                }
                K::File(i) => {
                    let f = &parts[*pi].list.files[*i];
                    lk.link(f, &dst)?;
                    files.insert(path.clone(), f.clone());
                }
                K::Link(i) => {
                    let l = &parts[*pi].list.symlinks[*i];
                    std::os::unix::fs::symlink(&l.target, &dst).map_err(|e| err(&dst, e))?;
                    links.insert(path.clone(), l.target.clone());
                }
            }
        }
        // The hooks, in the staging view (BTreeMap order is a parent before its children, so folders exist).
        let work = staging.join(".trig");
        fsx::mkdir(&work, 0o755)?;
        let closure: Vec<(String, &List)> = parts.iter().map(|p| (p.pkg.clone(), &p.list)).collect();
        let results = trigger::Runner::new(&root, &work).run_all(&closure);
        let mut outs = Vec::new();
        for r in &results {
            match &r.outcome {
                Outcome::Emulated(list) => {
                    for (rel, src) in list {
                        let fe = place_output(lk, &root, rel, src, &mut files, &mut links, &mut dirs)?;
                        outs.push(TriggerOut { hook: r.hook.clone(), tool: r.tool.clone(), output: format!("root/{rel}"), sha256: fe.sha256.clone() });
                    }
                }
                Outcome::Links(list) => {
                    for (rel, target) in list {
                        if files.contains_key(rel) || links.contains_key(rel) {
                            continue;
                        }
                        ensure_dirs(&root, rel, &mut dirs)?;
                        let dst = root.join(rel);
                        std::os::unix::fs::symlink(target, &dst).map_err(|e| err(&dst, e))?;
                        links.insert(rel.clone(), target.clone());
                    }
                }
                _ => {}
            }
        }
        fsx::remove_tree(&work)?;
        crate::cut::step("after triggers");
        // Launchers: every shown .desktop file of usr/share/applications.
        let mut launch = Vec::new();
        for (p, f) in &files {
            if let Some(n) = p.strip_prefix("usr/share/applications/")
                && n.ends_with(".desktop")
                && !n.contains('/')
                && f.size <= 1 << 20
                && let Ok(t) = fs::read_to_string(root.join(p))
                && let Some((name, exec, icon, cats)) = desktop(&t)
            {
                launch.push(Launch { desktop: format!("root/{p}"), name, exec, icon, categories: cats });
            }
        }
        let runnable = |rel: &str| files.get(rel).is_some_and(|f| f.exec) || links.contains_key(rel);
        let mut program = None;
        for l in &launch {
            let w = exec_word(&l.exec);
            let rel = if let Some(a) = w.strip_prefix('/') { a.to_string() } else { format!("usr/bin/{w}") };
            if !w.is_empty() && runnable(&rel) {
                program = Some(rel);
                break;
            }
        }
        if program.is_none() && runnable(&format!("usr/bin/{prog}")) {
            program = Some(format!("usr/bin/{prog}"));
        }
        if program.is_none() {
            program = files.iter().find(|(p, f)| f.exec && p.strip_prefix("usr/bin/").is_some_and(|b| !b.contains('/'))).map(|(p, _)| p.clone());
        }
        let m = Manifest {
            format: FORMAT_REPO,
            name: prog.to_string(),
            version: version.to_string(),
            de: de.to_string(),
            en: en.to_string(),
            program: program.map(|p| format!("root/{p}")),
            files: files.into_iter().map(|(p, mut f)| {
                f.path = format!("root/{p}");
                f
            }).collect(),
            symlinks: links.into_iter().map(|(p, t)| Symlink { path: format!("root/{p}"), target: t }).collect(),
            dirs: std::iter::once("root".to_string()).chain(dirs.into_iter().map(|d| format!("root/{d}"))).collect(),
            permissions: wanted(&launch),
            actions: Vec::new(),
            services: Vec::new(),
            settings: Vec::new(),
            origins: parts.iter().map(|p| p.origin.clone()).collect(),
            uses: parts.iter().map(|p| Uses { pkg: p.pkg.clone(), version: p.folder.clone() }).collect(),
            launch,
            triggers: outs,
        };
        // What the parser will read back is what is written (format 2's own checks run on it here).
        let text = m.to_text();
        Manifest::from_text(&text).map_err(|e| format!("Manifest der neuen Version: {e}"))?;
        fsx::write_atomic(&staging.join(FILE_NAME), text.as_bytes(), 0o444)?;
        fsx::seal_dirs(&staging)?;
        Ok(Built { manifest: m, triggers: results })
    })();
    match r {
        Ok(b) => {
            crate::cut::step("rename version into place");
            fsx::rename(&staging, &st.version_dir(prog, version))?;
            Ok(b)
        }
        Err(e) => {
            let _ = fsx::remove_tree(&staging);
            Err(e)
        }
    }
}

fn ensure_dirs(root: &Path, rel: &str, dirs: &mut Vec<String>) -> Result<(), String> {
    for pre in parents(rel) {
        let p = root.join(pre);
        match fs::symlink_metadata(&p) {
            Ok(m) if m.is_dir() => {}
            Ok(_) => return Err(err(&p, "kein Ordner")),
            Err(_) => {
                fsx::mkdir(&p, 0o755)?;
                dirs.push(pre.to_string());
            }
        }
    }
    Ok(())
}

/// A trigger's output file into the store and the tree (over a file of the same name a package shipped).
fn place_output(
    lk: &mut Linker,
    root: &Path,
    rel: &str,
    src: &Path,
    files: &mut BTreeMap<String, FileEntry>,
    links: &mut BTreeMap<String, String>,
    dirs: &mut Vec<String>,
) -> Result<FileEntry, String> {
    crate::names::check_repo_path(rel)?;
    let f = fs::OpenOptions::new().read(true).custom_flags(O_NOFOLLOW | O_NONBLOCK).open(src).map_err(|e| err(src, e))?;
    let md = f.metadata().map_err(|e| err(src, e))?;
    if !md.is_file() || md.len() > OUTPUT_MAX {
        return Err(err(src, "keine gewöhnliche Datei oder größer als 512 MiB"));
    }
    let mut f = f;
    let mut read = |b: &mut [u8]| std::io::Read::read(&mut f, b).map_err(|e| err(src, e));
    let fe = lk.store(&mut read, rel, false)?;
    ensure_dirs(root, rel, dirs)?;
    let dst = root.join(rel);
    if files.remove(rel).is_some() || links.remove(rel).is_some() {
        fs::remove_file(&dst).map_err(|e| err(&dst, e))?;
    }
    lk.link(&fe, &dst)?;
    files.insert(rel.to_string(), fe.clone());
    Ok(fe)
}

/// For tests and `info`: which packages of a version's closure own a path (file conflict check helper).
pub fn owners(parts: &[Part]) -> HashMap<String, Vec<String>> {
    let mut m: HashMap<String, Vec<String>> = HashMap::new();
    for p in parts {
        for f in &p.list.files {
            m.entry(f.path.clone()).or_default().push(p.pkg.clone());
        }
    }
    m
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn desktop_entries_and_wishes() {
        let t = "[Desktop Entry]\nType=Application\nName=Foot\nExec=foot\nIcon=foot\nCategories=System;TerminalEmulator;\n[Desktop Action new]\nExec=other\n";
        assert_eq!(desktop(t), Some(("Foot".into(), "foot".into(), "foot".into(), "System;TerminalEmulator;".into())));
        assert_eq!(desktop("[Desktop Entry]\nName=x\nExec=x\nNoDisplay=true\n"), None);
        assert_eq!(exec_word("\"/usr/bin/my prog\" %U"), "/usr/bin/my prog");
        let l = |c: &str| vec![Launch { desktop: "x".into(), name: "x".into(), exec: "x".into(), icon: String::new(), categories: c.into() }];
        assert_eq!(wanted(&l("System;")).len(), 3);
        assert_eq!(wanted(&l("Network;WebBrowser;")).len(), 7);
    }
}
