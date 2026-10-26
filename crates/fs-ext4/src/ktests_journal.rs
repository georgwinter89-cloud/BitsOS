//! In-kernel tests of FS-8b (Safe, rerunnable; only memory devices and views of their own):
//!
//! - `fs.ext4.journal`: on the ktest image (4 KiB, metadata_csum: csum v3): fsync commits; the disk cut right
//!   after a commit's flush (logged, not yet at home) is replayed at the next mount, a read-only mount
//!   replays it in memory and writes nothing; freeze leaves a disk without needs_recovery, a thaw lets
//!   changes through again; on a loop image (tmpfs) one fsync and one freeze are timed and the volume lock's
//!   lift is checked (event `ktest.journal`: `fsync_us`, `freeze_us`, `lift`); no new lock-order violation.

use crate::mount;
use crate::{MemDev, Options};
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use ktest::{kassert, ktest, TestResult};
use vfs::pagecache::SyncKind;

static IMAGE: &[u8] = include_bytes!("../testdata/ktest.img.z");

fn e<T: core::fmt::Debug>(x: T) -> String {
    format!("{x:?}")
}

fn view(img: Vec<u8>, writable: bool) -> Result<(Arc<MemDev>, vfs::MountNs, Arc<crate::Volume>), String> {
    let dev = Arc::new(MemDev::new(img));
    let ns = vfs::MountNs::new_empty();
    let (v, _) = mount::mount_dev(dev.clone(), None, &ns, "/x", Options { writable, ..Options::default() }).map_err(e)?;
    Ok((dev, ns, v))
}

fn recover_flag(img: &[u8]) -> bool {
    crate::le::g32(img, 1024 + crate::sb::FEATURE_INCOMPAT) & crate::sb::incompat::RECOVER != 0
}

fn fs_ext4_journal() -> TestResult {
    let before = ksync::lockdep::counts().unexpected;
    journal_inner()?;
    let after = ksync::lockdep::counts().unexpected;
    kassert!(after == before, "{} new lock-order violations: {:?}", after - before, ksync::lockdep::list());
    Ok(())
}

fn journal_inner() -> TestResult {
    let img = miniz_oxide::inflate::decompress_to_vec_zlib(IMAGE).map_err(|x| format!("inflate: {x:?}"))?;
    let (dev, ns, v) = view(img, true)?;
    v.sync_fs().map_err(e)?;
    let base = dev.image();
    dev.record();
    let commits = crate::journal::commits();
    let data: Vec<u8> = (0..50_000u32).map(|i| (i % 241) as u8).collect();
    ns.create_file_bytes(b"/x/logged", 0o644, &data).map_err(e)?;
    ns.lookup("/x/logged").map_err(e)?.inode.sync(SyncKind::Fsync).map_err(e)?;
    kassert!(crate::journal::commits() > commits, "fsync did not commit");
    let log = dev.take_log();
    let last = log.iter().rposition(|(a, _)| *a == u64::MAX).ok_or("no flush in the log")?;
    let mut cut = base.clone();
    for (at, d) in &log[..last] {
        if *at != u64::MAX {
            cut[*at as usize..*at as usize + d.len()].copy_from_slice(d);
        }
    }
    kassert!(recover_flag(&cut), "the cut disk does not need recovery");
    drop((ns, v));
    // Read-only: replayed in memory.
    let replays = crate::journal::replays();
    let (rdev, rns, _rv) = view(cut.clone(), false)?;
    kassert!(rns.read_file("/x/logged", 1 << 20).map_err(e)? == data, "read-only replay");
    kassert!(rdev.image() == cut, "a read-only mount wrote");
    // Writable: replayed to the disk.
    let (wdev, wns, wv) = view(cut, true)?;
    kassert!(crate::journal::replays() >= replays + 2, "no replay counted");
    kassert!(wns.read_file("/x/logged", 1 << 20).map_err(e)? == data, "replay");
    // Freeze: a disk that needs no recovery; changes after the thaw.
    let sb = wns.lookup("/x").map_err(e)?.sb.clone();
    wns.create_file_bytes(b"/x/before-freeze", 0o644, &vec![7u8; 5000]).map_err(e)?;
    vfs::FsFreeze::freeze(&*sb).map_err(e)?;
    let frozen = wdev.image();
    vfs::FsFreeze::thaw(&*sb);
    kassert!(!recover_flag(&frozen), "a frozen disk needs recovery");
    let (_fd, fns, _fv) = view(frozen, false)?;
    kassert!(fns.read_file("/x/before-freeze", 10_000).map_err(e)? == vec![7u8; 5000], "frozen disk lacks the written-back file");
    wns.create_file_bytes(b"/x/after-thaw", 0o644, b"thawed").map_err(e)?;
    wns.lookup("/x/after-thaw").map_err(e)?.inode.sync(SyncKind::Fsync).map_err(e)?;
    wv.sync_fs().map_err(e)?;
    let (_d3, ns3, _v3) = view(wdev.image(), true)?;
    kassert!(ns3.read_file("/x/after-thaw", 100).map_err(e)? == b"thawed");
    kassert!(ns3.read_file("/x/logged", 1 << 20).map_err(e)? == data);
    loop_timing(&base)
}

/// One fsync and one freeze on the image as a loop file of tmpfs (two image fsyncs per commit), and the
/// lift: the lock's holder goes Interactive while an Interactive thread waits.
fn loop_timing(img: &[u8]) -> TestResult {
    let ns = vfs::MountNs::new_empty();
    ns.mkdir_p("/img").map_err(e)?;
    ns.create_file_bytes(b"/img/j.img", 0o644, img).map_err(e)?;
    let (v, _) = mount::mount_image(&ns, "/img/j.img", "/x", Options::default()).map_err(e)?;
    ns.create_file_bytes(b"/x/timed", 0o644, &vec![3u8; 64 * 1024]).map_err(e)?;
    let t0 = ksync::now_ns().unwrap_or(0);
    ns.lookup("/x/timed").map_err(e)?.inode.sync(SyncKind::Fsync).map_err(e)?;
    let t1 = ksync::now_ns().unwrap_or(0);
    ns.create_file_bytes(b"/x/dirty", 0o644, &vec![4u8; 64 * 1024]).map_err(e)?;
    let sb = ns.lookup("/x").map_err(e)?.sb.clone();
    let t2 = ksync::now_ns().unwrap_or(0);
    vfs::FsFreeze::freeze(&*sb).map_err(e)?;
    let t3 = ksync::now_ns().unwrap_or(0);
    vfs::FsFreeze::thaw(&*sb);
    let lift = {
        let _st = v.lock();
        let l = v.dev.lift().ok_or("the loop image has no lift")?;
        let normal = l.prio();
        let _w = l.wait_as(block::prio::IoPriority::Interactive);
        let urgent = l.prio();
        kassert!(normal != block::prio::IoPriority::Background && urgent == block::prio::IoPriority::Interactive, "lift: {normal:?} then {urgent:?}");
        urgent
    };
    klog::event!(Info, "ext4", "ktest.journal", fsync_us = (t1 - t0) / 1000, freeze_us = (t3 - t2) / 1000, lift = format!("{lift:?}").as_str());
    v.sync_fs().map_err(e)
}
ktest!("fs.ext4.journal", Safe, fs_ext4_journal);
