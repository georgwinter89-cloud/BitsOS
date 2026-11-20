//! VCN 4 (GX-8v): the unified queue's packets, the kernel's test IBs, the DPG SRAM list and the shared page the
//! firmware reads, as Linux 7.3.1 builds them (`linux-7.3.1/drivers/gpu/drm/amd/amdgpu/`: amdgpu_vcn.h:60-161,
//! vcn_v2_0.c:1722-1790 for the ring's packets, amdgpu_vcn.c:668-1114 for the test IBs, vcn_v4_0.c:148-165 for the
//! shared page). Pure: the driver writes the words, the fake VCPU reads them back with the same functions.
//!
//! The unified ring (VCN 4 has no separate decode ring) takes 64-dword frames (`align_mask` 0x3f), NOP 0, pointers
//! in dwords, a 32-bit fence followed by a TRAP. A user IB starts with the unified header: a checksum block over
//! the packed IB and an engine block (2 encode, 3 decode).

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use crate::encode::EncodeError;

/// The ring's packet opcodes (`VCN_ENC_CMD_*`, amdgpu_vcn.h:60-66).
pub mod cmd {
    pub const NO_OP: u32 = 0x0;
    pub const END: u32 = 0x1;
    pub const IB: u32 = 0x2;
    pub const FENCE: u32 = 0x3;
    pub const TRAP: u32 = 0x4;
    pub const REG_WRITE: u32 = 0xb;
    pub const REG_WAIT: u32 = 0xc;
}

/// `align_mask` 0x3f (vcn_v4_0.c:2005): every commit pads to 64 dwords.
pub const RING_ALIGN_DW: u64 = 64;
/// `emit_ib_size` (vcn_v4_0.c:2021).
pub const IB_DW: u64 = 5;
/// `vcn_v2_0_enc_ring_emit_fence`: FENCE (4 dwords) and TRAP.
pub const FENCE_DW: u64 = 5;
/// `AMDGPU_VCN_STACK_SIZE` and `AMDGPU_VCN_CONTEXT_SIZE` (amdgpu_vcn.h:29-30).
pub const STACK_BYTES: u64 = 128 << 10;
pub const CONTEXT_BYTES: u64 = 512 << 10;
/// The DPG SRAM buffer Linux allocates (amdgpu_vcn.c:239: 64 pairs).
pub const SRAM_BYTES: u64 = 64 * 2 * 4;
/// An IB's length field: Linux's multimedia limit (amdgpu_cs.c:49-62).
pub const IB_MAX_DW: u32 = 0x7F_FFF0;

/// IB: [IB, vmid, lo, hi, len] (`vcn_v2_0_enc_ring_emit_ib`). Linux masks nothing: an IB must be dword aligned.
pub fn ib(out: &mut Vec<u32>, vmid: u8, addr: u64, len_dw: u32) -> Result<(), EncodeError> {
    if addr & 3 != 0 {
        return Err(EncodeError::Misaligned { addr, align: 4 });
    }
    if len_dw == 0 || len_dw > IB_MAX_DW {
        return Err(EncodeError::Length { what: "VCN IB", value: u64::from(len_dw) });
    }
    if vmid > 15 {
        return Err(EncodeError::Field { what: "VMID", value: u64::from(vmid) });
    }
    out.extend([cmd::IB, u32::from(vmid), addr as u32, (addr >> 32) as u32, len_dw]);
    Ok(())
}

/// FENCE + TRAP: `seq`'s low 32 bits at `addr` (VCN fences are 32-bit only, the 64-bit flag WARNs in Linux).
pub fn fence(out: &mut Vec<u32>, addr: u64, seq: u64) -> Result<(), EncodeError> {
    if addr & 3 != 0 {
        return Err(EncodeError::Misaligned { addr, align: 4 });
    }
    out.extend([cmd::FENCE, addr as u32, (addr >> 32) as u32, seq as u32, cmd::TRAP]);
    Ok(())
}

/// REG_WRITE of a register given as its dword offset (the packet takes the byte, `reg << 2`).
pub fn reg_write(out: &mut Vec<u32>, reg_dw: u32, value: u32) {
    out.extend([cmd::REG_WRITE, reg_dw << 2, value]);
}

/// REG_WAIT until `(reg & mask) == value`.
pub fn reg_wait(out: &mut Vec<u32>, reg_dw: u32, mask: u32, value: u32) {
    out.extend([cmd::REG_WAIT, reg_dw << 2, mask, value]);
}

