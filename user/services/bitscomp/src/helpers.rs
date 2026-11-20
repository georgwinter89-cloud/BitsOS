//! The helper programs installed on this machine (provider kind `program`, user/services/assist/HELPER.md
//! "Telling the settings page about a helper"): each describes itself in a small text file in one fixed folder
//! of the owner's home, `~/.local/share/bitsos/helpers/<name>.helper`, and the page "KI" offers it by name.
//! Nothing is ever executed to list a helper: only the description is read, and the program it names is only
//! looked at (a file that can be run). A folder or file that someone else than the owner could have written is
//! not read, so another user cannot put a program on the owner's page. Whatever does not fit is left out with a
//! log line; listing never fails.

use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

/// The folder below the home.
pub const DIR: &str = ".local/share/bitsos/helpers";
pub const SUFFIX: &str = ".helper";
/// The contract versions a description may name (HELPER.md).
pub const VERSIONS: [u32; 4] = [1, 2, 3, 4];
/// More files than this are not read; a bigger one is not a description.
const FILES_MAX: usize = 32;
const SIZE_MAX: u64 = 4096;
pub const NAME_MAX: usize = 60;
pub const ABOUT_MAX: usize = 160;
/// The `models` line: at most this many entries, a value and a label at most this long.
pub const MODELS_MAX: usize = 12;
pub const MODEL_MAX: usize = 64;
pub const LABEL_MAX: usize = 40;

#[derive(Clone, Debug, PartialEq)]
pub struct Helper {
    /// The file's name without `.helper`: the provider's name it gets.
    pub id: String,
    /// Shown on the page.
    pub name: String,
    /// One German line: what it is.
    pub about: String,
    /// The full path of the program.
    pub path: String,
    /// `cloud` or `local` when the description says where the conversation goes; else Georg sets it.
    pub place: Option<String>,
    /// The models it announces: (the value handed to it as the model, the label shown). Empty: none announced.
    pub models: Vec<(String, String)>,
}

/// The owner's home: `HOME` when it is a real folder, else BitsOS's `/data/home/<uid>` (a root start that
/// dropped to a user without a passwd line sets HOME to `/`).
pub fn home(uid: u32) -> PathBuf {
    match std::env::var_os("HOME").map(PathBuf::from) {
        Some(h) if h.is_absolute() && h != Path::new("/") => h,
        _ => PathBuf::from(format!("/data/home/{uid}")),
    }
}

pub fn dir(uid: u32) -> PathBuf {
    home(uid).join(DIR)
}

fn one_line(v: &str, max: usize) -> Option<String> {
    let v = v.trim();
    (!v.is_empty() && v.chars().count() <= max && !v.chars().any(char::is_control)).then(|| v.to_string())
}

/// The description in `text` of the helper whose file is `<id>.helper`. Err: why it is left out (English, log).
pub fn parse(id: &str, text: &str) -> Result<Helper, String> {
    if !crate::ki::valid_id(id) {
        return Err("the file name is no provider name (a-z, 0-9, - and _)".into());
    }
    let (mut version, mut name, mut about, mut path, mut place) = (None, None, None, None, None);
    let mut models: Option<Vec<(String, String)>> = None;
    for line in text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')) {
        let Some((k, v)) = line.split_once('=') else { return Err(format!("a line without '=': {line:?}")) };
        let v = v.trim();
        match k.trim() {
            "bitsos_helper" => version = v.parse::<u32>().ok(),
            "name" => name = Some(one_line(v, NAME_MAX).ok_or(format!("name empty or longer than {NAME_MAX} characters"))?),
            "about" => about = Some(one_line(v, ABOUT_MAX).ok_or(format!("about empty or longer than {ABOUT_MAX} characters"))?),
            "path" => path = Some(v.to_string()),
            "place" if matches!(v, "cloud" | "local") => place = Some(v.to_string()),
            "place" => return Err(format!("place is {v:?}, not cloud or local")),
            // A wrong models line costs only that line: the helper still answers with its own default.
            "models" if models.is_some() => eprintln!("bitscomp: helpers: {id}.helper: a second models line is ignored"),
            "models" => match models_of(v) {
                Ok(m) => models = Some(m),
                Err(why) => {
                    eprintln!("bitscomp: helpers: {id}.helper: its models line is ignored: {why}");
                    models = Some(Vec::new());
                }
            },
            // Later versions may add words: what is not known is skipped.
            _ => {}
        }
    }
    match version {
        Some(v) if VERSIONS.contains(&v) => {}
        Some(v) => return Err(format!("contract version {v} is not one this BitsOS speaks")),
        None => return Err("no bitsos_helper version".into()),
    }
    let name = name.ok_or("no name")?;
    let path = path.ok_or("no path")?;
    program(&path)?;
    Ok(Helper { id: id.to_string(), name, about: about.unwrap_or_default(), path, place, models: models.unwrap_or_default() })
}

