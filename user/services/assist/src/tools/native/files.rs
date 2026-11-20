//! `datei_lesen`, `datei_schreiben`, `datei_aendern`. A file is written in place (truncated and written), never
//! through a new file renamed over it: inside the assistant's tree a rename over a file sends the old one to the
//! trash (K6), and every edit would fill it.

use super::{OUT_MAX, cap, path_of};
use crate::json::Json;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

const LINES_DEFAULT: usize = 2000;
/// A line longer than this is cut on the way out.
const LINE_MAX: usize = 2000;
/// A file larger than this is not read whole into memory; datei_lesen reads its start.
const READ_MAX: u64 = 64 << 20;

/// What a write puts into a file, for Georg: its size, its start and its end.
pub fn sized(content: &str) -> String {
    let lines = content.lines().count();
    let lines = if lines == 1 { "1 Zeile".to_string() } else { format!("{lines} Zeilen") };
    format!("{lines}, {} Zeichen: {}", content.chars().count(), crate::term::wrap::inline(&cap(content, 1000), 1100))
}

/// An exact replacement, for Georg.
pub fn replaced(args: &Json) -> String {
    let half = |k: &str| crate::term::wrap::inline(&cap(args.str(k).unwrap_or(""), 500), 560);
    let all = if args.get("replace_all").and_then(Json::as_bool) == Some(true) { " (jede Stelle)" } else { "" };
    format!("„{}“ wird zu „{}“{all}", half("old_string"), half("new_string"))
}

fn unreadable(p: &std::path::Path, e: &std::io::Error) -> String {
    format!("{}: {e}", p.display())
}

fn not_plain(p: &Path) -> String {
    format!("{} is not a plain file (a pipe, a device or a socket); befehl can work with it", p.display())
}

/// Opens `p` only when it is a plain file. Without O_NONBLOCK the open of a pipe nobody writes to (or reads
/// from) waits without end, and with it the chat; the kind is told by the open file itself, so nothing can be
/// swapped in between.
pub(super) fn open_plain(p: &Path, write: bool) -> Result<(File, std::fs::Metadata), String> {
    let mut o = OpenOptions::new();
    if write {
        o.write(true).create(true);
    } else {
        o.read(true);
    }
    let f = o.custom_flags(libc::O_NONBLOCK).mode(0o666).open(p).map_err(|e| match e.raw_os_error() {
        Some(libc::ENXIO) => not_plain(p),
        _ => unreadable(p, &e),
    })?;
    let meta = f.metadata().map_err(|e| unreadable(p, &e))?;
    if !meta.is_file() {
        return Err(not_plain(p));
    }
    Ok((f, meta))
}

/// A plain file's text, at most `max` bytes of it.
pub(super) fn read_plain(p: &Path, max: u64) -> Result<(Vec<u8>, std::fs::Metadata), String> {
    let (f, meta) = open_plain(p, false)?;
    let mut bytes = Vec::new();
    f.take(max).read_to_end(&mut bytes).map_err(|e| unreadable(p, &e))?;
    Ok((bytes, meta))
}

pub fn read(args: &Json) -> Result<String, String> {
    let p = path_of(args.str("path").unwrap_or(""))?;
    let meta = std::fs::metadata(&p).map_err(|e| unreadable(&p, &e))?;
    if meta.is_dir() {
        return Err(format!("{} is a folder; dateien_finden lists what is in it", p.display()));
    }
    let (bytes, meta) = read_plain(&p, READ_MAX)?;
    if bytes[..bytes.len().min(8192)].contains(&0) {
        return Err(format!("{} is not a text file ({} bytes)", p.display(), meta.len()));
    }
    let text = String::from_utf8_lossy(&bytes);
    let from = args.get("offset").and_then(Json::as_i64).and_then(|o| usize::try_from(o).ok()).unwrap_or(1).max(1);
    let count = args.get("limit").and_then(Json::as_i64).and_then(|l| usize::try_from(l).ok()).unwrap_or(LINES_DEFAULT).max(1);
    let total = text.lines().count();
    let mut out = String::new();
    for (i, line) in text.lines().enumerate().skip(from - 1).take(count) {
        let line: String = if line.chars().count() > LINE_MAX { format!("{} … (line cut)", line.chars().take(LINE_MAX).collect::<String>()) } else { line.to_string() };
        out.push_str(&format!("{:>6}\t{line}\n", i + 1));
        if out.len() > OUT_MAX {
            out.push_str(&format!("… (stopped at line {} of {total}; read on with offset)\n", i + 1));
            break;
        }
    }
    if out.is_empty() {
        return Ok(format!("{}: {total} lines, nothing at line {from}", p.display()));
    }
    let shown_to = (from - 1 + count).min(total);
    if shown_to < total && !out.ends_with("offset)\n") {
        out.push_str(&format!("… ({total} lines in all; read on with offset {})\n", shown_to + 1));
    }
    if meta.len() > READ_MAX {
        out.push_str(&format!("… (only the first {READ_MAX} bytes of {} were read)\n", meta.len()));
    }
    Ok(out)
}

fn put(p: &std::path::Path, content: &[u8]) -> Result<(), String> {
    if let Some(dir) = p.parent() {
        std::fs::create_dir_all(dir).map_err(|e| unreadable(dir, &e))?;
    }
    let (mut f, _) = open_plain(p, true)?;
    f.set_len(0).map_err(|e| unreadable(p, &e))?;
    f.write_all(content).map_err(|e| unreadable(p, &e))
}

