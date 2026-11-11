//! PM4 command packets of the GFX 11 command processor (CP): encoders, a strict decoder and readable text.
//!
//! Bit positions follow linux-6.6/drivers/gpu/drm/amd/amdgpu/nvd.h:30-53 (headers), :88-140 (WRITE_DATA,
//! WAIT_REG_MEM, INDIRECT_BUFFER), :165-205 (RELEASE_MEM), :319-336 (register spaces); the packet layouts
//! are those gfx_v11_0.c emits (emit_fence :5357, emit_ib_gfx :5278, wait_reg_mem :291, cond_exec :5507).
//!
//! A type-3 header is `3 << 30 | count << 16 | opcode << 8`; the packet is `count + 2` dwords long. One
//! exception: a NOP whose count field is 0x3FFF ([`PAD`], Linux's ring and IB filler, gfx_v11_0.c:6085) is a
//! single dword; Linux fills rings with it one dword at a time (amdgpu_ring.c:109 insert_nop).

use core::fmt;

use crate::encode::{EncodeError, push_addr};

/// Largest count field (14 bits).
pub const MAX_COUNT: u32 = 0x3FFF;

/// Type-3 header.
pub const fn packet3(op: u8, count: u32) -> u32 {
    (3 << 30) | ((count & MAX_COUNT) << 16) | ((op as u32) << 8)
}

/// The one-dword filler: NOP with count 0x3FFF (0xffff1000).
pub const PAD: u32 = packet3(op::NOP, MAX_COUNT);

/// Type-2 filler (one dword, older CPs; the decoder accepts it).
pub const TYPE2: u32 = 2 << 30;

/// Opcodes this crate encodes, executes or names (nvd.h).
pub mod op {
    pub const NOP: u8 = 0x10;
    pub const CLEAR_STATE: u8 = 0x12;
    pub const INDEX_BUFFER_SIZE: u8 = 0x13;
    pub const DISPATCH_DIRECT: u8 = 0x15;
    pub const DISPATCH_INDIRECT: u8 = 0x16;
    pub const INDEX_BASE: u8 = 0x26;
    pub const DRAW_INDEX_2: u8 = 0x27;
    pub const INDEX_TYPE: u8 = 0x2A;
    pub const DRAW_INDEX_AUTO: u8 = 0x2D;
    pub const NUM_INSTANCES: u8 = 0x2F;
    pub const COND_EXEC: u8 = 0x22;
    pub const CONTEXT_CONTROL: u8 = 0x28;
    pub const INDIRECT_BUFFER_CNST: u8 = 0x33;
    pub const WRITE_DATA: u8 = 0x37;
    pub const WAIT_REG_MEM: u8 = 0x3C;
    pub const INDIRECT_BUFFER: u8 = 0x3F;
    pub const COPY_DATA: u8 = 0x40;
    pub const PFP_SYNC_ME: u8 = 0x42;
    pub const EVENT_WRITE: u8 = 0x46;
    pub const RELEASE_MEM: u8 = 0x49;
    pub const PREAMBLE_CNTL: u8 = 0x4A;
    pub const DMA_DATA: u8 = 0x50;
    pub const ACQUIRE_MEM: u8 = 0x58;
    pub const SET_CONFIG_REG: u8 = 0x68;
    pub const SET_CONTEXT_REG: u8 = 0x69;
    pub const SET_SH_REG: u8 = 0x76;
    pub const SET_UCONFIG_REG: u8 = 0x79;
    pub const SET_UCONFIG_REG_INDEX: u8 = 0x7A;
    pub const SET_SH_REG_INDEX: u8 = 0x9B;
    pub const LOAD_CONTEXT_REG_INDEX: u8 = 0x9F;
    pub const SWITCH_BUFFER: u8 = 0x8B;
    pub const FRAME_CONTROL: u8 = 0x90;
    pub const HDP_FLUSH: u8 = 0x95;
    pub const INVALIDATE_TLBS: u8 = 0x98;
    pub const SET_RESOURCES: u8 = 0xA0;
    pub const MAP_QUEUES: u8 = 0xA2;
    pub const UNMAP_QUEUES: u8 = 0xA3;
    pub const QUERY_STATUS: u8 = 0xA4;
}

