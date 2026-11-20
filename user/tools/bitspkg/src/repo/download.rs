//! Downloads through `/bin/fetch` (UL-4a), one process per file, as pacman 7 downloads: as an unprivileged user
//! into a temporary folder that user owns, after which root takes the file over (pacman/lib/libalpm/util.c:959
//! _alpm_download_dir_setup, dload.c:917 curl_download_internal_sandboxed, dload.c:76 finalize_download_file).
//! Here the user is `nobody` (65534, init's passwd: there is no `alpm` user on V2); the drop happens in the child
//! through std's `Command::uid/gid` (setgroups, setgid, setuid before exec), so bitspkg stays without unsafe.
//!
//! Why a process per file and not fetch as a library: the uid drop needs a process of its own (a thread cannot
//! give up root alone without unsafe raw syscalls, and the C library's setuid changes every thread), and a
//! crashed or hostile-input-confused TLS stack then never runs as root. The cost for a 400-file install (PK-6d):
//! 400 process starts (a few ms each on V2) and 400 TLS handshakes (one round trip plus a signature check, tens of
//! ms), against minutes of download: under 5 % even before PK-6d runs several fetches at once. fetch's own
//! bounds (connect and idle timeout, --max-time, --max-size) bound every network wait (B4); bitspkg's wait for the
//! child has a deadline of its own beyond that.

use crate::fsx;
use std::fs::{self, File, OpenOptions};
use std::io::Read;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

pub const NOBODY: u32 = 65534;
const O_NOFOLLOW: i32 = 0o400000;

#[derive(Clone, Debug)]
pub struct Fetcher {
    pub exe: PathBuf,
    /// (uid, gid) the download runs as; None = as we are (not root: a test, or a user's own root).
    pub drop_to: Option<(u32, u32)>,
    /// fetch's idle limit and limit per file (seconds).
    pub timeout: u64,
    pub max_time: u64,
}

#[derive(Debug, PartialEq)]
pub enum Got {
    /// The file, owned by root (or us) and on disk.
    File(PathBuf),
    /// The server answered, but not with the file (HTTP status not 2xx): an optional file is simply absent.
    Missing(String),
    Failed(String),
}

impl Fetcher {
    /// `/bin/fetch` (or `BITSPKG_FETCH`); root drops to nobody.
    pub fn standard() -> Fetcher {
        let exe = std::env::var_os("BITSPKG_FETCH").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/bin/fetch"));
        let root = usys::geteuid().map(|u| u == 0).unwrap_or(false);
        Fetcher { exe, drop_to: root.then_some((NOBODY, NOBODY)), timeout: 30, max_time: 600 }
    }

    /// This fetcher with a whole-transfer limit that fits `bytes` at 200 KiB/s (a slow WLAN): never below the
    /// standard limit, at most 6 h; fetch's idle `--timeout` still ends a stalled transfer (B4).
    pub fn sized(&self, bytes: u64) -> Fetcher {
        let mut f = self.clone();
        f.max_time = self.max_time.max((bytes / (200 << 10)).min(6 * 3600));
        f
    }

    /// A private download folder below `dir` (`download-<pid>`), owned by the download user.
    pub fn workdir(&self, dir: &Path) -> Result<WorkDir, String> {
        let p = dir.join(format!("download-{}", std::process::id()));
        if fs::symlink_metadata(&p).is_ok() {
            fsx::remove_tree(&p)?;
        }
        fs::create_dir(&p).map_err(|e| fsx::err(&p, e))?;
        fs::set_permissions(&p, fs::Permissions::from_mode(0o700)).map_err(|e| fsx::err(&p, e))?;
        if let Some((u, g)) = self.drop_to {
            std::os::unix::fs::chown(&p, Some(u), Some(g)).map_err(|e| fsx::err(&p, e))?;
        }
        Ok(WorkDir { path: p, owner: self.drop_to.map(|d| d.0) })
    }

