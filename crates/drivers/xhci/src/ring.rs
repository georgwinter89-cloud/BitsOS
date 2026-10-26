//! The two ring kinds of US-1 (xHCI 1.2 §4.9): a producer ring made of segments joined by Link TRBs (the command
//! ring; the transfer rings of US-2/3 use it too) and the event ring, whose segments the controller finds in the
//! Event Ring Segment Table (ERST). Both work on any number of segments (R1); each segment is one DMA buffer that
//! never crosses a 64 KiB boundary (the allocation asks for that).

use crate::mem::DmaMem;
use crate::trb::{ty, Trb, CHAIN, CYCLE, TRB_BYTES};
use alloc::vec::Vec;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RingError {
    /// A ring memory access fell outside its buffer (cannot happen with consistent bookkeeping).
    Mem,
    /// No free TRB: the controller has not consumed enough yet.
    Full,
    /// Segments of different sizes, or too small for a TRB plus the Link TRB.
    BadSegments,
    /// `grow` while the controller may still be ahead of the producer in the producer's own segment (on its
    /// previous lap): new segments there would be run by it on that lap or be overwritten under it.
    Wrapped,
}

/// A producer ring: software enqueues, the controller dequeues. The last TRB of every segment is a Link TRB to
/// the next segment; the last segment's link points back to the first and toggles the cycle state.
pub struct ProducerRing<M: DmaMem> {
    segs: Vec<M>,
    per_seg: usize,
    seg: usize,
    enq: usize,
    cycle: bool,
    in_flight: usize,
    wraps: u64,
}

impl<M: DmaMem> ProducerRing<M> {
    pub fn new(segs: Vec<M>) -> Result<Self, RingError> {
        let per_seg = segs.first().map(|s| s.size() / TRB_BYTES).ok_or(RingError::BadSegments)?;
        if per_seg < 2 || segs.iter().any(|s| s.size() / TRB_BYTES != per_seg) {
            return Err(RingError::BadSegments);
        }
        let mut r = ProducerRing { segs, per_seg, seg: 0, enq: 0, cycle: true, in_flight: 0, wraps: 0 };
        r.init()?;
        Ok(r)
    }

    /// Empties the ring: all TRBs zero, links written (not yet owned by the controller), cycle state 1. Used at
    /// start and after a command abort (V1 `xhci_abort`: the ring is set up afresh once it stands).
    pub fn init(&mut self) -> Result<(), RingError> {
        for s in &self.segs {
            if !s.zero() {
                return Err(RingError::Mem);
            }
        }
        for i in 0..self.segs.len() {
            let l = self.link(i);
            // Cycle 0 while the producer cycle is 1: the controller stops in front of it until we pass it.
            if !self.segs[i].write_trb(self.link_off(), &l) {
                return Err(RingError::Mem);
            }
        }
        self.seg = 0;
        self.enq = 0;
        self.cycle = true;
        self.in_flight = 0;
        Ok(())
    }

    fn link_off(&self) -> usize {
        (self.per_seg - 1) * TRB_BYTES
    }

    fn link(&self, i: usize) -> Trb {
        let n = self.segs.len();
        Trb::link(self.segs[(i + 1) % n].dev_addr(), i == n - 1)
    }

    /// Address of the first TRB (CRCR, endpoint context dequeue pointer).
    pub fn base(&self) -> u64 {
        self.segs[0].dev_addr()
    }
    /// The producer cycle state (CRCR.RCS / DCS for a fresh ring is 1).
    pub fn cycle(&self) -> bool {
        self.cycle
    }
    pub fn segments(&self) -> usize {
        self.segs.len()
    }
    /// TRBs that can be outstanding at once (one slot stays empty so full and empty differ).
    pub fn capacity(&self) -> usize {
        self.segs.len() * (self.per_seg - 1) - 1
    }
    pub fn in_flight(&self) -> usize {
        self.in_flight
    }
    /// How often the enqueue pointer passed the last segment's link (ktests: "past its Link TRB").
    pub fn wraps(&self) -> u64 {
        self.wraps
    }

