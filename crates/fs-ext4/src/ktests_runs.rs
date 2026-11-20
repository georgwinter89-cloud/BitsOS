//! `fs.ext4.write_lookups` (Safe): a write(2) over a file of preallocated runs and holes whose extents left the
//! inode asks the extent tree once per run, not once per page (`runs`), and promises blocks to the holes only.

use crate::mkfs::{self, MkfsOptions};
use crate::{mount, MemDev, Options};
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use ktest::{kassert, ktest, TestResult};
use vfs::abi::{O_CREAT, O_RDWR};
use vfs::ops::Ctx;

const PAGE: u64 = 4096;
const RUNS: u64 = 16;
const LEN: u64 = 8;

fn e<T: core::fmt::Debug>(x: T) -> String {
    format!("{x:?}")
}

fn write_lookups() -> TestResult {
    let dev = MemDev::new(vec![0u8; 32 << 20]);
    let mut o = MkfsOptions::new(b"runs", [5; 16], [6; 16], vfs::inode::now());
    o.zeroed = true;
    mkfs::format(&dev, &o).map_err(e)?;
    let ns = vfs::MountNs::new_empty();
    let (v, _) = mount::mount_dev(Arc::new(MemDev::new(dev.into_image())), None, &ns, "/x", Options::default()).map_err(e)?;
    let c = Ctx::kernel(&ns);
    let f = c.open(None, b"/x/f", O_RDWR | O_CREAT, 0o600).map_err(e)?;
    let pages = RUNS * 2 * LEN;
    c.truncate(None, b"/x/f", pages * PAGE).map_err(e)?;
    let node = ns.lookup("/x/f").map_err(e)?.inode.clone();
    for r in 0..RUNS {
        node.fallocate(0, r * 2 * LEN * PAGE, LEN * PAGE).map_err(e)?;
    }
    v.sync_fs().map_err(e)?;
    let ino = node.meta.ino as u32;
    let (extents, tree) = {
        let st = v.lock();
        let n = st.nodes.get(&ino).ok_or("the file is not held")?;
        let (x, t) = crate::extent::all(&st, None, &n.di).map_err(e)?;
        (x.len() as u64, t)
    };
    kassert!(extents >= RUNS && tree > 0, "{extents} extents, {tree} tree blocks: the tree must have left the inode");
    let before = v.lock().map_lookups.get();
    f.pwrite(0, &vec![0x3Cu8; (pages * PAGE) as usize]).map_err(e)?;
    let st = v.lock();
    let lookups = st.map_lookups.get() - before;
    klog::event!(Info, "ktest", "write_lookups", pages = pages, extents = extents, lookups = lookups);
    kassert!(lookups == extents + RUNS, "{lookups} lookups for {extents} extents and {RUNS} holes ({pages} pages)");
    let promised: Vec<u64> = st.nodes.get(&ino).map(|n| n.reserved.keys().copied().collect()).unwrap_or_default();
    kassert!(promised.iter().copied().eq((0..pages).filter(|p| p / LEN % 2 == 1)), "promised pages {promised:?}");
    Ok(())
}
ktest!("fs.ext4.write_lookups", Safe, write_lookups);