/// One ring packet as the fake VCPU and the hang report read it back.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Packet {
    Nop,
    End,
    Ib { vmid: u8, addr: u64, len_dw: u32 },
    Fence { addr: u64, seq: u32 },
    Trap,
    RegWrite { reg_dw: u32, value: u32 },
    RegWait { reg_dw: u32, mask: u32, value: u32 },
    /// An opcode the firmware does not know (the real VCPU's reaction is not documented).
    Unknown(u32),
}

impl Packet {
    /// The packet at `words[0]` and how many dwords it takes; `None`: it runs past the end.
    pub fn decode(words: &[u32]) -> Option<(Packet, usize)> {
        let w = |i: usize| words.get(i).copied();
        let p = match w(0)? {
            cmd::NO_OP => (Packet::Nop, 1),
            cmd::END => (Packet::End, 1),
            cmd::TRAP => (Packet::Trap, 1),
            cmd::IB => (Packet::Ib { vmid: w(1)? as u8, addr: u64::from(w(2)?) | u64::from(w(3)?) << 32, len_dw: w(4)? }, 5),
            cmd::FENCE => (Packet::Fence { addr: u64::from(w(1)?) | u64::from(w(2)?) << 32, seq: w(3)? }, 4),
            cmd::REG_WRITE => (Packet::RegWrite { reg_dw: w(1)? >> 2, value: w(2)? }, 3),
            cmd::REG_WAIT => (Packet::RegWait { reg_dw: w(1)? >> 2, mask: w(2)?, value: w(3)? }, 4),
            x => (Packet::Unknown(x), 1),
        };
        Some(p)
    }
}

/// The unified header's block ids (amdgpu_vcn.c:763-792; RADEON_VCN_* in vcn_v4_0.c).
pub const BLOCK_CHECKSUM: u32 = 0x3000_0002;
pub const BLOCK_ENGINE: u32 = 0x3000_0001;
/// `RADEON_VCN_ENGINE_TYPE_ENCODE` / `_DECODE`.
pub const ENGINE_ENCODE: u32 = 0x2;
pub const ENGINE_DECODE: u32 = 0x3;

/// The eight header dwords for `pack_dw` dwords of packed IB (`amdgpu_vcn_unified_ring_ib_header`); the checksum
/// (word 2) stays 0 until `seal`.
pub fn unified_header(pack_dw: u32, encode: bool) -> [u32; 8] {
    [0x10, BLOCK_CHECKSUM, 0, pack_dw, 0x10, BLOCK_ENGINE, if encode { ENGINE_ENCODE } else { ENGINE_DECODE }, pack_dw * 4]
}

/// `amdgpu_vcn_unified_ring_ib_checksum`: the sum of the `pack_dw` dwords after the checksum block into word 2.
pub fn seal(ib: &mut [u32]) -> Result<(), EncodeError> {
    let sum = checksum(ib).ok_or(EncodeError::Length { what: "unified IB", value: ib.len() as u64 })?;
    ib[2] = sum;
    Ok(())
}

/// The checksum the header's word 3 asks for, `None` when the IB is shorter.
pub fn checksum(ib: &[u32]) -> Option<u32> {
    let pack = *ib.get(3)? as usize;
    let body = ib.get(4..4usize.checked_add(pack)?)?;
    Some(body.iter().fold(0u32, |s, &w| s.wrapping_add(w)))
}

/// A unified IB as the firmware reads it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Unified {
    pub engine: u32,
    pub pack_dw: u32,
    pub checksum_ok: bool,
    /// The words after the eight header dwords.
    pub body: Vec<u32>,
}

/// Reads the header back; `None` when it is not a unified IB.
pub fn parse_unified(ib: &[u32]) -> Option<Unified> {
    if ib.len() < 8 || ib[0] != 0x10 || ib[1] != BLOCK_CHECKSUM || ib[4] != 0x10 || ib[5] != BLOCK_ENGINE {
        return None;
    }
    Some(Unified { engine: ib[6], pack_dw: ib[3], checksum_ok: checksum(ib) == Some(ib[2]), body: ib[8..].to_vec() })
}

/// The encoder's session ops of the IB test (amdgpu_vcn.c:960-961, :1020-1021).
pub const ENC_OP_INIT: u32 = 0x0800_0001;
pub const ENC_OP_CLOSE: u32 = 0x0800_0002;
/// The packed size Linux names for the session IBs (0x11 dwords).
const ENC_PACK_DW: u32 = 0x11;

