//! BIGDIR: getdents64 over a folder of 50000 names (tmpfs here: the listing is the VFS's own). Its own test
//! binary: it counts heap blocks (a counting allocator) and lock acquisitions (ksync's statistics, process-wide)
//! of single calls, which the unit tests running beside each other would blur. One listing call makes a constant
//! number of heap blocks and takes `vfs.dentry.dir` a constant number of times however many names it returns,
//! and never a child's `vfs.dentry.link`. Beside a thread that creates, renames and removes names, every name
//! that stays is listed exactly once per pass at a position that does not change, and positions only grow (a
//! name removed and made again during a pass is a new entry at a new position, as on Linux).

use mm_core::host_count::heap_blocks;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use vfs::abi::{O_CREAT, O_DIRECTORY, O_RDONLY, O_WRONLY};
use vfs::ops::{self, Ctx};
use vfs::MountNs;

// Counted by mm-core's front for the system allocator: implementing GlobalAlloc needs `unsafe`, which a test may
// not use (forbid-unsafe gate).
#[global_allocator]
static ALLOC: mm_core::host_count::Counting = mm_core::host_count::Counting;

fn taken(class: &str) -> u64 {
    ksync::stats::class(class).map_or(0, |c| c.acquired)
}

const NAMES: usize = 50_000;

fn name(i: usize) -> String {
    let pad = &"SharedData_Texture_Atlas_Block"[..i % 31];
    format!("Civ7_keep{i:06}_Base-Platforms{pad}.blp")
}

/// The records of one call: (name, d_off).
fn records(b: &[u8]) -> Vec<(Vec<u8>, u64)> {
    let mut out = Vec::new();
    let mut off = 0;
    while off < b.len() {
        let reclen = u16::from_le_bytes([b[off + 16], b[off + 17]]) as usize;
        let d_off = u64::from_le_bytes(b[off + 8..off + 16].try_into().unwrap());
        let n = b[off + 19..off + reclen].split(|&c| c == 0).next().unwrap().to_vec();
        out.push((n, d_off));
        off += reclen;
    }
    out
}

#[test]
fn listing_a_big_folder() {
    let ns = Arc::new(MountNs::new_empty());
    let c = Ctx::kernel(&ns);
    c.mkdir(None, b"/d", 0o755).unwrap();
    for i in 0..NAMES {
        drop(c.open(None, format!("/d/{}", name(i)).as_bytes(), O_CREAT | O_WRONLY, 0o644).unwrap());
    }
    constant_work_per_call(&c);
    stable_beside_changes(&ns);
}

/// No lock and no heap block per entry: a call that returns 700 names costs what a call for 3 costs.
fn constant_work_per_call(c: &Ctx) {
    // Once before the counting: what a lock class or a counter sets up at its first use is not the listing's.
    let f = c.open(None, b"/d", O_RDONLY | O_DIRECTORY, 0).unwrap();
    while !ops::read_dir(&f, 32 << 10).unwrap().is_empty() {}
    let f = c.open(None, b"/d", O_RDONLY | O_DIRECTORY, 0).unwrap();
    let mut listed = 0usize;
    let mut calls = 0u64;
    let (link0, dir0) = (taken("vfs.dentry.link"), taken("vfs.dentry.dir"));
    let t = std::time::Instant::now();
    let mut worst = 0;
    let mut bytes = 0u64;
    loop {
        let before = heap_blocks();
        let mut n = 0usize;
        let mut len = 0usize;
        ops::read_dir_into(&f, 32 << 10, |b| {
            len = b.len();
            // Counted without a heap block of the test's own.
            let mut off = 0;
            while off < b.len() {
                off += u16::from_le_bytes([b[off + 16], b[off + 17]]) as usize;
                n += 1;
            }
            Ok(())
        })
        .unwrap();
        worst = worst.max(heap_blocks() - before);
        if n == 0 {
            break;
        }
        calls += 1;
        listed += n;
        bytes += len as u64;
    }
    let ns_per_entry = t.elapsed().as_nanos() as u64 / listed as u64;
    let (link, dir) = (taken("vfs.dentry.link") - link0, taken("vfs.dentry.dir") - dir0);
    assert_eq!(listed, NAMES + 2);
    assert!(listed as u64 / calls > 300 && calls > 100, "{listed} names in {calls} calls of 32 KiB");
    assert!(worst <= 3, "a getdents64 call made {worst} heap blocks");
    // Once: the folder's own, for the inode of "..".
    assert!(link <= 1, "the listing took vfs.dentry.link {link} times");
    assert!(dir <= 4 * (calls + 1), "the listing took vfs.dentry.dir {dir} times in {calls} calls");
    println!("bigdir listing: {listed} names, {calls} calls, {bytes} bytes, {ns_per_entry} ns per entry, at most {worst} heap blocks per call, vfs.dentry.dir taken {dir} times, vfs.dentry.link {link}");
    // A buffer larger than the folder: one call, the same constants.
    let f = c.open(None, b"/d", O_RDONLY | O_DIRECTORY, 0).unwrap();
    let before = heap_blocks();
    let mut all = 0;
    while ops::read_dir_into(&f, 8 << 20, |b| {
        all += b.len();
        Ok(())
    })
    .unwrap()
        > 0
    {}
    assert_eq!(all as u64, bytes);
    assert!(heap_blocks() - before <= 600, "a listing into a large buffer made {} heap blocks for {NAMES} names", heap_blocks() - before);
    // Not even the first record fits: EINVAL, the position stays.
    let f = c.open(None, b"/d", O_RDONLY | O_DIRECTORY, 0).unwrap();
    assert_eq!(ops::read_dir(&f, 48).unwrap().len(), 48);
    assert_eq!(ops::read_dir(&f, 20).err(), Some(uapi::errno::EINVAL));
    assert_eq!(records(&ops::read_dir(&f, 200).unwrap())[0].0, name(0).into_bytes());
}

