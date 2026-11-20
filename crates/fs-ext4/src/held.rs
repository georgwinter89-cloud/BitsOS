//! FS-W2: on a volume without a journal, blocks that were freed are not handed out again before a device flush made
//! the free durable. With a journal the journal does that (a freed block is busy until the commit that frees it,
//! `journal::State::join`). Without one there was nothing: the delete's blocks (pointer, bitmap) wait in the disk's
//! cache for the next flush, and since FS-W2 a new file's data goes to the disk at once, so a power cut showed the
//! deleted file, still whole on the disk, with the new file's bytes in it. (Linux without a journal has that
//! window; `flight`'s promise for this layout, "never a pointer at data not written", needs it closed.)
//!
//! The ranges are kept in the order of their frees. A flush lets go of what was freed before it began
//! (`mark` under the volume lock before the flush, `release` after it), so a free that came while the flush ran
//! waits for the next. Who flushes: fsync and sync (`Volume::flush`, `sync_fs`), the background pass every few
//! seconds, and the allocator itself when only held blocks are left (`State::free_run`, as it commits the journal
//! for busy ones). The blocks count as free meanwhile, as the journal's busy blocks do.

use crate::errno::ENOMEM;
use alloc::vec::Vec;

#[derive(Default)]
pub struct Held {
    /// start -> end (exclusive), oldest first; one entry per free (R1: as many as were freed since the last flush).
    ranges: Vec<(u64, u64)>,
    /// How many ranges were let go since the mount: the number of `ranges[0]`.
    base: u64,
}

impl Held {
    pub fn is_empty(&self) -> bool {
        self.ranges.is_empty()
    }
    pub(crate) fn add(&mut self, start: u64, n: u64) -> Result<(), i64> {
        self.ranges.try_reserve(1).map_err(|_| ENOMEM)?;
        self.ranges.push((start, start + n));
        Ok(())
    }
    pub(crate) fn within(&self, start: u64, end: u64) -> impl Iterator<Item = (u64, u64)> + '_ {
        self.ranges.iter().copied().filter(move |&(s, e)| s < end && e > start)
    }
    /// Before a device flush, under the volume lock: what the flush will cover (None: nothing is held).
    pub fn mark(&self) -> Option<u64> {
        (!self.ranges.is_empty()).then(|| self.base + self.ranges.len() as u64)
    }
    /// The flush that began at `mark` succeeded.
    pub fn release(&mut self, mark: u64) {
        let n = (mark.saturating_sub(self.base) as usize).min(self.ranges.len());
        self.ranges.drain(..n);
        self.base += n as u64;
        if self.ranges.is_empty() {
            self.ranges.shrink_to_fit();
        }
    }
}