/// Name of an opcode for dumps (`None`: not one this crate knows).
pub fn op_name(opcode: u8) -> Option<&'static str> {
    Some(match opcode {
        op::NOP => "NOP",
        op::CLEAR_STATE => "CLEAR_STATE",
        op::DISPATCH_DIRECT => "DISPATCH_DIRECT",
        op::INDEX_BUFFER_SIZE => "INDEX_BUFFER_SIZE",
        op::DISPATCH_INDIRECT => "DISPATCH_INDIRECT",
        op::COND_EXEC => "COND_EXEC",
        op::INDEX_BASE => "INDEX_BASE",
        op::DRAW_INDEX_2 => "DRAW_INDEX_2",
        op::INDEX_TYPE => "INDEX_TYPE",
        op::CONTEXT_CONTROL => "CONTEXT_CONTROL",
        op::DRAW_INDEX_AUTO => "DRAW_INDEX_AUTO",
        op::NUM_INSTANCES => "NUM_INSTANCES",
        op::INDIRECT_BUFFER_CNST => "INDIRECT_BUFFER_CNST",
        op::WRITE_DATA => "WRITE_DATA",
        op::WAIT_REG_MEM => "WAIT_REG_MEM",
        op::INDIRECT_BUFFER => "INDIRECT_BUFFER",
        op::COPY_DATA => "COPY_DATA",
        op::PFP_SYNC_ME => "PFP_SYNC_ME",
        op::EVENT_WRITE => "EVENT_WRITE",
        0x47 => "EVENT_WRITE_EOP",
        op::RELEASE_MEM => "RELEASE_MEM",
        op::PREAMBLE_CNTL => "PREAMBLE_CNTL",
        op::DMA_DATA => "DMA_DATA",
        op::ACQUIRE_MEM => "ACQUIRE_MEM",
        op::SET_CONFIG_REG => "SET_CONFIG_REG",
        op::SET_CONTEXT_REG => "SET_CONTEXT_REG",
        op::SET_SH_REG => "SET_SH_REG",
        op::SET_UCONFIG_REG => "SET_UCONFIG_REG",
        op::SET_UCONFIG_REG_INDEX => "SET_UCONFIG_REG_INDEX",
        op::SET_SH_REG_INDEX => "SET_SH_REG_INDEX",
        op::LOAD_CONTEXT_REG_INDEX => "LOAD_CONTEXT_REG_INDEX",
        op::SWITCH_BUFFER => "SWITCH_BUFFER",
        op::FRAME_CONTROL => "FRAME_CONTROL",
        op::HDP_FLUSH => "HDP_FLUSH",
        op::INVALIDATE_TLBS => "INVALIDATE_TLBS",
        op::SET_RESOURCES => "SET_RESOURCES",
        op::MAP_QUEUES => "MAP_QUEUES",
        op::UNMAP_QUEUES => "UNMAP_QUEUES",
        op::QUERY_STATUS => "QUERY_STATUS",
        _ => return None,
    })
}

/// Register space of a SET_*_REG packet; the packet carries the offset from the space's start (nvd.h:319-336).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RegSpace {
    Config,
    Sh,
    Context,
    Uconfig,
}

