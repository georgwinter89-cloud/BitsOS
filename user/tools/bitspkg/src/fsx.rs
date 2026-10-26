//! File system steps with the order a cut can survive: write, fsync, then rename into place; folders synced
//! after names change in them. Runs the same on CachyOS and on V2 (renameat2 through usys).

use crate::cut::step;
use sha2::{Digest, Sha256};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;

/// x86_64 Linux open flags (linux-6.6/include/uapi/asm-generic/fcntl.h): O_NOFOLLOW refuses a package file that
/// is a symlink; O_NONBLOCK keeps a FIFO from blocking the open (fs/fifo.c:fifo_open waits for a writer without
/// it) and does nothing on a regular file; O_NOCTTY keeps a tty from becoming our controlling terminal.
const O_NOFOLLOW: i32 = 0o400000;
const O_NONBLOCK: i32 = 0o4000;
const O_NOCTTY: i32 = 0o400;
const OPEN_SAFE: i32 = O_NOFOLLOW | O_NONBLOCK | O_NOCTTY;
const RENAME_EXCHANGE: usize = 2;
const AT_FDCWD: isize = -100;
const CHUNK: usize = 256 << 10;

pub fn s(p: &Path) -> Result<&str, String> {
    p.to_str().ok_or_else(|| format!("{}: path is not UTF-8", p.display()))
}

pub fn err(p: &Path, e: impl std::fmt::Display) -> String {
    format!("{}: {e}", p.display())
}

/// Copies `src` (never through a final symlink) to the new file `dst` with `mode`, hashing on the way; the copy
/// is on disk when this returns.
pub fn copy_hashed(src: &Path, dst: &Path, mode: u32) -> Result<(String, u64), String> {
    let mut i = open_regular(src)?;
    let mut o = OpenOptions::new().write(true).create_new(true).mode(mode).open(dst).map_err(|e| err(dst, e))?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; CHUNK];
    let mut size = 0u64;
    loop {
        let n = i.read(&mut buf).map_err(|e| err(src, e))?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
        let half = n / 2;
        o.write_all(&buf[..half]).map_err(|e| err(dst, e))?;
        if size == 0 {
            step("file half written");
        }
        o.write_all(&buf[half..n]).map_err(|e| err(dst, e))?;
        size += n as u64;
    }
    o.sync_all().map_err(|e| err(dst, e))?;
    // The mode given to open is cut by the umask; package files get exactly theirs.
    fs::set_permissions(dst, fs::Permissions::from_mode(mode)).map_err(|e| err(dst, e))?;
    Ok((crate::names::hex(&h.finalize()), size))
}

/// Opens `p` for reading only if it is a regular file: never through a final symlink, never blocking on a FIFO,
/// never taking a tty; the type is checked on the open file (fstat), so a swap after a check cannot slip through.
fn open_regular(p: &Path) -> Result<File, String> {
    let f = OpenOptions::new().read(true).custom_flags(OPEN_SAFE).open(p).map_err(|e| err(p, e))?;
    if !f.metadata().map_err(|e| err(p, e))?.is_file() {
        return Err(err(p, "not a regular file (device, pipe and socket files cannot be packaged)"));
    }
    Ok(f)
}

/// SHA-256 and size of a file (never through a final symlink).
pub fn hash_file(p: &Path) -> Result<(String, u64), String> {
    let mut i = open_regular(p)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; CHUNK];
    let mut size = 0u64;
    loop {
        let n = i.read(&mut buf).map_err(|e| err(p, e))?;
        if n == 0 {
            return Ok((crate::names::hex(&h.finalize()), size));
        }
        h.update(&buf[..n]);
        size += n as u64;
    }
}

/// `<p>.new` (appended, so `a.x` and `a.y` never share one temporary name; `Store::recover` removes them).
pub fn tmp_name(p: &Path) -> std::path::PathBuf {
    let mut n = p.file_name().unwrap_or_default().to_os_string();
    n.push(".new");
    p.with_file_name(n)
}

/// Writes a small file atomically: `<p>.new`, fsync, rename over `p`, sync the folder.
pub fn write_atomic(p: &Path, data: &[u8], mode: u32) -> Result<(), String> {
    let tmp = tmp_name(p);
    let _ = fs::remove_file(&tmp);
    let mut f = OpenOptions::new().write(true).create_new(true).mode(mode).open(&tmp).map_err(|e| err(&tmp, e))?;
    f.write_all(data).map_err(|e| err(&tmp, e))?;
    f.sync_all().map_err(|e| err(&tmp, e))?;
    drop(f);
    fs::set_permissions(&tmp, fs::Permissions::from_mode(mode)).map_err(|e| err(&tmp, e))?;
    step("rename file into place");
    fs::rename(&tmp, p).map_err(|e| err(p, e))?;
    sync_parent(p)
}

