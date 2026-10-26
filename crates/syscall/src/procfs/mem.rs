//! `/proc/<pid>/mem` (linux-6.6/fs/proc/base.c `mem_open` :823, `mem_rw` :844): the process's memory read
//! at the file position as an address. Opening needs ptrace access (EACCES); a read stops at the first page
//! that cannot be read and answers EIO when not one byte came (`access_remote_vm` returned 0); once the
//! memory is gone a read gives 0 bytes.
//!
//! Writing needs the position, which `vfs::synth::FileSource::write` does not get: a write answers EIO
//! (change request to FS for a positioned write).

use super::{Target, may_read_mm};
use alloc::vec::Vec;
use uapi::errno::*;
use vfs::synth::FileSource;
use vfs::{Cred, Errno};

pub(super) struct Mem(pub(super) Target);

impl FileSource for Mem {
    fn may_open(&self, _who: Cred, _write: bool) -> Result<(), Errno> {
        let (_, p) = self.0.live()?;
        if !may_read_mm(&p) {
            return Err(EACCES);
        }
        Ok(())
    }
    fn render(&self, _who: Cred) -> Result<Vec<u8>, Errno> {
        Ok(Vec::new())
    }
    /// Every position is an address: the file has no end of its own.
    fn live_size(&self) -> Option<u64> {
        Some(u64::MAX)
    }
    fn size(&self) -> u64 {
        0
    }
    fn read_live(&self, _who: Cred, pos: u64, buf: &mut [u8]) -> Result<usize, Errno> {
        let Ok((g, p)) = self.0.live() else { return Ok(0) };
        if g.is_finished() {
            return Ok(0);
        }
        let mut got = 0;
        while got < buf.len() {
            let a = pos.saturating_add(got as u64);
            let n = ((4096 - (a & 4095)) as usize).min(buf.len() - got);
            if p.mm.copy_from_user(&mut buf[got..got + n], a).is_err() {
                break;
            }
            got += n;
        }
        if got == 0 && !buf.is_empty() {
            return Err(EIO);
        }
        Ok(got)
    }
    fn write(&self, _who: Cred, _data: &[u8]) -> Result<usize, Errno> {
        klog::event!(Debug, "proc", "mem.write_unsupported", tgid = self.0.tgid);
        Err(EIO)
    }
}
