//! FS-1b I/O calls: preadv/pwritev/preadv2/pwritev2 on the iovec code of `sys`, sendfile and copy_file_range
//! (a copy loop through one 64 KiB bounce buffer: never what the program asks for), fadvise64 (accepted).
//! linux-6.6/fs/read_write.c (`do_preadv`, `do_sendfile`, `vfs_copy_file_range`), mm/fadvise.c.

use crate::abi::{MAX_FILE_SIZE, MAX_RW_COUNT, O_NONBLOCK};
use crate::fd::OpenFile;
use crate::node::BufSource;
use crate::sys::{iovecs, read_into, readv_with, ret, write_from, writev_with, zeroed, Args, Sys, SysFn, CHUNK};
use crate::Errno;
use alloc::sync::Arc;
use uapi::errno::*;

pub const RWF_HIPRI: u64 = 0x01;
pub const RWF_DSYNC: u64 = 0x02;
pub const RWF_SYNC: u64 = 0x04;
pub const RWF_NOWAIT: u64 = 0x08;
pub const RWF_APPEND: u64 = 0x10;
/// `RWF_SUPPORTED` of 6.6; anything else is EOPNOTSUPP (`kiocb_set_rw_flags`).
const RWF_SUPPORTED: u64 = RWF_HIPRI | RWF_DSYNC | RWF_SYNC | RWF_NOWAIT | RWF_APPEND;

/// `rw_verify_area` for a position and a byte count: EINVAL for a negative position or one the count wraps.
fn verify_area(pos: i64, count: u64) -> Result<(), Errno> {
    if (count as i64) < 0 || pos < 0 || pos.checked_add(count as i64).is_none() {
        return Err(EINVAL);
    }
    Ok(())
}

/// preadv/pwritev at `pos` (`do_preadv`/`do_pwritev` + `vfs_readv`/`vfs_writev`): EINVAL (pos < 0), EBADF,
/// ESPIPE (no positioned I/O), the iovec errors, EBADF (mode), 0 for nothing to move, EOPNOTSUPP (flags).
fn rw_at(s: &Sys<'_>, a: Args, pos: i64, flags: u64, write: bool) -> Result<i64, Errno> {
    if pos < 0 {
        return Err(EINVAL);
    }
    let f = s.file(a[0])?;
    if !f.file.seekable() {
        return Err(ESPIPE);
    }
    let v = iovecs(s, a[1], a[2])?;
    if !(if write { f.can_write() } else { f.can_read() }) {
        return Err(EBADF);
    }
    let total: u64 = v.iter().map(|x| x.1).sum();
    if total == 0 {
        return Ok(0);
    }
    verify_area(pos, total)?;
    if flags & !RWF_SUPPORTED != 0 {
        return Err(EOPNOTSUPP);
    }
    let pos = pos as u64;
    let append = write && flags & RWF_APPEND != 0;
    let mut allowed = if write && !append { f.seal_clip(pos, total)? } else { total };
    let mut done = 0u64;
    for (base, len) in v {
        if len == 0 {
            continue;
        }
        if allowed == 0 {
            break;
        }
        let len = len.min(allowed);
        allowed -= len;
        let at = pos + done;
        let r = if !write {
            read_into(s, base, len, |off, chunk| f.pread(at + off as u64, chunk))
        } else if append {
            write_from(s, base, len, |_, chunk| f.pwrite_end(chunk).map(|x| x.0))
        } else {
            write_from(s, base, len, |off, chunk| f.pwrite(at + off as u64, chunk))
        };
        match r {
            Ok(n) => {
                done += n as u64;
                if (n as u64) < len {
                    break;
                }
            }
            Err(e) if done == 0 => return Err(e),
            Err(_) => break,
        }
    }
    if write && done > 0 && flags & (RWF_DSYNC | RWF_SYNC) != 0 {
        f.file.fsync()?;
    }
    crate::sys::notify_rw(&f, Ok(done as i64), write)
}

