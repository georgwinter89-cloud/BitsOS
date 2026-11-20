//! Version 3 of the helper contract (HELPER.md, "Pictures"): the picture of the newest question goes to a helper
//! only when the helper said in its description that it takes pictures (`bitsos_helper = 3` and
//! `pictures = yes` in `~/.local/share/bitsos/helpers/NAME.helper`, the file the settings page "KI" lists).
//! Asking the helper itself is not possible: a view must know before the first question whether a picture
//! can go, and a helper of version 1 or 2 would take any line as its request. The description is read by the
//! settings page's rules (owner only, no link, small, nothing executed), and matched to the provider by the
//! program it names, links followed.
//!
//! The description is no permission: a picture still goes only after Georg's own yes (or his Automodus) in
//! Durchblick. A wrong description at worst sends a picture to a helper of his that ignores it.

use crate::conv::Image;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

/// The folder below the owner's home (the settings page reads the same one).
pub const DIR: &str = ".local/share/bitsos/helpers";
const SUFFIX: &str = ".helper";
const FILES_MAX: usize = 32;
const SIZE_MAX: u64 = 4096;
/// The contract version from which a description may announce pictures.
pub const VERSION: u32 = 3;
/// The most base64 of one picture handed to a helper: what a view sends at most.
pub const PICTURE_MAX: usize = 12 << 20;
pub const TYPES: [&str; 2] = ["image/png", "image/jpeg"];

/// The owner's home as the settings page finds it: `HOME` when it is a real folder, else `/data/home/<uid>` (a
/// root start that dropped to a user without a passwd line has HOME `/`).
pub fn owner_home() -> Option<PathBuf> {
    match std::env::var_os("HOME").map(PathBuf::from) {
        Some(h) if h.is_absolute() && h != Path::new("/") => Some(h),
        _ => uid().map(|u| PathBuf::from(format!("/data/home/{u}"))),
    }
}

fn uid() -> Option<u32> {
    usys::geteuid().ok().and_then(|u| u32::try_from(u).ok())
}

/// Only the owner (or root) wrote it, and nobody else can.
fn own(m: &std::fs::Metadata, uid: u32) -> bool {
    (m.uid() == uid || m.uid() == 0) && m.mode() & 0o022 == 0
}

/// The longest name of a helper shown in Durchblick's note (the settings page takes no longer one).
pub const NAME_MAX: usize = 60;

/// What a description says about its program: the program's path, the name to show, whether it takes pictures.
struct Said<'a> {
    path: &'a str,
    name: Option<&'a str>,
    pictures: bool,
}

fn said(text: &str) -> Option<Said<'_>> {
    let (mut version, mut path, mut name, mut pictures) = (None, None, None, false);
    for line in text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')) {
        let Some((k, v)) = line.split_once('=') else { return None };
        match (k.trim(), v.trim()) {
            ("bitsos_helper", v) => version = v.parse::<u32>().ok(),
            ("path", v) => path = Some(v),
            ("name", v) => name = Some(v).filter(|v| !v.is_empty() && v.chars().count() <= NAME_MAX && !v.chars().any(char::is_control)),
            ("pictures", v) => pictures = v == "yes",
            _ => {}
        }
    }
    let version = version?;
    Some(Said { path: path?, name, pictures: pictures && version >= VERSION })
}

fn read(path: &Path, uid: u32) -> Option<String> {
    let m = std::fs::symlink_metadata(path).ok()?;
    (m.is_file() && own(&m, uid) && m.len() <= SIZE_MAX).then(|| std::fs::read_to_string(path).ok()).flatten()
}

/// What the descriptions below `home` say about the helper at `address`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Described {
    /// The name the settings page shows for it, when a description names one.
    pub name: Option<String>,
    /// A description of version 3 or later announces that it takes pictures.
    pub pictures: bool,
}

/// Reads the descriptions below `home` for the helper at `address` (none found: the default).
pub fn describe(home: &Path, address: &str) -> Described {
    let mut out = Described::default();
    let (Some(uid), Ok(program)) = (uid(), std::fs::canonicalize(address)) else { return out };
    let dir = home.join(DIR);
    if !std::fs::symlink_metadata(&dir).is_ok_and(|m| m.is_dir() && own(&m, uid)) {
        return out;
    }
    let Ok(rd) = std::fs::read_dir(&dir) else { return out };
    let mut names: Vec<String> = rd.flatten().filter_map(|e| e.file_name().into_string().ok()).filter(|n| n.ends_with(SUFFIX)).collect();
    names.sort();
    names.truncate(FILES_MAX);
    for text in names.iter().filter_map(|n| read(&dir.join(n), uid)) {
        let Some(s) = said(&text).filter(|s| std::fs::canonicalize(s.path).is_ok_and(|p| p == program)) else { continue };
        // The description that announces pictures names the helper; else the first one that names it.
        if s.pictures || out.name.is_none() {
            out.name = s.name.map(str::to_string).or(out.name);
        }
        out.pictures |= s.pictures;
    }
    out
}

/// Whether the helper at `address` announced in a description below `home` that it takes pictures.
pub fn takes_pictures(home: &Path, address: &str) -> bool {
    describe(home, address).pictures
}

/// A picture a helper may get: PNG or JPEG, base64 only, not bigger than [`PICTURE_MAX`].
pub fn fits(i: &Image) -> bool {
    TYPES.contains(&i.media_type.as_str()) && !i.base64.is_empty() && i.base64.len() <= PICTURE_MAX && i.base64.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'='))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The program a description announces pictures for, when it does.
    fn announced(text: &str) -> Option<&str> {
        said(text).filter(|s| s.pictures).map(|s| s.path)
    }

    #[test]
    fn only_a_description_of_version_3_with_pictures_yes_announces() {
        assert_eq!(announced("bitsos_helper = 3\nname = A\npath = /x/h\npictures = yes\n"), Some("/x/h"));
        for no in ["bitsos_helper = 2\npath = /x/h\npictures = yes", "bitsos_helper = 3\npath = /x/h", "bitsos_helper = 3\npath = /x/h\npictures = ja", "bitsos_helper = 3\npictures = yes", "bitsos_helper = 3\npath = /x/h\npictures = yes\nkaputt"] {
            assert_eq!(announced(no), None, "{no}");
        }
        // The name is told for every version, but only a one-line, short one.
        assert_eq!(said("bitsos_helper = 2\nname = Mein Helfer\npath = /x/h").map(|s| (s.name, s.pictures)), Some((Some("Mein Helfer"), false)));
        assert_eq!(said(&format!("bitsos_helper = 3\nname = {}\npath = /x/h", "n".repeat(NAME_MAX + 1))).and_then(|s| s.name), None);
    }

    #[test]
    fn a_picture_fits_when_it_is_png_or_jpeg_in_base64_and_not_too_big() {
        let i = |t: &str, d: String| Image { media_type: t.into(), base64: d };
        assert!(fits(&i("image/png", "iVBORw0KGgo=".into())) && fits(&i("image/jpeg", "/9j/".into())));
        for bad in [i("image/gif", "R0lG".into()), i("image/png", String::new()), i("image/png", "a b".into()), i("image/png", "A".repeat(PICTURE_MAX + 1))] {
            assert!(!fits(&bad), "{}", bad.media_type);
        }
    }
}
