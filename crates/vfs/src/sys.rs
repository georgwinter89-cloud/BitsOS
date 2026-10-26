//! The file syscalls (FS-1), written against an abstract user memory so the host tests run them unchanged.
//! The `vfs-sys` crate registers each with `syscall!` and maps `SysCtx` onto `Sys`. Every function takes the
//! six raw arguments and returns the value or -errno, with Linux's argument checks and error order.

use crate::abi::{self, AT_EMPTY_PATH, AT_FDCWD, AT_SYMLINK_NOFOLLOW, MAX_RW_COUNT, PATH_MAX};
use crate::fd::{FdTable, OpenFile};
use crate::inode::Cred;
use crate::mount_ns::{MountNs, Path};
use crate::ops::{self, Ctx};
use crate::Errno;
use alloc::sync::Arc;
use alloc::vec::Vec;
use uapi::errno::*;

/// The calling program's memory.
pub trait UserMem {
    /// Copies from user memory; false on a fault.
    fn read(&self, addr: u64, buf: &mut [u8]) -> bool;
    /// Copies to user memory; false on a fault.
    fn write(&self, addr: u64, data: &[u8]) -> bool;
    /// A NUL-terminated string of fewer than `max` bytes: EFAULT, or ENAMETOOLONG without a NUL in `max`.
    fn read_cstr(&self, addr: u64, max: usize) -> Result<Vec<u8>, Errno>;
    /// FS-4: whether the calling program is being ended (a kill; Linux `fatal_signal_pending`): poll/select/epoll
    /// waits end with EINTR, an epoll harvest too. The caller's process, also when a kernel thread makes the call
    /// for it (ktests).
    fn interrupted(&self) -> bool {
        crate::dentry::interrupted()
    }
    /// The open file's status flags (Linux `file->f_flags`) for a device ioctl, so O_NONBLOCK set with F_SETFL or
    /// FIONBIO reaches it (SN-4 CR 1); None where the caller has no open file (host tests, plain syscalls).
    fn file_status(&self) -> Option<u64> {
        None
    }
    /// FS-4: whether a signal waits for its handler (Linux `signal_pending`): a wait that found no event ends
    /// with EINTR (events first). False until AB-6 delivers signals to handlers (an EINTR for a signal that is
    /// never delivered would make the program spin).
    fn signal_pending(&self) -> bool {
        false
    }
    /// FS-4: sets the calling thread's temporary signal mask (`set_user_sigmask`; SIGKILL/SIGSTOP stay
    /// deliverable) and acts on a pending signal it unblocks whose default ends the program. Returns the mask
    /// before and the mask now in place, None without signal state (host tests).
    fn set_sigmask(&self, _mask: u64) -> Option<(u64, u64)> {
        None
    }
    /// FS-4: `restore_saved_sigmask_unless(interrupted)`: puts `old` back, but only while `temp` (what
    /// `set_sigmask` put in place) is still the mask. `interrupted`: the call ends with EINTR for a signal; Linux
    /// then keeps `temp` until the handler ran (AB-6's saved mask); without that hand-over the mask goes back now.
    fn restore_sigmask(&self, _old: u64, _temp: u64, _interrupted: bool) {}
}

/// Everything a file syscall needs from its caller.
pub struct Sys<'a> {
    pub mem: &'a dyn UserMem,
    pub fds: &'a FdTable,
    pub ns: &'a Arc<MountNs>,
    pub cred: Cred,
    pub subject: policy::Subject,
}

pub type Args = [u64; 6];
pub type SysFn = fn(&Sys<'_>, Args) -> i64;

/// Copies go through a bounce buffer of this size (the kernel never allocates what a program asks for).
pub(crate) const CHUNK: usize = 64 * 1024;

/// `access_ok` of x86-64 Linux 6.6 (arch/x86/include/asm/uaccess_64.h): the range must not wrap and must end
/// in the lower half. Anything else in it faults in the copy itself.
pub(crate) fn access_ok(buf: u64, count: u64) -> bool {
    buf.checked_add(count).is_some_and(|end| (end as i64) >= 0)
}

/// A zeroed kernel buffer that answers ENOMEM instead of aborting when the heap is exhausted (a full tmpfs can
/// bring the heap there).
pub(crate) fn zeroed(n: usize) -> Result<Vec<u8>, Errno> {
    let mut v = Vec::new();
    v.try_reserve_exact(n).map_err(|_| ENOMEM)?;
    v.resize(n, 0);
    Ok(v)
}

pub(crate) fn ret(r: Result<i64, Errno>) -> i64 {
    r.unwrap_or_else(|e| -e)
}

/// A program's buffers as Linux's `iov_iter` (FS-3: pipes and eventfd take a whole request at once). The
/// segments passed `access_ok` before. A copy that meets an unmapped page stops at that page, as Linux's
/// `copy_to_iter`/`copy_from_iter` do (whole pages before it are copied).
struct UserIov<'a> {
    mem: &'a dyn UserMem,
    segs: Vec<(u64, u64)>,
    seg: usize,
    off: u64,
    left: usize,
}

const PAGE_SIZE: u64 = 4096;

impl<'a> UserIov<'a> {
    fn new(mem: &'a dyn UserMem, segs: Vec<(u64, u64)>) -> Self {
        let left = segs.iter().map(|s| s.1 as usize).sum();
        UserIov { mem, segs, seg: 0, off: 0, left }
    }
    fn single(mem: &'a dyn UserMem, buf: u64, count: u64) -> Self {
        Self::new(mem, alloc::vec![(buf, count.min(MAX_RW_COUNT))])
    }
    /// Past a used-up segment (and empty ones after it) while bytes are left, as `iov_iter_advance`.
    fn settle(&mut self) {
        while self.left > 0 && self.segs.get(self.seg).is_some_and(|s| self.off >= s.1) {
            self.seg += 1;
            self.off = 0;
        }
    }
    /// Moves up to `len` bytes between the program and `dst` (from the program) or `src` (to it) at the
    /// current place; returns how many moved.
    fn transfer(&mut self, mut dst: Option<&mut [u8]>, src: Option<&[u8]>, len: usize) -> usize {
        let mut done = 0;
        while done < len && self.left > 0 {
            let Some(&(base, slen)) = self.segs.get(self.seg) else { break };
            if self.off >= slen {
                self.seg += 1;
                self.off = 0;
                continue;
            }
            let k = ((slen - self.off) as usize).min(len - done);
            let at = base + self.off;
            let moved = match (&mut dst, src) {
                (Some(d), _) => copy_in_pages(self.mem, at, &mut d[done..done + k]),
                (None, Some(s)) => copy_out_pages(self.mem, at, &s[done..done + k]),
                _ => 0,
            };
            self.off += moved as u64;
            self.left -= moved;
            done += moved;
            if moved < k {
                break;
            }
        }
        self.settle();
        done
    }
}

/// Copies from the program; at a fault, the pages before it.
fn copy_in_pages(mem: &dyn UserMem, at: u64, dst: &mut [u8]) -> usize {
    if mem.read(at, dst) {
        return dst.len();
    }
    let mut done = 0;
    while done < dst.len() {
        let a = at + done as u64;
        let k = ((PAGE_SIZE - a % PAGE_SIZE) as usize).min(dst.len() - done);
        if !mem.read(a, &mut dst[done..done + k]) {
            break;
        }
        done += k;
    }
    done
}

