//! kswapd's high-order side: the buddy query for a free block of an order and the orders noted for it (the blocks
//! themselves are made by compaction since W35 MM-3: tests/compact.rs).
use super::*;
use crate::OrderWant;

#[test]
fn the_query_sees_blocks_of_the_order_or_more() {
    let b = staged(&[free(MIB, 5 * MIB)]);
    assert!(b.has_free_block(8) && b.has_free_block(0));
    assert!(!b.has_free_block(11), "4 MiB hold no block of 8 MiB");
    // Everything in order-2 blocks, then one of them back.
    let blocks: Vec<u64> = core::iter::from_fn(|| b.alloc(2, Zone::Normal, Kind::Other)).collect();
    assert!(!b.has_free_block(0), "nothing free");
    b.free(blocks[5], 2, Kind::Other);
    assert!(b.has_free_block(0) && b.has_free_block(2));
    assert!(!b.has_free_block(3), "its buddy is taken");
    b.free(blocks[4], 2, Kind::Other);
    assert!(b.has_free_block(3), "merged with its buddy");
    for &p in &blocks[..4] {
        b.free(p, 2, Kind::Other);
    }
    for &p in &blocks[6..] {
        b.free(p, 2, Kind::Other);
    }
    b.check().unwrap();
}

#[test]
fn every_wanted_order_stays_until_taken() {
    let w = OrderWant::new();
    assert_eq!(w.pending(), 0);
    w.note(0);
    assert_eq!((w.pending(), w.noted()), (0, 0), "order 0 is the free count's business");
    w.note(2);
    w.note(8);
    w.note(2);
    assert_eq!((w.pending(), w.noted()), (1 << 2 | 1 << 8, 3));
    assert_eq!(w.take(), 1 << 2 | 1 << 8);
    assert_eq!(w.take(), 0);
    w.keep(4);
    w.note(5);
    assert_eq!((w.take(), w.noted()), (1 << 4 | 1 << 5, 4), "kswapd's keep is not counted");
    w.note(crate::MAX_WANTED + 1);
    assert_eq!((w.take(), w.noted()), (0, 4), "above Linux's MAX_PAGE_ORDER nobody is asked");
}
