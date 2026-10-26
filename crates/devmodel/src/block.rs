//! `C: BlockDevice` (trait owner FS; back ends US (USB storage), NV (NVMe), VS (bitsvol), KS (crypt), FS (RAM
//! disk)). Devices stack: partition -> crypt -> bitsvol -> file system (B1). Every request carries a priority
//! (B4). Writes may be refused by the device's write gate (R8a: NVMe without `nvme_write=on`, a disk with a
//! Windows or EFI System partition).

use crate::DevError;

/// I/O priority (B4): interactive reads first, background writeback last; aging prevents starvation (FS-12).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Default)]
pub enum IoPriority {
    Interactive,
    #[default]
    Normal,
    Background,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct BlockInfo {
    /// Bytes per logical block (512 or 4096).
    pub block_size: u32,
    pub blocks: u64,
    pub read_only: bool,
    /// Largest single request in blocks (USB stick 090C:1000: 64).
    pub max_blocks_per_request: u32,
    pub supports_discard: bool,
    pub has_volatile_cache: bool,
}

pub trait BlockDevice: Send + Sync {
    fn name(&self) -> &str;
    fn info(&self) -> BlockInfo;
    fn read_blocks(&self, lba: u64, buf: &mut [u8], prio: IoPriority) -> Result<(), DevError>;
    /// `fua`: the data is durable when this returns (journal commits).
    fn write_blocks(&self, lba: u64, buf: &[u8], fua: bool, prio: IoPriority) -> Result<(), DevError>;
    /// Makes all completed writes durable (SYNCHRONIZE CACHE / NVMe Flush).
    fn flush(&self) -> Result<(), DevError>;
    /// Every flush this device answered before this call is durable when it returns (INT-W12, VS-2b CR 3: a
    /// stacked device may answer a flush early and make it durable later, bitsvol during a snapshot's freeze).
    /// Called on the fsync caller's thread, never from the block dispatcher. Default: flushes are durable when
    /// answered.
    fn settle(&self) -> Result<(), DevError> {
        Ok(())
    }
    /// Tells the device the blocks are unused (TRIM). Default: ignored.
    fn discard(&self, _lba: u64, _blocks: u64) -> Result<(), DevError> {
        Ok(())
    }
}

/// Optional capability of a block device that can take consistent point-in-time snapshots (VS, B1).
pub trait Snapshots: Send + Sync {
    fn snapshot(&self, label: &str) -> Result<u64, DevError>;
    fn list(&self) -> alloc::vec::Vec<(u64, u64, alloc::string::String)>;
}
