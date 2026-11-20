//! The file allocation table: entries read through a sector cache, changes kept in memory until `write_out`,
//! which writes every changed sector to the first FAT and then to the second (so a cut between them leaves FAT 1
//! complete; fsck.fat then "uses the first FAT"). Callers order their `write_out`s: new chain entries before the
//! link that makes them reachable, and data before both (`vol`).

use crate::bpb::Geometry;
use crate::dev::Dev;
use alloc::collections::{BTreeMap, BTreeSet};
use alloc::vec;
use alloc::vec::Vec;
use uapi::errno::*;

pub const EOC: u32 = 0x0FFF_FFFF;
pub const BAD: u32 = 0x0FFF_FFF7;
const MASK: u32 = 0x0FFF_FFFF;
/// Sectors per command of `write_out_runs` and `preload` (32 KiB at 512-byte sectors, as `scan` reads).
const RUN: usize = 64;

pub struct Fat {
    first: u64,
    size: u64,
    copies: u8,
    /// The FAT copy reads come from (the active one when mirroring is off).
    read_from: u64,
    ss: usize,
    clusters: u32,
    cache: BTreeMap<u64, Vec<u8>>,
    dirty: BTreeSet<u64>,
    /// Free clusters, once counted (or taken from FSInfo).
    pub free: Option<u32>,
    /// Where the next free-cluster search starts.
    pub hint: u32,
}

impl Fat {
    pub fn new(g: &Geometry) -> Fat {
        let read_from = if g.mirrored { g.reserved } else { g.reserved + g.active_fat.min(g.fats - 1) as u64 * g.fat_size };
        Fat { first: g.reserved, size: g.fat_size, copies: g.fats, read_from, ss: g.bps, clusters: g.clusters, cache: BTreeMap::new(), dirty: BTreeSet::new(), free: None, hint: 2 }
    }

    fn valid(&self, c: u32) -> bool {
        c >= 2 && c <= self.clusters + 1
    }

    fn at(&self, c: u32) -> (u64, usize) {
        let byte = c as u64 * 4;
        (byte / self.ss as u64, (byte % self.ss as u64) as usize)
    }

    fn sector(&mut self, dev: &dyn Dev, idx: u64) -> Result<&mut Vec<u8>, i64> {
        if !self.cache.contains_key(&idx) {
            let mut b = vec![0u8; self.ss];
            dev.read(self.read_from + idx, &mut b)?;
            self.cache.insert(idx, b);
        }
        self.cache.get_mut(&idx).ok_or(EIO)
    }

    /// The entry of cluster `c` (28 bits).
    pub fn get(&mut self, dev: &dyn Dev, c: u32) -> Result<u32, i64> {
        if !self.valid(c) {
            return Err(EIO);
        }
        let (idx, off) = self.at(c);
        let s = self.sector(dev, idx)?;
        Ok(u32::from_le_bytes([s[off], s[off + 1], s[off + 2], s[off + 3]]) & MASK)
    }

    /// Sets cluster `c`'s entry in memory (the top 4 bits stay as they were, FAT spec §4).
    pub fn set(&mut self, dev: &dyn Dev, c: u32, v: u32) -> Result<(), i64> {
        if !self.valid(c) {
            return Err(EIO);
        }
        let (idx, off) = self.at(c);
        let s = self.sector(dev, idx)?;
        let old = u32::from_le_bytes([s[off], s[off + 1], s[off + 2], s[off + 3]]);
        let new = (old & !MASK) | (v & MASK);
        s[off..off + 4].copy_from_slice(&new.to_le_bytes());
        self.dirty.insert(idx);
        if let Some(f) = self.free.as_mut() {
            match (old & MASK == 0, v & MASK == 0) {
                (true, false) => *f = f.saturating_sub(1),
                (false, true) => *f += 1,
                _ => {}
            }
        }
        Ok(())
    }

    /// The clusters of a chain from `first` (empty for 0). A link outside the volume, to a free or bad cluster,
    /// or a loop (more links than clusters) is EIO, as Linux's "invalid cluster chain".
    pub fn chain(&mut self, dev: &dyn Dev, first: u32) -> Result<Vec<u32>, i64> {
        let mut out = Vec::new();
        if first == 0 {
            return Ok(out);
        }
        let mut c = first;
        loop {
            if !self.valid(c) || out.len() as u64 > self.clusters as u64 {
                return Err(EIO);
            }
            out.try_reserve(1).map_err(|_| ENOMEM)?;
            out.push(c);
            let next = self.get(dev, c)?;
            if next >= 0x0FFF_FFF8 {
                return Ok(out);
            }
            if next == 0 || next == BAD || !self.valid(next) {
                return Err(EIO);
            }
            c = next;
        }
    }

    /// Finds `n` free clusters (from the hint, wrapping) without taking them: callers `link` them at once.
    pub fn find_free(&mut self, dev: &dyn Dev, n: usize) -> Result<Vec<u32>, i64> {
        let mut out = Vec::new();
        out.try_reserve(n).map_err(|_| ENOMEM)?;
        if n == 0 {
            return Ok(out);
        }
        let total = self.clusters;
        let start = if self.valid(self.hint) { self.hint } else { 2 };
        let mut c = start;
        for _ in 0..total {
            if self.get(dev, c)? == 0 {
                out.push(c);
                if out.len() == n {
                    self.hint = if c + 1 > total + 1 { 2 } else { c + 1 };
                    return Ok(out);
                }
            }
            c = if c > total { 2 } else { c + 1 };
        }
        Err(ENOSPC)
    }