/// Copies to the program; at a fault, the pages before it.
fn copy_out_pages(mem: &dyn UserMem, at: u64, src: &[u8]) -> usize {
    if mem.write(at, src) {
        return src.len();
    }
    let mut done = 0;
    while done < src.len() {
        let a = at + done as u64;
        let k = ((PAGE_SIZE - a % PAGE_SIZE) as usize).min(src.len() - done);
        if !mem.write(a, &src[done..done + k]) {
            break;
        }
        done += k;
    }
    done
}

impl crate::node::IoSink for UserIov<'_> {
    fn left(&self) -> usize {
        self.left
    }
    fn copy(&mut self, src: &[u8]) -> usize {
        self.transfer(None, Some(src), src.len())
    }
}

impl crate::node::IoSource for UserIov<'_> {
    fn left(&self) -> usize {
        self.left
    }
    fn seg_left(&self) -> usize {
        if self.left == 0 {
            return 0;
        }
        self.segs.get(self.seg).map_or(0, |s| (s.1 - self.off) as usize)
    }
    fn copy(&mut self, dst: &mut [u8]) -> usize {
        let n = dst.len();
        self.transfer(Some(dst), None, n)
    }
}

fn fd_of(a: u64) -> Result<usize, Errno> {
    // fds are C ints: the upper half of the register is ignored, negative ones are EBADF.
    let fd = a as u32 as i32;
    if fd < 0 { Err(EBADF) } else { Ok(fd as usize) }
}

impl<'a> Sys<'a> {
    pub fn ctx(&self) -> Ctx<'a> {
        let root = self.ns.root_path();
        let cwd = self.fds.fs.cwd().unwrap_or_else(|| root.clone());
        Ctx { ns: self.ns, root, cwd, cred: self.cred, umask: self.fds.fs.umask(), subject: self.subject }
    }
    /// A path argument (`getname`): EFAULT, ENAMETOOLONG at PATH_MAX, ENOENT when empty unless allowed.
    pub(crate) fn path(&self, ptr: u64, allow_empty: bool) -> Result<Vec<u8>, Errno> {
        let p = self.mem.read_cstr(ptr, PATH_MAX)?;
        if p.is_empty() && !allow_empty {
            return Err(ENOENT);
        }
        Ok(p)
    }
    /// An fd for I/O (`fdget`): O_PATH files count as bad.
    pub(crate) fn file(&self, fd: u64) -> Result<Arc<OpenFile>, Errno> {
        let f = self.fds.get(fd_of(fd)?)?;
        if f.is_path_only() { Err(EBADF) } else { Ok(f) }
    }
    /// Any fd (`fdget_raw`).
    pub(crate) fn raw(&self, fd: u64) -> Result<Arc<OpenFile>, Errno> {
        self.fds.get(fd_of(fd)?)
    }
    /// The base for a relative path of an *at call (`path_init`): None = cwd; an fd must be a directory.
    pub(crate) fn base(&self, dirfd: u64, path: &[u8]) -> Result<Option<Path>, Errno> {
        if path.first() == Some(&b'/') || dirfd as u32 as i32 == AT_FDCWD {
            return Ok(None);
        }
        let f = self.raw(dirfd)?;
        match &f.path {
            Some(p) if p.dentry.is_dir() => Ok(Some(p.clone())),
            _ => Err(ENOTDIR),
        }
    }
    /// The object an empty path with AT_EMPTY_PATH names: the fd itself (or the cwd for AT_FDCWD).
    pub(crate) fn empty_target(&self, dirfd: u64) -> Result<Path, Errno> {
        if dirfd as u32 as i32 == AT_FDCWD {
            return Ok(self.ctx().cwd);
        }
        self.raw(dirfd)?.path.clone().ok_or(ENOENT)
    }
    pub(crate) fn copy_out(&self, addr: u64, data: &[u8]) -> Result<(), Errno> {
        if self.mem.write(addr, data) { Ok(()) } else { Err(EFAULT) }
    }
}

// ---- reading and writing --------------------------------------------------------------------------------

/// Reads up to `count` into user memory at `buf` with `rd(pos_or_0, chunk)`; stops at a short read. A fault
/// after some bytes returns the bytes so far (Linux's partial-copy rule).
/// The caller checked `access_ok(buf, count)`.
pub(crate) fn read_into(s: &Sys<'_>, buf: u64, count: u64, mut rd: impl FnMut(usize, &mut [u8]) -> Result<usize, Errno>) -> Result<i64, Errno> {
    let count = count.min(MAX_RW_COUNT) as usize;
    if count == 0 {
        return rd(0, &mut []).map(|_| 0);
    }
    let mut chunk = zeroed(count.min(CHUNK))?;
    let mut done = 0usize;
    while done < count {
        let k = (count - done).min(CHUNK);
        let n = match rd(done, &mut chunk[..k]) {
            Ok(n) => n,
            Err(e) if done == 0 => return Err(e),
            Err(_) => break,
        };
        if n == 0 {
            break;
        }
        let ok = buf.checked_add(done as u64).is_some_and(|at| s.mem.write(at, &chunk[..n]));
        if !ok {
            return if done == 0 { Err(EFAULT) } else { Ok(done as i64) };
        }
        done += n;
        if n < k {
            break;
        }
    }
    Ok(done as i64)
}

/// The caller checked `access_ok(buf, count)`.
pub(crate) fn write_from(s: &Sys<'_>, buf: u64, count: u64, mut wr: impl FnMut(usize, &[u8]) -> Result<usize, Errno>) -> Result<i64, Errno> {
    let count = count.min(MAX_RW_COUNT) as usize;
    if count == 0 {
        return wr(0, &[]).map(|_| 0);
    }
    let mut chunk = zeroed(count.min(CHUNK))?;
    let mut done = 0usize;
    while done < count {
        let k = (count - done).min(CHUNK);
        let ok = buf.checked_add(done as u64).is_some_and(|at| s.mem.read(at, &mut chunk[..k]));
        if !ok {
            return if done == 0 { Err(EFAULT) } else { Ok(done as i64) };
        }
        let n = match wr(done, &chunk[..k]) {
            Ok(n) => n,
            Err(e) if done == 0 => return Err(e),
            Err(_) => break,
        };
        done += n;
        if n < k {
            break;
        }
    }
    Ok(done as i64)
}

/// read on an open file: seekable files keep their position locked for the whole call and advance it only
/// by what reached the program.
pub(crate) fn file_read(s: &Sys<'_>, f: &OpenFile, buf: u64, count: u64) -> Result<i64, Errno> {
    if !f.can_read() {
        return Err(EBADF);
    }
    if !access_ok(buf, count) {
        return Err(EFAULT);
    }
    if !f.file.seekable() {
        // FS-3: pipes and eventfd take the whole request in one call (Linux `read_iter`).
        let mut iov = UserIov::single(s.mem, buf, count);
        if let Some(r) = f.file.read_iter(&mut iov, f.status_flags()) {
            return r.map(|n| n as i64);
        }
        let n = count.min(CHUNK as u64) as usize;
        let mut b = zeroed(n)?;
        let got = f.read(&mut b)?;
        s.copy_out(buf, &b[..got])?;
        return Ok(got as i64);
    }
    f.with_pos(|pos| {
        let start = *pos;
        let r = read_into(s, buf, count, |off, chunk| f.file.read_at(start + off as u64, chunk));
        if let Ok(n) = r {
            *pos = start + n as u64;
        }
        r
    })
}

