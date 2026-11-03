//! Interpreters: the engines of the fake GPU. [`Engine`] runs a ring (and the IBs it calls) of PM4 packets
//! (GFX CP) or SDMA 6.0 packets on a [`GpuBus`], in ring order, and reports what happened as [`Event`]s.
//!
//! Executed: PM4 NOP/PAD/type 2, WRITE_DATA (memory and registers), RELEASE_MEM (32/64-bit data, interrupt),
//! INDIRECT_BUFFER(_CNST), COND_EXEC, WAIT_REG_MEM, SET_*_REG; SDMA NOP (burst), WRITE_LINEAR, COPY_LINEAR,
//! CONST_FILL, FENCE, TRAP, INDIRECT, POLL_REGMEM (also the HDP flush form), SRBM_WRITE, COND_EXE. Cache,
//! sync and state packets the model has nothing to do for are accepted and reported ([`Event::NoEffect`]).
//! Anything else halts the engine ([`Hang`]) like a real CP or SDMA stops on a bad packet; it stays halted
//! until [`Engine::reset`].
//!
//! VMIDs: a ring INDIRECT packet names the IB's VMID; a nested IB inherits its caller's VMID (decision: the
//! safe model, a user IB cannot reach another address space through the field).
//!
//! Hostile input never panics and every run is bounded: a packet is never read past the ring's write pointer
//! or its IB's end (a PM4 NOP with count 0x3FFF is one dword, see [`crate::pm4::PAD`]); IB nesting, the
//! dwords fetched and the bytes moved per run are limited by [`Limits`]; memory is touched only through the
//! bus. A run that hits a limit returns [`Stop::Budget`] and continues where it stopped on the next call.

use alloc::vec::Vec;

use crate::bus::{GpuBus, MemFault, read_u32};
use crate::{pm4, sdma};

/// Which packet language the engine speaks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Isa {
    Pm4,
    Sdma,
}

/// Bounds of one [`Engine::run`] (no default: the fake chooses; R1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// Dwords fetched per run (ring and IBs).
    pub max_dw: u64,
    /// Bytes moved by COPY_LINEAR and CONST_FILL per run; a bigger single packet halts with TooLarge.
    pub max_bytes: u64,
    /// IB nesting: the hardware allows ring -> IB -> IB2 on the CP (2) and none below an SDMA IB (1).
    pub max_ib_depth: usize,
}

/// Interrupt sources.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IntSrc {
    /// RELEASE_MEM INT_SEL 1 or 2 (end of pipe).
    EndOfPipe,
    /// SDMA TRAP.
    Trap,
}

/// What an engine did, in order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    MemWrite { vmid: u8, addr: u64, bytes: u64 },
    /// A fence write (RELEASE_MEM data or SDMA FENCE); `wide`: 64 bits in one write.
    Fence { vmid: u8, addr: u64, value: u64, wide: bool },
    Interrupt { src: IntSrc, ctx: u32 },
    /// `modelled`: the bus knows the register.
    RegWrite { reg: u32, value: u32, modelled: bool },
    HdpFlush,
    /// A packet the model executes without effect (cache, sync, state).
    NoEffect { header: u32 },
    /// COND_EXEC/COND_EXE skipped this many dwords.
    CondSkip { dwords: u64 },
    /// An IB started.
    Ib { vmid: u8, addr: u64, len_dw: u64, depth: usize },
}

/// Why the engine is halted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hang {
    Fault(MemFault),
    Pm4(pm4::DecodeError),
    Sdma(sdma::DecodeError),
    /// The packet is longer than what is left of its IB (or a COND_EXEC skip runs past it).
    Truncated { need: u64, have: u64 },
    /// A packet the model does not execute (header shows which).
    Unsupported { header: u32 },
    /// A field value the packet does not allow (misaligned address, reserved value).
    BadField { header: u32, what: &'static str },
    IbTooDeep { depth: usize },
    /// One packet moves more bytes, or is longer, than a whole run may.
    TooLarge { header: u32 },
    /// The write pointer is more than a ring ahead of the read pointer.
    BadWptr { rptr: u64, wptr: u64 },
}

impl Hang {
    pub fn errno(&self) -> i32 {
        match self {
            Hang::Fault(_) => crate::errno::EFAULT,
            _ => crate::errno::EIO,
        }
    }
}

/// What a poll waits for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WaitOn {
    Mem { vmid: u8, addr: u64, value: u32, reference: u32, mask: u32, function: u8 },
    /// `value` is `None` when the bus does not model the register.
    Reg { reg: u32, value: Option<u32>, reference: u32, mask: u32, function: u8 },
}

/// Why a run returned.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stop {
    /// Read pointer reached the write pointer, no IB open.
    Idle,
    /// The last packet before the write pointer is not complete yet (the engine waits for more).
    Incomplete { need: u64, have: u64 },
    /// A limit of [`Limits`] was reached; run again.
    Budget,
    /// A poll is not satisfied; run again later.
    Wait(WaitOn),
    /// Halted until [`Engine::reset`].
    Hang(Hang),
}

