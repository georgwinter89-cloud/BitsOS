//! UPD-4: the rules of a version's parts (`\BITSOS2\bin` and every program tree), one file built twice: by the
//! kernel's install node (`crates/kexec/src/install`) and by bitsupdate (`#[path]`), so both judge a part's name,
//! a path in it, its list and a slot's `FILES` the same way. Pure text; each side hashes with its own SHA-256.
//!
//! A part's content hash is the SHA-256 of its list: one line `<sha256 hex> <size> <path>\n` per file, in byte
//! order of the paths, each path checked by [`List::add`]. The part lies in `\BITSOS2\SYS\STORE\<name>.<16 hex>\`
//! (the first 16 hex digits of the hash) with its record `<name>.<16 hex>.PART` beside it (the whole hash); a slot
//! names the parts it runs with in `\BITSOS2\SYS\<n>\FILES`, and the kernel binds each over `\BITSOS2\<name>`
//! when that slot starts.

use alloc::collections::BTreeSet;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

/// The part that is `\BITSOS2\bin` (flat: no folders in it).
pub const BIN: &str = "bin";
/// `\BITSOS2\SYS\STORE`: the parts of every kept slot.
pub const STORE: &str = "STORE";
/// A slot's list of parts, `\BITSOS2\SYS\<n>\FILES`.
pub const FILES: &str = "FILES";
const FILES_HEAD: &str = "bitsos-files 1";
const RECORD_HEAD: &str = "bitsos-part 1";
/// A part's name: a folder name the desk chooses (`mesa`, `brave`, `bin`).
pub const NAME_MAX: usize = 64;
/// FAT's longest name, and Linux's PATH_MAX for a whole path.
pub const COMPONENT_MAX: usize = 255;
pub const PATH_MAX: usize = 4096;
/// The most bytes one directory entry of a file takes with its long name (20 entries of 32 bytes).
const ENTRY_MAX: u64 = 640;

/// Names below `\BITSOS2\` that belong to the medium itself (fs-fat protect.rs, the stick tool, the update source).
const RESERVED: &[&str] = &["sys", "crash", "home", "mcp", "settings", "update", "efi"];

pub fn name_ok(n: &str) -> Result<(), String> {
    let b = n.as_bytes();
    let first = b.first().is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit());
    if !first || b.len() > NAME_MAX || !b.iter().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-' || *c == b'_') {
        return Err(format!("part name {n:?}: lower-case letters, digits, - and _, at most {NAME_MAX}, starting with a letter or digit"));
    }
    if RESERVED.contains(&n) {
        return Err(format!("part name {n:?}: the medium's own folder"));
    }
    Ok(())
}