    /// True if `addr` is a TRB slot of this ring (matching completion events to commands).
    pub fn contains(&self, addr: u64) -> bool {
        addr.is_multiple_of(TRB_BYTES as u64)
            && self.segs.iter().any(|s| addr >= s.dev_addr() && addr < s.dev_addr() + (self.link_off() as u64))
    }

    /// Enqueues one TRB with the current cycle bit; returns its address. Passing a Link TRB hands it to the
    /// controller with the current cycle bit and toggles the cycle state on the last segment's link.
    pub fn push(&mut self, t: Trb) -> Result<u64, RingError> {
        self.push_inner(t, false)
    }

    /// `held`: written with the cycle bit the controller does not own yet (the first TRB of a TD, `push_td`).
    fn push_inner(&mut self, mut t: Trb, held: bool) -> Result<u64, RingError> {
        if self.in_flight >= self.capacity() {
            return Err(RingError::Full);
        }
        let off = self.enq * TRB_BYTES;
        let addr = self.segs[self.seg].dev_addr() + off as u64;
        t.control = (t.control & !CYCLE) | (self.cycle != held) as u32;
        if !self.segs[self.seg].write_trb(off, &t) {
            return Err(RingError::Mem);
        }
        self.enq += 1;
        self.in_flight += 1;
        if self.enq == self.per_seg - 1 {
            let mut l = self.link(self.seg);
            // A TD that goes on past the link keeps its chain across it (xHCI §6.4.4.1, Linux `inc_enq`).
            l.control |= self.cycle as u32 | (t.control & CHAIN);
            if !self.segs[self.seg].write_trb(self.link_off(), &l) {
                return Err(RingError::Mem);
            }
            if self.seg == self.segs.len() - 1 {
                self.cycle = !self.cycle;
                self.wraps += 1;
            }
            self.seg = (self.seg + 1) % self.segs.len();
            self.enq = 0;
        }
        Ok(addr)
    }

    /// The controller consumed `n` TRBs (a completion event arrived).
    pub fn consumed(&mut self, n: usize) {
        self.in_flight = self.in_flight.saturating_sub(n);
    }

    /// Room for `n` more TRBs.
    pub fn has_room(&self, n: usize) -> bool {
        self.in_flight + n <= self.capacity()
    }

    /// Where the next TRB goes and its cycle bit: the dequeue pointer for Set TR Dequeue Pointer when everything
    /// queued so far is to be skipped (V1 `xhci_ep_restart`).
    pub fn enqueue_ptr(&self) -> (u64, bool) {
        (self.segs[self.seg].dev_addr() + (self.enq * TRB_BYTES) as u64, self.cycle)
    }

    /// The controller's dequeue pointer was moved to `enqueue_ptr()`: nothing is outstanding any more.
    pub fn skipped(&mut self) {
        self.in_flight = 0;
    }

    /// TRBs one segment holds besides its Link TRB.
    pub fn per_segment(&self) -> usize {
        self.per_seg - 1
    }
    /// TRB places left in the producer's segment before its Link TRB (at least 1).
    pub fn before_link(&self) -> usize {
        self.per_seg - 1 - self.enq
    }
    /// Free TRB places.
    pub fn room(&self) -> usize {
        self.capacity().saturating_sub(self.in_flight)
    }

    /// Enqueues a whole TD; returns the address of every TRB and the cycle bit of the first. The first TRB is
    /// handed over last (written with the wrong cycle bit, flipped after the rest): the controller never starts a
    /// TD it cannot see whole (Linux `giveback_first_trb`). All or nothing: refused with `Full` before any write.
    pub fn push_td(&mut self, trbs: &[Trb]) -> Result<(alloc::vec::Vec<u64>, bool), RingError> {
        if trbs.is_empty() {
            return Err(RingError::Mem);
        }
        if !self.has_room(trbs.len()) {
            return Err(RingError::Full);
        }
        let cycle = self.cycle;
        let mut addrs = alloc::vec::Vec::with_capacity(trbs.len());
        for (i, t) in trbs.iter().enumerate() {
            addrs.push(self.push_inner(*t, i == 0)?);
        }
        let (seg, off) = self.locate(addrs[0]).ok_or(RingError::Mem)?;
        let c = self.segs[seg].rd32(off + 12).ok_or(RingError::Mem)?;
        if !self.segs[seg].wr32(off + 12, (c & !CYCLE) | cycle as u32) {
            return Err(RingError::Mem);
        }
        Ok((addrs, cycle))
    }

