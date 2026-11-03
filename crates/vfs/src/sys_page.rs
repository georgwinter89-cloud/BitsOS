//! PF-4: read and pread of a cached regular file (`PageCache::read_direct`: no data lock). A caller whose user
//! memory copies straight from a page-cache frame (`UserMem::direct_page_copy`; the kernel's since PF-6) gets
//! each page copied to the program with no buffer in between; every other caller's bytes are gathered page by page into one buffer
//! (on the stack up to a page, else of at most `CHUNK`) and copied out per buffer, because each user copy
//! takes the address space's lock (`mm.as`) and a copy per page would take it 16 times as often as the bounce
//! path did. PF-5: cached pages are found and copied with no lock and no reference (`PageCache::read_quick`),
//! into the buffer or, with `UserMem::nofault_page_copy`, straight to the program; what that cannot do (pages
//! not cached, a user page that must fault in, a step that raced a truncate) the locked path
//! (`read_direct`) does. A user fault ends the read at the first user page that faults, and the bounce path
//! (`sys::read_into`) finishes it, as Linux's `filemap_read` ends at a short `copy_folio_to_iter`
//! (linux-6.6/mm/filemap.c:2703-2709) and returns the bytes so far.

use crate::abi::MAX_RW_COUNT;
use crate::frame::PAGE;
use crate::cache_view::Stop;
use crate::pagecache::{CachePage, PageCache};
use crate::sys::{read_into, zeroed, Sys, UserMem, CHUNK};
use crate::Errno;

klog::counter!(FAULT_ENDS, "vfs.read.cached_fault_ends");

/// A hole's bytes (tmpfs).
pub(crate) static ZEROS: [u8; PAGE] = [0; PAGE];

/// Copies of at most this many bytes use the small stack buffer (a 16-byte pread zeroes no page of stack).
const SMALL: usize = 256;

/// `data` to user memory at `addr`: all of it, or on a fault exactly the user pages before the one that
/// faulted (each tried on its own; nothing after the first that fails).
pub(crate) fn write_counted<M: UserMem + ?Sized>(m: &M, addr: u64, data: &[u8]) -> usize {
    if data.is_empty() || m.write(addr, data) {
        return data.len();
    }
    let mut done = 0usize;
    while done < data.len() {
        let Some(at) = addr.checked_add(done as u64) else { break };
        let k = (PAGE - (at % PAGE as u64) as usize).min(data.len() - done);
        if !m.write(at, &data[done..done + k]) {
            break;
        }
        done += k;
    }
    done
}

/// `UserMem::write_page`'s default: the page's bytes through a stack buffer.
pub(crate) fn through_stack<M: UserMem + ?Sized>(m: &M, addr: u64, page: &CachePage, off: usize, n: usize) -> usize {
    let n = n.min(PAGE.saturating_sub(off));
    if n <= SMALL {
        let mut b = [0u8; SMALL];
        page.read(off, &mut b[..n]);
        write_counted(m, addr, &b[..n])
    } else {
        let mut b = [0u8; PAGE];
        page.read(off, &mut b[..n]);
        write_counted(m, addr, &b[..n])
    }
}

/// read/pread of `count` bytes at `pos` of the file whose content is `c` into user memory at `buf` (the
/// caller checked the fd, `access_ok` and the position). The bytes read, or the error when none were.
pub(crate) fn read_cached(s: &Sys<'_>, c: &PageCache, buf: u64, count: u64, pos: u64) -> Result<i64, Errno> {
    let count = count.min(MAX_RW_COUNT) as usize;
    if count == 0 {
        return Ok(0);
    }
    let (done, fault) = if s.mem.direct_page_copy() {
        direct(s, c, buf, count, pos)?
    } else if count <= SMALL {
        gathered(s, c, buf, count, pos, &mut [0u8; SMALL][..count])?
    } else if count <= PAGE {
        gathered(s, c, buf, count, pos, &mut [0u8; PAGE][..count])?
    } else {
        gathered(s, c, buf, count, pos, &mut zeroed(count.min(CHUNK))?)?
    };
    if !fault || done == count {
        return Ok(done as i64);
    }
    // A user fault: the bounce path takes the rest (it answers at once when the same page faults again). A
    // program's bad buffer, so a counter and no event (B6: no flood from a program).
    FAULT_ENDS.inc();
    let (at, from) = (buf + done as u64, pos + done as u64);
    match read_into(s, at, (count - done) as u64, |off, chunk| c.read_at(from + off as u64, chunk)) {
        Ok(n) => Ok(done as i64 + n),
        Err(e) if done == 0 => Err(e),
        Err(_) => Ok(done as i64),
    }
}

/// Each page straight to the program: lockless as far as the cache and the no-fault copy reach (PF-5), the
/// rest on the locked path. (bytes that reached it, whether a fault ended the read)
fn direct(s: &Sys<'_>, c: &PageCache, buf: u64, count: usize, pos: u64) -> Result<(usize, bool), Errno> {
    let (n, stop) = if s.mem.nofault_page_copy() {
        c.read_quick(pos, count, &mut |page, off, k, done| buf.checked_add(done as u64).map_or(0, |at| s.mem.write_page_nofault(at, page, off, k)))
    } else {
        (0, Stop::Retry)
    };
    if n == count || stop == Stop::Eof {
        return Ok((n, false));
    }
    let r = c.read_direct(pos + n as u64, count - n, &mut |page, off, k, done| {
        let Some(at) = buf.checked_add((n + done) as u64) else { return 0 };
        match page {
            Some(p) => s.mem.write_page(at, p, off, k),
            None => s.mem.write_zeros(at, k),
        }
    });
    match r {
        Ok((m, fault)) => Ok((n + m, fault)),
        Err(_) if n > 0 => Ok((n, false)),
        Err(e) => Err(e),
    }
}

/// The pages through `b` (as long as the read or `CHUNK`), one user copy per filled buffer: each buffer is
/// filled lockless as far as the cache holds the pages (PF-5, `read_quick`), the rest of it on the locked path
/// (`read_direct`, which also reads missing pages from the backend).
fn gathered(s: &Sys<'_>, c: &PageCache, buf: u64, count: usize, pos: u64, b: &mut [u8]) -> Result<(usize, bool), Errno> {
    let copy = |b: &mut [u8], page: Option<&CachePage>, off: usize, k: usize, at: usize| {
        let out = &mut b[at..at + k];
        match page {
            Some(p) => p.read(off, out),
            None => out.fill(0),
        }
        k
    };
    let mut flushed = 0usize;
    while flushed < count {
        let want = (count - flushed).min(b.len());
        let here = pos + flushed as u64;
        let (n, stop) = c.read_quick(here, want, &mut |page, off, k, d| copy(b, page, off, k, d));
        let mut filled = n;
        let mut end = stop == Stop::Eof;
        if filled < want && !end {
            match c.read_direct(here + n as u64, want - n, &mut |page, off, k, d| copy(b, page, off, k, n + d)) {
                Ok((m, _)) => {
                    filled += m;
                    end = filled < want;
                }
                Err(e) if flushed + filled == 0 => return Err(e),
                Err(_) => end = true,
            }
        }
        let got = buf.checked_add(flushed as u64).map_or(0, |at| write_counted(s.mem, at, &b[..filled]));
        flushed += got;
        if got < filled {
            return Ok((flushed, true));
        }
        if end || filled < want {
            break;
        }
    }
    Ok((flushed, false))
}
