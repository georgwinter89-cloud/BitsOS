//! Opening a pool: pick the newest valid superblock, walk every tree once, rebuild the reference counts and
//! the free lists. Every pointer is checked (range, page kind, level, checksum, a chunk is either data or
//! metadata); a tree that fails makes the open fall back to the other superblock.

use crate::error::{Result, VolError};
use crate::layout::{self, DEPTH_BOUND, INNER_FANOUT, LEAF_FANOUT, PAGE, Record, Super, Tree, child_base};
use crate::pool::{Config, MetaChunk, Pool, Vol, zeroed};
use crate::tree::check;
use alloc::sync::Arc;
use alloc::vec::Vec;
use devmodel::block::{BlockDevice, IoPriority};

impl Pool {
    /// Opens the pool on `dev` (NotFound: no pool there).
    pub fn open(dev: Arc<dyn BlockDevice>, cfg: Config) -> Result<Pool> {
        let info = dev.info();
        let bs = info.block_size as usize;
        if bs == 0 || bs > PAGE || PAGE % bs != 0 {
            return Err(VolError::InvalidArgument);
        }
        let mut slots = zeroed(2 * PAGE)?;
        let max = (info.max_blocks_per_request.max(1) as usize) * bs;
        let mut done = 0;
        while done < slots.len() {
            let n = (slots.len() - done).min(max);
            dev.read_blocks((done / bs) as u64, &mut slots[done..done + n], IoPriority::Normal)?;
            done += n;
        }
        let mut cands: Vec<Super> = [0usize, 1].iter().filter_map(|&s| Super::decode(&slots[s * PAGE..(s + 1) * PAGE])).collect();
        if cands.len() == 1 {
            let other = (cands[0].generation as usize + 1) % 2;
            if slots[other * PAGE..(other + 1) * PAGE].iter().any(|&b| b != 0) {
                note!(cfg.quiet, "superblock.torn", slot = other, used = cands[0].generation);
            }
        }
        cands.sort_by(|a, b| b.generation.cmp(&a.generation));
        if cands.is_empty() {
            return Err(VolError::NotFound);
        }
        let mut last = VolError::NotFound;
        for (i, sb) in cands.iter().enumerate() {
            match Pool::build(dev.clone(), cfg, sb) {
                Ok(p) => {
                    if i > 0 {
                        klog::event!(Warn, "bitsvol", "recovery.older_superblock", generation = sb.generation, newest = cands[0].generation);
                    }
                    note!(cfg.quiet, "pool.open", generation = sb.generation, volumes = p.vols.len(), free = p.free, chunks = p.chunks);
                    return Ok(p);
                }
                Err(VolError::Corrupt(what)) => {
                    klog::event!(Warn, "bitsvol", "superblock.unusable", generation = sb.generation, reason = what);
                    last = VolError::Corrupt(what);
                }
                Err(e) => return Err(e),
            }
        }
        Err(last)
    }