/// An IB position.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IbPos {
    pub vmid: u8,
    pub base: u64,
    pub len_dw: u64,
    /// Dword offset of the packet in the IB.
    pub offset_dw: u64,
    /// 1 for an IB the ring called.
    pub depth: usize,
}

/// Where the engine stands: the ring read pointer and, inside an IB, the innermost IB position.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Where {
    pub rptr: u64,
    pub ib: Option<IbPos>,
}

/// The result of a run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Report {
    pub stop: Stop,
    /// The packet the engine stopped at (for Idle: the read pointer).
    pub at: Where,
    pub events: Vec<Event>,
    /// Dwords fetched in this run.
    pub fetched_dw: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Frame {
    vmid: u8,
    base: u64,
    len_dw: u64,
    pos: u64,
}

/// Length rule outcome while fetching a header.
enum Len {
    More(u64),
    Hang(Hang),
}

/// What executing one packet means for the stream.
enum Step {
    Next,
    Call { vmid: u8, base: u64, len_dw: u64 },
    Skip(u64),
    Wait(WaitOn),
    Budget,
    Hang(Hang),
}

/// One engine (a GFX CP queue or an SDMA queue) with its ring in VMID 0.
#[derive(Clone, Debug)]
pub struct Engine {
    isa: Isa,
    ring_base: u64,
    ring_dw: u64,
    rptr: u64,
    stack: Vec<Frame>,
    halted: Option<(Hang, Where)>,
    limits: Limits,
    /// The waiting packet whose write half (WAIT_REG_MEM write-wait, SDMA HDP-flush request) already
    /// ran: a retry only compares again, so the register is written and reported once per packet.
    armed: Option<Where>,
}

/// Why [`Engine::new`] refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EngineError {
    /// Ring size not a power of two, or base not 256-byte aligned or wrapping.
    BadRing,
}

const CHUNK: usize = 256;

impl Engine {
    /// An engine whose ring of `ring_dw` dwords lies at GPU address `ring_base` (VMID 0); `start` is the
    /// pointer value the driver programmed for rptr and wptr.
    pub fn new(isa: Isa, ring_base: u64, ring_dw: u64, start: u64, limits: Limits) -> Result<Engine, EngineError> {
        let bytes = ring_dw.checked_mul(4).ok_or(EngineError::BadRing)?;
        if !ring_dw.is_power_of_two() || ring_base & 0xFF != 0 || ring_base.checked_add(bytes).is_none() {
            return Err(EngineError::BadRing);
        }
        Ok(Engine { isa, ring_base, ring_dw, rptr: start, stack: Vec::new(), halted: None, limits, armed: None })
    }
    pub fn isa(&self) -> Isa {
        self.isa
    }
    /// Read pointer (dword counter; what the engine writes to the rptr writeback slot, in bytes for SDMA).
    pub fn rptr(&self) -> u64 {
        self.rptr
    }
    pub fn ib_depth(&self) -> usize {
        self.stack.len()
    }
    pub fn halted(&self) -> Option<(Hang, Where)> {
        self.halted
    }
    pub fn limits(&self) -> Limits {
        self.limits
    }
    pub fn set_limits(&mut self, limits: Limits) {
        self.limits = limits;
    }
    /// Forgets open IBs and the halt; the pointers start again at `start` (a queue reset).
    pub fn reset(&mut self, start: u64) {
        self.rptr = start;
        self.stack.clear();
        self.halted = None;
        self.armed = None;
    }

    fn here(&self) -> Where {
        let depth = self.stack.len();
        Where {
            rptr: self.rptr,
            ib: self.stack.last().map(|f| IbPos {
                vmid: f.vmid,
                base: f.base,
                len_dw: f.len_dw,
                offset_dw: f.pos,
                depth,
            }),
        }
    }

    /// Runs the IB of `len_dw` dwords at `addr` in `vmid` directly (as the raw-IB harness submits it),
    /// then whatever lies on the ring up to the read pointer (nothing).
    pub fn exec_ib<B: GpuBus + ?Sized>(&mut self, bus: &mut B, vmid: u8, addr: u64, len_dw: u64) -> Report {
        if self.halted.is_none() && len_dw > 0 {
            self.stack.push(Frame { vmid, base: addr, len_dw, pos: 0 });
        }
        let wptr = self.rptr;
        self.run(bus, wptr)
    }

