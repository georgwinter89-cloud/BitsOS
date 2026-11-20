//! The discards of freed chunks, without the pool lock.
//!
//! A commit that frees chunks discards them on the device below. That used to happen inside the commit with the
//! pool lock held: one synchronous request per run of freed chunks, queued as Background by the block disks below.
//! A delete of a snapshot whose chunks lay apart (an update that patched parts of files, a browser's cache) sent
//! one per chunk, and every read and write of the pool waited for all of them (plan/spec/storage.md §5.2).
//! Under a `PoolHandle` a commit only queues them (`trim`); they stay out of every allocation (`pend_set`, not in
//! `free`) until the handle has discarded them with the lock released, at Normal priority, and gives them back
//! (`PoolHandle::trim_out`: a delete sends what was owed when its commit ended, a snapshot's take a bounded part
//! after its window, `PoolHandle::trim_owed`), as dm-thin keeps freed blocks referenced while their passdown
//! discard is in flight (linux-7/drivers/md/dm-thin.c:1145, `process_prepared_discard_passdown_pt1`/`_pt2`). A
//! pool without a handle (tools, host tests) discards in the commit as before.
//!
//! A discard is a hint: when space runs short the queued chunks are free at once without it (`untrim`), beyond
//! `QUEUE_MAX` a commit's chunks are free at once (`bitsvol.trim_overflow`), and a power cut or a reopen leaves
//! them free and undiscarded. A discard under way is not a hint: the device may still carry it out, so its
//! chunks are given out only after the device answered (`trim_done`). One that failed in a way that does not
//! prove it never reached the device (a timeout: the block layer gives up while the device still holds it) keeps
//! its chunks out until the next open (`trim_lost`, `bitsvol.trim_lost`): a late deallocate on a chunk given out
//! again would zero another volume's data. Each round takes at most a quarter of the reserve (`round_chunks`), and
//! an allocation counts the chunks under way as free space against what it must leave (`Pool::alloc_chunk`): a
//! writer during a round neither drops an automatic snapshot for room nor eats the reserve's last half.

use crate::pool::Pool;
use alloc::vec::Vec;
use devmodel::DevError;

/// Chunks the queue holds at most (512 KiB of chunk numbers): what a commit frees beyond it is free at once.
pub(crate) const QUEUE_MAX: usize = 1 << 16;
/// Chunks one round may have under way, whatever the reserve (1 GiB of 64 KiB chunks).
const ROUND_CHUNKS_MAX: u64 = 1 << 14;

klog::counter!(TRIM_OVERFLOW, "bitsvol.trim_overflow");
klog::counter!(TRIM_LOST, "bitsvol.trim_lost");

/// Runs (first chunk, chunks) of the sorted `chunks`, at most `max` of them and `max_chunks` chunks in all (the
/// last one cut short).
pub(crate) fn runs(chunks: &[u64], max: usize, max_chunks: u64) -> Vec<(u64, u64)> {
    let mut out: Vec<(u64, u64)> = Vec::new();
    for (total, &c) in chunks.iter().enumerate() {
        if total as u64 == max_chunks {
            break;
        }
        if let Some((s, n)) = out.last_mut()
            && *s + *n == c
        {
            *n += 1;
        } else if out.len() == max {
            break;
        } else {
            out.push((c, 1));
        }
    }
    out
}

/// Whether the answer to a discard lets its chunks be given out again: done, or refused before it was sent
/// (no discard on this device, a range the layers below refuse, the write window or a read-only device).
/// Everything else (a timeout, a device gone or reset, a media or transport error) may have left it at the device.
pub(crate) fn settled(r: Result<(), DevError>, chunk: u64) -> bool {
    match r {
        Ok(()) => true,
        Err(e @ (DevError::Unsupported | DevError::InvalidArgument | DevError::Refused)) => {
            klog::event!(Debug, "bitsvol", "discard.failed", chunk = chunk, error = alloc::format!("{e:?}"));
            true
        }
        Err(e) => {
            klog::event!(Warn, "bitsvol", "discard.uncertain", chunk = chunk, error = alloc::format!("{e:?}");
                "the device may still carry it out: its chunks stay out of the pool until the next open");
            false
        }
    }
}

impl Pool {
    /// Commits leave the discards to `take_trim` (a `PoolHandle`'s pool).
    pub(crate) fn set_trim_later(&mut self) {
        self.trim_later = true;
    }

    /// Device blocks of a run of chunks.
    pub(crate) fn run_blocks(&self, (c, n): (u64, u64)) -> (u64, u64) {
        let per = self.cs / self.bs;
        (c * per, n * per)
    }

    /// Chunks under way at most (all rounds together): a quarter of the reserve, so an allocation that counts
    /// them as free (`alloc_chunk`) still leaves three quarters of what it must leave.
    pub(crate) fn round_chunks(&self) -> u64 {
        (self.reserve / 4).clamp(1, ROUND_CHUNKS_MAX)
    }

    /// Chunks queued for their discard, not under way yet.
    pub(crate) fn trim_len(&self) -> u64 {
        self.trim.len() as u64
    }

    /// At most `max` runs and `max_chunks` chunks of the queued chunks, lowest first, within what `round_chunks`
    /// leaves; they stay out of every allocation until `trim_done`.
    pub(crate) fn take_trim(&mut self, max: usize, max_chunks: u64) -> Vec<(u64, u64)> {
        let room = self.round_chunks().saturating_sub(self.trimming).min(max_chunks);
        if self.trim.is_empty() || room == 0 {
            return Vec::new();
        }
        self.trim.sort_unstable();
        let out = runs(&self.trim, max, room);
        let taken: u64 = out.iter().map(|r| r.1).sum();
        self.trim.drain(..taken as usize);
        self.trimming += taken;
        out
    }

