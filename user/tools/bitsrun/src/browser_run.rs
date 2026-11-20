//! BR-9: what a browser start needs from the system before its view is planned: one profile (the stick's taken
//! over once), the downloads folder, the desktop's theme and accessibility bus. `--plan` changes nothing.

use crate::browser::{self, Browser, Facts};
use crate::plan::{Plan, Tree};
use std::fs::{self, File, TryLockError};
use std::io::{ErrorKind, Read};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// A browser start, ready to rewrite its plan.
pub struct Ready {
    pub browser: &'static Browser,
    facts: Facts,
    notes: Vec<String>,
}

impl Ready {
    pub fn apply(self, p: &mut Plan) {
        browser::adjust(p, self.browser, &self.facts);
        p.notes.extend(self.notes);
    }
}

/// The note file left in the stick's profile folder once its profile moved into the program's.
pub const MOVED_NOTE: &str = ".moved-to";

/// `tree`/`program`: the program's view; `data`: its data folder; `stick`: the stick launcher's data folder
/// (`/data/appdata/brave`); `home`: the user's home folder. None when the program is no browser bitsrun knows.
pub fn prepare(tree: &dyn Tree, program: &str, data: &Path, stick: &Path, home: &str, uid: u32, dry: bool) -> Result<Option<Ready>, String> {
    let Some(b) = browser::detect(tree, program) else { return Ok(None) };
    let mut notes = Vec::new();
    if stick != data {
        notes.extend(take_over(stick, &data.join(b.profile), dry)?);
    }
    if !dry {
        notes.extend(downloads(Path::new(home), uid));
    }
    let runtime = format!("/run/user/{uid}");
    let bus = format!("{runtime}/at-spi/bus");
    let a11y = fs::symlink_metadata(&bus).is_ok_and(|m| m.file_type().is_socket()).then_some(bus);
    let cursor = std::env::var("XCURSOR_SIZE").ok().filter(|s| !s.is_empty() && s.len() <= 4 && s.bytes().all(|c| c.is_ascii_digit()));
    let mode = setting("theme.mode").unwrap_or_default();
    let dark = if mode.trim() == "auto" {
        browser::dark_now(&mode, answer("date", &["+%H"]).as_deref(), setting("theme.auto.light_from").as_deref(), setting("theme.auto.dark_from").as_deref())
    } else {
        browser::dark_now(&mode, None, None, None)
    };
    crate::event("pkg.run.browser", &[("browser", b.name), ("dark", if dark { "yes" } else { "no" }), ("a11y", if a11y.is_some() { "yes" } else { "no" })]);
    Ok(Some(Ready { browser: b, facts: Facts { dark, a11y_bus: a11y, cursor_size: cursor }, notes }))
}

/// One line a helper program prints within 3 s (B4: a hanging settingsd never holds a start), else None.
fn answer(prog: &str, args: &[&str]) -> Option<String> {
    let path = ["/bin", "/usr/bin", "/sbin"].iter().map(|d| PathBuf::from(d).join(prog)).find(|p| p.is_file())?;
    let mut c = Command::new(path).args(args).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().ok()?;
    let end = Instant::now() + Duration::from_secs(3);
    loop {
        match c.try_wait() {
            Ok(Some(s)) if s.success() => break,
            Ok(None) if Instant::now() < end => std::thread::sleep(Duration::from_millis(20)),
            _ => {
                let _ = c.kill();
                let _ = c.wait();
                return None;
            }
        }
    }
    let mut out = String::new();
    c.stdout.take()?.take(4096).read_to_string(&mut out).ok()?;
    out.lines().next().map(|l| l.trim().to_string())
}

fn setting(key: &str) -> Option<String> {
    answer("settings", &["get", key])
}

