//! The music library: a walk of the music folders (own thread, B4), each audio file's tags from its first
//! [`tags::HEAD_BYTES`], albums by their album tag (else the folder), lists from the `*.m3u` files. Bounds are per
//! input only (R1): folder depth, entries read per folder, bytes per file; no cap on tracks, albums or lists.

use crate::events::{Events, Level, q};
use crate::{m3u, tags};
use std::collections::HashMap;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

/// Folder depth below a music folder (input bound: a link loop is not followed anyway, this stops deep trees).
pub const DEPTH_MAX: usize = 16;
/// Entries read per folder (input bound).
pub const ENTRIES_MAX: usize = 100_000;

const AUDIO: &[&str] = &["mp3", "flac", "ogg", "oga", "wav", "m4a", "aac", "mp4"];
const LISTS: &[&str] = &["m3u", "m3u8"];
/// The album of tracks right in a music folder without an album tag (screen word).
pub const LOOSE_ALBUM: &str = "Einzelne Titel";

#[derive(Clone, Debug)]
pub struct Track {
    pub path: PathBuf,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub number: Option<u32>,
}

#[derive(Clone, Debug)]
pub struct Group {
    pub name: String,
    pub artist: String,
    /// Indices into [`Library::tracks`], in play order.
    pub tracks: Vec<usize>,
}

#[derive(Clone, Debug, Default)]
pub struct Library {
    pub tracks: Vec<Track>,
    pub albums: Vec<Group>,
    pub lists: Vec<Group>,
    /// The music folders that exist (for the events and `library`).
    pub roots: Vec<PathBuf>,
}

fn ext(p: &Path) -> String {
    p.extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default()
}

fn head(p: &Path, max: usize) -> std::io::Result<Vec<u8>> {
    let mut v = Vec::new();
    fs::File::open(p)?.take(max as u64).read_to_end(&mut v)?;
    Ok(v)
}

struct Walk<'a> {
    ev: &'a Events,
    audio: Vec<PathBuf>,
    lists: Vec<PathBuf>,
}

impl Walk<'_> {
    fn dir(&mut self, d: &Path, depth: usize) {
        let rd = match fs::read_dir(d) {
            Ok(r) => r,
            Err(e) => {
                self.ev.emit(
                    Level::Warn,
                    "scan.unreadable",
                    &format!(
                        "dir={} error={}",
                        q(&d.to_string_lossy()),
                        q(&e.to_string())
                    ),
                );
                return;
            }
        };
        let mut entries: Vec<(PathBuf, fs::FileType)> = Vec::new();
        for (n, e) in rd.enumerate() {
            if n == ENTRIES_MAX {
                self.ev.emit(
                    Level::Warn,
                    "scan.cut",
                    &format!("dir={} entries={ENTRIES_MAX}", q(&d.to_string_lossy())),
                );
                break;
            }
            let Ok(e) = e else { continue };
            let Ok(t) = e.file_type() else { continue };
            entries.push((e.path(), t));
        }
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        for (p, t) in entries {
            // Links are not followed (a loop or a link out of the music folder).
            if t.is_dir() {
                if depth + 1 >= DEPTH_MAX {
                    self.ev.emit(
                        Level::Warn,
                        "scan.too_deep",
                        &format!("dir={}", q(&p.to_string_lossy())),
                    );
                } else {
                    self.dir(&p, depth + 1);
                }
            } else if t.is_file() {
                let x = ext(&p);
                if AUDIO.contains(&x.as_str()) {
                    self.audio.push(p);
                } else if LISTS.contains(&x.as_str()) {
                    self.lists.push(p);
                }
            }
        }
    }
}

