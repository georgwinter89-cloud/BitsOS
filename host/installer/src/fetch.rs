//! Getting files: release assets from the project's GitHub release (or a folder the person downloaded them to,
//! `--assets DIR`), vendor files from their makers. Downloads go through `curl` (Windows 10 and later ship it in
//! System32; most Linux desktops have it, Ubuntu Desktop needs `sudo apt install curl`), into a cache folder, as
//! `<name>.part` that a later run resumes and that is renamed only when complete; nothing is used before its SHA-256
//! matched.

use crate::manifest::{self, Asset};
use crate::sha;
use std::path::{Path, PathBuf};
use std::process::Command;

pub struct Fetcher {
    /// `--assets DIR`: everything from there, nothing from the network.
    pub assets: Option<PathBuf>,
    /// Where downloads land (`<cache>/bitsos-install/<version>`).
    pub cache: PathBuf,
    /// Folders that may already hold vendor files (`--vendor-cache`), looked at before the network.
    pub vendor_dirs: Vec<PathBuf>,
    pub offline: bool,
}

pub fn default_cache() -> PathBuf {
    #[cfg(windows)]
    let base = std::env::var_os("LOCALAPPDATA").map(PathBuf::from);
    #[cfg(not(windows))]
    let base = std::env::var_os("XDG_CACHE_HOME").filter(|x| !x.is_empty()).map(PathBuf::from).or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")));
    base.unwrap_or_else(std::env::temp_dir).join("bitsos-install")
}

/// The curl to run. On Windows by its full path: a program started by name is looked for in the installer's own
/// folder first, and a `curl.exe` that sits beside it in Downloads would run with the installer's administrator rights.
fn curl() -> PathBuf {
    #[cfg(windows)]
    {
        let root = std::env::var_os("SystemRoot").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("C:\\Windows"));
        root.join("System32").join("curl.exe")
    }
    #[cfg(not(windows))]
    PathBuf::from("curl")
}

/// Whether curl can be started, asked at the start of a run that may download.
pub fn curl_ready() -> Result<(), String> {
    match Command::new(curl()).arg("--version").stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status() {
        Ok(s) if s.success() => Ok(()),
        _ => Err(format!(
            "cannot start {} for the downloads: install curl (Ubuntu and Debian: sudo apt install curl), or download the release files by hand and use --assets DIR",
            curl().display()
        )),
    }
}

/// What a curl exit status means for the person (curl's man page, EXIT CODES).
fn curl_says(code: Option<i32>) -> &'static str {
    match code {
        Some(22) => "the server answered with an error (not published, or gone)",
        Some(5..=7) => "no connection (is this computer online?)",
        Some(28) => "the connection stalled",
        Some(35 | 51 | 53 | 54 | 58 | 59 | 60 | 77 | 80 | 83 | 90 | 91) => "the secure connection failed (a wrong clock, or a proxy in between)",
        Some(23) => "the file could not be written (disk full?)",
        _ => "curl failed",
    }
}

/// Downloads `url` to `to` (via `to.part`, resumed when an earlier run left one).
pub fn download(url: &str, to: &Path) -> Result<(), String> {
    if let Some(d) = to.parent() {
        std::fs::create_dir_all(d).map_err(|e| format!("{}: {e}", d.display()))?;
    }
    let mut part = to.as_os_str().to_owned();
    part.push(".part");
    let part = PathBuf::from(part);
    let run = |resume: bool| {
        let mut c = Command::new(curl());
        c.args(["-fL", "--retry", "3", "--connect-timeout", "30", "--speed-limit", "1000", "--speed-time", "60", "--proto", "=https", "--tlsv1.2", "-#"]);
        if resume {
            c.args(["-C", "-"]);
        }
        c.arg("-o").arg(&part).arg(url).status().map_err(|e| format!("cannot start curl ({e}); install curl, or download the files by hand and use --assets DIR"))
    };
    let mut st = run(part.exists())?;
    // 33: the server will not resume; 416 arrives as 22 when the part is already longer than the file.
    if !st.success() && part.exists() && matches!(st.code(), Some(33 | 22)) {
        let _ = std::fs::remove_file(&part);
        st = run(false)?;
    }
    if !st.success() {
        return Err(format!("download failed: {} (curl exit {}): {url}\nRun the installer again to resume it.", curl_says(st.code()), st.code().map_or("?".into(), |c| c.to_string())));
    }
    std::fs::rename(&part, to).map_err(|e| format!("{}: {e}", to.display()))
}

/// A file with this size (None: any) and SHA-256 (None: any).
fn matches(p: &Path, size: Option<u64>, sha256: Option<&str>) -> Result<bool, String> {
    let Ok(m) = std::fs::metadata(p) else { return Ok(false) };
    if size.is_some_and(|s| s != m.len()) {
        return Ok(false);
    }
    match sha256 {
        Some(want) => Ok(sha::of_file(p).map_err(|e| format!("{}: {e}", p.display()))?.0 == want),
        None => Ok(true),
    }
}

