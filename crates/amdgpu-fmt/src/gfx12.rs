//! GFX 12 (the RX 9070 XT's GC 12.0.1): the PM4 packets Linux 7.3.1's `gfx_v12_0.c` emits, where they differ
//! from GFX 11's and the list of every opcode it uses (GX-10 wave 44, a change request to GR applied with item
//! CP). The packet header, the opcodes' layouts and the decoder are [`crate::pm4`]'s; what changes:
//!
//! - the fence (`gfx_v12_0_ring_emit_fence` :4567-4596) flushes less than GFX 11's: GCR_SEQ, GCR_GL2_WB, cache
//!   policy 3, CACHE_FLUSH_AND_INV_TS_EVENT index 5, so its first body dword is 0x06600514 (GFX 11: 0x0671f514);
//! - there is no constant engine: no CE packet, no INDIRECT_BUFFER_CNST, and `emit_cntxcntl` (:4657-4675) sets
//!   no CE bit;
//! - the cleaner shader is started with RUN_CLEANER_SHADER (0xD2, nvd.h:668; :5169-5174).
//!
//! An engine that speaks GFX 12 ([`crate::sim::Engine::speak_gfx12`]) is a gfx or compute queue: it halts at any
//! packet whose opcode is not in [`OPCODES`] (the KIQ's packets, [`KIQ_OPCODES`], included: on this card the KIQ
//! is MES's), and at type-0 and type-2 packets, which Linux's GFX 12 code never emits.

use alloc::vec::Vec;

use crate::encode::EncodeError;
use crate::pm4::{self, RegSpace, ReleaseMem, WaitRegMem, data_sel, dst_sel, int_sel, op, rm};

/// `PACKET3_RUN_CLEANER_SHADER` (nvd.h:668).
pub const RUN_CLEANER_SHADER: u8 = 0xD2;

/// Every opcode `gfx_v12_0.c` emits on a graphics or compute ring (`gfx_v12_0_ring_funcs_gfx` and `_compute`,
/// :5357-5441, with the filler NOP): what the command processor's engine runs.
pub const OPCODES: [u8; 11] = [
    op::NOP,
    op::WRITE_DATA,
    op::INDIRECT_BUFFER,
    op::WAIT_REG_MEM,
    op::SET_UCONFIG_REG,
    op::RELEASE_MEM,
    op::PFP_SYNC_ME,
    op::CONTEXT_CONTROL,
    op::COND_EXEC,
    op::ACQUIRE_MEM,
    RUN_CLEANER_SHADER,
];

/// The opcodes `gfx_v12_0.c` emits only on the KIQ ring: `gfx_v12_0_kiq_pm4_funcs` (:412-423) and the KIQ's
/// `emit_rreg` (COPY_DATA, :4694-4709, only in `gfx_v12_0_ring_funcs_kiq`). GC 12.0.1 runs with `enable_mes_kiq`
/// and `enable_uni_mes` (amdgpu_discovery.c:2995-3001): the KIQ is MES's pipe, not a queue of the command
/// processor, so a gfx or compute queue halts at these.
pub const KIQ_OPCODES: [u8; 6] = [op::SET_RESOURCES, op::MAP_QUEUES, op::UNMAP_QUEUES, op::QUERY_STATUS, op::INVALIDATE_TLBS, op::COPY_DATA];

/// The constant engine's packets of nvd.h:543-548, which GFX 12 does not have.
pub const CE_OPCODES: [u8; 6] = [0x80, 0x81, 0x83, 0x84, 0x85, 0x86];

/// Whether Linux's GFX 12 code emits the packet with this header on a gfx or compute ring: a type-3 packet
/// with an opcode of [`OPCODES`].
pub fn emitted(header: u32) -> bool {
    header >> 30 == 3 && OPCODES.contains(&pm4::hdr_opcode(header))
}

/// The first body dword of Linux's GFX 12 fence (:4575-4579).
pub const FENCE: u32 = rm::GCR_SEQ | rm::GCR_GL2_WB | rm::cache_policy(3) | rm::event_type(rm::CACHE_FLUSH_AND_INV_TS_EVENT) | rm::event_index(5);

/// `gfx_v12_0_ring_emit_fence`: RELEASE_MEM of 8 dwords, 64-bit data when `wide`, an interrupt after the write
/// when `int`; the address 8-aligned for 64 bits, 4-aligned otherwise (Linux's WARN_ON).
pub fn fence(out: &mut Vec<u32>, addr: u64, seq: u64, wide: bool, int: bool) -> Result<(), EncodeError> {
    pm4::release_mem(
        out,
        &ReleaseMem {
            event_cntl: FENCE,
            dst_sel: 0,
            int_sel: if int { int_sel::AFTER_WRITE } else { int_sel::NONE },
            data_sel: if wide { data_sel::DATA64 } else { data_sel::LOW32 },
            addr,
            data: seq,
            ctx: 0,
        },
    )
}

/// `gfx_v12_0_ring_emit_ib_gfx` (:4525-4545): INDIRECT_BUFFER, control = length | vmid << 24.
pub fn ib_gfx(out: &mut Vec<u32>, addr: u64, len_dw: u32, vmid: u8) -> Result<(), EncodeError> {
    pm4::indirect_buffer(out, addr, len_dw, vmid, false, 0)
}