    /// Executes from the read pointer up to `wptr` (dword counter).
    pub fn run<B: GpuBus + ?Sized>(&mut self, bus: &mut B, wptr: u64) -> Report {
        let mut events = Vec::new();
        let mut fetched = 0u64;
        let mut bytes_left = self.limits.max_bytes;
        let stop = loop {
            if let Some((h, at)) = self.halted {
                return Report { stop: Stop::Hang(h), at, events, fetched_dw: fetched };
            }
            // The source of the next packet: the innermost IB, else the ring.
            let (vmid, avail) = match self.stack.last() {
                Some(f) if f.pos >= f.len_dw => {
                    self.stack.pop();
                    continue;
                }
                Some(f) => (f.vmid, f.len_dw - f.pos),
                None => {
                    let avail = wptr.wrapping_sub(self.rptr);
                    if avail == 0 {
                        break Stop::Idle;
                    }
                    if avail > self.ring_dw {
                        self.halt(Hang::BadWptr { rptr: self.rptr, wptr });
                        continue;
                    }
                    (0, avail)
                }
            };
            let in_ib = !self.stack.is_empty();
            let budget = self.limits.max_dw - fetched.min(self.limits.max_dw);
            if budget == 0 {
                break Stop::Budget;
            }
            // Fetch the header, then as many dwords as the length rule asks for, never more than `avail`.
            let mut buf: Vec<u32> = Vec::new();
            let len = loop {
                let r = match self.isa {
                    Isa::Pm4 => pm4::packet_len(&buf).map_err(|e| match e {
                        pm4::DecodeError::Truncated { need, .. } => Len::More(need as u64),
                        e => Len::Hang(Hang::Pm4(e)),
                    }),
                    Isa::Sdma => sdma::packet_len(&buf).map_err(|e| match e {
                        sdma::DecodeError::Truncated { need, .. } => Len::More(need as u64),
                        e => Len::Hang(Hang::Sdma(e)),
                    }),
                };
                match r {
                    Ok(n) => break Ok(n as u64),
                    Err(Len::More(need)) if need > avail => break Err(Len::More(need)),
                    Err(Len::More(need)) => {
                        if let Err(f) = self.fetch(bus, vmid, buf.len() as u64, need, &mut buf) {
                            break Err(Len::Hang(Hang::Fault(f)));
                        }
                    }
                    Err(e) => break Err(e),
                }
            };
            let len = match len {
                Ok(n) => n,
                Err(Len::Hang(h)) => {
                    self.halt(h);
                    continue;
                }
                Err(Len::More(need)) => {
                    // Not even the length is complete before the end.
                    if let Some(s) = self.short(in_ib, need, avail) {
                        break s;
                    }
                    continue;
                }
            };
            if len > avail {
                if let Some(s) = self.short(in_ib, len, avail) {
                    break s;
                }
                continue;
            }
            if len > budget {
                if len > self.limits.max_dw {
                    self.halt(Hang::TooLarge { header: buf.first().copied().unwrap_or(0) });
                    continue;
                }
                break Stop::Budget;
            }
            if let Err(f) = self.fetch(bus, vmid, buf.len() as u64, len, &mut buf) {
                self.halt(Hang::Fault(f));
                continue;
            }
            fetched += len;
            let at = self.here();
            let armed = self.armed == Some(at);
            let step = match self.isa {
                Isa::Pm4 => exec_pm4(bus, vmid, &buf, &mut events, &mut bytes_left, self.limits.max_bytes, armed),
                Isa::Sdma => exec_sdma(bus, vmid, &buf, &mut events, &mut bytes_left, self.limits.max_bytes, armed),
            };
            self.armed = match step {
                Step::Wait(_) => Some(at),
                _ => None,
            };
            match step {
                Step::Next => self.advance(len),
                Step::Skip(k) => {
                    if k > avail - len {
                        fetched -= len;
                        if let Some(s) = self.short(in_ib, len + k, avail) {
                            break s;
                        }
                        continue;
                    }
                    events.push(Event::CondSkip { dwords: k });
                    self.advance(len + k);
                }
                Step::Call { vmid: v, base, len_dw } => {
                    if len_dw > 0 && self.stack.len() >= self.limits.max_ib_depth {
                        self.halt(Hang::IbTooDeep { depth: self.stack.len() + 1 });
                        continue;
                    }
                    self.advance(len);
                    if len_dw == 0 {
                        continue;
                    }
                    // Only the ring (the kernel's, VMID 0) picks the address space; an IB called from an IB
                    // runs in its caller's VMID, so a user IB cannot leave its VM.
                    let v = if in_ib { vmid } else { v };
                    self.stack.push(Frame { vmid: v, base, len_dw, pos: 0 });
                    events.push(Event::Ib { vmid: v, addr: base, len_dw, depth: self.stack.len() });
                }
                Step::Wait(w) => {
                    fetched -= len;
                    break Stop::Wait(w);
                }
                Step::Budget => {
                    fetched -= len;
                    break Stop::Budget;
                }
                Step::Hang(h) => self.halt(h),
            }
        };
        Report { stop, at: self.here(), events, fetched_dw: fetched }
    }

