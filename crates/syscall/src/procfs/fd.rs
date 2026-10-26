//! `/proc/<pid>/fd` and `/proc/<pid>/fdinfo` (linux-6.6/fs/proc/fd.c `proc_readfd_common`,
//! `proc_lookupfd_common`, `tid_fd_update_inode`, `proc_fd_link`, `seq_show` :23).
//!
//! fd: the open fds in number order, each a magic link whose text is `d_path` of the file (`/memfd:<name>
//! (deleted)`, a removed file's path with " (deleted)") or the object's own name (`pipe:[ino]`,
//! `anon_inode:[eventfd]`), whose permission bits follow the open file's access (read r-x, write -wx, both
//! rwx, for the owner only) and whose `st_size` is 64; following one opens the file again at its place.
//! fdinfo: `pos`, `flags` (octal, O_CLOEXEC from the fd table), `mnt_id`, `ino`, then the object's own lines
//! (`show_fdinfo`): timerfd and signalfd here; eventfd and epoll need FS's hook (change request).

use super::{Target, may_read_mm};
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use proc::Process;
use uapi::errno::*;
use vfs::synth::{DirSource, FileSource, MagicLink, SynthEntry};
use vfs::{Cred, Errno, OpenFile};

/// What an fd shows that has no place of its own and no name (kernel captures of a ktest's output).
const NO_NAME: &str = "anon_inode:[kernel]";
const O_CLOEXEC: u64 = 0o2000000;

pub(super) struct FdDir(pub(super) Target);

fn fd_entry(t: &Target, p: &Process, fd: usize, f: &OpenFile) -> SynthEntry {
    let text = match &f.path {
        Some(path) => super::d_path(path),
        None => f.file.link_name().unwrap_or_else(|| String::from(NO_NAME)).into_bytes(),
    };
    let perm = if f.can_read() { 0o500 } else { 0 } | if f.can_write() { 0o300 } else { 0 };
    super::owned(SynthEntry::magic(&format!("{fd}"), perm, text, Arc::new(FdLink { t: t.clone(), fd })), p, false).cookie(fd as u64 + 1)
}

impl DirSource for FdDir {
    fn entries(&self) -> Result<Vec<SynthEntry>, Errno> {
        let (_, p) = self.0.live().map_err(|_| ENOENT)?;
        Ok(p.fds.open_fds().iter().map(|(n, f)| fd_entry(&self.0, &p, *n, f)).collect())
    }
    fn lookup(&self, name: &[u8]) -> Result<Option<SynthEntry>, Errno> {
        let (_, p) = self.0.live().map_err(|_| ENOENT)?;
        let Some(fd) = vfs::synth::name_to_int(name) else { return Ok(None) };
        Ok(p.fds.get(fd as usize).ok().map(|f| fd_entry(&self.0, &p, fd as usize, &f)))
    }
}

struct FdLink {
    t: Target,
    fd: usize,
}

impl MagicLink for FdLink {
    /// The open file's place, looked up when the walk follows the link (closed meanwhile: ENOENT). Objects
    /// without a place answer ENXIO, except an open(2) of a pipe end (`open` below).
    fn target(&self) -> Result<vfs::Path, Errno> {
        let (_, p) = self.t.live().map_err(|_| ENOENT)?;
        let f = p.fds.get(self.fd).map_err(|_| ENOENT)?;
        f.path.clone().ok_or(ENXIO)
    }
    /// open(2) of a pipe end through the link (AB-7a CR8, for bash `<(cmd)` and `cat /dev/stdin` on a pipe):
    /// Linux jumps to the pipe's pipefs inode (`proc_fd_access_allowed`, then the inode's 0600 owner check) and
    /// `fifo_open` makes a new end with the access asked for. Sockets, eventfd and epoll stay ENXIO.
    fn open(&self, flags: u64) -> Option<Result<Arc<OpenFile>, Errno>> {
        let f = match self.t.live().map_err(|_| ENOENT).and_then(|(_, p)| p.fds.get(self.fd).map(|f| (p, f)).map_err(|_| ENOENT)) {
            Ok((p, f)) if f.path.is_none() => (p, f),
            Ok(_) => return None,
            Err(e) => return Some(Err(e)),
        };
        let (p, f) = f;
        // The opener's fsuid; 0 stands for CAP_DAC_OVERRIDE (`proc::cred::capable`).
        let me_uid = proc::current().map_or(0, |me| me.cred().fsuid);
        match f.file.mode() & uapi::S_IFMT {
            uapi::S_IFIFO => {}
            // S_IFSOCK: sockfs inodes are 0777, the open itself answers ENXIO (`sock_no_open`), as `target` does.
            0o140000 => return None,
            // The shared anon inode (eventfd, epoll, timerfd, signalfd, pidfd) is root's and 0600: others fail
            // its permission check (EACCES) before the open's ENXIO (linux-6.6/fs/anon_inodes.c, fs/libfs.c
            // `alloc_anon_inode`).
            _ if me_uid != 0 => return Some(Err(EACCES)),
            _ => return None,
        }
        if !may_read_mm(&p) || (me_uid != 0 && me_uid != f.stat().uid) {
            return Some(Err(EACCES));
        }
        let r = f.file.reopen(flags, me_uid == 0)?;
        Some(r.map(|end| OpenFile::new(end, vfs::OpenFlags::from_linux(flags))))
    }
}

