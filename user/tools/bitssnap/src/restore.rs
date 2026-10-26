//! A file or folder of /data back from a snapshot (VS-3's simplest restore, done in VS-2): the state now is
//! saved first as a snapshot "before-restore" (so the restore itself can be undone), the snapshot is mounted
//! read-only, the file or folder is copied next to its place under a hidden name and then swapped in with one
//! rename (RENAME_EXCHANGE), so a reader sees either the old or the restored one, never half of it; the old one
//! is removed afterwards (it is in the "before-restore" snapshot).
//!
//! Links: this runs as root on paths a uid-1000 program can shape. In the snapshot (read-only, so a check
//! holds) every parent of the path must be a real folder. In /data the parents are opened one by one with
//! O_NOFOLLOW (a symlinked parent is refused, never followed), the working directory is pinned to the parent
//! (fchdir) and every later step names one entry relative to it; the copy is built in a fresh root-only folder
//! (0700) and gets its owners and modes only at the end, deepest first, so nobody can plant a link inside it
//! meanwhile. Linux programs get the same from openat2 RESOLVE_BENEATH|RESOLVE_NO_SYMLINKS
//! (linux-6.6/include/uapi/linux/openat2.h, fs/namei.c).
//!
//! Kept: contents, owners, modes (setuid/setgid too: owner before mode, as chown clears them,
//! linux-6.6/fs/attr.c setattr_should_drop_suidgid), times, symlinks, hard links inside the copied tree, holes
//! of sparse files. Not copied but named in the report: sockets, FIFOs, device nodes (V2 has no mknod yet).
//! Extended attributes: V2 has no xattr calls yet, so no program can have set any.
//!
//! A restore cut off (crash, kill, Reload) leaves its hidden copy; `/data/.bitssnap-restoring` lists the copies
//! in progress with their process, and the next restore (and `serve` at its start) removes those whose process
//! is gone.

use crate::dev::Dev;
use std::collections::HashMap;
use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use usys::Errno;

pub const DATA: &str = "/data";
pub const BEFORE_LABEL: &str = "before-restore";
/// What a volume rollback keeps the state before it as (crates/bitsvol `BEFORE_ROLLBACK`).
pub const BEFORE_ROLLBACK: &str = "before-rollback";
/// The restores in progress, in /data: `pid hex(path below /data)` per line, root only.
pub const PENDING: &str = ".bitssnap-restoring";
const TMP_PREFIX: &str = ".bitssnap-restore-";
const RENAME_NOREPLACE: usize = 1;
const RENAME_EXCHANGE: usize = 2;
const AT_FDCWD: isize = -100;
const AT_SYMLINK_NOFOLLOW: usize = 0x100;
const O_RDWR: usize = 2;
const O_CREAT: usize = 0o100;
const O_DIRECTORY: usize = 0o200000;
const O_NOFOLLOW: usize = 0o400000;
const O_CLOEXEC: usize = 0o2000000;
const ESRCH: i32 = 3;
/// A run of zero bytes this long becomes a hole in the copy of a sparse file.
const HOLE: usize = 4096;

/// The working directory is the copy's anchor and belongs to the whole process: one restore at a time.
static ONE: Mutex<()> = Mutex::new(());

fn one() -> MutexGuard<'static, ()> {
    ONE.lock().unwrap_or_else(|p| p.into_inner())
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Report {
    pub from: u64,
    pub before: u64,
    pub path: String,
    pub files: u64,
    pub dirs: u64,
    pub links: u64,
    /// Extra names of a file already copied (hard links kept as such).
    pub hardlinks: u64,
    /// Sockets, FIFOs and device nodes: not copied (`special` names them, the answer lists them as
    /// not_restored; V2 has no mknod yet, and a socket node is dead after a restore anyway).
    pub skipped: u64,
    pub special: Vec<String>,
    pub bytes: u64,
    /// Whether something was at the place before (and was swapped out).
    pub replaced: bool,
}

/// `p` below /data, normalised: absolute, no `.`/`..`/empty parts, not /data itself. Returns the part after
/// "/data/".
pub fn below_data(p: &str) -> Result<String, String> {
    let rest = p.strip_prefix("/data/").ok_or_else(|| format!("{p}: not a path below /data"))?;
    let parts: Vec<&str> = rest.split('/').filter(|s| !s.is_empty()).collect();
    if parts.is_empty() {
        return Err("the whole /data volume is a rollback (VS-3), not a file restore".into());
    }
    if parts.iter().any(|s| *s == "." || *s == "..") {
        return Err(format!("{p}: . and .. are not allowed"));
    }
    Ok(parts.join("/"))
}

