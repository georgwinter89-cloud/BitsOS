//! `bitsdisk umzug` (GP-DISK): the stick's old data (read-only at `/stickdata` once `/data` lies on the SSD's window)
//! copied to the new `/data` once, checked, and marked, typed by Georg at the console. The same copy and the same
//! marker as the desk's `data_move` (crates/drivers/nvme tool.rs, host/tests/minipc/data_move.py), without the desk:
//! the Gaming-PC's desk is a remote machine under Windows. The stick's image is only read.
//!
//! What counts as the same (as data_move.py compares): kind, permission bits, owner, group, size and contents of a
//! file, a link's target, the modification time in seconds (not on a link itself: BusyBox's `cp -a` keeps none
//! there). An entry of `/data` modified since the copy began was written by a running service (the journal): it is
//! named, not a difference.

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Read};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

pub const MARKER: &str = ".stickdata-moved";
const MARKER_TEXT: &[u8] = b"moved from /stickdata\n";

/// One entry as compared.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Facts {
    kind: char,
    mode: u32,
    uid: u32,
    gid: u32,
    size: u64,
    mtime: i64,
    link: Option<PathBuf>,
}

fn facts(p: &Path) -> io::Result<Facts> {
    let m = fs::symlink_metadata(p)?;
    let t = m.file_type();
    let kind = if t.is_symlink() { 'l' } else if t.is_dir() { 'd' } else if t.is_file() { 'f' } else { 'o' };
    let link = if t.is_symlink() { Some(fs::read_link(p)?) } else { None };
    Ok(Facts { kind, mode: m.mode() & 0o7777, uid: m.uid(), gid: m.gid(), size: if kind == 'f' { m.len() } else { 0 }, mtime: m.mtime(), link })
}

/// Every entry below `root` by its path relative to it ("" is `root` itself).
fn walk(root: &Path) -> io::Result<BTreeMap<PathBuf, Facts>> {
    let mut out = BTreeMap::new();
    let mut todo = vec![PathBuf::new()];
    while let Some(rel) = todo.pop() {
        let f = facts(&root.join(&rel))?;
        if f.kind == 'd' {
            for e in fs::read_dir(root.join(&rel))? {
                todo.push(rel.join(e?.file_name()));
            }
        }
        out.insert(rel, f);
    }
    Ok(out)
}

fn same_contents(a: &Path, b: &Path) -> io::Result<bool> {
    let (mut fa, mut fb) = (fs::File::open(a)?, fs::File::open(b)?);
    let (mut ba, mut bb) = (vec![0u8; 1 << 16], vec![0u8; 1 << 16]);
    loop {
        let n = fa.read(&mut ba)?;
        let mut got = 0;
        while got < n {
            let m = fb.read(&mut bb[got..n])?;
            if m == 0 {
                return Ok(false);
            }
            got += m;
        }
        if ba[..n] != bb[..n] {
            return Ok(false);
        }
        if n == 0 {
            return Ok(fb.read(&mut bb[..1])? == 0);
        }
    }
}

/// What the check found.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Outcome {
    /// Entries of the stick's tree found the same on `/data`.
    pub same: usize,
    /// Differences, one line each (path and what).
    pub differ: Vec<String>,
    /// Entries a running service changed on `/data` since the copy began.
    pub fresh: Vec<String>,
}

/// Every entry of `stick` against `data` (`start`: when the copy began, seconds since 1970).
pub fn compare(stick: &Path, data: &Path, start: i64) -> io::Result<Outcome> {
    let mut o = Outcome::default();
    for (rel, s) in walk(stick)? {
        if rel.as_os_str().is_empty() {
            continue;
        }
        let show = rel.display().to_string();
        let Ok(d) = facts(&data.join(&rel)) else {
            o.differ.push(format!("{show}: fehlt"));
            continue;
        };
        if d.mtime >= start && d.kind == s.kind {
            o.fresh.push(show);
            continue;
        }
        let mut what = Vec::new();
        for (bad, name) in [(s.kind != d.kind, "Art"), (s.mode != d.mode, "Rechte"), (s.uid != d.uid || s.gid != d.gid, "Besitzer"), (s.size != d.size, "Groesse"), (s.link != d.link, "Ziel")] {
            if bad {
                what.push(name);
            }
        }
        if s.kind != 'l' && s.mtime != d.mtime {
            what.push("Zeit");
        }
        if what.is_empty() && s.kind == 'f' && !same_contents(&stick.join(&rel), &data.join(&rel))? {
            what.push("Inhalt");
        }
        if what.is_empty() {
            o.same += 1;
        } else {
            o.differ.push(format!("{show}: {}", what.join(", ")));
        }
    }
    Ok(o)
}

