//! File content: reading clusters, writing page-cache runs (allocating and zeroing new clusters), changing the
//! size, and the commit that writes the directory entry. FAT32 files end at 4 GiB - 1 (Linux `s_maxbytes`).
//!
//! Order of one write-back run: FSInfo "unknown" (once), zeros into newly allocated clusters outside the run and
//! into the stale bytes past the old end, the run's data, the new chain entries (FAT 1, FAT 2), the link from
//! the old last cluster (FAT 1, FAT 2); the directory entry (size, first cluster, time) at `commit`, FSInfo last.

use crate::dirent::ATTR_ARCHIVE;
use crate::fat::EOC;
use crate::vol::{State, Volume};
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::Ordering;
use uapi::errno::*;

/// Lock-free reads of a file before `read_data` reads under the volume lock.
const READ_TRIES: usize = 3;

/// The largest request the FAT code sends (bytes).
const CHUNK: usize = 1 << 20;

/// The largest FAT file size.
pub const MAX_SIZE: u64 = 0xFFFF_FFFF;

/// A piece of a byte range of a file on the disk: (first sector, offset in the range, bytes).
type Extent = (u64, usize, usize);

impl Volume {
    /// The disk pieces of file bytes [off, off + len) (sector aligned) over `chain`; bytes past the chain are
    /// left out.
    fn extents(&self, chain: &[u32], off: u64, len: usize) -> Vec<Extent> {
        let cb = self.geo.cluster_bytes() as u64;
        let bps = self.geo.bps as u64;
        let mut out: Vec<Extent> = Vec::new();
        let mut at = off;
        let end = off + len as u64;
        while at < end {
            let idx = (at / cb) as usize;
            let Some(&c) = chain.get(idx) else { break };
            let in_c = at % cb;
            let n = (cb - in_c).min(end - at);
            let sector = self.geo.cluster_sector(c) + in_c / bps;
            let pos = (at - off) as usize;
            match out.last_mut() {
                Some((s, p, l)) if *s + (*l as u64).div_ceil(bps) == sector && *p + *l == pos && (*l as u64).is_multiple_of(bps) => *l += n as usize,
                _ => out.push((sector, pos, n as usize)),
            }
            at += n;
        }
        out
    }

    fn write_extents(&self, ext: &[Extent], data: &[u8]) -> Result<(), i64> {
        let bps = self.geo.bps;
        for &(sector, pos, len) in ext {
            let whole = len / bps * bps;
            // At most 1 MiB per request: each one completes well inside the block layer's deadline (B4).
            let mut done = 0;
            while done < whole {
                let n = (whole - done).min(CHUNK);
                self.write(sector + (done / bps) as u64, &data[pos + done..pos + done + n])?;
                done += n;
            }
            if len > whole {
                // A last partial sector (data shorter than a sector): pad with zeros.
                let mut b = vec![0u8; bps];
                b[..len - whole].copy_from_slice(&data[pos + whole..pos + len]);
                self.write(sector + (whole / bps) as u64, &b)?;
            }
        }
        Ok(())
    }

    /// Reads file bytes at `off` (a multiple of the sector size) into `buf`; bytes at or past the file's size
    /// (or past its chain) read as zeros.
    ///
    /// The clusters are looked up under the volume lock and read without it (a slow stick blocks no writer).
    /// A truncate of this file meanwhile could free them and another file take them over, so the file's
    /// change count is compared after the read: a change reads again, and after a few changes in a row the
    /// read holds the lock (never another file's bytes, never a busy loop).
    pub fn read_data(&self, ino: u64, off: u64, buf: &mut [u8]) -> Result<(), i64> {
        for _ in 0..READ_TRIES {
            let (plan, dgen, seen) = {
                let mut st = self.lock()?;
                let dgen = Self::node(&st, ino)?.dgen.clone();
                let seen = dgen.load(Ordering::Acquire);
                (self.read_plan(&mut st, ino, off, buf.len())?, dgen, seen)
            };
            self.read_extents(&plan, off, buf)?;
            if dgen.load(Ordering::Acquire) == seen {
                return Ok(());
            }
            klog::event!(Debug, "fat", "read.retry", ino = ino, off = off; "the file changed while it was read: read again");
        }
        let mut st = self.lock()?;
        let plan = self.read_plan(&mut st, ino, off, buf.len())?;
        self.read_extents(&plan, off, buf)
    }