/// `gfx_v12_0_ring_emit_ib_compute` (:4547-4565): the same with INDIRECT_BUFFER_VALID.
pub fn ib_compute(out: &mut Vec<u32>, addr: u64, len_dw: u32, vmid: u8) -> Result<(), EncodeError> {
    pm4::indirect_buffer(out, addr, len_dw, vmid, false, pm4::IB_VALID)
}

/// The ring test's packet (`gfx_v12_0_ring_test_ring` :468-473 for a gfx or compute ring): SET_UCONFIG_REG of
/// 0xDEADBEEF into `scratch` (a dword register offset, SCRATCH_REG0).
pub const RING_TEST_VALUE: u32 = 0xDEAD_BEEF;
pub const RING_TEST_POISON: u32 = 0xCAFE_DEAD;

pub fn ring_test(out: &mut Vec<u32>, scratch: u32) -> Result<(), EncodeError> {
    pm4::set_reg(out, RegSpace::Uconfig, scratch, &[RING_TEST_VALUE])
}

/// The IB test's IB (`gfx_v12_0_ring_test_ib` :525-530): WRITE_DATA of 0xDEADBEEF to memory with WR_CONFIRM,
/// five dwords.
pub fn ib_test(out: &mut Vec<u32>, addr: u64) -> Result<(), EncodeError> {
    pm4::write_data(out, dst_sel::MEM, addr, pm4::WR_CONFIRM, &[RING_TEST_VALUE])
}

/// `gfx_v12_0_wait_reg_mem` (:430-450): function 3 (equal).
pub fn wait_reg_mem(out: &mut Vec<u32>, engine: u8, mem: bool, operation: u8, addr0: u32, addr1: u32, reference: u32, mask: u32, interval: u32) -> Result<(), EncodeError> {
    pm4::wait_reg_mem(out, &WaitRegMem { function: 3, mem, operation, engine, addr0, addr1, reference, mask, interval })
}

/// `gfx_v12_0_ring_emit_hdp_flush` (:4508-4523): write NBIF's request register, wait on its done register,
/// with the ring's bit (`amdgpu_gfx_get_hdp_flush_mask`).
pub fn hdp_flush(out: &mut Vec<u32>, engine: u8, req: u32, done: u32, bit: u32) -> Result<(), EncodeError> {
    wait_reg_mem(out, engine, false, 1, req, done, bit, bit, 0x20)
}

/// `gfx_v12_0_ring_emit_pipeline_sync` (:4598-4606): wait until the fence slot holds `seq`.
pub fn pipeline_sync(out: &mut Vec<u32>, gfx: bool, addr: u64, seq: u32) -> Result<(), EncodeError> {
    wait_reg_mem(out, u8::from(gfx), true, 0, addr as u32, (addr >> 32) as u32, seq, 0xFFFF_FFFF, 4)
}

/// `gfx_v12_0_ring_emit_vm_flush`'s PFP_SYNC_ME on a graphics ring (:4627-4630).
pub fn pfp_sync_me(out: &mut Vec<u32>) {
    out.extend_from_slice(&[pm4::packet3(op::PFP_SYNC_ME, 0), 0]);
}

/// `gfx_v12_0_ring_emit_cntxcntl` (:4657-4675): CONTEXT_CONTROL with load_enable, and the context's loads on a
/// context switch; no CE bit.
pub fn cntxcntl(out: &mut Vec<u32>, ctx_switch: bool) {
    let dw2 = 0x8000_0000 | if ctx_switch { 0x8001 | 0x0100_0000 | 0x10002 } else { 0 };
    out.extend_from_slice(&[pm4::packet3(op::CONTEXT_CONTROL, 1), dw2, 0]);
}

/// `gfx_v12_0_emit_mem_sync` (:5131-5152): ACQUIRE_MEM of 8 dwords over all of memory with the GCR bits GL2_INV,
/// GL2_WB, GLM_INV, GLM_WB, GL1_INV, GLV_INV, GLK_INV and GLI_INV (nvd.h:458-488).
pub const MEM_SYNC_GCR: u32 = (1 << 15) | (1 << 14) | (1 << 9) | (1 << 8) | (1 << 7) | (1 << 5) | (1 << 4) | 1;

pub fn mem_sync(out: &mut Vec<u32>) {
    out.extend_from_slice(&[pm4::packet3(op::ACQUIRE_MEM, 6), 0, 0xFFFF_FFFF, 0x00FF_FFFF, 0, 0, 0x0000_000A, MEM_SYNC_GCR]);
}

/// `gfx_v12_0_ring_emit_cleaner_shader` (:5169-5174).
pub fn cleaner_shader(out: &mut Vec<u32>) {
    out.extend_from_slice(&[pm4::packet3(RUN_CLEANER_SHADER, 0), 0]);
}

/// `gfx_v12_ring_insert_nop` (:5154-5167): one NOP packet over `n` dwords (its body the filler NOP), or the
/// filler alone for one.
pub fn insert_nop(out: &mut Vec<u32>, n: u32) {
    match n {
        0 => {}
        1 => out.push(pm4::PAD),
        _ => {
            out.push(pm4::packet3(op::NOP, (n - 2).min(0x3FFE)));
            out.extend(core::iter::repeat_n(pm4::PAD, n as usize - 1));
        }
    }
}
