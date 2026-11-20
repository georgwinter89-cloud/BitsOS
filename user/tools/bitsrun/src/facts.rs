//! The facts a plan is made from, read from the file system: the tree, the system's files and folders, the user.

use crate::plan::{DEVICES, Kind, SYSTEM_ETC, SYSTEM_FONTS, System, Tree, USER_FOLDERS};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

pub struct FsTree(pub PathBuf);

impl FsTree {
    fn at(&self, rel: &str) -> PathBuf {
        if rel.is_empty() { self.0.clone() } else { self.0.join(rel) }
    }
}

impl Tree for FsTree {
    fn kind(&self, rel: &str) -> Kind {
        match fs::symlink_metadata(self.at(rel)) {
            Err(_) => Kind::Missing,
            Ok(m) if m.file_type().is_symlink() => Kind::Link,
            Ok(m) if m.is_dir() => Kind::Dir,
            Ok(m) if m.is_file() => Kind::File,
            Ok(_) => Kind::Other,
        }
    }
    fn list(&self, rel: &str) -> Vec<String> {
        let mut v: Vec<String> = match fs::read_dir(self.at(rel)) {
            Ok(rd) => rd.flatten().filter_map(|e| e.file_name().into_string().ok()).collect(),
            Err(_) => Vec::new(),
        };
        v.sort();
        v
    }
    fn link(&self, rel: &str) -> Option<String> {
        fs::read_link(self.at(rel)).ok().and_then(|t| t.into_os_string().into_string().ok())
    }
}

/// What the system offers the view; `etc` is the system's /etc (the image's), `home` the user's home folder.
pub fn system(etc: &Path, uid: u32, home: &str) -> System {
    let file = |p: &Path| fs::metadata(p).map(|m| m.is_file()).unwrap_or(false);
    let dir = |p: &str| Path::new(p).is_dir();
    let runtime = format!("/run/user/{uid}");
    System {
        etc: SYSTEM_ETC
            .iter()
            .filter(|n| file(&etc.join(n)))
            .map(|n| (n.to_string(), etc.join(n).to_string_lossy().into_owned()))
            .collect(),
        runtime_dir: dir(&runtime).then_some(runtime),
        x11: dir("/tmp/.X11-unix").then(|| "/tmp/.X11-unix".to_string()),
        sys: dir("/sys"),
        devices: DEVICES.iter().filter(|(_, d)| dir(d)).map(|(c, d)| (c.to_string(), d.to_string())).collect(),
        user_folders: USER_FOLDERS.iter().filter(|f| dir(&format!("{home}/{f}"))).map(|f| f.to_string()).collect(),
        fonts: dir(SYSTEM_FONTS).then(|| SYSTEM_FONTS.to_string()),
    }
}

/// (name, home) of `uid` in a passwd file (read up to 1 MiB; a hostile line is skipped).
pub fn passwd(path: &Path, uid: u32) -> Option<(String, String)> {
    let mut t = String::new();
    fs::File::open(path).ok()?.take(1 << 20).read_to_string(&mut t).ok()?;
    t.lines().find_map(|l| {
        let f: Vec<&str> = l.split(':').collect();
        (f.len() >= 7 && f[2].parse::<u32>().ok() == Some(uid) && f[5].starts_with('/')).then(|| (f[0].to_string(), f[5].to_string()))
    })
}

/// The home folder: `$HOME` when it is an absolute folder other than `/`, else the passwd entry's, else
/// `/data/home/<uid>` (init's PASSWD for user 1000).
pub fn home(env_home: Option<&str>, pw: Option<&(String, String)>, uid: u32) -> String {
    match env_home {
        Some(h) if h.starts_with('/') && h != "/" && Path::new(h).is_dir() && !h.contains("/../") => h.trim_end_matches('/').to_string(),
        _ => pw.map(|(_, h)| h.clone()).unwrap_or_else(|| format!("/data/home/{uid}")),
    }
}