pub fn write(args: &Json) -> Result<String, String> {
    let p = path_of(args.str("path").unwrap_or(""))?;
    if p.is_dir() {
        return Err(format!("{} is a folder", p.display()));
    }
    let content = args.str("content").unwrap_or("");
    let was = p.exists();
    put(&p, content.as_bytes())?;
    Ok(format!("{} {} ({} bytes)", p.display(), if was { "replaced" } else { "written" }, content.len()))
}

pub fn edit(args: &Json) -> Result<String, String> {
    let p = path_of(args.str("path").unwrap_or(""))?;
    let (old, new) = (args.str("old_string").unwrap_or(""), args.str("new_string").unwrap_or(""));
    if old.is_empty() {
        return Err("old_string is empty; datei_schreiben writes a whole file".into());
    }
    if old == new {
        return Err("old_string and new_string are the same".into());
    }
    let (bytes, _) = read_plain(&p, u64::MAX)?;
    let text = String::from_utf8(bytes).map_err(|_| format!("{} is not a text file (UTF-8)", p.display()))?;
    let n = text.matches(old).count();
    let all = args.get("replace_all").and_then(Json::as_bool) == Some(true);
    match n {
        0 => return Err(format!("old_string does not occur in {}", p.display())),
        n if n > 1 && !all => return Err(format!("old_string occurs {n} times in {}; give more of the text around it, or replace_all", p.display())),
        _ => {}
    }
    let changed = if all { text.replace(old, new) } else { text.replacen(old, new, 1) };
    put(&p, changed.as_bytes())?;
    Ok(format!("{}: {} replaced", p.display(), if n == 1 { "1 place".to_string() } else { format!("{n} places") }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::json::parse;

    fn dir(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("assist-files-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn a(j: &str) -> Json {
        parse(j).unwrap()
    }

    #[test]
    fn write_read_and_change_a_file() {
        let d = dir("rw");
        let f = d.join("neu/probe.txt");
        let path = f.to_str().unwrap();
        assert!(write(&a(&format!(r#"{{"path":"{path}","content":"eins\nzwei\nzwei\n"}}"#))).unwrap().contains("written"));
        let r = read(&a(&format!(r#"{{"path":"{path}"}}"#))).unwrap();
        assert_eq!(r, "     1\teins\n     2\tzwei\n     3\tzwei\n");
        assert_eq!(read(&a(&format!(r#"{{"path":"{path}","offset":2,"limit":1}}"#))).unwrap(), "     2\tzwei\n… (3 lines in all; read on with offset 3)\n");
        assert!(edit(&a(&format!(r#"{{"path":"{path}","old_string":"zwei","new_string":"drei"}}"#))).unwrap_err().contains("2 times"));
        assert!(edit(&a(&format!(r#"{{"path":"{path}","old_string":"vier","new_string":"drei"}}"#))).unwrap_err().contains("does not occur"));
        assert!(edit(&a(&format!(r#"{{"path":"{path}","old_string":"zwei","new_string":"drei","replace_all":true}}"#))).unwrap().contains("2 places"));
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "eins\ndrei\ndrei\n");
        // In place: the same file (inode), never a new one renamed over it.
        let ino = std::os::unix::fs::MetadataExt::ino(&std::fs::metadata(&f).unwrap());
        write(&a(&format!(r#"{{"path":"{path}","content":"x"}}"#))).unwrap();
        assert_eq!(std::os::unix::fs::MetadataExt::ino(&std::fs::metadata(&f).unwrap()), ino);
        std::fs::write(d.join("bin"), b"\x7fELF\0\0\0").unwrap();
        assert!(read(&a(&format!(r#"{{"path":"{}"}}"#, d.join("bin").display()))).unwrap_err().contains("not a text file"));
        assert!(read(&a(&format!(r#"{{"path":"{}"}}"#, d.display()))).unwrap_err().contains("is a folder"));
        assert!(read(&a(r#"{"path":"/no/such/file"}"#)).is_err());
        let long = "y".repeat(5000);
        std::fs::write(d.join("long"), format!("{long}\n").repeat(40)).unwrap();
        let r = read(&a(&format!(r#"{{"path":"{}"}}"#, d.join("long").display()))).unwrap();
        assert!(r.contains("(line cut)") && r.len() < OUT_MAX + 3000 && r.contains("read on with offset"), "{}", r.len());
        assert!(sized("a\nb").starts_with("2 Zeilen, 3 Zeichen: "));
    }

    #[test]
    fn a_pipe_is_refused_at_once_never_waited_on() {
        let d = dir("fifo");
        let pipe = d.join("rohr");
        let made = std::process::Command::new("mkfifo").arg(&pipe).status().unwrap();
        assert!(made.success());
        let path = pipe.display();
        let t = std::time::Instant::now();
        for r in [
            read(&a(&format!(r#"{{"path":"{path}"}}"#))),
            write(&a(&format!(r#"{{"path":"{path}","content":"x"}}"#))),
            edit(&a(&format!(r#"{{"path":"{path}","old_string":"a","new_string":"b"}}"#))),
        ] {
            assert!(r.as_ref().unwrap_err().contains("not a plain file"), "{r:?}");
        }
        assert!(t.elapsed() < std::time::Duration::from_secs(5));
        assert!(write(&a(r#"{"path":"/dev/null","content":"x"}"#)).unwrap_err().contains("not a plain file"));
        let _ = std::fs::remove_dir_all(&d);
    }
}
