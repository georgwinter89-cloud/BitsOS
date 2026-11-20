//! Wave 43 (piece 12): SDMA 7.0.1's two engines as pure arithmetic: the registers `sdma_v7_0_gfx_resume_instance`
//! touches (A:sdma_v7_0.c:474-622) with the rule that places an instance (`sdma_v7_0_get_reg_offset` :125-141),
//! Linux's values for each, the doorbell numbers (A:amdgpu_doorbell.h:210-211, A:soc24.c:288-299), NBIF's doorbell
//! entry for the engines (A:nbif_v6_3_1.c:153-199), the in-ring HDP flush's registers (:433-447, :476-477) and the
//! interrupt entries (`sdma_v7_0_process_trap_irq` :1544-1571). Offsets and masks are Linux 7.3.1's
//! `include/asic_reg/gc/gc_12_0_0_{offset,sh_mask}.h` and `nbif/nbif_6_3_1_*`: SDMA 7 has no header of its own,
//! its registers lie in GC's block on GC's bases.
//!
//! The 780M's SDMA 6.0.1 (`amdgpu::ipmod::sdma6`) is the version before this one: the ring, its packets and the
//! write-back slots are the same code for both (`amdgpu_fmt::{ring, sdma, wb}`); what differs is here (other
//! offsets, an MCU control where 6 has F32_CNTL, RB_CNTL as Linux's plain read-modify-write).

use crate::ipfacts::hw;
use crate::nbreg::Field;
use crate::regmap::Reg;

const fn gc(name: &'static str, base_index: u8, offset: u32) -> Reg {
    Reg { name, hw_id: hw::GC, base_index, offset }
}

const fn nbif(name: &'static str, offset: u32) -> Reg {
    Reg { name, hw_id: hw::NBIF, base_index: 2, offset }
}

/// The engines of this card (the discovery table's SDMA0 and SDMA1).
pub const INSTANCES: u32 = 2;
/// `SDMA1_REG_OFFSET`, `SDMA0_HYP_DEC_REG_START/END`, `SDMA1_HYP_DEC_REG_OFFSET` (A:sdma_v7_0.c:51-54).
pub const SDMA1_REG_OFFSET: u32 = 0x600;
pub const HYP_DEC_REG_START: u32 = 0x5880;
pub const HYP_DEC_REG_END: u32 = 0x589A;
pub const SDMA1_HYP_DEC_REG_OFFSET: u32 = 0x20;

// gc_12_0_0_offset.h:34-274 (base index 0) and :946-948 (base index 1).
pub const UCODE_REV: Reg = gc("SDMA0_UCODE_REV", 0, 0x03);
pub const CNTL: Reg = gc("SDMA0_CNTL", 0, 0x0D);
pub const RB_RPTR_FETCH: Reg = gc("SDMA0_RB_RPTR_FETCH", 0, 0x20);
pub const RB_RPTR_FETCH_HI: Reg = gc("SDMA0_RB_RPTR_FETCH_HI", 0, 0x21);
pub const STATUS_REG: Reg = gc("SDMA0_STATUS_REG", 0, 0x24);
pub const WATCHDOG_CNTL: Reg = gc("SDMA0_WATCHDOG_CNTL", 0, 0x2B);
pub const UTCL1_CNTL: Reg = gc("SDMA0_UTCL1_CNTL", 0, 0x35);
pub const UTCL1_PAGE: Reg = gc("SDMA0_UTCL1_PAGE", 0, 0x38);
pub const UTCL1_RD_STATUS: Reg = gc("SDMA0_UTCL1_RD_STATUS", 0, 0x41);
pub const UTCL1_WR_STATUS: Reg = gc("SDMA0_UTCL1_WR_STATUS", 0, 0x42);
pub const INT_STATUS: Reg = gc("SDMA0_INT_STATUS", 0, 0x61);
pub const RB_CNTL: Reg = gc("SDMA0_QUEUE0_RB_CNTL", 0, 0x80);
pub const RB_BASE: Reg = gc("SDMA0_QUEUE0_RB_BASE", 0, 0x81);
pub const RB_BASE_HI: Reg = gc("SDMA0_QUEUE0_RB_BASE_HI", 0, 0x82);
pub const RB_RPTR: Reg = gc("SDMA0_QUEUE0_RB_RPTR", 0, 0x83);
pub const RB_RPTR_HI: Reg = gc("SDMA0_QUEUE0_RB_RPTR_HI", 0, 0x84);
pub const RB_WPTR: Reg = gc("SDMA0_QUEUE0_RB_WPTR", 0, 0x85);
pub const RB_WPTR_HI: Reg = gc("SDMA0_QUEUE0_RB_WPTR_HI", 0, 0x86);
pub const RB_RPTR_ADDR_LO: Reg = gc("SDMA0_QUEUE0_RB_RPTR_ADDR_LO", 0, 0x87);
pub const RB_RPTR_ADDR_HI: Reg = gc("SDMA0_QUEUE0_RB_RPTR_ADDR_HI", 0, 0x88);
pub const IB_CNTL: Reg = gc("SDMA0_QUEUE0_IB_CNTL", 0, 0x89);
pub const DOORBELL: Reg = gc("SDMA0_QUEUE0_DOORBELL", 0, 0x8F);
pub const DOORBELL_OFFSET: Reg = gc("SDMA0_QUEUE0_DOORBELL_OFFSET", 0, 0x91);
pub const RB_WPTR_POLL_ADDR_LO: Reg = gc("SDMA0_QUEUE0_RB_WPTR_POLL_ADDR_LO", 0, 0x98);
pub const RB_WPTR_POLL_ADDR_HI: Reg = gc("SDMA0_QUEUE0_RB_WPTR_POLL_ADDR_HI", 0, 0x99);
pub const MINOR_PTR_UPDATE: Reg = gc("SDMA0_QUEUE0_MINOR_PTR_UPDATE", 0, 0x9B);
pub const MCU_CNTL: Reg = gc("SDMA0_MCU_CNTL", 1, 0x588E);

