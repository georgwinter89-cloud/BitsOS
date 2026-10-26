//! The pool: device access, metadata page cache, allocation of chunks and pages, commit.
//!
//! Crash consistency: a committed page is never written again. Every page changed in a transaction is a fresh
//! copy (shadow), every chunk or page freed in it stays unusable until the commit is durable. Commit order:
//! dirty pages, flush (data written before is durable with them), superblock of the next generation into the
//! other slot with FUA (durable when written: `BlockDevice::write_blocks`; VS-2 flushed once more after it,
//! VS-2b dropped that barrier). A crash at any point leaves one valid superblock whose whole tree is intact. A
//! flush with nothing written since the last one is not sent to the device (VS-2b: each costs a SYNCHRONIZE
//! CACHE on the stick).
//!
//! Reference counts are not stored: `open` rebuilds them in one walk over all trees (lazy sharing as in Linux
//! dm-thin: a page's count is the number of parent pointers to it, a chunk's the number of leaves naming it).

use crate::error::{Result, VolError};
use crate::layout::{self, PAGE, Record, Super, Tree};
use crate::refs::RefCounts;
use alloc::collections::{BTreeMap, BTreeSet};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use devmodel::block::{BlockDevice, IoPriority};

#[derive(Clone, Copy, Debug)]
pub struct Config {
    /// Bytes per chunk, a multiple of 4096 and of the block size (used by `format`; `open` takes the pool's).
    pub chunk_size: u32,
    /// Chunks data writes leave for metadata (None: max(8, chunks / 256)).
    pub reserve_chunks: Option<u64>,
    /// Clean metadata pages kept in memory (a memory budget, not a limit on the pool).
    pub cache_pages: usize,
    /// The pool commits by itself once this many pages are dirty (bounds the memory of one transaction).
    pub commit_dirty_pages: usize,
    /// Routine events at Debug instead of Info.
    pub quiet: bool,
}