    /// A packet runs past the end: on the ring the engine waits for more, in an IB it halts.
    fn short(&mut self, in_ib: bool, need: u64, have: u64) -> Option<Stop> {
        if in_ib {
            self.halt(Hang::Truncated { need, have });
            None
        } else {
            Some(Stop::Incomplete { need, have })
        }
    }

    fn halt(&mut self, h: Hang) {
        self.halted = Some((h, self.here()));
    }

    fn advance(&mut self, n: u64) {
        match self.stack.last_mut() {
            Some(f) => f.pos += n,
            None => self.rptr = self.rptr.wrapping_add(n),
        }
    }

    /// Appends dwords `from..to` of the current packet to `buf`.
    fn fetch<B: GpuBus + ?Sized>(
        &self,
        bus: &mut B,
        vmid: u8,
        from: u64,
        to: u64,
        buf: &mut Vec<u32>,
    ) -> Result<(), MemFault> {
        for i in from..to {
            let addr = match self.stack.last() {
                Some(f) => f.base.wrapping_add((f.pos + i).wrapping_mul(4)),
                None => self.ring_base + ((self.rptr.wrapping_add(i) & (self.ring_dw - 1)) << 2),
            };
            let mut b = [0u8; 4];
            bus.fetch(vmid, addr, &mut b)?;
            buf.push(u32::from_le_bytes(b));
        }
        Ok(())
    }
}

fn write_dwords<B: GpuBus + ?Sized>(bus: &mut B, vmid: u8, addr: u64, data: &[u32]) -> Result<(), MemFault> {
    let mut bytes = [0u8; CHUNK];
    let mut cur = addr;
    for part in data.chunks(CHUNK / 4) {
        for (i, d) in part.iter().enumerate() {
            bytes[i * 4..i * 4 + 4].copy_from_slice(&d.to_le_bytes());
        }
        bus.write(vmid, cur, &bytes[..part.len() * 4])?;
        cur = cur.wrapping_add(part.len() as u64 * 4);
    }
    Ok(())
}

fn compare_step(function: u8, value: u32, mask: u32, reference: u32, header: u32, wait: WaitOn) -> Step {
    // INT-W28 (GX-7m CR): the reference is masked too, as the CP does: Linux's VM flush waits on the ack register
    // with reference = the request (0x00f80002 for VMID 1) and mask = 1 << vmid (gfx_v11_0.c reg_write_reg_wait).
    match pm4::compare(function, value, mask, reference & mask) {
        None => Step::Hang(Hang::BadField { header, what: "compare function" }),
        Some(true) => Step::Next,
        Some(false) => Step::Wait(wait),
    }
}

fn reg_write<B: GpuBus + ?Sized>(bus: &mut B, ev: &mut Vec<Event>, reg: u32, value: u32) {
    let modelled = bus.reg_write(reg, value);
    ev.push(Event::RegWrite { reg, value, modelled });
}

fn pm4_no_effect(opcode: u8) -> bool {
    use pm4::op::*;
    matches!(
        opcode,
        CLEAR_STATE
            | CONTEXT_CONTROL
            | PFP_SYNC_ME
            | EVENT_WRITE
            | ACQUIRE_MEM
            | PREAMBLE_CNTL
            | SWITCH_BUFFER
            | FRAME_CONTROL
            | INVALIDATE_TLBS
            // INT-W28 (GM-1c CR-GM1c-1): draws and dispatches run shaders the model does not execute; their
            // state packets go with them. Counted as no-effect work instead of halting the queue.
            | DRAW_INDEX_2
            | DRAW_INDEX_AUTO
            | DISPATCH_DIRECT
            | DISPATCH_INDIRECT
            | INDEX_TYPE
            | NUM_INSTANCES
            | INDEX_BASE
            | INDEX_BUFFER_SIZE
    )
}

/// DMA_DATA's fields (nvd.h PACKET3_DMA_DATA: CONTROL, SRC_ADDR_LO or DATA, SRC_ADDR_HI, DST_ADDR_LO,
/// DST_ADDR_HI, COMMAND | BYTE_COUNT).
mod dma {
    pub const DST_SHIFT: u32 = 20;
    pub const SRC_SHIFT: u32 = 29;
    /// DST_SEL / SRC_SEL: the address through DAS/SAS (memory when the COMMAND bit is 0), GDS, DATA (source
    /// only), the address through L2.
    pub const SEL_ADDR: u32 = 0;
    /// DST_SEL 2: nowhere (radeonsi's L2 prefetch: the source is read into L2, nothing is written).
    pub const DST_NOWHERE: u32 = 2;
    pub const SEL_DATA: u32 = 2;
    pub const SEL_L2: u32 = 3;
    pub const BYTE_COUNT: u32 = 0x3FF_FFFF;
    pub const CMD_SAS: u32 = 1 << 26;
    pub const CMD_DAS: u32 = 1 << 27;
    pub const CMD_SAIC: u32 = 1 << 28;
    pub const CMD_DAIC: u32 = 1 << 29;
}

