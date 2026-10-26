//! Host tests of the heap's guards against damaged input (MM-3 review): forged slab headers in data, freed
//! pointers into memory the heap does not own, use-after-free writes into freed seed blocks, damaged slab
//! list links, two CPUs freeing one large block at once, the buddy allocator's owner tags, and the
//! emergency mode of the panic path.
use super::heap::{free_block, heap, heap_frames, holds, host_buddy, host_memory, lay, fill};
use super::*;
use crate::meta::TAG_SLAB;
use core::sync::atomic::Ordering::Relaxed;
use std::sync::{Arc, Barrier};

fn fill_words(p: usize, len: usize, word: u64) {
    for i in 0..len / 8 {
        // SAFETY: [p, p + len) is a live block the test owns, 8-aligned.
        unsafe { *((p + 8 * i) as *mut u64) = word };
    }
}

#[test]
fn forged_headers_in_data_are_not_slabs() {
    let b = host_buddy(16);
    let h = heap(Some(b), None, 1);
    // A large block full of every order-0 class cache's magic: none of its pages is a slab.
    let run = lay(8 * PAGE as usize, 8);
    let r = h.alloc(run) as usize;
    for c in 0..8 {
        let magic = h.classes[c].magic();
        fill_words(r, 8 * PAGE as usize, magic);
        let size = h.classes[c].geo.size;
        for page in 0..8 {
            free_block(h, (r + page * PAGE as usize + size) as *mut u8, lay(size, 8));
        }
    }
    assert_eq!(h.reports().heap_bad_frees.load(Relaxed), 64);
    // 16-byte objects full of the 8-byte class's magic, freed as 8-byte objects: the header at the end of
    // their slab names the 16-byte class.
    let m8 = h.classes[0].magic();
    let objs: Vec<usize> = (0..600).map(|_| h.alloc(lay(16, 8)) as usize).collect();
    for &o in &objs {
        fill_words(o, 16, m8);
    }
    for &o in &objs {
        free_block(h, o as *mut u8, lay(8, 8));
    }
    assert_eq!(h.reports().heap_bad_frees.load(Relaxed), 664);
    h.check().unwrap();
    for &o in &objs {
        free_block(h, o as *mut u8, lay(16, 8));
    }
    free_block(h, r as *mut u8, run);
    assert_eq!(h.reports().heap_bad_frees.load(Relaxed), 664, "the real frees are accepted");
    h.shrink();
    assert_eq!(heap_frames(b), 0);
    h.check().unwrap();
}

#[test]
fn frees_into_memory_the_heap_does_not_own_read_nothing() {
    let b = host_buddy(8);
    let h = heap(Some(b), None, 1);
    // Unmapped addresses (a read would crash the test), a free buddy block, another kind's block.
    let foreign = b.alloc(3, Zone::Normal, Kind::Driver).unwrap() as usize;
    fill_words(foreign, 8 * PAGE as usize, h.classes[3].magic());
    let free_frame = b.alloc(0, Zone::Normal, Kind::Other).unwrap();
    b.free(free_frame, 0, Kind::Other);
    let mut n = 0;
    for v in [0x7FF0_0000_0000usize, 0x7FF0_0000_1000 + 64, 0x10, free_frame as usize + 64, foreign + 64, foreign] {
        for l in [lay(64, 8), lay(PAGE as usize, 8), lay(3 * PAGE as usize, 8)] {
            free_block(h, v as *mut u8, l);
            n += 1;
        }
    }
    assert_eq!(h.reports().heap_bad_frees.load(Relaxed), n);
    // SAFETY: the test's own block, 8-aligned.
    let words = unsafe { core::slice::from_raw_parts(foreign as *const u64, 8 * PAGE as usize / 8) };
    assert!(words.iter().all(|&w| w == h.classes[3].magic()), "the foreign block is untouched");
    assert_eq!(b.reports().bad_frees.load(Relaxed), 0, "nothing reached the buddy allocator");
    b.free(foreign as u64, 3, Kind::Driver);
    h.check().unwrap();
}

#[test]
fn use_after_free_writes_cannot_steer_the_seed() {
    let len = 2 << 20;
    let seed = host_memory(len, 2 << 20);
    let h = heap(None, Some((seed, len)), 0);
    let l = lay(2048, 8);
    let v: Vec<usize> = (0..300).map(|_| h.alloc(l) as usize).collect();
    for &p in &v {
        free_block(h, p as *mut u8, l);
    }
    h.shrink();
    let st = h.stats().seed;
    assert!(st.free > 0 && st.table > 0, "{st:?}");
    // Stray writes into every freed block: pointers to unmapped memory and into the seed's own table.
    for &p in &v {
        fill_words(p, 2048, 0x7FF0_0000_0000);
        fill_words(p, 16, seed as u64);
    }
    let again: Vec<usize> = (0..300).map(|_| h.alloc(l) as usize).collect();
    assert!(again.iter().all(|&p| p >= seed + st.table as usize && p < seed + len), "objects from the seed's data pages");
    assert_eq!(h.stats().seed.carved, st.carved, "the freed blocks were reused, nothing new carved");
    for &p in &again {
        fill(p as *mut u8, 2048, 0x5A);
    }
    assert!(again.iter().all(|&p| holds(p as *mut u8, 2048, 0x5A)), "no two objects overlap");
    h.check().unwrap();
    for &p in &again {
        free_block(h, p as *mut u8, l);
    }
    h.shrink();
    h.check().unwrap();
    assert_eq!(h.reports().heap_corrupt.load(Relaxed), 0);
}