/// The models of a `models` line: `value: label` entries separated by commas (`opus: Opus, haiku: Haiku`). The
/// value is what the provider's model setting gets (and the helper as `model`): no spaces, and never a leading
/// `-`, so a helper that hands it on as an option's value cannot have it read as an option. Err: why not.
pub fn models_of(line: &str) -> Result<Vec<(String, String)>, String> {
    let mut out: Vec<(String, String)> = Vec::new();
    for entry in line.split(',') {
        let Some((value, label)) = entry.split_once(':') else { return Err(format!("{:?} is not value: label", entry.trim())) };
        let (value, label) = (value.trim(), label.trim());
        let fits = |c: char| c.is_ascii_alphanumeric() || "._-/@[]".contains(c);
        if value.is_empty() || value.len() > MODEL_MAX || value.starts_with('-') || !value.chars().all(fits) {
            return Err(format!("{value:?} is no model name (1 to {MODEL_MAX} of a-z, 0-9, . _ - / @ [ ], not starting with -)"));
        }
        let label = one_line(label, LABEL_MAX)
            .filter(|l| !l.contains(':'))
            .ok_or(format!("the label of {value:?} is empty, longer than {LABEL_MAX} characters or holds a ':'"))?;
        if out.iter().any(|(v, _)| v == value) {
            return Err(format!("{value:?} twice"));
        }
        out.push((value.to_string(), label));
    }
    if out.len() > MODELS_MAX {
        return Err(format!("{} models, at most {MODELS_MAX}", out.len()));
    }
    Ok(out)
}

/// The program as assist will start it (its provider/program check): an absolute path to a regular file that
/// can be run. It is only looked at.
pub fn program(path: &str) -> Result<(), String> {
    let p = Path::new(path);
    if !p.is_absolute() {
        return Err(format!("path {path:?} is not absolute"));
    }
    let m = std::fs::metadata(p).map_err(|e| format!("path {path:?}: {e}"))?;
    if !m.is_file() || m.permissions().mode() & 0o111 == 0 {
        return Err(format!("path {path:?} is no program file"));
    }
    Ok(())
}

/// Only the owner (or root) wrote it, and nobody else can.
fn own(m: &std::fs::Metadata, uid: u32) -> bool {
    (m.uid() == uid || m.uid() == 0) && m.mode() & 0o022 == 0
}

/// The helpers described in `dir` for the user `uid`, by name. Each one left out is logged with why.
pub fn scan(dir: &Path, uid: u32) -> Vec<Helper> {
    let Ok(m) = std::fs::symlink_metadata(dir) else { return Vec::new() };
    if !m.is_dir() || !own(&m, uid) {
        eprintln!("bitscomp: helpers: {} is not a folder only its owner writes: nothing listed", dir.display());
        return Vec::new();
    }
    let Ok(rd) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut names: Vec<String> = rd.flatten().filter_map(|e| e.file_name().into_string().ok()).filter(|n| n.ends_with(SUFFIX)).collect();
    names.sort();
    if names.len() > FILES_MAX {
        eprintln!("bitscomp: helpers: {} descriptions, only the first {FILES_MAX} are read", names.len());
        names.truncate(FILES_MAX);
    }
    let mut out = Vec::new();
    for n in names {
        let id = &n[..n.len() - SUFFIX.len()];
        match read(&dir.join(&n), uid).and_then(|t| parse(id, &t)) {
            Ok(h) => out.push(h),
            Err(why) => eprintln!("bitscomp: helpers: {n} left out: {why}"),
        }
    }
    out
}

fn read(path: &Path, uid: u32) -> Result<String, String> {
    let m = std::fs::symlink_metadata(path).map_err(|e| e.to_string())?;
    if !m.is_file() {
        return Err("not a plain file (a link is not followed)".into());
    }
    if !own(&m, uid) {
        return Err("someone else than its owner can write it".into());
    }
    if m.len() > SIZE_MAX {
        return Err(format!("bigger than {SIZE_MAX} bytes"));
    }
    std::fs::read_to_string(path).map_err(|e| e.to_string())
}

#[cfg(test)]
#[path = "helpers_tests.rs"]
mod tests;
