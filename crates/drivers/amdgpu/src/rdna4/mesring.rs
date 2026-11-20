//! Wave 44, item ME: MES's two rings as the CPU drives them, Linux's `mes_v12_0_submit_pkt_and_poll_completion`
//! (mes_v12_0.c:152-272) over `amdgpu_ring_write_multiple`, `amdgpu_ring_commit` and `mes_v12_0_ring_set_wptr`
//! (:51-62: the 64-bit write pointer in dwords into its slot, then `WDOORBELL64`), and the piece of system memory
//! everything MES and the queues it maps read lies in.
//!
//! Every command goes out as Linux sends it: a write-back slot zeroed as its status, the sequence number taken,
//! Linux's first wait (room for the command: the fence must have reached `seq - num_fences_mask`), the command
//! with that slot as its completion fence and the value 1, then a QUERY_SCHEDULER_STATUS whose fence is the ring's
//! own fence slot with the sequence number, the commit, and the wait for that fence. Both waits are Linux's
//! `amdgpu_fence_wait_polling` (amdgpu_fence.c:310-320): `udelay(2)` steps for at most 2.1 s. The command
//! passed when the status' lower dword is not 0. Memory polls are no register accesses: the trace holds none.

use super::host::Card;
use super::sdmaq::{QStop, sys_read32, sys_read64, sys_write};
use alloc::vec::Vec;
use amdgpu_fmt::wb::{Slot, WbPool};
use rdna4_core::mes12::{self, Pipe};
use rdna4_core::mes12api as api;
use rdna4_core::pte12::PAGE;

/// The piece, as byte offsets into it. Linux allocated all of it in `sw_init` (GTT, behind the GART); BitsOS takes
/// it once in the stage `mes` and binds it whole there, right before the stage's first access. The kernel queues'
/// part is what the stage `mesq` fills (their MQDs) and maps.
pub mod lay {
    use super::{PAGE, mes12};
    /// Per MES pipe: its MQD page, its EOP page (2048 bytes used, `mes_v12_0_allocate_eop_buf`), its ring.
    pub const fn mqd(pipe: usize) -> u64 {
        pipe as u64 * PAGE
    }
    pub const fn eop(pipe: usize) -> u64 {
        (2 + pipe as u64) * PAGE
    }
    pub const fn ring(pipe: usize) -> u64 {
        4 * PAGE + pipe as u64 * mes12::RING_BYTES as u64
    }
    pub const WB: u64 = 4 * PAGE + 2 * mes12::RING_BYTES as u64;
    /// The kernel queues: an MQD page each (the five of `mes12::KERNEL_QUEUES`), the compute queues' EOP buffers
    /// in one (`gfx_v12_0_mec_init`: `num_compute_rings * GFX12_MEC_HPD_SIZE`), their rings.
    pub const fn kq_mqd(i: usize) -> u64 {
        WB + PAGE + i as u64 * PAGE
    }
    pub const KQ_EOP: u64 = WB + 6 * PAGE;
    pub const fn kq_ring(i: usize) -> u64 {
        KQ_EOP + 2 * PAGE + i as u64 * mes12::KQ_RING_BYTES as u64
    }
    pub const PIECE: u64 = kq_ring(5);
}

/// Slots of the write-back page (32 bytes each).
pub const WB_SLOTS: u32 = (PAGE / amdgpu_fmt::wb::SLOT_BYTES) as u32;

/// Where the piece lies: its system address and the GART address of its first page.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Place {
    pub sys: u64,
    pub mc: u64,
}

impl Place {
    pub fn slot_mc(&self, s: Slot) -> u64 {
        s.gpu_addr(self.mc + lay::WB)
    }
    pub fn slot_sys(&self, s: Slot) -> u64 {
        s.gpu_addr(self.sys + lay::WB)
    }
}

/// A ring's five slots in `amdgpu_ring_init`'s order (amdgpu_ring.c:264-320).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Slots {
    pub rptr: Slot,
    pub wptr: Slot,
    pub fence: Slot,
    pub trail: Slot,
    pub cond_exe: Slot,
}

