//! UPD-4, the desk host's half of `bitsupdate`: `manifest2` writes a version's form-2 manifest over the files
//! `cargo xtask update-release` put into the release folder (every `bin.<name>`, every tree piece
//! `tree.<name>.<k>.tar.zst`), each tree's content hash computed here from its unpacked files (the definition the
//! kernel checks, `parts_rules`) and its pieces renamed with it; `verify2` checks the whole set as BitsOS will.

use bitsupdate::manifest::{self, Entry, Manifest, Tree};
use bitsupdate::parts_rules as rules;
use bitsupdate::{partsio, verify};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

fn entry_of(name: &str, p: &Path) -> Result<Entry, String> {
    let b = std::fs::read(p).map_err(|e| format!("{}: {e}", p.display()))?;
    Ok(Entry { name: name.to_string(), size: b.len() as u64, sha256: verify::sha256(&b) })
}

/// `tree.<name>.<k>.tar.zst` (before `manifest2` named it with its hash): (name, k).
fn staged(file: &str) -> Option<(String, usize)> {
    let rest = file.strip_prefix("tree.")?.strip_suffix(".tar.zst")?;
    let (name, k) = rest.split_once('.')?;
    let k: usize = k.parse().ok().filter(|&k| k > 0)?;
    (rules::name_ok(name).is_ok() && name != rules::BIN).then(|| (name.to_string(), k))
}

pub fn manifest2(version: &str, dir: &str, date: &str, min: Option<&str>) -> Result<(), String> {
    let num = |s: &str| s.parse::<u32>().map_err(|_| format!("{s:?} is no number"));
    let d = Path::new(dir);
    let mut files = Vec::new();
    for (name, _) in manifest::FILES {
        let p = d.join(name);
        if p.exists() {
            files.push(entry_of(name, &p)?);
        }
    }
    let mut names: Vec<String> = std::fs::read_dir(d).map_err(|e| format!("{dir}: {e}"))?.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
    names.sort();
    let mut bins = Vec::new();
    let mut trees: BTreeMap<String, BTreeMap<usize, PathBuf>> = BTreeMap::new();
    for n in &names {
        if let Some(b) = n.strip_prefix("bin.") {
            bins.push(entry_of(b, &d.join(n))?);
        } else if let Some((t, k)) = staged(n) {
            trees.entry(t).or_default().insert(k, d.join(n));
        } else if n.starts_with("tree.") {
            return Err(format!("{n}: not a tree piece of this tool (tree.<name>.<k>.tar.zst)"));
        }
    }
    let mut out = Vec::new();
    for (name, pieces) in trees {
        if pieces.keys().copied().ne(1..=pieces.len()) {
            return Err(format!("tree {name}: its pieces are not 1 to {}", pieces.len()));
        }
        let paths: Vec<PathBuf> = pieces.into_values().collect();
        let (hash, nfiles, dirs, bytes) = partsio::scan(&paths).map_err(|e| format!("tree {name}: {e}"))?;
        let mut entries = Vec::new();
        for (i, p) in paths.iter().enumerate() {
            let asset = manifest::piece_asset(&name, &hash, i + 1);
            let e = entry_of(&asset, p)?;
            if e.size > manifest::ASSET_MAX {
                return Err(format!("{asset}: {} bytes, above GitHub's 2 GiB per asset", e.size));
            }
            std::fs::rename(p, d.join(&asset)).map_err(|x| format!("{asset}: {x}"))?;
            entries.push(e);
        }
        println!("tree {name}: {nfiles} files, {dirs} folders, {bytes} bytes, {} piece(s), hash {}", entries.len(), &hash[..16]);
        out.push(Tree { name, hash, files: nfiles, dirs, bytes, pieces: entries });
    }
    let m = Manifest { version: num(version)?, date: date.to_string(), min_previous: min.map(num).transpose()?, files, bins, trees: out };
    let t = m.render();
    Manifest::parse(t.as_bytes()).map_err(|e| format!("the manifest would not read back: {e}"))?;
    std::fs::write(d.join(manifest::MANIFEST2), &t).map_err(|e| format!("MANIFEST2: {e}"))?;
    println!("manifest2 of version {} with {} files, {} in bin, {} trees", m.version, m.files.len(), m.bins.len(), m.trees.len());
    Ok(())
}

/// MANIFEST2 and every part's asset checked as BitsOS checks them (the trees unpacked again); its files lines the
/// ones of MANIFEST (`m1`, checked already).
pub fn verify2(key: &[u8; 32], d: &Path, m1: &Manifest) -> Result<String, String> {
    let read = |n: &str| std::fs::read(d.join(n)).map_err(|e| format!("{n}: {e}"));
    let m = verify::manifest(Some(key), &read(manifest::MANIFEST2)?, &read(manifest::SIGNATURE2)?, m1.version, 0, u32::MAX).map_err(|w| format!("MANIFEST2: {w:?}"))?;
    if m.files != m1.files || m.date != m1.date || m.min_previous != m1.min_previous {
        return Err(String::from("MANIFEST2 names other files than MANIFEST"));
    }
    if !m.has_parts() {
        return Err(String::from("MANIFEST2 has no parts"));
    }
    let check = |e: &Entry, asset: &str| -> Result<(), String> {
        bitsupdate::partsio::check_file(&Entry { name: asset.to_string(), ..e.clone() }, &d.join(asset)).map_err(|f| format!("{asset}: {f:?}"))
    };
    for e in &m.bins {
        check(e, &manifest::bin_asset(&e.name))?;
    }
    for t in &m.trees {
        for e in &t.pieces {
            check(e, &e.name)?;
        }
        let got = partsio::scan(&t.pieces.iter().map(|e| d.join(&e.name)).collect::<Vec<_>>())?;
        if got != (t.hash.clone(), t.files, t.dirs, t.bytes) {
            return Err(format!("tree {}: its pieces unpack to something else than MANIFEST2 says", t.name));
        }
    }
    let bytes: u64 = m.bins.iter().map(|e| e.size).chain(m.trees.iter().flat_map(|t| t.pieces.iter().map(|e| e.size))).sum();
    Ok(format!("verified: MANIFEST2 with {} files in bin and {} trees, {bytes} bytes of parts", m.bins.len(), m.trees.len()))
}

