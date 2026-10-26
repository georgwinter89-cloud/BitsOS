//! Partitions as block devices (NV-1; the file is NV's inside FS's crate, fs-FS-6.md:171). The partition
//! table itself is read once by `table::scan` at registration (GPT: linux-6.6/block/partitions/efi.c:335-433,
//! :581-640; MBR: msdos.c); its `Scan::parts` (first LBA, blocks, type, Linux number) become `Partition`s here,
//! each a `C: BlockDevice` over the whole disk's `DiskDevice`, registered as `DiskKind::Stacked`. So every
//! write to a partition passes the whole disk's write-refusal hook (R8a: a disk with a Windows or EFI System
//! partition refuses for good, NVMe without `nvme_write=on` too) before its own.
//!
//! Only `devmodel` is used, so the file also builds inside the nvme crate until `pub mod gpt;` is in
//! `block/src/lib.rs` (change request NV-1 CR 1).

use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use devmodel::DevError;
use devmodel::block::{BlockDevice, BlockInfo, IoPriority};

/// Linux's partition name (linux-6.6/block/partitions/core.c:349): a disk name ending in a digit gets a
/// `p` before the number (`nvme0n1p1`), others not (`sda1`).
pub fn part_name(disk: &str, number: u32) -> String {
    if disk.chars().last().is_some_and(|c| c.is_ascii_digit()) { format!("{disk}p{number}") } else { format!("{disk}{number}") }
}

pub struct Partition {
    lower: Arc<dyn BlockDevice>,
    name: String,
    pub first: u64,
    pub blocks: u64,
    pub number: u32,
    pub kind: String,
}

impl Partition {
    /// None when the range is empty or does not lie inside the lower device (a hostile table).
    pub fn new(lower: Arc<dyn BlockDevice>, disk: &str, first: u64, blocks: u64, number: u32, kind: &str) -> Option<Partition> {
        let total = lower.info().blocks;
        if blocks == 0 || first.checked_add(blocks).is_none_or(|e| e > total) {
            return None;
        }
        Some(Partition { lower, name: part_name(disk, number), first, blocks, number, kind: String::from(kind) })
    }

    fn range(&self, lba: u64, n: u64) -> Result<u64, DevError> {
        match lba.checked_add(n) {
            Some(e) if n > 0 && e <= self.blocks => Ok(self.first + lba),
            _ => Err(DevError::InvalidArgument),
        }
    }

    fn count(&self, len: usize) -> Result<u64, DevError> {
        let bs = self.lower.info().block_size as usize;
        if bs == 0 || len == 0 || !len.is_multiple_of(bs) { Err(DevError::InvalidArgument) } else { Ok((len / bs) as u64) }
    }
}

impl BlockDevice for Partition {
    fn name(&self) -> &str {
        &self.name
    }
    fn info(&self) -> BlockInfo {
        BlockInfo { blocks: self.blocks, ..self.lower.info() }
    }
    fn read_blocks(&self, lba: u64, buf: &mut [u8], prio: IoPriority) -> Result<(), DevError> {
        let at = self.range(lba, self.count(buf.len())?)?;
        self.lower.read_blocks(at, buf, prio)
    }
    fn write_blocks(&self, lba: u64, buf: &[u8], fua: bool, prio: IoPriority) -> Result<(), DevError> {
        let at = self.range(lba, self.count(buf.len())?)?;
        self.lower.write_blocks(at, buf, fua, prio)
    }
    fn flush(&self) -> Result<(), DevError> {
        self.lower.flush()
    }
    fn settle(&self) -> Result<(), DevError> {
        self.lower.settle()
    }
    fn discard(&self, lba: u64, blocks: u64) -> Result<(), DevError> {
        let at = self.range(lba, blocks)?;
        self.lower.discard(at, blocks)
    }
}