impl Slots {
    pub fn take(pool: &mut WbPool) -> Result<Slots, QStop> {
        let mut get = || pool.get().map_err(QStop::Slots);
        Ok(Slots { rptr: get()?, wptr: get()?, fence: get()?, trail: get()?, cond_exe: get()? })
    }

    /// Linux's ring init: the four at 0, the conditional execution's at 1 ("always set cond_exec_polling to
    /// CONTINUE", amdgpu_ring.c:320).
    pub fn init(&self, card: &mut dyn Card, p: &Place) -> Result<(), QStop> {
        for s in [self.rptr, self.wptr, self.fence, self.trail] {
            sys_write(card, p.slot_sys(s), &0u64.to_le_bytes())?;
        }
        sys_write(card, p.slot_sys(self.cond_exe), &1u64.to_le_bytes())
    }
}

/// One MES pipe's ring as the driver keeps it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Port {
    pub pipe: Pipe,
    pub slots: Slots,
    /// The write pointer in dwords, as Linux keeps it (64 bits, never wrapped: `support_64bit_ptrs`).
    pub wptr: u64,
    /// `fence_drv.sync_seq`.
    pub seq: u32,
}

/// How a command went.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sent {
    /// The status' lower dword was not 0: (polls of the fence, the status).
    Done { polls: u32, status: u64 },
    /// The fence came, but the status' lower dword was 0 ("failed to respond"): the upper dword is the firmware's
    /// error code as the header documents it.
    Failed { polls: u32, status: u64 },
    /// No fence within Linux's 2.1 s.
    NoAnswer { polls: u32, status: u64 },
    /// Linux's first wait ran out: "MES ring buffer is full" (mes_v12_0.c:263-264).
    Full,
}

impl Sent {
    pub fn ok(self) -> bool {
        matches!(self, Sent::Done { .. })
    }
}

fn polls_bound() -> u32 {
    (mes12::TIMEOUT_US / mes12::POLL_US) as u32
}

impl Port {
    pub fn new(pipe: Pipe, pool: &mut WbPool) -> Result<Port, QStop> {
        Ok(Port { pipe, slots: Slots::take(pool)?, wptr: 0, seq: 0 })
    }

    pub fn ring_mc(&self, p: &Place) -> u64 {
        p.mc + lay::ring(self.pipe.index())
    }

    /// `amdgpu_fence_wait_polling` (amdgpu_fence.c:310-320) on the ring's fence slot: (reached in time, polls).
    /// Linux reads once more after its last `udelay(2)` but returns 0 then, which its caller takes as no answer:
    /// a fence that shows only at that read is no answer here either.
    fn wait_fence(&self, card: &mut dyn Card, p: &Place, seq: u32) -> Result<(bool, u32), QStop> {
        let at = p.slot_sys(self.slots.fence);
        let mut polls = 0;
        loop {
            let fence = sys_read32(card, at)?;
            if seq.wrapping_sub(fence) as i32 <= 0 || polls >= polls_bound() {
                return Ok((seq.wrapping_sub(fence) as i32 <= 0 && polls < polls_bound(), polls));
            }
            card.hw().delay_us(mes12::POLL_US);
            polls += 1;
        }
    }

    /// One command on this ring, as Linux submits it. `frame` is the command's 64 dwords without its status,
    /// which is filled here.
    pub fn submit(&mut self, card: &mut dyn Card, p: &Place, pool: &mut WbPool, frame: &mut [u32]) -> Result<Sent, QStop> {
        let opcode = api::header_fields(frame[0]).1;
        // Only Linux's commands are built here, and each has its status at a known dword.
        let at = api::status_at(opcode).ok_or(QStop::Ring(amdgpu_fmt::ring::RingError::State))?;
        let status = pool.get().map_err(QStop::Slots)?;
        let r = self.submit_with(card, p, frame, at, status);
        // Linux frees the status slot whatever came of the command. A command that went out and got no fence may
        // still be answered by a MES that runs on, into that slot: it stays taken for the boot, so no later
        // command reads that late answer as its own (plan/spec/gpu-rdna4.md section 7, `mes-answers`).
        if matches!(r, Ok(Sent::Done { .. } | Sent::Failed { .. } | Sent::Full)) {
            pool.free(status).map_err(QStop::Slots)?;
        }
        r
    }