impl RegSpace {
    /// First register (dword offset) of the space.
    pub fn start(self) -> u32 {
        match self {
            RegSpace::Config => 0x2000,
            RegSpace::Sh => 0x2c00,
            RegSpace::Context => 0xa000,
            RegSpace::Uconfig => 0xc000,
        }
    }
    fn opcode(self) -> u8 {
        match self {
            RegSpace::Config => op::SET_CONFIG_REG,
            RegSpace::Sh => op::SET_SH_REG,
            RegSpace::Context => op::SET_CONTEXT_REG,
            RegSpace::Uconfig => op::SET_UCONFIG_REG,
        }
    }
    fn of(opcode: u8) -> Option<RegSpace> {
        Some(match opcode {
            op::SET_CONFIG_REG => RegSpace::Config,
            op::SET_SH_REG => RegSpace::Sh,
            op::SET_CONTEXT_REG => RegSpace::Context,
            op::SET_UCONFIG_REG => RegSpace::Uconfig,
            // INT-W28 (GM-1c): the _INDEX forms carry an index in bits 31:28 of the offset dword (radeonsi's
            // GRBM_GFX_INDEX and SH writes); the offset is the same field.
            op::SET_UCONFIG_REG_INDEX => RegSpace::Uconfig,
            op::SET_SH_REG_INDEX => RegSpace::Sh,
            _ => return None,
        })
    }
}

/// WRITE_DATA destination (nvd.h:89-96 DST_SEL).
pub mod dst_sel {
    pub const REG: u32 = 0;
    pub const MEM_GRBM: u32 = 1;
    pub const GL2: u32 = 2;
    pub const MEM: u32 = 5;
}
/// WRITE_DATA: all data dwords go to the one address.
pub const WR_ONE_ADDR: u32 = 1 << 16;
/// WRITE_DATA: the CP waits for the write to be confirmed.
pub const WR_CONFIRM: u32 = 1 << 20;

/// RELEASE_MEM first body dword (nvd.h:166-185).
pub mod rm {
    pub const fn event_type(x: u32) -> u32 {
        x & 0x3F
    }
    pub const fn event_index(x: u32) -> u32 {
        (x & 0xF) << 8
    }
    pub const GCR_GLM_WB: u32 = 1 << 12;
    pub const GCR_GLM_INV: u32 = 1 << 13;
    pub const GCR_GLV_INV: u32 = 1 << 14;
    pub const GCR_GL1_INV: u32 = 1 << 15;
    pub const GCR_GL2_US: u32 = 1 << 16;
    pub const GCR_GL2_RANGE: u32 = 1 << 17;
    pub const GCR_GL2_DISCARD: u32 = 1 << 19;
    pub const GCR_GL2_INV: u32 = 1 << 20;
    pub const GCR_GL2_WB: u32 = 1 << 21;
    pub const GCR_SEQ: u32 = 1 << 22;
    pub const fn cache_policy(x: u32) -> u32 {
        (x & 3) << 25
    }
    pub const EXECUTE: u32 = 1 << 28;
    /// The end-of-pipe event Linux uses for fences (CACHE_FLUSH_AND_INV_TS_EVENT).
    pub const CACHE_FLUSH_AND_INV_TS_EVENT: u32 = 0x14;
    /// The first body dword of Linux's GFX 11 fence: flush and invalidate every cache level, bypass policy,
    /// end-of-pipe timestamp event, index 5 (gfx_v11_0.c:5364-5375). Equals 0x0671f514.
    pub const GFX11_FENCE: u32 = GCR_SEQ
        | GCR_GL2_WB
        | GCR_GL2_INV
        | GCR_GL2_US
        | GCR_GL1_INV
        | GCR_GLV_INV
        | GCR_GLM_INV
        | GCR_GLM_WB
        | cache_policy(3)
        | event_type(CACHE_FLUSH_AND_INV_TS_EVENT)
        | event_index(5);
}

/// RELEASE_MEM DATA_SEL (nvd.h:188-194).
pub mod data_sel {
    pub const NONE: u8 = 0;
    pub const LOW32: u8 = 1;
    pub const DATA64: u8 = 2;
    pub const GPU_CLOCK: u8 = 3;
    pub const SYS_CLOCK: u8 = 4;
}
/// RELEASE_MEM INT_SEL (nvd.h:195-199).
pub mod int_sel {
    pub const NONE: u8 = 0;
    pub const INT_ONLY: u8 = 1;
    pub const AFTER_WRITE: u8 = 2;
}

