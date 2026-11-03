//! The grants on the stick, `\BITSOS2\mcp\grants` (`/boot/BITSOS2/mcp/grants`, next to bitsmcp's keys): one line
//! per grant (`grant::Grants::to_text`), written whole to a new file and renamed over the old one, so a cut
//! write leaves the old grants. Below `\BITSOS2\` the stick shows root's files (fs-fat glue): only root writes
//! there. The service may start before the stick is mounted (MC-8b CR5): it waits for the mount before it reads,
//! and while the stick is not mounted it keeps grants in memory only (said so in the log).

use crate::decide::Keep;
use crate::grant::Grants;
use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

pub const DIR: &str = "/boot/BITSOS2/mcp";
pub const MOUNT: &str = "/boot";
const NAME: &str = "grants";
/// A real file holds a handful of short lines; anything bigger is not read (FAT on the stick cannot keep
/// root-only modes, so the file is input like any other).
pub const FILE_MAX: u64 = 16 * 1024;

pub struct FileKeep {
    dir: PathBuf,
    /// The mount point that must be a mount (None: tests, any directory).
    mount: Option<PathBuf>,
    /// The file's name in `dir` (default `grants`).
    name: &'static str,
    /// MC-10: on a volume that keeps owners and modes (/data): the uid that must own `dir` and the file, neither
    /// writable by group or others, the file no symbolic link (None: the stick, where FAT keeps no modes).
    owner: Option<u32>,
}

/// O_NOFOLLOW (linux-6.6/include/uapi/asm-generic/fcntl.h: 00400000): the last part of the path must not be a link.
const O_NOFOLLOW: i32 = 0o400000;

fn dev(p: &Path) -> Option<u64> {
    fs::metadata(p).map(|m| m.dev()).ok()
}

impl FileKeep {
    pub fn on_stick(dir: &Path, mount: &Path) -> FileKeep {
        FileKeep { dir: dir.into(), mount: Some(mount.into()), name: NAME, owner: None }
    }
    pub fn in_dir(dir: &Path) -> FileKeep {
        FileKeep { dir: dir.into(), mount: None, name: NAME, owner: None }
    }

    /// The same keeping under another file name in `dir`.
    pub fn named(mut self, name: &'static str) -> FileKeep {
        self.name = name;
        self
    }

    /// MC-10: the directory and the file must belong to `uid` and be writable by nobody else (checked on every
    /// load and save); a file that is not is not read and not written over.
    pub fn owned_by(mut self, uid: u32) -> FileKeep {
        self.owner = Some(uid);
        self
    }

    /// Whether `m` is what `owned_by` asks for (a directory or a regular file of `uid`, no group/other write).
    fn owned(&self, m: &fs::Metadata, dir: bool, what: &Path) -> Result<(), String> {
        let Some(uid) = self.owner else { return Ok(()) };
        let kind_ok = if dir { m.file_type().is_dir() } else { m.file_type().is_file() };
        if !kind_ok {
            return Err(format!("{}: not a {}, not used", what.display(), if dir { "directory" } else { "regular file" }));
        }
        if m.uid() != uid || m.mode() & 0o022 != 0 {
            return Err(format!("{}: owner {} mode {:o}, not uid {uid} without group/other write; not used", what.display(), m.uid(), m.mode() & 0o7777));
        }
        Ok(())
    }

    /// The directory as `owned_by` wants it (None: it does not exist yet).
    fn dir_checked(&self) -> Result<Option<()>, String> {
        match fs::symlink_metadata(&self.dir) {
            Ok(m) => self.owned(&m, true, &self.dir).map(Some),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(format!("{}: {e}", self.dir.display())),
        }
    }

    fn mounted(&self) -> bool {
        self.mount.as_deref().is_none_or(|m| dev(m).is_some() && dev(m) != dev(Path::new("/")))
    }

    /// MC-10: the same file kept on another volume (the full-access grants on /data): `mount` must be a mount.
    pub fn on_volume(dir: &Path, mount: &Path) -> FileKeep {
        FileKeep::on_stick(dir, mount)
    }

