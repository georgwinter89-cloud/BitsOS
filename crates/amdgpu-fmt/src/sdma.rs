//! SDMA 6.0 packets (the 780M's SDMA 6.0.1): encoders, a strict decoder and readable text.
//!
//! Opcodes from linux-6.6/drivers/gpu/drm/amd/amdgpu/navi10_sdma_pkt_open.h:26-65, field positions from
//! sdma_v6_0_0_pkt_open.h (6.0 additions), packet use from sdma_v6_0.c (insert_nop :224, emit_ib :247,
//! emit_mem_sync :282, emit_hdp_flush :307, emit_fence :338, pad_ib :1129, emit_wreg :1208, reg_wait :1217,
//! copy/fill :1631/:1658). A header is `sub_op << 8 | op` plus per-packet bits in 16..31.

use core::fmt;

use alloc::vec::Vec;

use crate::encode::{EncodeError, push_addr};

pub mod op {
    pub const NOP: u8 = 0;
    pub const COPY: u8 = 1;
    pub const WRITE: u8 = 2;
    pub const INDIRECT: u8 = 4;
    pub const FENCE: u8 = 5;
    pub const TRAP: u8 = 6;
    pub const SEM: u8 = 7;
    pub const POLL_REGMEM: u8 = 8;
    pub const COND_EXE: u8 = 9;
    pub const ATOMIC: u8 = 10;
    pub const CONST_FILL: u8 = 11;
    pub const PTEPDE: u8 = 12;
    pub const TIMESTAMP: u8 = 13;
    pub const SRBM_WRITE: u8 = 14;
    pub const PRE_EXE: u8 = 15;
    pub const GPUVM_INV: u8 = 16;
    pub const GCR_REQ: u8 = 17;
    pub const DUMMY_TRAP: u8 = 32;
}

pub const SUBOP_COPY_LINEAR: u8 = 0;
pub const SUBOP_WRITE_LINEAR: u8 = 0;
pub const SUBOP_PTEPDE_GEN: u8 = 0;

/// Name of an opcode for dumps.
pub fn op_name(opcode: u8) -> Option<&'static str> {
    Some(match opcode {
        op::NOP => "NOP",
        op::COPY => "COPY",
        op::WRITE => "WRITE",
        op::INDIRECT => "INDIRECT",
        op::FENCE => "FENCE",
        op::TRAP => "TRAP",
        op::SEM => "SEM",
        op::POLL_REGMEM => "POLL_REGMEM",
        op::COND_EXE => "COND_EXE",
        op::ATOMIC => "ATOMIC",
        op::CONST_FILL => "CONST_FILL",
        op::PTEPDE => "PTEPDE",
        op::TIMESTAMP => "TIMESTAMP",
        op::SRBM_WRITE => "SRBM_WRITE",
        op::PRE_EXE => "PRE_EXE",
        op::GPUVM_INV => "GPUVM_INV",
        op::GCR_REQ => "GCR_REQ",
        op::DUMMY_TRAP => "DUMMY_TRAP",
        _ => return None,
    })
}

pub const fn header(opcode: u8, sub: u8) -> u32 {
    ((sub as u32) << 8) | opcode as u32
}

/// The ring's NOP dword (sdma_v6_0.c:1558).
pub const NOP: u32 = header(op::NOP, 0);
/// NOP count field (bits 16..29): a burst NOP covers `1 + count` dwords.
pub const NOP_MAX_COUNT: u32 = 0x3FFF;
/// WRITE_LINEAR count field (20 bits, dwords - 1).
pub const WRITE_MAX_DW: u32 = 0x10_0000;
/// COPY_LINEAR and CONST_FILL count field (30 bits, bytes - 1). Linux splits copies at 4 MiB
/// (sdma_v6_0.c:1669 copy_max_bytes); the field allows 1 GiB.
pub const MAX_BYTES: u64 = 1 << 30;
/// INDIRECT size field (20 bits, dwords).
pub const IB_MAX_DW: u32 = 0xF_FFFF;
/// FENCE memory type Linux uses (UC, sdma_v6_0.c:345).
pub const FENCE_MTYPE_UC: u8 = 3;
/// CONST_FILL fill size: 0 bytes, 2 dwords (the field at header bits 30..31).
pub const FILL_BYTE: u8 = 0;
pub const FILL_DWORD: u8 = 2;