    /// Segment index and byte offset of a TRB address of this ring (not a Link TRB).
    fn locate(&self, addr: u64) -> Option<(usize, usize)> {
        if !addr.is_multiple_of(TRB_BYTES as u64) {
            return None;
        }
        self.segs.iter().position(|s| addr >= s.dev_addr() && addr < s.dev_addr() + self.link_off() as u64)
            .map(|i| (i, (addr - self.segs[i].dev_addr()) as usize))
    }

    /// Turns a queued TRB into a Transfer No-Op, keeping its cycle and chain bits and dropping IOC/ISP (a cancelled
    /// TD the controller has not reached; it passes it without an event). Only while the endpoint does not run.
    pub fn make_noop(&mut self, addr: u64) -> bool {
        let Some((seg, off)) = self.locate(addr) else { return false };
        let Some(c) = self.segs[seg].rd32(off + 12) else { return false };
        self.segs[seg].wr32(off + 12, (c & (CYCLE | CHAIN)) | (ty::TRANSFER_NOOP as u32) << 10)
    }

    /// Room for a TD of `n` TRBs that also keeps the producer from crossing a Link TRB into the segment of
    /// `oldest`, the oldest TRB the controller may still have to run (`None`: nothing outstanding). Producer and
    /// controller then never share a segment with the controller ahead (on its previous lap), the one position in
    /// which `grow` would be unsafe; a ring that cannot take the TD under this rule grows instead (R1). A ring of
    /// N segments holds up to (N - 1) segments' worth of TRBs this way.
    pub fn fits(&self, n: usize, oldest: Option<u64>) -> bool {
        if !self.has_room(n) {
            return false;
        }
        let Some((c, _)) = oldest.filter(|_| self.in_flight > 0).and_then(|a| self.locate(a)) else { return true };
        // `push_inner` crosses a segment's link right after filling its last place.
        let (mut left, mut seg, mut room) = (n, self.seg, self.before_link());
        while left >= room {
            left -= room;
            seg = (seg + 1) % self.segs.len();
            if seg == c {
                return false;
            }
            room = self.per_seg - 1;
        }
        true
    }

    /// Segments `grow` has to add (after the producer's segment) so that a TD of `n` TRBs `fits`.
    pub fn segments_for(&self, n: usize) -> usize {
        n.saturating_sub(self.before_link()) / (self.per_seg - 1) + 1
    }

    /// True if the controller may be ahead of the producer in the producer's own segment (see `fits`).
    fn wrapped(&self, oldest: Option<u64>) -> bool {
        oldest.filter(|_| self.in_flight > 0).and_then(|a| self.locate(a))
            .is_some_and(|(s, off)| s == self.seg && off >= self.enq * TRB_BYTES)
    }

    /// Adds segments after the one the producer is in (Linux `xhci_ring_expansion`): R1, a queue grows instead
    /// of refusing. `oldest` as for `fits`. Safe because the controller is not ahead of the producer in its
    /// segment (refused with `Wrapped` otherwise; `fits` keeps it from happening): it reaches the producer's link
    /// only after the producer has passed it. The new TRBs are Transfer No-Ops and they and the links carry the
    /// cycle bit the controller does not own yet, so it stops in front of them until the producer gets there
    /// (and would pass them harmlessly if it ever did not); the toggle moves to whichever segment is last now.
    pub fn grow(&mut self, extra: alloc::vec::Vec<M>, oldest: Option<u64>) -> Result<(), RingError> {
        if extra.iter().any(|s| s.size() / TRB_BYTES != self.per_seg) {
            return Err(RingError::BadSegments);
        }
        if self.wrapped(oldest) {
            return Err(RingError::Wrapped);
        }
        let not_owned = !self.cycle as u32;
        for s in &extra {
            for i in 0..self.per_seg {
                if !(s.wr32(i * TRB_BYTES, 0) && s.wr32(i * TRB_BYTES + 4, 0) && s.wr32(i * TRB_BYTES + 8, 0)
                    && s.wr32(i * TRB_BYTES + 12, (ty::TRANSFER_NOOP as u32) << 10 | not_owned))
                {
                    return Err(RingError::Mem);
                }
            }
        }
        let at = self.seg + 1;
        let n = extra.len();
        for (k, s) in extra.into_iter().enumerate() {
            self.segs.insert(at + k, s);
        }
        // The producer's segment (its link is not handed over in this lap) and the new ones get their links.
        for i in self.seg..at + n {
            let mut l = self.link(i);
            l.control |= not_owned;
            if !self.segs[i].write_trb(self.link_off(), &l) {
                return Err(RingError::Mem);
            }
        }
        // If the producer's segment was the last one, its old link carried the toggle; the new last one does now.
        Ok(())
    }
}

