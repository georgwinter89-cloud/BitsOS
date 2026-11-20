//! `dateien_finden` (glob) and `text_suchen` (a regular expression over the text of files). A walk follows no
//! link to a folder (a loop, or the whole machine through one link) and leaves out the kernel's own folders
//! (/proc, /sys, /dev); it ends at Esc and after [`WALK_TIME`].

use super::glob;
use super::{OUT_MAX, home, path_of};
use crate::json::Json;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

const WALK_TIME: Duration = Duration::from_secs(60);
/// The most names one answer lists, and the most lines of matches.
const NAMES_MAX: usize = 500;
/// A file larger than this is not searched.
const FILE_MAX: u64 = 16 << 20;

fn pseudo(p: &Path) -> bool {
    ["/proc", "/sys", "/dev"].iter().any(|x| p == Path::new(x))
}

/// Every file below `root` (links to files included, links to folders not followed), with its path relative
/// to `root`. False: the walk was cut (Esc or time).
fn walk(root: &Path, stop: &dyn Fn() -> bool, each: &mut dyn FnMut(&Path, &str) -> bool) -> bool {
    let until = Instant::now() + WALK_TIME;
    let mut todo: Vec<(PathBuf, String)> = vec![(root.to_path_buf(), String::new())];
    while let Some((dir, rel)) = todo.pop() {
        if stop() || Instant::now() > until {
            return false;
        }
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        let mut names: Vec<_> = rd.flatten().collect();
        names.sort_by_key(std::fs::DirEntry::file_name);
        for (i, e) in names.into_iter().rev().enumerate() {
            // A folder of many files, or a slow look at each, ends at Esc and in time too.
            if i % 64 == 63 && (stop() || Instant::now() > until) {
                return false;
            }
            let name = e.file_name().to_string_lossy().into_owned();
            let path = e.path();
            let r = if rel.is_empty() { name } else { format!("{rel}/{name}") };
            let Ok(ft) = e.file_type() else { continue };
            if ft.is_dir() {
                if !pseudo(&path) {
                    todo.push((path, r));
                }
            } else if !each(&path, &r) {
                return true;
            }
        }
    }
    true
}

fn folder(args: &Json) -> Result<PathBuf, String> {
    match args.str("path").filter(|p| !p.trim().is_empty()) {
        Some(p) => path_of(p),
        None => Ok(home()),
    }
}

pub fn find(args: &Json, stop: &dyn Fn() -> bool) -> Result<String, String> {
    let pattern = args.str("pattern").unwrap_or("").trim();
    if pattern.is_empty() {
        return Err("pattern is empty".into());
    }
    if pattern.chars().count() > glob::PATTERN_MAX {
        return Err(format!("pattern is longer than {} characters", glob::PATTERN_MAX));
    }
    // An absolute pattern names its own folder.
    let expanded = match pattern.strip_prefix('~') {
        Some(rest) => format!("{}{rest}", home().display()),
        None => pattern.to_string(),
    };
    let (fixed, pattern) = glob::base(&expanded);
    let root = if expanded.starts_with('/') { PathBuf::from(format!("/{fixed}")) } else { folder(args)?.join(fixed) };
    if !root.is_dir() {
        return Err(format!("{} is not a folder", root.display()));
    }
    let mut found: Vec<(SystemTime, PathBuf)> = Vec::new();
    let whole = walk(&root, stop, &mut |p, rel| {
        if glob::matches(&pattern, rel) {
            let t = std::fs::metadata(p).and_then(|m| m.modified()).unwrap_or(SystemTime::UNIX_EPOCH);
            found.push((t, p.to_path_buf()));
        }
        true
    });
    found.sort_by(|a, b| b.0.cmp(&a.0));
    let n = found.len();
    let mut out: String = found.iter().take(NAMES_MAX).map(|(_, p)| format!("{}\n", p.display())).collect();
    if n == 0 {
        out = format!("no file in {} fits {pattern:?}\n", root.display());
    } else if n > NAMES_MAX {
        out.push_str(&format!("… and {} more (give a narrower pattern)\n", n - NAMES_MAX));
    }
    if !whole {
        out.push_str("… (the search was cut: Esc or one minute)\n");
    }
    Ok(super::cap(&out, OUT_MAX))
}