/// preadv2/pwritev2 at the current position (offset -1): readv/writev with the flags checked; RWF_NOWAIT on a
/// stream is O_NONBLOCK for this call; RWF_APPEND on a positioned file writes at its end (`append_current`).
fn rw_current(s: &Sys<'_>, a: Args, flags: u64, write: bool) -> i64 {
    if flags & !RWF_SUPPORTED != 0 {
        // `vfs_readv` asks the flags only when there is something to move: a bad fd or iovec answers first.
        let r = (|| {
            let f = s.file(a[0])?;
            let v = iovecs(s, a[1], a[2])?;
            if !(if write { f.can_write() } else { f.can_read() }) {
                return Err(EBADF);
            }
            if v.iter().all(|x| x.1 == 0) { Ok(0) } else { Err(EOPNOTSUPP) }
        })();
        return ret(r);
    }
    if write && flags & RWF_APPEND != 0 {
        if let Ok(f) = s.file(a[0]) {
            if f.file.seekable() && !f.appending() {
                return ret(append_current(s, a, &f, flags));
            }
        }
    }
    let extra = if flags & RWF_NOWAIT != 0 { O_NONBLOCK } else { 0 };
    let r = if write { writev_with(s, a, extra) } else { readv_with(s, a, extra) };
    if write && r > 0 && flags & (RWF_DSYNC | RWF_SYNC) != 0 {
        if let Ok(f) = s.file(a[0]) {
            if let Err(e) = f.file.fsync() {
                return -e;
            }
        }
    }
    r
}

/// pwritev2(fd, iov, n, -1, RWF_APPEND) on a positioned file opened without O_APPEND: IOCB_APPEND writes at
/// the end (`generic_write_checks`) and the file position follows (`*ppos = iocb->ki_pos`, `do_iter_write`).
fn append_current(s: &Sys<'_>, a: Args, f: &OpenFile, flags: u64) -> Result<i64, Errno> {
    let v = iovecs(s, a[1], a[2])?;
    if !f.can_write() {
        return Err(EBADF);
    }
    if v.iter().all(|x| x.1 == 0) {
        return Ok(0);
    }
    if let Some(p) = &f.path {
        p.dentry.sb.wait_thawed()?;
    }
    let done = f.with_pos(|pos| {
        let mut done = 0u64;
        for (base, len) in v {
            if len == 0 {
                continue;
            }
            let r = write_from(s, base, len, |_, chunk| {
                f.pwrite_end(chunk).map(|(n, end)| {
                    *pos = end;
                    n
                })
            });
            match r {
                Ok(n) => {
                    done += n as u64;
                    if (n as u64) < len {
                        break;
                    }
                }
                Err(e) if done == 0 => return Err(e),
                Err(_) => break,
            }
        }
        Ok(done)
    })?;
    if done > 0 && flags & (RWF_DSYNC | RWF_SYNC) != 0 {
        f.file.fsync()?;
    }
    Ok(done as i64)
}

pub fn preadv(s: &Sys<'_>, a: Args) -> i64 {
    ret(rw_at(s, a, a[3] as i64, 0, false))
}

pub fn pwritev(s: &Sys<'_>, a: Args) -> i64 {
    ret(rw_at(s, a, a[3] as i64, 0, true))
}

pub fn preadv2(s: &Sys<'_>, a: Args) -> i64 {
    if a[3] as i64 == -1 {
        return rw_current(s, a, a[5] as u32 as u64, false);
    }
    ret(rw_at(s, a, a[3] as i64, a[5] as u32 as u64, false))
}

pub fn pwritev2(s: &Sys<'_>, a: Args) -> i64 {
    if a[3] as i64 == -1 {
        return rw_current(s, a, a[5] as u32 as u64, true);
    }
    ret(rw_at(s, a, a[3] as i64, a[5] as u32 as u64, true))
}

fn read_i64(s: &Sys<'_>, addr: u64) -> Result<i64, Errno> {
    let mut b = [0u8; 8];
    if s.mem.read(addr, &mut b) { Ok(i64::from_le_bytes(b)) } else { Err(EFAULT) }
}

