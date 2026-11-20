//! A sync database (`<repo>.db`): a tar of `<name>-<version>/desc` files of `%KEY%` blocks
//! (pacman/lib/libalpm/be_sync.c:419 sync_db_populate, :561 sync_db_read; the writer is
//! pacman/scripts/repo-add.sh.in:116 format_entry and :301-328). Differences from pacman, all refusals: a field
//! over 64 KiB (pacman's line bound is 512 KiB, be_sync.c:579), a database over 256 MiB unpacked, a link or any
//! other non-file entry, two entries for one package with different contents, a name or version pacman's
//! _alpm_pkg_check_meta (package.c:850) refuses or a `%FILENAME%` with a path (be_sync.c:493). pacman logs an
//! unknown key and goes on; so does this (the key is dropped).

use super::tar::{Kind, Tar};
use super::unpack;
use std::collections::BTreeMap;
use std::io::Read;

pub const DB_MAX: u64 = 256 << 20;
pub const FIELD_MAX: usize = 64 << 10;
/// One `desc` file is read whole; the largest real one is a few KiB (the `.files` lists, PK-6d's, a few MiB).
pub const ENTRY_MAX: u64 = 32 << 20;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Pkg {
    pub name: String,
    pub version: String,
    pub base: String,
    pub desc: String,
    pub filename: String,
    pub csize: u64,
    pub isize: u64,
    pub sha256: String,
    /// `%PGPSIG%` as the database has it (base64 of the detached signature).
    pub pgpsig: String,
    pub arch: String,
    pub builddate: i64,
    pub url: String,
    pub packager: String,
    pub groups: Vec<String>,
    pub licenses: Vec<String>,
    pub provides: Vec<String>,
    pub depends: Vec<String>,
    pub conflicts: Vec<String>,
    pub replaces: Vec<String>,
    pub optdepends: Vec<String>,
    pub makedepends: Vec<String>,
}

impl Pkg {
    /// Rust first (DECISIONS 24.10.): built with rust or cargo.
    pub fn rust(&self) -> bool {
        self.makedepends.iter().any(|d| {
            let n = d.split(['<', '>', '=', ':']).next().unwrap_or("");
            n == "rust" || n == "cargo" || n == "rustup"
        })
    }
}

#[derive(Debug, Default)]
pub struct Db {
    /// By name (a sync database holds one version per name, be_sync.c load_pkg_for_entry).
    pub pkgs: BTreeMap<String, Pkg>,
    /// Lines pacman would log as warnings (unknown keys, files outside a package folder).
    pub warnings: Vec<String>,
}

impl Db {
    /// The newest `%BUILDDATE%`: the database's age for the freshness check (plan 1.3).
    pub fn newest(&self) -> i64 {
        self.pkgs.values().map(|p| p.builddate).max().unwrap_or(0)
    }
}

/// pacman's entry name split (util.c:1336 _alpm_splitname): the version is the last two '-' parts.
pub fn split_name(entry: &str) -> Option<(&str, &str)> {
    let dir = entry.split('/').next()?;
    let rel = dir.rfind('-')?;
    let ver = dir[..rel].rfind('-')?;
    if ver == 0 {
        return None;
    }
    Some((&dir[..ver], &dir[ver + 1..]))
}

/// package.c:850 _alpm_pkg_check_meta, strict: the characters pacman's own tools allow in a name.
pub fn check_name(n: &str) -> Result<(), String> {
    if n.is_empty() || n.starts_with(['-', '.']) || !n.bytes().all(|b| b.is_ascii_alphanumeric() || b"+_.@-".contains(&b)) {
        return Err(format!("ungültiger Paketname {n:?}"));
    }
    Ok(())
}

pub fn check_version(v: &str) -> Result<(), String> {
    let dashes = v.matches('-').count();
    if v.is_empty() || dashes > 1 || v.contains('/') || v.bytes().any(|b| b <= b' ' || b == 0x7f) {
        return Err(format!("ungültige Version {v:?}"));
    }
    Ok(())
}

/// Reads a database file's bytes (any compression) into packages.
pub fn parse(r: impl Read + 'static, repo: &str) -> Result<Db, String> {
    // ruzstd and miniz_oxide are meant never to panic on bad input; a database comes from a stranger's mirror,
    // so a panic in either still ends as a refusal (with its message), never as a crash of bitspkg.
    let repo2 = repo.to_string();
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || parse_inner(r, &repo2))) {
        Ok(r) => r,
        Err(_) => Err(format!("Datenbank {repo}: der Entpacker brach ab (beschädigte Daten)")),
    }
}

