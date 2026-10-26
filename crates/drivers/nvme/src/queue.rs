//! One submission/completion queue pair as plain ring logic (no locks, no klog): the caller holds the queue's
//! lock, writes the doorbells the methods return and owns the waiting. Phase tag and head/tail wrap as in
//! NVMe 1.4 §4.1, §4.6 (linux-6.6/drivers/nvme/host/pci.c `nvme_sq_copy_cmd`, `nvme_cqe_pending`,
//! `nvme_update_cq_head`).

use crate::cmd::{Cqe, Encode};
use crate::hw::DmaMem;
use crate::regs::{CQE, SQE};
use core::marker::PhantomData;

pub struct QueuePair<M: DmaMem, C: Encode> {
    pub qid: u16,
    pub depth: u16,
    pub sq: M,
    pub cq: M,
    sq_tail: u16,
    cq_head: u16,
    /// The phase tag a new completion carries (starts at 1; flips at each CQ wrap).
    phase: bool,
    next_cid: u16,
    /// The controller's SQ head from the last completion: the tail never overtakes it.
    sq_head: u16,
    _c: PhantomData<C>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueError {
    /// No free slot (every slot submitted and not yet consumed by the controller).
    Full,
    /// A DMA access fell outside the queue memory (a wrong depth: never with the sizes `new` checks).
    Memory,
}

impl<M: DmaMem, C: Encode> QueuePair<M, C> {
    /// Fails (None) when the memory is too small for `depth` entries or `depth` < 2.
    pub fn new(qid: u16, depth: u16, sq: M, cq: M) -> Option<Self> {
        if depth < 2 || sq.size() < depth as usize * SQE || cq.size() < depth as usize * CQE {
            return None;
        }
        if !sq.zero() || !cq.zero() {
            return None;
        }
        Some(QueuePair { qid, depth, sq, cq, sq_tail: 0, cq_head: 0, phase: true, next_cid: 0, sq_head: 0, _c: PhantomData })
    }

    /// Back to the empty state (after a controller reset: the controller forgets its queues).
    pub fn reset(&mut self) -> bool {
        self.sq_tail = 0;
        self.cq_head = 0;
        self.phase = true;
        self.sq_head = 0;
        self.sq.zero() && self.cq.zero()
    }

    /// Places `cmd` in the next slot; returns (CID, the new tail to write to the SQ doorbell, opcode byte).
    pub fn push(&mut self, cmd: &C) -> Result<(u16, u16, u8), QueueError> {
        let next = (self.sq_tail + 1) % self.depth;
        if next == self.sq_head {
            return Err(QueueError::Full);
        }
        let cid = self.next_cid;
        // FFFFh is reserved (the error log's "no command"): the CID wraps before it.
        self.next_cid = if cid >= 0xFFFE { 0 } else { cid + 1 };
        let bytes = cmd.encode(cid);
        if !self.sq.copy_in(self.sq_tail as usize * SQE, &bytes) {
            return Err(QueueError::Memory);
        }
        self.sq_tail = next;
        Ok((cid, next, bytes[0]))
    }

    /// The next completion if the controller posted one; also returns the new CQ head for its doorbell.
    pub fn pop(&mut self) -> Result<Option<(Cqe, u16)>, QueueError> {
        let off = self.cq_head as usize * CQE;
        // Dword 3 first: its phase bit says whether the other dwords are the controller's (loads keep their
        // order on x86 and the accesses are volatile).
        let d3 = self.cq.rd32(off + 12).ok_or(QueueError::Memory)?;
        if ((d3 >> 16) & 1 != 0) != self.phase {
            return Ok(None);
        }
        let d0 = self.cq.rd32(off).ok_or(QueueError::Memory)?;
        let d2 = self.cq.rd32(off + 8).ok_or(QueueError::Memory)?;
        let cqe = Cqe::from_dwords([d0, 0, d2, d3]);
        self.cq_head += 1;
        if self.cq_head == self.depth {
            self.cq_head = 0;
            self.phase = !self.phase;
        }
        // A hostile SQ head beyond the ring is ignored (the old one stays).
        if cqe.sq_head < self.depth {
            self.sq_head = cqe.sq_head;
        }
        Ok(Some((cqe, self.cq_head)))
    }

    pub fn tail(&self) -> u16 {
        self.sq_tail
    }
    pub fn head(&self) -> u16 {
        self.cq_head
    }
    pub fn phase(&self) -> bool {
        self.phase
    }
}