/// One piece to the output of sendfile: at its position for a file, as a stream write for a pipe or socket
/// (O_NONBLOCK of the output as Linux's `splice_file_to_pipe`).
fn put_out(out: &OpenFile, data: &[u8]) -> Result<usize, Errno> {
    if out.file.seekable() {
        return out.with_pos(|p| out.write_locked(p, data));
    }
    let mut src = BufSource::new(data);
    match out.file.write_iter(&mut src, out.status_flags()) {
        Some(r) => r,
        None => out.file.write_stream(data, out.nonblocking()),
    }
}

/// Copies up to `count` bytes from `fin` at `pos` to `put`, through one bounce buffer. A failure after some
/// bytes ends the copy with the bytes so far. A kill ends it between pieces (`fatal_signal_pending` in Linux's
/// read and write loops): the bytes so far, EINTR before the first. The tick preempts a long copy (B4).
fn copy_loop(fin: &OpenFile, pos: u64, count: u64, mut put: impl FnMut(u64, &[u8]) -> Result<usize, Errno>) -> Result<u64, Errno> {
    let mut buf = zeroed(count.min(CHUNK as u64) as usize)?;
    let mut done = 0u64;
    while done < count {
        if crate::dentry::killed() {
            klog::event!(Debug, "vfs", "copy.killed", done = done, count = count);
            return if done == 0 { Err(EINTR) } else { Ok(done) };
        }
        let k = (count - done).min(CHUNK as u64) as usize;
        let n = match fin.pread(pos + done, &mut buf[..k]) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if done == 0 => return Err(e),
            Err(_) => break,
        };
        let w = match put(done, &buf[..n]) {
            Ok(w) => w,
            Err(e) if done == 0 => return Err(e),
            Err(_) => break,
        };
        done += w as u64;
        if w < n || n < k {
            break;
        }
    }
    Ok(done)
}

/// sendfile(out, in, offset, count) (`sys_sendfile64` + `do_sendfile`). The input must be a file read at a
/// position (V2 has no splice from pipes or sockets: EINVAL as Linux for a file without `splice_read`).
pub fn sendfile(s: &Sys<'_>, a: Args) -> i64 {
    ret((|| {
        let off = match a[2] {
            0 => None,
            p => Some(read_i64(s, p)?),
        };
        let fin = s.file(a[1])?;
        if !fin.can_read() {
            return Err(EBADF);
        }
        let pos = match off {
            None => fin.pos() as i64,
            Some(p) if fin.file.seekable() => p,
            Some(_) => return Err(ESPIPE),
        };
        verify_area(pos, a[3])?;
        let count = a[3].min(MAX_RW_COUNT);
        let fout = s.file(a[0])?;
        if !fout.can_write() {
            return Err(EBADF);
        }
        if fout.file.seekable() {
            verify_area(fout.pos() as i64, count)?;
            // `do_splice_direct`: an O_APPEND output is refused.
            if fout.appending() {
                return Err(EINVAL);
            }
        }
        if !fin.file.seekable() {
            return Err(EINVAL);
        }
        let done = copy_loop(&fin, pos as u64, count, |_, d| put_out(&fout, d))?;
        let end = pos as u64 + done;
        match a[2] {
            0 => fin.with_pos(|p| *p = end),
            p => s.copy_out(p, &(end as i64).to_le_bytes())?,
        }
        Ok(done as i64)
    })())
}