/// Scans the folders that exist among `roots` (a missing one is left out quietly: no stick, no /data).
pub fn scan(roots: &[PathBuf], ev: &Events) -> Library {
    let mut w = Walk {
        ev,
        audio: Vec::new(),
        lists: Vec::new(),
    };
    let mut lib = Library::default();
    let mut root_of: Vec<(PathBuf, usize)> = Vec::new();
    for r in roots {
        if !r.is_dir() {
            continue;
        }
        lib.roots.push(r.clone());
        let from = w.audio.len();
        w.dir(r, 0);
        root_of.push((r.clone(), from));
    }
    for (i, p) in w.audio.iter().enumerate() {
        let t = match head(p, tags::HEAD_BYTES) {
            Ok(h) => tags::read(&h),
            Err(e) => {
                ev.emit(
                    Level::Warn,
                    "scan.unreadable",
                    &format!(
                        "file={} error={}",
                        q(&p.to_string_lossy()),
                        q(&e.to_string())
                    ),
                );
                continue;
            }
        };
        let root = root_of
            .iter()
            .rev()
            .find(|(_, from)| *from <= i)
            .map(|(r, _)| r.as_path());
        let folder = p
            .parent()
            .filter(|f| Some(*f) != root)
            .and_then(|f| f.file_name())
            .map(|f| f.to_string_lossy().into_owned());
        let stem = p
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        lib.tracks.push(Track {
            path: p.clone(),
            title: t.title.unwrap_or(stem),
            artist: t.artist.unwrap_or_default(),
            album: t
                .album
                .or(folder)
                .unwrap_or_else(|| LOOSE_ALBUM.to_string()),
            number: t.track,
        });
    }
    lib.albums = albums(&lib.tracks);
    let by_key: HashMap<String, usize> = lib
        .tracks
        .iter()
        .enumerate()
        .map(|(i, t)| (m3u::key(&t.path), i))
        .collect();
    for l in &w.lists {
        let text = match head(l, m3u::LIST_BYTES) {
            Ok(b) => {
                if b.len() == m3u::LIST_BYTES {
                    ev.emit(
                        Level::Warn,
                        "list.cut",
                        &format!("file={} bytes={}", q(&l.to_string_lossy()), m3u::LIST_BYTES),
                    );
                }
                String::from_utf8_lossy(&b).into_owned()
            }
            Err(e) => {
                ev.emit(
                    Level::Warn,
                    "scan.unreadable",
                    &format!(
                        "file={} error={}",
                        q(&l.to_string_lossy()),
                        q(&e.to_string())
                    ),
                );
                continue;
            }
        };
        let base = l.parent().unwrap_or(Path::new("/"));
        let wanted = m3u::parse(&text, base);
        let tracks: Vec<usize> = wanted
            .iter()
            .filter_map(|p| by_key.get(&m3u::key(p)).copied())
            .collect();
        if tracks.len() < wanted.len() {
            ev.emit(
                Level::Warn,
                "list.missing",
                &format!(
                    "file={} missing={}",
                    q(&l.to_string_lossy()),
                    wanted.len() - tracks.len()
                ),
            );
        }
        let mut name = l
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let mut n = 2;
        while lib.lists.iter().any(|g| g.name.eq_ignore_ascii_case(&name)) {
            name = format!(
                "{} {n}",
                l.file_stem()
                    .map(|s| s.to_string_lossy())
                    .unwrap_or_default()
            );
            n += 1;
        }
        lib.lists.push(Group {
            name,
            artist: String::new(),
            tracks,
        });
    }
    lib
}

/// Albums by name (case ignored), tracks by number then path; the artist when all tracks agree.
fn albums(tracks: &[Track]) -> Vec<Group> {
    let mut order: Vec<String> = Vec::new();
    let mut by: HashMap<String, Vec<usize>> = HashMap::new();
    for (i, t) in tracks.iter().enumerate() {
        let k = t.album.to_lowercase();
        by.entry(k.clone())
            .or_insert_with(|| {
                order.push(k.clone());
                Vec::new()
            })
            .push(i);
    }
    let mut out: Vec<Group> = order
        .into_iter()
        .filter_map(|k| {
            let mut v = by.remove(&k)?;
            v.sort_by(|&a, &b| {
                (tracks[a].number.unwrap_or(u32::MAX), &tracks[a].path)
                    .cmp(&(tracks[b].number.unwrap_or(u32::MAX), &tracks[b].path))
            });
            let first = v
                .first()
                .map(|&i| tracks[i].artist.clone())
                .unwrap_or_default();
            let artist = if v.iter().all(|&i| tracks[i].artist == first) {
                first
            } else {
                String::new()
            };
            let name = tracks[*v.first()?].album.clone();
            Some(Group {
                name,
                artist,
                tracks: v,
            })
        })
        .collect();
    out.sort_by_key(|g| g.name.to_lowercase());
    out
}
