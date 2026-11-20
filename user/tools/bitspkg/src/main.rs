//! The `bitspkg` command (plan/PACKAGES.md section "The tool").
#![forbid(unsafe_code)]

use bitspkg::install::{self, Outcome};
use bitspkg::store::Store;
use bitspkg::{build, remove, verify};
use std::path::{Path, PathBuf};
use std::process::exit;

const USAGE: &str = "usage: bitspkg [--root DIR] [--settings SOCKET|none] COMMAND
  build TREE [--spec FILE] [--out DIR]   make a package (manifest.toml) from a folder and its package.toml
  check PKGDIR                           check a package's manifest without installing
  install PKGDIR                         install (or make current) a package version
  install [--from-repo] [REPO/]NAME [--sources FILE]
                                         a program from the repositories (PK-6d): download, check, unpack into
                                         /apps/.pkgs, its own root tree, hooks emulated; NAME that is no folder
                                         means the repositories
  switch NAME VERSION                    make an installed version current
  rollback NAME                          swap current and previous (with the closure difference, 1 Teil älter)
  remove NAME [--version V] [--keep-data]  remove a package (with its data folder) or one version; .pkgs entries
                                         no version uses any more go with their store files
  list                                   installed packages
  info NAME [VERSION]                    what a version brings and wants
  info --scripts NAME                    the .INSTALL scripts of a program's packages (kept, never run)
  register [NAME]                        register settings keys of the current versions again (after a boot)
  verify [NAME] [--store]                check every file of every installed version (reports, never repairs)
  space [NAME]                           space per program: versions, own/shared, data folder, caches; store, gc
  at SNAPDIR NAME [--apply] [--snap-data DIR]
                                         B1: the program's version (SNAPDIR/apps) and data (SNAPDIR/appdata, or DIR)
                                         in a snapshot; --apply makes that version current again (rollback undoes it)
  gc                                     remove unused versions, .pkgs entries and store files
  import SRC OUT [--origin DIR] [--system LIBSET] [--dlopen LIB]... [--exclude PATH]... [--layout libpath|view]
         [--launcher REL]                a foreign (CachyOS) program folder as a package tree with its libraries
  selftest DIR [--allow-copy]            round trip in a scratch folder (also the V2 check)
  snapshot DIR                           every entry below DIR (kind, mode, owner, size, hash, link): leftover checks
  refresh [--sources FILE]               fetch every source's database from its mirrors (Aktualisieren; PK-6a)
  sources [--sources FILE]               the sources in order: servers, packages, age (veraltet after 14 days)
  search QUERY [--sources FILE]          packages in the sources (Rust programs first among equal matches)
  info --repo [REPO/]NAME [--sources FILE]  a package as its source's database describes it
  update [NAME] [--check] [--sources FILE]  refresh, keyring and mirror list packages, then every program from
                                         the repositories re-resolved: a changed closure is a new version (only new
                                         packages are fetched), the old one stays previous; --check only reports
  plan install [REPO/]NAME [--sources FILE]  what installing a program would bring: packages, sizes, choices,
                                         Zusätze, refusals (nothing is downloaded; PK-6c)
  vercmp A B                             pacman's version order: -1, 0 or 1
  pgp verify FILE SIG [--keyring DIR]    check a detached OpenPGP signature against the pacman keyrings
  pgp verify-list LIST | pgp keys [--keyring DIR]
                                         many FILE<TAB>SIG pairs with one keyring load; every key's validity";

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let root = take_opt(&mut args, "--root").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/"));
    // PK-6a: `--sources FILE` may stand before the command too; the repository commands take it from the end.
    if let Some(f) = take_opt(&mut args, "--sources") {
        args.extend(["--sources".to_string(), f]);
    }
    let mut st = Store::new(&root);
    if let Some(s) = take_opt(&mut args, "--settings") {
        st.hooks = bitspkg::hooks::Hooks::standard((s != "none").then(|| PathBuf::from(s)));
    }
    let code = match run(&st, args) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("bitspkg: {e}");
            1
        }
    };
    exit(code);
}

fn take_opt(args: &mut Vec<String>, name: &str) -> Option<String> {
    let i = args.iter().position(|a| a == name)?;
    args.remove(i);
    (i < args.len()).then(|| args.remove(i))
}

fn take_flag(args: &mut Vec<String>, name: &str) -> bool {
    let n = args.len();
    args.retain(|a| a != name);
    args.len() != n
}