/// copy_file_range (`vfs_copy_file_range` with `generic_copy_file_checks`; the copy is the splice fallback,
/// at most MAX_RW_COUNT per call).
pub fn copy_file_range(s: &Sys<'_>, a: Args) -> i64 {
    ret((|| {
        let fin = s.file(a[0])?;
        let fout = s.file(a[2])?;
        let pos_in = if a[1] != 0 { read_i64(s, a[1])? } else { fin.pos() as i64 };
        let pos_out = if a[3] != 0 { read_i64(s, a[3])? } else { fout.pos() as i64 };
        if a[5] as u32 != 0 {
            return Err(EINVAL);
        }
        let (Some(pi), Some(po)) = (fin.path.as_ref(), fout.path.as_ref()) else { return Err(EINVAL) };
        let (ii, io) = (&pi.dentry.inode, &po.dentry.inode);
        if ii.is_dir() || io.is_dir() {
            return Err(EISDIR);
        }
        if !ii.is_reg() || !io.is_reg() {
            return Err(EINVAL);
        }
        if !fin.can_read() || !fout.can_write() || fout.appending() {
            return Err(EBADF);
        }
        if !Arc::ptr_eq(&pi.dentry.sb, &po.dentry.sb) {
            return Err(EXDEV);
        }
        if io.immutable.load(core::sync::atomic::Ordering::Relaxed) {
            return Err(EPERM);
        }
        let mut count = a[4];
        // `pos + count < pos` in unsigned arithmetic, as C compares a loff_t plus a u64.
        let wraps = |p: i64| (p as u64).wrapping_add(count) < p as u64;
        if wraps(pos_in) || wraps(pos_out) {
            return Err(EOVERFLOW);
        }
        // As C does it: a loff_t minus a u64 is unsigned (a negative pos_in is refused by `verify_area` below).
        let size_in = ii.size() as i64;
        count = if pos_in >= size_in { 0 } else { count.min((size_in as u64).wrapping_sub(pos_in as u64)) };
        if pos_out >= 0 {
            if pos_out as u64 >= MAX_FILE_SIZE {
                return Err(EFBIG);
            }
            count = count.min(MAX_FILE_SIZE - pos_out as u64);
        }
        if Arc::ptr_eq(ii, io) && pos_out.wrapping_add(count as i64) > pos_in && pos_out < pos_in.wrapping_add(count as i64) {
            return Err(EINVAL);
        }
        verify_area(pos_in, count)?;
        verify_area(pos_out, count)?;
        if count == 0 {
            return Ok(0);
        }
        let count = count.min(MAX_RW_COUNT);
        let (pin, pout) = (pos_in as u64, pos_out as u64);
        let done = copy_loop(&fin, pin, count, |off, d| fout.pwrite(pout + off, d))?;
        let mut r = Ok(done as i64);
        for (ptr, f, end) in [(a[1], &fin, pin + done), (a[3], &fout, pout + done)] {
            if done == 0 {
                break;
            }
            if ptr == 0 {
                f.with_pos(|p| *p = end);
            } else if s.copy_out(ptr, &(end as i64).to_le_bytes()).is_err() {
                r = Err(EFAULT);
            }
        }
        r
    })())
}

/// fadvise64 (`generic_fadvise`): accepted for files (V2 keeps its page cache by its own rules); WILLNEED
/// queues the range for the Background read-ahead (FS-12, `PageCache::will_need`); ESPIPE on a FIFO or pipe,
/// EINVAL for a negative length or an unknown advice.
pub fn fadvise64(s: &Sys<'_>, a: Args) -> i64 {
    ret((|| {
        let f = s.file(a[0])?;
        let mode = f.inode().map_or_else(|| f.file.mode(), |i| i.meta.mode());
        if mode & uapi::S_IFMT == uapi::S_IFIFO {
            return Err(ESPIPE);
        }
        if (a[2] as i64) < 0 || a[3] as u32 > 5 {
            return Err(EINVAL);
        }
        if a[3] as u32 == POSIX_FADV_WILLNEED
            && let Some(pc) = f.inode().and_then(|i| i.page_cache())
        {
            pc.will_need(a[1], a[2]);
        }
        Ok(0)
    })())
}

/// linux-6.6/include/uapi/linux/fadvise.h
const POSIX_FADV_WILLNEED: u32 = 3;

pub const CALLS: &[(usize, &str, SysFn)] = &[
    (40, "sendfile", sendfile),
    (221, "fadvise64", fadvise64),
    (295, "preadv", preadv),
    (296, "pwritev", pwritev),
    (326, "copy_file_range", copy_file_range),
    (327, "preadv2", preadv2),
    (328, "pwritev2", pwritev2),
];