/// COPY_DATA's control fields (count 4: CONTROL, SRC_LO, SRC_HI, DST_LO, DST_HI).
mod copy {
    pub const SEL_REG: u32 = 0;
    pub const SEL_MEM: u32 = 1;
    pub const SEL_L2: u32 = 2;
    pub const SEL_IMM: u32 = 5;
    pub const DST_MEM: u32 = 5;
    pub const COUNT_64: u32 = 1 << 16;
}

/// The byte budget of one run (DMA_DATA, SDMA COPY_LINEAR and CONST_FILL): `Some(step)` stops the packet.
fn take_bytes(bytes: u64, bytes_left: &mut u64, max_bytes: u64, header: u32) -> Option<Step> {
    if bytes > max_bytes {
        return Some(Step::Hang(Hang::TooLarge { header }));
    }
    if bytes > *bytes_left {
        return Some(Step::Budget);
    }
    *bytes_left -= bytes;
    None
}

/// Copies `bytes` from `src` to `dst` in `vmid` in chunks (the caller took the budget).
fn copy_mem<B: GpuBus + ?Sized>(bus: &mut B, vmid: u8, src: u64, dst: u64, bytes: u64) -> Result<(), MemFault> {
    let mut buf = [0u8; CHUNK];
    let mut done = 0u64;
    while done < bytes {
        let n = (bytes - done).min(CHUNK as u64) as usize;
        bus.read(vmid, src.wrapping_add(done), &mut buf[..n])?;
        bus.write(vmid, dst.wrapping_add(done), &buf[..n])?;
        done += n as u64;
    }
    Ok(())
}

/// Fills `bytes` at `dst` with the 4-byte `pattern` (the caller took the budget).
fn fill_mem<B: GpuBus + ?Sized>(bus: &mut B, vmid: u8, dst: u64, pattern: [u8; 4], bytes: u64) -> Result<(), MemFault> {
    let mut buf = [0u8; CHUNK];
    for (i, b) in buf.iter_mut().enumerate() {
        *b = pattern[i % 4];
    }
    let mut done = 0u64;
    while done < bytes {
        let n = (bytes - done).min(CHUNK as u64) as usize;
        bus.write(vmid, dst.wrapping_add(done), &buf[..n])?;
        done += n as u64;
    }
    Ok(())
}

/// INT-W28 (GM-1c CR-GM1c-1): DMA_DATA from memory or DATA to memory; GDS, register sides and fixed (non
/// incrementing) addresses are not modelled and halt.
fn pm4_dma_data<B: GpuBus + ?Sized>(bus: &mut B, vmid: u8, header: u32, body: &[u32], ev: &mut Vec<Event>, bytes_left: &mut u64, max_bytes: u64) -> Step {
    let bad = |what| Step::Hang(Hang::BadField { header, what });
    let [control, src_lo, src_hi, dst_lo, dst_hi, command] = match body {
        [a, b, c, d, e, f] => [*a, *b, *c, *d, *e, *f],
        _ => return bad("DMA_DATA count"),
    };
    let dst_sel = (control >> dma::DST_SHIFT) & 3;
    let src_sel = (control >> dma::SRC_SHIFT) & 3;
    if dst_sel == dma::DST_NOWHERE {
        // A prefetch only warms the cache the model does not have.
        ev.push(Event::NoEffect { header });
        return Step::Next;
    }
    let dst_mem = match dst_sel {
        dma::SEL_ADDR => command & dma::CMD_DAS == 0,
        dma::SEL_L2 => true,
        _ => false,
    };
    let src_mem = match src_sel {
        dma::SEL_ADDR => command & dma::CMD_SAS == 0,
        dma::SEL_L2 => true,
        _ => false,
    };
    if !dst_mem || !(src_mem || src_sel == dma::SEL_DATA) || command & dma::CMD_DAIC != 0 || (src_mem && command & dma::CMD_SAIC != 0) {
        return Step::Hang(Hang::Unsupported { header });
    }
    let dst = u64::from(dst_hi) << 32 | u64::from(dst_lo);
    let bytes = u64::from(command & dma::BYTE_COUNT);
    if bytes == 0 {
        return Step::Next;
    }
    if src_sel == dma::SEL_DATA && (dst & 3 != 0 || bytes & 3 != 0) {
        return bad("DMA_DATA fill alignment");
    }
    if let Some(s) = take_bytes(bytes, bytes_left, max_bytes, header) {
        return s;
    }
    let r = if src_sel == dma::SEL_DATA {
        fill_mem(bus, vmid, dst, src_lo.to_le_bytes(), bytes)
    } else {
        copy_mem(bus, vmid, u64::from(src_hi) << 32 | u64::from(src_lo), dst, bytes)
    };
    if let Err(f) = r {
        return Step::Hang(Hang::Fault(f));
    }
    ev.push(Event::MemWrite { vmid, addr: dst, bytes });
    Step::Next
}