/// The pid in Chromium's lock link (`<host>-<pid>`) when that process is a Brave on this profile. A lock a crash, a
/// kill or a power cut left names a pid that after a restart often belongs to another program: that lock is stale.
/// Chromium writes its argv as one string with spaces (its process title), so spaces split words as NULs do.
fn lock_alive(dir: &Path) -> Option<u32> {
    let l = fs::read_link(dir.join("SingletonLock")).ok()?;
    let pid: u32 = l.to_str()?.rsplit_once('-')?.1.parse().ok()?;
    let cmd = fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    brave_on(&cmd, dir).then_some(pid)
}

/// `cmdline` (a /proc/PID/cmdline) is a Brave whose `--user-data-dir` is the folder `dir`.
pub fn brave_on(cmdline: &[u8], dir: &Path) -> bool {
    let words: Vec<&[u8]> = cmdline.split(|b| *b == 0 || *b == b' ').filter(|w| !w.is_empty()).collect();
    let brave = words.iter().any(|w| !w.starts_with(b"-") && (*w == b"brave" || w.ends_with(b"/brave")));
    let same = |v: &[u8]| {
        let v = Path::new(std::ffi::OsStr::from_bytes(v));
        v == dir || matches!((fs::canonicalize(v), fs::canonicalize(dir)), (Ok(a), Ok(b)) if a == b)
    };
    brave && words.iter().any(|w| w.strip_prefix(b"--user-data-dir=").is_some_and(same))
}

/// The take-over's own lock (flock: a killed holder never leaves it taken): two starts at once (a double click on the
/// tile) would both find the program's profile empty and move the stick's entries against each other.
fn takeover_lock(p: &Path) -> Result<File, String> {
    let f = File::options().create(true).truncate(false).write(true).open(p).map_err(|e| failed("lock", p, &e))?;
    let start = Instant::now();
    loop {
        match f.try_lock() {
            Ok(()) => return Ok(f),
            Err(TryLockError::WouldBlock) if start.elapsed() < LOCK_WAIT => std::thread::sleep(Duration::from_millis(50)),
            Err(TryLockError::WouldBlock) => {
                crate::event("pkg.run.profile.busy", &[("lock", &p.to_string_lossy())]);
                return Err("Ein anderer Start von Brave übernimmt gerade das alte Profil. Bitte Brave gleich noch einmal öffnen.".into());
            }
            Err(TryLockError::Error(e)) => return Err(failed("lock", p, &e)),
        }
    }
}

/// How long a start waits for another one's take-over (renames on one file system: far below a second).
const LOCK_WAIT: Duration = Duration::from_secs(30);

/// Georg's sentence for a take-over that went wrong; what and where go to the desk's event line.
fn failed(step: &str, p: &Path, e: &dyn std::fmt::Display) -> String {
    crate::event("pkg.run.profile.failed", &[("step", step), ("path", &p.to_string_lossy()), ("error", &e.to_string())]);
    "Das alte Brave-Profil lässt sich nicht übernehmen; es bleibt, wie es war. Brave startet nicht.".into()
}

fn is_real_dir(p: &Path) -> bool {
    fs::symlink_metadata(p).is_ok_and(|m| m.is_dir())
}