/// Why the move does not start.
pub fn refuse(window_armed: bool, stick_mounted: bool, marker: bool) -> Option<&'static str> {
    if !window_armed {
        return Some("/data liegt nicht auf der SSD (kein Fenster offen). Erst bitsdisk, dann neu starten.");
    }
    if !stick_mounted {
        return Some("Die alten Daten vom Stick sind nicht da (/stickdata fehlt). Vom Stick starten, nicht von der SSD.");
    }
    if marker {
        return Some("Die Daten vom Stick wurden schon uebernommen. Es wurde nichts kopiert.");
    }
    None
}

/// The copy, the check, the marker. `copy` runs `cp -a <stick>/. <data>/` (the console: BusyBox's /bin/cp).
pub fn run(stick: &Path, data: &Path, start: i64, copy: &mut dyn FnMut() -> io::Result<bool>) -> io::Result<Result<Outcome, Outcome>> {
    if !copy()? {
        return Ok(Err(Outcome { differ: vec![String::from("das Kopieren selbst ist gescheitert")], ..Outcome::default() }));
    }
    let o = compare(stick, data, start)?;
    if !o.differ.is_empty() {
        return Ok(Err(o));
    }
    let m = data.join(MARKER);
    fs::write(&m, MARKER_TEXT)?;
    fs::File::open(&m)?.sync_all()?;
    Ok(Ok(o))
}