    /// Links `cs` into one chain ending with EOC (in memory).
    pub fn link(&mut self, dev: &dyn Dev, cs: &[u32]) -> Result<(), i64> {
        for (i, &c) in cs.iter().enumerate() {
            let v = cs.get(i + 1).copied().unwrap_or(EOC);
            self.set(dev, c, v)?;
        }
        Ok(())
    }

    /// Marks every cluster of `cs` free (in memory).
    pub fn release(&mut self, dev: &dyn Dev, cs: &[u32]) -> Result<(), i64> {
        for &c in cs {
            self.set(dev, c, 0)?;
        }
        if let Some(&c) = cs.first()
            && c < self.hint
        {
            self.hint = c;
        }
        Ok(())
    }

    /// Writes the changed sectors: all to FAT 1, then all to FAT 2 (and further copies). A failed write
    /// leaves the sectors marked changed (the next `write_out` tries again).
    pub fn write_out(&mut self, dev: &dyn Dev) -> Result<(), i64> {
        if self.dirty.is_empty() {
            return Ok(());
        }
        for copy in 0..self.copies as u64 {
            for &idx in &self.dirty {
                let s = self.cache.get(&idx).ok_or(EIO)?;
                dev.write(self.first + copy * self.size + idx, s)?;
            }
        }
        self.dirty.clear();
        Ok(())
    }

    /// GP-START: `write_out` with neighbouring changed sectors written together (at most `RUN` per write): the same
    /// bytes, FAT 1 before FAT 2, in far fewer commands (a USB-2 stick takes each 512-byte write as a command of its
    /// own). Only `Volume::allocate_unzeroed` uses it; every other path keeps `write_out`.
    pub fn write_out_runs(&mut self, dev: &dyn Dev) -> Result<(), i64> {
        let idx: Vec<u64> = self.dirty.iter().copied().collect();
        for copy in 0..self.copies as u64 {
            let mut i = 0;
            while i < idx.len() {
                let mut j = i + 1;
                while j < idx.len() && idx[j] == idx[j - 1] + 1 && j - i < RUN {
                    j += 1;
                }
                let mut buf = Vec::new();
                buf.try_reserve_exact((j - i) * self.ss).map_err(|_| ENOMEM)?;
                for k in &idx[i..j] {
                    buf.extend_from_slice(self.cache.get(k).ok_or(EIO)?);
                }
                dev.write(self.first + copy * self.size + idx[i], &buf)?;
                i = j;
            }
        }
        self.dirty.clear();
        Ok(())
    }

    /// GP-START: reads the FAT into the cache `RUN` sectors at a time from the sector of cluster `from` (wrapping to
    /// cluster 2) until the sectors hold `need` free entries at or after `from` the way `find_free` walks, so its
    /// `get`s (one sector read per miss) find everything cached. Sectors already cached keep their (maybe changed)
    /// bytes. Stops at the whole FAT.
    pub fn preload(&mut self, dev: &dyn Dev, from: u32, need: u32) -> Result<(), i64> {
        let from = if self.valid(from) { from } else { 2 };
        let per = (self.ss / 4) as u64;
        let last = self.at(self.clusters + 1).0;
        let (start, _) = self.at(from);
        let total = last + 1;
        let (mut done, mut free) = (0u64, 0u64);
        while done < total && free < need as u64 {
            let idx = (start + done) % total;
            let k = (total - idx).min(RUN as u64).min(total - done);
            if (idx..idx + k).any(|i| !self.cache.contains_key(&i)) {
                let mut buf = vec![0u8; k as usize * self.ss];
                dev.read(self.read_from + idx, &mut buf)?;
                for (j, s) in buf.chunks(self.ss).enumerate() {
                    self.cache.entry(idx + j as u64).or_insert_with(|| s.to_vec());
                }
            }
            for i in idx..idx + k {
                let s = self.cache.get(&i).ok_or(EIO)?;
                for (e, v) in s.chunks_exact(4).enumerate() {
                    let c = i * per + e as u64;
                    let past = if done == 0 { c >= from as u64 } else { true };
                    if past && self.valid(c as u32) && u32::from_le_bytes([v[0], v[1], v[2], v[3]]) & MASK == 0 {
                        free += 1;
                    }
                }
            }
            done += k;
        }
        Ok(())
    }

    /// Counts the free clusters (`scan`).
    pub fn count_free(&mut self, dev: &dyn Dev) -> Result<u32, i64> {
        if let Some(f) = self.free {
            return Ok(f);
        }
        let mut free = 0u32;
        self.scan(dev, |_, v| free += (v == 0) as u32)?;
        self.free = Some(free);
        Ok(free)
    }

    /// Calls `f(cluster, entry)` for every data cluster: the whole FAT read once, in 64-sector pieces, around the
    /// cache (its changed sectors count, not the disk's).
    pub fn scan(&mut self, dev: &dyn Dev, mut f: impl FnMut(u32, u32)) -> Result<(), i64> {
        let per = (self.ss / 4) as u64;
        let last_idx = (self.clusters as u64 + 1) / per;
        let mut idx = 0u64;
        let mut buf = Vec::new();
        while idx <= last_idx {
            let n = (last_idx + 1 - idx).min(64);
            buf.resize(n as usize * self.ss, 0);
            dev.read(self.read_from + idx, &mut buf)?;
            for k in 0..n {
                let s: &[u8] = match self.cache.get(&(idx + k)) {
                    Some(c) => c,
                    None => &buf[k as usize * self.ss..(k as usize + 1) * self.ss],
                };
                for (j, e) in s.chunks_exact(4).enumerate() {
                    let c = (idx + k) * per + j as u64;
                    if c >= 2 && c <= self.clusters as u64 + 1 {
                        f(c as u32, u32::from_le_bytes([e[0], e[1], e[2], e[3]]) & MASK);
                    }
                }
            }
            idx += n;
        }
        Ok(())
    }
}