    /// The file's text (None: there is none), waiting up to `budget` for the volume. A file that cannot be read
    /// is an error (said why), never a partial text.
    pub fn load_text(&self, budget: Duration) -> Result<Option<String>, String> {
        let until = Instant::now() + budget;
        while !self.mounted() {
            if Instant::now() >= until {
                let what = self.mount.as_deref().map_or_else(|| String::from("the volume"), |m| m.display().to_string());
                return Err(format!("{what} is not mounted after {} s", budget.as_secs()));
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        let p = self.dir.join(self.name);
        if self.owner.is_some() && self.dir_checked()?.is_none() {
            return Ok(None);
        }
        let mut raw = Vec::new();
        match fs::OpenOptions::new().read(true).custom_flags(if self.owner.is_some() { O_NOFOLLOW } else { 0 }).open(&p) {
            Ok(f) => {
                if self.owner.is_some() {
                    let m = f.metadata().map_err(|e| format!("{}: {e}", p.display()))?;
                    self.owned(&m, false, &p)?;
                }
                f.take(FILE_MAX + 1).read_to_end(&mut raw).map_err(|e| format!("{}: {e}", p.display()))?
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(format!("{}: {e}", p.display())),
        };
        if raw.len() as u64 > FILE_MAX {
            return Err(format!("{}: longer than {} KiB, not read", p.display(), FILE_MAX / 1024));
        }
        String::from_utf8(raw).map(Some).map_err(|_| format!("{}: not text, not read", p.display()))
    }

    /// The grants kept, waiting up to `budget` for the stick. A file that cannot be read gives none (said why).
    pub fn load(&self, budget: Duration) -> Result<Grants, String> {
        let Some(text) = self.load_text(budget)? else { return Ok(Grants::default()) };
        let p = self.dir.join(self.name);
        let (g, bad) = Grants::from_text(&text);
        if bad > 0 {
            crate::log::warn(&format!("{}: {bad} line(s) not taken (bad or longer than a day)", p.display()));
        }
        Ok(g)
    }
}

impl Keep for FileKeep {
    fn save(&self, text: &str) -> Result<(), String> {
        if !self.mounted() {
            return Err(String::from(if self.mount.as_deref() == Some(Path::new(MOUNT)) { "the stick is not mounted" } else { "the volume is not mounted" }));
        }
        let fail = |p: &Path, e: std::io::Error| format!("{}: {e}", p.display());
        fs::DirBuilder::new().recursive(true).mode(0o700).create(&self.dir).map_err(|e| fail(&self.dir, e))?;
        // A directory someone else made (or opened up) is not written into: what is in it is not ours.
        self.dir_checked()?;
        let tmp = self.dir.join(format!("{}.new", self.name));
        let dst = self.dir.join(self.name);
        let _ = fs::remove_file(&tmp);
        let mut f = fs::OpenOptions::new().write(true).create_new(true).custom_flags(O_NOFOLLOW).mode(0o600).open(&tmp).map_err(|e| fail(&tmp, e))?;
        f.write_all(text.as_bytes()).and_then(|_| f.sync_all()).map_err(|e| fail(&tmp, e))?;
        drop(f);
        fs::rename(&tmp, &dst).map_err(|e| fail(&dst, e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grant::Grants;
    use mcp_wire::installer::Kind;

    #[test]
    fn kept_and_read_back() {
        let dir = std::env::temp_dir().join(format!("bitsinstall-store-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let k = FileKeep::in_dir(&dir);
        assert_eq!(k.load(Duration::ZERO).unwrap(), Grants::default(), "no file: no grants");
        let mut g = Grants::default();
        g.set(Kind::Install, 2_000_000_600, 2_000_000_000).unwrap();
        k.save(&g.to_text()).unwrap();
        assert_eq!(k.load(Duration::ZERO).unwrap(), g);
        assert_eq!(fs::metadata(dir.join("grants")).unwrap().mode() & 0o777, 0o600);
        fs::write(dir.join("grants"), "install x y\n").unwrap();
        assert_eq!(k.load(Duration::ZERO).unwrap(), Grants::default(), "a bad file gives no grant");
        let _ = fs::remove_dir_all(&dir);
        let not = FileKeep::on_stick(&dir, Path::new("/"));
        assert!(not.save("").unwrap_err().contains("not mounted"), "/ is never the stick's mount");
        assert!(not.load(Duration::ZERO).is_err());
    }

    /// The file is input (anyone with the stick in another machine can write it): whatever it holds, loading
    /// never panics and never yields more than well-formed grants of at most a day; what the clock says is
    /// not now is swept at start.
    #[test]
    fn a_hostile_file_gives_no_grant() {
        const T: i64 = 2_000_000_000;
        let dir = std::env::temp_dir().join(format!("bitsinstall-hostile-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let k = FileKeep::in_dir(&dir);
        let f = dir.join("grants");
        let none = |what: &str| {
            let g = k.load(Duration::ZERO).unwrap_or_default();
            assert!(g.all().is_empty(), "{what}: {g:?}");
        };
        fs::write(&f, [0xffu8, 0xfe, 0x00, b'\n', 0xc3]).unwrap();
        assert!(k.load(Duration::ZERO).unwrap_err().contains("not text"));
        none("bytes");
        fs::write(&f, format!("install {}", T + 60).as_bytes()).unwrap();
        none("cut after the second word");
        fs::write(&f, format!("install {} {}\n", T + 60, T).repeat(2000)).unwrap();
        assert!(k.load(Duration::ZERO).unwrap_err().contains("longer than"));
        none("oversized");
        fs::write(&f, format!("nvme {} {}\nother {} {}\nsettings9 {} {}\n", T + 60, T, T + 60, T, T + 60, T)).unwrap();
        none("never granted or unknown kinds");
        fs::write(&f, format!("install {} {}\npin {} {}\nreload -5 -9999999999999999999\nreboot {} {}\n", i64::MAX, i64::MIN, T + 60, T + 60, T, T + 1)).unwrap();
        none("overflowing, empty and backwards times");
        // A day long but set ten years ahead: read, and swept before it can count (the clock is not there).
        let ahead = T + 10 * 365 * 86_400;
        fs::write(&f, format!("install {} {}\n", ahead + 600, ahead)).unwrap();
        let mut g = k.load(Duration::ZERO).unwrap();
        assert_eq!(g.covering(Kind::Install, T), None);
        assert_eq!(g.sweep(T).len(), 1);
        assert!(g.all().is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    /// MC-10: on /data the store must be root's own: a directory or file someone else made, one others may
    /// write, or a symbolic link in the file's place is not read (no grant comes from it) and not written into.
    #[test]
    fn a_planted_store_is_not_taken() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("bitsinstall-owner-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::DirBuilder::new().mode(0o700).create(&dir).unwrap();
        let me = fs::metadata(&dir).unwrap().uid();
        let k = FileKeep::in_dir(&dir).owned_by(me);
        let f = dir.join("grants");
        k.save("a\n").unwrap();
        assert_eq!(k.load_text(Duration::ZERO).unwrap().as_deref(), Some("a\n"), "its own store is read");
        fs::set_permissions(&f, fs::Permissions::from_mode(0o622)).unwrap();
        assert!(k.load_text(Duration::ZERO).unwrap_err().contains("not used"), "a file others may write");
        fs::set_permissions(&f, fs::Permissions::from_mode(0o600)).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o777)).unwrap();
        assert!(k.load_text(Duration::ZERO).unwrap_err().contains("not used"), "a directory others may write");
        assert!(k.save("b\n").is_err(), "nothing written into it");
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
        fs::remove_file(&f).unwrap();
        let elsewhere = dir.join("planted");
        fs::write(&elsewhere, "c\n").unwrap();
        std::os::unix::fs::symlink(&elsewhere, &f).unwrap();
        assert!(k.load_text(Duration::ZERO).is_err(), "a symbolic link in the file's place");
        assert!(FileKeep::in_dir(&dir).owned_by(me.wrapping_add(1)).load_text(Duration::ZERO).unwrap_err().contains("not used"), "another owner");
        k.save("d\n").unwrap();
        assert_eq!(fs::read_to_string(&elsewhere).unwrap(), "c\n", "the save replaced the link, never wrote through it");
        assert_eq!(k.load_text(Duration::ZERO).unwrap().as_deref(), Some("d\n"));
        let _ = fs::remove_dir_all(&dir);
    }
}