fn take_all(args: &mut Vec<String>, name: &str) -> Vec<String> {
    let mut v = Vec::new();
    while let Some(x) = take_opt(args, name) {
        v.push(x);
    }
    v
}

fn import(mut a: Vec<String>) -> Result<i32, String> {
    use bitspkg::foreign::{Import, Layout, import, read_libset};
    let origin = take_opt(&mut a, "--origin").map(PathBuf::from);
    let system = match take_opt(&mut a, "--system") {
        Some(p) => read_libset(Path::new(&p))?,
        None => Default::default(),
    };
    let layout = match take_opt(&mut a, "--layout").as_deref() {
        None | Some("libpath") => Layout::LibPath,
        Some("view") => Layout::View,
        Some(x) => return Err(format!("--layout {x}: want libpath or view")),
    };
    let opt = Import {
        origin,
        system,
        dlopen: take_all(&mut a, "--dlopen"),
        exclude: take_all(&mut a, "--exclude").into_iter().map(|x| x.trim_matches('/').to_string()).collect(),
        layout,
        launcher: take_opt(&mut a, "--launcher"),
    };
    if a.len() != 2 || a.iter().any(|x| x.starts_with("--")) {
        return usage();
    }
    for line in import(Path::new(&a[0]), Path::new(&a[1]), &opt)? {
        println!("{line}");
    }
    println!("imported {} into {}", a[0], a[1]);
    Ok(0)
}

fn usage() -> Result<i32, String> {
    eprintln!("{USAGE}");
    Ok(2)
}

