//! Submission and completion rings as plain logic (no locks, no klog): the caller holds the ring's lock, writes the
//! doorbells the methods return and owns the waiting. Phase tag and head/tail wrap as in NVMe 1.4 §4.1, §4.6
//! (linux-7/drivers/nvme/host/pci.c `nvme_sq_copy_cmd`, `nvme_cqe_pending`, `nvme_update_cq_head`). The admin
//! queue is a `QueuePair` (one command at a time, running CIDs); an I/O queue (`mq`) keeps its two rings under two
//! locks, the CQ's taken in the interrupt (BL-1).

use crate::cmd::{Cqe, Encode};
use crate::hw::DmaMem;
use crate::regs::{CQE, SQE};
use core::marker::PhantomData;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueError {
    /// No free slot (every slot submitted and not yet consumed by the controller).
    Full,
    /// A DMA access fell outside the queue memory (a wrong depth: never with the sizes `new` checks).
    Memory,
}

/// The submission ring: entries at `tail`, the controller's head as the last completion reported it.
pub struct SqRing<M: DmaMem> {
    pub mem: M,
    depth: u16,
    tail: u16,
}

impl<M: DmaMem> SqRing<M> {
    /// None when the memory is too small for `depth` entries or `depth` < 2.
    pub fn new(depth: u16, mem: M) -> Option<Self> {
        (depth >= 2 && mem.size() >= depth as usize * SQE && mem.zero()).then_some(SqRing { mem, depth, tail: 0 })
    }
    pub fn reset(&mut self) -> bool {
        self.tail = 0;
        self.mem.zero()
    }
    /// Places `bytes` at the tail unless that would reach `head`; returns the new tail for the doorbell.
    pub fn push(&mut self, bytes: &[u8; 64], head: u16) -> Result<u16, QueueError> {
        let next = (self.tail + 1) % self.depth;
        if next == head {
            return Err(QueueError::Full);
        }
        if !self.mem.copy_in(self.tail as usize * SQE, bytes) {
            return Err(QueueError::Memory);
        }
        self.tail = next;
        Ok(next)
    }
    pub fn tail(&self) -> u16 {
        self.tail
    }
}

/// The completion ring: the next entry at `head` is the controller's once its phase bit is `phase`.
pub struct CqRing<M: DmaMem> {
    pub mem: M,
    depth: u16,
    head: u16,
    /// The phase tag a new completion carries (starts at 1; flips at each CQ wrap).
    phase: bool,
}

impl<M: DmaMem> CqRing<M> {
    pub fn new(depth: u16, mem: M) -> Option<Self> {
        (depth >= 2 && mem.size() >= depth as usize * CQE && mem.zero()).then_some(CqRing { mem, depth, head: 0, phase: true })
    }
    pub fn reset(&mut self) -> bool {
        self.head = 0;
        self.phase = true;
        self.mem.zero()
    }
    /// The next completion if the controller posted one (the caller rings the head doorbell with `head()`).
    pub fn pop(&mut self) -> Result<Option<Cqe>, QueueError> {
        let off = self.head as usize * CQE;
        // Dword 3 first: its phase bit says whether the other dwords are the controller's (loads keep their
        // order on x86 and the accesses are volatile).
        let d3 = self.mem.rd32(off + 12).ok_or(QueueError::Memory)?;
        if ((d3 >> 16) & 1 != 0) != self.phase {
            return Ok(None);
        }
        let d0 = self.mem.rd32(off).ok_or(QueueError::Memory)?;
        let d2 = self.mem.rd32(off + 8).ok_or(QueueError::Memory)?;
        self.head += 1;
        if self.head == self.depth {
            self.head = 0;
            self.phase = !self.phase;
        }
        Ok(Some(Cqe::from_dwords([d0, 0, d2, d3])))
    }
    pub fn head(&self) -> u16 {
        self.head
    }
    pub fn phase(&self) -> bool {
        self.phase
    }
}

/// The admin queue: both rings under one lock, a running CID (one command at a time).
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