pub(crate) fn file_write(s: &Sys<'_>, f: &OpenFile, buf: u64, count: u64) -> Result<i64, Errno> {
    if !f.can_write() {
        return Err(EBADF);
    }
    if !access_ok(buf, count) {
        return Err(EFAULT);
    }
    if !f.file.seekable() {
        let mut iov = UserIov::single(s.mem, buf, count);
        if let Some(r) = f.file.write_iter(&mut iov, f.status_flags()) {
            return r.map(|n| n as i64);
        }
        let nonblock = f.nonblocking();
        return write_from(s, buf, count, |_, chunk| f.file.write_stream(chunk, nonblock));
    }
    // B1: wait out a freeze before taking the position lock, so other users of this fd are not held behind a
    // frozen writer (the write itself takes the guard; a freeze that starts in between is waited for there).
    if let Some(p) = &f.path {
        p.dentry.sb.wait_thawed()?;
    }
    f.with_pos(|pos| {
        // FS-10: a sealed tmpfs file decides on the whole request (Linux writes it in one pass).
        let count = f.seal_clip(*pos, count)?;
        write_from(s, buf, count, |_, chunk| f.write_locked(pos, chunk))
    })
}

/// FS-14: `fsnotify_access` / `fsnotify_modify` after a transfer of at least one byte (`vfs_read`, `vfs_write`).
pub(crate) fn notify_rw(f: &OpenFile, r: Result<i64, Errno>, write: bool) -> Result<i64, Errno> {
    if r.as_ref().is_ok_and(|&n| n > 0) {
        crate::inotify::hooks::file(f, if write { crate::inotify::IN_MODIFY } else { crate::inotify::IN_ACCESS });
    }
    r
}

pub fn read(s: &Sys<'_>, a: Args) -> i64 {
    ret(s.file(a[0]).and_then(|f| notify_rw(&f, file_read(s, &f, a[1], a[2]), false)))
}

pub fn write(s: &Sys<'_>, a: Args) -> i64 {
    ret(s.file(a[0]).and_then(|f| notify_rw(&f, file_write(s, &f, a[1], a[2]), true)))
}

pub fn pread64(s: &Sys<'_>, a: Args) -> i64 {
    let off = a[3] as i64;
    if off < 0 {
        return -EINVAL;
    }
    ret((|| {
        let f = s.file(a[0])?;
        if !f.file.seekable() {
            return Err(ESPIPE);
        }
        if !f.can_read() {
            return Err(EBADF);
        }
        if !access_ok(a[1], a[2]) {
            return Err(EFAULT);
        }
        notify_rw(&f, read_into(s, a[1], a[2], |done, chunk| f.pread(off as u64 + done as u64, chunk)), false)
    })())
}

pub fn pwrite64(s: &Sys<'_>, a: Args) -> i64 {
    let off = a[3] as i64;
    if off < 0 {
        return -EINVAL;
    }
    ret((|| {
        let f = s.file(a[0])?;
        if !f.file.seekable() {
            return Err(ESPIPE);
        }
        if !f.can_write() {
            return Err(EBADF);
        }
        if !access_ok(a[1], a[2]) {
            return Err(EFAULT);
        }
        let count = f.seal_clip(off as u64, a[2])?;
        notify_rw(&f, write_from(s, a[1], count, |done, chunk| f.pwrite(off as u64 + done as u64, chunk)), true)
    })())
}

/// The iovec array (`import_iovec`, linux-6.6/lib/iov_iter.c): EINVAL above UIO_MAXIOV, EFAULT for the array,
/// EINVAL for a negative length (while copying, so before any address check), then EFAULT for a segment that
/// fails `access_ok`; the total is clipped to MAX_RW_COUNT.
pub(crate) fn iovecs(s: &Sys<'_>, iov: u64, cnt: u64) -> Result<Vec<(u64, u64)>, Errno> {
    if cnt > abi::UIO_MAXIOV {
        return Err(EINVAL);
    }
    let mut raw = zeroed(cnt as usize * 16)?;
    if !s.mem.read(iov, &mut raw) {
        return Err(EFAULT);
    }
    let mut out = Vec::new();
    out.try_reserve_exact(cnt as usize).map_err(|_| ENOMEM)?;
    for c in raw.chunks_exact(16) {
        let base = u64::from_le_bytes(c[0..8].try_into().unwrap_or([0; 8]));
        let len = u64::from_le_bytes(c[8..16].try_into().unwrap_or([0; 8]));
        if (len as i64) < 0 {
            return Err(EINVAL);
        }
        out.push((base, len));
    }
    let mut total = 0u64;
    for (base, len) in out.iter_mut() {
        if !access_ok(*base, *len) {
            return Err(EFAULT);
        }
        *len = (*len).min(MAX_RW_COUNT - total);
        total += *len;
    }
    Ok(out)
}

pub fn readv(s: &Sys<'_>, a: Args) -> i64 {
    readv_with(s, a, 0)
}

/// readv with `extra` status flags for a stream's `read_iter` (FS-1b: preadv2's RWF_NOWAIT is O_NONBLOCK).
pub(crate) fn readv_with(s: &Sys<'_>, a: Args, extra: u64) -> i64 {
    let f = match s.file(a[0]) {
        Ok(f) => f,
        Err(e) => return -e,
    };
    ret(notify_rw(&f, readv_inner(s, &f, a, extra), false))
}

fn readv_inner(s: &Sys<'_>, f: &Arc<OpenFile>, a: Args, extra: u64) -> Result<i64, Errno> {
    let f = f.clone();
    (|| {
        let v = iovecs(s, a[1], a[2])?;
        if !f.can_read() {
            return Err(EBADF);
        }
        // `vfs_readv`: nothing to read is 0 without asking the file.
        if v.iter().all(|x| x.1 == 0) {
            return Ok(0);
        }
        if !f.file.seekable() {
            let mut iov = UserIov::new(s.mem, v.clone());
            if let Some(r) = f.file.read_iter(&mut iov, f.status_flags() | extra) {
                return r.map(|n| n as i64);
            }
        }
        let mut total = 0i64;
        for (base, len) in v {
            if len == 0 {
                continue;
            }
            match file_read(s, &f, base, len) {
                Ok(n) => {
                    total += n;
                    if (n as u64) < len {
                        break;
                    }
                }
                Err(e) => return if total > 0 { Ok(total) } else { Err(e) },
            }
        }
        Ok(total)
    })()
}

pub fn writev(s: &Sys<'_>, a: Args) -> i64 {
    writev_with(s, a, 0)
}

pub(crate) fn writev_with(s: &Sys<'_>, a: Args, extra: u64) -> i64 {
    let f = match s.file(a[0]) {
        Ok(f) => f,
        Err(e) => return -e,
    };
    ret(notify_rw(&f, writev_inner(s, &f, a, extra), true))
}

