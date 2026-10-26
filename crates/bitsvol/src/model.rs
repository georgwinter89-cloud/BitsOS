//! The reference model: every volume as a plain map block -> stamp, and a runner that drives a pool on a
//! `MemDev` through random steps (writes, reads, snapshots, deletes, rollbacks, discards, flushes, reopens)
//! and checks it against the model after every step. With `crash` on, the device records a crash image at
//! every write; each is opened as a pool and must show either the exact state of the newest commit or the
//! last exact checkpoint plus any of the blocks written since (what a block device promises after power loss).
//! Shared by the host tests and the ktests.

use crate::error::VolError;
use crate::memdev::{MemDev, xorshift};
use crate::pool::{Config, Pool};
use alloc::collections::{BTreeMap, BTreeSet};
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use devmodel::block::IoPriority::Normal;

#[derive(Clone, Copy, Debug)]
pub struct RunCfg {
    pub seed: u64,
    pub steps: u64,
    pub block_size: u32,
    pub chunk_size: u32,
    pub dev_chunks: u64,
    pub max_req: u32,
    /// Size of a new volume in chunks.
    pub vol_chunks: u64,
    pub crash: bool,
    pub cache_pages: usize,
    pub commit_dirty_pages: usize,
}

impl RunCfg {
    pub fn small(seed: u64, steps: u64) -> RunCfg {
        RunCfg {
            seed,
            steps,
            block_size: 512,
            chunk_size: 4096,
            dev_chunks: 160,
            max_req: 8,
            vol_chunks: 16,
            crash: false,
            cache_pages: 64,
            commit_dirty_pages: 16,
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Report {
    pub steps: u64,
    pub writes: u64,
    pub reads: u64,
    pub snapshots: u64,
    pub deletes: u64,
    pub rollbacks: u64,
    pub discards: u64,
    pub flushes: u64,
    pub reopens: u64,
    pub nospace: u64,
    pub dropped: u64,
    pub crash_images: u64,
    pub generations: u64,
}

#[derive(Clone, Debug, PartialEq)]
struct MVol {
    blocks: u64,
    snapshot: bool,
    auto: bool,
    data: BTreeMap<u64, u64>,
}

type MState = BTreeMap<u64, MVol>;

/// Block content for a stamp: the stamp, then a pattern derived from it (0 = a zero block).
pub fn fill(buf: &mut [u8], stamp: u64) {
    if stamp == 0 {
        buf.fill(0);
        return;
    }
    buf[..8].copy_from_slice(&stamp.to_le_bytes());
    for (i, b) in buf[8..].iter_mut().enumerate() {
        *b = (stamp.wrapping_mul(31) as u8).wrapping_add(i as u8) | 1;
    }
}

/// The stamp a block holds, None for anything no write produced.
pub fn stamp_of(buf: &[u8]) -> Option<u64> {
    if buf.iter().all(|&b| b == 0) {
        return Some(0);
    }
    let s = u64::from_le_bytes(buf[..8].try_into().ok()?);
    let ok = buf[8..].iter().enumerate().all(|(i, &b)| b == (s.wrapping_mul(31) as u8).wrapping_add(i as u8) | 1);
    if ok && s != 0 { Some(s) } else { None }
}

struct Expect {
    generation: u64,
    state: MState,
}

pub struct Runner {
    cfg: RunCfg,
    pcfg: Config,
    rng: u64,
    dev: Arc<MemDev>,
    pool: Option<Pool>,
    model: MState,
    stamp: u64,
    now: u64,
    ck: Expect,
    since: BTreeSet<(u64, u64, u64)>,
    pub report: Report,
    last: u64,
}

fn e<T>(r: Result<T, VolError>, what: &str) -> Result<T, String> {
    r.map_err(|e| format!("{what}: {e:?}"))
}

impl Runner {
    pub fn new(cfg: RunCfg) -> Result<Runner, String> {
        let blocks = cfg.dev_chunks * (cfg.chunk_size / cfg.block_size) as u64;
        let dev = Arc::new(MemDev::new("model", cfg.block_size, blocks, cfg.max_req).ok_or("no memory for the device")?);
        let pcfg = Config { chunk_size: cfg.chunk_size, reserve_chunks: None, cache_pages: cfg.cache_pages, commit_dirty_pages: cfg.commit_dirty_pages, quiet: true };
        let pool = e(Pool::format(dev.clone(), pcfg), "format")?;
        let generation = pool.generation();
        let mut r = Runner {
            cfg,
            pcfg,
            rng: cfg.seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1,
            dev,
            pool: Some(pool),
            model: MState::new(),
            stamp: 0,
            now: 1,
            ck: Expect { generation, state: MState::new() },
            since: BTreeSet::new(),
            report: Report::default(),
            last: 0,
        };
        r.create()?;
        r.create()?;
        r.checkpoint();
        if cfg.crash {
            r.dev.set_capture(true, cfg.seed);
        }
        Ok(r)
    }

    fn pool(&mut self) -> &mut Pool {
        self.pool.as_mut().expect("pool present between steps")
    }

    fn rnd(&mut self, n: u64) -> u64 {
        if n == 0 { 0 } else { xorshift(&mut self.rng) % n }
    }

    fn bpc(&self) -> u64 {
        (self.cfg.chunk_size / self.cfg.block_size) as u64
    }

    fn pick(&mut self, f: impl Fn(&MVol) -> bool) -> Option<u64> {
        let ids: Vec<u64> = self.model.iter().filter(|(_, v)| f(v)).map(|(&k, _)| k).collect();
        if ids.is_empty() {
            return None;
        }
        let i = self.rnd(ids.len() as u64) as usize;
        Some(ids[i])
    }

    fn checkpoint(&mut self) {
        let generation = self.pool.as_ref().map_or(0, |p| p.generation());
        self.ck = Expect { generation, state: self.model.clone() };
        self.since.clear();
    }

    fn create(&mut self) -> Result<bool, String> {
        let chunks = self.cfg.vol_chunks.max(1);
        let blocks = chunks * self.bpc() - self.rnd(self.bpc());
        self.now += 1;
        let now = self.now;
        let bs = self.cfg.block_size as u64;
        match self.pool().create_volume("vol", blocks * bs, now) {
            Ok(id) => {
                self.model.insert(id, MVol { blocks, snapshot: false, auto: false, data: BTreeMap::new() });
                Ok(true)
            }
            Err(VolError::NoSpace) => {
                self.report.nospace += 1;
                if self.pool.is_some() {
                    self.sync_drops()?;
                }
                Ok(false)
            }
            Err(x) => Err(format!("create: {x:?}")),
        }
    }

    /// Drops model volumes the pool dropped for space (only automatic snapshots may go).
    fn sync_drops(&mut self) -> Result<(), String> {
        let have: BTreeSet<u64> = e(self.pool().list(), "list")?.iter().map(|v| v.id).collect();
        let gone: Vec<u64> = self.model.keys().filter(|k| !have.contains(k)).copied().collect();
        for id in gone {
            if !self.model[&id].auto {
                return Err(format!("volume {id} vanished but was not an automatic snapshot"));
            }
            self.model.remove(&id);
            self.report.dropped += 1;
        }
        for id in have {
            if !self.model.contains_key(&id) {
                return Err(format!("pool has volume {id} the model does not know"));
            }
        }
        Ok(())
    }

    fn write(&mut self) -> Result<bool, String> {
        let Some(id) = self.pick(|v| !v.snapshot) else { return Ok(false) };
        let vb = self.model[&id].blocks;
        let len = 1 + self.rnd(self.bpc() * 2 + 3).min(vb - 1);
        let lba = self.rnd(vb - len + 1);
        let fua = self.rnd(10) == 0;
        let bs = self.cfg.block_size as usize;
        let mut buf = alloc::vec![0u8; len as usize * bs];
        let mut stamps = Vec::new();
        for i in 0..len {
            self.stamp += 1;
            fill(&mut buf[i as usize * bs..(i as usize + 1) * bs], self.stamp);
            stamps.push(self.stamp);
            self.since.insert((id, lba + i, self.stamp));
        }
        self.report.writes += 1;
        let mut done = fua;
        match self.pool().write(id, lba, &buf, fua, Normal) {
            Ok(()) => {
                let v = self.model.get_mut(&id).ok_or("model volume")?;
                for (i, s) in stamps.iter().enumerate() {
                    v.data.insert(lba + i as u64, *s);
                }
            }
            Err(VolError::NoSpace) => {
                self.report.nospace += 1;
                done = false;
                let got = self.read_blocks(id, lba, len)?;
                let v = self.model.get_mut(&id).ok_or("model volume")?;
                for (i, g) in got.into_iter().enumerate() {
                    let b = lba + i as u64;
                    let old = v.data.get(&b).copied().unwrap_or(0);
                    if g != old && g != stamps[i] {
                        return Err(format!("after ENOSPC block {b} of {id} holds {g}, neither {old} nor {}", stamps[i]));
                    }
                    v.data.insert(b, g);
                }
            }
            Err(x) => return Err(format!("write {id}@{lba}+{len}: {x:?}")),
        }
        self.sync_drops()?;
        self.check_range(id, lba, len)?;
        Ok(done)
    }

    fn read_blocks(&mut self, id: u64, lba: u64, len: u64) -> Result<Vec<u64>, String> {
        let bs = self.cfg.block_size as usize;
        let mut buf = alloc::vec![0u8; len as usize * bs];
        e(self.pool().read(id, lba, &mut buf, Normal), "read")?;
        buf.chunks(bs).enumerate().map(|(i, b)| stamp_of(b).ok_or(format!("block {} of {id} holds garbage", lba + i as u64))).collect()
    }

    fn check_range(&mut self, id: u64, lba: u64, len: u64) -> Result<(), String> {
        let got = self.read_blocks(id, lba, len)?;
        let v = &self.model[&id];
        for (i, g) in got.iter().enumerate() {
            let b = lba + i as u64;
            let want = v.data.get(&b).copied().unwrap_or(0);
            if *g != want {
                return Err(format!("volume {id} block {b}: pool {g}, model {want}"));
            }
        }
        Ok(())
    }

    fn check_all(&mut self) -> Result<(), String> {
        let list = e(self.pool().list(), "list")?;
        if list.len() != self.model.len() {
            return Err(format!("pool has {} volumes, model {}", list.len(), self.model.len()));
        }
        let bs = self.cfg.block_size as u64;
        for v in list {
            let m = self.model.get(&v.id).ok_or(format!("unknown volume {}", v.id))?;
            if v.size != m.blocks * bs || v.snapshot != m.snapshot || v.auto != m.auto {
                return Err(format!("volume {} differs: {v:?}", v.id));
            }
            let n = m.blocks;
            self.check_range(v.id, 0, n)?;
        }
        Ok(())
    }

    fn discard(&mut self) -> Result<(), String> {
        let Some(id) = self.pick(|v| !v.snapshot) else { return Ok(()) };
        let vb = self.model[&id].blocks;
        let len = 1 + self.rnd(self.bpc() * 4).min(vb - 1);
        let lba = self.rnd(vb - len + 1);
        let bpc = self.bpc();
        let (first, last) = (lba.div_ceil(bpc), (lba + len) / bpc);
        for c in first..last {
            for b in c * bpc..(c + 1) * bpc {
                self.since.insert((id, b, 0));
            }
        }
        match self.pool().discard(id, lba, len) {
            Ok(()) => {
                let v = self.model.get_mut(&id).ok_or("model volume")?;
                for b in first * bpc..last * bpc {
                    v.data.remove(&b);
                }
            }
            Err(VolError::NoSpace) if last > first => {
                // Unmapping copies tree pages; on a full pool part of the range may stay.
                self.report.nospace += 1;
                let got = self.read_blocks(id, first * bpc, (last - first) * bpc)?;
                let v = self.model.get_mut(&id).ok_or("model volume")?;
                for (i, g) in got.into_iter().enumerate() {
                    let b = first * bpc + i as u64;
                    if g != 0 && v.data.get(&b).copied().unwrap_or(0) != g {
                        return Err(format!("after ENOSPC in discard block {b} of {id} holds {g}"));
                    }
                    if g == 0 {
                        v.data.remove(&b);
                    }
                }
                self.sync_drops()?;
            }
            Err(x) => return Err(format!("discard: {x:?}")),
        }
        self.report.discards += 1;
        Ok(())
    }

    fn snapshot(&mut self) -> Result<bool, String> {
        let Some(src) = self.pick(|v| !v.snapshot) else { return Ok(false) };
        let auto = self.rnd(2) == 0;
        self.now += 1;
        let now = self.now;
        match self.pool().snapshot(src, "snap", now, auto) {
            Ok(id) => {
                let mut v = self.model[&src].clone();
                (v.snapshot, v.auto) = (true, auto);
                self.model.insert(id, v);
                self.report.snapshots += 1;
                self.sync_drops()?;
                Ok(true)
            }
            Err(VolError::NoSpace) => {
                self.report.nospace += 1;
                self.sync_drops()?;
                Ok(false)
            }
            Err(x) => Err(format!("snapshot: {x:?}")),
        }
    }

    fn delete(&mut self) -> Result<bool, String> {
        let Some(id) = self.pick(|_| true) else { return Ok(false) };
        e(self.pool().delete(id), "delete")?;
        self.model.remove(&id);
        self.report.deletes += 1;
        Ok(true)
    }

    fn rollback(&mut self) -> Result<bool, String> {
        let (Some(id), Some(snap)) = (self.pick(|v| !v.snapshot), self.pick(|v| v.snapshot)) else { return Ok(false) };
        self.now += 1;
        let now = self.now;
        match self.pool().rollback(id, snap, "before rollback", now) {
            Ok(kept) => {
                let mut old = self.model[&id].clone();
                (old.snapshot, old.auto) = (true, false);
                self.model.insert(kept, old);
                let s = self.model[&snap].clone();
                let v = self.model.get_mut(&id).ok_or("model volume")?;
                (v.blocks, v.data) = (s.blocks, s.data);
                self.report.rollbacks += 1;
                self.sync_drops()?;
                Ok(true)
            }
            Err(VolError::NoSpace) => {
                self.report.nospace += 1;
                self.sync_drops()?;
                Ok(false)
            }
            Err(x) => Err(format!("rollback: {x:?}")),
        }
    }

    /// Commit, then open the pool again from the device and from a copy of its image.
    fn reopen(&mut self) -> Result<(), String> {
        e(self.pool().commit(), "commit")?;
        let digest = self.pool().alloc_digest();
        let old = self.pool.take().ok_or("no pool")?;
        let p = e(Pool::open(self.dev.clone(), self.pcfg), "reopen")?;
        if p.alloc_digest() != digest {
            return Err(format!("reference counts rebuilt on open differ from the live ones: {}", old.alloc_diff(&p)));
        }
        drop(old);
        self.pool = Some(p);
        self.check_all()?;
        let copy = Arc::new(MemDev::from_image("copy", self.cfg.block_size, self.cfg.max_req, self.dev.image()).ok_or("image copy")?);
        let mut p2 = e(Pool::open(copy, self.pcfg), "open image copy")?;
        if p2.alloc_digest() != digest || p2.generation() != self.pool().generation() {
            return Err(String::from("image copy opens differently"));
        }
        let ids: Vec<u64> = self.model.keys().copied().collect();
        let bs = self.cfg.block_size as usize;
        for id in ids {
            let n = self.model[&id].blocks as usize;
            let (mut a, mut b) = (alloc::vec![0u8; n * bs], alloc::vec![0u8; n * bs]);
            e(self.pool().read(id, 0, &mut a, Normal), "read")?;
            e(p2.read(id, 0, &mut b, Normal), "read copy")?;
            if a != b {
                return Err(format!("volume {id} differs in the image copy"));
            }
        }
        self.report.reopens += 1;
        Ok(())
    }

    /// One random step; true when it ended with a commit that matches the model exactly.
    fn step(&mut self) -> Result<bool, String> {
        let r = self.rnd(100);
        self.last = r;
        let exact = match r {
            0..=44 => self.write()?,
            45..=54 => {
                let Some(id) = self.pick(|_| true) else { return Ok(false) };
                let vb = self.model[&id].blocks;
                let len = 1 + self.rnd(self.bpc() * 3).min(vb - 1);
                let lba = self.rnd(vb - len + 1);
                self.check_range(id, lba, len)?;
                self.report.reads += 1;
                false
            }
            55..=62 => {
                self.discard()?;
                false
            }
            // Keep the number of volumes bounded so the pool is not full all the time.
            63..=72 if self.model.len() > 7 => self.delete()?,
            63..=72 => self.snapshot()?,
            73..=77 => self.delete()?,
            78..=81 => self.rollback()?,
            82..=91 => {
                e(self.pool().commit(), "flush")?;
                self.report.flushes += 1;
                true
            }
            92..=94 => {
                self.reopen()?;
                true
            }
            _ => {
                if self.model.values().filter(|v| !v.snapshot).count() < 3 {
                    self.create()?
                } else {
                    false
                }
            }
        };
        Ok(exact)
    }

    /// Opens every crash image recorded during the last step and checks it.
    fn check_images(&mut self, exact: Option<&Expect>) -> Result<(), String> {
        for img in self.dev.take_images() {
            self.report.crash_images += 1;
            let dev = Arc::new(MemDev::from_image("crash", self.cfg.block_size, self.cfg.max_req, img).ok_or("crash image")?);
            let mut p = e(Pool::open(dev, self.pcfg), "open crash image")?;
            let g = p.generation();
            match exact {
                Some(x) if x.generation == g && g > self.ck.generation => {
                    compare(&mut p, &x.state, None, false, self.cfg.block_size).map_err(|m| format!("{m} (exact at generation {g})"))?
                }
                _ if g >= self.ck.generation => compare(&mut p, &self.ck.state, Some(&self.since), g > self.ck.generation, self.cfg.block_size)
                    .map_err(|m| format!("{m} (generation {g}, checkpoint {})", self.ck.generation))?,
                _ => return Err(format!("crash image opened at generation {g}, older than the durable {}", self.ck.generation)),
            }
        }
        Ok(())
    }

    pub fn run(&mut self) -> Result<Report, String> {
        for n in 0..self.cfg.steps {
            let before = self.pool().generation();
            let exact = self.step().map_err(|m| format!("step {n} (seed {}): {m}", self.cfg.seed))?;
            // Exact only when the step's last device write was its commit's superblock (a commit that found
            // nothing to write only flushes, and the generation may come from a commit in the middle).
            let exact = exact && self.pool().generation() > before && self.dev.last_write_offset() < 2 * crate::layout::PAGE;
            let exp = Expect { generation: self.pool().generation(), state: self.model.clone() };
            if self.cfg.crash {
                self.check_images(if exact { Some(&exp) } else { None }).map_err(|m| format!("step {n} (seed {}, op {}): {m}", self.cfg.seed, self.last))?;
            }
            if exact {
                self.checkpoint();
            }
            if n % 64 == 63 {
                self.check_all().map_err(|m| format!("step {n} (seed {}): {m}", self.cfg.seed))?;
            }
            self.report.steps += 1;
        }
        self.reopen().map_err(|m| format!("final reopen (seed {}): {m}", self.cfg.seed))?;
        self.report.generations = self.pool().generation();
        Ok(self.report)
    }
}

/// Checks a pool against a model state; `since`: blocks may instead hold any stamp written after it.
fn compare(p: &mut Pool, st: &MState, since: Option<&BTreeSet<(u64, u64, u64)>>, auto_may_go: bool, bs: u32) -> Result<(), String> {
    let list = e(p.list(), "list")?;
    for v in &list {
        if !st.contains_key(&v.id) {
            return Err(format!("crash image has volume {} the model does not", v.id));
        }
    }
    let bs = bs as usize;
    for (&id, m) in st {
        let Some(v) = list.iter().find(|v| v.id == id) else {
            if auto_may_go && m.auto {
                continue;
            }
            return Err(format!("crash image lost volume {id}"));
        };
        if v.size != m.blocks * bs as u64 || v.snapshot != m.snapshot {
            return Err(format!("crash image: volume {id} differs"));
        }
        let mut buf = alloc::vec![0u8; m.blocks as usize * bs];
        e(p.read(id, 0, &mut buf, Normal), "read crash image")?;
        for (b, blk) in buf.chunks(bs).enumerate() {
            let b = b as u64;
            let g = stamp_of(blk).ok_or(format!("crash image: volume {id} block {b} holds garbage"))?;
            let want = m.data.get(&b).copied().unwrap_or(0);
            if g == want || since.is_some_and(|s| s.contains(&(id, b, g))) {
                continue;
            }
            return Err(format!("crash image: volume {id} block {b} holds {g}, expected {want}"));
        }
    }
    Ok(())
}
