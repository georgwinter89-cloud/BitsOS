//! The per-repository index `/data/bitspkg/sync/<repo>.idx` (plan 1.3): what search, info and (PK-6c) the
//! resolver need from a database, one line per package, so nothing reads the database tar again until it
//! changes. Its head names the database's SHA-256: an index whose database changed (or a cut between the two
//! renames) is rebuilt from the database. Fields are tab-separated; list items are separated by 0x1f; a tab,
//! newline, 0x1f or backslash inside a value is escaped.

use super::syncdb::{Db, Pkg};
use crate::fsx;
use std::collections::BTreeMap;
use std::path::Path;

const MAGIC: &str = "bitspkg-idx 1";
const SEP: char = '\u{1f}';

fn esc(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => o.push_str("\\\\"),
            '\t' => o.push_str("\\t"),
            '\n' => o.push_str("\\n"),
            '\r' => o.push_str("\\r"),
            SEP => o.push_str("\\u"),
            c => o.push(c),
        }
    }
    o
}

fn unesc(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    let mut it = s.chars();
    while let Some(c) = it.next() {
        if c != '\\' {
            o.push(c);
            continue;
        }
        match it.next() {
            Some('t') => o.push('\t'),
            Some('n') => o.push('\n'),
            Some('r') => o.push('\r'),
            Some('u') => o.push(SEP),
            Some(c) => o.push(c),
            None => {}
        }
    }
    o
}

fn list(v: &[String]) -> String {
    v.iter().map(|x| esc(x)).collect::<Vec<_>>().join(&SEP.to_string())
}

fn unlist(s: &str) -> Vec<String> {
    if s.is_empty() { Vec::new() } else { s.split(SEP).map(unesc).collect() }
}

pub struct Index {
    pub db_sha256: String,
    pub newest: i64,
    pub pkgs: BTreeMap<String, Pkg>,
}

pub fn render(db: &Db, db_sha256: &str) -> String {
    let mut o = format!("{MAGIC}\ndb_sha256 {db_sha256}\nnewest {}\ncount {}\n", db.newest(), db.pkgs.len());
    for p in db.pkgs.values() {
        let f = [
            esc(&p.name),
            esc(&p.version),
            esc(&p.base),
            esc(&p.desc),
            esc(&p.filename),
            p.csize.to_string(),
            p.isize.to_string(),
            esc(&p.sha256),
            esc(&p.pgpsig),
            esc(&p.arch),
            p.builddate.to_string(),
            esc(&p.url),
            esc(&p.packager),
            list(&p.groups),
            list(&p.licenses),
            list(&p.provides),
            list(&p.depends),
            list(&p.conflicts),
            list(&p.replaces),
            list(&p.optdepends),
            list(&p.makedepends),
        ];
        o.push_str(&f.join("\t"));
        o.push('\n');
    }
    o
}

pub fn write(path: &Path, db: &Db, db_sha256: &str) -> Result<(), String> {
    fsx::write_atomic(path, render(db, db_sha256).as_bytes(), 0o644)
}

pub fn parse(text: &str) -> Result<Index, String> {
    let mut lines = text.lines();
    if lines.next() != Some(MAGIC) {
        return Err("kein bitspkg-Index".into());
    }
    let mut head = |k: &str| -> Result<String, String> {
        let l = lines.next().unwrap_or("");
        l.strip_prefix(k).and_then(|r| r.strip_prefix(' ')).map(str::to_string).ok_or_else(|| format!("Index: {k} fehlt"))
    };
    let db_sha256 = head("db_sha256")?;
    let newest = head("newest")?.parse().map_err(|_| "Index: newest")?;
    let count: usize = head("count")?.parse().map_err(|_| "Index: count")?;
    let mut pkgs = BTreeMap::new();
    for l in lines {
        let f: Vec<&str> = l.split('\t').collect();
        if f.len() != 21 {
            return Err(format!("Index: Zeile mit {} Feldern", f.len()));
        }
        let n = |i: usize| -> Result<u64, String> { f[i].parse().map_err(|_| format!("Index: Feld {i}")) };
        let p = Pkg {
            name: unesc(f[0]),
            version: unesc(f[1]),
            base: unesc(f[2]),
            desc: unesc(f[3]),
            filename: unesc(f[4]),
            csize: n(5)?,
            isize: n(6)?,
            sha256: unesc(f[7]),
            pgpsig: unesc(f[8]),
            arch: unesc(f[9]),
            builddate: f[10].parse().map_err(|_| "Index: builddate")?,
            url: unesc(f[11]),
            packager: unesc(f[12]),
            groups: unlist(f[13]),
            licenses: unlist(f[14]),
            provides: unlist(f[15]),
            depends: unlist(f[16]),
            conflicts: unlist(f[17]),
            replaces: unlist(f[18]),
            optdepends: unlist(f[19]),
            makedepends: unlist(f[20]),
        };
        pkgs.insert(p.name.clone(), p);
    }
    if pkgs.len() != count {
        return Err(format!("Index: {count} Pakete angesagt, {} gelesen", pkgs.len()));
    }
    Ok(Index { db_sha256, newest, pkgs })
}