impl Fetcher {
    /// The run downloads nothing at all (`--offline`): the cache or `--assets` has to hold everything.
    fn refuse_offline(&self, what: &str) -> Result<(), String> {
        if self.offline {
            return Err(format!("{what} is not in {} and the run is offline (--offline); give --assets DIR, or run without --offline", self.cache.display()));
        }
        Ok(())
    }

    pub fn manifest(&self, url: &str) -> Result<Vec<u8>, String> {
        let p = match &self.assets {
            Some(d) => d.join(manifest::FILE),
            None => {
                let p = self.cache.join(manifest::FILE);
                if !(self.offline && p.is_file()) {
                    self.refuse_offline(manifest::FILE)?;
                    // Always fresh: a cached manifest of a release that was replaced would only be refused.
                    let _ = std::fs::remove_file(&p);
                    download(url, &p)?;
                }
                p
            }
        };
        let b = std::fs::read(&p).map_err(|e| format!("{}: {e}", p.display()))?;
        if b.len() > 1 << 20 {
            return Err("the release manifest is larger than 1 MiB".into());
        }
        Ok(b)
    }

    /// The asset's file, checked against the manifest.
    pub fn asset(&self, a: &Asset, url: &str) -> Result<PathBuf, String> {
        if let Some(d) = &self.assets {
            let p = d.join(&a.name);
            manifest::verify_asset(&p, a)?;
            return Ok(p);
        }
        let p = self.cache.join(&a.name);
        if !matches(&p, Some(a.size), Some(&a.sha256))? {
            self.refuse_offline(&a.name)?;
            let _ = std::fs::remove_file(&p);
            println!("downloading {} ({} MB)", a.name, a.size >> 20);
            download(url, &p)?;
        }
        if let Err(e) = manifest::verify_asset(&p, a) {
            // A damaged download is not resumed next time; the next run fetches it whole.
            let _ = std::fs::remove_file(&p);
            return Err(e);
        }
        Ok(p)
    }

    /// A vendor file: from a `--vendor-cache` folder or the cache when it matches, else from `url`.
    pub fn vendor(&self, names: &[&str], url: &str, size: Option<u64>, sha256: Option<&str>) -> Result<PathBuf, String> {
        for d in self.vendor_dirs.iter().chain(std::iter::once(&self.cache.join("vendor"))) {
            for n in names {
                let p = d.join(n);
                if matches(&p, size, sha256)? && (size.is_some() || sha256.is_some() || p.is_file()) {
                    return Ok(p);
                }
            }
        }
        if self.offline {
            return Err(format!("{} is not in the vendor folders and the run is offline (--offline)", names[0]));
        }
        let p = self.cache.join("vendor").join(names[0]);
        println!("downloading {url}");
        download(url, &p)?;
        if !matches(&p, size, sha256)? {
            let _ = std::fs::remove_file(&p);
            return Err(format!("{url}: the download is not the pinned file (size or SHA-256 differ)"));
        }
        Ok(p)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fetcher(tag: &str) -> Fetcher {
        let cache = std::env::temp_dir().join(format!("bi-fetch-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&cache);
        std::fs::create_dir_all(&cache).unwrap();
        Fetcher { assets: None, cache, vendor_dirs: Vec::new(), offline: true }
    }

    #[test]
    fn offline_downloads_nothing() {
        // An unroutable https URL: reaching curl at all would be the failure this test is about.
        let url = "https://192.0.2.1/x";
        let f = fetcher("off");
        assert!(f.manifest(url).unwrap_err().contains("offline"));
        let a = manifest::Asset { name: "bitsos-system-1.tar.zst".into(), size: 3, sha256: sha::hex(b"sys"), kind: manifest::Kind::System, tree: None, needs: vec![], unpacked: 3, files: 1, vendor_bytes: 0, vendor_files: 0 };
        assert!(f.asset(&a, url).unwrap_err().contains("offline"));
        // What the cache already holds is used.
        std::fs::write(f.cache.join(manifest::FILE), b"{}").unwrap();
        assert_eq!(f.manifest(url).unwrap(), b"{}");
        std::fs::write(f.cache.join(&a.name), b"sys").unwrap();
        assert_eq!(f.asset(&a, url).unwrap(), f.cache.join(&a.name));
        // A cached file that does not match is not used.
        std::fs::write(f.cache.join(&a.name), b"syz").unwrap();
        assert!(f.asset(&a, url).unwrap_err().contains("offline"));
        let _ = std::fs::remove_dir_all(&f.cache);
    }

    #[test]
    fn curl_failures_are_said_plainly() {
        assert!(curl_says(Some(22)).contains("not published"));
        assert!(curl_says(Some(6)).contains("online"));
        assert!(curl_says(Some(60)).contains("clock"));
    }
}
