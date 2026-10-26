//! `$XDG_RUNTIME_DIR`, where the Wayland socket and the control socket live. V2's init sets none (change request
//! in wy-WY-1a.md: /run/user/<uid> from init), so without it bitscomp uses /run/user/<uid> when that exists, else
//! its own /tmp/bitscomp-<uid> (mode 0700; /run is root's), and exports the choice to the programs it starts.
//! `bitscomp ctl` follows the same rule, so it finds the socket without the variable too.
//!
//! Only the /tmp fallback is bitscomp's own and is held strictly: it must be a real directory (no symlink), ours,
//! and closed to others (a looser one of ours is set to 0700), else bitscomp refuses to put its sockets there,
//! since anyone can make that path in /tmp first. A directory named by the user or by init is only noted when loose.

use std::ffi::OsString;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

fn uid() -> u32 {
    // SAFETY: getuid has no failure and touches no memory.
    unsafe { libc::getuid() }
}

/// The directory by the rule above (nothing is made).
pub fn dir() -> PathBuf {
    choose(std::env::var_os("XDG_RUNTIME_DIR"), uid(), |p| p.is_dir())
}

/// The rule itself: `env` is XDG_RUNTIME_DIR, `is_dir` looks at /run/user/<uid>.
pub fn choose(env: Option<OsString>, uid: u32, is_dir: impl Fn(&Path) -> bool) -> PathBuf {
    if let Some(d) = env.filter(|d| !d.is_empty()) {
        return d.into();
    }
    let run = PathBuf::from(format!("/run/user/{uid}"));
    if is_dir(&run) {
        return run;
    }
    fallback(uid)
}

pub fn fallback(uid: u32) -> PathBuf {
    PathBuf::from(format!("/tmp/bitscomp-{uid}"))
}

/// Makes `d` if missing (0700) and checks it. `strict`: bitscomp's own fallback (see the header).
pub fn prepare(d: &Path, uid: u32, strict: bool) -> Result<(), String> {
    let err = |e: std::io::Error| format!("{}: {e}", d.display());
    match std::fs::symlink_metadata(d) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => std::fs::DirBuilder::new().mode(0o700).create(d).map_err(err)?,
        Err(e) => return Err(err(e)),
        Ok(_) => {}
    }
    let meta = std::fs::symlink_metadata(d).map_err(err)?;
    let mode = meta.permissions().mode() & 0o777;
    let problem = if !meta.file_type().is_dir() {
        Some("not a directory (a symlink?)".to_string())
    } else if meta.uid() != uid {
        Some(format!("belongs to uid {}", meta.uid()))
    } else if mode & 0o077 != 0 {
        if strict {
            std::fs::set_permissions(d, std::fs::Permissions::from_mode(0o700)).map_err(err)?;
        }
        (!strict).then(|| format!("mode {mode:o}"))
    } else {
        None
    };
    match problem {
        Some(p) if strict => Err(format!("{}: {p}: refusing to put the sockets there", d.display())),
        Some(p) => {
            eprintln!("bitscomp: note: {} is not ours alone ({p})", d.display());
            Ok(())
        }
        None => Ok(()),
    }
}

/// The directory for a bitscomp that drops to `target` (WY-7): XDG_RUNTIME_DIR only when it is the target's own
/// (`owner` reads a path's uid), else the rule for the target (/run/user/<target>, init makes it 0700 for 1000).
pub fn choose_for(env: Option<OsString>, target: u32, is_dir: impl Fn(&Path) -> bool, owner: impl Fn(&Path) -> Option<u32>) -> PathBuf {
    let env = env.filter(|d| !d.is_empty() && owner(Path::new(d)) == Some(target));
    choose(env, target, is_dir)
}

