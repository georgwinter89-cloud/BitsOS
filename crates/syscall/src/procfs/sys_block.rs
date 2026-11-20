//! SYS-K: the block devices as Linux's tools read them: `/proc/diskstats` (linux-7.3.1/block/genhd.c:1340
//! `diskstats_show`) and `/sys/block/<disk>/{dev,size,ro,stat,queue/{rotational,logical_block_size}}` with one
//! directory per partition (`dev`, `partition`, `start`, `size`, `ro`, `stat`; block/partitions/core.c), all 0444.
//! Partitions come from the disk's partition table (`Disk::scan`), their numbers from the partition's own disk when
//! BitsOS registered one (NVMe), else zeros (a stick's partitions are read through the whole disk). Other stacked
//! disks (volumes, encrypted disks) are disks of their own, as device-mapper's are.
//!
//! BitsOS makes no block device nodes, so the numbers follow Linux's scheme without a node behind them: NVMe and
//! partitions of other disks on the extended major 259 in registration order (an NVMe disk, then its partitions),
//! USB disks on 8 (16 minors each, the partition number added), RAM disks on 1, other stacked disks on 253.
//! Values are the block layer's counters (no device I/O on read).

use super::attr::{Live, PAGE};
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use block::disk::Disk;
use block::iostat::{DISCARD, FLUSH, IoStat, READ, WRITE};
use block::DiskKind;
use core::fmt::Write;
use uapi::errno::ENODEV;
use vfs::Errno;
use vfs::synth::{DirSource, SynthEntry};

/// One line's numbers (a disk or a partition).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counts {
    pub io: IoStat,
    pub inflight: u64,
    pub busy_ns: u64,
}

/// One block device as the files show it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Dev {
    pub name: String,
    pub major: u32,
    pub minor: u32,
    /// 512-byte sectors.
    pub sectors: u64,
    pub ro: bool,
    pub counts: Counts,
    /// A disk's own facts (None for a partition).
    pub disk: Option<DiskFacts>,
    /// A partition's number and first sector.
    pub part: Option<(u32, u64)>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiskFacts {
    pub rotational: bool,
    pub block_size: u32,
    pub removable: bool,
}

fn ms(ns: u64) -> u64 {
    ns / 1_000_000
}

/// The 17 numbers after the name, Linux's order.
fn fields(c: &Counts) -> [u64; 17] {
    let s = &c.io;
    let all = s.nsecs.iter().sum::<u64>();
    [
        s.ios[READ], s.merges[READ], s.sectors[READ], ms(s.nsecs[READ]) & 0xffff_ffff,
        s.ios[WRITE], s.merges[WRITE], s.sectors[WRITE], ms(s.nsecs[WRITE]) & 0xffff_ffff,
        c.inflight, ms(c.busy_ns) & 0xffff_ffff, ms(all) & 0xffff_ffff,
        s.ios[DISCARD], s.merges[DISCARD], s.sectors[DISCARD], ms(s.nsecs[DISCARD]) & 0xffff_ffff,
        s.ios[FLUSH], ms(s.nsecs[FLUSH]) & 0xffff_ffff,
    ]
}

/// `/proc/diskstats`: `%4u %7u %s` then the numbers; partitions without sectors are left out (genhd.c:1379).
pub fn diskstats(devs: &[Dev]) -> String {
    let mut out = String::new();
    for d in devs.iter().filter(|d| d.disk.is_some() || d.sectors > 0) {
        let _ = write!(out, "{:4} {:7} {}", d.major, d.minor, d.name);
        for v in fields(&d.counts) {
            let _ = write!(out, " {v}");
        }
        out.push('\n');
    }
    out
}

/// `stat` (genhd.c:1080 `part_stat_show`): the same numbers, each `%8`.
pub fn stat_line(c: &Counts) -> String {
    let mut out = String::new();
    for (i, v) in fields(c).iter().enumerate() {
        if i > 0 {
            out.push(' ');
        }
        let _ = write!(out, "{v:8}");
    }
    out.push('\n');
    out
}

fn counts(d: &Disk) -> Counts {
    let s = d.stats();
    Counts { io: d.io_stat(), inflight: s.inflight, busy_ns: s.busy_ns }
}

