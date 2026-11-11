//! The files QEMU mode makes and the record of them: everything lives in one folder except the shortcuts, and
//! `installed.txt` there names every file a run made, so `--uninstall` removes those and nothing else.

use super::args::Os;
use std::path::{Path, PathBuf};

pub struct Layout {
    pub dir: PathBuf,
    pub os: Os,
}

/// Every name QEMU mode ever creates; a record line naming anything else is ignored by `--uninstall`.
pub const OWN_NAMES: &[&str] = &[
    "bitsos.img",
    "bitsos.img.part",
    "bitsos-vars.fd",
    "start-bitsos.sh",
    "start-bitsos.cmd",
    "bitsos-serial.log",
    "qemu.pid",
    "qemu-output.log",
    "token.txt",
    "installed.txt",
    "bitsos.desktop",
    "BitsOS.lnk",
    "BitsOS.cmd",
];

impl Layout {
    pub fn image(&self) -> PathBuf {
        self.dir.join("bitsos.img")
    }
    /// A new image is written here and renamed once complete: a failed run leaves no half image behind the name.
    pub fn image_part(&self) -> PathBuf {
        self.dir.join("bitsos.img.part")
    }
    pub fn vars(&self) -> PathBuf {
        self.dir.join("bitsos-vars.fd")
    }
    pub fn script(&self) -> PathBuf {
        self.dir.join(if self.os == Os::Windows { "start-bitsos.cmd" } else { "start-bitsos.sh" })
    }
    pub fn log(&self) -> PathBuf {
        self.dir.join("bitsos-serial.log")
    }
    pub fn pidfile(&self) -> PathBuf {
        self.dir.join("qemu.pid")
    }
    pub fn output(&self) -> PathBuf {
        self.dir.join("qemu-output.log")
    }
    pub fn token(&self) -> PathBuf {
        self.dir.join("token.txt")
    }
    pub fn record(&self) -> PathBuf {
        self.dir.join("installed.txt")
    }

    fn own(&self) -> Vec<PathBuf> {
        vec![self.image(), self.image_part(), self.vars(), self.script(), self.log(), self.pidfile(), self.output(), self.token(), self.record()]
    }

    /// Adds `p` to the record.
    pub fn remember(&self, p: &Path) -> Result<(), String> {
        let r = self.record();
        let mut t = std::fs::read_to_string(&r).unwrap_or_default();
        let line = p.display().to_string();
        if !t.lines().any(|l| l == line) {
            t.push_str(&line);
            t.push('\n');
            std::fs::write(&r, t).map_err(|e| format!("{}: {e}", r.display()))?;
        }
        Ok(())
    }

    /// What `--uninstall` removes: the folder's own files and the recorded ones, each only if it is still there, is
    /// a file (or a link, removed itself) and carries one of QEMU mode's names.
    pub fn to_remove(&self) -> Vec<PathBuf> {
        let recorded: Vec<PathBuf> = std::fs::read_to_string(self.record()).unwrap_or_default().lines().map(PathBuf::from).filter(|p| p.is_absolute()).collect();
        let mut out: Vec<PathBuf> = Vec::new();
        for p in self.own().into_iter().chain(recorded) {
            let named = p.file_name().is_some_and(|n| OWN_NAMES.contains(&n.to_string_lossy().as_ref()));
            let file = std::fs::symlink_metadata(&p).is_ok_and(|m| !m.is_dir());
            // A launcher or shortcut another image folder's run wrote over this one's belongs to that run now.
            let text_launcher = p.file_name().is_some_and(|n| n == "bitsos.desktop" || n == "BitsOS.cmd");
            let script = self.script().display().to_string();
            let ours = if text_launcher {
                std::fs::read_to_string(&p).is_ok_and(|t| t.contains(&script) || t.contains(&super::args::desktop_exec(&self.script())))
            } else if p.file_name().is_some_and(|n| n == "BitsOS.lnk") {
                std::fs::read(&p).is_ok_and(|b| super::win::shortcut_names(&b, &script))
            } else {
                true
            };
            if named && file && ours && !out.contains(&p) {
                out.push(p);
            }
        }
        // The record last: an interrupted removal can be run again.
        out.sort_by_key(|p| *p == self.record());
        out
    }
}

/// `$XDG_DATA_HOME` or `~/.local/share` (Linux); `%LOCALAPPDATA%` (Windows).
pub fn data_home(os: Os, env: &dyn Fn(&str) -> Option<String>) -> Option<PathBuf> {
    let set = |k: &str| env(k).filter(|v| !v.is_empty()).map(PathBuf::from);
    match os {
        Os::Windows => set("LOCALAPPDATA"),
        Os::Linux => set("XDG_DATA_HOME").filter(|p| p.is_absolute()).or_else(|| set("HOME").map(|h| h.join(".local/share"))),
    }
}

pub fn default_dir(os: Os, env: &dyn Fn(&str) -> Option<String>) -> Option<PathBuf> {
    data_home(os, env).map(|d| d.join(if os == Os::Windows { "BitsOS" } else { "bitsos" }))
}

/// The desktop folder of a Linux desktop: `$XDG_DESKTOP_DIR`, its line in `user-dirs.dirs`
/// (`XDG_DESKTOP_DIR="$HOME/Schreibtisch"`), else `~/Desktop`; only one that exists.
pub fn desktop_dir(env: &dyn Fn(&str) -> Option<String>) -> Option<PathBuf> {
    let home = env("HOME").filter(|h| !h.is_empty()).map(PathBuf::from)?;
    let config = env("XDG_CONFIG_HOME").filter(|v| !v.is_empty()).map(PathBuf::from).unwrap_or_else(|| home.join(".config"));
    let from_file = std::fs::read_to_string(config.join("user-dirs.dirs")).ok().and_then(|t| {
        t.lines().find_map(|l| {
            let v = l.trim().strip_prefix("XDG_DESKTOP_DIR=")?.trim().trim_matches('"');
            Some(match v.strip_prefix("$HOME") {
                Some(rest) => home.join(rest.trim_start_matches('/')),
                None => PathBuf::from(v),
            })
        })
    });
    let d = env("XDG_DESKTOP_DIR").filter(|v| !v.is_empty()).map(PathBuf::from).or(from_file).unwrap_or_else(|| home.join("Desktop"));
    // A desktop folder that is the home folder itself (some minimal setups) is no desktop.
    (d.is_dir() && d != home).then_some(d)
}
