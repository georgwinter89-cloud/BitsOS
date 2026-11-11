//! Trees whose makers' files BitsOS does not redistribute (Brave, Steam). Their runtime asset carries the open-source
//! rest of the tree and `VENDOR.txt`, which names every missing file with its size and SHA-256 as the tree had it
//! when the release was tested, and where the maker publishes it:
//!
//! ```text
//! source <id> <url> <size|-> <sha256|-> <cache name>
//! file <path in the tree> <size> <sha256> <source id> <member>
//! ```
//!
//! A member is `-` (the download itself), `zip:<path>` or `deb:<path in data.tar.xz>`. Every file is taken out of
//! its download on the host and checked before the stick is touched; a file that cannot be fetched or does not
//! match leaves the whole tree out, and the installer says so.

pub mod unpack;

use crate::fetch::Fetcher;
use crate::stick::VendorFile;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Source {
    pub id: String,
    pub url: String,
    pub size: Option<u64>,
    pub sha256: Option<String>,
    pub cache_name: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct File {
    pub rel: String,
    pub size: u64,
    pub sha256: String,
    pub source: String,
    pub member: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct List {
    pub sources: Vec<Source>,
    pub files: Vec<File>,
}

fn opt<T: std::str::FromStr>(s: &str) -> Result<Option<T>, String> {
    if s == "-" { Ok(None) } else { s.parse().map(Some).map_err(|_| format!("VENDOR.txt: bad value {s}")) }
}

impl List {
    pub fn parse(text: &str) -> Result<List, String> {
        let mut l = List::default();
        for line in text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')) {
            let f: Vec<&str> = line.split_whitespace().collect();
            match f.as_slice() {
                ["source", id, url, size, sha, cache] => {
                    if !url.starts_with("https://") {
                        return Err(format!("VENDOR.txt: {url} is not https"));
                    }
                    let sha = opt::<String>(sha)?;
                    if sha.as_deref().is_some_and(|s| !crate::sha::is_hex64(s)) || !crate::manifest::name_ok(cache) {
                        return Err(format!("VENDOR.txt: bad source line for {id}"));
                    }
                    l.sources.push(Source { id: id.to_string(), url: url.to_string(), size: opt(size)?, sha256: sha, cache_name: cache.to_string() });
                }
                ["file", rel, size, sha, source, member] => {
                    let rel = crate::tar::safe_path(rel).ok_or_else(|| format!("VENDOR.txt: unsafe path {rel}"))?;
                    if !crate::sha::is_hex64(sha) || !l.sources.iter().any(|s| s.id == *source) {
                        return Err(format!("VENDOR.txt: bad file line for {rel}"));
                    }
                    let size = size.parse().map_err(|_| format!("VENDOR.txt: bad size for {rel}"))?;
                    l.files.push(File { rel, size, sha256: sha.to_string(), source: source.to_string(), member: member.to_string() });
                }
                _ => return Err(format!("VENDOR.txt: cannot read the line '{line}'")),
            }
        }
        Ok(l)
    }

    pub fn render(&self) -> String {
        let d = |o: Option<String>| o.unwrap_or_else(|| "-".into());
        let mut s = String::from("# The files of this tree BitsOS does not ship: bitsos-install fetches them from their makers and checks each one.\n");
        for x in &self.sources {
            s.push_str(&format!("source {} {} {} {} {}\n", x.id, x.url, d(x.size.map(|v| v.to_string())), d(x.sha256.clone()), x.cache_name));
        }
        for f in &self.files {
            s.push_str(&format!("file {} {} {} {} {}\n", f.rel, f.size, f.sha256, f.source, f.member));
        }
        s
    }

    pub fn bytes(&self) -> u64 {
        self.files.iter().map(|f| f.size).sum()
    }
}

/// The member path inside a zip whose entries may sit below one top folder (`brave-browser-1.2.3/brave`).
fn zip_name<'a>(entries: &'a [unpack::ZipEntry], want: &str) -> Option<&'a unpack::ZipEntry> {
    entries.iter().find(|e| e.name == want).or_else(|| entries.iter().find(|e| e.name.split_once('/').is_some_and(|(_, rest)| rest == want)))
}