    fn submit_with(&mut self, card: &mut dyn Card, p: &Place, frame: &mut [u32], at: usize, status: Slot) -> Result<Sent, QStop> {
        sys_write(card, p.slot_sys(status), &0u64.to_le_bytes())?;
        self.seq = self.seq.wrapping_add(1);
        let seq = self.seq;
        if !self.wait_fence(card, p, seq.wrapping_sub(mes12::FENCES_MASK))?.0 {
            return Ok(Sent::Full);
        }
        api::set_status(frame, at, p.slot_mc(status), 1);
        let mut query = api::query_status();
        api::set_status(&mut query, api::at::QUERY_STATUS, p.slot_mc(self.slots.fence), u64::from(seq));
        self.put(card, p, frame)?;
        self.put(card, p, &query)?;
        // amdgpu_ring_commit: mb(), then the write pointer into its slot and the doorbell (no HDP flush: the ring
        // lies in system memory).
        card.hw().store_barrier();
        sys_write(card, p.slot_sys(self.slots.wptr), &self.wptr.to_le_bytes())?;
        let index_dw = self.pipe.doorbell();
        if !card.hw().doorbell_write64(index_dw, self.wptr) {
            return Err(QStop::Doorbell { index_dw });
        }
        let (reached, polls) = self.wait_fence(card, p, seq)?;
        let got = sys_read64(card, p.slot_sys(status))?;
        Ok(match (reached, got as u32 != 0) {
            (true, true) => Sent::Done { polls, status: got },
            (true, false) => Sent::Failed { polls, status: got },
            (false, _) => Sent::NoAnswer { polls, status: got },
        })
    }

    /// `amdgpu_ring_write_multiple` of one frame at the write pointer, wrapping at the ring's end.
    fn put(&mut self, card: &mut dyn Card, p: &Place, words: &[u32]) -> Result<(), QStop> {
        let base = p.sys + lay::ring(self.pipe.index());
        let mut done = 0;
        while done < words.len() {
            let at = (self.wptr % u64::from(mes12::RING_DW)) as usize;
            let n = (words.len() - done).min(mes12::RING_DW as usize - at);
            let bytes: Vec<u8> = words[done..done + n].iter().flat_map(|w| w.to_le_bytes()).collect();
            sys_write(card, base + at as u64 * 4, &bytes)?;
            (done, self.wptr) = (done + n, self.wptr + n as u64);
        }
        Ok(())
    }
}

/// A command's line, as Linux's messages name it (mes_v12_0.c:217-227, :237-247).
pub fn said(pipe: Pipe, frame: &[u32], s: Sent) -> alloc::string::String {
    let op = api::header_fields(frame[0]).1;
    let misc = if op == api::op::MISC { alloc::format!(" ({})", api::misc_name(frame[api::at::MISC_OPCODE])) } else { alloc::string::String::new() };
    let name = api::op_name(op);
    let p = pipe.index();
    match s {
        Sent::Done { polls, status } => alloc::format!("MES({p}) {name}{misc}: answered after {polls} polls of its fence, status {status:#x}"),
        Sent::Failed { polls, status } => alloc::format!("MES({p}) failed to respond to msg={name}{misc}: the fence came after {polls} polls, the status is {status:#x} ({})", api::describe_error((status >> 32) as u32)),
        Sent::NoAnswer { polls, status } => alloc::format!("MES({p}) failed to respond to msg={name}{misc}: no fence after {polls} polls 2 us apart (Linux's 2.1 s), the status is {status:#x}"),
        Sent::Full => alloc::format!("MES({p}) msg={name}{misc}: MES ring buffer is full (the fence of the command 15 before did not come within 2.1 s)"),
    }
}