/// INT-W28 (GM-1c CR-GM1c-1): COPY_DATA of one or two dwords from a register, memory or the immediate source
/// to memory or a register.
fn pm4_copy_data<B: GpuBus + ?Sized>(bus: &mut B, vmid: u8, header: u32, body: &[u32], ev: &mut Vec<Event>) -> Step {
    let bad = |what| Step::Hang(Hang::BadField { header, what });
    let [control, src_lo, src_hi, dst_lo, dst_hi] = match body {
        [a, b, c, d, e] => [*a, *b, *c, *d, *e],
        _ => return bad("COPY_DATA count"),
    };
    let wide = control & copy::COUNT_64 != 0;
    let n = if wide { 2 } else { 1 };
    let src_addr = u64::from(src_hi) << 32 | u64::from(src_lo);
    let mut v = [0u32; 2];
    match control & 0xF {
        copy::SEL_REG => {
            for (i, x) in v.iter_mut().take(n).enumerate() {
                *x = bus.reg_read(src_lo.wrapping_add(i as u32)).unwrap_or(0);
            }
        }
        copy::SEL_MEM | copy::SEL_L2 => {
            if src_addr & 3 != 0 {
                return bad("COPY_DATA source alignment");
            }
            for (i, x) in v.iter_mut().take(n).enumerate() {
                match read_u32(bus, vmid, src_addr.wrapping_add(i as u64 * 4)) {
                    Ok(d) => *x = d,
                    Err(f) => return Step::Hang(Hang::Fault(f)),
                }
            }
        }
        copy::SEL_IMM => v = [src_lo, src_hi],
        _ => return Step::Hang(Hang::Unsupported { header }),
    }
    match (control >> 8) & 0xF {
        copy::SEL_REG => {
            for (i, &x) in v.iter().take(n).enumerate() {
                reg_write(bus, ev, dst_lo.wrapping_add(i as u32), x);
            }
            Step::Next
        }
        copy::DST_MEM | copy::SEL_L2 => {
            let dst = u64::from(dst_hi) << 32 | u64::from(dst_lo);
            if dst & 3 != 0 {
                return bad("COPY_DATA destination alignment");
            }
            if let Err(f) = write_dwords(bus, vmid, dst, &v[..n]) {
                return Step::Hang(Hang::Fault(f));
            }
            ev.push(Event::MemWrite { vmid, addr: dst, bytes: n as u64 * 4 });
            Step::Next
        }
        _ => Step::Hang(Hang::Unsupported { header }),
    }
}