/// One decoded packet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Packet<'a> {
    /// NOP covering `len` dwords (1 + the count field: Linux's burst form).
    Nop { len: u32 },
    WriteLinear { addr: u64, data: &'a [u32] },
    CopyLinear { src: u64, dst: u64, bytes: u64, header: u32, params: u32 },
    ConstFill { dst: u64, data: u32, bytes: u64, fillsize: u8 },
    Fence { addr: u64, data: u32, mtype: u8 },
    Trap { ctx: u32 },
    Indirect { vmid: u8, addr: u64, len_dw: u32, csa: u64, privileged: bool },
    PollRegMem(PollRegMem),
    SrbmWrite { reg: u32, value: u32, byte_en: u8 },
    /// Execute the next `count` dwords only if the dword at `addr` equals `reference`, else skip them.
    CondExe { addr: u64, reference: u32, count: u32 },
    GcrReq { payload: [u32; 4] },
    Timestamp { sub: u8, words: [u32; 2] },
    PtePde { pe: u64, mask: u64, value: u64, incr: u32, count: u32 },
}

/// A POLL_REGMEM packet: poll memory (`mem`) or a register until `(value & mask) func reference`; with
/// `hdp_flush` the engine first writes `reference` to the request register (`addr_hi >> 2`) and polls the
/// done register (`addr_lo >> 2`) (sdma_v6_0.c:307-321).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PollRegMem {
    pub hdp_flush: bool,
    pub mem: bool,
    pub function: u8,
    pub addr_lo: u32,
    pub addr_hi: u32,
    pub reference: u32,
    pub mask: u32,
    pub interval: u16,
    pub retry: u16,
}

impl PollRegMem {
    pub fn mem_addr(&self) -> u64 {
        self.addr_lo as u64 | ((self.addr_hi as u64) << 32)
    }
}

/// Why dwords do not decode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DecodeError {
    Truncated { need: usize, have: usize },
    /// An opcode/sub-op whose length this crate does not know (it cannot even be skipped).
    Unknown { header: u32 },
}

impl DecodeError {
    pub fn errno(&self) -> i32 {
        crate::errno::EINVAL
    }
}

pub fn hdr_op(h: u32) -> u8 {
    h as u8
}
pub fn hdr_sub(h: u32) -> u8 {
    (h >> 8) as u8
}

/// Length in dwords of the packet at `words[0]`; `Truncated` when more dwords are needed to tell.
pub fn packet_len(words: &[u32]) -> Result<usize, DecodeError> {
    let Some(&h) = words.first() else {
        return Err(DecodeError::Truncated { need: 1, have: 0 });
    };
    let unknown = DecodeError::Unknown { header: h };
    Ok(match (hdr_op(h), hdr_sub(h)) {
        (op::NOP, _) => 1 + ((h >> 16) & NOP_MAX_COUNT) as usize,
        (op::WRITE, SUBOP_WRITE_LINEAR) => {
            let Some(&c) = words.get(3) else {
                return Err(DecodeError::Truncated { need: 4, have: words.len() });
            };
            5 + (c & (WRITE_MAX_DW - 1)) as usize
        }
        (op::COPY, SUBOP_COPY_LINEAR) => 7,
        (op::INDIRECT, 0) => 6,
        (op::FENCE, 0) => 4,
        (op::TRAP, 0) => 2,
        (op::POLL_REGMEM, 0) => 6,
        (op::COND_EXE, 0) => 5,
        (op::CONST_FILL, 0) => 5,
        (op::PTEPDE, SUBOP_PTEPDE_GEN) => 10,
        (op::TIMESTAMP, 0..=2) => 3,
        (op::SRBM_WRITE, 0) => 3,
        (op::GCR_REQ, 0) => 5,
        _ => return Err(unknown),
    })
}

fn lohi(lo: u32, hi: u32) -> u64 {
    lo as u64 | ((hi as u64) << 32)
}

