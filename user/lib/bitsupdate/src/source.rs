//! Where versions come from and the token. `SOURCE.TXT` (optional, a test's or the desk's: `api=`, `repo=`, `ca=`
//! lines) in `\BITSOS2\UPDATE\`. The token (`update.tok`: Georg's fine-grained, read-only GitHub token for the
//! workshop repository, one line, typed in by him on the update page or the console) in `\BITSOS2\settings\`.
//!
//! Why that folder (notes, decision 4): every kernel since INT-CL3T (11.11., so Mini-PC 163 and the Gaming-PC
//! stick's 103-107 too) shows it root-only (fs-fat `protect.rs`: class Private) and keeps MCP's file tools out of
//! it (`settings_refusal`); a version that falls back or is gone back to is such an older kernel, and it must not
//! hand the token to programs or to MCP. settingsd reads only its `*.toml`, `journal.log`, `owners.list` there and
//! removes only its own `.new` names, so `update.tok` stays untouched. Not CONFIG.TXT: that is the kernel's file
//! (bitsboot and every kernel read it); a user-space secret has no business there.

use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

pub const DIR: &str = "/boot/BITSOS2/UPDATE";
pub const TOKEN_DIR: &str = "/boot/BITSOS2/settings";
pub const TOKEN: &str = "update.tok";
pub const SOURCE: &str = "SOURCE.TXT";
/// GitHub's fine-grained tokens (github_pat_...) and the classic ones (ghp_...); the test tokens of the scenario.
pub const TOKEN_PREFIXES: [&str; 3] = ["github_pat_", "ghp_", "test_"];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Source {
    pub api: String,
    pub repo: String,
    /// A CA bundle instead of the system's (a test CA).
    pub ca: Option<String>,
}

impl Default for Source {
    fn default() -> Self {
        Source { api: crate::github::API.into(), repo: crate::github::REPO.into(), ca: None }
    }
}

/// The source's folder (`BITSUPDATE_DIR` for host tests).
pub fn dir() -> PathBuf {
    std::env::var_os("BITSUPDATE_DIR").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(DIR))
}

/// The token's folder (host tests: `BITSUPDATE_DIR` holds both).
pub fn token_dir() -> PathBuf {
    std::env::var_os("BITSUPDATE_DIR").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(TOKEN_DIR))
}

pub fn parse_source(t: &str) -> Source {
    let mut s = Source::default();
    for l in t.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')) {
        let Some((k, v)) = l.split_once('=') else { continue };
        let v = v.trim().to_string();
        match k.trim() {
            "api" if v.starts_with("https://") => s.api = v,
            "repo" if v.split('/').count() == 2 && !v.contains(' ') => s.repo = v,
            "ca" if v.starts_with('/') => s.ca = Some(v),
            _ => {}
        }
    }
    s
}

pub fn source(d: &Path) -> Source {
    std::fs::read_to_string(d.join(SOURCE)).map(|t| parse_source(&t)).unwrap_or_default()
}

/// A token's form: a known prefix, then letters, digits and `_` only, 20 to 255 characters in all.
pub fn token_ok(t: &str) -> bool {
    (20..=255).contains(&t.len()) && TOKEN_PREFIXES.iter().any(|p| t.starts_with(p)) && t.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

pub fn read_token(d: &Path) -> Option<String> {
    let t = std::fs::read_to_string(d.join(TOKEN)).ok()?;
    let t = t.trim().to_string();
    token_ok(&t).then_some(t)
}

/// Stores the token (a sibling file, then renamed over the old one; 0600). Never logged.
pub fn store_token(d: &Path, t: &str) -> Result<(), String> {
    let t = t.trim();
    if !token_ok(t) {
        return Err(String::from("Das sieht nicht wie ein GitHub-Schlüssel aus (er beginnt mit github_pat_)."));
    }
    // settingsd's folder: made 0700 as settingsd makes it (the stick's FAT refuses the chmod and shows it root-only).
    std::fs::create_dir_all(d).map_err(|e| format!("{}: {e}", d.display()))?;
    let _ = std::fs::set_permissions(d, std::fs::Permissions::from_mode(0o700));
    let tmp = d.join("update.tok.new");
    let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp).map_err(|e| format!("{}: {e}", tmp.display()))?;
    f.write_all(format!("{t}\n").as_bytes()).and_then(|_| f.sync_all()).map_err(|e| format!("{}: {e}", tmp.display()))?;
    drop(f);
    std::fs::rename(&tmp, d.join(TOKEN)).map_err(|e| format!("{}: {e}", d.display()))
}

pub fn clear_token(d: &Path) -> Result<(), String> {
    match std::fs::remove_file(d.join(TOKEN)) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("{}: {e}", d.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_lines() {
        assert_eq!(parse_source(""), Source::default());
        let s = parse_source("# test\napi=https://10.0.2.2:24610\nrepo=a/b\nca=/boot/BITSOS2/UPDATE/ca.pem\nweird\napi=http://plain\n");
        assert_eq!(s, Source { api: "https://10.0.2.2:24610".into(), repo: "a/b".into(), ca: Some("/boot/BITSOS2/UPDATE/ca.pem".into()) });
    }

    #[test]
    fn tokens() {
        assert!(token_ok("github_pat_11ABCDEFG0123456789_abcdefXYZ"));
        assert!(token_ok("ghp_0123456789abcdefghijABCDEFGHIJ0123"));
        for bad in ["", "github_pat_", "hello_world_this_is_long", "github_pat_abc def ghi jkl mno", "github_pat_abc\ndefghijklmnop"] {
            assert!(!token_ok(bad), "{bad:?}");
        }
        let d = std::env::temp_dir().join(format!("bitsupdate-tok-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        assert_eq!(read_token(&d), None);
        assert!(store_token(&d, "nonsense").is_err());
        store_token(&d, " github_pat_11ABCDEFG0123456789_abcdef \n").unwrap();
        assert_eq!(read_token(&d).as_deref(), Some("github_pat_11ABCDEFG0123456789_abcdef"));
        assert_eq!(std::fs::metadata(d.join(TOKEN)).unwrap().permissions().mode() & 0o777, 0o600);
        clear_token(&d).unwrap();
        clear_token(&d).unwrap();
        assert_eq!(read_token(&d), None);
        let _ = std::fs::remove_dir_all(&d);
    }
}