    /// The device answered for runs `take_trim` gave: `freed` are free from now on, `lost` (an answer that does
    /// not say the device is done with them, `settled`) stay out of the pool until its next open.
    pub(crate) fn trim_done(&mut self, freed: &[(u64, u64)], lost: &[(u64, u64)]) {
        for &(_, n) in freed.iter().chain(lost) {
            debug_assert!(self.trimming >= n, "{n} chunks back, {} under way", self.trimming);
            self.trimming = self.trimming.saturating_sub(n);
        }
        for &(c, n) in freed {
            for x in c..c + n {
                let was = self.pend_set.remove(&x);
                debug_assert!(was, "chunk {x} came back from its discard but was not held");
            }
            self.free += n;
        }
        self.trim_lose(lost.iter().map(|r| r.1).sum());
        if !freed.is_empty() {
            // As in `release`: a reader that mapped one of them before its free sees the count moved.
            self.reuse_bump();
        }
    }

    /// `n` chunks under way whose answer never came back to the pool (its lock did not come): lost as well.
    pub(crate) fn trim_orphaned(&mut self, n: u64) {
        debug_assert!(self.trimming >= n, "{n} chunks orphaned, {} under way", self.trimming);
        self.trimming = self.trimming.saturating_sub(n);
        self.trim_lose(n);
    }

    /// `n` chunks stay out of the pool until its next open (they are free on the device: a reopen counts them).
    pub(crate) fn trim_lose(&mut self, n: u64) {
        if n != 0 {
            self.trim_lost += n;
            TRIM_LOST.add(n);
        }
    }

    /// Freed chunks become free (their discard sent, or none wanted).
    pub(crate) fn trimmed_chunks(&mut self, chunks: &[u64]) {
        for c in chunks {
            self.pend_set.remove(c);
        }
        self.free += chunks.len() as u64;
    }

    /// What `release` frees in the commit: discarded first by a pool without a handle, queued under one. Chunks
    /// whose inline discard did not settle stay out as `trim_done` keeps them.
    pub(crate) fn trim_freed(&mut self, mut gone: Vec<u64>) {
        if !self.discard_ok || gone.is_empty() {
            self.trimmed_chunks(&gone);
        } else if self.trim_later {
            let room = self.trim_max.saturating_sub(self.trim.len());
            let (queued, over) = gone.split_at(gone.len().min(room));
            self.trim.extend_from_slice(queued);
            if !over.is_empty() {
                TRIM_OVERFLOW.add(over.len() as u64);
            }
            self.trimmed_chunks(over);
        } else {
            gone.sort_unstable();
            let mut free = Vec::new();
            for run in runs(&gone, usize::MAX, u64::MAX) {
                let (lba, blocks) = self.run_blocks(run);
                if settled(self.dev.discard(lba, blocks), run.0) {
                    free.extend(run.0..run.0 + run.1);
                } else {
                    self.trim_lose(run.1);
                }
            }
            self.trimmed_chunks(&free);
        }
    }

    /// Space is short: the queued chunks are free now, undiscarded (none of them is under way). True when there
    /// were any.
    pub(crate) fn untrim(&mut self) -> bool {
        if self.trim.is_empty() {
            return false;
        }
        let q = core::mem::take(&mut self.trim);
        self.trimmed_chunks(&q);
        self.reuse_bump();
        true
    }

    /// The first chunk from `c` on that no allocation is held back from (`pend_set`): one walk over a run of
    /// held chunks instead of one lookup per chunk.
    pub(crate) fn past_held(&self, c: u64) -> u64 {
        let mut end = c;
        for &x in self.pend_set.range(c..) {
            if x != end {
                break;
            }
            end += 1;
        }
        end
    }

    /// Chunks queued for their discard or under way (host tests).
    #[cfg(test)]
    pub(crate) fn trim_queued(&self) -> u64 {
        self.trim.len() as u64 + self.trimming
    }

    /// A smaller queue bound (host tests of the overflow).
    #[cfg(test)]
    pub(crate) fn set_trim_max(&mut self, n: usize) {
        self.trim_max = n;
    }
}

#[cfg(test)]
mod tests {
    use super::{runs, settled};
    use devmodel::DevError;

    #[test]
    fn runs_join_neighbours_and_stop_at_the_bounds() {
        assert_eq!(runs(&[1, 2, 3, 7, 9, 10], usize::MAX, u64::MAX), [(1, 3), (7, 1), (9, 2)]);
        assert_eq!(runs(&[1, 2, 3, 7, 9, 10], 2, u64::MAX), [(1, 3), (7, 1)]);
        assert_eq!(runs(&[1, 2, 3, 7, 9, 10], usize::MAX, 2), [(1, 2)]);
        assert_eq!(runs(&[1, 2, 3, 7, 9, 10], usize::MAX, 5), [(1, 3), (7, 1), (9, 1)]);
        assert!(runs(&[], 4, 4).is_empty());
        assert!(runs(&[5], 4, 0).is_empty());
    }

    #[test]
    fn only_a_refusal_before_the_device_settles_a_failed_discard() {
        assert!(settled(Ok(()), 0));
        for e in [DevError::Unsupported, DevError::InvalidArgument, DevError::Refused] {
            assert!(settled(Err(e), 0), "{e:?}");
        }
        for e in [DevError::Timeout, DevError::Io, DevError::Gone, DevError::NoMemory, DevError::NoSpace] {
            assert!(!settled(Err(e), 0), "{e:?}");
        }
    }
}
