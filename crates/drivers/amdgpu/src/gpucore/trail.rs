//! GX-10: what the device saw from a bring-up owner, in order: every read, every write, each poll of a register as
//! ONE wait with its condition and its bound (`psp::Hw::wait_begin`), and each sleep of a millisecond or more.
//! `rdna4::linuxseq` compares a stage's trail with the register trace of Linux's own start of the card, so it holds
//! bus accesses and the silences between them only (memory and doorbells are not register accesses, and Linux's
//! trace has none of them).
//!
//! An owner has no trail unless it is asked for one (`Bringup::with_trail`): the 780M's running owner keeps none.

use super::psp::{Bound, Cond};
use alloc::collections::VecDeque;

/// A delay of at least this long outside a poll is a sleep the device can tell (Linux's `mdelay(20)` handshakes,
/// psp_v14_0.c:229-230); shorter ones are poll steps (a fence polled in memory every 10 to 100 us).
pub const SLEEP_US: u64 = 1000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Access {
    /// `value` is `None` when the read failed.
    Read { byte: u32, value: Option<u32> },
    /// `reached` is false when the write never got to the device (a shut gate, a guard or a fake that refused).
    Write { byte: u32, value: u32, reached: bool },
    /// `reads` reads of `byte` in `us`, the last one `last`; the poll would have given up at `bound`.
    Wait { byte: u32, cond: Cond, bound: Bound, reads: u32, last: Option<u32>, us: u64 },
    /// No access for `us` because the owner was told to wait (delays in a row are one sleep).
    Sleep { us: u64 },
}

pub struct Trail {
    keep: usize,
    recent: VecDeque<Access>,
    total: u64,
    /// The running wait: its register, its number in `total`'s count, when it began.
    open: Option<(u32, u64, u64)>,
}

impl Trail {
    pub fn new(keep: usize) -> Trail {
        Trail { keep: keep.max(1), recent: VecDeque::new(), total: 0, open: None }
    }

    fn push(&mut self, a: Access) {
        if self.recent.len() == self.keep {
            self.recent.pop_front();
        }
        self.recent.push_back(a);
        self.total += 1;
    }

    pub(super) fn read(&mut self, byte: u32, value: Option<u32>) {
        if let Some((b, n, _)) = self.open
            && b == byte
            && let Some(Access::Wait { reads, last, .. }) = self.entry(n)
        {
            *reads += 1;
            *last = value;
            return;
        }
        self.push(Access::Read { byte, value });
    }

    pub(super) fn write(&mut self, byte: u32, value: u32, reached: bool) {
        self.push(Access::Write { byte, value, reached });
    }

    pub(super) fn begin(&mut self, byte: u32, cond: Cond, bound: Bound, now_us: u64) {
        self.open = Some((byte, self.total, now_us));
        self.push(Access::Wait { byte, cond, bound, reads: 0, last: None, us: 0 });
    }

    /// A delay: inside a poll it is the poll's own step.
    pub(super) fn delay(&mut self, us: u64) {
        if self.open.is_some() || us < SLEEP_US {
            return;
        }
        if let Some(Access::Sleep { us: sum }) = self.recent.back_mut() {
            *sum = sum.saturating_add(us);
            return;
        }
        self.push(Access::Sleep { us });
    }

    pub(super) fn end(&mut self, now_us: u64) {
        if let Some((_, n, start)) = self.open.take()
            && let Some(Access::Wait { us, .. }) = self.entry(n)
        {
            *us = now_us.saturating_sub(start);
        }
    }

    /// The kept entry that was pushed as number `n`.
    fn entry(&mut self, n: u64) -> Option<&mut Access> {
        let back = usize::try_from(self.total.checked_sub(n + 1)?).ok()?;
        let at = self.recent.len().checked_sub(back + 1)?;
        self.recent.get_mut(at)
    }

    /// The kept accesses, oldest first.
    pub fn recent(&self) -> impl Iterator<Item = &Access> {
        self.recent.iter()
    }

    /// Accesses that fell out of the kept ones: a trail that lost any is no proof of a sequence.
    pub fn lost(&self) -> u64 {
        self.total - self.recent.len() as u64
    }
}
