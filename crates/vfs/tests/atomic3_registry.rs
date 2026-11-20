//! ATOMIC-3: the registries of page caches (`reclaim::register`: the reclaim ring and the frame-owner ids) grow
//! with the number of open files, and never under their spin locks: there the kernel's heap can neither reclaim
//! nor wait, and a browser start doubled them to 256 KiB each (Mini-PC 12.11.2026). The allocator here records the
//! largest block asked for while this thread holds a spin lock (ksync's preempt count).

use std::sync::Arc;
use vfs::pagecache::{Backend, CachePage, PageCache};
use vfs::Errno;

#[global_allocator]
static ALLOC: mm_core::host_count::Counting = mm_core::host_count::Counting;

/// The largest block `f` asked for under a spin lock.
fn largest_under_spin(f: impl FnOnce()) -> usize {
    mm_core::host_count::largest_under_spin(f).0
}

struct Zero;

impl Backend for Zero {
    fn read_page(&self, _pgoff: u64, _page: &Arc<CachePage>) -> Result<(), Errno> {
        Ok(())
    }
    fn write_run(&self, _first: u64, _pages: &[Arc<CachePage>], _size: u64) -> Result<(), Errno> {
        Ok(())
    }
}

const TWO_PAGES: usize = 8192;

/// 60 000 caches registered one by one (both lists pass 256 KiB), half of them dropped, as many made again (the
/// free ids and the dead ring entries are used): no block of two pages or more under a spin lock, and every
/// live cache is listed.
#[test]
fn many_caches_register_without_a_large_block_under_a_spin_lock() {
    std::thread::spawn(|| {
        const N: usize = 60_000;
        let b: Arc<dyn Backend> = Arc::new(Zero);
        let (before, _) = vfs::reclaim::stats().listed;
        let mut caches = Vec::with_capacity(N);
        let big = largest_under_spin(|| {
            for _ in 0..N {
                caches.push(PageCache::with_backend(b.clone(), 4096));
            }
        });
        assert!(big < TWO_PAGES, "a block of {big} bytes under a spin lock while registering");
        let (listed, _) = vfs::reclaim::stats().listed;
        assert!(listed >= before + N, "listed {listed}, before {before}");
        let big = largest_under_spin(|| {
            caches.truncate(N / 2);
            for _ in 0..N / 2 {
                caches.push(PageCache::with_backend(b.clone(), 4096));
            }
        });
        assert!(big < TWO_PAGES, "a block of {big} bytes under a spin lock while dropping and registering again");
        drop(caches);
    })
    .join()
    .unwrap();
}
