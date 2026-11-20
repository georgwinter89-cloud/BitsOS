//! ATOMIC-3: `Disk::submit_via` grows the queue's lists (requests, the elevator's entries) with `block.queue`
//! dropped: under the spin lock the kernel's heap can neither reclaim nor wait (10 KiB on the preloader's reads,
//! Mini-PC 12.11.2026). The allocator here records the largest block asked for while this thread holds a spin
//! lock (ksync's preempt count).

use block::disk::Dispatch;
use block::{Data, Disk, DiskKind, Kind, RamDisk};
use std::sync::Arc;
use block::prio::IoPriority;

use mm_core::host_count::largest_under_spin;

#[global_allocator]
static ALLOC: mm_core::host_count::Counting = mm_core::host_count::Counting;

const TWO_PAGES: usize = 8192;

/// 3000 reads queued and none dispatched (both lists pass 16 KiB), then all of them waited for: no block of two
/// pages or more under a spin lock, and every read comes back.
#[test]
fn a_long_queue_grows_without_a_large_block_under_a_spin_lock() {
    const N: u64 = 3000;
    let r = Arc::new(RamDisk::new("q", 512, N + 8));
    let d = Disk::open(r, DiskKind::Ram, Dispatch::Inline).unwrap();
    let bufs: Vec<Data> = (0..N).map(|_| Data::Bytes(vec![1u8; 512])).collect();
    let (big, tickets) = largest_under_spin(|| bufs.into_iter().enumerate().map(|(i, b)| d.submit(Kind::Read, i as u64, 0, b, IoPriority::Normal).unwrap()).collect::<Vec<_>>());
    assert!(big < TWO_PAGES, "a block of {big} bytes under a spin lock while queueing");
    for t in tickets {
        match t.wait().unwrap() {
            Data::Bytes(v) => assert!(v.iter().all(|&b| b == 0), "a fresh RAM disk reads zeros"),
            _ => panic!("a read gives its bytes back"),
        }
    }
}

/// The probe itself: a block of two pages asked for under a spin lock is seen, one after it is not.
#[test]
fn the_probe_sees_a_large_block_under_a_spin_lock() {
    let l = ksync::SpinLock::new(());
    let (big, v) = largest_under_spin(|| {
        let _g = l.lock();
        std::hint::black_box(vec![0u8; TWO_PAGES])
    });
    assert!(big >= TWO_PAGES && v.len() == TWO_PAGES, "the probe saw {big} bytes");
    let (big, _) = largest_under_spin(|| std::hint::black_box(vec![0u8; TWO_PAGES]));
    assert_eq!(big, 0, "a block outside any spin lock counted");
}
