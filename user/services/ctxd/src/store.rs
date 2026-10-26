//! The data folder (DESIGN §4). Source of truth: `workspaces/`, `usage/`, `config/`. Derived and
//! rebuildable: `index/`, `text/`, `cache/`. Crash safety (§4.3): nothing is updated in place; an index
//! generation is complete or absent and is committed by renaming `index/CURRENT` after an fsync; the text
//! and vector caches are written by temp + rename with a checksum, so after a kill -9 only the batch in
//! flight is lost.

use crate::roots::Denied;
use crate::util::{sync_tree, write_atomic};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

pub const FORMAT: &str = "ctxd-data 1";

/// Chunk byte ranges of one document and their vectors (row after row).
pub type ChunkVecs = (Vec<(u32, u32)>, Vec<f32>);

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PathRec {
    pub path: String,
    pub root: usize,
    pub dev: u64,
    pub ino: u64,
    pub size: u64,
    pub mtime: i64,
    #[serde(default)]
    pub mtime_ns: i64,
    #[serde(default)]
    pub ctime_ns: i64,
    /// The file changed within `RACY_NS` of the scan that hashed it: a second write in the same timestamp
    /// tick would look unchanged, so its hash is never reused (git's "racily clean" rule).
    #[serde(default)]
    pub racy: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DocRec {
    pub id: String,
    /// Several paths when the same content lies in several places (shown once, "auch in: ...").
    pub paths: Vec<PathRec>,
    pub hash: String,
    pub kind: String,
    pub title: String,
    /// `ok`, `empty`, `failed:<reason>`.
    pub status: String,
    /// True when any path lies under a private root: never shown to Claude (Georg 15.10.).
    pub private: bool,
    pub mtime: i64,
    pub size: u64,
    pub chunk_start: u32,
    pub chunk_count: u32,
    #[serde(default)]
    pub warnings: Vec<String>,
    #[serde(default)]
    pub meta: Vec<(String, String)>,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq)]
pub struct Calib {
    pub floor: f32,
    pub ceil: f32,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct RootRec {
    pub path: String,
    pub private: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Catalog {
    pub version: u32,
    pub generation: u64,
    pub built: i64,
    pub embedder: Option<String>,
    pub dim: usize,
    pub chunk_sig: String,
    pub roots: Vec<RootRec>,
    pub denied: Vec<Denied>,
    pub docs: Vec<DocRec>,
    pub n_chunks: usize,
    pub chunks_sum: String,
    pub vectors_sum: String,
    pub calib: Calib,
    pub scan: serde_json::Value,
}

/// Extracted text cached by content hash (+ extractor version).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedText {
    pub extractor: String,
    pub kind: String,
    pub title: Option<String>,
    pub status: String,
    pub text: String,
    #[serde(default)]
    pub meta: Vec<(String, String)>,
    #[serde(default)]
    pub warnings: Vec<String>,
    pub sum: String,
}

pub struct Store {
    pub dir: PathBuf,
    _lock: Option<fs::File>,
}

fn sum(b: &[u8]) -> String {
    blake3::hash(b).to_hex()[..32].to_string()
}

impl Store {
    /// Opens (and creates) the data folder. `exclusive` takes the folder lock: one writer at a time.
    pub fn open(dir: &Path, exclusive: bool) -> Result<Store, String> {
        // The folder holds extracted text of private roots: only Georg's user may enter it.
        crate::util::private_dir(dir).map_err(|e| format!("data folder {}: {e}", dir.display()))?;
        for d in ["index", "text", "cache", "workspaces", "usage", "tmp", "log", "config"] {
            fs::create_dir_all(dir.join(d)).map_err(|e| format!("data folder {}: {e}", dir.display()))?;
        }
        let fmt = dir.join("FORMAT");
        match fs::read_to_string(&fmt) {
            Ok(s) if s.trim() == FORMAT => {}
            Ok(s) => return Err(format!("data folder has format {:?}; this ctxd knows {FORMAT:?}", s.trim())),
            Err(_) => write_atomic(&fmt, format!("{FORMAT}\n").as_bytes(), true).map_err(|e| e.to_string())?,
        }
        let lock = if exclusive {
            let f = fs::OpenOptions::new()
                .create(true)
                .truncate(false)
                .write(true)
                .open(dir.join("LOCK"))
                .map_err(|e| e.to_string())?;
            f.try_lock().map_err(|_| "another ctxd owns this data folder".to_string())?;
            Some(f)
        } else {
            None
        };
        let st = Store { dir: dir.to_path_buf(), _lock: lock };
        if exclusive {
            st.recover();
        }
        Ok(st)
    }

    /// Start-up clean-up: scratch is emptied, generations newer than CURRENT (a crash mid-build) go.
    fn recover(&self) {
        let _ = fs::remove_dir_all(self.dir.join("tmp"));
        let _ = fs::create_dir_all(self.dir.join("tmp"));
        let cur = self.current();
        for (n, p) in self.generations() {
            if cur.is_none_or(|c| n > c) {
                crate::event!("info", "recover.drop_generation", generation = n);
                let _ = fs::remove_dir_all(p);
            }
        }
        for e in fs::read_dir(self.dir.join("index")).into_iter().flatten().flatten() {
            if e.file_name().to_string_lossy().starts_with(".CURRENT.tmp") {
                let _ = fs::remove_file(e.path());
            }
        }
    }