/// `armed`: this packet waited before and its write half already ran (see [`Engine`]'s `armed`).
fn exec_pm4<B: GpuBus + ?Sized>(bus: &mut B, vmid: u8, words: &[u32], ev: &mut Vec<Event>, bytes_left: &mut u64, max_bytes: u64, armed: bool) -> Step {
    use pm4::Packet as P;
    let header = words[0];
    let bad = |what| Step::Hang(Hang::BadField { header, what });
    let (pkt, _) = match pm4::decode(words) {
        Ok(p) => p,
        Err(e) => return Step::Hang(Hang::Pm4(e)),
    };
    match pkt {
        P::Type2 | P::Pad | P::Nop { .. } => Step::Next,
        P::Type0 { .. } => Step::Hang(Hang::Unsupported { header }),
        P::WriteData { control, dst_sel, addr, data } => {
            let one = control & pm4::WR_ONE_ADDR != 0;
            match dst_sel {
                pm4::dst_sel::REG => {
                    let Ok(reg) = u32::try_from(addr) else {
                        return bad("WRITE_DATA register");
                    };
                    for (i, &v) in data.iter().enumerate() {
                        let r = if one { reg } else { reg.wrapping_add(i as u32) };
                        reg_write(bus, ev, r, v);
                    }
                    Step::Next
                }
                pm4::dst_sel::MEM_GRBM | pm4::dst_sel::GL2 | pm4::dst_sel::MEM => {
                    if addr & 3 != 0 {
                        return bad("WRITE_DATA address alignment");
                    }
                    let r = if one {
                        data.iter().try_for_each(|&d| write_dwords(bus, vmid, addr, &[d]))
                    } else {
                        write_dwords(bus, vmid, addr, data)
                    };
                    if let Err(f) = r {
                        return Step::Hang(Hang::Fault(f));
                    }
                    let bytes = if one { 4 } else { data.len() as u64 * 4 };
                    ev.push(Event::MemWrite { vmid, addr, bytes });
                    Step::Next
                }
                _ => Step::Hang(Hang::Unsupported { header }),
            }
        }
        P::ReleaseMem(r) => {
            let (align, wide) = match r.data_sel {
                pm4::data_sel::NONE => (1, false),
                pm4::data_sel::LOW32 => (4, false),
                pm4::data_sel::DATA64 => (8, true),
                _ => return Step::Hang(Hang::Unsupported { header }),
            };
            if r.int_sel > pm4::int_sel::AFTER_WRITE {
                return bad("RELEASE_MEM int_sel");
            }
            if r.addr & (align - 1) != 0 {
                return bad("RELEASE_MEM address alignment");
            }
            if r.data_sel != pm4::data_sel::NONE {
                let res = if wide {
                    bus.write(vmid, r.addr, &r.data.to_le_bytes())
                } else {
                    bus.write(vmid, r.addr, &(r.data as u32).to_le_bytes())
                };
                if let Err(f) = res {
                    return Step::Hang(Hang::Fault(f));
                }
                let value = if wide { r.data } else { r.data & 0xFFFF_FFFF };
                ev.push(Event::Fence { vmid, addr: r.addr, value, wide });
            }
            if r.int_sel != pm4::int_sel::NONE {
                ev.push(Event::Interrupt { src: IntSrc::EndOfPipe, ctx: r.ctx });
            }
            Step::Next
        }
        P::IndirectBuffer(ib) => {
            if ib.swap != 0 {
                return Step::Hang(Hang::Unsupported { header });
            }
            Step::Call { vmid: ib.vmid, base: ib.addr, len_dw: ib.len_dw as u64 }
        }
        P::CondExec { addr, skip } => {
            if addr & 3 != 0 {
                return bad("COND_EXEC address alignment");
            }
            match read_u32(bus, vmid, addr) {
                Err(f) => Step::Hang(Hang::Fault(f)),
                Ok(0) => Step::Skip(skip as u64),
                Ok(_) => Step::Next,
            }
        }
        P::WaitRegMem(w) => {
            if w.mem && w.operation != 0 {
                return bad("WAIT_REG_MEM write-wait on memory");
            }
            if w.operation > 1 {
                return bad("WAIT_REG_MEM operation");
            }
            if w.operation == 1 && !armed {
                reg_write(bus, ev, w.addr0, w.reference);
            }
            if w.mem {
                let addr = w.mem_addr();
                if addr & 3 != 0 {
                    return bad("WAIT_REG_MEM address alignment");
                }
                let value = match read_u32(bus, vmid, addr) {
                    Ok(v) => v,
                    Err(f) => return Step::Hang(Hang::Fault(f)),
                };
                let on = WaitOn::Mem { vmid, addr, value, reference: w.reference, mask: w.mask, function: w.function };
                compare_step(w.function, value, w.mask, w.reference, header, on)
            } else {
                let reg = w.poll_reg();
                let value = bus.reg_read(reg);
                let on = WaitOn::Reg { reg, value, reference: w.reference, mask: w.mask, function: w.function };
                match value {
                    Some(v) => compare_step(w.function, v, w.mask, w.reference, header, on),
                    None if w.function == 0 => Step::Next,
                    None => Step::Wait(on),
                }
            }
        }
        P::SetReg { reg, values, .. } => {
            for (i, &v) in values.iter().enumerate() {
                reg_write(bus, ev, reg.wrapping_add(i as u32), v);
            }
            Step::Next
        }
        P::Other { opcode, .. } if opcode == pm4::op::HDP_FLUSH => {
            ev.push(Event::HdpFlush);
            Step::Next
        }
        P::Other { opcode, body, .. } if opcode == pm4::op::DMA_DATA => pm4_dma_data(bus, vmid, header, body, ev, bytes_left, max_bytes),
        P::Other { opcode, body, .. } if opcode == pm4::op::COPY_DATA => pm4_copy_data(bus, vmid, header, body, ev),
        P::Other { opcode, .. } if pm4_no_effect(opcode) => {
            ev.push(Event::NoEffect { header });
            Step::Next
        }
        P::Other { .. } => Step::Hang(Hang::Unsupported { header }),
    }
}

