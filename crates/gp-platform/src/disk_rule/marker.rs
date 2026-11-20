//! The kernel's mark that it knows the disk guard (GP-GUARD). The whole rule "no driver for the internal disks"
//! lives in the kernel that is started: a kernel built before `nvme_bind` ignores the loader's forced line and
//! binds the NVMe driver (it identifies the controllers and reads their partition tables). So on a machine whose
//! internal disks are off limits nothing starts a kernel without this mark: bitsboot skips such a generation,
//! install_kernel refuses such a bundle, a Reload refuses such an image.
//!
//! The mark is a byte string in the kernel's read-only data (`kernel/src/config.rs`). Its number goes up when a
//! later guard needs something a kernel with the old mark does not do.

/// What a kernel with the guard carries. Level 1: `nvme_bind=off` keeps every mass-storage function untouched, and
/// the kernel recognises the Gaming-PC's board itself (`tiers::board`).
pub const KERNEL_MARK: &[u8; 28] = b"BITSOS-DISK-GUARD level=1 \xb7\x0a";

/// Whether `bytes` (a KERNEL file, or a part of it) holds the mark.
pub fn has_mark(bytes: &[u8]) -> bool {
    let mut s = Scan::new();
    s.feed(bytes);
    s.found()
}

/// The same search over a file that arrives in pieces (a Reload streams its kernel): the mark may lie across two
/// pieces, so the end of each piece is kept.
#[derive(Clone, Debug)]
pub struct Scan {
    tail: [u8; KERNEL_MARK.len() - 1],
    have: usize,
    found: bool,
}

impl Default for Scan {
    fn default() -> Self {
        Self::new()
    }
}

fn find(hay: &[u8]) -> bool {
    let (first, rest) = (KERNEL_MARK[0], &KERNEL_MARK[1..]);
    let mut at = 0;
    while let Some(i) = hay[at..].iter().position(|&b| b == first) {
        let start = at + i;
        if hay.len() - start < KERNEL_MARK.len() {
            return false;
        }
        if &hay[start + 1..start + KERNEL_MARK.len()] == rest {
            return true;
        }
        at = start + 1;
    }
    false
}

impl Scan {
    pub const fn new() -> Self {
        Scan { tail: [0; KERNEL_MARK.len() - 1], have: 0, found: false }
    }

    pub fn feed(&mut self, piece: &[u8]) {
        if self.found || piece.is_empty() {
            return;
        }
        let keep = KERNEL_MARK.len() - 1;
        // The seam: the kept end of what came before, with the start of this piece.
        if self.have > 0 {
            let mut seam = [0u8; 2 * (KERNEL_MARK.len() - 1)];
            let n = piece.len().min(keep);
            seam[..self.have].copy_from_slice(&self.tail[..self.have]);
            seam[self.have..self.have + n].copy_from_slice(&piece[..n]);
            if find(&seam[..self.have + n]) {
                self.found = true;
                return;
            }
        }
        if find(piece) {
            self.found = true;
            return;
        }
        if piece.len() >= keep {
            self.tail.copy_from_slice(&piece[piece.len() - keep..]);
            self.have = keep;
        } else {
            // A short piece: the old end moves up, the piece goes behind it.
            let old = (self.have + piece.len()).min(keep) - piece.len();
            self.tail.copy_within(self.have - old..self.have, 0);
            self.tail[old..old + piece.len()].copy_from_slice(piece);
            self.have = old + piece.len();
        }
    }

    pub fn found(&self) -> bool {
        self.found
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;

    fn file(mark_at: Option<usize>, len: usize) -> Vec<u8> {
        // Filler full of the mark's first bytes, so a careless search trips.
        let mut v: Vec<u8> = (0..len).map(|i| b"BITSOS-DISK-GUARD level=0 BITS"[i % 30]).collect();
        if let Some(at) = mark_at {
            v[at..at + KERNEL_MARK.len()].copy_from_slice(KERNEL_MARK);
        }
        v
    }

    #[test]
    fn a_kernel_with_and_without_the_mark() {
        assert!(!has_mark(&[]));
        assert!(!has_mark(&KERNEL_MARK[..KERNEL_MARK.len() - 1]));
        assert!(has_mark(KERNEL_MARK));
        assert!(!has_mark(&file(None, 4096)), "near misses are not the mark");
        for at in [0, 1, 27, 28, 1000, 4096 - KERNEL_MARK.len()] {
            assert!(has_mark(&file(Some(at), 4096)), "mark at {at}");
        }
        // One byte off anywhere in the mark: not it.
        for i in 0..KERNEL_MARK.len() {
            let mut f = file(Some(100), 400);
            f[100 + i] ^= 1;
            assert!(!has_mark(&f), "byte {i} changed");
        }
    }

    /// However the file is cut into pieces, the stream finds exactly what the whole-file search finds.
    #[test]
    fn pieces_of_any_size_find_what_the_whole_file_finds() {
        for mark_at in [None, Some(0), Some(5), Some(63), Some(64), Some(200), Some(300 - KERNEL_MARK.len())] {
            let f = file(mark_at, 300);
            for size in 1..=70 {
                let mut s = Scan::new();
                f.chunks(size).for_each(|c| s.feed(c));
                assert_eq!(s.found(), mark_at.is_some(), "mark {mark_at:?} pieces of {size}");
            }
            // Uneven pieces, empty ones between.
            let mut s = Scan::new();
            let mut rest = &f[..];
            for (i, size) in [1usize, 0, 2, 40, 3, 0, 27, 1, 1, 90, 5].iter().cycle().enumerate() {
                if rest.is_empty() || i > 1000 {
                    break;
                }
                let n = (*size).min(rest.len());
                s.feed(&rest[..n]);
                rest = &rest[n..];
            }
            assert_eq!(s.found(), mark_at.is_some(), "mark {mark_at:?} uneven pieces");
        }
    }
}