fn writev_inner(s: &Sys<'_>, f: &Arc<OpenFile>, a: Args, extra: u64) -> Result<i64, Errno> {
    let f = f.clone();
    (|| {
        let v = iovecs(s, a[1], a[2])?;
        if !f.can_write() {
            return Err(EBADF);
        }
        if v.iter().all(|x| x.1 == 0) {
            return Ok(0);
        }
        if !f.file.seekable() {
            let mut iov = UserIov::new(s.mem, v.clone());
            if let Some(r) = f.file.write_iter(&mut iov, f.status_flags() | extra) {
                return r.map(|n| n as i64);
            }
        }
        // FS-10: a sealed tmpfs file decides on the whole vector once (Linux: one `generic_perform_write`).
        let mut allowed = if f.file.seekable() { f.seal_clip(f.pos(), v.iter().map(|x| x.1).sum())? } else { u64::MAX };
        let mut total = 0i64;
        for (base, len) in v {
            if len == 0 {
                continue;
            }
            if allowed == 0 {
                break;
            }
            let len = len.min(allowed);
            allowed -= len;
            match file_write(s, &f, base, len) {
                Ok(n) => {
                    total += n;
                    if (n as u64) < len {
                        break;
                    }
                }
                Err(e) => return if total > 0 { Ok(total) } else { Err(e) },
            }
        }
        Ok(total)
    })()
}

pub fn lseek(s: &Sys<'_>, a: Args) -> i64 {
    ret((|| {
        let f = s.file(a[0])?;
        let whence = a[2] as u32;
        if whence > abi::SEEK_HOLE {
            return Err(EINVAL);
        }
        f.seek(a[1] as i64, whence).map(|p| p as i64)
    })())
}

pub fn fsync(s: &Sys<'_>, a: Args) -> i64 {
    ret(s.file(a[0]).and_then(|f| f.file.fsync()).map(|_| 0))
}

// ---- opening, closing, fds ----------------------------------------------------------------------------

fn do_open(s: &Sys<'_>, dirfd: u64, path: u64, flags: u64, mode: u64) -> Result<i64, Errno> {
    let p = s.path(path, false)?;
    // The fd is taken before the walk on Linux, so a full table answers EMFILE before ENOENT.
    if !s.fds.has_free() {
        return Err(EMFILE);
    }
    let base = s.base(dirfd, &p)?;
    let f = s.ctx().open(base.as_ref(), &p, flags, mode as u32)?;
    s.fds.install_flags(f, 0, flags & abi::O_CLOEXEC != 0).map(|fd| fd as i64)
}

pub fn open(s: &Sys<'_>, a: Args) -> i64 {
    ret(do_open(s, AT_FDCWD as u64, a[0], a[1], a[2]))
}

pub fn openat(s: &Sys<'_>, a: Args) -> i64 {
    ret(do_open(s, a[0], a[1], a[2], a[3]))
}

pub fn creat(s: &Sys<'_>, a: Args) -> i64 {
    ret(do_open(s, AT_FDCWD as u64, a[0], abi::O_CREAT | abi::O_WRONLY | abi::O_TRUNC, a[1]))
}

pub fn close(s: &Sys<'_>, a: Args) -> i64 {
    ret(fd_of(a[0]).and_then(|fd| s.fds.close(fd)).map(|_| 0))
}

pub fn dup(s: &Sys<'_>, a: Args) -> i64 {
    ret(s.raw(a[0]).and_then(|f| s.fds.install_flags(f, 0, false)).map(|fd| fd as i64))
}

fn dup3_inner(s: &Sys<'_>, old: u64, new: u64, flags: u64) -> Result<i64, Errno> {
    if flags & !abi::O_CLOEXEC != 0 {
        return Err(EINVAL);
    }
    if old as u32 == new as u32 {
        return Err(EINVAL);
    }
    let n = fd_of(new)?;
    if n >= s.fds.limit() {
        return Err(EBADF);
    }
    let f = s.raw(old)?;
    s.fds.install_at_flags(n, f, flags & abi::O_CLOEXEC != 0)?;
    Ok(n as i64)
}

pub fn dup2(s: &Sys<'_>, a: Args) -> i64 {
    if a[0] as u32 == a[1] as u32 {
        return ret(s.raw(a[0]).map(|_| a[1] as u32 as i64));
    }
    ret(dup3_inner(s, a[0], a[1], 0))
}

pub fn dup3(s: &Sys<'_>, a: Args) -> i64 {
    ret(dup3_inner(s, a[0], a[1], a[2]))
}

pub fn fcntl(s: &Sys<'_>, a: Args) -> i64 {
    ret((|| {
        let fd = fd_of(a[0])?;
        let f = s.raw(a[0])?;
        let cmd = a[1] as u32 as u64;
        if f.is_path_only() && !matches!(cmd, abi::F_DUPFD | abi::F_DUPFD_CLOEXEC | abi::F_GETFD | abi::F_SETFD | abi::F_GETFL) {
            return Err(EBADF);
        }
        match cmd {
            abi::F_DUPFD | abi::F_DUPFD_CLOEXEC => {
                let from = a[2] as u32 as i32;
                if from < 0 || from as usize >= s.fds.limit() {
                    return Err(EINVAL);
                }
                s.fds.install_flags(f, from as usize, cmd == abi::F_DUPFD_CLOEXEC).map(|n| n as i64)
            }
            abi::F_GETFD => s.fds.cloexec(fd).map(|c| c as i64),
            abi::F_SETFD => s.fds.set_cloexec(fd, a[2] & abi::FD_CLOEXEC != 0).map(|_| 0),
            abi::F_GETFL => Ok(f.status_flags() as i64),
            abi::F_SETFL => {
                f.set_status_flags(a[2]);
                Ok(0)
            }
            F_GETLK | F_OFD_GETLK | F_SETLK | F_SETLKW | F_OFD_SETLK | F_OFD_SETLKW => posix_lock(s, &f, cmd, a[2]),
            // FS-3: `pipe_fcntl` (the int argument as unsigned); EBADF on anything that is not a pipe.
            crate::pipe::F_SETPIPE_SZ | crate::pipe::F_GETPIPE_SZ => f.file.fcntl(cmd, a[2] as u32 as u64, s.cred).unwrap_or(Err(EBADF)),
            // FS-10: `memfd_fcntl` (the seal part AB-5 dispatches to when it takes fcntl over).
            crate::memfd::F_ADD_SEALS | crate::memfd::F_GET_SEALS => crate::memfd::seal_fcntl(&f, cmd, a[2]),
            _ => Err(EINVAL),
        }
    })())
}

const F_GETLK: u64 = 5;
const F_SETLK: u64 = 6;
const F_SETLKW: u64 = 7;
const F_OFD_GETLK: u64 = 36;
const F_OFD_SETLK: u64 = 37;
const F_OFD_SETLKW: u64 = 38;
const F_RDLCK: i16 = 0;
const F_WRLCK: i16 = 1;
const F_UNLCK: i16 = 2;

