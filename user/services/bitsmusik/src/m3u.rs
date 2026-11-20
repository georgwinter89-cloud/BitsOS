//! `*.m3u` / `*.m3u8` lists (pure): one path per line, `#` lines (#EXTM3U, #EXTINF) skipped, relative paths
//! against the list's folder, `\` as written on Windows or FAT read as `/`. The scan matches the paths to its
//! tracks without regard to case (the stick is FAT).

use std::path::{Path, PathBuf};

/// Bytes read of one list file (input bound; a longer list keeps its first part and an event names it).
pub const LIST_BYTES: usize = 1024 * 1024;

pub fn parse(text: &str, base: &Path) -> Vec<PathBuf> {
    text.trim_start_matches('\u{feff}')
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(|l| {
            let l = l.strip_prefix("file://").unwrap_or(l).replace('\\', "/");
            let p = Path::new(&l);
            if p.is_absolute() {
                p.to_path_buf()
            } else {
                base.join(p)
            }
        })
        .collect()
}

/// A path without `.` and `..` parts, lower case: the key a list entry and a scanned track are matched by.
pub fn key(p: &Path) -> String {
    let mut parts: Vec<String> = Vec::new();
    for c in p.components() {
        match c {
            std::path::Component::ParentDir => {
                parts.pop();
            }
            std::path::Component::Normal(s) => parts.push(s.to_string_lossy().to_lowercase()),
            _ => {}
        }
    }
    format!("/{}", parts.join("/"))
}
