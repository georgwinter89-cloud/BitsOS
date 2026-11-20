//! `/proc/<pid>/pagemap` (linux-7.3.1/fs/proc/task_mmu.c `pagemap_read` :2304, `pagemap_open` :2398): eight
//! bytes per page of the process's address space, the entry of the page at address `pos / 8 * 4096`. Opening
//! needs ptrace access (EACCES), a reaped process ESRCH; a read whose position or size is not a multiple of eight
//! is EINVAL; reading ends at the top of user space. The open binds the memory it saw (`proc_mem_open` keeps the
//! mm in `file->private_data`): once that memory is gone (the target ran execve or ended) a read gives 0 bytes,
//! so a descriptor never shows a new image the ptrace check at open did not see. Frame numbers only when the
//! opener had CAP_SYS_ADMIN in the initial namespace (`file_ns_capable` with the file's credentials, taken at
//! open). The entries come from `mm::AsCore::pagemap`.

use super::{Target, may_read_mm};
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use mm::AddressSpace;
use proc::cred::{self, Cap};
use uapi::errno::*;
use vfs::synth::FileSource;
use vfs::{Cred, Errno};

const ENTRY: u64 = 8;
/// PAGEMAP_WALK_SIZE: the address range read under one hold of the space's lock.
const WALK: u64 = 2 << 20;

pub(super) struct Pagemap(pub(super) Target);

/// One open: the memory seen at open and whether the opener may see frame numbers.
struct Opened {
    t: Target,
    mm: Weak<AddressSpace>,
    show_pfn: bool,
}

impl FileSource for Pagemap {
    fn may_open(&self, _who: Cred, _write: bool) -> Result<(), Errno> {
        let (_, p) = self.0.live()?;
        if !may_read_mm(&p) {
            return Err(EACCES);
        }
        Ok(())
    }
    fn bind(&self, _who: Cred) -> Result<Option<Arc<dyn FileSource>>, Errno> {
        let (_, p) = self.0.live()?;
        let show_pfn = proc::current().is_some_and(|me| cred::capable(&me.cred(), Cap::SysAdmin));
        Ok(Some(Arc::new(Opened { t: self.0.clone(), mm: Arc::downgrade(&p.mm), show_pfn })))
    }
    fn render(&self, _who: Cred) -> Result<Vec<u8>, Errno> {
        Ok(Vec::new())
    }
    fn live_size(&self) -> Option<u64> {
        Some(u64::MAX)
    }
    fn size(&self) -> u64 {
        0
    }
    /// A kernel reader (no open, `vfs::file_of`): the current image, never frame numbers.
    fn read_live(&self, _who: Cred, pos: u64, buf: &mut [u8]) -> Result<usize, Errno> {
        let Ok((_, p)) = self.0.live() else { return Ok(0) };
        read(&self.0, &p.mm, false, pos, buf)
    }
}

impl FileSource for Opened {
    fn render(&self, _who: Cred) -> Result<Vec<u8>, Errno> {
        Ok(Vec::new())
    }
    /// Every position is a page's entry: the file has no end of its own (`mem_lseek`).
    fn live_size(&self) -> Option<u64> {
        Some(u64::MAX)
    }
    fn size(&self) -> u64 {
        0
    }
    fn read_live(&self, _who: Cred, pos: u64, buf: &mut [u8]) -> Result<usize, Errno> {
        // `mmget_not_zero` failing: the image of the open is gone.
        let Some(mm) = self.mm.upgrade() else { return Ok(0) };
        read(&self.t, &mm, self.show_pfn, pos, buf)
    }
}

fn read(t: &Target, mm: &AddressSpace, show_pfn: bool, pos: u64, buf: &mut [u8]) -> Result<usize, Errno> {
    if t.group().map_or(true, |g| g.is_finished()) {
        return Ok(0);
    }
    if pos % ENTRY != 0 || buf.len() as u64 % ENTRY != 0 {
        return Err(EINVAL);
    }
    let top = hal::USER_TOP;
    let svpfn = pos / ENTRY;
    if buf.is_empty() || svpfn > u64::MAX >> 12 || svpfn << 12 >= top {
        return Ok(0);
    }
    let mut va = svpfn << 12;
    let end = va.checked_add((buf.len() as u64 / ENTRY) << 12).filter(|&e| e < top).unwrap_or(top);
    let mut got = 0;
    let mut entries = Vec::new();
    while va < end {
        let to = ((va + WALK) & !(WALK - 1)).min(end);
        entries.clear();
        mm.pagemap(va, to, show_pfn, &mut entries).map_err(|_| ENOMEM)?;
        for e in &entries {
            buf[got..got + ENTRY as usize].copy_from_slice(&e.to_le_bytes());
            got += ENTRY as usize;
        }
        va = to;
    }
    Ok(got)
}