/// Chooses the directory and sets it as XDG_RUNTIME_DIR, before any thread exists (`drop_to`: the uid bitscomp
/// will run as; the variable is then always set, since root's own would not be the user's). Nothing is made yet:
/// `ensure` does that once bitscomp runs as its user.
pub fn select(drop_to: Option<u32>) -> PathBuf {
    let env = std::env::var_os("XDG_RUNTIME_DIR");
    let d = match drop_to {
        Some(t) => choose_for(env.clone(), t, |p| p.is_dir(), |p| std::fs::symlink_metadata(p).ok().map(|m| m.uid())),
        None => dir(),
    };
    if drop_to.is_some() || env.as_ref().is_none_or(|v| v.is_empty()) {
        if drop_to.is_some() && env.as_deref().is_some_and(|e| Path::new(e) != d) {
            eprintln!("bitscomp: XDG_RUNTIME_DIR {} is not the user's: {} instead", env.unwrap_or_default().display(), d.display());
        }
        // SAFETY: called at the start of `run`, before the event loop, back end or any child thread exists, so no
        // other thread reads the environment concurrently.
        unsafe { std::env::set_var("XDG_RUNTIME_DIR", &d) };
    }
    d
}

/// `d` made if needed and checked for this process's uid.
pub fn ensure(d: &Path) -> Result<(), String> {
    prepare(d, uid(), d == fallback(uid()))
}

/// WY-7: what an earlier bitscomp of another uid (a root start before the drop existed) left in `d`, which is
/// `owner`'s: its Wayland lock file would refuse our socket (we cannot open it for writing), its font cache would
/// refuse fontconfig. A lock nobody holds (flock) goes with its socket; a held one stays (that bitscomp still
/// runs, and the socket then says so). A foreign cache directory is moved aside (it cannot be emptied by us).
/// Called by a dropping root start while still root, so a lock root made 0600 can still be probed and removed.
pub fn clear_foreign(d: &Path, socket_name: &str, owner: u32) {
    let foreign = |p: &Path| std::fs::symlink_metadata(p).ok().is_some_and(|m| m.uid() != owner);
    let lock = d.join(format!("{socket_name}.lock"));
    if foreign(&lock) {
        match unheld(&lock) {
            Some(true) => {
                for p in [lock.clone(), d.join(socket_name)] {
                    match std::fs::remove_file(&p) {
                        Ok(()) => eprintln!("bitscomp: removed {} (left by another user's bitscomp)", p.display()),
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                        Err(e) => eprintln!("bitscomp: {}: {e}", p.display()),
                    }
                }
            }
            Some(false) => {}
            None => eprintln!(
                "bitscomp: {} belongs to another user and cannot be read: remove it and {} by hand",
                lock.display(),
                d.join(socket_name).display()
            ),
        }
    }
    let cache = d.join("cache");
    if foreign(&cache) {
        let aside = d.join(format!("cache.foreign-{}", std::process::id()));
        match std::fs::rename(&cache, &aside) {
            Ok(()) => eprintln!("bitscomp: {} belonged to another user: moved to {}", cache.display(), aside.display()),
            Err(e) => eprintln!("bitscomp: {}: belongs to another user, not moved: {e}", cache.display()),
        }
    }
}

