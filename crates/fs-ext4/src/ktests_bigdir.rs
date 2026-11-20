//! BIGDIR (Steam updating Civilization VII on the Mini-PC, 12.11.2026: folders of 50000 files, 1 MB/s with 54 %
//! system time): what Steam does in such a folder must not cost time in proportion to the folder.
//!
//! - `fs.ext4.bigdir` (Safe; an ext4 RAM disk of 256 MiB): 50000 creates in one folder, 200 full listings (open,
//!   fstat, getdents64 with a 32 KiB buffer to the end), failed lookups (access) alone and then each followed by a
//!   listing (Steam's case-insensitive search for a name it did not find), 50000 renames in place and 50000
//!   unlinks in a scattered order. One event `ext4 ktest.bigdir` carries the rates; loose floors are asserted.
//!   After the renames the folder's index is compared with a fresh pass over its blocks, and at the end no index
//!   may ever have been found stale.
//!   Every phase has a time budget (the code before BIGDIR needed minutes per phase): a phase that runs into it
//!   stops there, its rate is of what it did, and the floors then fail anyway.

use crate::mkfs::{self, MkfsOptions};
use crate::{mount, MemDev, Options};
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use ktest::{kassert, ktest, TestResult};
use vfs::abi::{O_CREAT, O_DIRECTORY, O_RDONLY, O_RDWR};
use vfs::ops::Ctx;

const FILES: u64 = 50_000;
const LISTINGS: u64 = 200;
const MISSES: u64 = 200_000;
const MISS_THEN_LIST: u64 = 5_000;
/// Per phase; the create phase gets `CREATE_BUDGET_NS` (the later phases need its folder).
const BUDGET_NS: u64 = 10_000_000_000;
const CREATE_BUDGET_NS: u64 = 300_000_000_000;

// Loose floors (numbers in plan/spec/abi.md §10): the code before BIGDIR made 110 creates, 36
// renames and 106 unlinks a second and needed 826 ns per listed entry; with it the debug kernel under QEMU/KVM
// makes 14000 to 19000 changes a second and lists an entry in 150 ns. A failed lookup never depended on the
// folder's size (190000 a second before, 250000 now): its floor only guards against a walk coming back.
// Steam's pattern itself, a failed lookup and then a listing of the folder, ran 36 times a second before and
// runs about 140 times now (a listing of 50000 names in 7 ms). The listing's ceiling leaves room for a build
// host whose CPUs are all busy with other lanes' tests: 334 ns and 84 pairs a second were measured there once.
const MIN_CHANGES_PER_S: u64 = 4_000;
const MAX_NS_PER_LISTED: u64 = 400;
const MIN_MISSES_PER_S: u64 = 80_000;
const MIN_MISS_THEN_LIST_PER_S: u64 = 60;

fn e<T: core::fmt::Debug>(x: T) -> String {
    format!("{x:?}")
}

fn now() -> u64 {
    ksync::now_ns().unwrap_or(0)
}

/// Names like Steam's: 30 to 60 bytes, mixed case.
fn name(i: u64, gen_: u32) -> String {
    let pad = &"SharedData_Texture_Atlas_Block"[..(i % 30) as usize];
    format!("/x/d/Civ7_Asset{gen_}_{i:06}_Base-Platforms{pad}.blp")
}

fn rate(done: u64, ns: u64) -> u64 {
    (done as u128 * 1_000_000_000 / ns.max(1) as u128) as u64
}

/// One listing as a program does it: open, fstat, getdents64 until nothing comes; returns the bytes listed.
fn list(c: &Ctx) -> Result<u64, String> {
    let f = c.open(None, b"/x/d", O_RDONLY | O_DIRECTORY, 0).map_err(e)?;
    if let Some(p) = f.path.as_ref() {
        let _ = p.dentry.attr();
    }
    let mut bytes = 0u64;
    loop {
        let n = vfs::ops::read_dir_into(&f, 32 << 10, |_| Ok(())).map_err(e)?;
        if n == 0 {
            return Ok(bytes);
        }
        bytes += n as u64;
    }
}

/// The entries of one listing, counted record by record.
fn count(c: &Ctx) -> Result<u64, String> {
    let f = c.open(None, b"/x/d", O_RDONLY | O_DIRECTORY, 0).map_err(e)?;
    let mut entries = 0u64;
    loop {
        let n = vfs::ops::read_dir_into(&f, 32 << 10, |b| {
            let mut off = 0;
            while off + 19 <= b.len() {
                off += u16::from_le_bytes([b[off + 16], b[off + 17]]) as usize;
                entries += 1;
            }
            Ok(())
        })
        .map_err(e)?;
        if n == 0 {
            return Ok(entries);
        }
    }
}