/// A RELEASE_MEM packet (count 6, seven body dwords).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReleaseMem {
    /// Event, cache-flush and policy bits (body dword 1, see [`rm`]).
    pub event_cntl: u32,
    pub dst_sel: u8,
    pub int_sel: u8,
    pub data_sel: u8,
    pub addr: u64,
    pub data: u64,
    /// Interrupt context (body dword 7; Linux: 0 outside MES).
    pub ctx: u32,
}

impl ReleaseMem {
    pub fn event_type(&self) -> u32 {
        self.event_cntl & 0x3F
    }
    pub fn event_index(&self) -> u32 {
        (self.event_cntl >> 8) & 0xF
    }
}

/// A WAIT_REG_MEM packet (count 5; nvd.h:116-135).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WaitRegMem {
    /// 0 always, 1 <, 2 <=, 3 ==, 4 !=, 5 >=, 6 >.
    pub function: u8,
    /// Poll memory (`addr0 | addr1 << 32`) instead of register `addr0`.
    pub mem: bool,
    /// 1: write `reference` to register `addr0` first, then poll register `addr1` (wr_wait_wr_reg).
    pub operation: u8,
    /// 0 ME, 1 PFP.
    pub engine: u8,
    pub addr0: u32,
    pub addr1: u32,
    pub reference: u32,
    pub mask: u32,
    pub interval: u32,
}

impl WaitRegMem {
    fn control(&self) -> u32 {
        (self.function as u32 & 7)
            | ((self.mem as u32) << 4)
            | ((self.operation as u32 & 3) << 6)
            | ((self.engine as u32 & 3) << 8)
    }
    /// The register or memory dword to poll.
    pub fn poll_reg(&self) -> u32 {
        if self.operation == 1 { self.addr1 } else { self.addr0 }
    }
    pub fn mem_addr(&self) -> u64 {
        self.addr0 as u64 | ((self.addr1 as u64) << 32)
    }
}

/// Compare function shared by WAIT_REG_MEM and SDMA POLL_REGMEM (`None`: function 7, reserved).
pub fn compare(function: u8, value: u32, mask: u32, reference: u32) -> Option<bool> {
    let v = value & mask;
    Some(match function {
        0 => true,
        1 => v < reference,
        2 => v <= reference,
        3 => v == reference,
        4 => v != reference,
        5 => v >= reference,
        6 => v > reference,
        _ => return None,
    })
}

/// INDIRECT_BUFFER control dword fields (nvd.h:138-140; gfx_v11_0.c:5288 `length_dw | vmid << 24`).
pub const IB_SIZE_MASK: u32 = 0xF_FFFF;
pub const IB_VALID: u32 = 1 << 23;

/// An INDIRECT_BUFFER(_CNST) packet (count 2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IndirectBuffer {
    /// INDIRECT_BUFFER_CNST (constant engine on older chips; GFX 11 runs it like a normal IB).
    pub cnst: bool,
    pub addr: u64,
    /// Swap bits 1:0 of the address dword (big-endian hosts only).
    pub swap: u8,
    pub len_dw: u32,
    pub vmid: u8,
    /// The whole control dword (preemption, cache policy, valid bits).
    pub control: u32,
}