#[test]
fn damaged_slab_links_are_not_followed() {
    let b = host_buddy(8);
    let h = heap(Some(b), None, 0);
    let l = lay(64, 8);
    // Four full slabs of 63 objects; then a write past the last object of the second one reaches its
    // bitmaps and header: the links point into unmapped memory and the list number is out of range.
    let v: Vec<usize> = (0..4 * 63).map(|_| h.alloc(l) as usize).collect();
    let mut bases: Vec<usize> = v.iter().map(|&p| p & !(PAGE as usize - 1)).collect();
    bases.dedup();
    assert_eq!(bases.len(), 4);
    let hdr = bases[1] + h.classes[3].geo.hdr_off;
    assert_eq!(h.classes[3].geo.hdr_off, 63 * 64 + 16, "the header 16 bytes behind the last object");
    // SAFETY: the header of a slab of this heap (prev, next, taken, list), which the test damages on purpose
    // (host memory, mapped).
    unsafe {
        *(hdr as *mut usize) = 0x7FF0_0000_0000;
        *((hdr + 8) as *mut usize) = 0x7FF0_0000_2000;
        *((hdr + 20) as *mut u32) = 7;
    }
    for &p in &v {
        free_block(h, p as *mut u8, l);
    }
    assert!(h.reports().heap_corrupt.load(Relaxed) >= 1, "the damage was counted");
    // The heap keeps working: new objects, all distinct and usable.
    let w: Vec<usize> = (0..500).map(|_| h.alloc(l) as usize).collect();
    for &p in &w {
        fill(p as *mut u8, 64, 0x33);
    }
    assert!(w.iter().all(|&p| holds(p as *mut u8, 64, 0x33)));
    for &p in &w {
        free_block(h, p as *mut u8, l);
    }
    h.shrink();
}

#[test]
fn two_cpus_freeing_one_large_block_at_once() {
    let b = host_buddy(16);
    let h = heap(Some(b), None, 4);
    let l = lay(5 * PAGE as usize, 8);
    for round in 0..200 {
        let p = h.alloc(l) as usize;
        let go = Arc::new(Barrier::new(4));
        let ts: Vec<_> = (0..4)
            .map(|t| {
                let go = go.clone();
                std::thread::spawn(move || {
                    super::heap::set_cpu(t);
                    go.wait();
                    free_block(h, p as *mut u8, l);
                })
            })
            .collect();
        ts.into_iter().for_each(|t| t.join().unwrap());
        assert_eq!(h.reports().heap_bad_frees.load(Relaxed), 3 * (round + 1), "exactly one free accepted");
        assert_eq!(h.stats().large_bytes, 0);
        h.check().unwrap();
    }
    assert_eq!(b.reports().bad_frees.load(Relaxed), 0);
}

#[test]
fn buddy_owner_tags() {
    let b = staged(&[free(16 * MIB, 32 * MIB)]);
    // A slab block: only `free_tagged` with the slab tag frees it.
    let s = b.alloc_tagged(2, Zone::Dma32, Kind::Heap, TAG_SLAB).unwrap();
    assert!(b.is_tagged(s, 2, Kind::Heap, TAG_SLAB) && !b.is_tagged(s, 2, Kind::Driver, TAG_SLAB));
    b.free(s, 2, Kind::Heap);
    assert_eq!(b.reports().last_bad_free().map(|x| x.1), Some(crate::BadFree::HeapOwned));
    assert!(!b.free_exact(s, 4, Kind::Heap), "a slab is not a page run");
    assert!(b.free_tagged(s, 2, Kind::Heap, TAG_SLAB));
    // A run of 5 pages: the later piece is not freed on its own, nor through the frame allocator.
    let r = b.alloc_exact(5, 1, Zone::Dma32, Kind::Heap).unwrap();
    b.free(r + 4 * PAGE, 0, Kind::Heap);
    assert!(!b.free_exact(r + 4 * PAGE, 1, Kind::Heap));
    assert!(!b.free_exact(r, 4, Kind::Heap), "too short: the tail piece would be lost");
    assert!(!b.free_exact(r, 5, Kind::User) && !b.free_exact(r, 6, Kind::Heap));
    let s7 = b.alloc_exact(7, 1, Zone::Dma32, Kind::Heap).unwrap();
    assert!(!b.free_exact(s7, 6, Kind::Heap) && b.free_exact(s7, 7, Kind::Heap));
    assert!(b.run_allocated(r, 5, Kind::Heap), "the refused frees changed nothing");
    assert!(b.free_exact(r, 5, Kind::Heap));
    assert!(!b.free_exact(r, 5, Kind::Heap));
    assert_eq!(b.reports().bad_frees.load(Relaxed), 2);
    b.check().unwrap();
    assert_eq!(b.stats().used_by_kind[Kind::Heap as usize], 0);
}

#[test]
fn emergency_mode_takes_no_lock() {
    let b = host_buddy(4);
    let h = heap(Some(b), None, 1);
    let p = h.alloc(lay(64, 8));
    let q = h.alloc(lay(3 * PAGE as usize, 8));
    let requested = h.allocated_bytes();
    h.enter_emergency();
    // This CPU holds its magazine lock (as a panicking CPU might): nothing below may wait for it.
    let mags = h.mags.this().unwrap();
    let held = mags.lock();
    assert!(h.alloc(lay(64, 8)).is_null() && h.alloc(lay(8 * PAGE as usize, 8)).is_null());
    free_block(h, p, lay(64, 8));
    free_block(h, q, lay(3 * PAGE as usize, 8));
    drop(held);
    assert_eq!(h.allocated_bytes(), requested, "frees in emergency mode leak");
    assert_eq!(h.reports().heap_bad_frees.load(Relaxed), 0);
}