static LOCKS_UNTRACKED: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// POSIX and OFD record locks (linux-6.6/fs/locks.c `fcntl_getlk`, `fcntl_setlk`, `flock64_to_posix_lock`,
/// `check_fmode_for_setlk`), checked like Linux but not recorded yet: every lock is granted and F_GETLK always
/// answers F_UNLCK. Within one process that is exactly Linux's answer (a process never conflicts with its own
/// POSIX locks); between processes locks do not exclude each other until a lock table exists (open point in
/// plan/notes/fs-FS-1.md).
fn posix_lock(s: &Sys<'_>, f: &OpenFile, cmd: u64, arg: u64) -> Result<i64, Errno> {
    // struct flock (x86-64): l_type i16, l_whence i16, pad, l_start i64, l_len i64, l_pid i32, pad: 32 bytes.
    let mut fl = [0u8; 32];
    if !s.mem.read(arg, &mut fl) {
        return Err(EFAULT);
    }
    let l_type = i16::from_le_bytes([fl[0], fl[1]]);
    let whence = i16::from_le_bytes([fl[2], fl[3]]);
    let start = i64::from_le_bytes(fl[8..16].try_into().unwrap_or([0; 8]));
    let len = i64::from_le_bytes(fl[16..24].try_into().unwrap_or([0; 8]));
    let pid = i32::from_le_bytes(fl[24..28].try_into().unwrap_or([0; 4]));
    let get = matches!(cmd, F_GETLK | F_OFD_GETLK);
    if cmd == F_GETLK && l_type != F_RDLCK && l_type != F_WRLCK {
        return Err(EINVAL);
    }
    // flock64_to_posix_lock: the start, the range, then the type.
    let base: i64 = match whence as u32 {
        abi::SEEK_SET => 0,
        abi::SEEK_CUR => f.pos() as i64,
        abi::SEEK_END => f.file.size() as i64,
        _ => return Err(EINVAL),
    };
    if start > i64::MAX - base {
        return Err(EOVERFLOW);
    }
    let first = base + start;
    if first < 0 {
        return Err(EINVAL);
    }
    if len > 0 && len - 1 > i64::MAX - first {
        return Err(EOVERFLOW);
    }
    if len < 0 && first + len < 0 {
        return Err(EINVAL);
    }
    if !matches!(l_type, F_RDLCK | F_WRLCK | F_UNLCK) {
        return Err(EINVAL);
    }
    if !get {
        if (l_type == F_RDLCK && !f.can_read()) || (l_type == F_WRLCK && !f.can_write()) {
            return Err(EBADF);
        }
    }
    if matches!(cmd, F_OFD_GETLK | F_OFD_SETLK | F_OFD_SETLKW) && pid != 0 {
        return Err(EINVAL);
    }
    if l_type != F_UNLCK && !LOCKS_UNTRACKED.swap(true, core::sync::atomic::Ordering::Relaxed) {
        klog::event!(Info, "vfs", "locks.untracked", cmd = cmd; "record locks are granted without a lock table");
    }
    if get {
        fl[0..2].copy_from_slice(&F_UNLCK.to_le_bytes());
        s.copy_out(arg, &fl)?;
    }
    Ok(0)
}

// ---- stat, access, links ----------------------------------------------------------------------------

/// The path argument of a call that may take AT_EMPTY_PATH: the object, following symlinks or not.
pub(crate) fn at_lookup(s: &Sys<'_>, dirfd: u64, path: u64, flags: u64, follow_default: bool) -> Result<Path, Errno> {
    let allow_empty = flags & AT_EMPTY_PATH != 0;
    let p = s.path(path, allow_empty)?;
    if p.is_empty() {
        return s.empty_target(dirfd);
    }
    let base = s.base(dirfd, &p)?;
    let follow = follow_default && flags & AT_SYMLINK_NOFOLLOW == 0;
    s.ctx().lookup(base.as_ref(), &p, follow)
}

fn put_stat(s: &Sys<'_>, buf: u64, a: &abi::Attr) -> Result<i64, Errno> {
    s.copy_out(buf, &abi::stat_bytes(a)).map(|_| 0)
}

pub fn stat(s: &Sys<'_>, a: Args) -> i64 {
    ret(at_lookup(s, AT_FDCWD as u64, a[0], 0, true).and_then(|p| put_stat(s, a[1], &p.attr())))
}

pub fn lstat(s: &Sys<'_>, a: Args) -> i64 {
    ret(at_lookup(s, AT_FDCWD as u64, a[0], AT_SYMLINK_NOFOLLOW, true).and_then(|p| put_stat(s, a[1], &p.attr())))
}

pub fn fstat(s: &Sys<'_>, a: Args) -> i64 {
    ret(s.raw(a[0]).and_then(|f| put_stat(s, a[1], &f.stat())))
}

pub fn newfstatat(s: &Sys<'_>, a: Args) -> i64 {
    let flags = a[3] as u32 as u64;
    if flags & !(AT_SYMLINK_NOFOLLOW | abi::AT_NO_AUTOMOUNT | AT_EMPTY_PATH | abi::AT_STATX_SYNC_TYPE) != 0 {
        return -EINVAL;
    }
    ret((|| {
        if flags & AT_EMPTY_PATH != 0 {
            // fstatat(fd, "", AT_EMPTY_PATH) is fstat (and works on any fd, also those without a name).
            let p = s.path(a[1], true)?;
            if p.is_empty() && a[0] as u32 as i32 != AT_FDCWD {
                return put_stat(s, a[2], &s.raw(a[0])?.stat());
            }
        }
        let p = at_lookup(s, a[0], a[1], flags, true)?;
        put_stat(s, a[2], &p.attr())
    })())
}

pub fn statx(s: &Sys<'_>, a: Args) -> i64 {
    let (flags, mask) = (a[2] as u32 as u64, a[3] as u32);
    if mask & abi::STATX_RESERVED != 0 || flags & abi::AT_STATX_SYNC_TYPE == abi::AT_STATX_SYNC_TYPE {
        return -EINVAL;
    }
    if flags & !(AT_SYMLINK_NOFOLLOW | abi::AT_NO_AUTOMOUNT | AT_EMPTY_PATH | abi::AT_STATX_SYNC_TYPE) != 0 {
        return -EINVAL;
    }
    ret((|| {
        // Newer kernels take a NULL path with AT_EMPTY_PATH as "the fd itself".
        let attr = if a[1] == 0 && flags & AT_EMPTY_PATH != 0 {
            if a[0] as u32 as i32 == AT_FDCWD { s.ctx().cwd.attr() } else { s.raw(a[0])?.stat() }
        } else {
            let p = s.path(a[1], flags & AT_EMPTY_PATH != 0)?;
            if p.is_empty() && a[0] as u32 as i32 != AT_FDCWD {
                s.raw(a[0])?.stat()
            } else {
                at_lookup(s, a[0], a[1], flags, true)?.attr()
            }
        };
        s.copy_out(a[4], &abi::statx_bytes(&attr, mask)).map(|_| 0)
    })())
}

