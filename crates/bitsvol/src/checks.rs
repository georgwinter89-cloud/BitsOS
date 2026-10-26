//! Targeted checks shared by host tests and ktests: out of space through the stacked device, rollback and its
//! undo, a damaged superblock. Each builds its own RAM device, so they are rerunnable.

use crate::layout::PAGE;
use crate::memdev::MemDev;
use crate::model::{fill, stamp_of};
use crate::{Config, Pool, PoolHandle, VolError};
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use devmodel::block::{BlockDevice, IoPriority::Normal};
use ktest::{TestResult, kassert};

fn e<E: core::fmt::Debug>(x: E) -> String {
    format!("{x:?}")
}

fn cfg() -> Config {
    Config { chunk_size: 4096, reserve_chunks: Some(4), cache_pages: 32, commit_dirty_pages: 64, quiet: true }
}

fn dev(chunks: u64) -> Result<Arc<MemDev>, String> {
    Ok(Arc::new(MemDev::new("chk", 512, chunks * 8, 8).ok_or("no memory for the RAM device")?))
}

fn block(stamp: u64) -> Vec<u8> {
    let mut b = vec![0u8; 512];
    fill(&mut b, stamp);
    b
}

fn stamp(p: &mut Pool, id: u64, lba: u64) -> Result<u64, String> {
    let mut b = vec![0u8; 512];
    p.read(id, lba, &mut b, Normal).map_err(e)?;
    stamp_of(&b).ok_or_else(|| format!("block {lba} of volume {id} has no stamp"))
}

/// A pool smaller than its volume: the writer gets NoSpace (errno 28) both from the pool and through the
/// stacked device, everything written before stays, and a discard plus flush makes writes work again.
pub(crate) fn enospc_then_discard() -> TestResult {
    let d = dev(30)?;
    let h = PoolHandle::new(Pool::format(d, cfg()).map_err(e)?, "chk");
    let v = h.lock().map_err(e)?.create_volume("a", 100 * 4096, 1).map_err(e)?;
    let vd = h.open_volume(v).map_err(e)?;
    let mut full = None;
    for c in 0..100u64 {
        if let Err(x) = vd.write_blocks(c * 8, &block(c + 1), false, Normal) {
            full = Some((c, x));
            break;
        }
    }
    let (c, x) = full.ok_or("the pool is smaller than the volume, yet no write failed")?;
    kassert!(x == devmodel::DevError::NoSpace && VolError::NoSpace.errno() == 28, "device error {x:?}");
    {
        let mut p = h.lock().map_err(e)?;
        let px = p.write(v, c * 8, &block(c + 1), false, Normal).err();
        kassert!(px == Some(VolError::NoSpace), "pool error {px:?}");
        kassert!(c > 10, "full after only {c} chunks");
        kassert!(stamp(&mut p, v, 8)? == 2, "data written before the full pool changed");
    }
    vd.discard(0, 8 * 4).map_err(e)?;
    vd.flush().map_err(e)?;
    vd.write_blocks(c * 8, &block(500), false, Normal).map_err(e)?;
    let mut p = h.lock().map_err(e)?;
    kassert!(stamp(&mut p, v, c * 8)? == 500);
    kassert!(stamp(&mut p, v, 0)? == 0, "the discarded chunk still reads data");
    Ok(())
}

/// Rollback keeps a "before" snapshot, so it can be undone; snapshots stay read-only.
pub(crate) fn rollback_and_undo() -> TestResult {
    let mut p = Pool::format(dev(64)?, cfg()).map_err(e)?;
    let v = p.create_volume("a", 16 * 4096, 1).map_err(e)?;
    p.write(v, 0, &block(1), false, Normal).map_err(e)?;
    let s = p.snapshot(v, "yesterday", 2, false).map_err(e)?;
    p.write(v, 0, &block(2), false, Normal).map_err(e)?;
    p.write(v, 9, &block(3), false, Normal).map_err(e)?;
    let now = p.rollback(v, s, "before rollback", 3).map_err(e)?;
    kassert!((stamp(&mut p, v, 0)?, stamp(&mut p, v, 9)?) == (1, 0));
    p.write(v, 0, &block(4), false, Normal).map_err(e)?;
    kassert!(stamp(&mut p, s, 0)? == 1, "writing after the rollback changed the snapshot");
    let undo = p.rollback(v, now, "undo", 4).map_err(e)?;
    kassert!((stamp(&mut p, v, 0)?, stamp(&mut p, v, 9)?) == (2, 3), "the undo did not restore the state");
    kassert!(stamp(&mut p, undo, 0)? == 4);
    kassert!(p.rollback(v, v, "x", 5).err() == Some(VolError::InvalidArgument));
    kassert!(p.write(s, 0, &block(5), false, Normal).err() == Some(VolError::ReadOnly));
    Ok(())
}

/// A power cut at every write of a commit, the superblock write torn included: every image opens at the old
/// or the new generation, a torn new superblock falls back to the old one, and with both superblocks damaged
/// open fails without a panic. (After a complete commit the older generation's freed pages are discarded, so
/// it is a fallback for a torn commit only, as in dm-thin.)
pub(crate) fn damaged_superblock() -> TestResult {
    let d = dev(64)?;
    let mut p = Pool::format(d.clone(), cfg()).map_err(e)?;
    let v = p.create_volume("a", 64 * 512, 1).map_err(e)?;
    p.write(v, 3, &block(7), false, Normal).map_err(e)?;
    p.commit().map_err(e)?;
    let g = p.generation();
    p.write(v, 40, &block(9), false, Normal).map_err(e)?;
    d.set_capture(true, 5);
    p.commit().map_err(e)?;
    d.set_capture(false, 0);
    drop(p);
    let (mut old, mut new) = (0, 0);
    for img in d.take_images() {
        let mut q = Pool::open(Arc::new(MemDev::from_image("cut", 512, 8, img).ok_or("no memory")?), cfg()).map_err(e)?;
        let want = if q.generation() == g { old += 1; 0 } else { new += 1; 9 };
        kassert!(q.generation() == g || q.generation() == g + 1, "opened generation {}", q.generation());
        kassert!(stamp(&mut q, v, 3)? == 7 && stamp(&mut q, v, 40)? == want);
    }
    kassert!(old > 0, "no cut image fell back to generation {g} ({new} at the new one)");
    let mut img = d.image();
    img[100] ^= 0x10;
    img[PAGE + 100] ^= 0x10;
    let r = Pool::open(Arc::new(MemDev::from_image("sb2", 512, 8, img).ok_or("no memory")?), cfg());
    kassert!(r.is_err(), "a pool with two damaged superblocks opened");
    Ok(())
}
