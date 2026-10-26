//! PK-2's leftover check: everything below a folder (kind, mode, owner, size, content hash, link target), so a
//! test can say "the tree after the removal is the tree before the install", byte for byte.

use crate::fsx::err;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

pub type Snapshot = BTreeMap<String, String>;

pub fn take(dir: &Path) -> Result<Snapshot, String> {
    let mut out = Snapshot::new();
    walk(dir, "", &mut out)?;
    Ok(out)
}

fn walk(dir: &Path, rel: &str, out: &mut Snapshot) -> Result<(), String> {
    let rd = match fs::read_dir(dir) {
        Ok(r) => r,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && rel.is_empty() => return Ok(()),
        Err(e) => return Err(err(dir, e)),
    };
    for e in rd {
        let e = e.map_err(|e| err(dir, e))?;
        let p = e.path();
        let name = e.file_name().to_string_lossy().into_owned();
        let r = if rel.is_empty() { name } else { format!("{rel}/{name}") };
        let md = fs::symlink_metadata(&p).map_err(|x| err(&p, x))?;
        let ft = md.file_type();
        let head = format!("{:o} {}:{}", md.mode() & 0o7777, md.uid(), md.gid());
        let what = if ft.is_symlink() {
            format!("link {head} -> {}", fs::read_link(&p).map_err(|x| err(&p, x))?.display())
        } else if ft.is_dir() {
            format!("dir {head}")
        } else if ft.is_file() {
            format!("file {head} {} {}", md.len(), hash(&p)?)
        } else {
            format!("other {head}")
        };
        out.insert(r.clone(), what);
        if ft.is_dir() {
            walk(&p, &r, out)?;
        }
    }
    Ok(())
}

fn hash(p: &Path) -> Result<String, String> {
    let mut f = fs::File::open(p).map_err(|e| err(p, e))?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 64 << 10];
    loop {
        let n = f.read(&mut buf).map_err(|e| err(p, e))?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

/// What differs between two snapshots (one line per path); `allow` names paths that may change or appear.
pub fn diff(before: &Snapshot, after: &Snapshot, allow: &[&str]) -> Vec<String> {
    let mut d = Vec::new();
    for (k, v) in before {
        match after.get(k) {
            None => d.push(format!("gone: {k} ({v})")),
            Some(w) if w != v && !allow.contains(&k.as_str()) => d.push(format!("changed: {k}: {v} -> {w}")),
            _ => {}
        }
    }
    for (k, v) in after {
        if !before.contains_key(k) && !allow.contains(&k.as_str()) {
            d.push(format!("left over: {k} ({v})"));
        }
    }
    d
}