fn exec_sdma<B: GpuBus + ?Sized>(
    bus: &mut B,
    vmid: u8,
    words: &[u32],
    ev: &mut Vec<Event>,
    bytes_left: &mut u64,
    max_bytes: u64,
    armed: bool,
) -> Step {
    use sdma::Packet as P;
    let header = words[0];
    let bad = |what| Step::Hang(Hang::BadField { header, what });
    let (pkt, _) = match sdma::decode(words) {
        Ok(p) => p,
        Err(e) => return Step::Hang(Hang::Sdma(e)),
    };
    let mut take = |bytes: u64| take_bytes(bytes, bytes_left, max_bytes, header);
    match pkt {
        P::Nop { .. } => Step::Next,
        P::WriteLinear { addr, data } => {
            if addr & 3 != 0 {
                return bad("WRITE_LINEAR address alignment");
            }
            if let Err(f) = write_dwords(bus, vmid, addr, data) {
                return Step::Hang(Hang::Fault(f));
            }
            ev.push(Event::MemWrite { vmid, addr, bytes: data.len() as u64 * 4 });
            Step::Next
        }
        P::CopyLinear { src, dst, bytes, header: h, .. } => {
            // TMZ (18), backwards (25) and broadcast (27) copies are not modelled.
            if h & ((1 << 18) | (1 << 25) | (1 << 27)) != 0 {
                return Step::Hang(Hang::Unsupported { header });
            }
            if let Some(s) = take(bytes) {
                return s;
            }
            let mut buf = [0u8; CHUNK];
            let mut done = 0u64;
            while done < bytes {
                let n = (bytes - done).min(CHUNK as u64) as usize;
                let r = bus
                    .read(vmid, src.wrapping_add(done), &mut buf[..n])
                    .and_then(|_| bus.write(vmid, dst.wrapping_add(done), &buf[..n]));
                if let Err(f) = r {
                    return Step::Hang(Hang::Fault(f));
                }
                done += n as u64;
            }
            ev.push(Event::MemWrite { vmid, addr: dst, bytes });
            Step::Next
        }
        P::ConstFill { dst, data, bytes, fillsize } => {
            let pattern: [u8; 4] = match fillsize {
                sdma::FILL_DWORD => {
                    if dst & 3 != 0 || bytes & 3 != 0 {
                        return bad("CONST_FILL dword alignment");
                    }
                    data.to_le_bytes()
                }
                sdma::FILL_BYTE => [data as u8; 4],
                _ => return Step::Hang(Hang::Unsupported { header }),
            };
            if let Some(s) = take(bytes) {
                return s;
            }
            let mut buf = [0u8; CHUNK];
            for (i, b) in buf.iter_mut().enumerate() {
                *b = pattern[i % 4];
            }
            let mut done = 0u64;
            while done < bytes {
                let n = (bytes - done).min(CHUNK as u64) as usize;
                if let Err(f) = bus.write(vmid, dst.wrapping_add(done), &buf[..n]) {
                    return Step::Hang(Hang::Fault(f));
                }
                done += n as u64;
            }
            ev.push(Event::MemWrite { vmid, addr: dst, bytes });
            Step::Next
        }
        P::Fence { addr, data, .. } => {
            if addr & 3 != 0 {
                return bad("FENCE address alignment");
            }
            if let Err(f) = bus.write(vmid, addr, &data.to_le_bytes()) {
                return Step::Hang(Hang::Fault(f));
            }
            ev.push(Event::Fence { vmid, addr, value: data as u64, wide: false });
            Step::Next
        }
        P::Trap { ctx } => {
            ev.push(Event::Interrupt { src: IntSrc::Trap, ctx });
            Step::Next
        }
        P::Indirect { vmid: v, addr, len_dw, .. } => {
            if addr & 31 != 0 {
                return bad("INDIRECT address alignment");
            }
            Step::Call { vmid: v, base: addr, len_dw: len_dw as u64 }
        }
        P::PollRegMem(p) => {
            if p.function > 6 {
                return bad("POLL_REGMEM function");
            }
            if p.hdp_flush && !armed {
                // Request register in dword 2, done register in dword 1 (byte offsets).
                ev.push(Event::HdpFlush);
                reg_write(bus, ev, p.addr_hi >> 2, p.reference);
            }
            if p.mem && !p.hdp_flush {
                let addr = p.mem_addr();
                if addr & 3 != 0 {
                    return bad("POLL_REGMEM address alignment");
                }
                let value = match read_u32(bus, vmid, addr) {
                    Ok(v) => v,
                    Err(f) => return Step::Hang(Hang::Fault(f)),
                };
                let on = WaitOn::Mem { vmid, addr, value, reference: p.reference, mask: p.mask, function: p.function };
                compare_step(p.function, value, p.mask, p.reference, header, on)
            } else {
                let reg = p.addr_lo >> 2;
                let value = bus.reg_read(reg);
                let on = WaitOn::Reg { reg, value, reference: p.reference, mask: p.mask, function: p.function };
                match value {
                    Some(v) => compare_step(p.function, v, p.mask, p.reference, header, on),
                    None if p.function == 0 => Step::Next,
                    None => Step::Wait(on),
                }
            }
        }
        P::SrbmWrite { reg, value, .. } => {
            reg_write(bus, ev, reg, value);
            Step::Next
        }
        P::CondExe { addr, reference, count } => {
            if addr & 3 != 0 {
                return bad("COND_EXE address alignment");
            }
            match read_u32(bus, vmid, addr) {
                Err(f) => Step::Hang(Hang::Fault(f)),
                Ok(v) if v == reference => Step::Next,
                Ok(_) => Step::Skip(count as u64),
            }
        }
        P::GcrReq { .. } => {
            ev.push(Event::NoEffect { header });
            Step::Next
        }
        P::Timestamp { .. } | P::PtePde { .. } => Step::Hang(Hang::Unsupported { header }),
    }
}