/// Every disk with its partitions behind it, numbered (see the module text).
fn devices() -> Vec<Dev> {
    let all = block::disks();
    let parts_of = |d: &Disk| d.scan().parts;
    // Stacked disks that are a partition of a listed disk appear under it.
    let mut part_names = Vec::new();
    for d in all.iter().filter(|d| d.kind() != DiskKind::Stacked) {
        for (_, _, _, n) in parts_of(d) {
            part_names.push(block::gpt::part_name(d.name(), n));
        }
    }
    let (mut ext, mut usb, mut ram, mut dm) = (0u32, 0u32, 0u32, 0u32);
    let mut out = Vec::new();
    for d in &all {
        if d.kind() == DiskKind::Stacked && part_names.iter().any(|p| p == d.name()) {
            continue;
        }
        let info = d.dev_info();
        let sectors = info.blocks * info.block_size as u64 / 512;
        let (major, minor) = match d.kind() {
            DiskKind::Usb => (8, usb * 16),
            DiskKind::Ram => (1, ram),
            DiskKind::Stacked => (253, dm),
            DiskKind::Nvme | DiskKind::Other => (259, ext),
        };
        match d.kind() {
            DiskKind::Usb => usb += 1,
            DiskKind::Ram => ram += 1,
            DiskKind::Stacked => dm += 1,
            DiskKind::Nvme | DiskKind::Other => ext += 1,
        }
        let rotational = d.kind() == DiskKind::Usb;
        out.push(Dev { name: String::from(d.name()), major, minor, sectors, ro: info.read_only, counts: counts(d), disk: Some(DiskFacts { rotational, block_size: info.block_size, removable: d.removable() }), part: None });
        if d.kind() == DiskKind::Stacked {
            continue;
        }
        let per = info.block_size as u64 / 512;
        for (first, blocks, _, n) in parts_of(d) {
            let name = block::gpt::part_name(d.name(), n);
            let own = all.iter().find(|x| x.name() == name);
            let (pmajor, pminor) = if d.kind() == DiskKind::Usb && n < 16 {
                (8, minor + n)
            } else {
                ext += 1;
                (259, ext - 1)
            };
            out.push(Dev {
                name,
                major: pmajor,
                minor: pminor,
                sectors: blocks * per,
                ro: info.read_only,
                counts: own.map(|x| counts(x)).unwrap_or_default(),
                disk: None,
                part: Some((n, first * per)),
            });
        }
    }
    out
}

fn render_diskstats() -> String {
    diskstats(&devices())
}
vfs::procfile!("/proc/diskstats", render_diskstats);

/// The device named `name` now (ENODEV once it went).
fn find(name: &str) -> Result<Dev, Errno> {
    devices().into_iter().find(|d| d.name == name).ok_or(ENODEV)
}

fn attr(name: &str, f: fn(&Dev) -> String) -> Live {
    let n = String::from(name);
    Live::new(PAGE, move || find(&n).map(|d| f(&d)))
}

const COMMON: [(&str, fn(&Dev) -> String); 4] = [
    ("dev", |d| format!("{}:{}\n", d.major, d.minor)),
    ("ro", |d| format!("{}\n", u8::from(d.ro))),
    ("size", |d| format!("{}\n", d.sectors)),
    ("stat", |d| stat_line(&d.counts)),
];

fn files(name: &str, extra: &[(&str, fn(&Dev) -> String)]) -> Vec<SynthEntry> {
    COMMON.iter().chain(extra).map(|(n, f)| SynthEntry::file(n, 0o444, Arc::new(attr(name, *f)))).collect()
}

struct QueueDir(String);

impl DirSource for QueueDir {
    fn entries(&self) -> Result<Vec<SynthEntry>, Errno> {
        // hw_sector_size is the logical size as in Linux (`queue_logical_block_size`); BitsOS asks no device for a
        // larger physical size, so physical_block_size is the logical one too.
        let bs = |d: &Dev| format!("{}\n", d.disk.as_ref().map_or(512, |f| f.block_size));
        let q: [(&str, fn(&Dev) -> String); 4] = [
            ("hw_sector_size", bs),
            ("logical_block_size", bs),
            ("physical_block_size", bs),
            ("rotational", |d| format!("{}\n", u8::from(d.disk.as_ref().is_some_and(|f| f.rotational)))),
        ];
        Ok(numbered(q.iter().map(|(n, f)| SynthEntry::file(n, 0o444, Arc::new(attr(&self.0, *f)))).collect()))
    }
}