/// `/proc/<pid>/fdinfo`: r-x for everyone; listing and reading need ptrace READ access (EACCES).
pub(super) struct FdInfoDir(pub(super) Target);

impl FdInfoDir {
    fn entry(&self, p: &Process, fd: usize) -> SynthEntry {
        let src = FdInfo { t: self.0.clone(), fd };
        super::owned(SynthEntry::file(&format!("{fd}"), 0o444, Arc::new(src)), p, false).cookie(fd as u64 + 1)
    }
}

impl DirSource for FdInfoDir {
    fn may_open(&self) -> Result<(), Errno> {
        let (_, p) = self.0.live().map_err(|_| ENOENT)?;
        if !may_read_mm(&p) {
            return Err(EACCES);
        }
        Ok(())
    }
    fn entries(&self) -> Result<Vec<SynthEntry>, Errno> {
        let (_, p) = self.0.live().map_err(|_| ENOENT)?;
        if !may_read_mm(&p) {
            return Err(EACCES);
        }
        Ok(p.fds.open_fds().iter().map(|(n, _)| self.entry(&p, *n)).collect())
    }
    fn lookup(&self, name: &[u8]) -> Result<Option<SynthEntry>, Errno> {
        let (_, p) = self.0.live().map_err(|_| ENOENT)?;
        if !may_read_mm(&p) {
            return Err(EACCES);
        }
        let Some(fd) = vfs::synth::name_to_int(name) else { return Ok(None) };
        Ok(p.fds.is_open(fd as usize).then(|| self.entry(&p, fd as usize)))
    }
}

/// One fdinfo file, made at each read at offset 0 (FS's `SynthFile`, as seq_file). The described file's
/// position is read without its lock (`OpenFile::pos_racy`, as Linux reads `f_pos`): the read holds the
/// reading file's position lock, and `cat /proc/self/fdinfo/3` with fd 3 the fdinfo file would wait for itself.
struct FdInfo {
    t: Target,
    fd: usize,
}

impl FdInfo {
    fn make(&self) -> Result<Vec<u8>, Errno> {
        let (_, p) = self.t.live().map_err(|_| ENOENT)?;
        let f = p.fds.get(self.fd).map_err(|_| ENOENT)?;
        let cloexec = p.fds.cloexec(self.fd).unwrap_or(false);
        let a = f.stat();
        let mut s = fdinfo_head(f.pos_racy(), f.status_flags() | if cloexec { O_CLOEXEC } else { 0 }, a.mnt_id, a.ino);
        // `f_op->show_fdinfo` of the objects this lane owns.
        if let Some(extra) = crate::timerfd::fdinfo(&f).or_else(|| crate::signalfd::fdinfo(&f)).or_else(|| proc::pidfd::fdinfo(&f)) {
            s.push_str(&extra);
        }
        // FS-owned objects (eventfd, epoll) print their own lines (AB-7a CR1).
        f.file.show_fdinfo(&mut s);
        Ok(s.into_bytes())
    }
}

/// `seq_show`'s four lines (`flags` in octal with a leading 0, as `0%o`).
pub(super) fn fdinfo_head(pos: u64, flags: u64, mnt_id: u64, ino: u64) -> String {
    format!("pos:\t{pos}\nflags:\t0{flags:o}\nmnt_id:\t{mnt_id}\nino:\t{ino}\n")
}

impl FileSource for FdInfo {
    fn may_open(&self, _who: Cred, _write: bool) -> Result<(), Errno> {
        let (_, p) = self.t.live().map_err(|_| ENOENT)?;
        if !may_read_mm(&p) {
            return Err(EACCES);
        }
        // The fd must exist at open (ENOENT), as Linux's lookup of fdinfo/<n>.
        p.fds.get(self.fd).map(|_| ()).map_err(|_| ENOENT)
    }
    fn render(&self, _who: Cred) -> Result<Vec<u8>, Errno> {
        self.make()
    }
}
