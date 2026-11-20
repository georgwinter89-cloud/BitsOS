//! Downloads through `/bin/fetch` (UL-4a), one process per file as bitspkg does (repo/download.rs): as `nobody`
//! when we are root, into a folder only that user can write, so a confused TLS stack never runs as root. The
//! token goes on fetch's stdin (`--auth-stdin`), never on its command line; fetch sends it to the API's own host
//! only and drops it on the redirect to GitHub's storage host.

use std::io::Write;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

pub const NOBODY: u32 = 65534;

/// Why a download did not come (fetch's exit codes, its own `--help`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Got {
    Status(u16),
    Network(String),
    Tls(String),
    Clock,
    TooLarge,
    Other(String),
}

#[derive(Clone, Debug)]
pub struct Fetcher {
    pub exe: PathBuf,
    pub drop_to: Option<(u32, u32)>,
    pub ca: Option<String>,
    /// Idle limit and whole-file limit (s).
    pub timeout: u64,
    pub max_time: u64,
}

impl Fetcher {
    pub fn standard(exe: PathBuf, ca: Option<String>) -> Fetcher {
        let root = usys::geteuid().map(|u| u == 0).unwrap_or(false);
        Fetcher { exe, drop_to: root.then_some((NOBODY, NOBODY)), ca, timeout: 30, max_time: 600 }
    }

    /// The folder downloads go to: made for the user fetch runs as, empty.
    pub fn folder(&self, path: &Path) -> Result<(), String> {
        let _ = std::fs::remove_dir_all(path);
        std::fs::create_dir_all(path).map_err(|e| format!("{}: {e}", path.display()))?;
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).map_err(|e| format!("{}: {e}", path.display()))?;
        if let Some((u, g)) = self.drop_to {
            std::os::unix::fs::chown(path, Some(u), Some(g)).map_err(|e| format!("{}: {e}", path.display()))?;
        }
        Ok(())
    }

    /// GET `url` into `out` (at most `max` bytes), with `accept` and the token.
    pub fn get(&self, url: &str, out: &Path, accept: &str, token: Option<&str>, max: u64) -> Result<(), Got> {
        let mut c = Command::new(&self.exe);
        c.args(["-q", "--timeout", &self.timeout.to_string(), "--max-time", &self.max_time.max(max / (200 << 10)).to_string()]);
        c.args(["--max-size", &max.to_string(), "--accept", accept, "-o"]).arg(out);
        if let Some(ca) = &self.ca {
            c.args(["--ca", ca]);
        }
        if token.is_some() {
            c.arg("--auth-stdin");
        }
        c.arg("--").arg(url).stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::piped());
        if let Some((u, g)) = self.drop_to {
            c.uid(u).gid(g);
        }
        let mut child = c.spawn().map_err(|e| Got::Other(format!("{}: {e}", self.exe.display())))?;
        if let Some(mut si) = child.stdin.take() {
            if let Some(t) = token {
                let _ = si.write_all(format!("{t}\n").as_bytes());
            }
        }
        let o = child.wait_with_output().map_err(|e| Got::Other(format!("fetch: {e}")))?;
        let err = String::from_utf8_lossy(&o.stderr).trim().to_string();
        match o.status.code() {
            Some(0) => Ok(()),
            Some(7) => Err(Got::Status(status_of(&err).unwrap_or(0))),
            Some(3 | 4 | 11) => Err(Got::Network(err)),
            Some(5) => Err(Got::Tls(err)),
            Some(6) => Err(Got::Clock),
            Some(12) => Err(Got::TooLarge),
            _ => Err(Got::Other(err)),
        }
    }
}

/// The status in fetch's "fetch: HTTP 401 Unauthorized".
pub fn status_of(stderr: &str) -> Option<u16> {
    let rest = &stderr[stderr.find("HTTP ")? + 5..];
    rest.split_whitespace().next()?.parse().ok()
}

#[cfg(test)]
mod tests {
    #[test]
    fn statuses() {
        assert_eq!(super::status_of("fetch: HTTP 401 Unauthorized"), Some(401));
        assert_eq!(super::status_of("fetch: HTTP 404 "), Some(404));
        assert_eq!(super::status_of("fetch: connect failed"), None);
    }
}