fn parse_inner(r: impl Read + 'static, repo: &str) -> Result<Db, String> {
    let r = unpack::open(r).map_err(|e| format!("Datenbank {repo}: {e}"))?;
    let mut tar = Tar::new(unpack::Bounded::new(r, DB_MAX, &format!("Datenbank {repo}")));
    let mut db = Db::default();
    let mut seen: BTreeMap<String, Pkg> = BTreeMap::new();
    let fail = |e: String| format!("Datenbank {repo}: {e}");
    while let Some(e) = tar.next_entry().map_err(fail)? {
        match e.kind {
            Kind::Dir => continue,
            Kind::File => {}
            Kind::Symlink(_) | Kind::HardLink(_) | Kind::Special(_) => return Err(fail(format!("{}: Verweis in einer Datenbank abgelehnt", e.path))),
        }
        let Some((dir, file)) = e.path.split_once('/') else {
            db.warnings.push(format!("unbekannte Datei {} in der Datenbank", e.path));
            continue;
        };
        if file.contains('/') {
            return Err(fail(format!("{}: zu tief verschachtelt", e.path)));
        }
        let (name, version) = split_name(dir).ok_or_else(|| fail(format!("ungültiger Eintrag {dir:?}")))?;
        check_name(name).map_err(fail)?;
        check_version(version).map_err(fail)?;
        if !matches!(file, "desc" | "depends" | "files") {
            db.warnings.push(format!("unbekannte Datei {}", e.path));
            continue;
        }
        let text = read_entry(&mut tar, &e.path, e.size).map_err(fail)?;
        let p = seen.entry(name.to_string()).or_insert_with(|| Pkg { name: name.into(), version: version.into(), ..Pkg::default() });
        if p.version != version {
            return Err(fail(format!("{name} steht zweimal in der Datenbank ({} und {version})", p.version)));
        }
        let before = p.clone();
        let mut fresh = p.clone();
        blocks(&text, &mut fresh, &mut db.warnings).map_err(|m| fail(format!("{}: {m}", e.path)))?;
        // pacman merges a second `desc` of the same package into the first; a database that carries one twice
        // with other contents is inconsistent (a doubled block), so it is refused instead.
        if file == "desc" && before.filename.len() + before.sha256.len() > 0 && fresh != before {
            return Err(fail(format!("{name}: Eintrag doppelt mit anderem Inhalt")));
        }
        *p = fresh;
    }
    for (n, p) in seen {
        if p.filename.is_empty() {
            return Err(fail(format!("{n}: %FILENAME% fehlt")));
        }
        db.pkgs.insert(n, p);
    }
    Ok(db)
}

fn read_entry<R: Read>(tar: &mut Tar<R>, path: &str, size: u64) -> Result<String, String> {
    if size > ENTRY_MAX {
        return Err(format!("{path}: {size} Bytes, mehr als {} MiB für eine Beschreibung", ENTRY_MAX >> 20));
    }
    let mut v = Vec::with_capacity(size as usize);
    let mut buf = [0u8; 16 << 10];
    loop {
        let n = tar.read_data(&mut buf)?;
        if n == 0 {
            break;
        }
        v.extend_from_slice(&buf[..n]);
    }
    String::from_utf8(v).map_err(|_| format!("{path}: kein UTF-8"))
}

fn num(v: &str, what: &str) -> Result<u64, String> {
    v.trim().parse().map_err(|_| format!("{what} ist keine Zahl ({v:?})"))
}

/// The `%KEY%` blocks of one file (be_sync.c:599-718): a key line, then its value lines up to an empty line.
fn blocks(text: &str, p: &mut Pkg, warn: &mut Vec<String>) -> Result<(), String> {
    let mut lines = text.split('\n').map(|l| l.strip_suffix('\r').unwrap_or(l)).peekable();
    while let Some(line) = lines.next() {
        if line.is_empty() {
            continue;
        }
        if line.len() > FIELD_MAX {
            return Err(format!("Zeile mit {} Bytes (höchstens {FIELD_MAX})", line.len()));
        }
        let mut vals = Vec::new();
        while let Some(&l) = lines.peek() {
            if l.is_empty() {
                break;
            }
            if l.len() > FIELD_MAX {
                return Err(format!("{line}: Wert mit {} Bytes (höchstens {FIELD_MAX})", l.len()));
            }
            vals.push(l.to_string());
            lines.next();
        }
        // READ_NEXT reads exactly one line for a single value (an empty one is an empty value there).
        let one = |vals: &[String]| -> Result<String, String> { Ok(vals.first().cloned().unwrap_or_default()) };
        match line {
            "%NAME%" => {
                if one(&vals)? != p.name {
                    warn.push(format!("{}: %NAME% passt nicht zum Eintrag", p.name));
                }
            }
            "%VERSION%" => {
                if one(&vals)? != p.version {
                    warn.push(format!("{}: %VERSION% passt nicht zum Eintrag", p.name));
                }
            }
            "%FILENAME%" => {
                let f = one(&vals)?;
                if f.starts_with('.') || f.contains('/') || f.len() > 4096 {
                    return Err(format!("unzulässiger Dateiname {f:?}"));
                }
                p.filename = f;
            }
            "%BASE%" => p.base = one(&vals)?,
            "%DESC%" => p.desc = one(&vals)?,
            "%URL%" => p.url = one(&vals)?,
            "%ARCH%" => p.arch = one(&vals)?,
            "%PACKAGER%" => p.packager = one(&vals)?,
            "%BUILDDATE%" => p.builddate = num(&one(&vals)?, "%BUILDDATE%")? as i64,
            "%CSIZE%" => p.csize = num(&one(&vals)?, "%CSIZE%")?,
            "%ISIZE%" => p.isize = num(&one(&vals)?, "%ISIZE%")?,
            "%SHA256SUM%" => {
                let s = one(&vals)?;
                if s.len() != 64 || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
                    return Err(format!("%SHA256SUM% ist keine SHA-256 ({s:?})"));
                }
                p.sha256 = s.to_ascii_lowercase();
            }
            "%PGPSIG%" => p.pgpsig = one(&vals)?,
            "%MD5SUM%" => {}
            "%GROUPS%" => p.groups = vals,
            "%LICENSE%" => p.licenses = vals,
            "%PROVIDES%" => p.provides = vals,
            "%DEPENDS%" => p.depends = vals,
            "%CONFLICTS%" => p.conflicts = vals,
            "%REPLACES%" => p.replaces = vals,
            "%OPTDEPENDS%" => p.optdepends = vals,
            "%MAKEDEPENDS%" => p.makedepends = vals,
            "%CHECKDEPENDS%" | "%DATA%" | "%FILES%" => {}
            _ => warn.push(format!("{}: unbekannter Schlüssel {line}", p.name)),
        }
    }
    Ok(())
}