fn do_access(s: &Sys<'_>, dirfd: u64, path: u64, mode: u64, flags: u64) -> Result<i64, Errno> {
    if mode & !7 != 0 {
        return Err(EINVAL);
    }
    if flags & !(abi::AT_EACCESS | AT_SYMLINK_NOFOLLOW | AT_EMPTY_PATH) != 0 {
        return Err(EINVAL);
    }
    let p = at_lookup(s, dirfd, path, flags, true)?;
    // Real and effective ids are the same in V2, so AT_EACCESS changes nothing.
    if mode == 0 {
        return Ok(0);
    }
    let m = &p.dentry.inode.meta;
    // FS-13: X_OK of a regular file on a noexec mount (linux-6.6/fs/open.c:489), before the permission bits.
    if mode as u32 & abi::X_OK != 0 && m.file_type() == uapi::S_IFREG && p.noexec() {
        return Err(EACCES);
    }
    if mode as u32 & abi::W_OK != 0 && p.mnt.readonly && matches!(m.file_type(), uapi::S_IFREG | uapi::S_IFDIR | uapi::S_IFLNK) {
        return Err(EROFS);
    }
    m.permission(s.cred, mode as u32).map(|_| 0)
}

pub fn access(s: &Sys<'_>, a: Args) -> i64 {
    ret(do_access(s, AT_FDCWD as u64, a[0], a[1], 0))
}

pub fn faccessat(s: &Sys<'_>, a: Args) -> i64 {
    ret(do_access(s, a[0], a[1], a[2], 0))
}

pub fn faccessat2(s: &Sys<'_>, a: Args) -> i64 {
    ret(do_access(s, a[0], a[1], a[2], a[3] as u32 as u64))
}

pub fn readlinkat_inner(s: &Sys<'_>, dirfd: u64, path: u64, buf: u64, size: u64) -> Result<i64, Errno> {
    if size as u32 as i32 <= 0 {
        return Err(EINVAL);
    }
    let p = s.path(path, true)?;
    let target = if p.is_empty() {
        s.empty_target(dirfd)?
    } else {
        let base = s.base(dirfd, &p)?;
        s.ctx().lookup(base.as_ref(), &p, false)?
    };
    // An empty path naming something that is not a link: ENOENT, a named one EINVAL (linux-6.6/fs/stat.c:494).
    let body = target.dentry.inode.readlink().map_err(|e| if e == EINVAL && p.is_empty() { ENOENT } else { e })?;
    let n = body.len().min(size as u32 as usize);
    s.copy_out(buf, &body[..n])?;
    Ok(n as i64)
}

pub fn readlink(s: &Sys<'_>, a: Args) -> i64 {
    ret(readlinkat_inner(s, AT_FDCWD as u64, a[0], a[1], a[2]))
}

pub fn readlinkat(s: &Sys<'_>, a: Args) -> i64 {
    ret(readlinkat_inner(s, a[0], a[1], a[2], a[3]))
}

pub fn getdents64(s: &Sys<'_>, a: Args) -> i64 {
    ret((|| {
        let f = s.file(a[0])?;
        let count = (a[2] as u32 as usize).min(1 << 20);
        let r = ops::read_dir_into(&f, count, |out| s.copy_out(a[1], out)).map(|n| n as i64);
        // FS-14: `iterate_dir` reports IN_ACCESS after every pass over a live directory.
        if r.is_ok() {
            crate::inotify::hooks::file(&f, crate::inotify::IN_ACCESS);
        }
        r
    })())
}

// ---- names ----------------------------------------------------------------------------------------------

fn at_path(s: &Sys<'_>, dirfd: u64, path: u64) -> Result<(Option<Path>, Vec<u8>), Errno> {
    let p = s.path(path, false)?;
    let base = s.base(dirfd, &p)?;
    Ok((base, p))
}

pub fn mkdir(s: &Sys<'_>, a: Args) -> i64 {
    ret(at_path(s, AT_FDCWD as u64, a[0]).and_then(|(b, p)| s.ctx().mkdir(b.as_ref(), &p, a[1] as u32)).map(|_| 0))
}

pub fn mkdirat(s: &Sys<'_>, a: Args) -> i64 {
    ret(at_path(s, a[0], a[1]).and_then(|(b, p)| s.ctx().mkdir(b.as_ref(), &p, a[2] as u32)).map(|_| 0))
}

pub fn unlink(s: &Sys<'_>, a: Args) -> i64 {
    ret(at_path(s, AT_FDCWD as u64, a[0]).and_then(|(b, p)| s.ctx().unlink(b.as_ref(), &p)).map(|_| 0))
}

pub fn rmdir(s: &Sys<'_>, a: Args) -> i64 {
    ret(at_path(s, AT_FDCWD as u64, a[0]).and_then(|(b, p)| s.ctx().rmdir(b.as_ref(), &p)).map(|_| 0))
}

pub fn unlinkat(s: &Sys<'_>, a: Args) -> i64 {
    let flags = a[2] as u32 as u64;
    if flags & !abi::AT_REMOVEDIR != 0 {
        return -EINVAL;
    }
    ret(at_path(s, a[0], a[1])
        .and_then(|(b, p)| if flags & abi::AT_REMOVEDIR != 0 { s.ctx().rmdir(b.as_ref(), &p) } else { s.ctx().unlink(b.as_ref(), &p) })
        .map(|_| 0))
}

fn do_rename(s: &Sys<'_>, od: u64, op: u64, nd: u64, np: u64, flags: u64) -> Result<i64, Errno> {
    if flags & !(abi::RENAME_NOREPLACE | abi::RENAME_EXCHANGE | abi::RENAME_WHITEOUT) != 0 {
        return Err(EINVAL);
    }
    let (ob, o) = at_path(s, od, op)?;
    let (nb, n) = at_path(s, nd, np)?;
    s.ctx().rename(ob.as_ref(), &o, nb.as_ref(), &n, flags).map(|_| 0)
}

pub fn rename(s: &Sys<'_>, a: Args) -> i64 {
    ret(do_rename(s, AT_FDCWD as u64, a[0], AT_FDCWD as u64, a[1], 0))
}

pub fn renameat(s: &Sys<'_>, a: Args) -> i64 {
    ret(do_rename(s, a[0], a[1], a[2], a[3], 0))
}

pub fn renameat2(s: &Sys<'_>, a: Args) -> i64 {
    ret(do_rename(s, a[0], a[1], a[2], a[3], a[4] as u32 as u64))
}

fn do_link(s: &Sys<'_>, od: u64, op: u64, nd: u64, np: u64, flags: u64) -> Result<i64, Errno> {
    if flags & !(abi::AT_SYMLINK_FOLLOW | AT_EMPTY_PATH) != 0 {
        return Err(EINVAL);
    }
    // AT_EMPTY_PATH needs CAP_DAC_READ_SEARCH; without it Linux answers ENOENT for the empty name.
    let allow_empty = flags & AT_EMPTY_PATH != 0 && s.cred.uid == 0;
    let o = s.path(op, allow_empty)?;
    let old = if o.is_empty() {
        s.empty_target(od)?
    } else {
        let ob = s.base(od, &o)?;
        s.ctx().lookup(ob.as_ref(), &o, flags & abi::AT_SYMLINK_FOLLOW != 0)?
    };
    let (nb, n) = at_path(s, nd, np)?;
    s.ctx().link_to(&old, nb.as_ref(), &n).map(|_| 0)
}

pub fn link(s: &Sys<'_>, a: Args) -> i64 {
    ret(do_link(s, AT_FDCWD as u64, a[0], AT_FDCWD as u64, a[1], 0))
}