    /// The extents of a read of `len` bytes at `off`, and the file's size (None extents: nothing to read).
    fn read_plan(&self, st: &mut State, ino: u64, off: u64, len: usize) -> Result<(Vec<Extent>, u64), i64> {
        let size = Self::node(st, ino)?.e.size as u64;
        if off >= size {
            return Ok((Vec::new(), size));
        }
        let want = (len as u64).min(size - off) as usize;
        let rounded = want.div_ceil(self.geo.bps) * self.geo.bps;
        let chain = self.chain(st, ino)?;
        Ok((self.extents(chain, off, rounded.min(len)), size))
    }

    fn read_extents(&self, plan: &(Vec<Extent>, u64), off: u64, buf: &mut [u8]) -> Result<(), i64> {
        let (ext, size) = (&plan.0, plan.1);
        buf.fill(0);
        let mut tmp = Vec::new();
        for &(sector, pos, len) in ext {
            let n = len.div_ceil(self.geo.bps) * self.geo.bps;
            if pos + n <= buf.len() {
                self.dev.read(sector, &mut buf[pos..pos + n])?;
            } else {
                tmp.resize(n, 0);
                self.dev.read(sector, &mut tmp)?;
                let k = buf.len() - pos;
                buf[pos..].copy_from_slice(&tmp[..k]);
            }
        }
        // Bytes past the end on the disk may hold anything (a cluster's tail): never shown.
        if off < size && off + buf.len() as u64 > size {
            buf[(size - off) as usize..].fill(0);
        }
        Ok(())
    }

    fn bump(st: &State, ino: u64) -> Result<(), i64> {
        Self::node(st, ino)?.dgen.fetch_add(1, Ordering::AcqRel);
        Ok(())
    }

    /// Zeros file bytes [from, to) on the disk (inside the chain); a partial first sector keeps its bytes before
    /// `from`.
    fn zero_range(&self, st: &mut State, ino: u64, from: u64, to: u64) -> Result<(), i64> {
        if from >= to {
            return Ok(());
        }
        let bps = self.geo.bps as u64;
        let start = from / bps * bps;
        let chain = self.chain(st, ino)?.clone();
        let ext = self.extents(&chain, start, (to - start) as usize);
        let mut data = vec![0u8; (to - start) as usize];
        if start < from
            && let Some(&(s, _, _)) = ext.first()
        {
            let mut head = vec![0u8; bps as usize];
            self.dev.read(s, &mut head)?;
            let keep = (from - start) as usize;
            data[..keep].copy_from_slice(&head[..keep]);
        }
        self.write_extents(&ext, &data)
    }

    fn stamp_write(&self, st: &mut State, ino: u64) -> Result<(), i64> {
        let t = self.clock.stamp_now();
        let n = Self::node_mut(st, ino)?;
        n.e.wdate = t.date;
        n.e.wtime = t.time;
        n.e.adate = t.date;
        n.e.attr |= ATTR_ARCHIVE;
        n.entry_dirty = true;
        Ok(())
    }

