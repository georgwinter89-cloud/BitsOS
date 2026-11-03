//! What programs are told about a reset: the device's counters, a context's snapshot of them, and how a job ends.
//!
//! Linux tells contexts, not file descriptors: `AMDGPU_CTX_OP_QUERY_STATE2` compares the context's snapshot with
//! the device (amdgpu_ctx.c:574-630), and a fd learns of a lost VRAM only through its contexts and its VM's
//! generation. DR's crates/drm/src/ctx.rs holds the same comparison on the live objects.
//!
//! BitsOS adds one thing, a queue-scope recovery: it blames one context and must not tell the others of a reset, so
//! the device's counter stays and [`Counters::blame`] moves that context's snapshot instead. The flags stay Linux's.

use crate::errno;
use crate::watch::{Pending, RingId};

/// `AMDGPU_CTX_QUERY2_FLAGS_*` (include/uapi/drm/amdgpu_drm.h:245-254). No RAS on the 780M.
pub mod query2 {
    pub const RESET: u64 = 1 << 0;
    pub const VRAMLOST: u64 = 1 << 1;
    pub const GUILTY: u64 = 1 << 2;
    pub const RESET_IN_PROGRESS: u64 = 1 << 5;
}

/// `gpu_reset_counter`, `vram_lost_counter`, `amdgpu_in_reset`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counters {
    pub reset: u32,
    pub vram_lost: u32,
    pub in_reset: bool,
}

/// What a context took at creation, and its guilty flag.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ContextState {
    pub reset_counter: u32,
    pub generation: u32,
    pub guilty: bool,
}

impl Counters {
    pub fn new_context(&self) -> ContextState {
        ContextState { reset_counter: self.reset, generation: self.vram_lost, guilty: false }
    }
    /// Marks a context guilty. Its snapshot leaves the device's counter behind, for good as a counter move would:
    /// Mesa reads GUILTY only inside RESET, and a queue-scope recovery does not move the device's counter.
    pub fn blame(&self, c: &mut ContextState) {
        c.guilty = true;
        if c.reset_counter == self.reset {
            c.reset_counter = self.reset.wrapping_sub(1);
        }
    }
    /// The flags of QUERY_STATE2, by Linux's rule (amdgpu_ctx.c:596-606): each from its own source.
    pub fn query2(&self, c: &ContextState) -> u64 {
        let mut flags = 0;
        if c.reset_counter != self.reset {
            flags |= query2::RESET;
        }
        if c.generation != self.vram_lost {
            flags |= query2::VRAMLOST;
        }
        if c.guilty {
            flags |= query2::GUILTY;
        }
        if self.in_reset {
            flags |= query2::RESET_IN_PROGRESS;
        }
        flags
    }
}

/// How a job a recovery took off a ring ends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fate {
    /// The hung job: ETIME, DR's contract (crates/drm/src/job.rs, amdgpu_job.c:68).
    Hung,
    /// Lost with the ring: ECANCELED (sched_main.c drm_sched_start). Linux 6.6 resubmits nothing.
    Canceled,
}

impl Fate {
    pub fn errno(self) -> i32 {
        match self {
            Fate::Hung => errno::ETIME,
            Fate::Canceled => errno::ECANCELED,
        }
    }
}

/// One job's end.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct JobFate {
    pub ring: RingId,
    pub job: Pending,
    pub fate: Fate,
}