pub fn linkat(s: &Sys<'_>, a: Args) -> i64 {
    ret(do_link(s, a[0], a[1], a[2], a[3], a[4] as u32 as u64))
}

fn do_symlink(s: &Sys<'_>, target: u64, nd: u64, np: u64) -> Result<i64, Errno> {
    let t = s.path(target, false)?;
    let (b, p) = at_path(s, nd, np)?;
    s.ctx().symlink(&t, b.as_ref(), &p).map(|_| 0)
}

pub fn symlink(s: &Sys<'_>, a: Args) -> i64 {
    ret(do_symlink(s, a[0], AT_FDCWD as u64, a[1]))
}

pub fn symlinkat(s: &Sys<'_>, a: Args) -> i64 {
    ret(do_symlink(s, a[0], a[1], a[2]))
}

pub fn truncate(s: &Sys<'_>, a: Args) -> i64 {
    if (a[1] as i64) < 0 {
        return -EINVAL;
    }
    ret(at_path(s, AT_FDCWD as u64, a[0]).and_then(|(b, p)| s.ctx().truncate(b.as_ref(), &p, a[1])).map(|_| 0))
}

pub fn ftruncate(s: &Sys<'_>, a: Args) -> i64 {
    if (a[1] as i64) < 0 {
        return -EINVAL;
    }
    ret((|| {
        let f = s.file(a[0])?;
        let i = f.inode().filter(|i| i.is_reg()).ok_or(EINVAL)?;
        if !f.can_write() {
            return Err(EINVAL);
        }
        let _g = match &f.path {
            Some(p) => Some(p.dentry.sb.write_begin()?),
            None => None,
        };
        i.truncate(a[1])?;
        if let Some(p) = &f.path {
            crate::inotify::hooks::changed(&p.dentry, crate::setattr::ATTR_MTIME, true);
        }
        Ok(0)
    })())
}

/// fallocate(fd, mode, offset, len) (`ksys_fallocate` + `vfs_fallocate`, linux-6.6/fs/open.c:244-330, with the
/// mode rules of CachyOS's kernel, see `fallocate_mode_ok`) in Linux's order: EBADF (no fd, O_PATH), EINVAL
/// (offset < 0, len <= 0), EOPNOTSUPP (a mode that is not one of the known ones), EBADF (not open for writing),
/// EPERM (immutable), ESPIPE (pipe, FIFO), EISDIR, ENODEV (anything but a regular file or block device: devices,
/// eventfd, sockets), EFBIG (the end past the largest offset), then the file's own fallocate under the freeze
/// guard (`Inode::fallocate`: tmpfs files, EOPNOTSUPP for the rest).
pub fn fallocate(s: &Sys<'_>, a: Args) -> i64 {
    ret((|| {
        let f = s.file(a[0])?;
        let mode = a[1] as u32;
        let (off, len) = (a[2] as i64, a[3] as i64);
        if off < 0 || len <= 0 {
            return Err(EINVAL);
        }
        if !fallocate_mode_ok(mode) {
            return Err(EOPNOTSUPP);
        }
        if !f.can_write() {
            return Err(EBADF);
        }
        let inode = f.inode();
        if inode.is_some_and(|i| i.immutable.load(core::sync::atomic::Ordering::Relaxed)) {
            return Err(EPERM);
        }
        // `file_inode(file)->i_mode`: the node's type (a device node opened from devfs is S_IFCHR), else the
        // object's own (pipes S_IFIFO, anonymous inodes none).
        let ty = inode.map_or_else(|| f.file.mode(), |i| i.meta.mode()) & uapi::S_IFMT;
        match ty {
            uapi::S_IFIFO => return Err(ESPIPE),
            uapi::S_IFDIR => return Err(EISDIR),
            uapi::S_IFREG | abi::S_IFBLK => {}
            _ => return Err(ENODEV),
        }
        // Both are at most i64::MAX, so the sum fits: past MAX_LFS_FILESIZE (or "wrapping through zero").
        if off as u64 + len as u64 > abi::MAX_FILE_SIZE {
            return Err(EFBIG);
        }
        let (Some(i), Some(p)) = (inode, f.path.as_ref()) else { return Err(EOPNOTSUPP) };
        let _g = p.dentry.sb.write_begin()?;
        i.fallocate(mode, off as u64, len as u64)?;
        // `vfs_fallocate`: IN_MODIFY after every success (KEEP_SIZE too).
        crate::inotify::hooks::file(&f, crate::inotify::IN_MODIFY);
        Ok(0)
    })())
}

/// The modes `vfs_fallocate` lets through to the file system. 6.6 checked them bit by bit (EOPNOTSUPP for
/// unknown bits, PUNCH_HOLE with ZERO_RANGE or without KEEP_SIZE; EINVAL for COLLAPSE/INSERT/UNSHARE combined
/// with others); CachyOS's kernel takes exactly one operation, KEEP_SIZE only where it means something, and
/// FALLOC_FL_WRITE_ZEROES, and answers EOPNOTSUPP for everything else (measured on 7.2.11 for all 256 low
/// values and high bits: allowed are 0, KEEP_SIZE, PUNCH_HOLE|KEEP_SIZE, COLLAPSE_RANGE, ZERO_RANGE with or
/// without KEEP_SIZE, INSERT_RANGE, UNSHARE_RANGE with or without KEEP_SIZE, WRITE_ZEROES). The oracle wins.
fn fallocate_mode_ok(mode: u32) -> bool {
    use abi::{FALLOC_FL_COLLAPSE_RANGE as COLLAPSE, FALLOC_FL_INSERT_RANGE as INSERT, FALLOC_FL_KEEP_SIZE as KEEP, FALLOC_FL_PUNCH_HOLE as PUNCH};
    use abi::{FALLOC_FL_UNSHARE_RANGE as UNSHARE, FALLOC_FL_WRITE_ZEROES as WRITE_ZEROES, FALLOC_FL_ZERO_RANGE as ZERO};
    let op = mode & !KEEP;
    let keep = mode & KEEP != 0;
    match op {
        0 | ZERO | UNSHARE => true,
        PUNCH => keep,
        COLLAPSE | INSERT | WRITE_ZEROES => !keep,
        _ => false,
    }
}

// ---- working directory, umask --------------------------------------------------------------------------

pub fn getcwd(s: &Sys<'_>, a: Args) -> i64 {
    ret((|| {
        let c = s.ctx();
        if c.cwd.dentry.is_unlinked() {
            return Err(ENOENT);
        }
        let mut p = match c.ns.path_of(&c.root, &c.cwd) {
            Some(p) => p,
            None => {
                let mut v = b"(unreachable)".to_vec();
                v.extend_from_slice(&c.cwd.dentry.path_in_fs().concat());
                v
            }
        };
        p.push(0);
        if p.len() > PATH_MAX {
            return Err(ENAMETOOLONG);
        }
        if p.len() as u64 > a[1] {
            return Err(ERANGE);
        }
        s.copy_out(a[0], &p)?;
        Ok(p.len() as i64)
    })())
}