/// Georg's words for the result.
pub fn result_text(r: &Result<Outcome, Outcome>) -> String {
    match r {
        Ok(o) => {
            let mut t = format!("Fertig: {} Eintraege vom Stick sind auf /data, gleich wie auf dem Stick.\n", o.same);
            if !o.fresh.is_empty() {
                t.push_str(&format!("{} davon hat BitsOS seitdem selbst weitergeschrieben (das ist gut so): {}\n", o.fresh.len(), o.fresh.join(" ")));
            }
            t.push_str("Der Stick bleibt, wie er war. Die alten Daten liegen weiter nur zum Lesen unter /stickdata.\n");
            t
        }
        Err(o) => {
            let mut t = String::from("Nicht fertig: auf /data ist nicht alles gleich wie auf dem Stick. Nichts ist verloren, der Stick ist unveraendert.\n");
            for d in o.differ.iter().take(20) {
                t.push_str(&format!("  {d}\n"));
            }
            if o.differ.len() > 20 {
                t.push_str(&format!("  ... und {} mehr\n", o.differ.len() - 20));
            }
            t.push_str("Mach ein Foto davon fuer Claude.\n");
            t
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::process::Command;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("bitsdisk-umzug-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn tree(root: &Path) {
        fs::create_dir_all(root.join("home/1000/move/d/e")).unwrap();
        fs::write(root.join("home/1000/move/a"), b"eins\n").unwrap();
        fs::set_permissions(root.join("home/1000/move/a"), fs::Permissions::from_mode(0o640)).unwrap();
        fs::hard_link(root.join("home/1000/move/a"), root.join("home/1000/move/hard")).unwrap();
        symlink("a", root.join("home/1000/move/sym")).unwrap();
        fs::write(root.join("home/1000/move/d/e/big"), vec![7u8; 300_000]).unwrap();
        fs::create_dir_all(root.join("journal")).unwrap();
        fs::write(root.join("journal/system.log"), b"old\n").unwrap();
        // An old time, as the Mini-PC scenario sets one.
        Command::new("touch").args(["-h", "-d", "2021-03-04 05:06:07"]).arg(root.join("home/1000/move/a")).status().unwrap();
    }

    fn cp(stick: &Path, data: &Path) -> impl FnMut() -> io::Result<bool> {
        let (s, d) = (stick.join("."), data.to_path_buf());
        move || Ok(Command::new("cp").arg("-a").arg(&s).arg(&d).status()?.success())
    }

    #[test]
    fn the_move_copies_checks_and_marks_once() {
        let (stick, data) = (tmp("stick"), tmp("data"));
        tree(&stick);
        fs::create_dir_all(data.join("lost+found")).unwrap();
        let r = run(&stick, &data, i64::MAX, &mut cp(&stick, &data)).unwrap();
        let o = r.as_ref().expect("the same");
        assert!(o.same >= 9 && o.differ.is_empty(), "{o:?}");
        assert_eq!(fs::read(data.join(MARKER)).unwrap(), MARKER_TEXT);
        assert_eq!(fs::read(data.join("home/1000/move/d/e/big")).unwrap().len(), 300_000);
        assert!(result_text(&r).starts_with("Fertig: "));
        assert_eq!(refuse(true, true, data.join(MARKER).exists()), Some("Die Daten vom Stick wurden schon uebernommen. Es wurde nichts kopiert."));
        fs::remove_dir_all(&stick).unwrap();
        fs::remove_dir_all(&data).unwrap();
    }

    #[test]
    fn a_difference_is_named_and_no_marker_is_written() {
        let (stick, data) = (tmp("stick2"), tmp("data2"));
        tree(&stick);
        // A copy that drops a file and changes another's contents and rights.
        let mut bad = || {
            let ok = Command::new("cp").arg("-a").arg(stick.join(".")).arg(&data).status()?.success();
            fs::remove_file(data.join("home/1000/move/hard"))?;
            fs::write(data.join("home/1000/move/d/e/big"), vec![8u8; 300_000])?;
            Command::new("touch").args(["-r"]).arg(stick.join("home/1000/move/d/e/big")).arg(data.join("home/1000/move/d/e/big")).status()?;
            fs::set_permissions(data.join("home/1000/move/a"), fs::Permissions::from_mode(0o644))?;
            Command::new("touch").args(["-h", "-d", "2021-03-04 05:06:07"]).arg(data.join("home/1000/move/a")).status()?;
            Ok(ok)
        };
        let r = run(&stick, &data, i64::MAX, &mut bad).unwrap();
        let o = r.as_ref().unwrap_err();
        let mut d = o.differ.clone();
        d.sort();
        assert_eq!(d, ["home/1000/move/a: Rechte", "home/1000/move/d/e/big: Inhalt", "home/1000/move/hard: fehlt"]);
        assert!(!data.join(MARKER).exists(), "no marker after a difference");
        assert!(result_text(&r).starts_with("Nicht fertig"));
        // A failed copy: no check, no marker.
        let r = run(&stick, &data, i64::MAX, &mut || Ok(false)).unwrap();
        assert!(r.is_err() && !data.join(MARKER).exists());
        fs::remove_dir_all(&stick).unwrap();
        fs::remove_dir_all(&data).unwrap();
    }

    /// The journal goes on writing on the new /data while the copy runs: named, not a difference.
    #[test]
    fn what_a_service_wrote_since_is_not_a_difference() {
        let (stick, data) = (tmp("stick3"), tmp("data3"));
        tree(&stick);
        let start = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64;
        let mut copy = || {
            let ok = Command::new("cp").arg("-a").arg(stick.join(".")).arg(&data).status()?.success();
            fs::write(data.join("journal/system.log"), b"old\nnew line\n")?;
            Ok(ok)
        };
        let r = run(&stick, &data, start, &mut copy).unwrap();
        let o = r.as_ref().unwrap();
        assert!(o.fresh.iter().any(|f| f == "journal/system.log"), "{o:?}");
        assert!(data.join(MARKER).exists());
        fs::remove_dir_all(&stick).unwrap();
        fs::remove_dir_all(&data).unwrap();
    }

    #[test]
    fn it_starts_only_with_the_window_and_the_stick() {
        assert!(refuse(false, true, false).unwrap().contains("bitsdisk"));
        assert!(refuse(true, false, false).unwrap().contains("Vom Stick starten"));
        assert_eq!(refuse(true, true, false), None);
    }
}
