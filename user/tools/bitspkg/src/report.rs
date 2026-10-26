//! What `bitspkg verify`, `space` and `at` print (PK-5). One fact per line, `key value` where a program reads it
//! (VS-3, MCP), exact byte counts with a rounded form for people.

use crate::history::{self, SnapRoot, Step, Then};
use crate::space::{self, Size, human};
use crate::store::Store;
use crate::verify;
use std::path::Path;

pub fn verify(st: &Store, only: Option<&str>, store: bool) -> Result<i32, String> {
    let (bad, c) = verify::verify_counted(st, only, store)?;
    for b in &bad {
        println!("BAD {b}");
    }
    let what = format!(
        "{} packages, {} versions, {} files, {} store files, {} read",
        c.packages,
        c.versions,
        c.files,
        c.store_files,
        human(c.bytes_hashed)
    );
    if bad.is_empty() {
        println!("verify: all good ({what})");
        return Ok(0);
    }
    eprintln!("bitspkg: event=verify.bad problems={} (reported, nothing repaired)", bad.len());
    println!("verify: {} problems ({what})", bad.len());
    Ok(1)
}

fn sz(s: Size) -> String {
    format!("{} bytes, {} on disk ({})", s.bytes, s.disk, human(s.disk))
}

pub fn space(st: &Store, only: Option<&str>) -> Result<i32, String> {
    let (progs, s) = space::report(st, only)?;
    for p in &progs {
        println!("{}: package {}", p.name, sz(p.package));
        println!("  own {}", sz(p.own));
        println!("  shared with other programs {}", sz(p.shared));
        if p.copies > 0 {
            println!("  copies not linked to the store: {} files", p.copies);
        }
        for v in &p.versions {
            println!("  version {} {}: {}; only this version {}", v.version, v.role, sz(v.size), sz(v.alone));
        }
        match &p.data {
            None => println!("  data none"),
            Some(d) => {
                println!("  data {}", sz(d.size));
                println!("  cache {}{}", sz(d.cache), if d.cache_dirs.is_empty() { String::new() } else { format!(" in {}", d.cache_dirs.join(", ")) });
                if d.unreadable > 0 {
                    println!("  data unreadable entries {} (sizes too low; run as root)", d.unreadable);
                }
            }
        }
    }
    if only.is_none() {
        println!("store: {} files, {}", s.files, sz(s.size));
        println!("store unused: {} files, {} (bitspkg gc frees it)", s.unused_files, sz(s.unused));
        println!("trash: {}", sz(s.trash));
        for (n, d) in &s.orphan_data {
            println!("data without program: {n} {}", sz(*d));
        }
    }
    Ok(0)
}

/// `snap_data`: where the data folders are inside the snapshot when not `<mnt>/appdata` (a test store below a
/// folder of the volume: `<mnt>/<folder>/data/appdata`).
pub fn at(st: &Store, mnt: &str, name: &str, apply: bool, snap_data: Option<&str>) -> Result<i32, String> {
    let mut sr = SnapRoot::volume(Path::new(mnt));
    if let Some(d) = snap_data {
        sr.data = Path::new(d).to_path_buf();
    }
    let at = history::program_at(&sr, st, name)?;
    let opt = |o: &Option<String>| o.clone().unwrap_or_else(|| "-".into());
    println!("program {}", at.name);
    match &at.then {
        Then::Unknown(why) => println!("then unknown {why}"),
        Then::NotInstalled => println!("then not-installed"),
        Then::Installed { current, previous, versions, program_id, current_dir, problem } => {
            println!("then installed");
            println!("then.current {}", opt(current));
            println!("then.previous {}", opt(previous));
            println!("then.versions {}", versions.join(" "));
            println!("then.id {}", program_id.map(|i| i.to_string()).unwrap_or_else(|| "-".into()));
            println!("then.dir {}", current_dir.as_ref().map(|d| d.display().to_string()).unwrap_or_else(|| "-".into()));
            if let Some(p) = problem {
                println!("then.problem {p}");
            }
        }
    }
    println!("then.data {}", at.data_then.as_ref().map(|d| d.display().to_string()).unwrap_or_else(|| "-".into()));
    println!("now.current {}", opt(&at.current_now));
    println!("now.id {}", at.id_now.map(|i| i.to_string()).unwrap_or_else(|| "-".into()));
    println!("now.data {}", at.data_now.display());
    if at.id_changed() {
        println!("id.changed yes (restored data must belong to the live program's uid)");
    }
    match &at.step {
        Step::Nothing => println!("step nothing"),
        Step::Switch(v) => println!("step switch {v}"),
        Step::Install(d) => println!("step install {}", d.display()),
        Step::Remove => println!("step remove"),
        Step::Absent => println!("step absent"),
        Step::Blocked(why) => println!("step blocked {why}"),
    }
    if apply {
        println!("done {}", history::bring_back(st, &at)?);
    }
    Ok(0)
}