    /// One page-cache run: `data` at file offset `off` (sector aligned) of a file now `size` bytes long.
    pub fn write_data(&self, ino: u64, off: u64, data: &[u8], size: u64) -> Result<(), i64> {
        if size > MAX_SIZE {
            return Err(EFBIG);
        }
        let mut st = self.lock()?;
        let class = Self::node(&st, ino)?.class;
        // The content was admitted by `write_at` (SYS: under the grant); this runs on the flusher's thread too.
        self.rules().writeback(class).map_err(|e| self.refused("write", ino, e))?;
        self.reap(&mut st)?;
        if off >= size {
            return Ok(());
        }
        let len = (data.len() as u64).min(size - off) as usize;
        let old = Self::node(&st, ino)?.e.size as u64;
        Self::bump(&st, ino)?;
        self.begin(&mut st)?;
        // Bytes between the old end and this run were never written: zeros (inside the old chain).
        let cb = self.geo.cluster_bytes() as u64;
        let had = self.chain(&mut st, ino)?.len() as u64 * cb;
        if off > old {
            self.zero_range(&mut st, ino, old, off.min(had))?;
        }
        let have = self.chain(&mut st, ino)?.len();
        let need = size.div_ceil(cb) as usize;
        let keep = (off, off + len as u64);
        let new_from = have as u64 * cb;
        // Data of the run that lands in clusters not allocated yet goes after `grow`; zeros first, then data.
        if need > have {
            self.grow_with_data(&mut st, ino, size, keep, off, &data[..len], new_from)?;
        } else {
            let chain = self.chain(&mut st, ino)?.clone();
            let ext = self.extents(&chain, off, len);
            self.write_extents(&ext, &data[..len])?;
        }
        if size != old {
            Self::node_mut(&mut st, ino)?.e.size = size as u32;
        }
        self.stamp_write(&mut st, ino)
    }

    /// Allocates clusters so the chain of `ino` covers `size` bytes. Order: the run's bytes inside the old chain,
    /// then each new cluster written (the run's bytes where [keep) covers it, zeros elsewhere), then the new FAT
    /// entries, then the link from the old last cluster. The entry's first cluster changes in memory only.
    #[allow(clippy::too_many_arguments)]
    fn grow_with_data(&self, st: &mut State, ino: u64, size: u64, keep: (u64, u64), off: u64, data: &[u8], new_from: u64) -> Result<(), i64> {
        // The part of the run inside the old chain goes first (overwrites in place).
        if off < new_from {
            let n = ((new_from - off) as usize).min(data.len());
            let chain = self.chain(st, ino)?.clone();
            let ext = self.extents(&chain, off, n);
            self.write_extents(&ext, &data[..n])?;
        }
        // New clusters: found, zeroed outside the run and filled with the run's bytes, then linked.
        let cb = self.geo.cluster_bytes() as u64;
        let have = self.chain(st, ino)?.len();
        let need = size.div_ceil(cb) as usize;
        self.begin(st)?;
        let new = self.allocate(st, Some(ino), need - have)?;
        let mut all = self.chain(st, ino)?.clone();
        all.extend_from_slice(&new);
        // The new clusters' bytes (the run's where it covers them, zeros elsewhere), written in pieces of at
        // most 1 MiB along contiguous clusters.
        let base = have as u64 * cb;
        let per_piece = (CHUNK as u64 / cb).max(1) as usize;
        let mut k = 0;
        while k < new.len() {
            let mut n = 1;
            while k + n < new.len() && n < per_piece && new[k + n] == new[k + n - 1] + 1 {
                n += 1;
            }
            let (a, b) = (base + k as u64 * cb, base + (k + n) as u64 * cb);
            let mut buf = vec![0u8; (b - a) as usize];
            let (ka, kb) = (keep.0.max(a), keep.1.min(b));
            if ka < kb {
                buf[(ka - a) as usize..(kb - a) as usize].copy_from_slice(&data[(ka - off) as usize..(kb - off) as usize]);
            }
            self.write(self.geo.cluster_sector(new[k]), &buf)?;
            k += n;
        }
        st.fat.link(&*self.dev, &new)?;
        st.fat.write_out(&*self.dev)?;
        let n = Self::node_mut(st, ino)?;
        match n.chain.as_ref().and_then(|c| c.last().copied()) {
            Some(last) => {
                st.fat.set(&*self.dev, last, new[0])?;
                st.fat.write_out(&*self.dev)?;
            }
            None => {
                n.e.first = new[0];
                n.entry_dirty = true;
            }
        }
        Self::node_mut(st, ino)?.chain = Some(all);
        Ok(())
    }