    pub fn generations(&self) -> Vec<(u64, PathBuf)> {
        let mut v: Vec<(u64, PathBuf)> = fs::read_dir(self.dir.join("index"))
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| {
                let n = e.file_name().to_string_lossy().strip_prefix('g')?.parse().ok()?;
                Some((n, e.path()))
            })
            .collect();
        v.sort();
        v
    }

    pub fn current(&self) -> Option<u64> {
        let s = fs::read_to_string(self.dir.join("index/CURRENT")).ok()?;
        s.trim().strip_prefix('g')?.parse().ok()
    }

    pub fn gen_dir(&self, n: u64) -> PathBuf {
        self.dir.join(format!("index/g{n:06}"))
    }

    pub fn begin_generation(&self) -> io::Result<(u64, PathBuf)> {
        let n = self.generations().last().map(|g| g.0).max(self.current()).unwrap_or(0) + 1;
        let d = self.gen_dir(n);
        fs::create_dir_all(&d)?;
        Ok((n, d))
    }

    /// fsync the whole generation, then switch CURRENT by rename. `CTXD_TEST_COMMIT_PAUSE_MS` holds the
    /// commit between the two steps so the kill -9 test can hit exactly that window.
    pub fn commit(&self, n: u64) -> io::Result<()> {
        sync_tree(&self.gen_dir(n))?;
        if let Some(ms) = std::env::var("CTXD_TEST_COMMIT_PAUSE_MS").ok().and_then(|v| v.parse::<u64>().ok()) {
            eprintln!("ctxd-test: commit pause g{n}");
            std::thread::sleep(std::time::Duration::from_millis(ms));
        }
        write_atomic(&self.dir.join("index/CURRENT"), format!("g{n:06}\n").as_bytes(), true)
    }

    /// Keep the live generation and the one before it (a query may still hold it).
    pub fn gc(&self) {
        let Some(cur) = self.current() else { return };
        let gens = self.generations();
        let keep_prev = gens.iter().filter(|g| g.0 < cur).map(|g| g.0).max();
        for (n, p) in gens {
            if n != cur && Some(n) != keep_prev {
                let _ = fs::remove_dir_all(p);
            }
        }
    }

    fn text_path(&self, hash: &str) -> PathBuf {
        self.dir.join("text").join(&hash[..2]).join(format!("{hash}.json"))
    }

    pub fn get_text(&self, hash: &str, extractor: &str) -> Option<CachedText> {
        let b = fs::read(self.text_path(hash)).ok()?;
        let t: CachedText = serde_json::from_slice(&b).ok()?;
        (t.extractor == extractor && t.sum == sum(t.text.as_bytes())).then_some(t)
    }

    pub fn put_text(&self, hash: &str, mut t: CachedText) -> io::Result<()> {
        t.sum = sum(t.text.as_bytes());
        let b = serde_json::to_vec(&t).map_err(io::Error::other)?;
        write_atomic(&self.text_path(hash), &b, false)
    }

    pub fn drop_text(&self, hash: &str) {
        let _ = fs::remove_file(self.text_path(hash));
    }

    fn vec_path(&self, emb: &str, hash: &str, sig: &str) -> PathBuf {
        self.dir.join("cache").join(crate::embed::slug(emb)).join(&hash[..2]).join(format!("{hash}-{sig}.bin"))
    }

    /// Chunk ranges and their vectors for one content hash: "CXV1", dim, n, ranges, vectors, checksum.
    pub fn get_vecs(&self, emb: &str, hash: &str, sig: &str, dim: usize) -> Option<ChunkVecs> {
        let b = fs::read(self.vec_path(emb, hash, sig)).ok()?;
        if b.len() < 12 + 16 || &b[..4] != b"CXV1" {
            return None;
        }
        let (body, check) = b.split_at(b.len() - 16);
        if blake3::hash(body).as_bytes()[..16] != *check {
            return None;
        }
        let d = u32::from_le_bytes(body[4..8].try_into().ok()?) as usize;
        let n = u32::from_le_bytes(body[8..12].try_into().ok()?) as usize;
        if d != dim || body.len() != 12 + n * 8 + n * d * 4 {
            return None;
        }
        let mut ranges = Vec::with_capacity(n);
        for i in 0..n {
            let o = 12 + i * 8;
            ranges.push((
                u32::from_le_bytes(body[o..o + 4].try_into().ok()?),
                u32::from_le_bytes(body[o + 4..o + 8].try_into().ok()?),
            ));
        }
        let vecs = body[12 + n * 8..]
            .as_chunks::<4>()
            .0
            .iter()
            .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect();
        Some((ranges, vecs))
    }