/// Decodes the packet at `words[0]`; returns it and its length.
pub fn decode(words: &[u32]) -> Result<(Packet<'_>, usize), DecodeError> {
    let n = packet_len(words)?;
    if words.len() < n {
        return Err(DecodeError::Truncated { need: n, have: words.len() });
    }
    let w = &words[..n];
    let h = w[0];
    let p = match hdr_op(h) {
        op::NOP => Packet::Nop { len: n as u32 },
        op::WRITE => Packet::WriteLinear { addr: lohi(w[1], w[2]), data: &w[4..] },
        op::COPY => Packet::CopyLinear {
            bytes: (w[1] & (MAX_BYTES as u32 - 1)) as u64 + 1,
            params: w[2],
            src: lohi(w[3], w[4]),
            dst: lohi(w[5], w[6]),
            header: h,
        },
        op::INDIRECT => Packet::Indirect {
            vmid: ((h >> 16) & 0xF) as u8,
            privileged: h & (1 << 31) != 0,
            addr: lohi(w[1], w[2]),
            len_dw: w[3] & IB_MAX_DW,
            csa: lohi(w[4], w[5]),
        },
        op::FENCE => Packet::Fence { addr: lohi(w[1], w[2]), data: w[3], mtype: ((h >> 16) & 7) as u8 },
        op::TRAP => Packet::Trap { ctx: w[1] & 0x0FFF_FFFF },
        op::POLL_REGMEM => Packet::PollRegMem(PollRegMem {
            hdp_flush: h & (1 << 26) != 0,
            mem: h & (1 << 31) != 0,
            function: ((h >> 28) & 7) as u8,
            addr_lo: w[1],
            addr_hi: w[2],
            reference: w[3],
            mask: w[4],
            interval: w[5] as u16,
            retry: ((w[5] >> 16) & 0xFFF) as u16,
        }),
        op::COND_EXE => Packet::CondExe { addr: lohi(w[1], w[2]), reference: w[3], count: w[4] & 0x3FFF },
        op::CONST_FILL => Packet::ConstFill {
            dst: lohi(w[1], w[2]),
            data: w[3],
            bytes: (w[4] & (MAX_BYTES as u32 - 1)) as u64 + 1,
            fillsize: (h >> 30) as u8,
        },
        op::PTEPDE => Packet::PtePde {
            pe: lohi(w[1], w[2]),
            mask: lohi(w[3], w[4]),
            value: lohi(w[5], w[6]),
            incr: w[7],
            count: w[9].wrapping_add(1),
        },
        op::TIMESTAMP => Packet::Timestamp { sub: hdr_sub(h), words: [w[1], w[2]] },
        op::SRBM_WRITE => Packet::SrbmWrite { reg: w[1] & 0x3FFFF, value: w[2], byte_en: (h >> 28) as u8 },
        _ => Packet::GcrReq { payload: [w[1], w[2], w[3], w[4]] },
    };
    Ok((p, n))
}

// ---- encoders ----

/// `n` dwords of NOP: one burst NOP (header count n-1) then n-1 NOP dwords when `burst`, else n NOP dwords
/// (sdma_v6_0.c:224-235). n = 0 appends nothing.
pub fn nops(out: &mut Vec<u32>, n: u32, burst: bool) -> Result<(), EncodeError> {
    if n == 0 {
        return Ok(());
    }
    if burst && n - 1 > NOP_MAX_COUNT {
        return Err(EncodeError::Length { what: "SDMA burst NOP", value: n as u64 });
    }
    for i in 0..n {
        out.push(if burst && i == 0 { NOP | ((n - 1) << 16) } else { NOP });
    }
    Ok(())
}

/// WRITE_LINEAR of `data` (1..=2^20 dwords) to `addr` (4-aligned).
pub fn write_linear(out: &mut Vec<u32>, addr: u64, data: &[u32]) -> Result<(), EncodeError> {
    if data.is_empty() || data.len() > WRITE_MAX_DW as usize {
        return Err(EncodeError::Length { what: "WRITE_LINEAR data", value: data.len() as u64 });
    }
    if addr & 3 != 0 {
        return Err(EncodeError::Misaligned { addr, align: 4 });
    }
    out.push(header(op::WRITE, SUBOP_WRITE_LINEAR));
    push_addr(out, addr);
    out.push(data.len() as u32 - 1);
    out.extend_from_slice(data);
    Ok(())
}

/// COPY_LINEAR of `bytes` (1..=2^30) bytes; `tmz` marks a protected copy (sdma_v6_0.c:1631-1646).
pub fn copy_linear(out: &mut Vec<u32>, src: u64, dst: u64, bytes: u64, tmz: bool) -> Result<(), EncodeError> {
    if bytes == 0 || bytes > MAX_BYTES {
        return Err(EncodeError::Length { what: "COPY_LINEAR bytes", value: bytes });
    }
    out.push(header(op::COPY, SUBOP_COPY_LINEAR) | ((tmz as u32) << 18));
    out.push(bytes as u32 - 1);
    out.push(0);
    push_addr(out, src);
    push_addr(out, dst);
    Ok(())
}

