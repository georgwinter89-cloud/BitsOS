//! DB1: the content of program windows. BitsOS's own programs publish their elements in a file keyed by their pid
//! (user/lib/uilist `own`: opt-in, written by the program, read here only when asked, only when the file belongs
//! to the program's own user and was written after the process started: a file left by a crashed program is not
//! taken for a later process with its pid). The channel is advisory: any program of the same user could write a
//! file for another's pid; what it can steer is only where in that window a click lands. Foreign programs are
//! asked over contract C3 (/run/bitsos/atspi.sock, DB5a) from a thread (`ui_fetch`, DB3), never by the main loop:
//! a list takes what that cache holds; otherwise a window is only its picture. Each answer is bounded in size.

use std::fs::Metadata;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use uilist::{Element, Role, Source};

use crate::layout::Rect;
use crate::state::App;
use crate::win::Window;

pub const ATSPI_SOCKET: &str = "/run/bitsos/atspi.sock";
/// The longest answer line taken from the provider (atspid reads at most 4000 elements).
const ATSPI_MAX: u64 = 4 * 1024 * 1024;

/// The tick of /proc/<pid>/stat's start time (USER_HZ, fixed in the ABI, Linux and BitsOS alike).
const USER_HZ: u64 = 100;
/// A file this much older than its process is still its own (the boot time is in whole seconds).
const STALE_SLACK: Duration = Duration::from_secs(2);

/// When `pid` started: /proc/<pid>/stat field 22 (ticks after boot) after /proc/stat's btime.
fn started(pid: i32) -> Option<SystemTime> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // The name in parentheses may hold spaces and ')': the fields are counted after the last ')' (field 3 first).
    let ticks: u64 = stat.rsplit_once(')')?.1.split_whitespace().nth(19)?.parse().ok()?;
    let btime: u64 = std::fs::read_to_string("/proc/stat").ok()?.lines().find_map(|l| l.strip_prefix("btime "))?.trim().parse().ok()?;
    Some(UNIX_EPOCH + Duration::from_secs(btime) + Duration::from_millis(ticks * 1000 / USER_HZ))
}

/// Whether the file was written before `pid` started: a dead program's, its pid now another process's. It is
/// left alone, not removed: a clock set back after a program wrote its file would make a live file look stale.
fn stale(meta: &Metadata, pid: i32) -> bool {
    match (meta.modified(), started(pid)) {
        (Ok(m), Some(s)) => m + STALE_SLACK < s,
        _ => false,
    }
}

/// The program's file of `pid` under `dir`, when it is there, fresh and belongs to the program's user.
fn own_file_of(dir: &Path, pid: i32) -> Option<(std::fs::File, Result<(), String>)> {
    if pid <= 0 {
        return None;
    }
    let path = uilist::own_file(dir, pid as u32);
    let f = std::fs::File::open(&path).ok()?;
    let owner = std::fs::metadata(format!("/proc/{pid}")).ok()?.uid();
    let meta = f.metadata().ok()?;
    if meta.uid() != owner || !meta.is_file() {
        return Some((f, Err(format!("{} does not belong to the program's user", path.display()))));
    }
    if stale(&meta, pid) {
        return None;
    }
    Some((f, Ok(())))
}

/// The program's elements from its file, or why the file is not taken.
fn own_elements(dir: &Path, pid: i32) -> Option<Result<Vec<Element>, String>> {
    let (f, ok) = own_file_of(dir, pid)?;
    if let Err(why) = ok {
        return Some(Err(why));
    }
    let mut bytes = Vec::new();
    if f.take(uilist::OWN_MAX + 1).read_to_end(&mut bytes).is_err() {
        return Some(Err("unreadable".into()));
    }
    Some(uilist::parse_own(&bytes))
}