/// The partitions of `parts` (as `table::Scan::parts`) over `lower`; entries that do not fit are returned
/// as texts for the log.
pub fn partitions(lower: &Arc<dyn BlockDevice>, disk: &str, parts: &[(u64, u64, String, u32)]) -> (Vec<Partition>, Vec<String>) {
    let mut ok = Vec::new();
    let mut bad = Vec::new();
    for (first, blocks, kind, number) in parts {
        match Partition::new(lower.clone(), disk, *first, *blocks, *number, kind) {
            Some(p) => ok.push(p),
            None => bad.push(format!("{} at {first}+{blocks} outside the disk", part_name(disk, *number))),
        }
    }
    (ok, bad)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;
    use ksync::SpinLock;

    struct Mem {
        bs: u32,
        data: SpinLock<Vec<u8>>,
        writes: SpinLock<Vec<(u64, usize)>>,
    }
    impl BlockDevice for Mem {
        fn name(&self) -> &str {
            "mem0"
        }
        fn info(&self) -> BlockInfo {
            BlockInfo { block_size: self.bs, blocks: (self.data.lock().len() / self.bs as usize) as u64, read_only: false, max_blocks_per_request: 8, supports_discard: true, has_volatile_cache: false }
        }
        fn read_blocks(&self, lba: u64, buf: &mut [u8], _p: IoPriority) -> Result<(), DevError> {
            let o = lba as usize * self.bs as usize;
            buf.copy_from_slice(self.data.lock().get(o..o + buf.len()).ok_or(DevError::InvalidArgument)?);
            Ok(())
        }
        fn write_blocks(&self, lba: u64, buf: &[u8], _f: bool, _p: IoPriority) -> Result<(), DevError> {
            self.writes.lock().push((lba, buf.len()));
            Ok(())
        }
        fn flush(&self) -> Result<(), DevError> {
            Ok(())
        }
    }

    fn mem(bs: u32, blocks: usize) -> Arc<dyn BlockDevice> {
        let data = (0..blocks * bs as usize).map(|i| (i / bs as usize) as u8).collect();
        Arc::new(Mem { bs, data: SpinLock::new(data), writes: SpinLock::new(Vec::new()) })
    }

    #[test]
    fn names_follow_linux() {
        assert_eq!(part_name("nvme0n1", 3), "nvme0n1p3");
        assert_eq!(part_name("sda", 1), "sda1");
        assert_eq!(part_name("msd", 2), "msd2");
    }

    #[test]
    fn reads_are_offset_and_bounded() {
        for bs in [512u32, 4096] {
            let m = mem(bs, 64);
            let p = Partition::new(m.clone(), "d0", 10, 20, 1, "x").unwrap();
            assert_eq!(p.info().blocks, 20);
            assert_eq!(p.info().block_size, bs);
            let mut b = vec![0u8; 2 * bs as usize];
            p.read_blocks(0, &mut b, IoPriority::Normal).unwrap();
            assert_eq!(b[0], 10);
            assert_eq!(b[bs as usize], 11);
            p.read_blocks(18, &mut b, IoPriority::Normal).unwrap();
            assert_eq!(b[0], 28);
            assert_eq!(p.read_blocks(19, &mut b, IoPriority::Normal), Err(DevError::InvalidArgument));
            assert_eq!(p.read_blocks(u64::MAX, &mut b, IoPriority::Normal), Err(DevError::InvalidArgument));
            let mut odd = vec![0u8; bs as usize + 1];
            assert_eq!(p.read_blocks(0, &mut odd, IoPriority::Normal), Err(DevError::InvalidArgument));
            assert_eq!(p.discard(20, 1), Err(DevError::InvalidArgument));
            assert_eq!(p.discard(0, 0), Err(DevError::InvalidArgument));
        }
    }

    #[test]
    fn hostile_entries_are_skipped() {
        let m = mem(512, 64);
        let parts = vec![
            (2u64, 10u64, String::from("a"), 1u32),
            (60, 10, String::from("past the end"), 2),
            (u64::MAX - 1, 5, String::from("overflow"), 3),
            (5, 0, String::from("empty"), 4),
            (40, 24, String::from("to the end"), 5),
        ];
        let (ok, bad) = partitions(&m, "nvme1n1", &parts);
        assert_eq!(ok.iter().map(|p| p.number).collect::<Vec<_>>(), vec![1, 5]);
        assert_eq!(ok[1].name(), "nvme1n1p5");
        assert_eq!(bad.len(), 3);
    }
}