fn run(st: &Store, mut a: Vec<String>) -> Result<i32, String> {
    if a.is_empty() {
        return usage();
    }
    let cmd = a.remove(0);
    if cmd == "import" {
        return import(a);
    }
    // PK-6d: `install NAME` (or `--from-repo`) is a repository install; a folder is a package folder as before.
    let repo_install = cmd == "install" && (a.iter().any(|x| x == "--from-repo") || a.iter().find(|x| !x.starts_with("--")).is_some_and(|x| !Path::new(x).is_dir()));
    let repo_info = cmd == "info" && a.iter().any(|x| x == "--repo" || x == "--scripts");
    if matches!(cmd.as_str(), "refresh" | "sources" | "search" | "vercmp" | "plan" | "update") || repo_info || repo_install {
        return bitspkg::repo::cli::run(st, &cmd, a);
    }
    if cmd == "pgp" {
        return bitspkg::pgp::cli(a, &st.root);
    }
    let spec = take_opt(&mut a, "--spec");
    let out = take_opt(&mut a, "--out");
    let version = take_opt(&mut a, "--version");
    let keep = take_flag(&mut a, "--keep-data");
    let store = take_flag(&mut a, "--store");
    let allow_copy = take_flag(&mut a, "--allow-copy");
    let apply = take_flag(&mut a, "--apply");
    let snap_data = take_opt(&mut a, "--snap-data");
    if let Some(x) = a.iter().find(|x| x.starts_with("--")) {
        return Err(format!("unknown option {x}"));
    }
    let arg = |i: usize| a.get(i).map(String::as_str);
    match (cmd.as_str(), a.len()) {
        ("build", 1) => {
            let m = build::build(Path::new(&a[0]), spec.as_deref().map(Path::new), out.as_deref().map(Path::new))?;
            println!("built {} {}: {} files, {} symlinks, {} bytes", m.name, m.version, m.files.len(), m.symlinks.len(), m.total_size());
        }
        ("check", 1) => {
            let m = install::read_source(Path::new(&a[0]))?;
            println!("ok {} {}", m.name, m.version);
        }
        ("install", 1) => {
            let r = install::install(st, Path::new(&a[0]))?;
            let m = &r.manifest;
            let how = if r.outcome == Outcome::AlreadyThere { "already installed, now current" } else { "installed" };
            let prev = r.previous.as_deref().map(|p| format!(", previous {p}")).unwrap_or_default();
            println!("{how}: {} {} (id {}{prev}; {} files shared, {} copied)", m.name, m.version, r.program_id, r.shared, r.copied);
            if r.hooks_failed > 0 {
                eprintln!("bitspkg: {} registration(s) failed (events above); bitspkg register {} repeats them", r.hooks_failed, m.name);
                return Ok(1);
            }
        }
        ("switch", 2) => {
            let (old, failed) = install::switch(st, &a[0], &a[1])?;
            println!("{} current {} (previous {})", a[0], a[1], old.as_deref().unwrap_or("-"));
            if failed > 0 {
                eprintln!("bitspkg: {failed} registration(s) failed (events above); bitspkg register {} repeats them", a[0]);
                return Ok(1);
            }
        }
        ("rollback", 1) => {
            let (from, to, failed) = install::rollback(st, &a[0])?;
            println!("{} back from {from} to {to}", a[0]);
            for l in bitspkg::repo::update::rollback_lines(st, &a[0], &from, &to) {
                println!("{l}");
            }
            if failed > 0 {
                eprintln!("bitspkg: {failed} registration(s) failed (events above); bitspkg register {} repeats them", a[0]);
                return Ok(1);
            }
        }
        ("remove", 1) => match version {
            Some(v) => {
                let freed = remove::remove_version(st, &a[0], &v)?;
                println!("removed {} {v} ({freed} bytes freed)", a[0]);
            }
            None => {
                let r = remove::remove(st, &a[0], keep)?;
                let data = if r.data_removed { ", data folder removed" } else if keep { ", data folder kept" } else { "" };
                println!("removed {} ({}{data}; {} .pkgs entries, {} bytes freed)", a[0], r.versions.join(" "), r.pkgs_gone, r.freed);
            }
        },
        ("register", 0 | 1) => {
            let failed = install::register(st, arg(0))?;
            if failed > 0 {
                return Err(format!("{failed} registration(s) failed (events above)"));
            }
            println!("registered");
        }
        ("list", 0) => {
            for i in verify::list(st)? {
                let id = i.program_id.map(|n| n.to_string()).unwrap_or_else(|| "-".into());
                let others: Vec<&str> = i.versions.iter().map(String::as_str).filter(|v| Some(*v) != i.current.as_deref() && Some(*v) != i.previous.as_deref()).collect();
                println!(
                    "{} current {} previous {} id {id}{}",
                    i.name,
                    i.current.as_deref().unwrap_or("-"),
                    i.previous.as_deref().unwrap_or("-"),
                    if others.is_empty() { String::new() } else { format!(" other {}", others.join(" ")) }
                );
            }
        }
        ("info", 1 | 2) => {
            let name = &a[0];
            bitspkg::names::check_name(name)?;
            let v = match arg(1) {
                Some(v) => v.to_string(),
                None => st.current(name).ok_or_else(|| format!("{name} is not installed"))?,
            };
            info(&st.manifest(name, &v)?);
        }
        ("verify", 0 | 1) => return bitspkg::report::verify(st, arg(0), store),
        ("space", 0 | 1) => return bitspkg::report::space(st, arg(0)),
        ("at", 2) => return bitspkg::report::at(st, &a[0], &a[1], apply, snap_data.as_deref()),
        ("gc", 0) => {
            let c = remove::gc(st)?;
            for (n, v) in &c.versions {
                println!("removed {n} {v}");
            }
            for n in &c.empty_packages {
                println!("removed unfinished {n}");
            }
            if c.pkgs_gone > 0 {
                println!("removed {} .pkgs entries no version uses", c.pkgs_gone);
            }
            println!("gc: {} bytes freed", c.freed);
        }
        ("snapshot", 1) => {
            for (k, v) in bitspkg::snapshot::take(Path::new(&a[0]))? {
                println!("{k} {v}");
            }
        }
        ("selftest", 1) => bitspkg::selftest::run(Path::new(&a[0]), allow_copy)?,
        _ => return usage(),
    }
    Ok(0)
}

fn info(m: &bitspkg::manifest::Manifest) {
    println!("{} {}", m.name, m.version);
    for (k, v) in [("de", &m.de), ("en", &m.en)] {
        if !v.is_empty() {
            println!("  {k}: {v}");
        }
    }
    if let Some(p) = &m.program {
        println!("  program: {p}");
    }
    println!("  files: {} ({} bytes), symlinks: {}", m.files.len(), m.total_size(), m.symlinks.len());
    for p in &m.permissions {
        println!("  wants: {}", p.describe());
    }
    for x in &m.actions {
        println!("  action: app.{}.{} ({}) {}", m.name, x.name, x.safety, x.en);
    }
    for s in &m.services {
        println!("  service: {} = {} {} ({}, restart {})", s.name, s.exec, s.args.join(" "), s.class, s.restart);
    }
    for s in &m.settings {
        println!("  setting: {} ({})", s.def.name, s.def.ty.name());
    }
}