/// Fetches and checks every file of `list`; the result names each file's checked copy on the host.
pub fn resolve(list: &List, fetch: &Fetcher, staging: &Path) -> Result<Vec<VendorFile>, String> {
    let mut got: BTreeMap<&str, PathBuf> = BTreeMap::new();
    for s in &list.sources {
        let base = s.url.rsplit('/').next().unwrap_or(&s.cache_name);
        let p = fetch.vendor(&[s.cache_name.as_str(), base], &s.url, s.size, s.sha256.as_deref())?;
        got.insert(s.id.as_str(), p);
    }
    let mut zips: BTreeMap<&str, Vec<unpack::ZipEntry>> = BTreeMap::new();
    let mut debs: BTreeMap<&str, Vec<u8>> = BTreeMap::new();
    let mut out = Vec::new();
    for f in &list.files {
        let src = &got[f.source.as_str()];
        let checked = |p: &Path, sha: &str, n: u64| -> Result<(), String> {
            if sha != f.sha256 || n != f.size {
                return Err(format!("{}: {} is not the file BitsOS was tested with (size or SHA-256 differ)", src.display(), f.rel));
            }
            let _ = p;
            Ok(())
        };
        let file = if f.member == "-" {
            let (sha, n) = crate::sha::of_file(src).map_err(|e| format!("{}: {e}", src.display()))?;
            checked(src, &sha, n)?;
            src.clone()
        } else {
            let to = staging.join(&f.rel);
            std::fs::create_dir_all(to.parent().expect("a file below staging")).map_err(|e| format!("{}: {e}", staging.display()))?;
            let mut o = std::io::BufWriter::new(std::fs::File::create(&to).map_err(|e| format!("{}: {e}", to.display()))?);
            let (sha, n) = if let Some(m) = f.member.strip_prefix("zip:") {
                let mut zf = std::fs::File::open(src).map_err(|e| format!("{}: {e}", src.display()))?;
                if !zips.contains_key(f.source.as_str()) {
                    zips.insert(&f.source, unpack::zip_list(&mut zf).map_err(|e| format!("{}: {e}", src.display()))?);
                }
                let e = zip_name(&zips[f.source.as_str()], m).ok_or_else(|| format!("{}: has no {m}", src.display()))?;
                unpack::zip_extract(&mut zf, e, &mut o).map_err(|e| format!("{}: {e}", src.display()))?
            } else if let Some(m) = f.member.strip_prefix("deb:") {
                if !debs.contains_key(f.source.as_str()) {
                    debs.insert(&f.source, unpack::deb_data_tar(src)?);
                }
                let mut t = crate::tar::Reader::new(&debs[f.source.as_str()][..]);
                let mut found = None;
                while let Some(e) = t.next_entry().map_err(|e| format!("{}: {e}", src.display()))? {
                    if e.path == m {
                        let mut h = crate::sha::Hashing::new(&mut t);
                        std::io::copy(&mut h, &mut o).map_err(|e| format!("{}: {e}", to.display()))?;
                        found = Some((h.sha.hex(), h.bytes));
                        break;
                    }
                }
                found.ok_or_else(|| format!("{}: has no {m}", src.display()))?
            } else {
                return Err(format!("VENDOR.txt: unknown member {}", f.member));
            };
            use std::io::Write;
            o.flush().map_err(|e| format!("{}: {e}", to.display()))?;
            checked(&to, &sha, n)?;
            to
        };
        out.push(VendorFile { rel: f.rel.clone(), file, size: f.size, sha256: f.sha256.clone() });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_round_trip_and_checks() {
        let l = List {
            sources: vec![Source { id: "z".into(), url: "https://example.org/a.zip".into(), size: None, sha256: None, cache_name: "a.zip".into() }],
            files: vec![File { rel: "brave".into(), size: 3, sha256: crate::sha::hex(b"abc"), source: "z".into(), member: "zip:brave".into() }],
        };
        assert_eq!(List::parse(&l.render()).unwrap(), l);
        assert!(List::parse("source z http://example.org/a.zip - - a.zip").is_err());
        assert!(List::parse(&format!("source z https://e/a - - a\nfile ../x 1 {} z -", crate::sha::hex(b""))).is_err());
        assert!(List::parse(&format!("file x 1 {} nosuch -", crate::sha::hex(b""))).is_err());
    }
}