    pub fn put_vecs(
        &self,
        emb: &str,
        hash: &str,
        sig: &str,
        dim: usize,
        ranges: &[(u32, u32)],
        vecs: &[f32],
    ) -> io::Result<()> {
        let mut b = Vec::with_capacity(12 + ranges.len() * 8 + vecs.len() * 4 + 16);
        b.extend_from_slice(b"CXV1");
        b.extend_from_slice(&(dim as u32).to_le_bytes());
        b.extend_from_slice(&(ranges.len() as u32).to_le_bytes());
        for (s, e) in ranges {
            b.extend_from_slice(&s.to_le_bytes());
            b.extend_from_slice(&e.to_le_bytes());
        }
        for v in vecs {
            b.extend_from_slice(&v.to_le_bytes());
        }
        let check = blake3::hash(&b);
        b.extend_from_slice(&check.as_bytes()[..16]);
        write_atomic(&self.vec_path(emb, hash, sig), &b, false)
    }

    pub fn drop_vecs(&self, hash: &str) {
        for e in fs::read_dir(self.dir.join("cache")).into_iter().flatten().flatten() {
            let d = e.path().join(&hash[..2]);
            for f in fs::read_dir(&d).into_iter().flatten().flatten() {
                if f.file_name().to_string_lossy().starts_with(hash) {
                    let _ = fs::remove_file(f.path());
                }
            }
        }
    }

    pub fn write_catalog(dir: &Path, cat: &Catalog) -> io::Result<()> {
        let b = serde_json::to_vec(cat).map_err(io::Error::other)?;
        fs::write(dir.join("catalog.json"), b)
    }

    pub fn read_catalog(dir: &Path) -> Result<Catalog, String> {
        let b = fs::read(dir.join("catalog.json")).map_err(|e| format!("catalog: {e}"))?;
        serde_json::from_slice(&b).map_err(|e| format!("catalog: {e}"))
    }

    pub fn file_sum(path: &Path) -> io::Result<String> {
        Ok(sum(&fs::read(path)?))
    }

    pub fn bytes_sum(b: &[u8]) -> String {
        sum(b)
    }

    /// Delete derived state (forget scope `index`): generations, texts, vectors.
    pub fn wipe_derived(&self) -> u64 {
        let mut n = 0;
        for d in ["index", "text", "cache"] {
            let p = self.dir.join(d);
            n += count_files(&p);
            let _ = fs::remove_dir_all(&p);
            let _ = fs::create_dir_all(&p);
        }
        n
    }

    pub fn wipe_dir(&self, name: &str) -> u64 {
        let p = self.dir.join(name);
        let n = count_files(&p);
        let _ = fs::remove_dir_all(&p);
        let _ = fs::create_dir_all(&p);
        n
    }
}

fn count_files(p: &Path) -> u64 {
    let mut n = 0;
    let mut stack = vec![p.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in fs::read_dir(&d).into_iter().flatten().flatten() {
            match e.file_type() {
                Ok(t) if t.is_dir() => stack.push(e.path()),
                Ok(_) => n += 1,
                Err(_) => {}
            }
        }
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("ctxd-store-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn generations_commit_and_recover() {
        let d = tmp("gen");
        {
            let s = Store::open(&d, true).unwrap();
            assert!(Store::open(&d, true).is_err(), "second writer refused");
            let (n, _) = s.begin_generation().unwrap();
            s.commit(n).unwrap();
            assert_eq!(s.current(), Some(n));
            let (m, gd) = s.begin_generation().unwrap();
            assert_eq!(m, n + 1);
            fs::write(gd.join("half"), b"x").unwrap();
        }
        // Reopen: the uncommitted generation is dropped, the committed one stays.
        let s = Store::open(&d, true).unwrap();
        assert_eq!(s.generations().len(), 1);
        assert_eq!(s.current(), Some(1));
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn caches_detect_damage() {
        let d = tmp("cache");
        let s = Store::open(&d, true).unwrap();
        let h = "abcdef0123456789";
        s.put_vecs("e", h, "s", 2, &[(0, 3)], &[0.5, 0.25]).unwrap();
        assert_eq!(s.get_vecs("e", h, "s", 2), Some((vec![(0, 3)], vec![0.5, 0.25])));
        assert_eq!(s.get_vecs("e", h, "s", 3), None);
        let p = s.vec_path("e", h, "s");
        let mut b = fs::read(&p).unwrap();
        b[13] ^= 1;
        fs::write(&p, b).unwrap();
        assert_eq!(s.get_vecs("e", h, "s", 2), None, "checksum");
        let t = CachedText {
            extractor: "x".into(),
            kind: "text".into(),
            title: None,
            status: "ok".into(),
            text: "hi".into(),
            meta: vec![],
            warnings: vec![],
            sum: String::new(),
        };
        s.put_text(h, t).unwrap();
        assert!(s.get_text(h, "x").is_some());
        assert!(s.get_text(h, "y").is_none(), "other extractor version");
        let _ = fs::remove_dir_all(&d);
    }
}