pub fn grep(args: &Json, stop: &dyn Fn() -> bool) -> Result<String, String> {
    let pattern = args.str("pattern").unwrap_or("");
    if pattern.is_empty() {
        return Err("pattern is empty".into());
    }
    let ignore = args.get("ignore_case").and_then(Json::as_bool) == Some(true);
    let re = regex_lite::RegexBuilder::new(pattern).case_insensitive(ignore).size_limit(1 << 20).build().map_err(|e| format!("pattern is no regular expression: {e}"))?;
    let only = args.str("glob").filter(|g| !g.trim().is_empty()).map(str::to_string);
    if only.as_ref().is_some_and(|g| g.chars().count() > glob::PATTERN_MAX) {
        return Err(format!("glob is longer than {} characters", glob::PATTERN_MAX));
    }
    let root = folder(args)?;
    let mut lines: Vec<String> = Vec::new();
    let mut more = 0usize;
    let mut look = |p: &Path, rel: &str| {
        // Only plain files: a pipe or a device in the folder would hold the search without end.
        if only.as_deref().is_some_and(|g| !glob::matches(g, rel)) || std::fs::metadata(p).map_or(true, |m| !m.is_file() || m.len() > FILE_MAX) {
            return true;
        }
        let Ok((bytes, _)) = super::files::read_plain(p, FILE_MAX) else { return true };
        if bytes[..bytes.len().min(8192)].contains(&0) {
            return true;
        }
        for (i, line) in String::from_utf8_lossy(&bytes).lines().enumerate() {
            if re.is_match(line) {
                if lines.len() < NAMES_MAX {
                    let shown: String = line.chars().take(300).collect();
                    lines.push(format!("{}:{}: {shown}", p.display(), i + 1));
                } else {
                    more += 1;
                }
            }
        }
        true
    };
    let whole = if root.is_file() {
        let rel = root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        look(&root, &rel);
        true
    } else if root.is_dir() {
        walk(&root, stop, &mut look)
    } else {
        return Err(format!("{} is neither a file nor a folder", root.display()));
    };
    let mut out = if lines.is_empty() { format!("no line in {} fits {pattern:?}\n", root.display()) } else { lines.join("\n") + "\n" };
    if more > 0 {
        out.push_str(&format!("… and {more} more lines (give a narrower pattern or path)\n"));
    }
    if !whole {
        out.push_str("… (the search was cut: Esc or one minute)\n");
    }
    Ok(super::cap(&out, OUT_MAX))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::json::parse;

    #[test]
    fn find_and_grep_in_a_tree_with_a_link_loop() {
        let d = std::env::temp_dir().join(format!("assist-search-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("a/b")).unwrap();
        std::fs::write(d.join("a/b/Rechnung 1.pdf"), "x").unwrap();
        std::fs::write(d.join("a/notiz.txt"), "Hallo Georg\nzweite Zeile\n").unwrap();
        std::fs::write(d.join("bild.bin"), b"Georg\0\0").unwrap();
        std::os::unix::fs::symlink(&d, d.join("a/schleife")).unwrap();
        let j = |s: String| parse(&s).unwrap();
        let root = d.display();
        let f = find(&j(format!(r#"{{"pattern":"*.pdf","path":"{root}"}}"#)), &|| false).unwrap();
        assert_eq!(f.trim(), d.join("a/b/Rechnung 1.pdf").display().to_string());
        let abs = find(&j(format!(r#"{{"pattern":"{root}/a/*.txt"}}"#)), &|| false).unwrap();
        assert!(abs.contains("notiz.txt") && !abs.contains("pdf"), "{abs}");
        assert!(find(&j(format!(r#"{{"pattern":"*.doc","path":"{root}"}}"#)), &|| false).unwrap().starts_with("no file"));
        let g = grep(&j(format!(r#"{{"pattern":"georg","ignore_case":true,"path":"{root}"}}"#)), &|| false).unwrap();
        assert_eq!(g.trim(), format!("{}:1: Hallo Georg", d.join("a/notiz.txt").display()), "binary files are left out");
        let g = grep(&j(format!(r#"{{"pattern":"Zeile$","glob":"*.txt","path":"{root}"}}"#)), &|| false).unwrap();
        assert!(g.contains(":2: zweite Zeile"), "{g}");
        assert!(grep(&j(format!(r#"{{"pattern":"(","path":"{root}"}}"#)), &|| false).unwrap_err().contains("no regular expression"));
        assert!(find(&j(format!(r#"{{"pattern":"*","path":"{root}"}}"#)), &|| true).unwrap().contains("was cut"));
        // A pipe nobody writes to is passed by, never waited on.
        assert!(std::process::Command::new("mkfifo").arg(d.join("a/rohr")).status().unwrap().success());
        let t = Instant::now();
        let g = grep(&j(format!(r#"{{"pattern":"Georg","path":"{root}"}}"#)), &|| false).unwrap();
        assert!(g.contains("Hallo Georg") && t.elapsed() < Duration::from_secs(5), "{g}");
        assert!(grep(&j(format!(r#"{{"pattern":"x","path":"{}"}}"#, d.join("a/rohr").display())), &|| false).is_err(), "a pipe is neither file nor folder");
        let _ = std::fs::remove_dir_all(&d);
    }
}