fn err(p: &Path, e: impl std::fmt::Display) -> String {
    format!("{}: {e}", p.display())
}

fn sys(what: &str, e: Errno) -> String {
    format!("{what}: errno {}", e.0)
}

/// An fd closed when dropped.
struct Fd(usize);

impl Drop for Fd {
    fn drop(&mut self) {
        let _ = usys::close(self.0);
    }
}

/// A folder below `dirfd`, never through a symlink.
fn open_dir(dirfd: isize, name: &str) -> Result<Fd, Errno> {
    usys::openat(dirfd, name, O_DIRECTORY | O_NOFOLLOW | O_CLOEXEC, 0).map(Fd)
}

fn times(m: &fs::Metadata) -> fs::FileTimes {
    let at = |s: i64, ns: i64| if s >= 0 { UNIX_EPOCH + Duration::new(s as u64, ns as u32) } else { UNIX_EPOCH };
    fs::FileTimes::new().set_accessed(at(m.atime(), m.atime_nsec())).set_modified(at(m.mtime(), m.mtime_nsec()))
}

fn copy_file(src: &Path, dst: &Path, m: &fs::Metadata, r: &mut Report) -> Result<(), String> {
    let mut i = fs::File::open(src).map_err(|e| err(src, e))?;
    // create_new: O_EXCL, so an existing name (a link included) is never written through.
    let mut o = fs::OpenOptions::new().write(true).create_new(true).open(dst).map_err(|e| err(dst, e))?;
    // As cp --sparse=auto: only a file with holes gets holes again.
    let sparse = (m.blocks() as u64).saturating_mul(512) < m.size();
    let mut buf = vec![0u8; 1 << 20];
    let mut len = 0u64;
    loop {
        let n = i.read(&mut buf).map_err(|e| err(src, e))?;
        if n == 0 {
            break;
        }
        if sparse {
            for c in buf[..n].chunks(HOLE) {
                if c.iter().all(|&b| b == 0) {
                    o.seek(SeekFrom::Current(c.len() as i64)).map_err(|e| err(dst, e))?;
                } else {
                    o.write_all(c).map_err(|e| err(dst, e))?;
                }
            }
        } else {
            o.write_all(&buf[..n]).map_err(|e| err(dst, e))?;
        }
        len += n as u64;
    }
    if sparse {
        o.set_len(len).map_err(|e| err(dst, e))?;
    }
    r.bytes += len;
    // Owner before mode: a chown clears setuid/setgid.
    std::os::unix::fs::fchown(&o, Some(m.uid()), Some(m.gid())).map_err(|e| err(dst, e))?;
    o.set_permissions(fs::Permissions::from_mode(m.mode() & 0o7777)).map_err(|e| err(dst, e))?;
    o.set_times(times(m)).map_err(|e| err(dst, e))?;
    o.sync_all().map_err(|e| err(dst, e))?;
    r.files += 1;
    Ok(())
}

/// One copy: the folders get owner, mode and times only in `finish` (deepest first), so every folder of the
/// copy stays root-only and writable while it is filled.
struct Copy<'a> {
    r: &'a mut Report,
    /// Source (dev, inode) of a file with more names -> its first copy and that copy's inode.
    seen: HashMap<(u64, u64), (PathBuf, u64)>,
    dirs: Vec<(PathBuf, fs::Metadata)>,
}