/// 64 lower-case hex digits.
pub fn hex_ok(h: &str) -> bool {
    h.len() == 64 && h.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// The part's folder in `\BITSOS2\SYS\STORE\` (`hash`: 64 hex digits, checked by the caller).
pub fn folder(name: &str, hash: &str) -> String {
    format!("{name}.{}", &hash[..16.min(hash.len())])
}

/// Its record beside the folder: present only once every file is in and the hash was checked.
pub fn record_name(name: &str, hash: &str) -> String {
    format!("{}.PART", folder(name, hash))
}

/// The folder a part being written lies in until it is sealed.
pub fn new_folder(name: &str, hash: &str) -> String {
    format!("{}.NEW", folder(name, hash))
}

/// One path in a part: below its folder only (no `..`, no root, no backslash or drive), names FAT can hold, no
/// trailing dot or space (FAT drops them, two names would become one). `flat`: one component (the bin part).
pub fn path_ok(p: &str, flat: bool) -> Result<(), String> {
    if p.is_empty() || p.len() > PATH_MAX {
        return Err(format!("a path of {} bytes (1 to {PATH_MAX})", p.len()));
    }
    let comps: Vec<&str> = p.split('/').collect();
    if flat && comps.len() != 1 {
        return Err(format!("{p:?}: the bin part has no folders"));
    }
    for c in &comps {
        let bad = |why: &str| Err(format!("{p:?}: {why}"));
        if c.is_empty() {
            return bad("an empty name (a leading, doubled or trailing /)");
        }
        if *c == "." || *c == ".." {
            return bad("names . and .. are refused");
        }
        if c.len() > COMPONENT_MAX {
            return bad("a name longer than 255 bytes");
        }
        if c.chars().any(|ch| (ch as u32) < 0x20 || ch == '\u{7f}' || "\\:*?\"<>|".contains(ch)) {
            return bad("a character FAT cannot hold (\\ : * ? \" < > | or a control character)");
        }
        if c.ends_with('.') || c.ends_with(' ') {
            return bad("a name ending in a dot or a space");
        }
    }
    Ok(())
}

/// FAT compares names without case: the key two names collide on.
fn fold(p: &str) -> String {
    p.to_ascii_uppercase()
}

/// A part's list while it is read or written: paths in byte order, each once (FAT's case folding counted), no
/// file where a folder is or the other way round.
#[derive(Default)]
pub struct List {
    last: Option<String>,
    files: BTreeSet<String>,
    dirs: BTreeSet<String>,
    pub count: u64,
    pub bytes: u64,
}

impl List {
    /// Takes one file; its line is `line(sha, size, path)`.
    pub fn add(&mut self, path: &str, flat: bool, size: u64) -> Result<(), String> {
        path_ok(path, flat)?;
        if self.last.as_deref().is_some_and(|l| l >= path) {
            return Err(format!("{path:?} after {:?}: the list is in byte order, each path once", self.last.as_deref().unwrap_or("")));
        }
        let key = fold(path);
        if self.files.contains(&key) || self.dirs.contains(&key) {
            return Err(format!("{path:?}: another file or folder of the same name (FAT ignores case)"));
        }
        let mut parents = Vec::new();
        for (i, _) in path.match_indices('/') {
            let d = fold(&path[..i]);
            if self.files.contains(&d) {
                return Err(format!("{path:?}: a file of that name is there already, not a folder"));
            }
            parents.push(d);
        }
        self.bytes = self.bytes.checked_add(size).ok_or_else(|| String::from("the part's sizes overflow"))?;
        self.count += 1;
        self.dirs.extend(parents);
        self.files.insert(key);
        self.last = Some(path.to_string());
        Ok(())
    }
    pub fn dirs(&self) -> u64 {
        self.dirs.len() as u64
    }
}

pub fn line(sha_hex: &str, size: u64, path: &str) -> String {
    format!("{sha_hex} {size} {path}\n")
}

/// The most a part takes on a FAT volume of `cluster`-byte clusters: each file rounded up to whole clusters,
/// each folder at least one cluster, every entry with its longest long name, one cluster more.
pub fn on_medium(files: u64, dirs: u64, bytes: u64, cluster: u64) -> u64 {
    let c = cluster.max(512);
    let entries = files.saturating_add(dirs).saturating_mul(ENTRY_MAX).div_ceil(c).saturating_mul(c);
    bytes.saturating_add(files.saturating_mul(c)).saturating_add(dirs.saturating_mul(c)).saturating_add(entries).saturating_add(c)
}

/// `FILES`: the parts a slot runs with, (name, hash) sorted by name, each once.
pub fn files_render(parts: &[(String, String)]) -> String {
    let mut t = String::from(FILES_HEAD);
    t.push('\n');
    for (n, h) in parts {
        t.push_str(&format!("part {n} {h}\n"));
    }
    t
}

pub fn files_parse(b: &[u8]) -> Result<Vec<(String, String)>, String> {
    let t = core::str::from_utf8(b).map_err(|_| String::from("FILES is no UTF-8 text"))?;
    let mut lines = t.lines();
    if lines.next() != Some(FILES_HEAD) {
        return Err(format!("FILES does not start with {FILES_HEAD:?}"));
    }
    let mut out: Vec<(String, String)> = Vec::new();
    for l in lines {
        let w: Vec<&str> = l.split(' ').collect();
        let ["part", n, h] = w.as_slice() else { return Err(format!("FILES: unknown line {l:?}")) };
        name_ok(n)?;
        if !hex_ok(h) {
            return Err(format!("FILES: {n}: bad hash"));
        }
        if out.last().is_some_and(|(p, _)| p.as_str() >= *n) {
            return Err(format!("FILES: {n} out of order or twice"));
        }
        out.push((n.to_string(), h.to_string()));
    }
    if files_render(&out).as_bytes() != b {
        return Err(String::from("FILES is not in its one written form"));
    }
    Ok(out)
}

/// A sealed part's record: (name, hash, files, bytes).
pub fn record_render(name: &str, hash: &str, files: u64, bytes: u64) -> String {
    format!("{RECORD_HEAD}\nname {name}\nhash {hash}\nfiles {files}\nbytes {bytes}\n")
}

pub fn record_parse(b: &[u8]) -> Result<(String, String, u64, u64), String> {
    let t = core::str::from_utf8(b).map_err(|_| String::from("a record is no UTF-8 text"))?;
    let mut w = t.lines();
    let field = |l: Option<&str>, k: &str| l.and_then(|l| l.strip_prefix(k)).and_then(|v| v.strip_prefix(' ')).map(str::to_string);
    if w.next() != Some(RECORD_HEAD) {
        return Err(String::from("not a part record"));
    }
    let (name, hash, files, bytes) = (field(w.next(), "name"), field(w.next(), "hash"), field(w.next(), "files"), field(w.next(), "bytes"));
    let (Some(name), Some(hash), Some(files), Some(bytes)) = (name, hash, files.and_then(|f| f.parse().ok()), bytes.and_then(|b| b.parse().ok())) else {
        return Err(String::from("a part record lacks a line"));
    };
    name_ok(&name)?;
    if !hex_ok(&hash) || record_render(&name, &hash, files, bytes).as_bytes() != b {
        return Err(String::from("a part record is not in its one written form"));
    }
    Ok((name, hash, files, bytes))
}