/// Every register of an engine the driver or its model names, for a table by BAR5 byte.
pub const ALL: [Reg; 27] = [
    UCODE_REV, CNTL, RB_RPTR_FETCH, RB_RPTR_FETCH_HI, STATUS_REG, WATCHDOG_CNTL, UTCL1_CNTL, UTCL1_PAGE, UTCL1_RD_STATUS, UTCL1_WR_STATUS, INT_STATUS, RB_CNTL, RB_BASE, RB_BASE_HI,
    RB_RPTR, RB_RPTR_HI, RB_WPTR, RB_WPTR_HI, RB_RPTR_ADDR_LO, RB_RPTR_ADDR_HI, IB_CNTL, DOORBELL, DOORBELL_OFFSET, RB_WPTR_POLL_ADDR_LO, RB_WPTR_POLL_ADDR_HI, MINOR_PTR_UPDATE, MCU_CNTL,
];

/// `sdma_v7_0_get_reg_offset` :125-141: the register of engine `i` (the name stays engine 0's). The "hypervisor
/// decode" registers lie on GC's base 1, 0x20 dwords apart; all others on base 0, 0x600 apart.
pub const fn instance(r: Reg, i: u32) -> Reg {
    let hyp = r.offset >= HYP_DEC_REG_START && r.offset <= HYP_DEC_REG_END;
    Reg { offset: r.offset + i * if hyp { SDMA1_HYP_DEC_REG_OFFSET } else { SDMA1_REG_OFFSET }, ..r }
}

/// Field masks (gc_12_0_0_sh_mask.h:67-84, :169-195, :281, :378-386, :908-918, :948, :970-979, :1025, :2756-2757).
pub mod f {
    use super::Field;
    pub const TRAP_ENABLE: Field = Field::new(0, 0x0000_0001);
    pub const PAGE_FAULT_INT_ENABLE: Field = Field::new(13, 0x0000_2000);
    pub const IDLE: Field = Field::new(0, 0x0000_0001);
    pub const RB_EMPTY: Field = Field::new(2, 0x0000_0004);
    pub const QUEUE_HANG_COUNT: Field = Field::new(0, 0x0000_00FF);
    pub const REDO_DELAY: Field = Field::new(0, 0x0000_001F);
    pub const RESP_MODE: Field = Field::new(9, 0x0000_0600);
    pub const RD_L2_POLICY: Field = Field::new(12, 0x0000_3000);
    pub const WR_L2_POLICY: Field = Field::new(14, 0x0000_C000);
    pub const RB_ENABLE: Field = Field::new(0, 0x0000_0001);
    pub const RB_SIZE: Field = Field::new(1, 0x0000_003E);
    pub const WPTR_POLL_ENABLE: Field = Field::new(8, 0x0000_0100);
    pub const MCU_WPTR_POLL_ENABLE: Field = Field::new(11, 0x0000_0800);
    pub const RPTR_WRITEBACK_ENABLE: Field = Field::new(12, 0x0000_1000);
    pub const RB_PRIV: Field = Field::new(23, 0x0080_0000);
    pub const IB_ENABLE: Field = Field::new(0, 0x0000_0001);
    pub const DOORBELL_ENABLE: Field = Field::new(28, 0x1000_0000);
    pub const DOORBELL_OFFSET: Field = Field::new(2, 0x0FFF_FFFC);
    pub const MINOR_PTR_UPDATE_ENABLE: Field = Field::new(0, 0x0000_0001);
    pub const HALT: Field = Field::new(0, 0x0000_0001);
    pub const RESET: Field = Field::new(1, 0x0000_0002);
}

