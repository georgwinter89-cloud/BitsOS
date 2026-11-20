//! CRASHDISK: the panic record's area on the boot volume, `\BITSOS2\CRASH\SLOTS.BIN` (owner RB). The kernel's
//! crash code makes the file at boot (whole clusters, durable), writes its empty slots' headers through here and
//! asks for the file's place on the volume (`runs`); the panic path then writes those blocks itself, without the
//! file system (Linux 7.3.1 fs/pstore/blk.c and zone.c: a preallocated region the panic writer reaches with its
//! own polled I/O). Only this API changes anything below `CRASH` (the SYS grant, `protect::Class::Crash`), so no
//! program can free or move the clusters a panic would write.

use crate::vol::Volume;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use uapi::errno::*;
use vfs::MountNs;
use vfs::abi::{O_CREAT, O_WRONLY};
use vfs::ops::Ctx;
use vfs::pagecache::SyncKind;

pub const DIR: &str = "BITSOS2/CRASH";
pub const FILE: &str = "SLOTS.BIN";

/// Where the file lies on the volume.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Layout {
    /// The file's size in bytes (the directory entry's).
    pub size: u64,
    /// (first volume sector, sectors) of each contiguous run of the cluster chain, in file order; together they
    /// cover exactly the chain.
    pub runs: Vec<(u64, u64)>,
    /// Bytes per sector.
    pub bps: usize,
    /// The data area's first sector and the volume's sector count (a run outside them is a damaged chain).
    pub data_start: u64,
    pub total: u64,
    /// Links into the chain from elsewhere (`Volume::foreign_links`): 0 when the clusters are the file's alone.
    pub foreign: u64,
}

/// The CRASH folder of a mounted volume (`mount`: where its root is in `ns`, "/boot").
pub struct Area<'a> {
    pub vol: &'a Arc<Volume>,
    pub ns: &'a MountNs,
    pub mount: &'a str,
}

impl Area<'_> {
    fn dir(&self) -> String {
        format!("{}/{DIR}", self.mount)
    }
    fn path(&self) -> String {
        format!("{}/{FILE}", self.dir())
    }

    /// Makes the file exactly `bytes` long (created or grown with zeros, or cut), durable. True when it changed.
    pub fn ensure(&self, bytes: u64) -> Result<bool, i64> {
        let _g = self.vol.grant_sys();
        let c = Ctx::kernel(self.ns);
        match c.mkdir(None, self.dir().as_bytes(), 0o700) {
            Ok(_) | Err(EEXIST) => {}
            Err(e) => return Err(e),
        }
        let f = c.open(None, self.path().as_bytes(), O_WRONLY | O_CREAT, 0o600)?;
        let ino = f.inode().ok_or(EIO)?;
        let had = self.vol.disk_size(ino.meta.ino)?;
        if had == bytes {
            return Ok(false);
        }
        if had > bytes {
            c.truncate(None, self.path().as_bytes(), bytes)?;
        } else {
            // Written, not only sized: every cluster is allocated and holds zeros on the disk.
            let zeros = vec![0u8; 64 << 10];
            let mut at = had;
            while at < bytes {
                let n = ((bytes - at) as usize).min(zeros.len());
                let w = f.pwrite(at, &zeros[..n])?;
                if w == 0 {
                    return Err(EIO);
                }
                at += w as u64;
            }
        }
        ino.sync(SyncKind::Fsync)?;
        drop(f);
        self.vol.sync(true)?;
        klog::event!(Info, "fat", "crash_area.sized", bytes = bytes, before = had);
        Ok(true)
    }

    /// Writes `data` at `off` (both whole sectors, inside the file), durable.
    pub fn write_at(&self, off: u64, data: &[u8]) -> Result<(), i64> {
        let bps = self.vol.geo.bps as u64;
        if off % bps != 0 || data.len() as u64 % bps != 0 {
            return Err(EINVAL);
        }
        let _g = self.vol.grant_sys();
        let c = Ctx::kernel(self.ns);
        let d = self.ns.lookup(&self.path())?;
        if off + data.len() as u64 > self.vol.disk_size(d.inode.meta.ino)? {
            return Err(EINVAL);
        }
        let f = c.open(None, self.path().as_bytes(), O_WRONLY, 0)?;
        let mut done = 0;
        while done < data.len() {
            let w = f.pwrite(off + done as u64, &data[done..])?;
            if w == 0 {
                return Err(EIO);
            }
            done += w;
        }
        f.inode().ok_or(EIO)?.sync(SyncKind::Fsync)?;
        drop(f);
        self.vol.sync(true)
    }

    /// Up to `buf.len()` bytes at `off` (a multiple of the sector size) as the disk holds them, not the cache.
    pub fn read_at(&self, off: u64, buf: &mut [u8]) -> Result<usize, i64> {
        let d = self.ns.lookup(&self.path())?;
        self.vol.read_disk_at(d.inode.meta.ino, off, buf)
    }

    /// The file's place on the volume and whether anything else links into it. ENOENT without the file.
    pub fn layout(&self) -> Result<Layout, i64> {
        let d = self.ns.lookup(&self.path())?;
        let (size, runs) = self.vol.file_runs(d.inode.meta.ino)?;
        let foreign = self.vol.foreign_links(d.inode.meta.ino)?;
        let g = &self.vol.geo;
        Ok(Layout { size, runs, bps: g.bps, data_start: g.data_start, total: g.total, foreign })
    }
}

/// The runs of a cluster chain as (first sector, sectors), neighbours merged (pure; host-tested).
pub fn runs_of(chain: &[u32], cluster_sector: impl Fn(u32) -> u64, spc: u64) -> Vec<(u64, u64)> {
    let mut out: Vec<(u64, u64)> = Vec::new();
    for &c in chain {
        let s = cluster_sector(c);
        match out.last_mut() {
            Some((first, n)) if *first + *n == s => *n += spc,
            _ => out.push((s, spc)),
        }
    }
    out
}

impl Volume {
    /// A file's size and the sector runs of its cluster chain (CRASHDISK).
    pub fn file_runs(&self, ino: u64) -> Result<(u64, Vec<(u64, u64)>), i64> {
        let mut st = self.lock()?;
        let size = Self::node(&st, ino)?.e.size as u64;
        if Self::node(&st, ino)?.is_dir() {
            return Err(EISDIR);
        }
        let chain = self.chain(&mut st, ino)?.clone();
        let g = &self.geo;
        if (chain.len() as u64) * (g.cluster_bytes() as u64) < size || !chain.iter().all(|&c| g.valid_cluster(c)) {
            return Err(EIO);
        }
        Ok((size, runs_of(&chain, |c| g.cluster_sector(c), g.spc as u64)))
    }
}