/// One decoded packet; slices borrow from the decoded dwords.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Packet<'a> {
    /// Type-0 register writes: `values.len()` registers from `reg` on.
    Type0 { reg: u32, values: &'a [u32] },
    /// Type-2 one-dword filler.
    Type2,
    /// The one-dword NOP filler [`PAD`].
    Pad,
    /// NOP with a body (ignored by the CP).
    Nop { body: &'a [u32] },
    WriteData { control: u32, dst_sel: u32, addr: u64, data: &'a [u32] },
    ReleaseMem(ReleaseMem),
    IndirectBuffer(IndirectBuffer),
    /// Skip the next `skip` dwords when the dword at `addr` is 0.
    CondExec { addr: u64, skip: u32 },
    WaitRegMem(WaitRegMem),
    SetReg { space: RegSpace, reg: u32, values: &'a [u32] },
    /// Any other type-3 packet (the body is not interpreted).
    Other { header: u32, opcode: u8, body: &'a [u32] },
}

/// Why dwords do not decode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DecodeError {
    /// The packet needs `need` dwords, only `have` are there.
    Truncated { need: usize, have: usize },
    /// Type-1 header (never valid).
    BadType { header: u32 },
    /// The count field does not fit the opcode's layout.
    BadCount { header: u32 },
}

impl DecodeError {
    pub fn errno(&self) -> i32 {
        crate::errno::EINVAL
    }
}

fn hdr_type(h: u32) -> u32 {
    h >> 30
}
fn hdr_count(h: u32) -> u32 {
    (h >> 16) & MAX_COUNT
}
/// Opcode of a type-3 header.
pub fn hdr_opcode(h: u32) -> u8 {
    (h >> 8) as u8
}

/// Length in dwords of the packet starting at `words[0]` (`Truncated` with `need: 1` when `words` is empty).
pub fn packet_len(words: &[u32]) -> Result<usize, DecodeError> {
    let Some(&h) = words.first() else {
        return Err(DecodeError::Truncated { need: 1, have: 0 });
    };
    match hdr_type(h) {
        0 => Ok(hdr_count(h) as usize + 2),
        1 => Err(DecodeError::BadType { header: h }),
        2 => Ok(1),
        _ if h == PAD || (hdr_opcode(h) == op::NOP && hdr_count(h) == MAX_COUNT) => Ok(1),
        _ => Ok(hdr_count(h) as usize + 2),
    }
}

fn lohi(lo: u32, hi: u32) -> u64 {
    lo as u64 | ((hi as u64) << 32)
}

/// Decodes the packet at `words[0]`; returns it and its length. Extra dwords after it are ignored.
pub fn decode(words: &[u32]) -> Result<(Packet<'_>, usize), DecodeError> {
    let n = packet_len(words)?;
    if words.len() < n {
        return Err(DecodeError::Truncated { need: n, have: words.len() });
    }
    let w = &words[..n];
    let h = w[0];
    let bad = DecodeError::BadCount { header: h };
    let p = match hdr_type(h) {
        0 => Packet::Type0 { reg: h & 0xFFFF, values: &w[1..] },
        2 => Packet::Type2,
        _ if n == 1 => Packet::Pad,
        _ => {
            let body = &w[1..];
            let opcode = hdr_opcode(h);
            match opcode {
                op::NOP => Packet::Nop { body },
                op::WRITE_DATA => {
                    if body.len() < 4 {
                        return Err(bad);
                    }
                    let control = body[0];
                    Packet::WriteData {
                        control,
                        dst_sel: (control >> 8) & 0xF,
                        addr: lohi(body[1], body[2]),
                        data: &body[3..],
                    }
                }
                op::RELEASE_MEM => {
                    if body.len() != 7 {
                        return Err(bad);
                    }
                    Packet::ReleaseMem(ReleaseMem {
                        event_cntl: body[0],
                        dst_sel: ((body[1] >> 16) & 3) as u8,
                        int_sel: ((body[1] >> 24) & 7) as u8,
                        data_sel: (body[1] >> 29) as u8,
                        addr: lohi(body[2], body[3]),
                        data: lohi(body[4], body[5]),
                        ctx: body[6],
                    })
                }
                op::INDIRECT_BUFFER | op::INDIRECT_BUFFER_CNST => {
                    if body.len() != 3 {
                        return Err(bad);
                    }
                    Packet::IndirectBuffer(IndirectBuffer {
                        cnst: opcode == op::INDIRECT_BUFFER_CNST,
                        addr: lohi(body[0] & !3, body[1] & 0xFFFF),
                        swap: (body[0] & 3) as u8,
                        len_dw: body[2] & IB_SIZE_MASK,
                        vmid: ((body[2] >> 24) & 0xF) as u8,
                        control: body[2],
                    })
                }
                op::COND_EXEC => {
                    if body.len() != 4 {
                        return Err(bad);
                    }
                    Packet::CondExec { addr: lohi(body[0], body[1]), skip: body[3] & MAX_COUNT }
                }
                op::WAIT_REG_MEM => {
                    if body.len() != 6 {
                        return Err(bad);
                    }
                    let c = body[0];
                    Packet::WaitRegMem(WaitRegMem {
                        function: (c & 7) as u8,
                        mem: c & (1 << 4) != 0,
                        operation: ((c >> 6) & 3) as u8,
                        engine: ((c >> 8) & 3) as u8,
                        addr0: body[1],
                        addr1: body[2],
                        reference: body[3],
                        mask: body[4],
                        interval: body[5],
                    })
                }
                _ => match RegSpace::of(opcode) {
                    Some(space) => {
                        if body.len() < 2 {
                            return Err(bad);
                        }
                        let off = body[0] & 0xFFFF;
                        Packet::SetReg { space, reg: space.start().wrapping_add(off), values: &body[1..] }
                    }
                    None => Packet::Other { header: h, opcode, body },
                },
            }
        }
    };
    Ok((p, n))
}

// ---- encoders ----

/// NOP with `body` (1..=0x3FFF dwords; the CP ignores it). For one-dword filling use [`PAD`].
pub fn nop(out: &mut alloc::vec::Vec<u32>, body: &[u32]) -> Result<(), EncodeError> {
    if body.is_empty() || body.len() > MAX_COUNT as usize {
        return Err(EncodeError::Length { what: "NOP body", value: body.len() as u64 });
    }
    out.push(packet3(op::NOP, body.len() as u32 - 1));
    out.extend_from_slice(body);
    Ok(())
}

/// WRITE_DATA of `data` to memory (`dst_sel` 1, 2 or 5; `addr` 4-aligned) or to register `addr` (dst_sel 0).
/// `flags` may add [`WR_ONE_ADDR`] and [`WR_CONFIRM`]; ENGINE_SEL stays ME.
pub fn write_data(
    out: &mut alloc::vec::Vec<u32>,
    dst: u32,
    addr: u64,
    flags: u32,
    data: &[u32],
) -> Result<(), EncodeError> {
    if data.is_empty() || data.len() > (MAX_COUNT - 2) as usize {
        return Err(EncodeError::Length { what: "WRITE_DATA data", value: data.len() as u64 });
    }
    if !matches!(dst, dst_sel::REG | dst_sel::MEM_GRBM | dst_sel::GL2 | dst_sel::MEM) {
        return Err(EncodeError::Field { what: "WRITE_DATA dst_sel", value: dst as u64 });
    }
    if dst != dst_sel::REG && addr & 3 != 0 {
        return Err(EncodeError::Misaligned { addr, align: 4 });
    }
    if dst == dst_sel::REG && addr > u32::MAX as u64 {
        return Err(EncodeError::Field { what: "WRITE_DATA register", value: addr });
    }
    out.push(packet3(op::WRITE_DATA, data.len() as u32 + 2));
    out.push((dst << 8) | (flags & (WR_ONE_ADDR | WR_CONFIRM)));
    push_addr(out, addr);
    out.extend_from_slice(data);
    Ok(())
}

/// RELEASE_MEM. The address must be 8-aligned for DATA_SEL 2..4 and 4-aligned for 1 (gfx_v11_0.c:5379-5386,
/// where Linux has BUG_ON).
pub fn release_mem(out: &mut alloc::vec::Vec<u32>, r: &ReleaseMem) -> Result<(), EncodeError> {
    let align = match r.data_sel {
        data_sel::NONE => 1,
        data_sel::LOW32 => 4,
        data_sel::DATA64 | data_sel::GPU_CLOCK | data_sel::SYS_CLOCK => 8,
        x => return Err(EncodeError::Field { what: "RELEASE_MEM data_sel", value: x as u64 }),
    };
    if r.addr & (align - 1) != 0 {
        return Err(EncodeError::Misaligned { addr: r.addr, align });
    }
    if r.int_sel > 2 || r.dst_sel > 1 {
        return Err(EncodeError::Field { what: "RELEASE_MEM int_sel/dst_sel", value: r.int_sel as u64 });
    }
    out.push(packet3(op::RELEASE_MEM, 6));
    out.push(r.event_cntl);
    out.push(((r.data_sel as u32) << 29) | ((r.int_sel as u32) << 24) | ((r.dst_sel as u32) << 16));
    push_addr(out, r.addr);
    out.push(r.data as u32);
    out.push((r.data >> 32) as u32);
    out.push(r.ctx);
    Ok(())
}

/// Linux's GFX 11 fence: RELEASE_MEM with [`rm::GFX11_FENCE`], 64-bit data when `wide`, interrupt after the
/// write when `int` (gfx_v11_0.c:5357-5392).
pub fn fence(out: &mut alloc::vec::Vec<u32>, addr: u64, seq: u64, wide: bool, int: bool) -> Result<(), EncodeError> {
    release_mem(
        out,
        &ReleaseMem {
            event_cntl: rm::GFX11_FENCE,
            dst_sel: 0,
            int_sel: if int { int_sel::AFTER_WRITE } else { int_sel::NONE },
            data_sel: if wide { data_sel::DATA64 } else { data_sel::LOW32 },
            addr,
            data: if wide { seq } else { seq & 0xFFFF_FFFF },
            ctx: 0,
        },
    )
}

/// INDIRECT_BUFFER (or _CNST) of `len_dw` dwords at `addr` (4-aligned, 48 bits) in `vmid`; `extra` adds
/// control bits such as [`IB_VALID`] (compute rings, gfx_v11_0.c:5318).
pub fn indirect_buffer(
    out: &mut alloc::vec::Vec<u32>,
    addr: u64,
    len_dw: u32,
    vmid: u8,
    cnst: bool,
    extra: u32,
) -> Result<(), EncodeError> {
    if addr & 3 != 0 {
        return Err(EncodeError::Misaligned { addr, align: 4 });
    }
    // A compute queue wants the upper half sign-extended (gfx_v11_0_ring_emit_ib_compute): it drops the job with
    // CP_HQD_ERROR.SUA_ERROR otherwise, measured on the 780M. So both forms are in range here.
    if addr >> 48 != 0 && addr >> 47 != 0x1_ffff {
        return Err(EncodeError::Field { what: "IB address", value: addr });
    }
    if len_dw > IB_SIZE_MASK {
        return Err(EncodeError::Length { what: "IB", value: len_dw as u64 });
    }
    if vmid > 15 {
        return Err(EncodeError::Field { what: "vmid", value: vmid as u64 });
    }
    let opc = if cnst { op::INDIRECT_BUFFER_CNST } else { op::INDIRECT_BUFFER };
    out.push(packet3(opc, 2));
    push_addr(out, addr);
    out.push((extra & !(IB_SIZE_MASK | (0xF << 24))) | len_dw | ((vmid as u32) << 24));
    Ok(())
}

/// WAIT_REG_MEM (memory address 4-aligned).
pub fn wait_reg_mem(out: &mut alloc::vec::Vec<u32>, w: &WaitRegMem) -> Result<(), EncodeError> {
    if w.function > 6 || w.operation > 1 || w.engine > 1 {
        return Err(EncodeError::Field { what: "WAIT_REG_MEM control", value: w.control() as u64 });
    }
    if w.mem && w.addr0 & 3 != 0 {
        return Err(EncodeError::Misaligned { addr: w.mem_addr(), align: 4 });
    }
    out.extend_from_slice(&[packet3(op::WAIT_REG_MEM, 5), w.control(), w.addr0, w.addr1, w.reference, w.mask, w.interval]);
    Ok(())
}

/// COND_EXEC: skip the next `skip` dwords when the dword at `addr` (4-aligned) is 0.
pub fn cond_exec(out: &mut alloc::vec::Vec<u32>, addr: u64, skip: u32) -> Result<(), EncodeError> {
    if addr & 3 != 0 {
        return Err(EncodeError::Misaligned { addr, align: 4 });
    }
    if skip > MAX_COUNT {
        return Err(EncodeError::Length { what: "COND_EXEC skip", value: skip as u64 });
    }
    out.push(packet3(op::COND_EXEC, 3));
    push_addr(out, addr);
    out.push(0);
    out.push(skip);
    Ok(())
}

/// SET_*_REG: `values` to consecutive registers from `reg` (a dword register offset inside `space`).
pub fn set_reg(out: &mut alloc::vec::Vec<u32>, space: RegSpace, reg: u32, values: &[u32]) -> Result<(), EncodeError> {
    let off = reg.wrapping_sub(space.start());
    if reg < space.start() || off > 0xFFFF {
        return Err(EncodeError::Field { what: "SET_REG register", value: reg as u64 });
    }
    if values.is_empty() || values.len() > (MAX_COUNT - 1) as usize {
        return Err(EncodeError::Length { what: "SET_REG values", value: values.len() as u64 });
    }
    out.push(packet3(space.opcode(), values.len() as u32));
    out.push(off);
    out.extend_from_slice(values);
    Ok(())
}

// ---- text ----

impl fmt::Display for Packet<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Packet::Type0 { reg, values } => write!(f, "TYPE0 reg=0x{reg:x} n={}", values.len()),
            Packet::Type2 => f.write_str("TYPE2"),
            Packet::Pad => f.write_str("NOP (pad)"),
            Packet::Nop { body } => write!(f, "NOP body={}", body.len()),
            Packet::WriteData { control, dst_sel, addr, data } => {
                write!(f, "WRITE_DATA dst_sel={dst_sel} addr=0x{addr:x} n={}", data.len())?;
                if control & WR_ONE_ADDR != 0 {
                    f.write_str(" one_addr")?;
                }
                if control & WR_CONFIRM != 0 {
                    f.write_str(" confirm")?;
                }
                if let Some(d) = data.first() {
                    write!(f, " data0=0x{d:08x}")?;
                }
                Ok(())
            }
            Packet::ReleaseMem(r) => write!(
                f,
                "RELEASE_MEM event=0x{:x} index={} cntl=0x{:08x} data_sel={} int_sel={} addr=0x{:x} data=0x{:x} ctx={}",
                r.event_type(),
                r.event_index(),
                r.event_cntl,
                r.data_sel,
                r.int_sel,
                r.addr,
                r.data,
                r.ctx
            ),
            Packet::IndirectBuffer(ib) => write!(
                f,
                "{} addr=0x{:x} len={} vmid={} control=0x{:08x}",
                if ib.cnst { "INDIRECT_BUFFER_CNST" } else { "INDIRECT_BUFFER" },
                ib.addr,
                ib.len_dw,
                ib.vmid,
                ib.control
            ),
            Packet::CondExec { addr, skip } => write!(f, "COND_EXEC addr=0x{addr:x} skip={skip}"),
            Packet::WaitRegMem(w) => write!(
                f,
                "WAIT_REG_MEM func={} {} op={} eng={} a0=0x{:x} a1=0x{:x} ref=0x{:x} mask=0x{:x}",
                w.function,
                if w.mem { "mem" } else { "reg" },
                w.operation,
                w.engine,
                w.addr0,
                w.addr1,
                w.reference,
                w.mask
            ),
            Packet::SetReg { space, reg, values } => {
                write!(f, "SET_{space:?}_REG reg=0x{reg:x} n={}", values.len())?;
                if let Some(v) = values.first() {
                    write!(f, " v0=0x{v:08x}")?;
                }
                Ok(())
            }
            Packet::Other { opcode, body, .. } => match op_name(*opcode) {
                Some(n) => write!(f, "{n} body={}", body.len()),
                None => write!(f, "OP_0x{opcode:02x} body={}", body.len()),
            },
        }
    }
}
