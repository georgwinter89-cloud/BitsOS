//! Block devices for the data volume (FS-8c): `ImageBlock` is a `C: BlockDevice` over a regular file of
//! another file system (the stick's `\BITSOS2\data.img`), so bitsvol's pool can live inside the image;
//! `BlockDev` is the other direction, a `Dev` over any `BlockDevice` (mkfs on a pool volume before it is
//! registered, host tests on a stacked volume).

use alloc::string::String;
use alloc::sync::Arc;
use crate::dev::Dev;
use crate::errno::*;
use devmodel::block::{BlockDevice, BlockInfo, IoPriority};
use devmodel::DevError;
use vfs::inode::{Inode, WriteAccess};
use vfs::pagecache::SyncKind;

const BS: u64 = 4096;
/// Largest request (1 MiB): one page-cache run of the image file.
const MAX_REQUEST_BLOCKS: u32 = 256;

fn dev_err(e: i64) -> DevError {
    match e {
        ENOSPC => DevError::NoSpace,
        ENOMEM => DevError::NoMemory,
        EROFS | EPERM | EACCES => DevError::Refused,
        ENODEV | ENXIO => DevError::Gone,
        ETIMEDOUT => DevError::Timeout,
        _ => DevError::Io,
    }
}

/// A loop device: 4 KiB blocks of an image file through its page cache. Its size is fixed when made (the
/// pool never grows its device); `flush` and a FUA write are the file's fsync (its pages, its file system's
/// metadata, the device cache), so the pool's commits are as durable as the stick.
pub struct ImageBlock {
    name: String,
    file: Arc<Inode>,
    blocks: u64,
    _write: Option<WriteAccess>,
}

impl ImageBlock {
    pub fn new(name: &str, file: Arc<Inode>, writable: bool) -> Result<ImageBlock, i64> {
        if !file.is_reg() {
            return Err(EINVAL);
        }
        let w = if writable { Some(file.get_write_access()?) } else { None };
        Ok(ImageBlock { name: name.into(), blocks: file.size() / BS, file, _write: w })
    }
    pub fn inode(&self) -> &Arc<Inode> {
        &self.file
    }
    fn range(&self, lba: u64, len: usize) -> Result<u64, DevError> {
        if len as u64 % BS != 0 || lba.checked_add(len as u64 / BS).is_none_or(|e| e > self.blocks) {
            return Err(DevError::InvalidArgument);
        }
        Ok(lba * BS)
    }
}

impl BlockDevice for ImageBlock {
    fn name(&self) -> &str {
        &self.name
    }
    fn info(&self) -> BlockInfo {
        BlockInfo {
            block_size: BS as u32,
            blocks: self.blocks,
            read_only: self._write.is_none(),
            max_blocks_per_request: MAX_REQUEST_BLOCKS,
            supports_discard: false,
            has_volatile_cache: true,
        }
    }
    fn read_blocks(&self, lba: u64, buf: &mut [u8], prio: IoPriority) -> Result<(), DevError> {
        let at = self.range(lba, buf.len())?;
        let _f = block::prio::floor(prio);
        match self.file.read_at(at, buf) {
            Ok(n) if n == buf.len() => Ok(()),
            Ok(_) => Err(DevError::Io),
            Err(e) => Err(dev_err(e)),
        }
    }
    fn write_blocks(&self, lba: u64, buf: &[u8], fua: bool, prio: IoPriority) -> Result<(), DevError> {
        if self._write.is_none() {
            return Err(DevError::Refused);
        }
        let at = self.range(lba, buf.len())?;
        let _f = block::prio::floor(prio);
        match self.file.write_at(at, buf) {
            Ok(n) if n == buf.len() => {}
            Ok(_) => return Err(DevError::Io),
            Err(e) => return Err(dev_err(e)),
        }
        if fua { self.flush() } else { Ok(()) }
    }
    fn flush(&self) -> Result<(), DevError> {
        if self._write.is_none() {
            return Ok(());
        }
        self.file.sync(SyncKind::Fsync).map_err(dev_err)
    }
}

/// A `Dev` straight on a block device (no queue, no cache): each call is one request in order.
pub struct BlockDev {
    dev: Arc<dyn BlockDevice>,
    bs: u64,
    bytes: u64,
}

impl BlockDev {
    pub fn new(dev: Arc<dyn BlockDevice>) -> BlockDev {
        let i = dev.info();
        let bs = i.block_size.max(1) as u64;
        BlockDev { dev, bs, bytes: i.blocks.saturating_mul(bs) }
    }
    fn lba(&self, at: u64, len: usize) -> Result<u64, i64> {
        if at % self.bs != 0 || len as u64 % self.bs != 0 || at.checked_add(len as u64).is_none_or(|e| e > self.bytes) {
            return Err(EIO);
        }
        Ok(at / self.bs)
    }
    fn errno(e: DevError) -> i64 {
        block::BlockError::from_dev(e).errno()
    }
    fn read_whole(&self, at: u64, buf: &mut [u8]) -> Result<(), i64> {
        let lba = self.lba(at, buf.len())?;
        let m = self.max_bytes();
        for (i, c) in buf.chunks_mut(m).enumerate() {
            self.dev.read_blocks(lba + (i * m) as u64 / self.bs, c, IoPriority::Normal).map_err(Self::errno)?;
        }
        Ok(())
    }
    /// Requests no larger than the device takes.
    fn max_bytes(&self) -> usize {
        (self.dev.info().max_blocks_per_request.max(1) as u64 * self.bs) as usize
    }
}

impl Dev for BlockDev {
    fn size(&self) -> u64 {
        self.bytes
    }
    fn read(&self, at: u64, buf: &mut [u8]) -> Result<(), i64> {
        crate::dev::read_part(self.bs, at, buf, |a, b| self.read_whole(a, b))
    }
    fn write(&self, at: u64, data: &[u8]) -> Result<(), i64> {
        crate::dev::write_part(self.bs, at, data, |a, b| self.read_whole(a, b), |a, d| {
            let lba = self.lba(a, d.len())?;
            let m = self.max_bytes();
            for (i, c) in d.chunks(m).enumerate() {
                self.dev.write_blocks(lba + (i * m) as u64 / self.bs, c, false, IoPriority::Normal).map_err(Self::errno)?;
            }
            Ok(())
        })
    }
    fn flush(&self) -> Result<(), i64> {
        self.dev.flush().map_err(Self::errno)
    }
    fn settle(&self) -> Result<(), i64> {
        self.dev.settle().map_err(Self::errno)
    }
}