/// `amdgpu_ring_init(adev, ring, 1024, ...)` (:1330) with `amdgpu_sched_hw_submission` 2 (A:amdgpu_drv.c:178):
/// `roundup_pow_of_two(1024 * 4 * 2)` bytes (A:amdgpu_ring.c:338), 2048 dwords.
pub const RING_BYTES: u32 = 8192;
pub const RING_DW: u32 = RING_BYTES / 4;
/// `adev->usec_timeout` (A:amdgpu.h:274): the watchdog's unit count and the ring test's polls come from it.
pub const USEC_TIMEOUT: u32 = 100_000;
/// The firmware takes the counted NOP from this feature version on (A:amdgpu_sdma.c:182-183).
pub const BURST_NOP_FEATURE: u32 = 20;
/// `ucode_feature_version` in `sdma_firmware_header_v3_0` (A:amdgpu_ucode.h:371-376: right behind the 32-byte
/// common header).
pub const FEATURE_VERSION_AT: usize = 32;
/// What the ring test writes and expects (`sdma_v7_0_ring_test_ring` :917-968).
pub const POISON: u32 = 0xCAFE_DEAD;
pub const RING_TEST: u32 = 0xDEAD_BEEF;

/// `AMDGPU_NAVI10_DOORBELL_sDMA_ENGINE0/1` (A:amdgpu_doorbell.h:210-211; soc24.c:288-289), in 64-bit doorbells.
pub const DOORBELL_ENGINE: [u32; 2] = [0x100, 0x10A];
/// `adev->doorbell_index.sdma_doorbell_range` (soc24.c:299), per engine.
pub const DOORBELL_RANGE: u32 = 20;

/// An engine's doorbell as a dword index of BAR2 (`sdma_engine[i] << 1`, :1325-1326): the 64-bit store goes to
/// byte `index * 4` (A:amdgpu_doorbell_mgr.c:103-112).
pub const fn doorbell_dw(i: u32) -> u32 {
    DOORBELL_ENGINE[(i % INSTANCES) as usize] << 1
}

/// RB_CNTL as `gfx_resume_instance` first writes it (:485-494): the size and RB_PRIV into what the register read.
pub const fn rb_cntl_sized(read: u32) -> u32 {
    f::RB_PRIV.put(f::RB_SIZE.put(read, RING_DW.trailing_zeros()), 1)
}

/// The same value with the write-back on, the driver's poll off and the MCU's on (:520-526); `enable` adds
/// RB_ENABLE (:594).
pub const fn rb_cntl_run(sized: u32, enable: bool) -> u32 {
    let v = f::MCU_WPTR_POLL_ENABLE.put(f::WPTR_POLL_ENABLE.put(f::RPTR_WRITEBACK_ENABLE.put(sized, 1), 0), 1);
    if enable { f::RB_ENABLE.put(v, 1) } else { v }
}

/// :542-551 with `use_doorbell`.
pub const fn doorbell(read: u32) -> u32 {
    f::DOORBELL_ENABLE.put(read, 1)
}

pub const fn doorbell_offset(read: u32, index_dw: u32) -> u32 {
    f::DOORBELL_OFFSET.put(read, index_dw)
}

/// :565-570: "100ms per unit".
pub const fn watchdog(read: u32) -> u32 {
    let n = USEC_TIMEOUT / 100_000;
    f::QUEUE_HANG_COUNT.put(read, if n == 0 { 1 } else { n })
}

/// :572-576.
pub const fn utcl1_cntl(read: u32) -> u32 {
    f::REDO_DELAY.put(f::RESP_MODE.put(read, 3), 9)
}

/// :578-584 with `CACHE_READ_POLICY_L2__DEFAULT` 2 and `CACHE_WRITE_POLICY_L2__DEFAULT` 3 (A:sdma_common.h:27-39).
pub const fn utcl1_page(read: u32) -> u32 {
    (read & 0x00FF_0FFF) | (2 << 12) | (3 << 14)
}