/// One C3 request, waiting at most `wait` for each step: the answer line as JSON, or why not. Only ever called
/// from a thread (`ui_fetch`).
pub(crate) fn atspi_ask_at(path: &Path, op: &str, args: serde_json::Value, wait: Duration) -> Result<serde_json::Value, String> {
    let mut s = UnixStream::connect(path).map_err(|e| e.to_string())?;
    s.set_read_timeout(Some(wait)).map_err(|e| e.to_string())?;
    s.set_write_timeout(Some(wait)).map_err(|e| e.to_string())?;
    let line = serde_json::json!({"id": "1", "op": op, "args": args});
    s.write_all(format!("{line}\n").as_bytes()).map_err(|e| e.to_string())?;
    let mut answer = String::new();
    BufReader::new(s.take(ATSPI_MAX)).read_line(&mut answer).map_err(|e| e.to_string())?;
    let v: serde_json::Value = serde_json::from_str(answer.trim()).map_err(|e| format!("not JSON: {e}"))?;
    match v.get("error").and_then(|e| e.as_str()) {
        Some(e) => Err(e.to_string()),
        None => Ok(v),
    }
}

/// Moves `v` from the program's own coordinates (logical px from its window's top-left) to screen pixels.
fn place(v: &mut [Element], origin: (i32, i32), content: Rect, s: f64) {
    for e in v {
        let [x, y, w, h] = e.rect;
        let p = crate::header::px_rect(Rect { x: content.x + x - origin.0, y: content.y + y - origin.1, w, h }, s);
        e.rect = [p.x, p.y, p.w, p.h];
        e.visible &= p.w > 0 && p.h > 0 && content.contains((content.x + x - origin.0) as f64 + w as f64 / 2.0, (content.y + y - origin.1) as f64 + h as f64 / 2.0);
        place(&mut e.children, origin, content, s);
    }
}

impl App {
    /// Where a window's content comes from, without reading it.
    pub(crate) fn ui_source_of(&self, pid: i32) -> Source {
        if own_file_of(&crate::rundir::dir(), pid).is_some_and(|(_, ok)| ok.is_ok()) {
            Source::Client
        } else {
            Source::Picture
        }
    }

    /// A program window's content: its own file, else the provider's tree as `ui_fetch` last read it, else its
    /// picture. Never asks the provider itself.
    pub(crate) fn ui_foreign(&self, w: &Window, pid: i32, content: Rect) -> (Source, Vec<Element>) {
        let s = self.theme.scale;
        match own_elements(&crate::rundir::dir(), pid) {
            Some(Ok(mut v)) => {
                place(&mut v, (0, 0), content, s);
                return (Source::Client, v);
            }
            Some(Err(why)) => eprintln!("bitscomp: ui: the element file of pid {pid} is not taken: {why}"),
            None => {}
        }
        let (origin, _) = self.ui_surface(w, content);
        if let Some(v) = self.ui_fetch.elements(&crate::ui_reg::wid(w), pid, origin, content, s) {
            return (Source::Atspi, v);
        }
        (Source::Picture, vec![Element::new(Role::Picture, "Bild des Fensters", self.ui_px(content)).key("picture")])
    }
}

/// The provider's ids become the keys of the stable ids (`uilist::Window::number`).
pub(crate) fn mark_atspi(v: &mut [Element]) {
    for e in v {
        e.id = format!("atspi:{}", e.id);
        mark_atspi(&mut e.children);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(what: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("bitscomp-ui-{what}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn a_file_older_than_its_process_is_not_taken() {
        let d = scratch("own");
        let pid = std::process::id();
        let e = [Element::new(Role::Button, "Quadrat", [1, 2, 3, 4])];
        uilist::write_own(&d, pid, &e).unwrap();
        assert!(matches!(own_elements(&d, pid as i32), Some(Ok(v)) if v[0].name == "Quadrat"), "a fresh file is taken");
        let f = std::fs::File::options().write(true).open(uilist::own_file(&d, pid)).unwrap();
        f.set_modified(UNIX_EPOCH + Duration::from_secs(1000)).unwrap();
        assert!(started(pid as i32).is_some_and(|s| s > UNIX_EPOCH + Duration::from_secs(1000)));
        assert!(own_elements(&d, pid as i32).is_none(), "a file from before the process started is a dead program's");
        assert!(own_file_of(&d, pid as i32).is_none());
        let _ = std::fs::remove_dir_all(&d);
    }
}