    fn build(dev: Arc<dyn BlockDevice>, cfg: Config, sb: &Super) -> Result<Pool> {
        if sb.block_size != dev.info().block_size {
            return Err(VolError::Corrupt("block size differs from the device"));
        }
        // Chunks beyond the pool's own count (the device grew) stay unused until a grow step exists.
        let mut p = Pool::empty(dev, cfg, sb.chunk_size, Some(sb.chunks)).map_err(|e| match e {
            VolError::NoMemory => e,
            _ => VolError::Corrupt("superblock geometry"),
        })?;
        if sb.table_depth > DEPTH_BOUND || (sb.table_root != 0 && sb.table_depth == 0) {
            return Err(VolError::Corrupt("superblock fields"));
        }
        (p.committed_gen, p.time, p.next_id) = (sb.generation, sb.time, sb.next_id);
        (p.table_root, p.table_depth) = (sb.table_root, sb.table_depth);
        let mut recs: Vec<(u64, Record)> = Vec::new();
        if sb.table_root != 0 {
            p.scan_page(sb.table_root, sb.table_depth - 1, 0, Tree::Table, &mut recs)?;
        }
        for (rp, rec) in recs {
            if rec.id >= p.next_id || rec.depth > DEPTH_BOUND || (rec.root != 0 && rec.depth == 0) || rec.snap_time > p.time {
                return Err(VolError::Corrupt("record fields"));
            }
            if rec.size == 0 || rec.size % p.bs != 0 {
                return Err(VolError::Corrupt("record size"));
            }
            if rec.root != 0 {
                p.scan_page(rec.root, rec.depth - 1, 0, Tree::Map, &mut Vec::new())?;
            }
            p.vols.insert(rec.id, Vol { rec, rec_page: rp, dirty: false });
        }
        for (&c, m) in p.meta.iter() {
            for s in 0..p.ppc {
                if m.refs[s as usize] == 0 {
                    p.page_free.insert(c * p.ppc + s);
                }
            }
        }
        let used = (p.first..p.chunks).filter(|&c| p.crefs.get(c) != 0).count() as u64;
        p.free = p.chunks - p.first - used;
        Ok(p)
    }

    /// Counts one reference to page `pg`; true on its first one (then the caller walks it).
    fn scan_ref(&mut self, pg: u64) -> Result<bool> {
        let c = pg / self.ppc;
        if c < self.first || c >= self.chunks {
            return Err(VolError::Corrupt("page number out of range"));
        }
        if !self.meta.contains_key(&c) {
            if self.crefs.get(c) != 0 {
                return Err(VolError::Corrupt("chunk is data and metadata"));
            }
            self.crefs.inc(c)?;
            let mut refs = Vec::new();
            refs.try_reserve_exact(self.ppc as usize).map_err(|_| VolError::NoMemory)?;
            refs.resize(self.ppc as usize, 0);
            self.meta.insert(c, MetaChunk { refs, free: self.ppc });
        }
        let m = self.meta.get_mut(&c).ok_or(VolError::Corrupt("metadata chunk"))?;
        let r = &mut m.refs[(pg % self.ppc) as usize];
        *r = r.checked_add(1).ok_or(VolError::Corrupt("page refcount"))?;
        if *r == 1 {
            m.free -= 1;
            return Ok(true);
        }
        Ok(false)
    }

    fn scan_page(&mut self, pg: u64, level: u32, base: u64, tree: Tree, recs: &mut Vec<(u64, Record)>) -> Result<()> {
        if !self.scan_ref(pg)? {
            return Ok(());
        }
        let n = if level > 0 { INNER_FANOUT } else { LEAF_FANOUT };
        let mut ents: Vec<(u64, u64)> = Vec::new();
        ents.try_reserve_exact(n as usize).map_err(|_| VolError::NoMemory)?;
        {
            let b = self.page(pg)?;
            check(b, level, tree)?;
            for i in 0..n {
                ents.push(if level > 0 { (layout::inner_get(b, i), 0) } else { layout::leaf_get(b, i) });
            }
        }
        for (i, (v, t)) in ents.into_iter().enumerate() {
            if v == 0 {
                continue;
            }
            let key = child_base(base, level, i as u64);
            match (level, tree) {
                (0, Tree::Map) => {
                    if v < self.first || v >= self.chunks || self.meta.contains_key(&v) || t > self.time {
                        return Err(VolError::Corrupt("mapping entry"));
                    }
                    self.crefs.inc(v)?;
                }
                (0, Tree::Table) => {
                    if !self.scan_ref(v)? {
                        return Err(VolError::Corrupt("record page shared"));
                    }
                    let rec = Record::decode(self.page(v)?)?;
                    if rec.id != key {
                        return Err(VolError::Corrupt("record under the wrong id"));
                    }
                    recs.try_reserve(1).map_err(|_| VolError::NoMemory)?;
                    recs.push((v, rec));
                }
                _ => self.scan_page(v, level - 1, key, tree, recs)?,
            }
        }
        Ok(())
    }
}