/// Whether nobody holds `p`'s flock (a read-only open suffices for flock); None: it cannot be opened. Never
/// follows a symlink (root probes a directory the user owns).
fn unheld(p: &Path) -> Option<bool> {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::OpenOptionsExt;
    let f = std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW).open(p).ok()?;
    // SAFETY: flock on our own open fd; the lock goes with the fd at the end of this function.
    Some(unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) == 0 })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("bitscomp-rundir-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let _ = std::fs::remove_file(&d);
        d
    }

    #[test]
    fn fallback_order() {
        assert_eq!(choose(Some("/x/run".into()), 1000, |_| true), PathBuf::from("/x/run"));
        assert_eq!(choose(Some("".into()), 1000, |_| true), PathBuf::from("/run/user/1000"));
        assert_eq!(choose(None, 1000, |_| true), PathBuf::from("/run/user/1000"));
        assert_eq!(choose(None, 1000, |_| false), PathBuf::from("/tmp/bitscomp-1000"));
    }

    #[test]
    fn a_dropping_root_takes_the_users_directory() {
        let owner = |uid: u32| move |_: &Path| Some(uid);
        // The console shell's /run/user/1000 (init's, the user's): kept.
        assert_eq!(choose_for(Some("/run/user/1000".into()), 1000, |_| true, owner(1000)), PathBuf::from("/run/user/1000"));
        // Root's own or a missing one: the user's rule instead.
        assert_eq!(choose_for(Some("/run/user/0".into()), 1000, |_| true, owner(0)), PathBuf::from("/run/user/1000"));
        assert_eq!(choose_for(Some("/x".into()), 1000, |_| true, |_| None), PathBuf::from("/run/user/1000"));
        assert_eq!(choose_for(None, 1000, |_| false, owner(1000)), PathBuf::from("/tmp/bitscomp-1000"));
    }

    #[test]
    fn own_files_stay_and_a_held_lock_is_seen() {
        // Our own files are never touched (only another uid's are foreign); the flock probe sees a holder.
        let d = scratch("foreign-lock");
        prepare(&d, uid(), true).unwrap();
        std::fs::write(d.join("wayland-t.lock"), "").unwrap();
        std::fs::write(d.join("wayland-t"), "").unwrap();
        clear_foreign(&d, "wayland-t", uid());
        assert!(d.join("wayland-t.lock").exists() && d.join("wayland-t").exists());
        let f = std::fs::File::open(d.join("wayland-t.lock")).unwrap();
        use std::os::fd::AsRawFd;
        // SAFETY: flock on an fd this test holds open.
        assert_eq!(unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) }, 0);
        assert_eq!(unheld(&d.join("wayland-t.lock")), Some(false), "a held lock is held");
        drop(f);
        assert_eq!(unheld(&d.join("wayland-t.lock")), Some(true));
        assert_eq!(unheld(&d.join("missing.lock")), None);
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn another_users_stale_lock_socket_and_cache_go() {
        // Seen from `owner` (not us), our files are the foreign ones: an unheld lock goes with its socket, the
        // cache is moved aside; a held lock stays with its socket.
        let d = scratch("stale");
        prepare(&d, uid(), true).unwrap();
        for f in ["wayland-s.lock", "wayland-s", "wayland-h.lock", "wayland-h"] {
            std::fs::write(d.join(f), "").unwrap();
        }
        std::fs::create_dir(d.join("cache")).unwrap();
        let other = uid().wrapping_add(1);
        clear_foreign(&d, "wayland-s", other);
        assert!(!d.join("wayland-s.lock").exists() && !d.join("wayland-s").exists(), "the unheld lock and its socket go");
        assert!(!d.join("cache").exists(), "the foreign cache is moved aside");
        assert!(std::fs::read_dir(&d).unwrap().any(|e| e.unwrap().file_name().to_string_lossy().starts_with("cache.foreign-")));
        let held = std::fs::File::open(d.join("wayland-h.lock")).unwrap();
        use std::os::fd::AsRawFd;
        // SAFETY: flock on an fd this test holds open.
        assert_eq!(unsafe { libc::flock(held.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) }, 0);
        clear_foreign(&d, "wayland-h", other);
        assert!(d.join("wayland-h.lock").exists() && d.join("wayland-h").exists(), "a held lock stays");
        drop(held);
        std::os::unix::fs::symlink(d.join("wayland-h"), d.join("wayland-l.lock")).unwrap();
        assert_eq!(unheld(&d.join("wayland-l.lock")), None, "a symlinked lock is never followed");
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn made_with_0700_and_loose_one_tightened() {
        let d = scratch("made");
        prepare(&d, uid(), true).unwrap();
        assert_eq!(std::fs::metadata(&d).unwrap().permissions().mode() & 0o777, 0o700);
        std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o755)).unwrap();
        prepare(&d, uid(), true).unwrap();
        assert_eq!(std::fs::metadata(&d).unwrap().permissions().mode() & 0o777, 0o700);
        std::fs::remove_dir(&d).unwrap();
    }

    #[test]
    fn strict_refuses_foreign_or_symlink() {
        let d = scratch("foreign");
        prepare(&d, uid(), true).unwrap();
        assert!(prepare(&d, uid() + 1, true).is_err(), "another user's directory");
        assert!(prepare(&d, uid() + 1, false).is_ok(), "a named directory is only noted");
        let l = scratch("link");
        std::os::unix::fs::symlink(&d, &l).unwrap();
        assert!(prepare(&l, uid(), true).is_err(), "a symlink is not followed");
        std::fs::remove_file(&l).unwrap();
        std::fs::remove_dir(&d).unwrap();
    }
}
