//! UPD-4: the parts of a version for `update-release`: every file the image put into `\BITSOS2\bin` as one asset
//! `bin.<name>` each, and every program tree (and `firmware`) as tar + zstd pieces `tree.<name>.<k>.tar.zst`,
//! each piece holding at most `PIECE_RAW` bytes of files so it stays below GitHub's 2 GiB per asset. A tree's
//! files are the image's own (`V2_STICK_LIST`: what `xtask image` puts there, with its `PKGLIST.TXT`), packed in
//! byte order of their paths, which is the order BitsOS checks. `bitsupdate manifest2` hashes the trees from the
//! pieces and names them with the hash.

use crate::util::*;
use std::collections::BTreeMap;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Bytes of files in one piece: zstd's worst case adds a few hundred bytes per 128 KiB block, tar 512 per file, so
/// the piece stays well below 2 GiB - 1 (`bitsos_install::manifest::MAX_ASSET`).
pub const PIECE_RAW: u64 = 1800 << 20;

#[derive(Clone, Debug, PartialEq)]
pub enum Trees {
    All,
    None,
    Only(Vec<String>),
}

impl Trees {
    pub fn parse(v: &str) -> Trees {
        match v {
            "all" => Trees::All,
            "none" => Trees::None,
            l => Trees::Only(l.split(',').map(|t| t.trim().to_string()).filter(|t| !t.is_empty()).collect()),
        }
    }
    fn wants(&self, t: &str) -> bool {
        match self {
            Trees::All => true,
            Trees::None => false,
            Trees::Only(l) => l.iter().any(|n| n == t),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct PartsOpts {
    pub trees: Trees,
    /// `--bin-file NAME=FILE`: a file of `\BITSOS2\bin` replaced or added (the scenario's stand-in).
    pub bin_files: Vec<(String, PathBuf)>,
    /// `--extra-tree NAME=DIR`: a tree from a folder (the scenario's small fake tree).
    pub extra_trees: Vec<(String, PathBuf)>,
}

fn pair(v: &str, what: &str) -> Result<(String, PathBuf), String> {
    let (n, p) = v.split_once('=').ok_or_else(|| format!("{what} NAME=PATH, not {v:?}"))?;
    Ok((n.to_string(), PathBuf::from(p)))
}

pub fn parse(a: &mut Args) -> Result<PartsOpts, String> {
    let trees = a.value("--trees")?.map(|v| Trees::parse(&v)).unwrap_or(Trees::All);
    let bin_files = a.values("--bin-file")?.iter().map(|v| pair(v, "--bin-file")).collect::<Result<_, _>>()?;
    let extra_trees = a.values("--extra-tree")?.iter().map(|v| pair(v, "--extra-tree")).collect::<Result<_, _>>()?;
    Ok(PartsOpts { trees, bin_files, extra_trees })
}

/// A part's name as BitsOS takes it (`parts_rules::name_ok`, said again here: a tree the medium refuses never
/// gets packed).
pub fn tree_name_ok(n: &str) -> bool {
    let b = n.as_bytes();
    b.first().is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && b.len() <= 64
        && b.iter().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-' || *c == b'_')
        && !["sys", "crash", "home", "mcp", "settings", "update", "efi", "bin"].contains(&n)
}

/// A file of `\BITSOS2\bin` as an asset name GitHub keeps.
pub fn bin_name_ok(n: &str) -> bool {
    !n.is_empty() && !n.starts_with('.') && n.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
}

enum Src {
    File(PathBuf),
    Bytes(Vec<u8>),
}

fn walk(dir: &Path, rel: &str, out: &mut Vec<(String, Src)>) -> Result<(), String> {
    let mut es: Vec<_> = std::fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))?.flatten().collect();
    es.sort_by_key(|e| e.file_name());
    for e in es {
        let name = e.file_name().to_string_lossy().into_owned();
        let r = if rel.is_empty() { name } else { format!("{rel}/{name}") };
        let p = e.path();
        if std::fs::metadata(&p).map_err(|x| format!("{}: {x}", p.display()))?.is_dir() {
            walk(&p, &r, out)?;
        } else {
            out.push((r, Src::File(p)));
        }
    }
    Ok(())
}

/// The trees' files: (tree, path below it, source), from the image's stick list, the firmware the image wrote, and
/// the extra trees.
fn tree_files(esp: &Path, list: &str, o: &PartsOpts) -> Result<BTreeMap<String, Vec<(String, Src)>>, String> {
    let mut trees: BTreeMap<String, Vec<(String, Src)>> = BTreeMap::new();
    let mut lists: Vec<(String, PathBuf)> = Vec::new();
    for line in list.lines().filter(|l| !l.is_empty()) {
        let (rel, src) = line.split_once('\t').ok_or_else(|| format!("stick list: {line:?}"))?;
        if !crate::image::package::is_tree_file(rel) {
            continue;
        }
        let mut parts = rel.splitn(3, '/');
        let (Some("BITSOS2"), Some(t), Some(inner)) = (parts.next(), parts.next(), parts.next()) else { continue };
        if !o.trees.wants(t) {
            continue;
        }
        if !tree_name_ok(t) {
            return Err(format!("tree {t:?}: not a name BitsOS takes for a part"));
        }
        trees.entry(t.to_string()).or_default().push((inner.to_string(), Src::File(PathBuf::from(src))));
        lists.push((rel.to_string(), PathBuf::from(src)));
    }
    // The tree's own PKGLIST.TXT, as `xtask image` writes it beside the files.
    for (tree, text) in crate::image::tree_list_texts(lists.iter().map(|(r, p)| (r.as_str(), p.clone())))? {
        if let Some(t) = tree.strip_prefix("BITSOS2/")
            && let Some(v) = trees.get_mut(t)
        {
            v.push((String::from("PKGLIST.TXT"), Src::Bytes(text.into_bytes())));
        }
    }
    let fw = esp.join("BITSOS2/firmware");
    if o.trees.wants("firmware") && fw.is_dir() {
        walk(&fw, "", trees.entry(String::from("firmware")).or_default())?;
    }
    for (name, dir) in &o.extra_trees {
        if !tree_name_ok(name) || trees.contains_key(name) {
            return Err(format!("--extra-tree {name}: not a part name, or the tree exists already"));
        }
        walk(dir, "", trees.entry(name.clone()).or_default())?;
    }
    trees.retain(|_, v| !v.is_empty());
    Ok(trees)
}

/// Writes `files` (in this order) as one tar through `zstd` into `out`.
fn pack(out: &Path, files: &[&(String, Src)]) -> Result<(), String> {
    let tmp = out.with_extension("part");
    let mut z = Command::new("zstd")
        .args(["-q", "-f", "-T0", "-12", "-o"])
        .arg(&tmp)
        .stdin(Stdio::piped())
        .spawn()
        .map_err(|e| format!("cannot start zstd: {e}"))?;
    {
        let stdin = z.stdin.take().ok_or("zstd: no stdin")?;
        let mut w = bitsos_install::tar::Writer::new(BufWriter::with_capacity(1 << 20, stdin));
        for (path, src) in files {
            let err = |e: std::io::Error| format!("{}: {path}: {e}", out.display());
            match src {
                Src::File(p) => {
                    let f = std::fs::File::open(p).map_err(|e| format!("{}: {e}", p.display()))?;
                    let len = f.metadata().map_err(|e| format!("{}: {e}", p.display()))?.len();
                    w.file(path, 0o644, len, f).map_err(err)?;
                }
                Src::Bytes(b) => w.file(path, 0o644, b.len() as u64, &b[..]).map_err(err)?,
            }
        }
        w.finish().map_err(|e| format!("{}: {e}", out.display()))?.flush().map_err(|e| e.to_string())?;
    }
    let st = z.wait().map_err(|e| format!("zstd: {e}"))?;
    if !st.success() {
        return Err(format!("zstd failed ({st}) for {}", out.display()));
    }
    std::fs::rename(&tmp, out).map_err(|e| format!("{}: {e}", out.display()))
}

fn size(s: &Src) -> Result<u64, String> {
    match s {
        Src::File(p) => std::fs::metadata(p).map(|m| m.len()).map_err(|e| format!("{}: {e}", p.display())),
        Src::Bytes(b) => Ok(b.len() as u64),
    }
}

/// Consecutive pieces of at most `max` bytes of files each (a file above `max` gets a piece of its own, and the
/// caller's 2 GiB check refuses it if it is too large).
pub fn split(sizes: &[u64], max: u64) -> Vec<std::ops::Range<usize>> {
    let mut out = Vec::new();
    let (mut start, mut sum) = (0, 0u64);
    for (i, &s) in sizes.iter().enumerate() {
        if i > start && sum.saturating_add(s) > max {
            out.push(start..i);
            start = i;
            sum = 0;
        }
        sum = sum.saturating_add(s);
    }
    if start < sizes.len() {
        out.push(start..sizes.len());
    }
    out
}

/// The parts into `out`; one line per part for the log.
pub fn write(esp: &Path, list_file: &Path, o: &PartsOpts, out: &Path) -> Result<Vec<String>, String> {
    let mut said = Vec::new();
    // \BITSOS2\bin as the image has it, with --bin-file's replacements.
    let bin = esp.join("BITSOS2/bin");
    let mut bins: BTreeMap<String, PathBuf> = BTreeMap::new();
    if bin.is_dir() {
        for e in std::fs::read_dir(&bin).map_err(|e| format!("{}: {e}", bin.display()))?.flatten() {
            if e.path().is_file() {
                bins.insert(e.file_name().to_string_lossy().into_owned(), e.path());
            }
        }
    }
    for (n, p) in &o.bin_files {
        bins.insert(n.clone(), p.clone());
    }
    if bins.is_empty() {
        return Err(String::from("no file for \\BITSOS2\\bin: a version brings at least the GPU desktop"));
    }
    let mut bytes = 0u64;
    for (n, p) in &bins {
        if !bin_name_ok(n) {
            return Err(format!("\\BITSOS2\\bin\\{n}: not a name that stays an asset name on GitHub ([A-Za-z0-9._-])"));
        }
        bytes += std::fs::copy(p, out.join(format!("bin.{n}"))).map_err(|e| format!("{}: {e}", p.display()))?;
    }
    said.push(format!("bin: {} files, {bytes} bytes", bins.len()));
    let list = std::fs::read_to_string(list_file).map_err(|e| format!("{}: {e}", list_file.display()))?;
    for (name, mut files) in tree_files(esp, &list, o)? {
        files.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
        files.dedup_by(|a, b| a.0 == b.0);
        let sizes: Vec<u64> = files.iter().map(|f| size(&f.1)).collect::<Result<_, _>>()?;
        let pieces = split(&sizes, PIECE_RAW);
        for (k, r) in pieces.iter().enumerate() {
            let refs: Vec<&(String, Src)> = files[r.clone()].iter().collect();
            pack(&out.join(format!("tree.{name}.{}.tar.zst", k + 1)), &refs)?;
        }
        said.push(format!("tree {name}: {} files, {} bytes, {} piece(s)", files.len(), sizes.iter().sum::<u64>(), pieces.len()));
    }
    Ok(said)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pieces_and_names() {
        assert_eq!(split(&[5, 5, 5], 10), vec![0..2, 2..3]);
        assert_eq!(split(&[20, 1], 10), vec![0..1, 1..2], "a large file alone");
        assert_eq!(split(&[], 10), Vec::<std::ops::Range<usize>>::new());
        assert_eq!(split(&[u64::MAX, u64::MAX], 10), vec![0..1, 1..2]);
        assert!(PIECE_RAW + (PIECE_RAW >> 7) < (2 << 30) - 1);
        for good in ["mesa", "lo-gly", "firmware", "fake"] {
            assert!(tree_name_ok(good), "{good}");
        }
        for bad in ["bin", "sys", "Brave", "a.b", "", "../x"] {
            assert!(!tree_name_ok(bad), "{bad}");
        }
        assert!(bin_name_ok("brave.desktop") && bin_name_ok("bitscomp") && !bin_name_ok("a b") && !bin_name_ok(".x") && !bin_name_ok("a+b"));
        assert_eq!(Trees::parse("mesa, fake"), Trees::Only(vec!["mesa".into(), "fake".into()]));
        assert!(Trees::All.wants("x") && !Trees::None.wants("x"));
    }
}
