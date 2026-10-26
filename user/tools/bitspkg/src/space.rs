//! The space report (PK-5): per program what its versions, its data folder and the caches in it take, and for the
//! store what gc would free. Read-only, metadata only (nothing is hashed).
//!
//! Counting follows GNU du: disk use is `st_blocks * 512` (linux-6.6/fs/stat.c generic_fillattr), a hard-linked
//! inode counts once. Version files are hard links into `apps/.store/` and shared between versions and between
//! programs, so a program's package is split into `own` (inodes no other program uses: removing the program frees
//! them) and `shared` (also used by another program), and each version says what only it uses (`alone`:
//! removing just that version frees it).

use crate::store::Store;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

/// Apparent bytes (file sizes) and disk bytes (allocated blocks).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Size {
    pub bytes: u64,
    pub disk: u64,
}

impl Size {
    fn add_md(&mut self, md: &fs::Metadata) {
        self.bytes += md.len();
        self.disk += disk_of(md);
    }
    fn add(&mut self, o: Size) {
        self.bytes += o.bytes;
        self.disk += o.disk;
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionSpace {
    pub version: String,
    /// "current", "previous" or "kept" (a version left by a cut or tampering; gc removes it).
    pub role: &'static str,
    /// Everything below the version folder, each inode once.
    pub size: Size,
    /// What only this version uses (removing just this version frees it, once gc ran).
    pub alone: Size,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DataSpace {
    pub size: Size,
    /// Part of `size` below folders that are caches (see [`is_cache`]).
    pub cache: Size,
    /// Cache folders found, relative to the data folder.
    pub cache_dirs: Vec<String>,
    /// Entries that could not be read (the data folder belongs to the program's uid): the sizes are then too low.
    pub unreadable: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProgramSpace {
    pub name: String,
    pub versions: Vec<VersionSpace>,
    /// All versions together, each inode once.
    pub package: Size,
    /// Part of `package` no other program uses.
    pub own: Size,
    /// Part of `package` another program uses too.
    pub shared: Size,
    /// Version files that are separate copies instead of hard links to the store (no hard links there, or a store
    /// file replaced after it went bad): their space is not shared with anything.
    pub copies: usize,
    /// None: there is no data folder.
    pub data: Option<DataSpace>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct StoreSpace {
    pub files: usize,
    pub size: Size,
    /// Store files no version links to: `bitspkg gc` frees them.
    pub unused: Size,
    pub unused_files: usize,
    /// Removals in progress (`apps/.trash`, `data/appdata/.trash`): the next changing command finishes them.
    pub trash: Size,
    /// Data folders without an installed program (`remove --keep-data` leaves them on purpose).
    pub orphan_data: Vec<(String, Size)>,
}

type Ino = (u64, u64);

/// Who uses an inode: (program, version) pairs.
#[derive(Default)]
struct Users(HashMap<Ino, HashSet<(String, String)>>);

/// A folder cache: a name ending in "cache" or "caches", any case (`cache`, `.cache`, `Cache`, `fontcache`,
/// `GPUCache`, `Code Cache`, `ShaderCache`, `GrShaderCache`). A heuristic by name only: the layout has no
/// separate cache folder (plan/PACKAGES.md: the data folder holds "profile, caches, its own files").
pub fn is_cache(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    n.ends_with("cache") || n.ends_with("caches")
}

/// The report for every program (or only `only`) and, without `only`, the store part (default when `only`).
pub fn report(st: &Store, only: Option<&str>) -> Result<(Vec<ProgramSpace>, StoreSpace), String> {
    if let Some(n) = only {
        crate::names::check_name(n)?;
        if !fs::symlink_metadata(st.pkg(n)).map(|m| m.is_dir()).unwrap_or(false) {
            return Err(format!("{n} is not installed"));
        }
    }
    let names = st.packages()?;
    // Every program is scanned even for one name: sharing is only known from all of them.
    let mut users = Users::default();
    let mut meta: HashMap<Ino, fs::Metadata> = HashMap::new();
    let mut per_version: HashMap<(String, String), HashSet<Ino>> = HashMap::new();
    let mut other: HashMap<(String, String), Size> = HashMap::new();
    // manifest.toml is written into the version folder, never into the store: no copy.
    let mut manifests: HashSet<Ino> = HashSet::new();
    for name in &names {
        for v in st.versions(name)? {
            let key = (name.clone(), v.clone());
            let mut inos = HashSet::new();
            let mut rest = Size::default();
            walk(&st.version_dir(name, &v), &mut |md| {
                if md.is_file() {
                    let i = (md.dev(), md.ino());
                    inos.insert(i);
                    meta.entry(i).or_insert_with(|| md.clone());
                    users.0.entry(i).or_default().insert(key.clone());
                } else {
                    rest.add_md(md);
                }
            });
            if let Ok(md) = fs::symlink_metadata(st.version_dir(name, &v).join(crate::manifest::FILE_NAME)) {
                manifests.insert((md.dev(), md.ino()));
            }
            per_version.insert(key.clone(), inos);
            other.insert(key, rest);
        }
    }
    let size_of = |set: &mut dyn Iterator<Item = &Ino>| {
        let mut s = Size::default();
        for i in set {
            if let Some(md) = meta.get(i) {
                s.add_md(md);
            }
        }
        s
    };
    let store_inos = store_inodes(st);
    let mut out = Vec::new();
    for name in names.iter().filter(|n| only.is_none_or(|o| o == n.as_str())) {
        let (cur, prev) = (st.current(name), st.previous(name));
        let mut p = ProgramSpace {
            name: name.clone(),
            versions: Vec::new(),
            package: Size::default(),
            own: Size::default(),
            shared: Size::default(),
            copies: 0,
            data: data_space(&st.data_dir(name)),
        };
        let mut all: HashSet<Ino> = HashSet::new();
        for v in st.versions(name)? {
            let key = (name.clone(), v.clone());
            let inos = &per_version[&key];
            let alone: Vec<&Ino> = inos.iter().filter(|i| users.0[*i].len() == 1).collect();
            let mut size = size_of(&mut inos.iter());
            let rest = other[&key];
            size.add(rest);
            let mut alone_size = size_of(&mut alone.into_iter());
            alone_size.add(rest);
            let role = if cur.as_deref() == Some(&v) {
                "current"
            } else if prev.as_deref() == Some(&v) {
                "previous"
            } else {
                "kept"
            };
            p.versions.push(VersionSpace { version: v, role, size, alone: alone_size });
            p.package.add(rest);
            p.own.add(rest);
            all.extend(inos.iter().copied());
        }
        for i in &all {
            let Some(md) = meta.get(i) else { continue };
            p.package.add_md(md);
            if users.0[i].iter().all(|(n, _)| n == name) {
                p.own.add_md(md);
            } else {
                p.shared.add_md(md);
            }
            if !store_inos.contains(i) && !manifests.contains(i) {
                p.copies += 1;
            }
        }
        out.push(p);
    }
    let mut s = StoreSpace::default();
    if only.is_some() {
        // One program asked for: the store part (and walking orphan data folders) is left out.
        return Ok((out, s));
    }
    if let Ok(rd) = fs::read_dir(st.content()) {
        for e in rd.flatten() {
            let Ok(md) = fs::symlink_metadata(e.path()) else { continue };
            s.files += 1;
            s.size.add_md(&md);
            if md.is_file() && !users.0.contains_key(&(md.dev(), md.ino())) {
                s.unused.add_md(&md);
                s.unused_files += 1;
            }
        }
    }
    for t in [st.trash(), st.data_trash()] {
        let mut seen = HashSet::new();
        walk(&t, &mut |md| {
            if !md.is_file() || seen.insert((md.dev(), md.ino())) {
                s.trash.add_md(md);
            }
        });
    }
    let installed: HashSet<&String> = names.iter().collect();
    if let Ok(rd) = fs::read_dir(&st.data) {
        let mut orphans: Vec<(String, Size)> = rd
            .flatten()
            .filter_map(|e| {
                let n = e.file_name().to_string_lossy().to_string();
                let dir = e.file_type().ok()?.is_dir();
                (dir && !n.starts_with('.') && !installed.contains(&n)).then(|| (n.clone(), data_space(&e.path()).map(|d| d.size).unwrap_or_default()))
            })
            .collect();
        orphans.sort_by(|a, b| a.0.cmp(&b.0));
        s.orphan_data = orphans;
    }
    Ok((out, s))
}

fn store_inodes(st: &Store) -> HashSet<Ino> {
    let Ok(rd) = fs::read_dir(st.content()) else { return HashSet::new() };
    rd.flatten().filter_map(|e| fs::symlink_metadata(e.path()).ok()).map(|md| (md.dev(), md.ino())).collect()
}

/// Disk use of one inode: `st_blocks * 512`. A file system that leaves st_blocks 0 for a non-empty regular file
/// (an in-memory one that does not fill it) would make the report read 0 without a warning, so such a file
/// counts as its length rounded up to 512; a fully sparse file is then overcounted, which package and data
/// files are not in practice.
pub fn disk_of(md: &fs::Metadata) -> u64 {
    match md.blocks() {
        0 if md.is_file() && md.len() > 0 => md.len().div_ceil(512) * 512,
        b => b * 512,
    }
}

/// Calls `f` for every entry below `top` (not `top` itself), never following symlinks, never leaving `top`'s
/// file system: like `du -x`, a mount point inside counts as its folder but is not descended into. Unreadable
/// folders are skipped; the count of them is returned.
fn walk(top: &Path, f: &mut dyn FnMut(&fs::Metadata)) -> usize {
    let Ok(md) = fs::symlink_metadata(top) else { return 0 };
    if !md.is_dir() {
        return 0;
    }
    let dev = md.dev();
    let mut unreadable = 0;
    // An explicit stack: folder depth comes from the disk, so no recursion limit is involved (R1).
    let mut stack = vec![top.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = fs::read_dir(&d) else {
            unreadable += 1;
            continue;
        };
        for e in rd.flatten() {
            let p = e.path();
            let Ok(md) = fs::symlink_metadata(&p) else {
                unreadable += 1;
                continue;
            };
            f(&md);
            if md.is_dir() && md.dev() == dev {
                stack.push(p);
            }
        }
    }
    unreadable
}

fn data_space(dir: &Path) -> Option<DataSpace> {
    let md = fs::symlink_metadata(dir).ok()?;
    if !md.is_dir() {
        return None;
    }
    let mut d = DataSpace::default();
    let mut seen = HashSet::new();
    let mut count = |md: &fs::Metadata| md.is_dir() || md.nlink() < 2 || seen.insert((md.dev(), md.ino()));
    // Whole folder first, then each cache folder (not nested in another cache) once more for the cache part.
    d.unreadable = walk(dir, &mut |md| {
        if count(md) {
            d.size.add_md(md);
        }
    });
    let mut caches = Vec::new();
    let dev = md.dev();
    let mut stack = vec![(dir.to_path_buf(), String::new())];
    while let Some((p, rel)) = stack.pop() {
        let Ok(rd) = fs::read_dir(&p) else { continue };
        for e in rd.flatten() {
            // Real folders on the same file system only (the same rule as walk).
            if !fs::symlink_metadata(e.path()).map(|m| m.is_dir() && m.dev() == dev).unwrap_or(false) {
                continue;
            }
            let n = e.file_name().to_string_lossy().to_string();
            let r = if rel.is_empty() { n.clone() } else { format!("{rel}/{n}") };
            if is_cache(&n) {
                caches.push((e.path(), r));
            } else {
                stack.push((e.path(), r));
            }
        }
    }
    caches.sort_by(|a, b| a.1.cmp(&b.1));
    let mut seen = HashSet::new();
    for (p, r) in caches {
        let mut s = Size::default();
        if let Ok(md) = fs::symlink_metadata(&p) {
            s.add_md(&md);
        }
        walk(&p, &mut |md| {
            if md.is_dir() || md.nlink() < 2 || seen.insert((md.dev(), md.ino())) {
                s.add_md(md);
            }
        });
        d.cache.add(s);
        d.cache_dirs.push(r);
    }
    Some(d)
}

/// "328.1 MB": decimal units, one decimal, for people reading the report.
pub fn human(b: u64) -> String {
    const U: [&str; 5] = ["kB", "MB", "GB", "TB", "PB"];
    if b < 1000 {
        return format!("{b} B");
    }
    let mut x = b as f64 / 1000.0;
    let mut u = 0;
    while x >= 1000.0 && u + 1 < U.len() {
        x /= 1000.0;
        u += 1;
    }
    format!("{x:.1} {}", U[u])
}
