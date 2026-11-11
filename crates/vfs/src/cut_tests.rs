//! A truncate takes a file's pages out of the cache in steps (`PageCache::truncate`, `CUT_STEP`): every page past
//! the new size is gone however many steps that takes, the kept ones and the cut page's head stay.

extern crate std;

use crate::pagecache::{Backend, CachePage, PageCache};
use alloc::sync::Arc;

const P: u64 = 4096;

struct Numbered;

impl Backend for Numbered {
    fn read_page(&self, pgoff: u64, page: &Arc<CachePage>) -> Result<(), i64> {
        page.write(0, &[pgoff as u8 ^ 0x5A; P as usize]);
        Ok(())
    }
    fn write_run(&self, _first: u64, _pages: &[Arc<CachePage>], _size: u64) -> Result<(), i64> {
        Ok(())
    }
}

fn fill(c: &PageCache, pages: u64) {
    let mut one = [0u8; 1];
    for pg in 0..pages {
        assert_eq!(c.read_at(pg * P, &mut one), Ok(1));
        assert_eq!(one[0], pg as u8 ^ 0x5A);
    }
    assert_eq!(c.pages(), pages);
}

#[test]
fn a_truncate_over_many_steps_leaves_no_page_past_the_size() {
    // More than three steps' worth, and not a multiple of a step.
    let pages = 3 * 1024 + 517;
    let c = PageCache::with_backend(Arc::new(Numbered), pages * P);
    fill(&c, pages);
    c.truncate(5 * P + 100).unwrap();
    assert_eq!(c.pages(), 6, "pages 0..=5 stay");
    assert!(c.cached(6).is_none() && c.cached(1024).is_none() && c.cached(pages - 1).is_none());
    let mut page = [0u8; P as usize];
    assert_eq!(c.read_at(3 * P, &mut page), Ok(P as usize));
    assert!(page.iter().all(|&b| b == 3 ^ 0x5A), "a kept page changed");
    let mut tail = [0xFFu8; 100];
    assert_eq!(c.read_at(5 * P, &mut tail), Ok(100));
    assert!(tail.iter().all(|&b| b == 5 ^ 0x5A), "the cut page lost its head");
    // Growing again shows zeros behind the cut, not the old bytes.
    c.truncate(6 * P).unwrap();
    assert_eq!(c.read_at(5 * P, &mut page), Ok(P as usize));
    assert!(page[..100].iter().all(|&b| b == 5 ^ 0x5A) && page[100..].iter().all(|&b| b == 0), "old bytes behind the cut");
    // Exactly one step, then nothing.
    let c = PageCache::with_backend(Arc::new(Numbered), 1024 * P);
    fill(&c, 1024);
    c.truncate(0).unwrap();
    assert_eq!(c.pages(), 0);
}