impl Default for Config {
    fn default() -> Self {
        Config { chunk_size: 64 * 1024, reserve_chunks: None, cache_pages: 16384, commit_dirty_pages: 4096, quiet: false }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stats {
    pub chunk_size: u32,
    pub chunks: u64,
    /// Chunks free now (freed chunks count after the next commit).
    pub free: u64,
    pub pending: u64,
    pub meta_chunks: u64,
    pub reserve: u64,
    pub generation: u64,
    pub volumes: u64,
    pub snapshots: u64,
    /// Automatic snapshots dropped to make room since the pool was opened.
    pub dropped: u64,
    /// Deletes whose tree could not be walked to the end (memory or a metadata read failed): the space it
    /// still holds comes back at the next open, which rebuilds every reference count.
    pub leaks: u64,
}

pub(crate) struct CPage {
    pub data: Vec<u8>,
    pub dirty: bool,
}

pub(crate) struct MetaChunk {
    pub refs: Vec<u32>,
    /// Pages of this chunk in `page_free`.
    pub free: u64,
}

pub(crate) struct Vol {
    pub rec: Record,
    /// Page holding the record (0: not written yet).
    pub rec_page: u64,
    pub dirty: bool,
}

pub struct Pool {
    pub(crate) dev: Arc<dyn BlockDevice>,
    pub(crate) bs: u64,
    pub(crate) cs: u64,
    pub(crate) chunks: u64,
    pub(crate) ppc: u64,
    /// Chunks holding the superblock slots (pages 0 and 1): one, or two for 4 KiB chunks.
    pub(crate) first: u64,
    dev_max: u64,
    discard_ok: bool,
    pub(crate) cfg: Config,
    pub(crate) reserve: u64,
    pub(crate) committed_gen: u64,
    pub(crate) time: u64,
    pub(crate) next_id: u64,
    pub(crate) table_root: u64,
    pub(crate) table_depth: u32,
    pub(crate) table_dirty: bool,
    pub(crate) vols: BTreeMap<u64, Vol>,
    pub(crate) crefs: RefCounts,
    pub(crate) meta: BTreeMap<u64, MetaChunk>,
    pub(crate) page_free: BTreeSet<u64>,
    pend_chunks: Vec<u64>,
    pend_set: BTreeSet<u64>,
    pend_pages: Vec<u64>,
    pub(crate) free: u64,
    cursor: u64,
    pub(crate) cache: BTreeMap<u64, CPage>,
    pub(crate) dirty_pages: usize,
    /// Allocation may use the whole reserve (deleting, which frees space after its commit).
    pub(crate) urgent: bool,
    pub(crate) busy: BTreeMap<u64, u64>,
    pub(crate) dropped: u64,
    pub(crate) leaks: u64,
    io: Barriers,
    /// The volume of an open copy window (`begin_copy_window`).
    pub(crate) window: Option<u64>,
}

/// The device barriers this pool asked for (cache flushes and FUA writes: on a USB stick without DPOFUA and on an
/// image file each FUA write is a write plus a flush), for the snapshot pause's breakdown (VS-2b).
struct Barriers {
    count: AtomicU64,
    ns: AtomicU64,
    /// Something was written (not FUA) since the last flush; true at open (writes before it are unknown).
    unflushed: AtomicBool,
}

impl Default for Barriers {
    fn default() -> Self {
        Barriers { count: AtomicU64::new(0), ns: AtomicU64::new(0), unflushed: AtomicBool::new(true) }
    }
}

impl Barriers {
    fn add(&self, t0: Option<u64>) {
        self.count.fetch_add(1, Ordering::Relaxed);
        if let (Some(a), Some(b)) = (t0, ksync::now_ns()) {
            self.ns.fetch_add(b.saturating_sub(a), Ordering::Relaxed);
        }
    }
}

pub(crate) fn zeroed(n: usize) -> Result<Vec<u8>> {
    let mut v = Vec::new();
    v.try_reserve_exact(n).map_err(|_| VolError::NoMemory)?;
    v.resize(n, 0);
    Ok(v)
}

impl Pool {
    /// `chunks`: the pool's own chunk count (from its superblock), at most what the device holds.
    pub(crate) fn empty(dev: Arc<dyn BlockDevice>, cfg: Config, chunk_size: u32, chunks: Option<u64>) -> Result<Pool> {
        let info = dev.info();
        let bs = info.block_size as u64;
        let cs = chunk_size as u64;
        if bs == 0 || bs > PAGE as u64 || PAGE as u64 % bs != 0 || cs == 0 || cs % PAGE as u64 != 0 || cs % bs != 0 {
            return Err(VolError::InvalidArgument);
        }
        let fits = info.blocks.checked_mul(bs).ok_or(VolError::InvalidArgument)? / cs;
        let chunks = chunks.unwrap_or(fits);
        if chunks > fits {
            return Err(VolError::InvalidArgument);
        }
        let reserve = cfg.reserve_chunks.unwrap_or((chunks / 256).max(8));
        if chunks < reserve + 4 {
            return Err(VolError::InvalidArgument);
        }
        let first = (2 * PAGE as u64).div_ceil(cs);
        let mut crefs = RefCounts::new(chunks)?;
        for c in 0..first {
            crefs.inc(c)?;
        }
        Ok(Pool {
            dev,
            bs,
            cs,
            chunks,
            ppc: cs / PAGE as u64,
            first,
            dev_max: (info.max_blocks_per_request.max(1) as u64) * bs,
            discard_ok: info.supports_discard,
            cfg,
            reserve,
            committed_gen: 0,
            time: 0,
            next_id: 1,
            table_root: 0,
            table_depth: 0,
            table_dirty: false,
            vols: BTreeMap::new(),
            crefs,
            meta: BTreeMap::new(),
            page_free: BTreeSet::new(),
            pend_chunks: Vec::new(),
            pend_set: BTreeSet::new(),
            pend_pages: Vec::new(),
            free: chunks - first,
            cursor: first,
            cache: BTreeMap::new(),
            dirty_pages: 0,
            urgent: false,
            busy: BTreeMap::new(),
            dropped: 0,
            leaks: 0,
            io: Barriers::default(),
            window: None,
        })
    }

    /// Makes an empty pool on `dev`. Both superblock slots are cleared first, so an older pool on the same
    /// device can never win over the new one.
    pub fn format(dev: Arc<dyn BlockDevice>, cfg: Config) -> Result<Pool> {
        let mut p = Pool::empty(dev, cfg, cfg.chunk_size, None)?;
        let zero = zeroed(2 * PAGE)?;
        p.io_write(0, &zero, false, IoPriority::Normal)?;
        p.dev.flush()?;
        p.table_dirty = true;
        p.commit()?;
        note!(p.cfg.quiet, "pool.format", chunks = p.chunks, chunk_size = p.cs, reserve = p.reserve);
        Ok(p)
    }

    pub fn device(&self) -> &Arc<dyn BlockDevice> {
        &self.dev
    }
    /// Device barriers since open: (how many, ns spent in them).
    pub fn barriers(&self) -> (u64, u64) {
        (self.io.count.load(Ordering::Relaxed), self.io.ns.load(Ordering::Relaxed))
    }
    /// The device's cache flush, counted; none when nothing was written since the last one.
    pub(crate) fn barrier(&self) -> Result<()> {
        if !self.io.unflushed.swap(false, Ordering::AcqRel) {
            return Ok(());
        }
        let t0 = ksync::now_ns();
        let r = self.dev.flush();
        self.io.add(t0);
        if r.is_err() {
            self.io.unflushed.store(true, Ordering::Release);
        }
        Ok(r?)
    }
    pub fn generation(&self) -> u64 {
        self.committed_gen
    }
    pub fn block_size(&self) -> u32 {
        self.bs as u32
    }
    pub fn chunk_size(&self) -> u32 {
        self.cs as u32
    }

    pub fn stats(&self) -> Stats {
        let snaps = self.vols.values().filter(|v| v.rec.is_snapshot()).count() as u64;
        Stats {
            chunk_size: self.cs as u32,
            chunks: self.chunks,
            free: self.free,
            pending: self.pend_chunks.len() as u64,
            meta_chunks: self.meta.len() as u64,
            reserve: self.reserve,
            generation: self.committed_gen,
            volumes: self.vols.len() as u64 - snaps,
            snapshots: snaps,
            dropped: self.dropped,
            leaks: self.leaks,
        }
    }

    /// Digest of the allocation state (reference counts, free lists): a pool reopened from its committed
    /// image must give the same digest as the live one (the tests' consistency check).
    pub fn alloc_digest(&self) -> u64 {
        let mut h = 0xcbf2_9ce4_8422_2325u64;
        let mut mix = |v: u64| {
            h ^= v;
            h = h.wrapping_mul(0x100_0000_01b3);
        };
        for c in 0..self.chunks {
            mix(self.crefs.get(c));
        }
        for (&c, m) in &self.meta {
            mix(c);
            m.refs.iter().for_each(|&r| mix(r as u64));
        }
        self.page_free.iter().for_each(|&p| mix(p));
        mix(self.free);
        h
    }

    /// What differs in the allocation state of two pools (for test failures).
    pub fn alloc_diff(&self, o: &Pool) -> alloc::string::String {
        use core::fmt::Write;
        let mut out = alloc::string::String::new();
        for c in 0..self.chunks.max(o.chunks) {
            if self.crefs.get(c) != o.crefs.get(c) || self.meta.contains_key(&c) != o.meta.contains_key(&c) {
                let _ = write!(out, "chunk {c}: {} meta={} vs {} meta={}; ", self.crefs.get(c), self.meta.contains_key(&c), o.crefs.get(c), o.meta.contains_key(&c));
            }
        }
        for (c, m) in &self.meta {
            if let Some(n) = o.meta.get(c)
                && (n.refs != m.refs || n.free != m.free)
            {
                let _ = write!(out, "meta {c}: {:?}/{} vs {:?}/{}; ", m.refs, m.free, n.refs, n.free);
            }
        }
        let _ = write!(out, "free {} vs {}; page_free {} vs {}", self.free, o.free, self.page_free.len(), o.page_free.len());
        out
    }

    // ---- device access (requests split to the device's limit) ----

    pub(crate) fn io_read(&self, off: u64, buf: &mut [u8], prio: IoPriority) -> Result<()> {
        let mut done = 0usize;
        while done < buf.len() {
            let n = (buf.len() - done).min(self.dev_max as usize);
            self.dev.read_blocks((off + done as u64) / self.bs, &mut buf[done..done + n], prio)?;
            done += n;
        }
        Ok(())
    }

    pub(crate) fn io_write(&self, off: u64, buf: &[u8], fua: bool, prio: IoPriority) -> Result<()> {
        let t0 = if fua { ksync::now_ns() } else { None };
        if !fua {
            self.io.unflushed.store(true, Ordering::Release);
        }
        let mut done = 0usize;
        while done < buf.len() {
            let n = (buf.len() - done).min(self.dev_max as usize);
            self.dev.write_blocks((off + done as u64) / self.bs, &buf[done..done + n], fua, prio)?;
            done += n;
        }
        if fua {
            self.io.add(t0);
        }
        Ok(())
    }

    // ---- metadata pages ----

    fn meta_of(&self, p: u64) -> Result<(u64, usize)> {
        let c = p / self.ppc;
        if c < self.first || c >= self.chunks || !self.meta.contains_key(&c) {
            return Err(VolError::Corrupt("page outside the metadata chunks"));
        }
        Ok((c, (p % self.ppc) as usize))
    }

    pub(crate) fn load(&mut self, p: u64) -> Result<()> {
        if self.cache.contains_key(&p) {
            return Ok(());
        }
        self.meta_of(p)?;
        let mut data = zeroed(PAGE)?;
        self.io_read(p * PAGE as u64, &mut data, IoPriority::Normal)?;
        layout::verify(&data, p)?;
        self.evict();
        self.cache.insert(p, CPage { data, dirty: false });
        Ok(())
    }

    /// Drops clean pages down to three quarters of the budget; dirty pages stay until the commit.
    fn evict(&mut self) {
        if self.cache.len() < self.cfg.cache_pages + self.dirty_pages {
            return;
        }
        let target = self.cfg.cache_pages * 3 / 4 + self.dirty_pages;
        let excess = self.cache.len().saturating_sub(target);
        let victims: Vec<u64> = self.cache.iter().filter(|(_, c)| !c.dirty).map(|(&k, _)| k).take(excess).collect();
        for k in victims {
            self.cache.remove(&k);
        }
    }

    pub(crate) fn page(&mut self, p: u64) -> Result<&[u8]> {
        self.load(p)?;
        self.cache.get(&p).map(|c| c.data.as_slice()).ok_or(VolError::Corrupt("page cache"))
    }

    /// A page of the current transaction (never a committed one).
    pub(crate) fn page_mut(&mut self, p: u64) -> Result<&mut [u8]> {
        match self.cache.get_mut(&p) {
            Some(c) if c.dirty => Ok(c.data.as_mut_slice()),
            _ => Err(VolError::Corrupt("write to a committed page")),
        }
    }

    pub(crate) fn is_dirty(&self, p: u64) -> bool {
        self.cache.get(&p).is_some_and(|c| c.dirty)
    }

    /// A fresh page of the current transaction with count 1, zeroed and typed.
    pub(crate) fn alloc_page(&mut self, kind: u8, level: u8, tree: Tree) -> Result<u64> {
        if self.page_free.is_empty() {
            let keep = if self.urgent { 0 } else { self.reserve / 2 };
            let c = self.alloc_chunk(keep)?;
            let mut refs = Vec::new();
            refs.try_reserve_exact(self.ppc as usize).map_err(|_| VolError::NoMemory)?;
            refs.resize(self.ppc as usize, 0);
            self.meta.insert(c, MetaChunk { refs, free: self.ppc });
            for s in 0..self.ppc {
                self.page_free.insert(c * self.ppc + s);
            }
        }
        let mut data = zeroed(PAGE)?;
        let p = self.page_free.pop_first().ok_or(VolError::Corrupt("page free list"))?;
        let (c, s) = self.meta_of(p)?;
        let m = self.meta.get_mut(&c).ok_or(VolError::Corrupt("metadata chunk"))?;
        m.refs[s] = 1;
        m.free -= 1;
        layout::init_page(&mut data, kind, level, tree);
        self.cache.insert(p, CPage { data, dirty: true });
        self.dirty_pages += 1;
        Ok(p)
    }

    pub(crate) fn page_rc(&self, p: u64) -> Result<u32> {
        let (c, s) = self.meta_of(p)?;
        Ok(self.meta[&c].refs[s])
    }

    pub(crate) fn page_inc(&mut self, p: u64) -> Result<()> {
        let (c, s) = self.meta_of(p)?;
        let r = &mut self.meta.get_mut(&c).ok_or(VolError::Corrupt("metadata chunk"))?.refs[s];
        if *r == 0 {
            return Err(VolError::Corrupt("reference to a free page"));
        }
        *r = r.checked_add(1).ok_or(VolError::Corrupt("page refcount"))?;
        Ok(())
    }

    /// Drops one reference; a page left without any is freed after the next commit.
    pub(crate) fn page_dec(&mut self, p: u64) -> Result<u32> {
        let (c, s) = self.meta_of(p)?;
        let r = &mut self.meta.get_mut(&c).ok_or(VolError::Corrupt("metadata chunk"))?.refs[s];
        if *r == 0 {
            return Err(VolError::Corrupt("page refcount below zero"));
        }
        *r -= 1;
        let n = *r;
        if n == 0 {
            match self.cache.remove(&p) {
                // Allocated in this transaction: no committed tree names it, so it is free at once.
                Some(cp) if cp.dirty => {
                    self.dirty_pages -= 1;
                    self.free_page(p);
                }
                _ => self.pend_pages.push(p),
            }
        }
        Ok(n)
    }

    // ---- chunks ----

    /// A free chunk with count 1, leaving `keep` chunks free.
    pub(crate) fn alloc_chunk(&mut self, keep: u64) -> Result<u64> {
        if self.free <= keep {
            return Err(VolError::NoSpace);
        }
        let mut at = self.cursor.max(self.first);
        let mut wrapped = false;
        loop {
            match self.crefs.next_zero(at) {
                Some(c) if self.pend_set.contains(&c) => at = c + 1,
                Some(c) => {
                    self.crefs.inc(c)?;
                    self.free -= 1;
                    self.cursor = c + 1;
                    return Ok(c);
                }
                None if !wrapped => {
                    wrapped = true;
                    at = self.first;
                }
                None => return Err(VolError::Corrupt("free chunk count")),
            }
        }
    }

    pub(crate) fn chunk_inc(&mut self, c: u64) -> Result<()> {
        if c < self.first || self.meta.contains_key(&c) || self.crefs.get(c) == 0 {
            return Err(VolError::Corrupt("reference to a chunk that is not data"));
        }
        self.crefs.inc(c)
    }

    pub(crate) fn chunk_dec(&mut self, c: u64) -> Result<()> {
        if c < self.first || self.meta.contains_key(&c) {
            return Err(VolError::Corrupt("data reference to a metadata chunk"));
        }
        if self.crefs.dec(c)? == 0 {
            self.pend_chunks.push(c);
            self.pend_set.insert(c);
        }
        Ok(())
    }

    /// Gives back a chunk allocated in this transaction that nothing maps (a failed write): free at once.
    pub(crate) fn unalloc_chunk(&mut self, c: u64) -> Result<()> {
        if self.crefs.dec(c)? != 0 {
            return Err(VolError::Corrupt("unmapped chunk still referenced"));
        }
        self.free += 1;
        Ok(())
    }

    pub(crate) fn has_pending(&self) -> bool {
        !self.pend_chunks.is_empty() || !self.pend_pages.is_empty()
    }

    // ---- commit ----

    fn meta_dirty(&self) -> bool {
        self.dirty_pages > 0 || self.table_dirty || self.has_pending() || self.vols.values().any(|v| v.dirty)
    }

    /// Makes everything written so far durable (the stacked device's flush).
    pub fn commit(&mut self) -> Result<()> {
        if !self.meta_dirty() {
            return self.barrier();
        }
        let generation = self.committed_gen + 1;
        // The pages below are written before the barrier asks whether anything was.
        self.io.unflushed.store(true, Ordering::Release);
        for v in self.vols.values_mut().filter(|v| v.dirty) {
            match self.cache.get_mut(&v.rec_page) {
                Some(c) if c.dirty => v.rec.encode(&mut c.data),
                _ => return Err(VolError::Corrupt("record page not prepared")),
            }
        }
        let mut pages = 0u64;
        let mut run: Vec<u8> = Vec::new();
        let mut run_start = 0u64;
        let mut next = 0u64;
        for (&p, c) in self.cache.iter_mut().filter(|(_, c)| c.dirty) {
            layout::seal(&mut c.data, p, generation);
            if !run.is_empty() && (p != next || run.len() as u64 >= self.dev_max) {
                Self::write_run(&self.dev, self.bs, self.dev_max, run_start, &run)?;
                run.clear();
            }
            if run.is_empty() {
                run_start = p;
            }
            run.try_reserve(PAGE).map_err(|_| VolError::NoMemory)?;
            run.extend_from_slice(&c.data);
            next = p + 1;
            pages += 1;
        }
        if !run.is_empty() {
            Self::write_run(&self.dev, self.bs, self.dev_max, run_start, &run)?;
        }
        self.barrier()?;
        let sb = Super {
            block_size: self.bs as u32,
            chunk_size: self.cs as u32,
            chunks: self.chunks,
            generation,
            table_root: self.table_root,
            table_depth: self.table_depth,
            next_id: self.next_id,
            time: self.time,
        };
        self.io_write((generation % 2) * PAGE as u64, &sb.encode(), true, IoPriority::Normal)?;
        for c in self.cache.values_mut() {
            c.dirty = false;
        }
        for v in self.vols.values_mut() {
            v.dirty = false;
        }
        self.dirty_pages = 0;
        self.table_dirty = false;
        self.committed_gen = generation;
        self.window_step();
        let freed = self.release();
        klog::event!(Debug, "bitsvol", "commit", generation = generation, pages = pages, freed = freed, free = self.free);
        Ok(())
    }

    fn write_run(dev: &Arc<dyn BlockDevice>, bs: u64, max: u64, page: u64, run: &[u8]) -> Result<()> {
        let off = page * PAGE as u64;
        let mut done = 0usize;
        while done < run.len() {
            let n = (run.len() - done).min(max as usize);
            dev.write_blocks((off + done as u64) / bs, &run[done..done + n], false, IoPriority::Normal)?;
            done += n;
        }
        Ok(())
    }

    /// Puts a page no tree names any more on the free list; a metadata chunk left without pages in use goes
    /// back to the pool at once (returned).
    fn free_page(&mut self, p: u64) -> Option<u64> {
        let c = p / self.ppc;
        self.page_free.insert(p);
        let m = self.meta.get_mut(&c)?;
        m.free += 1;
        if m.free != self.ppc {
            return None;
        }
        self.meta.remove(&c);
        for s in 0..self.ppc {
            self.page_free.remove(&(c * self.ppc + s));
        }
        self.crefs.dec(c).ok()?;
        self.free += 1;
        Some(c)
    }

    /// After a durable commit: freed chunks and pages become usable, empty metadata chunks return to the pool
    /// and freed chunks are discarded on the device below.
    fn release(&mut self) -> u64 {
        let mut gone = core::mem::take(&mut self.pend_chunks);
        for &c in &gone {
            self.pend_set.remove(&c);
        }
        self.free += gone.len() as u64;
        for p in core::mem::take(&mut self.pend_pages) {
            if let Some(c) = self.free_page(p) {
                gone.push(c);
            }
        }
        let n = gone.len() as u64;
        if self.discard_ok && !gone.is_empty() {
            gone.sort_unstable();
            let per = self.cs / self.bs;
            let mut i = 0;
            while i < gone.len() {
                let mut j = i + 1;
                while j < gone.len() && gone[j] == gone[j - 1] + 1 {
                    j += 1;
                }
                if let Err(e) = self.dev.discard(gone[i] * per, (j - i) as u64 * per) {
                    klog::event!(Debug, "bitsvol", "discard.failed", chunk = gone[i], error = alloc::format!("{e:?}"));
                }
                i = j;
            }
        }
        n
    }
}