struct PartDir(String);

impl DirSource for PartDir {
    fn entries(&self) -> Result<Vec<SynthEntry>, Errno> {
        let extra: [(&str, fn(&Dev) -> String); 2] = [
            ("partition", |d| format!("{}\n", d.part.map_or(0, |p| p.0))),
            ("start", |d| format!("{}\n", d.part.map_or(0, |p| p.1))),
        ];
        Ok(numbered(files(&self.0, &extra)))
    }
}

struct DiskDir(String);

impl DirSource for DiskDir {
    fn entries(&self) -> Result<Vec<SynthEntry>, Errno> {
        let all = devices();
        let i = all.iter().position(|d| d.name == self.0).ok_or(ENODEV)?;
        let removable: [(&str, fn(&Dev) -> String); 1] = [("removable", |d| format!("{}\n", u8::from(d.disk.as_ref().is_some_and(|f| f.removable))))];
        let mut v = files(&self.0, &removable);
        v.push(SynthEntry::dir("queue", Arc::new(QueueDir(self.0.clone()))).with_perm(0o755));
        for p in all[i + 1..].iter().take_while(|d| d.disk.is_none()) {
            v.push(SynthEntry::dir(&p.name, Arc::new(PartDir(p.name.clone()))).with_perm(0o755));
        }
        Ok(numbered(v))
    }
}

fn numbered(mut v: Vec<SynthEntry>) -> Vec<SynthEntry> {
    for (i, e) in v.iter_mut().enumerate() {
        e.cookie = i as u64 + 1;
    }
    v
}

/// `/sys/block`: one directory per disk.
struct BlockRoot;

impl DirSource for BlockRoot {
    fn entries(&self) -> Result<Vec<SynthEntry>, Errno> {
        Ok(numbered(devices().into_iter().filter(|d| d.disk.is_some()).map(|d| SynthEntry::dir(&d.name, Arc::new(DiskDir(d.name.clone()))).with_perm(0o755)).collect()))
    }
}

pub(super) fn block_root() -> Arc<dyn DirSource> {
    Arc::new(BlockRoot)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dev() -> Dev {
        let mut io = IoStat::default();
        io.ios = [10, 3, 0, 2];
        io.merges = [1, 0, 0, 0];
        io.sectors = [80, 24, 0, 0];
        io.nsecs = [5_000_000, 2_500_000, 0, 1_000_000];
        Dev { name: String::from("nvme0n1"), major: 259, minor: 0, sectors: 1000, ro: false, counts: Counts { io, inflight: 1, busy_ns: 7_900_000 }, disk: Some(DiskFacts { rotational: false, block_size: 512, removable: false }), part: None }
    }

    #[test]
    fn diskstats_lines_follow_genhd() {
        let mut p = dev();
        p.name = String::from("nvme0n1p1");
        p.disk = None;
        p.part = Some((1, 2048));
        p.minor = 1;
        let mut empty = p.clone();
        empty.name = String::from("nvme0n1p2");
        empty.sectors = 0;
        let t = diskstats(&[dev(), p, empty]);
        assert_eq!(t, " 259       0 nvme0n1 10 1 80 5 3 0 24 2 1 7 8 0 0 0 0 2 1\n 259       1 nvme0n1p1 10 1 80 5 3 0 24 2 1 7 8 0 0 0 0 2 1\n");
    }

    /// GP-R2: a removable disk without a medium (0 sectors, never read) keeps its line of zeros: genhd.c:1378-1380
    /// leaves out only partitions without sectors, never a whole disk.
    #[test]
    fn an_empty_removable_disk_keeps_its_line() {
        let mut d = dev();
        d.name = String::from("sdb");
        (d.major, d.minor, d.sectors) = (8, 16, 0);
        d.counts = Counts { io: IoStat::default(), inflight: 0, busy_ns: 0 };
        d.disk = Some(DiskFacts { rotational: false, block_size: 512, removable: true });
        assert_eq!(diskstats(&[d]), "   8      16 sdb 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0\n");
    }

    #[test]
    fn stat_is_seventeen_columns_of_eight() {
        let s = stat_line(&dev().counts);
        assert_eq!(s.split_whitespace().count(), 17);
        assert!(s.starts_with("      10        1       80        5"), "{s}");
        assert!(s.ends_with("       2        1\n"));
    }
}