    /// GETs `url` into `wd/name`, at most `max` bytes, and hands the file over (owner root or us, mode 0644).
    pub fn get(&self, wd: &WorkDir, url: &str, name: &str, max: u64) -> Got {
        let out = wd.path.join(name);
        let _ = fs::remove_file(&out);
        let mut c = Command::new(&self.exe);
        c.arg("-q")
            .arg("-o")
            .arg(&out)
            .arg("--timeout")
            .arg(self.timeout.to_string())
            .arg("--max-time")
            .arg(self.max_time.to_string())
            .arg("--max-size")
            .arg(max.to_string())
            .arg("--")
            .arg(url)
            .stdin(Stdio::null())
            .stdout(Stdio::null());
        // fetch's few stderr lines go to a file of root's beside the folder (no reader thread, no full pipe).
        let errf = wd.err_file();
        match File::create(&errf) {
            Ok(f) => c.stderr(f),
            Err(e) => return Got::Failed(fsx::err(&errf, e)),
        };
        if let Some((u, g)) = self.drop_to {
            c.uid(u).gid(g);
        }
        let mut child = match c.spawn() {
            Ok(ch) => ch,
            Err(e) => return Got::Failed(format!("{} startet nicht: {e}", self.exe.display())),
        };
        let deadline = Instant::now() + Duration::from_secs(self.max_time + self.timeout + 10);
        let status = loop {
            match child.try_wait() {
                Ok(Some(s)) => break Ok(s),
                Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
                Ok(None) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    break Err("fetch hängt über seine eigene Frist hinaus und wurde beendet".to_string());
                }
                Err(e) => break Err(format!("fetch: {e}")),
            }
        };
        let msg = String::from_utf8_lossy(&read_small(&errf, 64 << 10).unwrap_or_default()).into_owned();
        let _ = fs::remove_file(&errf);
        let msg = msg.lines().rfind(|l| !l.trim().is_empty()).unwrap_or("").trim_start_matches("fetch: ").to_string();
        let status = match status {
            Ok(s) => s,
            Err(m) => return Got::Failed(m),
        };
        match status.code() {
            Some(0) => {}
            Some(7) => return Got::Missing(msg),
            Some(12) => return Got::Failed(format!("zu groß: {msg}")),
            Some(c) => return Got::Failed(format!("{msg} (fetch {c})")),
            None => return Got::Failed("fetch durch ein Signal beendet".into()),
        }
        match take_over(&out, wd.owner) {
            Ok(()) => Got::File(out),
            Err(e) => Got::Failed(e),
        }
    }
}

/// Root's side of the hand-over (dload.c:76): only a regular file with one name, owned by the download user, is
/// taken (a link planted in the folder is refused, never followed); it becomes root's, 0644, and is synced.
fn take_over(p: &Path, owner: Option<u32>) -> Result<(), String> {
    let f = OpenOptions::new().read(true).custom_flags(O_NOFOLLOW).open(p).map_err(|e| fsx::err(p, e))?;
    let md = f.metadata().map_err(|e| fsx::err(p, e))?;
    if !md.is_file() || md.nlink() != 1 {
        return Err(fsx::err(p, "keine gewöhnliche Datei mit einem Namen: abgelehnt"));
    }
    if let Some(u) = owner {
        if md.uid() != u {
            return Err(fsx::err(p, format!("gehört uid {} statt {u}: abgelehnt", md.uid())));
        }
        std::os::unix::fs::fchown(&f, Some(0), Some(0)).map_err(|e| fsx::err(p, e))?;
    }
    f.set_permissions(fs::Permissions::from_mode(0o644)).map_err(|e| fsx::err(p, e))?;
    f.sync_all().map_err(|e| fsx::err(p, e))
}

pub struct WorkDir {
    pub path: PathBuf,
    owner: Option<u32>,
}

impl WorkDir {
    fn err_file(&self) -> PathBuf {
        self.path.with_extension("err")
    }
}

impl Drop for WorkDir {
    fn drop(&mut self) {
        let _ = fsx::remove_tree(&self.path);
        let _ = fs::remove_file(self.err_file());
    }
}

/// A small file's bytes (a `.sig`, `lastupdate`), bounded.
pub fn read_small(p: &Path, max: u64) -> Result<Vec<u8>, String> {
    let mut v = Vec::new();
    File::open(p).map_err(|e| fsx::err(p, e))?.take(max + 1).read_to_end(&mut v).map_err(|e| fsx::err(p, e))?;
    if v.len() as u64 > max {
        return Err(fsx::err(p, format!("größer als {max} Bytes")));
    }
    Ok(v)
}
