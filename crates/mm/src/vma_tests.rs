extern crate std;
use super::*;
use std::vec::Vec;

const P: u64 = 4096;
const RW: Prot = Prot { read: true, write: true, exec: false };
const RO: Prot = Prot { read: true, write: false, exec: false };

fn anon(start: u64, end: u64, prot: Prot) -> Vma {
    Vma::anon(start, end, prot, VmaKind::Anonymous)
}

fn ranges(t: &VmaTree) -> Vec<(u64, u64, Prot)> {
    t.iter().map(|v| (v.start, v.end, v.prot)).collect()
}

fn check(t: &VmaTree) {
    for w in t.v.windows(2) {
        assert!(w[0].end <= w[1].start, "overlap: {w:?}");
        assert!(!w[0].joins(&w[1]), "unmerged neighbours: {w:?}");
    }
    assert!(t.iter().all(|v| v.start < v.end));
    assert_eq!(t.pages(), t.iter().map(|v| v.pages()).sum::<u64>(), "kept total = sum of the VMAs");
    assert!(t.peak_pages() >= t.pages());
}

#[test]
fn total_and_peak_follow_insert_remove_split() {
    let mut t = VmaTree::new();
    t.insert(anon(10 * P, 20 * P, RW)).unwrap();
    t.insert(anon(20 * P, 30 * P, RW)).unwrap();
    assert_eq!((t.pages(), t.peak_pages()), (20, 20));
    t.protect(12 * P, 14 * P, RO).unwrap();
    assert_eq!(t.pages(), 20, "a split changes no size");
    check(&t);
    assert!(t.remove(11 * P, 13 * P).unwrap());
    assert_eq!((t.pages(), t.peak_pages()), (18, 20), "VmPeak keeps the high-water mark");
    check(&t);
    t.insert(anon(40 * P, 45 * P, RW)).unwrap();
    assert_eq!((t.pages(), t.peak_pages()), (23, 23));
    t.restore_peak(20);
    assert_eq!(t.peak_pages(), 23, "a restored peak is never below the current size");
    assert!(t.remove(0, 50 * P).unwrap());
    t.restore_peak(21);
    assert_eq!((t.pages(), t.peak_pages()), (0, 21));
    check(&t);
}

#[test]
fn insert_merges_equal_neighbours_only() {
    let mut t = VmaTree::new();
    t.insert(anon(10 * P, 20 * P, RW)).unwrap();
    t.insert(anon(30 * P, 40 * P, RW)).unwrap();
    t.insert(anon(20 * P, 30 * P, RW)).unwrap();
    assert_eq!(ranges(&t), [(10 * P, 40 * P, RW)]);
    t.insert(anon(40 * P, 41 * P, RO)).unwrap();
    t.insert(Vma::anon(41 * P, 42 * P, RO, VmaKind::Heap)).unwrap();
    assert_eq!(t.len(), 3, "other prot or kind stays separate");
    check(&t);
}

#[test]
fn find_and_overlap_queries() {
    let mut t = VmaTree::new();
    t.insert(anon(10 * P, 20 * P, RW)).unwrap();
    t.insert(anon(30 * P, 40 * P, RO)).unwrap();
    assert_eq!(t.find(10 * P).map(|v| v.start), Some(10 * P));
    assert_eq!(t.find(20 * P), None, "end is exclusive");
    assert_eq!(t.find(25 * P), None);
    assert_eq!(t.next_from(25 * P).map(|v| v.start), Some(30 * P));
    assert_eq!(t.overlapping(15 * P, 35 * P).len(), 2);
    assert!(t.is_free(20 * P, 30 * P));
    assert!(!t.is_free(19 * P, 21 * P));
    assert_eq!(t.covered_from(12 * P, 35 * P), 20 * P, "stops at the hole");
    assert_eq!(t.covered_from(25 * P, 35 * P), 25 * P, "not mapped at the start");
    assert_eq!(t.covered_from(32 * P, 35 * P), 35 * P);
}

#[test]
fn remove_splits_and_cuts() {
    let mut t = VmaTree::new();
    t.insert(anon(10 * P, 20 * P, RW)).unwrap();
    assert!(t.remove(14 * P, 16 * P).unwrap());
    assert_eq!(ranges(&t), [(10 * P, 14 * P, RW), (16 * P, 20 * P, RW)]);
    assert!(t.remove(12 * P, 18 * P).unwrap(), "across the hole");
    assert_eq!(ranges(&t), [(10 * P, 12 * P, RW), (18 * P, 20 * P, RW)]);
    assert!(!t.remove(13 * P, 17 * P).unwrap(), "nothing there");
    assert!(t.remove(0, 100 * P).unwrap());
    assert!(t.is_empty());
}

#[test]
fn protect_splits_and_merges_back() {
    let mut t = VmaTree::new();
    t.insert(anon(10 * P, 20 * P, RW)).unwrap();
    t.protect(12 * P, 13 * P, RO).unwrap();
    assert_eq!(ranges(&t), [(10 * P, 12 * P, RW), (12 * P, 13 * P, RO), (13 * P, 20 * P, RW)]);
    t.protect(15 * P, 16 * P, Prot::NONE).unwrap();
    assert_eq!(t.len(), 5);
    t.protect(10 * P, 20 * P, RW).unwrap();
    assert_eq!(ranges(&t), [(10 * P, 20 * P, RW)], "merged into one again");
    check(&t);
}

