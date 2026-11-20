//! `State::reserve_pages` asks the extent tree once per run of mapped blocks or of a hole, not once per block
//! (`runs`), and still promises blocks to exactly the pages the disk does not map.

use super::util::*;
use super::write::file;
use crate::inode::ROOT;
use std::vec::Vec;

const PAGE: u64 = 4096;
const RUNS: u64 = 10;
/// Pages of one preallocated run and of the hole after it.
const LEN: u64 = 4;

#[test]
fn a_write_over_a_tree_of_runs_looks_up_each_run_once() {
    for extra in [&["-b", "4096"][..], &["-b", "1024"][..], &["-b", "4096", "-O", "^has_journal"][..]] {
        let s = Scratch::new();
        let (_dev, v) = mount(mkfs(&s, "32M", extra, None));
        let f = file(&v, ROOT, "f");
        v.hold(f).unwrap();
        let pages = RUNS * 2 * LEN;
        v.set_size(f, pages * PAGE).unwrap();
        for r in 0..RUNS {
            v.fallocate(f, r * 2 * LEN * PAGE, LEN * PAGE).unwrap();
        }
        v.sync_fs().unwrap();
        let tree_blocks = {
            let st = v.lock();
            crate::extent::all(&st, None, &st.nodes[&f].di).unwrap()
        };
        // A run can be two extents (its blocks in two places on the disk): each is a lookup of its own.
        let extents = tree_blocks.0.len() as u64;
        assert!(extents >= RUNS, "{extra:?}: {extents} extents");
        assert!(tree_blocks.1 > 0, "{extra:?}: the extents left the inode (a leaf block is read per lookup)");
        let before = v.lock().map_lookups.get();
        v.reserve(f, 0, pages - 1).unwrap();
        let st = v.lock();
        assert_eq!(st.map_lookups.get() - before, extents + RUNS, "{extra:?}: one lookup per extent and per hole, not per block");
        let promised: Vec<u64> = st.nodes[&f].reserved.keys().copied().collect();
        let holes: Vec<u64> = (0..pages).filter(|p| p / LEN % 2 == 1).collect();
        assert_eq!(promised, holes, "{extra:?}: blocks promised to the pages in holes only");
        let per_page = (PAGE / st.geo.bs).max(1) as u32;
        assert!(st.nodes[&f].reserved.values().all(|&n| n == per_page), "{extra:?}: every block of a page in a hole");
    }
}

/// Pages past the disk's size take no lookup at all (a file that grows), and a second call for promised pages none.
#[test]
fn pages_past_the_disk_size_or_promised_already_take_no_lookup() {
    let s = Scratch::new();
    let (_dev, v) = mount(mkfs(&s, "32M", &["-b", "4096"], None));
    let f = file(&v, ROOT, "f");
    v.hold(f).unwrap();
    let before = v.lock().map_lookups.get();
    v.reserve(f, 0, 255).unwrap();
    v.reserve(f, 0, 255).unwrap();
    let st = v.lock();
    assert_eq!(st.map_lookups.get(), before);
    assert_eq!(st.nodes[&f].reserved.len(), 256);
}
