//! A namespace as `C: BlockDevice` (FS-6's block layer puts it behind a queue). Read-only (R8a) unless the
//! controller holds a checked window for this namespace (NV-2p): `info().read_only` says so, and `write_blocks`
//! and `discard` go to `Controller::write`/`deallocate`, which refuse everything outside the window without
//! building a command. `flush` sends Flush only when the controller has a volatile write cache (Linux: core.c
//! `nvme_init_identify`, VWC).

use crate::ctrl::Controller;
use crate::hw::Hw;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};
use devmodel::DevError;
use devmodel::block::{BlockDevice, BlockInfo, FrameIo, IoBuf, IoPriority};

pub struct NsDevice<H: Hw> {
    ctrl: Arc<Controller<H>>,
    pub nsid: u32,
    name: String,
    block_size: u32,
    blocks: u64,
    max_blocks: u32,
    cache: bool,
    /// ONCS says Dataset Management works.
    dsm: bool,
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
        let dsm = info.ident.as_ref().is_some_and(|i| i.oncs & crate::cmd::ONCS_DSM != 0);
        // Linux's name: nvme<controller>n<namespace>.
        let name = alloc::format!("{}n{}", ctrl.name, nsid);
        Some(NsDevice { layout: ctrl.layout(), ctrl, nsid, name, block_size: bs, blocks: ns.id.nsze, max_blocks, cache, dsm, refused_logged: AtomicBool::new(false) })
    }

    pub fn controller(&self) -> &Arc<Controller<H>> {
        &self.ctrl
    }

    fn current(&self) -> Result<(), DevError> {
        if self.ctrl.layout() != self.layout { Err(DevError::Gone) } else { Ok(()) }
    }

    /// A window of this namespace is armed.
    fn writable(&self) -> bool {
        self.ctrl.window().is_some_and(|w| w.nsid() == self.nsid)
    }

    fn refuse(&self, what: &str) -> Result<(), DevError> {
        if !self.refused_logged.swap(true, Ordering::Relaxed) {
            klog::event!(Warn, "nvme", "write.refused", disk = self.name.as_str(), op = what;
                "R8a: no write window on this namespace; logged once");
        }
        Err(DevError::Refused)
    }
}

impl<H: Hw + 'static> BlockDevice for NsDevice<H> {
    fn name(&self) -> &str {
        &self.name
    }
    fn info(&self) -> BlockInfo {
        let writable = self.writable();
        BlockInfo {
            block_size: self.block_size,
            blocks: self.blocks,
            read_only: !writable,
            max_blocks_per_request: self.max_blocks,
            supports_discard: writable && self.dsm,
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
    /// BL-2: every read in flight at once, straight into the frames where the PRP rules allow it.
    fn read_frames(&self, reqs: &[FrameIo], _prio: IoPriority) -> Result<(), DevError> {
        self.current()?;
        let bs = self.block_size as usize;
        let mut list: Vec<(u64, &IoBuf)> = Vec::new();
        list.try_reserve_exact(reqs.len()).map_err(|_| DevError::NoMemory)?;
        for r in reqs {
            let len = r.buf.len();
            if len == 0 || !len.is_multiple_of(bs) || r.lba.checked_add((len / bs) as u64).is_none_or(|e| e > self.blocks) {
                return Err(DevError::InvalidArgument);
            }
            list.push((r.lba, &r.buf));
        }
        self.ctrl.read_frames(self.nsid, &list).map(|_| ()).map_err(|e| e.to_dev())
    }
    /// BL-2: the controller's tags.
    fn queue_depth(&self) -> u32 {
        self.ctrl.queue_depth()
    }
    /// BL-2: straight from the frames (no gather), under `write`'s window rules per command.
    fn write_frames(&self, lba: u64, buf: &IoBuf, fua: bool, _prio: IoPriority) -> Result<(), DevError> {
        if !self.writable() {
            return self.refuse("write");
        }
        self.current()?;
        self.ctrl.write_frames(self.nsid, lba, buf, fua).map_err(|e| e.to_dev())
    }
    fn write_blocks(&self, lba: u64, buf: &[u8], fua: bool, _prio: IoPriority) -> Result<(), DevError> {
        if !self.writable() {
            return self.refuse("write");
        }
        self.current()?;
        self.ctrl.write(self.nsid, lba, buf, fua).map_err(|e| e.to_dev())
    }
    fn flush(&self) -> Result<(), DevError> {
        self.current()?;
        if !self.cache {
            return Ok(());
        }
        self.ctrl.flush(self.nsid).map_err(|e| e.to_dev())
    }
    fn discard(&self, lba: u64, blocks: u64) -> Result<(), DevError> {
        if !self.writable() {
            return self.refuse("discard");
        }
        self.current()?;
        if !self.dsm {
            return Err(DevError::Unsupported);
        }
        self.ctrl.deallocate(self.nsid, lba, blocks).map_err(|e| e.to_dev())
    }
}