/// One Brave, one profile: the first start whose profile is still empty moves the stick launcher's profile
/// (`/data/appdata/brave`, same file system) into it, entry by entry (the user may not rename the folder itself:
/// /data/appdata is root's). Refused while a Brave runs on the stick's profile; a failed move is undone.
pub fn take_over(stick: &Path, target: &Path, dry: bool) -> Result<Option<String>, String> {
    if !is_real_dir(stick) || !stick.join("Local State").is_file() || target.join("Local State").exists() {
        return Ok(None);
    }
    let busy = |pid: u32| {
        crate::event("pkg.run.profile.in_use", &[("pid", &pid.to_string()), ("profile", &stick.to_string_lossy())]);
        "Brave läuft noch mit dem alten Profil. Bitte dieses Brave schließen und Brave noch einmal öffnen.".to_string()
    };
    if let Some(pid) = lock_alive(stick) {
        return Err(busy(pid));
    }
    if dry {
        return Ok(Some(format!("Profil: {} wird beim Start nach {} übernommen", stick.display(), target.display())));
    }
    let parent = target.parent().ok_or_else(|| failed("parent", target, &"no parent"))?;
    fs::DirBuilder::new().recursive(true).mode(0o700).create(parent).map_err(|e| failed("mkdir", parent, &e))?;
    let _lock = takeover_lock(&parent.join(".takeover-lock"))?;
    // Under the lock again: the start that waited finds what the other one moved.
    if !stick.join("Local State").is_file() || target.join("Local State").exists() {
        return Ok(None);
    }
    if let Some(pid) = lock_alive(stick) {
        return Err(busy(pid));
    }
    let mut note = Vec::new();
    if is_real_dir(target) && fs::read_dir(target).map_err(|e| failed("read", target, &e))?.next().is_some() {
        // Brave ran here without ever finishing a start (no Local State): kept aside, never deleted.
        let aside = (0..).map(|n| target.with_extension(if n == 0 { "before-takeover".into() } else { format!("before-takeover-{n}") })).find(|p| !p.exists());
        let aside = aside.ok_or_else(|| failed("aside", target, &"no free name"))?;
        fs::rename(target, &aside).map_err(|e| failed("aside", target, &e))?;
        note.push(format!("das angefangene Profil liegt jetzt in {}", aside.display()));
    }
    fs::DirBuilder::new().recursive(true).mode(0o700).create(target).map_err(|e| failed("mkdir", target, &e))?;
    fs::set_permissions(target, fs::Permissions::from_mode(0o700)).map_err(|e| failed("chmod", target, &e))?;
    let mut names: Vec<_> = fs::read_dir(stick).map_err(|e| failed("read", stick, &e))?.flatten().map(|e| e.file_name()).collect();
    names.sort();
    let mut moved = Vec::new();
    for n in names {
        let s = n.to_string_lossy();
        if s.starts_with("Singleton") || s == MOVED_NOTE {
            continue;
        }
        match fs::rename(stick.join(&n), target.join(&n)) {
            Ok(()) => moved.push(n),
            // Gone since the listing: nothing of it to move.
            Err(e) if e.kind() == ErrorKind::NotFound => {}
            Err(e) => {
                for m in moved.iter().rev() {
                    if let Err(u) = fs::rename(target.join(m), stick.join(m)) {
                        crate::event("pkg.run.profile.undo_failed", &[("entry", &m.to_string_lossy()), ("error", &u.to_string())]);
                    }
                }
                return Err(failed("move", &stick.join(&n), &e));
            }
        }
    }
    for l in ["SingletonLock", "SingletonSocket", "SingletonCookie"] {
        let _ = fs::remove_file(stick.join(l));
    }
    let _ = fs::write(stick.join(MOVED_NOTE), format!("{}\n", target.display()));
    crate::event("pkg.run.profile.taken", &[("from", &stick.to_string_lossy()), ("to", &target.to_string_lossy()), ("entries", &moved.len().to_string())]);
    note.insert(0, format!("Profil: das Profil des Sticks übernommen ({} Einträge von {} nach {})", moved.len(), stick.display(), target.display()));
    Ok(Some(note.join("; ")))
}

/// The downloads folder the view binds (the manifest wants it): made, uid's own and 0700, when the home folder is
/// the user's and has none yet (Chromium would try to make it inside the read-only view).
fn downloads(home: &Path, uid: u32) -> Option<String> {
    let d = home.join("Downloads");
    if fs::symlink_metadata(&d).is_ok() || !fs::symlink_metadata(home).is_ok_and(|m| m.is_dir() && m.uid() == uid) {
        return None;
    }
    match fs::DirBuilder::new().mode(0o700).create(&d) {
        Ok(()) => Some(format!("{} angelegt", d.display())),
        Err(e) => Some(format!("{} lässt sich nicht anlegen: {e}", d.display())),
    }
}