/// One thread creates, renames, exchanges and removes other names; listings with small and large buffers show
/// every name that stays exactly once per pass, always at the same position.
fn stable_beside_changes(ns: &Arc<MountNs>) {
    let stop = Arc::new(AtomicBool::new(false));
    let changer = {
        let (ns, stop) = (ns.clone(), stop.clone());
        std::thread::spawn(move || {
            let c = Ctx::kernel(&ns);
            let mut i = 0u64;
            while !stop.load(Ordering::Relaxed) {
                let (a, b, x) = (format!("/d/churn-{}", i % 97), format!("/d/churn-moved-{}", i % 89), format!("/d/churn-x-{}", i % 7));
                let _ = c.open(None, a.as_bytes(), O_CREAT | O_WRONLY, 0o644);
                let _ = c.open(None, x.as_bytes(), O_CREAT | O_WRONLY, 0o644);
                let _ = c.rename(None, a.as_bytes(), None, b.as_bytes(), 0);
                let _ = c.rename(None, b.as_bytes(), None, x.as_bytes(), vfs::abi::RENAME_EXCHANGE);
                if i % 3 == 0 {
                    let _ = c.unlink(None, b.as_bytes());
                }
                if i % 5 == 0 {
                    let _ = c.unlink(None, x.as_bytes());
                }
                i += 1;
            }
            i
        })
    };
    let c = Ctx::kernel(ns);
    let mut place: BTreeMap<Vec<u8>, u64> = BTreeMap::new();
    for (pass, size) in [(0, 32 << 10), (1, 300), (2, 4096), (3, 1 << 20), (4, 32 << 10), (5, 1000)] {
        let f = c.open(None, b"/d", O_RDONLY | O_DIRECTORY, 0).unwrap();
        let mut seen = BTreeSet::new();
        let mut kept = 0;
        let mut last = 0;
        loop {
            let b = ops::read_dir(&f, size).unwrap();
            if b.is_empty() {
                break;
            }
            for (n, d_off) in records(&b) {
                assert!(d_off > last, "pass {pass}: positions do not grow ({d_off} after {last})");
                last = d_off;
                if n.starts_with(b"Civ7_keep") {
                    assert!(seen.insert(n.clone()), "pass {pass}: {} listed twice", String::from_utf8_lossy(&n));
                    kept += 1;
                    let at = *place.entry(n.clone()).or_insert(d_off);
                    assert_eq!(at, d_off, "pass {pass}: {} moved", String::from_utf8_lossy(&n));
                }
            }
        }
        assert_eq!(kept, NAMES, "pass {pass}: names that stay, each once");
    }
    stop.store(true, Ordering::Relaxed);
    let rounds = changer.join().unwrap();
    assert!(rounds > 100, "the changing thread made {rounds} rounds");
}
