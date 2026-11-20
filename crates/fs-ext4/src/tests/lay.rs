//! The power cut the cut tests lay over an image: what a disk with a volatile cache has kept of a write log
//! (`dev::WriteLog`). Since FS-W the journal's commit block is written through alone, not behind a second flush
//! of everything: what other threads wrote between a commit's flush and its commit block can be lost with the
//! commit block kept, and the cuts here show exactly that.

use crate::dev::{is_mark, WriteLog, FLUSHED, THROUGH};
use std::vec::Vec;

pub struct Rng(pub u64);

impl Rng {
    pub fn bit(&mut self) -> bool {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 & 1 == 1
    }
}

/// The image after a cut behind the first `k` entries of `log`, laid over `before`: every write before the last
/// flush, every write that went through (the entry before a `THROUGH` mark), and of all others each `bs`-byte
/// piece only when `kept` says so (None: all of them, the plain prefix). Returns the image and the index behind
/// the last flush.
pub fn cut_image(before: &[u8], log: &WriteLog, k: usize, bs: usize, mut kept: Option<&mut dyn FnMut() -> bool>) -> (Vec<u8>, usize) {
    let log = &log[..k];
    let flush = log.iter().rposition(|(a, _)| *a == FLUSHED).map_or(0, |i| i + 1);
    let mut img = before.to_vec();
    for (i, (at, d)) in log.iter().enumerate() {
        if is_mark(*at) {
            continue;
        }
        let stable = i < flush || log.get(i + 1).is_some_and(|n| n.0 == THROUGH);
        for (c, piece) in d.chunks(bs).enumerate() {
            if !stable && kept.as_mut().is_some_and(|f| !f()) {
                continue;
            }
            let a = *at as usize + c * bs;
            img[a..a + piece.len()].copy_from_slice(piece);
        }
    }
    (img, flush)
}

/// The index behind the last barrier of `log` (a flush or a write that went through): the cut right after a
/// commit, before its home writes.
pub fn last_barrier(log: &WriteLog) -> usize {
    log.iter().rposition(|(a, _)| is_mark(*a)).expect("no barrier in the log")
}