#[test]
fn gaps_top_down_and_bottom_up() {
    let mut t = VmaTree::new();
    assert_eq!(t.gap_topdown(4 * P, P, 100 * P), Some(96 * P));
    t.insert(anon(90 * P, 100 * P, RW)).unwrap();
    t.insert(anon(80 * P, 85 * P, RW)).unwrap();
    assert_eq!(t.gap_topdown(5 * P, P, 100 * P), Some(85 * P), "the 5-page hole between them");
    assert_eq!(t.gap_topdown(6 * P, P, 100 * P), Some(74 * P));
    assert_eq!(t.gap_topdown(6 * P, P, 88 * P), Some(74 * P), "hi inside a gap");
    assert_eq!(t.gap_topdown(3 * P, P, 95 * P), Some(87 * P), "hi inside a mapping");
    assert_eq!(t.gap_topdown(80 * P, P, 100 * P), None);
    assert_eq!(t.gap_topdown(79 * P, P, 100 * P), Some(P));
    assert_eq!(t.gap_bottomup(5 * P, 0, 100 * P), Some(0));
    assert_eq!(t.gap_bottomup(5 * P, 79 * P, 100 * P), Some(85 * P));
    assert_eq!(t.gap_bottomup(6 * P, 79 * P, 100 * P), None);
    assert_eq!(t.gap_topdown(0, P, 100 * P), None);
}

#[test]
fn random_operations_keep_the_invariants() {
    let mut t = VmaTree::new();
    let mut pages = [0u8; 256];
    let mut x = 0x2545_F491_4F6C_DD1Du64;
    let prots = [Prot::NONE, RO, RW];
    for _ in 0..20_000 {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        let a = (x >> 8) % 256;
        let b = (a + 1 + (x >> 20) % 16).min(256);
        let p = ((x >> 40) % 3) as usize;
        match x % 3 {
            0 => {
                if t.is_free(a * P, b * P) {
                    t.insert(anon(a * P, b * P, prots[p])).unwrap();
                    pages[a as usize..b as usize].fill(p as u8 + 1);
                }
            }
            1 => {
                t.remove(a * P, b * P).unwrap();
                pages[a as usize..b as usize].fill(0);
            }
            _ => {
                let e = t.covered_from(a * P, b * P);
                if e > a * P {
                    t.protect(a * P, e, prots[p]).unwrap();
                    pages[a as usize..(e / P) as usize].fill(p as u8 + 1);
                }
            }
        }
        check(&t);
    }
    for (i, &want) in pages.iter().enumerate() {
        let got = t.find(i as u64 * P).map(|v| prots.iter().position(|q| *q == v.prot).unwrap() as u8 + 1).unwrap_or(0);
        assert_eq!(got, want, "page {i}");
    }
}

/// A file mapping object for the tree tests (FS-5): no pages, only an identity.
struct NoPages;
impl vfs::mmap::MapBacking for NoPages {
    fn get_page(&self, _pgoff: u64, _write: bool) -> Result<u64, vfs::pagecache::PageFault> {
        Err(vfs::pagecache::PageFault::Bus)
    }
    fn put_page(&self, _phys: u64) -> bool {
        false
    }
    fn shared(&self) -> bool {
        true
    }
    fn track_dirty(&self) -> bool {
        false
    }
    fn sync(&self, _first: u64, _last: u64) -> Result<(), i64> {
        Ok(())
    }
}

fn file(start: u64, end: u64, b: &alloc::sync::Arc<dyn vfs::mmap::MapBacking>, pgoff: u64, may_write: bool) -> Vma {
    Vma { start, end, prot: RW, kind: VmaKind::File, file: Some(FileRef::file(b.clone(), pgoff, true, may_write)), flags: Default::default() }
}

/// FS-5: splitting a file VMA keeps each piece's file offset; neighbours merge only for the same mapping
/// object with contiguous file pages.
#[test]
fn file_vmas_split_and_merge() {
    let a: alloc::sync::Arc<dyn vfs::mmap::MapBacking> = alloc::sync::Arc::new(NoPages);
    let b: alloc::sync::Arc<dyn vfs::mmap::MapBacking> = alloc::sync::Arc::new(NoPages);
    let mut t = VmaTree::new();
    t.insert(file(10 * P, 20 * P, &a, 5, true)).unwrap();
    t.remove(12 * P, 13 * P).unwrap();
    let v: Vec<(u64, u64)> = t.iter().map(|v| (v.start, v.pgoff_at(v.start).unwrap())).collect();
    assert_eq!(v, [(10 * P, 5), (13 * P, 8)], "the right piece starts at file page 5 + 3");
    assert_eq!(t.find(19 * P).unwrap().pgoff_at(19 * P), Some(14));
    t.protect(15 * P, 16 * P, RO).unwrap();
    t.protect(15 * P, 16 * P, RW).unwrap();
    assert_eq!(t.len(), 2, "split by mprotect and merged back with contiguous file pages");
    // Contiguous addresses but not contiguous file pages, or another object: separate.
    t.insert(file(20 * P, 21 * P, &a, 99, true)).unwrap();
    t.insert(file(21 * P, 22 * P, &b, 100, true)).unwrap();
    t.insert(file(12 * P, 13 * P, &a, 7, true)).unwrap();
    assert_eq!(t.len(), 3, "the hole filled with the right file page merges all three pieces of a: {:?}", t.to_vec());
    t.insert(anon(22 * P, 23 * P, RW)).unwrap();
    assert_eq!(t.len(), 4, "anonymous memory never merges with a file VMA");
    check(&t);
}