    /// truncate/ftruncate: shrinking writes the entry first, then cuts the chain, then frees the rest; growing
    /// allocates zeroed clusters (and zeros the old last cluster's stale tail) before the entry grows.
    pub fn set_size(&self, ino: u64, size: u64) -> Result<(), i64> {
        if size > MAX_SIZE {
            return Err(EFBIG);
        }
        let mut st = self.lock()?;
        let class = Self::node(&st, ino)?.class;
        self.rules().write(class).map_err(|e| self.refused("truncate", ino, e))?;
        self.reap(&mut st)?;
        let old = Self::node(&st, ino)?.e.size as u64;
        if Self::node(&st, ino)?.is_dir() {
            return Err(EISDIR);
        }
        Self::bump(&st, ino)?;
        self.begin(&mut st)?;
        let cb = self.geo.cluster_bytes() as u64;
        let chain = self.chain(&mut st, ino)?.clone();
        let need = size.div_ceil(cb) as usize;
        if size < old || need < chain.len() {
            {
                let n = Self::node_mut(&mut st, ino)?;
                n.e.size = size as u32;
                if need == 0 {
                    n.e.first = 0;
                }
            }
            self.stamp_write(&mut st, ino)?;
            self.write_entry(&mut st, ino)?;
            if need < chain.len() {
                if need > 0 {
                    st.fat.set(&*self.dev, chain[need - 1], EOC)?;
                    st.fat.write_out(&*self.dev)?;
                }
                st.fat.release(&*self.dev, &chain[need..])?;
                st.fat.write_out(&*self.dev)?;
                Self::node_mut(&mut st, ino)?.chain = Some(chain[..need].to_vec());
            }
        } else if size > old {
            self.zero_range(&mut st, ino, old, size.min(chain.len() as u64 * cb))?;
            let from = chain.len() as u64 * cb;
            self.grow_with_data(&mut st, ino, size, (0, 0), from, &[], from)?;
            Self::node_mut(&mut st, ino)?.e.size = size as u32;
            self.stamp_write(&mut st, ino)?;
            self.write_entry(&mut st, ino)?;
        }
        // The cache is `size` long now: promises past it go back.
        self.unreserve_past(&mut st, ino, size)?;
        self.finish(&mut st)
    }

    /// utimensat: the entry's modification time and access date (FAT keeps no access time of day), UTC
    /// seconds; one sector. The same rules as a content change (`\BITSOS2\` files only on the stick).
    pub fn set_times(&self, ino: u64, mtime: Option<i64>, atime: Option<i64>) -> Result<(), i64> {
        self.set_entry(ino, None, mtime, atime)
    }

    /// FS-1b chmod (`fat_setattr` after `fat_sanitize_mode`, linux-6.6/fs/fat/file.c:421): the read-only
    /// attribute follows the owner's w bit (`fat_make_attrs`). Unchanged: nothing written.
    pub fn set_read_only_attr(&self, ino: u64, ro: bool) -> Result<(), i64> {
        self.set_entry(ino, Some(ro), None, None)
    }