/// Points the symlink `at` to `target` atomically: a new link `<at>.new`, renamed over `at`.
pub fn set_link(at: &Path, target: &str) -> Result<(), String> {
    let tmp = tmp_name(at);
    let _ = fs::remove_file(&tmp);
    std::os::unix::fs::symlink(target, &tmp).map_err(|e| err(&tmp, e))?;
    step("switch link");
    fs::rename(&tmp, at).map_err(|e| err(at, e))?;
    sync_parent(at)
}

pub fn read_link(at: &Path) -> Option<String> {
    fs::read_link(at).ok().and_then(|t| t.to_str().map(str::to_string))
}

/// Swaps two names in one step (renameat2 RENAME_EXCHANGE, Linux 3.15; tmpfs and ext4 on V2, FS-1).
pub fn exchange(a: &Path, b: &Path) -> Result<(), String> {
    step("exchange links");
    usys::renameat2(AT_FDCWD, s(a)?, AT_FDCWD, s(b)?, RENAME_EXCHANGE).map(|_| ()).map_err(|e| err(a, e))?;
    sync_parent(a)
}

/// Moves a sealed version folder elsewhere: moving a folder to another parent needs write access to the folder
/// itself (its `..` changes; linux-6.6/fs/namei.c:4802 vfs_rename), so it is opened up first.
pub fn move_dir(a: &Path, b: &Path) -> Result<(), String> {
    fs::set_permissions(a, fs::Permissions::from_mode(0o755)).map_err(|e| err(a, e))?;
    rename(a, b)
}

pub fn rename(a: &Path, b: &Path) -> Result<(), String> {
    step("rename");
    fs::rename(a, b).map_err(|e| err(b, e))?;
    sync_parent(b)
}

/// fsync of a folder makes its new names durable (ext4). A file system that cannot sync a folder (EINVAL,
/// ENOSYS, EOPNOTSUPP: linux-6.6/fs/sync.c vfs_fsync_range without ->fsync) loses nothing by it; that is logged
/// once per run instead of failing work that is already done.
pub fn sync_dir(p: &Path) -> Result<(), String> {
    match File::open(p).and_then(|f| f.sync_all()) {
        Ok(()) => Ok(()),
        Err(e) if matches!(e.raw_os_error(), Some(22 | 38 | 95)) => {
            static ONCE: std::sync::Once = std::sync::Once::new();
            ONCE.call_once(|| eprintln!("bitspkg: event=fallback.no_dir_sync path={} reason=\"{e}\"", p.display()));
            Ok(())
        }
        Err(e) => Err(err(p, e)),
    }
}

pub fn sync_parent(p: &Path) -> Result<(), String> {
    match p.parent() {
        Some(d) => sync_dir(d),
        None => Ok(()),
    }
}

/// A new folder (its name made durable in the parent); an existing folder is fine.
pub fn mkdir(p: &Path, mode: u32) -> Result<(), String> {
    match fs::create_dir(p) {
        Ok(()) => {
            fs::set_permissions(p, fs::Permissions::from_mode(mode)).map_err(|e| err(p, e))?;
            sync_parent(p)
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && p.is_dir() => Ok(()),
        Err(e) => Err(err(p, e)),
    }
}

/// Deletes a tree whose folders may be read-only (installed versions are 0555), never following a symlink.
pub fn remove_tree(p: &Path) -> Result<(), String> {
    let md = match fs::symlink_metadata(p) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(err(p, e)),
    };
    if md.is_dir() {
        // Only a sealed folder needs opening; chmod is the owner's (or CAP_FOWNER's, linux-6.6/fs/attr.c:199
        // setattr_prepare): a folder the program made is left as it is, and a refusal shows up below.
        if md.mode() & 0o700 != 0o700 {
            let _ = fs::set_permissions(p, fs::Permissions::from_mode(0o700));
        }
        for e in fs::read_dir(p).map_err(|e| gone_err(p, e))? {
            remove_tree(&e.map_err(|e| err(p, e))?.path())?;
        }
        step("remove folder");
        fs::remove_dir(p).map_err(|e| gone_err(p, e))
    } else {
        step("remove file");
        fs::remove_file(p).map_err(|e| gone_err(p, e))
    }
}

/// A removal that failed, said plainly: a mount point or a running program (EBUSY), or not ours (EPERM/EACCES).
fn gone_err(p: &Path, e: std::io::Error) -> String {
    let why = match e.raw_os_error() {
        Some(16) => " (still in use: a mount point or a running program)",
        Some(1 | 13) => " (not bitspkg's to remove: another user's file, or a folder closed to this user)",
        _ => "",
    };
    format!("{}{why}", err(p, e))
}

/// Folders read-only from the leaves up (`chmod -R a-w` for folders; files already have their mode).
pub fn seal_dirs(p: &Path) -> Result<(), String> {
    for e in fs::read_dir(p).map_err(|e| err(p, e))? {
        let e = e.map_err(|e| err(p, e))?;
        if e.file_type().map_err(|x| err(&e.path(), x))?.is_dir() {
            seal_dirs(&e.path())?;
        }
    }
    fs::set_permissions(p, fs::Permissions::from_mode(0o555)).map_err(|e| err(p, e))?;
    sync_dir(p)
}