impl<'a> Copy<'a> {
    fn new(r: &'a mut Report) -> Copy<'a> {
        Copy { r, seen: HashMap::new(), dirs: Vec::new() }
    }

    fn entry(&mut self, src: &Path, dst: &Path) -> Result<(), String> {
        let m = fs::symlink_metadata(src).map_err(|e| err(src, e))?;
        let ft = m.file_type();
        if ft.is_symlink() {
            let t = fs::read_link(src).map_err(|e| err(src, e))?;
            std::os::unix::fs::symlink(&t, dst).map_err(|e| err(dst, e))?;
            std::os::unix::fs::lchown(dst, Some(m.uid()), Some(m.gid())).map_err(|e| err(dst, e))?;
            // The link's own times, as cp -a does (lutimes: utimensat with AT_SYMLINK_NOFOLLOW).
            let p = dst.to_str().ok_or_else(|| format!("{}: not a UTF-8 name", dst.display()))?;
            let ts = [(m.atime(), m.atime_nsec()), (m.mtime(), m.mtime_nsec())];
            usys::utimensat(AT_FDCWD, Some(p), Some(ts), AT_SYMLINK_NOFOLLOW).map_err(|e| sys(p, e))?;
            self.r.links += 1;
        } else if ft.is_file() {
            let key = (m.dev(), m.ino());
            if m.nlink() > 1
                && let Some((first, ino)) = self.seen.get(&key)
            {
                fs::hard_link(first, dst).map_err(|e| err(dst, e))?;
                if fs::symlink_metadata(dst).map(|d| d.ino()).ok() == Some(*ino) {
                    self.r.hardlinks += 1;
                    return Ok(());
                }
                // Not the copy it should be: a file of its own instead.
                fs::remove_file(dst).map_err(|e| err(dst, e))?;
            }
            copy_file(src, dst, &m, self.r)?;
            if m.nlink() > 1 {
                let ino = fs::symlink_metadata(dst).map_err(|e| err(dst, e))?.ino();
                self.seen.insert(key, (dst.to_path_buf(), ino));
            }
        } else if ft.is_dir() {
            fs::create_dir(dst).map_err(|e| err(dst, e))?;
            self.dirs.push((dst.to_path_buf(), m));
            self.children(src, dst)?;
        } else {
            self.r.skipped += 1;
            self.r.special.push(src.display().to_string());
        }
        Ok(())
    }

    fn children(&mut self, src: &Path, dst: &Path) -> Result<(), String> {
        let mut names: Vec<_> = fs::read_dir(src).map_err(|e| err(src, e))?.map(|e| e.map(|e| e.file_name())).collect::<Result<_, _>>().map_err(|e| err(src, e))?;
        names.sort();
        for n in names {
            self.entry(&src.join(&n), &dst.join(&n))?;
        }
        Ok(())
    }

    /// Owners, modes and times of the folders, deepest first (each one's parents are still root-only then).
    fn finish(&mut self) -> Result<(), String> {
        while let Some((d, m)) = self.dirs.pop() {
            std::os::unix::fs::lchown(&d, Some(m.uid()), Some(m.gid())).map_err(|e| err(&d, e))?;
            fs::set_permissions(&d, fs::Permissions::from_mode(m.mode() & 0o7777)).map_err(|e| err(&d, e))?;
            let f = fs::File::open(&d).map_err(|e| err(&d, e))?;
            f.set_times(times(&m)).map_err(|e| err(&d, e))?;
            let _ = f.sync_all();
            self.r.dirs += 1;
        }
        Ok(())
    }
}

/// Copies `src` (file, folder or symlink) to the new name `dst` (tests and the rescue side; the restore itself
/// goes through `in_place`).
pub fn copy_tree(src: &Path, dst: &Path, r: &mut Report) -> Result<(), String> {
    let mut c = Copy::new(r);
    let res = c.entry(src, dst);
    res.and_then(|_| c.finish())
}

fn remove_any(p: &Path) -> std::io::Result<()> {
    match fs::symlink_metadata(p) {
        // remove_dir_all never follows a link (it opens each level with O_NOFOLLOW).
        Ok(m) if m.is_dir() => fs::remove_dir_all(p),
        Ok(_) => fs::remove_file(p),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// `rel` in the snapshot: every parent a real folder, the last part present (a symlink there is copied as one).
pub fn in_snapshot(root: &Path, rel: &str) -> Result<PathBuf, String> {
    let parts: Vec<&str> = rel.split('/').collect();
    let mut cur = root.to_path_buf();
    for p in &parts[..parts.len() - 1] {
        cur.push(p);
        match fs::symlink_metadata(&cur) {
            Ok(m) if m.file_type().is_dir() => {}
            Ok(_) => return Err(format!("{p} is a symlink or a file there, not a folder (never followed)")),
            Err(e) => return Err(format!("not there ({e})")),
        }
    }
    cur.push(parts[parts.len() - 1]);
    fs::symlink_metadata(&cur).map_err(|e| format!("not there ({e})"))?;
    Ok(cur)
}

/// The live parent folder of `rel` below `base`, opened step by step without following links; missing parents
/// are made (with `create`) with the snapshot's owner and mode, else root 0755.
fn live_parent(base: &str, snap_root: &Path, parents: &[&str], create: bool) -> Result<Fd, String> {
    let mut fd = open_dir(AT_FDCWD, base).map_err(|e| sys(base, e))?;
    let (mut shown, mut in_snap) = (PathBuf::from(base), snap_root.to_path_buf());
    for p in parents {
        shown.push(p);
        in_snap.push(p);
        let refuse = |e: Errno| format!("{}: not a real folder (errno {}); a symlinked parent is never followed", shown.display(), e.0);
        let next = match open_dir(fd.0 as isize, p) {
            Ok(f) => f,
            Err(e) if e == usys::ENOENT && create => {
                match usys::mkdirat(fd.0 as isize, p, 0o700) {
                    Ok(_) => {}
                    Err(e) if e == usys::EEXIST => {}
                    Err(e) => return Err(sys(&shown.display().to_string(), e)),
                }
                let f = open_dir(fd.0 as isize, p).map_err(refuse)?;
                match fs::symlink_metadata(&in_snap) {
                    Ok(m) => {
                        let _ = usys::fchown(f.0, m.uid(), m.gid());
                        let _ = usys::fchmod(f.0, (m.mode() & 0o7777) as usize);
                    }
                    Err(_) => {
                        let _ = usys::fchmod(f.0, 0o755);
                    }
                }
                f
            }
            Err(e) => return Err(refuse(e)),
        };
        fd = next;
    }
    Ok(fd)
}

fn fnv(s: &str) -> u64 {
    s.bytes().fold(0xcbf2_9ce4_8422_2325u64, |h, b| (h ^ b as u64).wrapping_mul(0x0100_0000_01b3))
}

/// The hidden copy's name: the same for the same target (a leftover is found again), short whatever the
/// target's length (NAME_MAX 255).
pub fn tmp_name(name: &str) -> String {
    format!("{TMP_PREFIX}{:016x}", fnv(name))
}

fn hex(s: &str) -> String {
    s.bytes().map(|b| format!("{b:02x}")).collect()
}

fn unhex(s: &str) -> Option<String> {
    let b = s.as_bytes();
    if b.len() % 2 != 0 {
        return None;
    }
    let v: Option<Vec<u8>> = b.chunks(2).map(|c| u8::from_str_radix(std::str::from_utf8(c).ok()?, 16).ok()).collect();
    String::from_utf8(v?).ok()
}

/// The pending list (opened without following a link).
struct Pending(Fd);

impl Pending {
    fn open(data: &Fd) -> Result<Pending, String> {
        usys::openat(data.0 as isize, PENDING, O_RDWR | O_CREAT | O_NOFOLLOW | O_CLOEXEC, 0o600).map(|f| Pending(Fd(f))).map_err(|e| sys(PENDING, e))
    }

    fn load(&self) -> Vec<(u32, String)> {
        let mut all = Vec::new();
        let mut buf = vec![0u8; 4096];
        while let Ok(n) = usys::pread64(self.0.0, &mut buf, all.len() as i64) {
            if n == 0 {
                break;
            }
            all.extend_from_slice(&buf[..n]);
        }
        String::from_utf8_lossy(&all)
            .lines()
            .filter_map(|l| {
                let (pid, p) = l.split_once(' ')?;
                Some((pid.parse().ok()?, unhex(p)?))
            })
            .collect()
    }

    fn store(&self, v: &[(u32, String)]) -> Result<(), String> {
        let text: String = v.iter().map(|(pid, p)| format!("{pid} {}\n", hex(p))).collect();
        usys::ftruncate(self.0.0, 0).map_err(|e| sys(PENDING, e))?;
        let mut off = 0;
        while off < text.len() {
            off += usys::pwrite64(self.0.0, &text.as_bytes()[off..], off as i64).map_err(|e| sys(PENDING, e))?;
        }
        usys::fsync(self.0.0).map_err(|e| sys(PENDING, e))?;
        Ok(())
    }
}

/// Another process that still runs (this process's own entries are left over: one restore at a time here).
fn alive(pid: u32) -> bool {
    pid != std::process::id() && usys::kill(pid as isize, 0).map_or_else(|e| e.0 != ESRCH, |_| true)
}

/// Removes the hidden copies of restores whose process is gone (call with `one()` held).
fn sweep_locked(base: &str) -> Result<Vec<String>, String> {
    let data = open_dir(AT_FDCWD, base).map_err(|e| sys(base, e))?;
    let pend = Pending::open(&data)?;
    let all = pend.load();
    if all.is_empty() {
        return Ok(Vec::new());
    }
    let (mut keep, mut gone) = (Vec::new(), Vec::new());
    for (pid, rel) in all {
        if alive(pid) {
            keep.push((pid, rel));
            continue;
        }
        let parts: Vec<&str> = rel.split('/').collect();
        let name = parts[parts.len() - 1];
        if !name.starts_with(TMP_PREFIX) {
            continue;
        }
        let parents = &parts[..parts.len() - 1];
        // The parent went (or turned into a link): nothing of ours is left there.
        let Ok(p) = live_parent(base, Path::new("/"), parents, false) else { continue };
        let removed = usys::fchdir(p.0).map_err(|e| sys("fchdir", e)).and_then(|_| remove_any(Path::new(name)).map_err(|e| format!("{rel}: {e}")));
        let _ = usys::chdir("/");
        match removed {
            Ok(()) => gone.push(format!("{base}/{rel}")),
            Err(e) => {
                crate::log(&format!("a left-over restore copy stays: {e}"));
                keep.push((pid, rel));
            }
        }
    }
    pend.store(&keep)?;
    Ok(gone)
}

/// Removes what restores cut off (crash, kill, Reload) left behind in /data; `serve` calls it at its start.
pub fn sweep() -> Result<Vec<String>, String> {
    let _one = one();
    sweep_locked(DATA)
}

/// The copy of `src` swapped in at `rel` below `base` (the restore without its snapshot steps).
fn in_place(base: &str, src: &Path, rel: &str, snap_root: &Path, rep: &mut Report) -> Result<(), String> {
    if let Ok(g) = sweep_locked(base)
        && !g.is_empty()
    {
        crate::log(&format!("removed left-over restore copies: {}", g.join(", ")));
    }
    let parts: Vec<&str> = rel.split('/').collect();
    let (parents, name) = (&parts[..parts.len() - 1], parts[parts.len() - 1]);
    let pfd = live_parent(base, snap_root, parents, true)?;
    let tmp = tmp_name(name);
    let tmp_rel = parents.iter().copied().chain([tmp.as_str()]).collect::<Vec<_>>().join("/");
    let data = open_dir(AT_FDCWD, base).map_err(|e| sys(base, e))?;
    let pend = Pending::open(&data)?;
    let mut list = pend.load();
    if list.iter().any(|(pid, p)| *p == tmp_rel && alive(*pid)) {
        return Err(format!("{base}/{rel}: another restore of it is running"));
    }
    list.retain(|(_, p)| *p != tmp_rel);
    list.push((std::process::id(), tmp_rel.clone()));
    pend.store(&list)?;
    let old_mask = usys::umask(0o077);
    let r = usys::fchdir(pfd.0).map_err(|e| sys("fchdir", e)).and_then(|_| copy_and_swap(src, name, &tmp, &pfd, rep));
    if let Ok(m) = old_mask {
        let _ = usys::umask(m);
    }
    let _ = usys::chdir("/");
    let mut list = pend.load();
    list.retain(|(pid, p)| !(*p == tmp_rel && *pid == std::process::id()));
    let _ = pend.store(&list);
    r
}

/// In the pinned parent (the working directory): the copy under `tmp`, then the swap.
fn copy_and_swap(src: &Path, name: &str, tmp: &str, pfd: &Fd, rep: &mut Report) -> Result<(), String> {
    let tp = Path::new(tmp);
    remove_any(tp).map_err(|e| err(tp, e))?;
    if let Err(e) = copy_top(src, tmp, pfd, rep) {
        let _ = usys::fchdir(pfd.0);
        let _ = remove_any(tp);
        return Err(e);
    }
    let mut done = None;
    // A name appearing or going between the look and the rename: look again.
    for _ in 0..3 {
        let replaced = usys::newfstatat(AT_FDCWD, name, AT_SYMLINK_NOFOLLOW).is_ok();
        let flags = if replaced { RENAME_EXCHANGE } else { RENAME_NOREPLACE };
        match usys::renameat2(AT_FDCWD, tmp, AT_FDCWD, name, flags) {
            Ok(_) => {
                done = Some(replaced);
                break;
            }
            Err(e) if (replaced && e == usys::ENOENT) || (!replaced && e == usys::EEXIST) => continue,
            Err(e) => {
                let _ = remove_any(tp);
                return Err(format!("{name}: the swap failed (errno {}); nothing changed", e.0));
            }
        }
    }
    let Some(replaced) = done else {
        let _ = remove_any(tp);
        return Err(format!("{name}: it kept appearing and going; nothing changed"));
    };
    rep.replaced = replaced;
    // The old one (now under the hidden name) is in the before-restore snapshot.
    if replaced {
        remove_any(tp).map_err(|e| err(tp, e))?;
    }
    let _ = usys::fsync(pfd.0);
    Ok(())
}

fn copy_top(src: &Path, tmp: &str, pfd: &Fd, rep: &mut Report) -> Result<(), String> {
    let m = fs::symlink_metadata(src).map_err(|e| err(src, e))?;
    if !m.file_type().is_dir() {
        return copy_tree(src, Path::new(tmp), rep);
    }
    // The top folder is a name in a folder others may write: work inside it through its fd only.
    fs::create_dir(tmp).map_err(|e| err(Path::new(tmp), e))?;
    let t = open_dir(AT_FDCWD, tmp).map_err(|e| sys(tmp, e))?;
    let st = usys::fstat(t.0).map_err(|e| sys(tmp, e))?;
    let me = usys::geteuid().map_err(|e| sys("geteuid", e))? as u32;
    if st.mode & 0o170000 != 0o040000 || st.uid != me || st.mode & 0o777 != 0o700 {
        return Err(format!("{tmp}: swapped for something else while being made; nothing changed"));
    }
    usys::fchdir(t.0).map_err(|e| sys("fchdir", e))?;
    let mut c = Copy::new(rep);
    let r = c.children(src, Path::new("")).and_then(|_| c.finish());
    usys::fchdir(pfd.0).map_err(|e| sys("fchdir", e))?;
    r?;
    usys::fchown(t.0, m.uid(), m.gid()).map_err(|e| sys(tmp, e))?;
    usys::fchmod(t.0, (m.mode() & 0o7777) as usize).map_err(|e| sys(tmp, e))?;
    let ts = [(m.atime(), m.atime_nsec()), (m.mtime(), m.mtime_nsec())];
    usys::utimensat(t.0 as isize, None, Some(ts), 0).map_err(|e| sys(tmp, e))?;
    let _ = usys::fsync(t.0);
    rep.dirs += 1;
    Ok(())
}

/// Mounts snapshot `id` of /data read-only and runs `f` with its root, one restore at a time; unmounts it on
/// every way out.
pub(crate) fn with_snapshot<T>(dev: &mut Dev, id: u64, f: impl FnOnce(&mut Dev, &Path) -> Result<T, String>) -> Result<T, String> {
    let snaps = dev.list(false).map_err(|e| e.to_string())?;
    if !snaps.iter().any(|s| s.id == id) {
        return Err(format!("no snapshot {id} of /data (bitssnap list)"));
    }
    let _one = one();
    let at = dev.mount(id).map_err(|e| e.to_string())?;
    let r = f(dev, Path::new(&at));
    let un = dev.umount(id);
    let r = r?;
    un.map_err(|e| format!("restored, but the snapshot stays mounted: {e}"))?;
    Ok(r)
}

/// Restores `path` (below /data) from snapshot `id`.
pub fn restore(dev: &mut Dev, id: u64, path: &str) -> Result<Report, String> {
    let rel = below_data(path)?;
    with_snapshot(dev, id, |dev, snap_root| {
        let src = in_snapshot(snap_root, &rel).map_err(|e| format!("{path}: not in snapshot {id} ({e})"))?;
        // Only now that there is something to restore: the state before, for the undo.
        let before = dev.take(BEFORE_LABEL, false, false).map_err(|e| format!("saving the state before: {e}"))?;
        let mut rep = Report { from: id, before: before.id, path: format!("{DATA}/{rel}"), ..Report::default() };
        in_place(DATA, &src, &rel, snap_root, &mut rep)?;
        Ok(rep)
    })
}

/// Seconds since 1970 now (0 before it).
pub fn now_s() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

pub mod program;
pub mod volume;

#[cfg(test)]
mod tests;
