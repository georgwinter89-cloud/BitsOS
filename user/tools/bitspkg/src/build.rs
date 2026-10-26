//! build: a folder plus its `package.toml` (name, version, permissions, actions, services, settings) becomes a
//! package folder with a complete `manifest.toml` (every file with its hash, size and exec bit, every symlink,
//! every empty folder). Symlinks live in the manifest only, so a built package survives a copy over FAT.

use crate::fsx::{self, err};
use crate::manifest::{FILE_NAME, Manifest};
use crate::names::{check_link_target, check_rel_path};
use crate::store::read_manifest_text;
use crate::toml::{self, Table, Value};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

pub const SPEC_NAME: &str = "package.toml";

struct Walk {
    exclude: Vec<String>,
    files: Vec<(String, String, u64, bool)>,
    links: Vec<(String, String)>,
    dirs: Vec<String>,
    /// Entries that cannot be packaged, all reported together (a foreign tree has many at once).
    refused: Vec<String>,
}

/// How many refused entries one error lists (the rest are counted): bounds the message for a hostile tree.
const REFUSED_SHOWN: usize = 40;

pub fn build(tree: &Path, spec: Option<&Path>, out: Option<&Path>) -> Result<Manifest, String> {
    let spec_p = spec.map(Path::to_path_buf).unwrap_or_else(|| tree.join(SPEC_NAME));
    let mut doc = toml::parse(&read_manifest_text(&spec_p)?).map_err(|e| err(&spec_p, e))?;
    if let Some((k, t)) = doc.arrays.iter().find(|(k, _)| matches!(k.as_str(), "file" | "symlink" | "dir")) {
        return Err(format!("{} line {}: [[{k}]] entries are made by build, not written by hand", spec_p.display(), t.line));
    }
    let mut w = Walk { exclude: take_exclude(&mut doc, &spec_p)?, files: Vec::new(), links: Vec::new(), dirs: Vec::new(), refused: Vec::new() };
    walk(tree, "", &mut w)?;
    if !w.refused.is_empty() {
        let n = w.refused.len();
        let mut msg = format!("{n} entr{} of {} cannot be packaged:", if n == 1 { "y" } else { "ies" }, tree.display());
        for r in w.refused.iter().take(REFUSED_SHOWN) {
            msg.push_str("\n  ");
            msg.push_str(r);
        }
        if n > REFUSED_SHOWN {
            msg.push_str(&format!("\n  ... and {} more", n - REFUSED_SHOWN));
        }
        msg.push_str("\nhint: list them in package.toml `exclude = [...]`, or rewrite a symlink as a relative link inside \
                      the tree (plan/PACKAGES.md section 7)");
        return Err(msg);
    }
    let s = |v: &str| Value::Str(v.to_string());
    let entry = |e: Vec<(&str, Value)>| Table { line: 0, entries: e.into_iter().map(|(k, v)| (k.to_string(), v)).collect() };
    let mut arrays = Vec::new();
    for (p, sha, size, exec) in &w.files {
        let mut e = vec![("path", s(p)), ("sha256", s(sha)), ("size", Value::Int(*size as i64))];
        if *exec {
            e.push(("exec", Value::Bool(true)));
        }
        arrays.push(("file".to_string(), entry(e)));
    }
    for (p, t) in &w.links {
        arrays.push(("symlink".to_string(), entry(vec![("path", s(p)), ("target", s(t))])));
    }
    for d in &w.dirs {
        arrays.push(("dir".to_string(), entry(vec![("path", s(d))])));
    }
    arrays.append(&mut doc.arrays);
    doc.arrays = arrays;
    let m = Manifest::from_doc(&doc)?;
    match out {
        None => fsx::write_atomic(&tree.join(FILE_NAME), m.to_text().as_bytes(), 0o644)?,
        Some(o) => {
            fs::create_dir(o).map_err(|e| err(o, e))?;
            for d in &m.dirs {
                fs::create_dir_all(o.join(d)).map_err(|e| err(o, e))?;
            }
            for f in &m.files {
                let dst = o.join(&f.path);
                if let Some(parent) = dst.parent() {
                    fs::create_dir_all(parent).map_err(|e| err(parent, e))?;
                }
                let (sha, _) = fsx::copy_hashed(&tree.join(&f.path), &dst, if f.exec { 0o755 } else { 0o644 })?;
                if sha != f.sha256 {
                    return Err(format!("{}: changed while building", f.path));
                }
            }
            fsx::write_atomic(&o.join(FILE_NAME), m.to_text().as_bytes(), 0o644)?;
        }
    }
    Ok(m)
}

/// `exclude = ["cron", "chrome-sandbox"]` in package.toml: paths of the tree (files or whole folders) that stay
/// out of the package. A build-only key, so it is taken out before the manifest check.
fn take_exclude(doc: &mut toml::Doc, spec: &Path) -> Result<Vec<String>, String> {
    let Some(i) = doc.root.entries.iter().position(|(k, _)| k == "exclude") else { return Ok(Vec::new()) };
    let (_, v) = doc.root.entries.remove(i);
    let bad = || format!("{}: exclude must be an array of package paths", spec.display());
    let Value::Arr(a) = v else { return Err(bad()) };
    a.into_iter()
        .map(|x| match x {
            Value::Str(p) => check_rel_path(&p).map(|_| p),
            _ => Err(bad()),
        })
        .collect()
}

fn walk(dir: &Path, rel: &str, w: &mut Walk) -> Result<(), String> {
    let mut names: Vec<_> = fs::read_dir(dir).map_err(|e| err(dir, e))?.collect::<Result<_, _>>().map_err(|e| err(dir, e))?;
    names.sort_by_key(|e| e.file_name());
    let empty = names.is_empty();
    for e in names {
        let p = e.path();
        let Some(n) = e.file_name().to_str().map(str::to_string) else {
            w.refused.push(format!("{}: name is not UTF-8", p.display()));
            continue;
        };
        if rel.is_empty() && (n == FILE_NAME || n == SPEC_NAME) {
            continue;
        }
        let r = if rel.is_empty() { n } else { format!("{rel}/{n}") };
        if w.exclude.contains(&r) {
            continue;
        }
        if let Err(e) = check_rel_path(&r) {
            w.refused.push(e);
            continue;
        }
        let md = fs::symlink_metadata(&p).map_err(|e| err(&p, e))?;
        let mode = md.permissions().mode();
        if md.file_type().is_symlink() {
            match fsx::read_link(&p) {
                None => w.refused.push(format!("{r}: unreadable symlink")),
                Some(t) => match check_link_target(&r, &t) {
                    Ok(()) => w.links.push((r, t)),
                    Err(e) => w.refused.push(e),
                },
            }
        } else if md.is_dir() {
            walk(&p, &r, w)?;
        } else if md.is_file() && mode & 0o6000 != 0 {
            // Dropping the bit silently would make e.g. a setuid sandbox helper fail later with a confusing error.
            w.refused.push(format!("{r}: setuid/setgid file (packages never carry setuid)"));
        } else if md.is_file() {
            let (sha, size) = fsx::hash_file(&p)?;
            w.files.push((r, sha, size, mode & 0o111 != 0));
        } else {
            w.refused.push(format!("{r}: not a file, folder or symlink (device, pipe and socket files cannot be packaged)"));
        }
    }
    if empty && !rel.is_empty() {
        w.dirs.push(rel.to_string());
    }
    Ok(())
}