    /// FS-1b setattr in one step: the read-only attribute (None: kept) and the times, checked by the rules
    /// once and written as one sector, so a refusal leaves the entry as it was (no half-done change).
    pub fn set_entry(&self, ino: u64, ro: Option<bool>, mtime: Option<i64>, atime: Option<i64>) -> Result<(), i64> {
        let mut st = self.lock()?;
        let n = Self::node(&st, ino)?;
        let ro = ro.filter(|&r| (n.e.attr & crate::dirent::ATTR_RO != 0) != r);
        let times = mtime.is_some() || atime.is_some();
        if ro.is_none() && !times {
            return Ok(());
        }
        let class = n.class;
        self.rules().write(class).map_err(|e| self.refused(if times { "times" } else { "attr" }, ino, e))?;
        self.reap(&mut st)?;
        let local = |t: i64| crate::dirent::stamp(t + (self.clock.offset)(t) as i64 * 60);
        {
            let n = Self::node_mut(&mut st, ino)?;
            match ro {
                Some(true) => n.e.attr |= crate::dirent::ATTR_RO,
                Some(false) => n.e.attr &= !crate::dirent::ATTR_RO,
                None => {}
            }
            if let Some(t) = mtime {
                let s = local(t);
                n.e.wdate = s.date;
                n.e.wtime = s.time;
            }
            if let Some(t) = atime {
                n.e.adate = local(t).date;
            }
            // The root and a removed file have no entry: the times live in memory only (as Linux's root).
            n.entry_dirty |= n.linked;
        }
        self.begin(&mut st)?;
        self.write_entry(&mut st, ino)?;
        self.finish(&mut st)
    }

    /// The time FAT keeps for `t` (UTC seconds): the modification time to 2 seconds, the access time to the
    /// start of its local day (`fat_truncate_mtime`/`fat_truncate_atime`, linux-6.6/fs/fat/misc.c:287-308).
    pub fn kept_time(&self, t: i64, atime: bool) -> i64 {
        if atime {
            let off = (self.clock.offset)(t) as i64 * 60;
            t - (t + off).rem_euclid(86_400)
        } else {
            t - t.rem_euclid(2)
        }
    }

    /// After a write-back: the directory entry if it changed, then FSInfo.
    pub fn commit(&self, ino: u64) -> Result<(), i64> {
        let mut st = self.lock()?;
        if Self::node(&st, ino)?.entry_dirty {
            self.write_entry(&mut st, ino)?;
        }
        self.reap(&mut st)?;
        self.finish(&mut st)
    }

    /// The whole file as it is on the disk (not through the page cache: a read-back after a write checks what
    /// the stick holds), at most `max` bytes (EFBIG beyond).
    pub fn read_disk(&self, ino: u64, max: usize) -> Result<Vec<u8>, i64> {
        let size = self.disk_size(ino)? as usize;
        if size > max {
            return Err(EFBIG);
        }
        let mut out = Vec::new();
        out.try_reserve_exact(size).map_err(|_| ENOMEM)?;
        out.resize(size, 0);
        let mut at = 0;
        while at < size {
            let n = (size - at).min(CHUNK);
            self.read_data(ino, at as u64, &mut out[at..at + n])?;
            at += n;
        }
        Ok(out)
    }

    /// Up to `buf.len()` bytes of the file at `off` (below the end: a multiple of the sector size) from the disk,
    /// not the page cache; the count read, 0 at or past the end. A read-back piece by piece, for files too big
    /// for a second whole copy in memory.
    pub fn read_disk_at(&self, ino: u64, off: u64, buf: &mut [u8]) -> Result<usize, i64> {
        let n = self.disk_size(ino)?.saturating_sub(off).min(buf.len() as u64) as usize;
        if n > 0 && off % self.geo.bps as u64 != 0 {
            return Err(EINVAL);
        }
        let mut at = 0;
        while at < n {
            let k = (n - at).min(CHUNK);
            self.read_data(ino, off + at as u64, &mut buf[at..at + k])?;
            at += k;
        }
        Ok(n)
    }

    /// 512-byte blocks the file's chain holds.
    pub fn blocks(&self, ino: u64) -> u64 {
        let Ok(mut st) = self.lock() else { return 0 };
        let cb = self.geo.cluster_bytes() as u64;
        self.chain(&mut st, ino).map_or(0, |c| c.len() as u64 * cb / 512)
    }
}
