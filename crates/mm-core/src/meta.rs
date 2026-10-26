//! Per-frame state, 16 bits per 4 KiB frame packed four to a word of the metadata blob.
//!
//! Only the first frame of a block (its head) carries a state; every other frame reads `NONE`. That makes
//! split, merge, alloc and free O(1) in the block size and lets `free` refuse a pointer into the middle of a
//! block, a frame that is already free or cached, and a wrong order (double-free detection without trusting
//! the caller). Writes under the zone lock use a CAS loop because the per-CPU lists change heads of the same
//! word without that lock.

use core::sync::atomic::{AtomicU64, Ordering};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    /// Not a block head (inside a block, or never part of the pool).
    None,
    /// Head of a free block in the buddy lists.
    Free,
    /// Head of a block handed out.
    Alloc,
    /// An order-0 frame parked in a per-CPU list (free for the stats, allocated for the buddy).
    Cached,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Meta {
    pub state: State,
    pub order: u32,
    pub kind: usize,
    /// Who an allocated block belongs to beyond its kind (`TAG_*`); 0 for every block outside the heap.
    pub tag: u8,
}

/// A plain block.
pub const TAG_NONE: u8 = 0;
/// A slab of the heap: only the heap frees it (`Buddy::free_tagged`), so a pointer into a slab freed as a page
/// block or as a large heap run is refused.
pub const TAG_SLAB: u8 = 1;
/// A piece after the first of an exact run (`alloc_exact`): only `free_exact` of the whole run frees it.
pub const TAG_TAIL: u8 = 2;
/// A piece `free_exact` has claimed and is about to free (makes check-and-free of a run one step).
pub const TAG_BUSY: u8 = 3;

const ORDER_MASK: u16 = 0x3F;
const KIND_SHIFT: u16 = 6;
const KIND_MASK: u16 = 0x3F;
const TAG_SHIFT: u16 = 12;
const TAG_MASK: u16 = 0x3;
const STATE_SHIFT: u16 = 14;

impl Meta {
    pub const NONE: Meta = Meta { state: State::None, order: 0, kind: 0, tag: TAG_NONE };

    pub fn new(state: State, order: u32, kind: usize) -> Meta {
        Meta { state, order, kind, tag: TAG_NONE }
    }

    pub fn with_tag(self, tag: u8) -> Meta {
        Meta { tag, ..self }
    }

    pub fn encode(self) -> u16 {
        let st = match self.state {
            State::None => return 0,
            State::Free => 1u16,
            State::Alloc => 2,
            State::Cached => 3,
        };
        st << STATE_SHIFT
            | (self.tag as u16 & TAG_MASK) << TAG_SHIFT
            | (self.kind as u16 & KIND_MASK) << KIND_SHIFT
            | (self.order as u16 & ORDER_MASK)
    }

    pub fn decode(v: u16) -> Meta {
        let state = match v >> STATE_SHIFT {
            1 => State::Free,
            2 => State::Alloc,
            3 => State::Cached,
            _ => return Meta::NONE,
        };
        Meta {
            state,
            order: (v & ORDER_MASK) as u32,
            kind: ((v >> KIND_SHIFT) & KIND_MASK) as usize,
            tag: ((v >> TAG_SHIFT) & TAG_MASK) as u8,
        }
    }
}

/// Words needed for `frames` entries.
pub fn words_for(frames: u64) -> usize {
    frames.div_ceil(4) as usize
}

fn slot(i: u64) -> (usize, u32) {
    ((i / 4) as usize, (i % 4) as u32 * 16)
}

pub fn get(words: &[AtomicU64], i: u64) -> Meta {
    let (w, sh) = slot(i);
    Meta::decode((words[w].load(Ordering::Acquire) >> sh) as u16)
}

pub fn set(words: &[AtomicU64], i: u64, m: Meta) {
    let (w, sh) = slot(i);
    let v = m.encode() as u64;
    let _ = words[w].try_update(Ordering::AcqRel, Ordering::Acquire, |old| Some(old & !(0xFFFF << sh) | v << sh));
}

/// Replaces entry `i` with `new` only if it is `expect`; on failure returns what was there.
pub fn cas(words: &[AtomicU64], i: u64, expect: Meta, new: Meta) -> Result<(), Meta> {
    let (w, sh) = slot(i);
    let (e, n) = (expect.encode() as u64, new.encode() as u64);
    words[w]
        .try_update(Ordering::AcqRel, Ordering::Acquire, |old| (old >> sh & 0xFFFF == e).then_some(old & !(0xFFFF << sh) | n << sh))
        .map(|_| ())
        .map_err(|old| Meta::decode((old >> sh) as u16))
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;

    #[test]
    fn round_trip_and_neighbours() {
        let w: std::vec::Vec<AtomicU64> = (0..3).map(|_| AtomicU64::new(0)).collect();
        for (i, m) in [Meta::new(State::Free, 18, 0), Meta::new(State::Alloc, 0, 7), Meta::new(State::Cached, 0, 3), Meta::new(State::Alloc, 63, 63), Meta::new(State::Alloc, 5, 7).with_tag(TAG_BUSY)]
            .into_iter()
            .enumerate()
        {
            set(&w, i as u64 + 2, m);
            assert_eq!(get(&w, i as u64 + 2), m);
        }
        assert_eq!(get(&w, 0), Meta::NONE);
        assert_eq!(get(&w, 1), Meta::NONE);
        let a = Meta::new(State::Alloc, 0, 7);
        assert_eq!(cas(&w, 3, Meta::NONE, a), Err(a));
        assert_eq!(cas(&w, 3, a, Meta::new(State::Cached, 0, 0)), Ok(()));
        assert_eq!(get(&w, 2), Meta::new(State::Free, 18, 0));
        set(&w, 2, Meta::NONE);
        assert_eq!(get(&w, 2), Meta::NONE);
        assert_eq!(get(&w, 4), Meta::new(State::Cached, 0, 3));
    }
}