/// The event ring of one interrupter: the controller produces, software consumes.
pub struct EventRing<M: DmaMem> {
    segs: Vec<M>,
    erst: M,
    seg: usize,
    deq: usize,
    cycle: bool,
    wraps: u64,
}

impl<M: DmaMem> EventRing<M> {
    /// `erst` must hold 16 bytes per segment.
    pub fn new(segs: Vec<M>, erst: M) -> Result<Self, RingError> {
        if segs.is_empty() || erst.size() < segs.len() * 16 || segs.iter().any(|s| s.size() < 16 * TRB_BYTES) {
            return Err(RingError::BadSegments);
        }
        let r = EventRing { segs, erst, seg: 0, deq: 0, cycle: true, wraps: 0 };
        for s in &r.segs {
            if !s.zero() {
                return Err(RingError::Mem);
            }
        }
        for (i, s) in r.segs.iter().enumerate() {
            // The ERST entry's Ring Segment Size is 16..4096 TRBs.
            let n = (s.size() / TRB_BYTES).min(4096) as u32;
            if !(r.erst.wr64(i * 16, s.dev_addr()) && r.erst.wr32(i * 16 + 8, n) && r.erst.wr32(i * 16 + 12, 0)) {
                return Err(RingError::Mem);
            }
        }
        Ok(r)
    }

    fn seg_trbs(&self, i: usize) -> usize {
        (self.segs[i].size() / TRB_BYTES).min(4096)
    }
    pub fn erst_base(&self) -> u64 {
        self.erst.dev_addr()
    }
    pub fn segments(&self) -> usize {
        self.segs.len()
    }
    pub fn capacity(&self) -> usize {
        (0..self.segs.len()).map(|i| self.seg_trbs(i)).sum()
    }
    /// The software dequeue pointer, as ERDP wants it.
    pub fn dequeue_ptr(&self) -> u64 {
        self.segs[self.seg].dev_addr() + (self.deq * TRB_BYTES) as u64
    }
    /// The dequeue segment index (ERDP.DESI, low three bits).
    pub fn dequeue_segment(&self) -> usize {
        self.seg
    }
    pub fn wraps(&self) -> u64 {
        self.wraps
    }

    /// True if the controller has written an event software has not consumed.
    pub fn pending(&self) -> Result<bool, RingError> {
        self.segs[self.seg].read_trb_if(self.deq * TRB_BYTES, self.cycle).map(|t| t.is_some()).ok_or(RingError::Mem)
    }

    /// Takes the next event, if the controller has written one. The dequeue pointer moves before the caller
    /// handles the event, so an event whose handling fails does not come back (V1 `xhci_events`).
    pub fn pop(&mut self) -> Result<Option<Trb>, RingError> {
        let Some(t) = self.segs[self.seg].read_trb_if(self.deq * TRB_BYTES, self.cycle).ok_or(RingError::Mem)? else {
            return Ok(None);
        };
        self.deq += 1;
        if self.deq == self.seg_trbs(self.seg) {
            self.deq = 0;
            self.seg += 1;
            if self.seg == self.segs.len() {
                self.seg = 0;
                self.cycle = !self.cycle;
                self.wraps += 1;
            }
        }
        Ok(Some(t))
    }
}