/// `amdgpu_vcn_enc_get_create_msg` / `_destroy_msg` with the unified header: 24 dwords (16 + 8), sealed.
pub fn enc_session_ib(handle: u32, msg_addr: u64, op: u32) -> Vec<u32> {
    let mut v = unified_header(ENC_PACK_DW, true).to_vec();
    v.extend([0x18, 0x1, handle, (msg_addr >> 32) as u32, msg_addr as u32, 0]);
    v.extend([0x14, 0x2, 0x1c, 0, 0]);
    v.extend([0x8, op]);
    v.resize(24, 0);
    // Cannot fail: the header's pack (17) fits the 24 dwords.
    let _ = seal(&mut v);
    v
}

/// The decoder's create message of the IB test (amdgpu_vcn.c:668-697): 1024 dwords, a 1920x1088 stream.
pub fn dec_create_msg(handle: u32) -> Vec<u32> {
    let mut m = vec![0u32; 1024];
    m[..14].copy_from_slice(&[0x28, 0x38, 0x1, 0, handle, 0, 0x1, 0x28, 0x10, 0, 0x7, 0, 0x780, 0x440]);
    m
}

/// The decoder's destroy message (amdgpu_vcn.c:699-718).
pub fn dec_destroy_msg(handle: u32) -> Vec<u32> {
    let mut m = vec![0u32; 1024];
    m[..6].copy_from_slice(&[0x28, 0x18, 0, 0x2, handle, 0]);
    m
}

/// The message type (word 3) of a decode message: 0 create, 2 destroy (1 decode).
pub fn dec_msg_type(msg: &[u32]) -> Option<u32> {
    msg.get(3).copied()
}

/// `sizeof(struct amdgpu_vcn_decode_buffer)` in dwords (amdgpu_vcn.h:483-488).
pub const DECODE_BUFFER_DW: u32 = 33;
/// `AMDGPU_VCN_IB_FLAG_DECODE_BUFFER`, `AMDGPU_VCN_CMD_FLAG_MSG_BUFFER`.
pub const IB_FLAG_DECODE_BUFFER: u32 = 0x1;
pub const CMD_FLAG_MSG_BUFFER: u32 = 0x1;

/// `amdgpu_vcn_dec_sw_send_msg`'s IB with the unified header: 72 dwords pointing at a decode message.
pub fn dec_sw_ib(msg_addr: u64) -> Vec<u32> {
    let pack = DECODE_BUFFER_DW + 4 + 2;
    let mut v = unified_header(pack, false).to_vec();
    v.extend([DECODE_BUFFER_DW * 4 + 8, IB_FLAG_DECODE_BUFFER]);
    v.extend([CMD_FLAG_MSG_BUFFER, (msg_addr >> 32) as u32, msg_addr as u32]);
    v.resize(72, 0);
    let _ = seal(&mut v);
    v
}

/// The message a decode IB names (`vcn_v4_0_ring_patch_cs_in_place` reads the same words: engine block's +6..+8).
pub fn dec_msg_addr(u: &Unified) -> Option<u64> {
    let b = &u.body;
    (u.engine == ENGINE_DECODE && b.get(2)? & CMD_FLAG_MSG_BUFFER != 0).then(|| u64::from(b[3]) << 32 | u64::from(b[4]))
}

/// SOC15_DPG_MODE_OFFSET (amdgpu_vcn.h:101-142) with VCN 4's bases (vcn_v4_0.c:47-49): the VCPU's own offset of the
/// register at BAR5 byte `byte`, as the SRAM list names it.
pub fn dpg_offset(byte: u32) -> u32 {
    const VID_SOC: u32 = 0x1fb00;
    const VID1_SOC: u32 = 0x48300;
    const AON_SOC: u32 = 0x1f800;
    const AON1_SOC: u32 = 0x48000;
    const AON_IP: u32 = 0x30000;
    let a = byte & 0xF_FFFF;
    let within = |base: u32, len: u32| a >= base && a < base + len;
    let internal = if within(VID_SOC, 0x2600) {
        a - VID_SOC
    } else if within(AON_SOC, 0x600) {
        a - AON_SOC + AON_IP
    } else if within(VID1_SOC, 0x2600) {
        a - VID1_SOC
    } else if within(AON1_SOC, 0x600) {
        a - AON1_SOC + AON_IP
    } else {
        a
    };
    internal >> 2
}