fn bigdir() -> TestResult {
    let dev = Arc::new(MemDev::new(vec![0u8; 256 << 20]));
    let mut o = MkfsOptions::new(b"bigdir", [3; 16], [5; 16], vfs::inode::now());
    o.zeroed = true;
    o.bytes_per_inode = 4096;
    mkfs::format(&*dev, &o).map_err(e)?;
    let ns = Arc::new(vfs::MountNs::new_empty());
    let (v, _m) = mount::mount_dev(dev, None, &ns, "/x", Options::default()).map_err(e)?;
    let c = Ctx::kernel(&ns);
    c.mkdir(None, b"/x/d", 0o755).map_err(e)?;
    let folder = c.stat(None, b"/x/d", true).map_err(e)?.ino as u32;

    let t = now();
    let mut made = 0u64;
    while made < FILES && now() - t < CREATE_BUDGET_NS {
        drop(c.open(None, name(made, 0).as_bytes(), O_RDWR | O_CREAT, 0o644).map_err(e)?);
        made += 1;
    }
    let create_ns = now() - t;
    kassert!(count(&c)? == made + 2, "a listing shows every name once");

    let t = now();
    let (mut listings, mut listed_bytes) = (0u64, 0u64);
    while listings < LISTINGS && now() - t < BUDGET_NS {
        listed_bytes += list(&c)?;
        listings += 1;
    }
    let list_ns = now() - t;
    let ns_per_listed = list_ns / (listings * (made + 2)).max(1);

    let t = now();
    let mut misses = 0u64;
    while misses < MISSES && now() - t < BUDGET_NS {
        // The name in another case: Steam's first try.
        let n = name(misses % made, 0).to_ascii_lowercase();
        kassert!(c.access(None, n.as_bytes(), 0, true).is_err(), "a name in another case was found");
        misses += 1;
    }
    let miss_ns = now() - t;

    let t = now();
    let mut pairs = 0u64;
    while pairs < MISS_THEN_LIST && now() - t < BUDGET_NS {
        let n = name(pairs % made, 0).to_ascii_lowercase();
        kassert!(c.access(None, n.as_bytes(), 0, true).is_err(), "a name in another case was found");
        list(&c)?;
        pairs += 1;
    }
    let pair_ns = now() - t;

    let t = now();
    let mut renamed = 0u64;
    while renamed < made && now() - t < BUDGET_NS {
        let i = renamed * 7919 % made;
        c.rename(None, name(i, 0).as_bytes(), None, name(i, 1).as_bytes(), 0).map_err(e)?;
        renamed += 1;
    }
    let rename_ns = now() - t;
    kassert!(count(&c)? == made + 2, "a listing after the renames shows every name once");
    // Outside the timed phases: the folder's index is what a fresh pass over its blocks builds.
    let mut checked = v.dirx_check(folder);
    if checked == Ok(None) {
        // The reclaim thread took the index back just now (memory was short): a change builds it again.
        drop(c.open(None, b"/x/d/probe", O_RDWR | O_CREAT, 0o644).map_err(e)?);
        c.unlink(None, b"/x/d/probe").map_err(e)?;
        checked = v.dirx_check(folder);
    }
    kassert!(checked == Ok(Some(made as usize)), "the folder's index after the renames: {checked:?} ({made} names)");

    let t = now();
    let mut removed = 0u64;
    while removed < made && now() - t < BUDGET_NS {
        // The order of the renames: the first `renamed` of it carry the new name.
        let i = removed * 7919 % made;
        let gen_ = u32::from(removed < renamed);
        c.unlink(None, name(i, gen_).as_bytes()).map_err(e)?;
        removed += 1;
    }
    let unlink_ns = now() - t;
    if removed == made {
        kassert!(count(&c)? == 2, "the folder is empty after the unlinks");
    }

    let (creates, renames, unlinks) = (rate(made, create_ns), rate(renamed, rename_ns), rate(removed, unlink_ns));
    let (misses_s, pairs_s) = (rate(misses, miss_ns), rate(pairs, pair_ns));
    // An index that was wrong is forgotten and built again: only this count shows there was one.
    let stale = v.dirx_stales(folder).0;
    klog::event!(Info, "ext4", "ktest.bigdir", files = made, creates_per_s = creates, listings = listings, ns_per_listed = ns_per_listed,
        listed_mb = listed_bytes >> 20, misses_per_s = misses_s, miss_then_list = pairs, miss_then_list_per_s = pairs_s,
        renamed = renamed, renames_per_s = renames, removed = removed, unlinks_per_s = unlinks, stale = stale);
    kassert!(stale == 0, "{stale} times an index did not match its folder's blocks");
    kassert!(made == FILES, "only {made} of {FILES} files made in {} s", CREATE_BUDGET_NS / 1_000_000_000);
    kassert!(creates >= MIN_CHANGES_PER_S, "{creates} creates per second in a folder of {made} files (floor {MIN_CHANGES_PER_S})");
    kassert!(renames >= MIN_CHANGES_PER_S, "{renames} renames per second (floor {MIN_CHANGES_PER_S})");
    kassert!(unlinks >= MIN_CHANGES_PER_S, "{unlinks} unlinks per second (floor {MIN_CHANGES_PER_S})");
    kassert!(ns_per_listed <= MAX_NS_PER_LISTED, "{ns_per_listed} ns per listed entry (at most {MAX_NS_PER_LISTED})");
    kassert!(misses_s >= MIN_MISSES_PER_S, "{misses_s} failed lookups per second (floor {MIN_MISSES_PER_S})");
    kassert!(pairs_s >= MIN_MISS_THEN_LIST_PER_S, "{pairs_s} failed lookups each followed by a listing per second (floor {MIN_MISS_THEN_LIST_PER_S})");
    Ok(())
}

ktest!("fs.ext4.bigdir", Safe, bigdir);