pub fn chdir(s: &Sys<'_>, a: Args) -> i64 {
    ret((|| {
        let p = s.path(a[0], false)?;
        let t = s.ctx().chdir_target(None, &p)?;
        s.fds.fs.set_cwd(t);
        Ok(0)
    })())
}

pub fn fchdir(s: &Sys<'_>, a: Args) -> i64 {
    ret((|| {
        let f = s.raw(a[0])?;
        let p = f.path.clone().ok_or(ENOTDIR)?;
        Ctx::dir_usable(&p, s.cred)?;
        s.fds.fs.set_cwd(p);
        Ok(0)
    })())
}

pub fn umask(s: &Sys<'_>, a: Args) -> i64 {
    s.fds.fs.set_umask(a[0] as u32 & 0o777) as i64
}

// ---- pipes, FIFOs, eventfd (FS-3) -----------------------------------------------------------------------

/// `do_pipe2` (fs/pipe.c): the flags, the pipe (ENOPKG for O_NOTIFICATION_PIPE without watch queues), two fd
/// numbers reserved (EMFILE), the pair copied out, and only then the fds published (`fd_install`): another
/// thread of the program never sees fds that an EFAULT takes back.
fn do_pipe2(s: &Sys<'_>, fildes: u64, flags: u64) -> Result<i64, Errno> {
    let flags = flags as u32 as u64;
    if flags & !(abi::O_CLOEXEC | abi::O_NONBLOCK | abi::O_DIRECT | crate::pipe::O_NOTIFICATION_PIPE) != 0 {
        return Err(EINVAL);
    }
    let (r, w) = crate::pipe::new_pipe(s.cred, flags)?;
    let cloexec = flags & abi::O_CLOEXEC != 0;
    let rfd = s.fds.reserve(0, cloexec)?;
    let wfd = match s.fds.reserve(0, cloexec) {
        Ok(n) => n,
        Err(e) => {
            s.fds.unreserve(rfd);
            return Err(e);
        }
    };
    let mut out = [0u8; 8];
    out[..4].copy_from_slice(&(rfd as i32).to_le_bytes());
    out[4..].copy_from_slice(&(wfd as i32).to_le_bytes());
    if !s.mem.write(fildes, &out) {
        s.fds.unreserve(rfd);
        s.fds.unreserve(wfd);
        return Err(EFAULT);
    }
    s.fds.commit(rfd, r);
    s.fds.commit(wfd, w);
    Ok(0)
}

pub fn pipe(s: &Sys<'_>, a: Args) -> i64 {
    ret(do_pipe2(s, a[0], 0))
}

pub fn pipe2(s: &Sys<'_>, a: Args) -> i64 {
    ret(do_pipe2(s, a[0], a[1]))
}

/// `do_mknodat`: `may_mknod` on the type first (S_IFDIR EPERM, unknown types EINVAL), then the name.
/// `mode` is a `umode_t` (16 bits), `dev` an unsigned int.
fn do_mknod(s: &Sys<'_>, dirfd: u64, path: u64, mode: u64, dev: u64) -> Result<i64, Errno> {
    let mode = mode as u16 as u32;
    match mode & uapi::S_IFMT {
        0 | uapi::S_IFREG | uapi::S_IFCHR | abi::S_IFBLK | uapi::S_IFIFO | abi::S_IFSOCK => {}
        uapi::S_IFDIR => return Err(EPERM),
        _ => return Err(EINVAL),
    }
    let (b, p) = at_path(s, dirfd, path)?;
    s.ctx().mknod(b.as_ref(), &p, mode, dev as u32).map(|_| 0)
}

pub fn mknod(s: &Sys<'_>, a: Args) -> i64 {
    ret(do_mknod(s, AT_FDCWD as u64, a[0], a[1], a[2]))
}

pub fn mknodat(s: &Sys<'_>, a: Args) -> i64 {
    ret(do_mknod(s, a[0], a[1], a[2], a[3]))
}

/// `do_eventfd`: the flags, then the fd (the initial count is an unsigned int).
fn do_eventfd(s: &Sys<'_>, count: u64, flags: u64) -> Result<i64, Errno> {
    let flags = flags as u32 as u64;
    if flags & !crate::eventfd::EFD_FLAGS != 0 {
        return Err(EINVAL);
    }
    let f = crate::eventfd::new_eventfd(count as u32, flags);
    s.fds.install_flags(f, 0, flags & crate::eventfd::EFD_CLOEXEC != 0).map(|fd| fd as i64)
}

pub fn eventfd(s: &Sys<'_>, a: Args) -> i64 {
    ret(do_eventfd(s, a[0], 0))
}

pub fn eventfd2(s: &Sys<'_>, a: Args) -> i64 {
    ret(do_eventfd(s, a[0], a[1]))
}

/// Every call of this module with its x86-64 number (the glue registers exactly these).
pub const CALLS: &[(usize, &str, SysFn)] = &[
    (0, "read", read),
    (1, "write", write),
    (2, "open", open),
    (3, "close", close),
    (4, "stat", stat),
    (5, "fstat", fstat),
    (6, "lstat", lstat),
    (8, "lseek", lseek),
    (17, "pread64", pread64),
    (18, "pwrite64", pwrite64),
    (19, "readv", readv),
    (20, "writev", writev),
    (21, "access", access),
    (22, "pipe", pipe),
    (32, "dup", dup),
    (33, "dup2", dup2),
    (72, "fcntl", fcntl),
    (74, "fsync", fsync),
    (75, "fdatasync", fsync),
    (76, "truncate", truncate),
    (77, "ftruncate", ftruncate),
    (79, "getcwd", getcwd),
    (80, "chdir", chdir),
    (81, "fchdir", fchdir),
    (82, "rename", rename),
    (83, "mkdir", mkdir),
    (84, "rmdir", rmdir),
    (85, "creat", creat),
    (86, "link", link),
    (87, "unlink", unlink),
    (88, "symlink", symlink),
    (89, "readlink", readlink),
    (95, "umask", umask),
    (133, "mknod", mknod),
    (217, "getdents64", getdents64),
    (257, "openat", openat),
    (258, "mkdirat", mkdirat),
    (259, "mknodat", mknodat),
    (262, "newfstatat", newfstatat),
    (263, "unlinkat", unlinkat),
    (264, "renameat", renameat),
    (265, "linkat", linkat),
    (266, "symlinkat", symlinkat),
    (267, "readlinkat", readlinkat),
    (269, "faccessat", faccessat),
    (284, "eventfd", eventfd),
    (285, "fallocate", fallocate),
    (290, "eventfd2", eventfd2),
    (292, "dup3", dup3),
    (293, "pipe2", pipe2),
    (316, "renameat2", renameat2),
    (319, "memfd_create", crate::memfd::memfd_create),
    (332, "statx", statx),
    (439, "faccessat2", faccessat2),
];

/// Dispatch by number (host tests and the glue's table check).
pub fn call(s: &Sys<'_>, nr: usize, a: Args) -> i64 {
    match CALLS.iter().chain(crate::pollsys::CALLS).chain(crate::sys_attr::CALLS).chain(crate::sys_copy::CALLS).chain(crate::inotify::sys::CALLS).find(|c| c.0 == nr) {
        Some(c) => (c.2)(s, a),
        None => -ENOSYS,
    }
}