/// The indirect SRAM list (`WREG32_SOC15_DPG_MODE` with `indirect`): (offset, value) pairs, loaded by the PSP.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Sram(pub Vec<u32>);

impl Sram {
    pub fn push(&mut self, byte: u32, value: u32) {
        self.0.extend([dpg_offset(byte), value]);
    }
    /// The size the PSP is told (`dpg_sram_curr_addr - dpg_sram_cpu_addr`).
    pub fn bytes(&self) -> u32 {
        (self.0.len() * 4) as u32
    }
    pub fn to_bytes(&self) -> Vec<u8> {
        self.0.iter().flat_map(|w| w.to_le_bytes()).collect()
    }
    /// Pairs read back from bytes (the fake PSP's side).
    pub fn pairs(bytes: &[u8]) -> Vec<(u32, u32)> {
        let w: Vec<u32> = bytes.chunks_exact(4).map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect();
        w.chunks_exact(2).map(|p| (p[0], p[1])).collect()
    }
}

/// `struct amdgpu_vcn4_fw_shared` (amdgpu_vcn.h:461-473, natural C layout): its size and the fields Linux sets.
pub mod shared {
    pub const BYTES: usize = 148;
    pub const PRESENT_FLAG_0: usize = 0;
    pub const SQ_IS_ENABLED: usize = 16;
    pub const SQ_QUEUE_MODE: usize = 17;
    pub const SMU_INTERFACE_TYPE: usize = 120;
    pub const DRM_KEY_WA_METHOD: usize = 124;
    /// AMDGPU_FW_SHARED_FLAG_0_UNIFIED_QUEUE, _DRM_KEY_INJECT, AMDGPU_VCN_SMU_DPM_INTERFACE_FLAG.
    pub const FLAG_UNIFIED_QUEUE: u32 = 1 << 2;
    pub const FLAG_DRM_KEY_INJECT: u32 = 1 << 4;
    pub const FLAG_SMU_DPM_INTERFACE: u32 = 1 << 11;
    /// AMDGPU_VCN_SMU_DPM_INTERFACE_APU, AMDGPU_DRM_KEY_INJECT_WORKAROUND_VCNFW_ASD_HANDSHAKING.
    pub const SMU_APU: u8 = 1;
    pub const DRM_KEY_ASD_HANDSHAKING: u8 = 2;
    /// `enum fw_queue_mode`.
    pub const QUEUE_RING_RESET: u8 = 1;
    pub const QUEUE_DPG_HOLD_OFF: u8 = 2;
}

/// `vcn_v4_0_fw_shared_init` for an APU; `drm_key`: VCN 4.0.2's key-inject workaround.
pub fn fw_shared(drm_key: bool) -> [u8; shared::BYTES] {
    let mut b = [0u8; shared::BYTES];
    let mut flags = shared::FLAG_UNIFIED_QUEUE | shared::FLAG_SMU_DPM_INTERFACE;
    if drm_key {
        flags |= shared::FLAG_DRM_KEY_INJECT;
        b[shared::DRM_KEY_WA_METHOD] = shared::DRM_KEY_ASD_HANDSHAKING;
    }
    b[..4].copy_from_slice(&flags.to_le_bytes());
    b[shared::SQ_IS_ENABLED] = 1;
    b[shared::SMU_INTERFACE_TYPE] = shared::SMU_APU;
    b
}

/// The packets of `words` as hang-report lines (the ring or an IB of a VCN ring), first dword at `base`.
pub fn decode_stream(words: &[u32], base: u64) -> Vec<crate::hang::Line> {
    use crate::hang::{Line, LineKind};
    use alloc::format;
    let mut v = Vec::new();
    let mut i = 0usize;
    while i < words.len() {
        let pos = base.wrapping_add(i as u64);
        let header = words[i];
        let (kind, len, text) = match Packet::decode(&words[i..]) {
            Some((Packet::Unknown(x), n)) => (LineKind::Raw, n, format!("?? VCN opcode {x:#x}")),
            Some((p, n)) => (LineKind::Packet, n, format!("{p:?}")),
            None => (LineKind::Truncated, words.len() - i, String::from("VCN packet TRUNCATED")),
        };
        v.push(Line { pos, len: len as u64, header, kind, text, at_rptr: false });
        i += len.max(1);
    }
    v
}
