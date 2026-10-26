//! A namespace as `C: BlockDevice` (FS-6's block layer puts it behind a queue). Read side only (R8a, NV-1):
//! `info().read_only` is true, `write_blocks` and `discard` refuse without reaching the controller, `flush`
//! sends Flush only when the controller has a volatile write cache (Linux: core.c `nvme_init_identify`, VWC).

use crate::ctrl::Controller;
use crate::hw::Hw;
use alloc::string::String;
use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, Ordering};
use devmodel::DevError;
use devmodel::block::{BlockDevice, BlockInfo, IoPriority};

pub struct NsDevice<H: Hw> {
    ctrl: Arc<Controller<H>>,
    pub nsid: u32,
    name: String,
    block_size: u32,
    blocks: u64,
    max_blocks: u32,
    cache: bool,
    /// The controller's namespace layout this device was made for (a restart that changed it ends it).
    layout: u64,
    refused_logged: AtomicBool,
}

impl<H: Hw> NsDevice<H> {
    /// None when the controller does not list `nsid` as usable.
    pub fn new(ctrl: Arc<Controller<H>>, nsid: u32) -> Option<NsDevice<H>> {
        let info = ctrl.info();
        let ns = info.namespaces.iter().find(|n| n.nsid == nsid && n.unusable.is_none())?;
        let bs = ns.block_size();
        let max_blocks = u32::try_from(info.max_transfer / bs as usize).unwrap_or(u32::MAX).max(1);
        let cache = info.ident.as_ref().is_some_and(|i| i.volatile_cache());
        // Linux's name: nvme<controller>n<namespace>.
        let name = alloc::format!("{}n{}", ctrl.name, nsid);
        Some(NsDevice { layout: ctrl.layout(), ctrl, nsid, name, block_size: bs, blocks: ns.id.nsze, max_blocks, cache, refused_logged: AtomicBool::new(false) })
    }

    pub fn controller(&self) -> &Arc<Controller<H>> {
        &self.ctrl
    }

    fn current(&self) -> Result<(), DevError> {
        if self.ctrl.layout() != self.layout { Err(DevError::Gone) } else { Ok(()) }
    }

    fn refuse(&self, what: &str) -> Result<(), DevError> {
        if !self.refused_logged.swap(true, Ordering::Relaxed) {
            klog::event!(Warn, "nvme", "write.refused", disk = self.name.as_str(), op = what;
                "R8a: the NVMe driver has no write path (NV-1); logged once");
        }
        Err(DevError::Refused)
    }
}

impl<H: Hw + 'static> BlockDevice for NsDevice<H> {
    fn name(&self) -> &str {
        &self.name
    }
    fn info(&self) -> BlockInfo {
        BlockInfo {
            block_size: self.block_size,
            blocks: self.blocks,
            read_only: true,
            max_blocks_per_request: self.max_blocks,
            supports_discard: false,
            has_volatile_cache: self.cache,
        }
    }
    fn read_blocks(&self, lba: u64, buf: &mut [u8], _prio: IoPriority) -> Result<(), DevError> {
        self.current()?;
        let bs = self.block_size as usize;
        if buf.is_empty() || !buf.len().is_multiple_of(bs) || lba.checked_add((buf.len() / bs) as u64).is_none_or(|e| e > self.blocks) {
            return Err(DevError::InvalidArgument);
        }
        self.ctrl.read(self.nsid, lba, buf, true).map_err(|e| e.to_dev())
    }
    fn write_blocks(&self, _lba: u64, _buf: &[u8], _fua: bool, _prio: IoPriority) -> Result<(), DevError> {
        self.refuse("write")
    }
    fn flush(&self) -> Result<(), DevError> {
        self.current()?;
        if !self.cache {
            return Ok(());
        }
        self.ctrl.flush(self.nsid).map_err(|e| e.to_dev())
    }
    fn discard(&self, _lba: u64, _blocks: u64) -> Result<(), DevError> {
        self.refuse("discard")
    }
}
