//! Fence timelines: 64-bit sequence numbers that the engine writes to a writeback slot after a submission.
//!
//! Linux keeps 32-bit sequence numbers and compares them masked (amdgpu_fence.c:273-299). BitsOS keeps the
//! whole 64 bits (one timeline per ring or context, GX-D4) and still compares with wrapping arithmetic, so a
//! timeline may start anywhere, also just below 2^64 (the tests do). A value read back is accepted only if it
//! lies in the window of emitted, not yet signalled numbers: an older value (a torn 64-bit read, a stale
//! slot) is ignored, a value beyond the last emitted one is garbage (a hang dump's business). The number of
//! fences in flight is the caller's choice (Linux: num_hw_submission * 2, amdgpu_fence.c:526).

/// Why a fence could not be emitted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FenceError {
    /// `max_in_flight` fences are outstanding; wait for one.
    Full,
}

impl FenceError {
    pub fn errno(&self) -> i32 {
        crate::errno::EBUSY
    }
}

/// What a value read from the fence slot meant.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Observed {
    /// The timeline moved from `from` (exclusive) to `to` (inclusive).
    Advanced { from: u64, to: u64 },
    /// Same as the last signalled value.
    Unchanged,
    /// Older than the last signalled value (torn read or stale slot): ignored.
    Stale(u64),
    /// Beyond the last emitted value: the slot holds garbage.
    Ahead(u64),
}

/// One timeline.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Timeline {
    emitted: u64,
    signalled: u64,
    max_in_flight: u64,
}

impl Timeline {
    /// A timeline whose slot was initialised to `start` (nothing in flight). `max_in_flight` >= 1.
    pub fn new(start: u64, max_in_flight: u64) -> Timeline {
        Timeline { emitted: start, signalled: start, max_in_flight: max_in_flight.max(1) }
    }
    pub fn emitted(&self) -> u64 {
        self.emitted
    }
    pub fn signalled(&self) -> u64 {
        self.signalled
    }
    pub fn in_flight(&self) -> u64 {
        self.emitted.wrapping_sub(self.signalled)
    }
    /// The next sequence number (wrapping past 2^64 - 1 to 0).
    pub fn emit(&mut self) -> Result<u64, FenceError> {
        if self.in_flight() >= self.max_in_flight {
            return Err(FenceError::Full);
        }
        self.emitted = self.emitted.wrapping_add(1);
        Ok(self.emitted)
    }
    /// Takes back the last emitted number (the submission was undone before the engine saw it).
    pub fn unemit(&mut self) -> bool {
        if self.in_flight() == 0 {
            return false;
        }
        self.emitted = self.emitted.wrapping_sub(1);
        true
    }
    /// Feeds a value read from the slot.
    pub fn observe(&mut self, value: u64) -> Observed {
        let d = value.wrapping_sub(self.signalled);
        if d == 0 {
            return Observed::Unchanged;
        }
        if d <= self.in_flight() {
            let from = self.signalled;
            self.signalled = value;
            return Observed::Advanced { from, to: value };
        }
        if (d as i64) < 0 { Observed::Stale(value) } else { Observed::Ahead(value) }
    }
    /// Whether `seq` (emitted on this timeline) has signalled. Numbers never emitted are not signalled.
    pub fn is_signalled(&self, seq: u64) -> bool {
        let back = self.signalled.wrapping_sub(seq);
        let ahead = seq.wrapping_sub(self.signalled);
        if ahead != 0 && ahead <= self.in_flight() {
            return false;
        }
        // Older than or equal to the signalled value; anything beyond the emitted window is treated as old
        // only within half the number space.
        (back as i64) >= 0
    }
    /// Forces every emitted number signalled (after a reset that dropped the work; Linux
    /// amdgpu_fence_driver_force_completion). Returns the range that completed this way.
    pub fn force_complete(&mut self) -> Observed {
        if self.in_flight() == 0 {
            return Observed::Unchanged;
        }
        let from = self.signalled;
        self.signalled = self.emitted;
        Observed::Advanced { from, to: self.emitted }
    }
}

/// Which half of a 64-bit fence slot [`read_split`] asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Half {
    Low,
    High,
}

/// Reads a 64-bit fence value that the engine writes as two dwords, low first (SDMA's two FENCE packets).
/// Reads high, low, high; if the high half changed in between, reads the low half again under the new high
/// half. A value that is still torn (low wrapped and high not yet written) comes out older than the truth,
/// and [`Timeline::observe`] ignores it until the next poll.
pub fn read_split(mut read: impl FnMut(Half) -> u32) -> u64 {
    let hi1 = read(Half::High);
    let lo = read(Half::Low);
    let hi2 = read(Half::High);
    if hi1 == hi2 {
        return ((hi1 as u64) << 32) | lo as u64;
    }
    let lo2 = read(Half::Low);
    ((hi2 as u64) << 32) | lo2 as u64
}