/// `sdma_v7_0_enable` :444-462 (`halt` as `!enable`).
pub const fn mcu_halt(read: u32, halt: bool) -> u32 {
    f::HALT.put(read, halt as u32)
}

/// The unhalt of `gfx_resume_instance` :586-591.
pub const fn mcu_unhalt(read: u32) -> u32 {
    f::RESET.put(f::HALT.put(read, 0), 0)
}

/// `sdma_v7_0_gfx_stop` :397-410.
pub const fn rb_cntl_stopped(read: u32) -> u32 {
    f::RB_ENABLE.put(read, 0)
}

pub const fn ib_cntl(read: u32, on: bool) -> u32 {
    f::IB_ENABLE.put(read, on as u32)
}

// NBIF's side (nbif_6_3_1_offset.h:1773-1776, :1843-1844; nbif_6_3_1_sh_mask.h:11114-11115, :11360-11368).
/// `nbif_v6_3_1_sdma_doorbell_range` :153-199, written for engine 0 only (A:sdma_v7_0.c:556-559).
pub const DOORBELL_ENTRY_2: Reg = nbif("GDC_S2A0_S2A_DOORBELL_ENTRY_2_CTRL", 0x1CD);
pub const PORT2_ENABLE: Field = Field::new(0, 0x0000_0001);
pub const PORT2_AWID: Field = Field::new(1, 0x0000_003E);
pub const PORT2_RANGE_OFFSET: Field = Field::new(7, 0x0001_FF80);
pub const PORT2_RANGE_SIZE: Field = Field::new(17, 0x01FE_0000);
pub const PORT2_AWADDR_31_28_VALUE: Field = Field::new(28, 0xF000_0000);

/// The entry as Linux writes it for what it read: both engines' doorbells behind engine 0's index.
pub const fn doorbell_range(read: u32) -> u32 {
    let v = PORT2_AWID.put(PORT2_ENABLE.put(read, 1), 0xE);
    let v = PORT2_RANGE_SIZE.put(PORT2_RANGE_OFFSET.put(v, doorbell_dw(0)), DOORBELL_RANGE * INSTANCES);
    PORT2_AWADDR_31_28_VALUE.put(v, 0x3)
}

/// The HDP flush an engine makes from its ring (`sdma_v7_0_ring_emit_hdp_flush` :327-344): it writes its bit to
/// the request register and polls the done register for it.
pub const GPU_HDP_FLUSH_REQ: Reg = nbif("BIF_BX_PF0_GPU_HDP_FLUSH_REQ", 0x106);
pub const GPU_HDP_FLUSH_DONE: Reg = nbif("BIF_BX_PF0_GPU_HDP_FLUSH_DONE", 0x107);
/// `ref_and_mask_sdma0 << ring->me`.
pub const fn hdp_flush_bit(i: u32) -> u32 {
    0x400 << (i % INSTANCES)
}
/// `SDMA_PKT_POLL_REGMEM_DW5_RETRY_COUNT(0xfff)` and `..._INTERVAL(10)`.
pub const HDP_FLUSH_RETRY: u16 = 0xFFF;
pub const HDP_FLUSH_INTERVAL: u16 = 10;

/// `SOC21_IH_CLIENTID_GFX` (include/soc15_ih_clientid.h:93) and the two sources `sdma_v7_0_sw_init` registers
/// for, the trap and the user fence (:1302-1312; include/ivsrcid/gfx/irqsrcs_gfx_12_0_0.h:31, :49).
pub const CLIENT_GFX: u8 = 0x0A;
pub const SRC_TRAP: u8 = 49;
pub const SRC_FENCE: u8 = 70;

/// A trap entry's ring id for engine `i`, queue 0 (`process_trap_irq`: queue in the low nibble, the engine in
/// the high one; the load trace of 18.11. shows 0 and 16).
pub const fn trap_ring_id(i: u32) -> u8 {
    ((i % INSTANCES) << 4) as u8
}

/// The engine and queue a trap entry names, as `process_trap_irq` reads them; `None` for an engine this card
/// does not have ("wrong ring_ID detected").
pub const fn trap_of(ring_id: u8) -> Option<(u32, u8)> {
    let i = ((ring_id & 0xF0) >> 4) as u32;
    if i < INSTANCES { Some((i, ring_id & 0xF)) } else { None }
}
