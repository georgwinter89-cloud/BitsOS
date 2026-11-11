//! BL-3: a regular file's pages filled straight from the device, without the disk's own cache in between (Linux
//! 7.3.1 fs/ext4/readpage.c:137 `ext4_mpage_readpages`, :342 `ext4_readahead`; mm/readahead.c:160 `read_pages`).
//!
//! The extents of the window are mapped under the volume lock (`Volume::file_plan`, FS-STALL: no device wait
//! there); with the lock released, each contiguous extent piece becomes one read into the pages themselves
//! (`Dev::read_pages`: on a disk the request's frames are the cache pages, BL-2). Physically adjacent pieces go
//! as one read, as `ext4_mpage_readpages` adds a folio to the open bio while `last_block_in_bio` continues.
//! Holes and unwritten extents are not read: the pages come zeroed (`PageCache::alloc_run`), as
//! `folio_zero_segment` zeroes them in Linux. A piece that does not start on a block or would end past the
//! window (block size above the page size) is read through a buffer and copied, as Linux's "confused" path falls
//! back to buffer heads. Inline-data files keep the old path (Linux: `ext4_has_inline_data`, readpage.c:157).

use crate::dev::PageRead;
use crate::errno::*;
use crate::vol::Volume;
use alloc::sync::Arc;
use alloc::vec::Vec;
use block::PageSeg;
use vfs::pagecache::CachePage;

const PAGE: usize = 4096;
static ZEROS: [u8; PAGE] = [0; PAGE];

/// Appends the pages of window bytes [pos, pos + len) to `segs` (a piece continuing in the page of the one before
/// widens its segment: one segment per page and run).
fn add_segs(segs: &mut Vec<PageSeg>, pages: &[Arc<CachePage>], mut pos: usize, mut len: usize) -> Result<(), i64> {
    while len > 0 {
        let (i, off) = (pos / PAGE, pos % PAGE);
        let n = (PAGE - off).min(len);
        let page = pages.get(i).ok_or(EIO)?;
        match segs.last_mut() {
            Some(s) if Arc::ptr_eq(&s.page, page) && s.off + s.len == off => s.len += n,
            _ => {
                segs.try_reserve(1).map_err(|_| ENOMEM)?;
                segs.push(PageSeg { page: page.clone(), off, len: n });
            }
        }
        pos += n;
        len -= n;
    }
    Ok(())
}

impl Volume {
    /// BL-3: fills `pages` (zeroed, consecutive, from file byte `off`, page aligned) of regular file `ino` from the
    /// device. Ok(false): inline data or not a regular file, nothing read (the caller takes `read`). Bytes past the
    /// file's end stay zero. A block outside the volume is EUCLEAN (a damaged extent never reaches the device).
    pub fn read_pages(&self, ino: u32, off: u64, pages: &[Arc<CachePage>]) -> Result<bool, i64> {
        let window = pages.len().checked_mul(PAGE).ok_or(EINVAL)?;
        let Some(plan) = self.file_plan(ino, off, window)? else { return Ok(false) };
        let (bs, blocks) = (self.geo.bs, self.geo.blocks);
        let mut reads: Vec<PageRead> = Vec::new();
        // The open read's next disk block and window byte: a piece starting there joins it.
        let mut open: Option<(u64, usize)> = None;
        let mut pos = 0usize;
        for (len, at) in plan.pieces() {
            let here = pos;
            pos += len;
            let Some((p, in_blk)) = at else { continue };
            let nblk = ((in_blk + len) as u64).div_ceil(bs);
            if p.checked_add(nblk).is_none_or(|e| e > blocks) {
                return Err(EUCLEAN);
            }
            let bytes = (nblk * bs) as usize;
            if in_blk != 0 || here + bytes > window {
                let mut segs = Vec::new();
                add_segs(&mut segs, pages, here, len)?;
                let r = PageRead { at: p * bs, len: bytes, segs };
                bounce_part(self, &r, in_blk, len)?;
                continue;
            }
            match (open, reads.last_mut()) {
                (Some((next_blk, next_pos)), Some(r)) if next_blk == p && next_pos == here => {
                    add_segs(&mut r.segs, pages, here, bytes)?;
                    r.len += bytes;
                }
                _ => {
                    let mut segs = Vec::new();
                    add_segs(&mut segs, pages, here, bytes)?;
                    reads.try_reserve(1).map_err(|_| ENOMEM)?;
                    reads.push(PageRead { at: p * bs, len: bytes, segs });
                }
            }
            // A piece cut short (the file's last block) ends the read: nothing joins after its rounded end.
            open = (bytes == len).then_some((p + nblk, here + bytes));
        }
        if !reads.is_empty() {
            self.dev.read_pages(reads)?;
        }
        // The last block's bytes past the end of the file are not file data (Linux: `zero_user` of the tail).
        let total = plan.total();
        if total < window && total % PAGE != 0 {
            pages[total / PAGE].write(total % PAGE, &ZEROS[total % PAGE..]);
        }
        Ok(true)
    }
}

/// A piece not on a block boundary: whole blocks through a buffer (`r.len`), its `len` bytes from `skip` into the
/// pages of `r.segs`.
fn bounce_part(vol: &Volume, r: &PageRead, skip: usize, len: usize) -> Result<(), i64> {
    let mut tmp = Vec::new();
    tmp.try_reserve_exact(r.len).map_err(|_| ENOMEM)?;
    tmp.resize(r.len, 0);
    vol.dev.read(r.at, &mut tmp)?;
    let mut done = skip;
    for s in &r.segs {
        s.page.write(s.off, &tmp[done..done + s.len]);
        done += s.len;
    }
    crate::file::COPY_EXT4_PAGE.add(len as u64);
    Ok(())
}