/// CONST_FILL of `bytes` bytes at `dst` with `data`. `fillsize` [`FILL_DWORD`] needs `dst` and `bytes`
/// 4-aligned; Linux emits [`FILL_BYTE`] (sdma_v6_0.c:1658-1667).
pub fn const_fill(out: &mut Vec<u32>, dst: u64, data: u32, bytes: u64, fillsize: u8) -> Result<(), EncodeError> {
    if bytes == 0 || bytes > MAX_BYTES {
        return Err(EncodeError::Length { what: "CONST_FILL bytes", value: bytes });
    }
    match fillsize {
        FILL_BYTE => {}
        FILL_DWORD if dst & 3 != 0 => return Err(EncodeError::Misaligned { addr: dst, align: 4 }),
        FILL_DWORD if bytes & 3 != 0 => return Err(EncodeError::Length { what: "CONST_FILL dwords", value: bytes }),
        FILL_DWORD => {}
        x => return Err(EncodeError::Field { what: "CONST_FILL fillsize", value: x as u64 }),
    }
    out.push(header(op::CONST_FILL, 0) | ((fillsize as u32) << 30));
    push_addr(out, dst);
    out.push(data);
    out.push(bytes as u32 - 1);
    Ok(())
}

/// FENCE: writes `data` to `addr` (4-aligned) with memory type UC.
pub fn fence(out: &mut Vec<u32>, addr: u64, data: u32) -> Result<(), EncodeError> {
    if addr & 3 != 0 {
        return Err(EncodeError::Misaligned { addr, align: 4 });
    }
    out.push(header(op::FENCE, 0) | ((FENCE_MTYPE_UC as u32) << 16));
    push_addr(out, addr);
    out.push(data);
    Ok(())
}

/// TRAP: an interrupt with context `ctx` (28 bits).
pub fn trap(out: &mut Vec<u32>, ctx: u32) -> Result<(), EncodeError> {
    if ctx > 0x0FFF_FFFF {
        return Err(EncodeError::Field { what: "TRAP context", value: ctx as u64 });
    }
    out.push(header(op::TRAP, 0));
    out.push(ctx);
    Ok(())
}

/// Linux's SDMA fence (sdma_v6_0.c:338-370): FENCE of the low half, a second FENCE of the high half at
/// `addr + 4` when `wide` (two writes: a reader can see the new low and the old high), TRAP when `int`.
pub fn emit_fence(out: &mut Vec<u32>, addr: u64, seq: u64, wide: bool, int: bool) -> Result<(), EncodeError> {
    if addr & 3 != 0 || addr > u64::MAX - 4 {
        return Err(EncodeError::Misaligned { addr, align: 4 });
    }
    fence(out, addr, seq as u32)?;
    if wide {
        fence(out, addr + 4, (seq >> 32) as u32)?;
    }
    if int {
        trap(out, 0)?;
    }
    Ok(())
}

/// INDIRECT: run the IB of `len_dw` dwords at `addr` (32-byte aligned) in `vmid` (sdma_v6_0.c:247-270).
pub fn indirect(out: &mut Vec<u32>, vmid: u8, addr: u64, len_dw: u32, csa: u64) -> Result<(), EncodeError> {
    if addr & 31 != 0 {
        return Err(EncodeError::Misaligned { addr, align: 32 });
    }
    if len_dw > IB_MAX_DW {
        return Err(EncodeError::Length { what: "SDMA IB", value: len_dw as u64 });
    }
    if vmid > 15 {
        return Err(EncodeError::Field { what: "vmid", value: vmid as u64 });
    }
    out.push(header(op::INDIRECT, 0) | ((vmid as u32) << 16));
    push_addr(out, addr);
    out.push(len_dw);
    push_addr(out, csa);
    Ok(())
}

/// POLL_REGMEM.
pub fn poll_regmem(out: &mut Vec<u32>, p: &PollRegMem) -> Result<(), EncodeError> {
    if p.function > 6 || p.retry > 0xFFF {
        return Err(EncodeError::Field { what: "POLL_REGMEM func/retry", value: p.function as u64 });
    }
    if p.mem && p.addr_lo & 3 != 0 {
        return Err(EncodeError::Misaligned { addr: p.mem_addr(), align: 4 });
    }
    out.push(
        header(op::POLL_REGMEM, 0)
            | ((p.hdp_flush as u32) << 26)
            | ((p.function as u32) << 28)
            | ((p.mem as u32) << 31),
    );
    out.extend_from_slice(&[p.addr_lo, p.addr_hi, p.reference, p.mask]);
    out.push(p.interval as u32 | ((p.retry as u32) << 16));
    Ok(())
}

/// SRBM_WRITE of `value` to register `reg` (dword offset, 18 bits), all bytes enabled (sdma_v6_0.c:1208).
pub fn srbm_write(out: &mut Vec<u32>, reg: u32, value: u32) -> Result<(), EncodeError> {
    if reg > 0x3FFFF {
        return Err(EncodeError::Field { what: "SRBM_WRITE register", value: reg as u64 });
    }
    out.extend_from_slice(&[header(op::SRBM_WRITE, 0) | (0xF << 28), reg, value]);
    Ok(())
}

/// COND_EXE: the next `count` dwords run only if the dword at `addr` (4-aligned) equals `reference`.
pub fn cond_exe(out: &mut Vec<u32>, addr: u64, reference: u32, count: u32) -> Result<(), EncodeError> {
    if addr & 3 != 0 {
        return Err(EncodeError::Misaligned { addr, align: 4 });
    }
    if count > 0x3FFF {
        return Err(EncodeError::Length { what: "COND_EXE count", value: count as u64 });
    }
    out.push(header(op::COND_EXE, 0));
    push_addr(out, addr);
    out.push(reference);
    out.push(count);
    Ok(())
}

// ---- text ----

impl fmt::Display for Packet<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Packet::Nop { len } => write!(f, "NOP len={len}"),
            Packet::WriteLinear { addr, data } => {
                write!(f, "WRITE_LINEAR addr=0x{addr:x} n={}", data.len())?;
                for d in data.iter().take(4) {
                    write!(f, " {d:08x}")?;
                }
                if data.len() > 4 {
                    f.write_str(" ..")?;
                }
                Ok(())
            }
            Packet::CopyLinear { src, dst, bytes, .. } => {
                write!(f, "COPY_LINEAR src=0x{src:x} dst=0x{dst:x} bytes={bytes}")
            }
            Packet::ConstFill { dst, data, bytes, fillsize } => {
                write!(f, "CONST_FILL dst=0x{dst:x} data=0x{data:08x} bytes={bytes} fillsize={fillsize}")
            }
            Packet::Fence { addr, data, mtype } => write!(f, "FENCE addr=0x{addr:x} data=0x{data:08x} mtype={mtype}"),
            Packet::Trap { ctx } => write!(f, "TRAP ctx=0x{ctx:x}"),
            Packet::Indirect { vmid, addr, len_dw, csa, privileged } => write!(
                f,
                "INDIRECT vmid={vmid} addr=0x{addr:x} len={len_dw} csa=0x{csa:x}{}",
                if *privileged { " priv" } else { "" }
            ),
            Packet::PollRegMem(p) => write!(
                f,
                "POLL_REGMEM {}{} func={} lo=0x{:x} hi=0x{:x} ref=0x{:x} mask=0x{:x}",
                if p.mem { "mem" } else { "reg" },
                if p.hdp_flush { " hdp_flush" } else { "" },
                p.function,
                p.addr_lo,
                p.addr_hi,
                p.reference,
                p.mask
            ),
            Packet::SrbmWrite { reg, value, .. } => write!(f, "SRBM_WRITE reg=0x{reg:x} value=0x{value:08x}"),
            Packet::CondExe { addr, reference, count } => {
                write!(f, "COND_EXE addr=0x{addr:x} ref=0x{reference:x} count={count}")
            }
            Packet::GcrReq { payload } => write!(f, "GCR_REQ cntl=0x{:x}", payload[1] & 0xFFFF),
            Packet::Timestamp { sub, words } => write!(f, "TIMESTAMP sub={sub} 0x{:x}", lohi(words[0], words[1])),
            Packet::PtePde { pe, count, .. } => write!(f, "PTEPDE pe=0x{pe:x} count={count}"),
        }
    }
}
