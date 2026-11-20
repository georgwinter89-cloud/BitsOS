//! Piece 10: the sequences Linux runs on a memory hub, over a register set of `hub41` (`MM` or `GC`), as pure
//! arithmetic in Linux's order: the GART start, the fault control, the invalidation request, the switch-off.
//! A: `mmhub_v4_1_0.c` (gart_enable :368-382 and what it calls, gart_disable :384-407, set_fault_enable_default
//! :415-457) and `gfxhub_v12_0.c` (gart_enable :360-386, gart_disable :388-409, set_fault_enable_default
//! :417-462): the two files run the same steps on their own registers. Line numbers in the comments are
//! mmhub_v4_1_0.c's; `hub41::Hub` says where gfxhub_v12_0.c differs.

use crate::hub41::{BLOCK_SIZE, ENGINES, Hub, L2_CNTL4_DEFAULT, L2_CNTL5_DEFAULT, Layout12, MAX_PFN, MTYPE_UC, NUM_LEVEL, USER_CONTEXTS, f};
use crate::nbreg::member;
use crate::regmap::Reg;

/// What the stage chooses itself: where the GART table and the default page lie in VRAM (offsets) and the system
/// page a fault reads. One choice for both hubs: Linux hands each the same table (`adev->gart.bo`), the same
/// scratch page and the same dummy page.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Chosen {
    pub table: u64,
    pub scratch: u64,
    pub dummy: u64,
}

/// One step of a register sequence.
#[derive(Clone, Copy, Debug)]
pub enum Op {
    Write(Reg, u32),
    /// Read the register, write back what the function makes of it (Linux's RREG32, REG_SET_FIELD.., WREG32).
    Modify(Reg, fn(u32) -> u32),
    /// Read the register, write it with these bits set, write it with them cleared (a reset pulse,
    /// gfx_v12_0.c:2167-2176).
    Pulse(Reg, u32),
}

impl Op {
    /// The register the step writes.
    pub const fn reg(&self) -> Reg {
        match *self {
            Op::Write(r, _) | Op::Modify(r, _) | Op::Pulse(r, _) => r,
        }
    }
}

/// `mmhub_v4_1_0_init_tlb_regs` :196-214.
pub fn l1_tlb(v: u32) -> u32 {
    let v = f::ENABLE_L1_TLB.put(v, 1);
    let v = f::SYSTEM_ACCESS_MODE.put(v, 3);
    let v = f::ENABLE_ADVANCED_DRIVER_MODEL.put(v, 1);
    let v = f::SYSTEM_APERTURE_UNMAPPED_ACCESS.put(v, 0);
    let v = f::ECO_BITS.put(v, 0);
    f::L1_MTYPE.put(v, MTYPE_UC)
}

/// `mmhub_v4_1_0_init_cache_regs` :228-239.
pub fn l2_cntl(v: u32) -> u32 {
    let v = f::ENABLE_L2_CACHE.put(v, 1);
    let v = f::ENABLE_L2_FRAGMENT_PROCESSING.put(v, 0);
    let v = f::ENABLE_DEFAULT_PAGE_OUT_TO_SYSTEM_MEMORY.put(v, 1);
    let v = f::L2_PDE0_CACHE_TAG_GENERATION_MODE.put(v, 0);
    let v = f::PDE_FAULT_CLASSIFICATION.put(v, 0);
    let v = f::CONTEXT1_IDENTITY_ACCESS_MODE.put(v, 1);
    f::IDENTITY_MODE_FRAGMENT_SIZE.put(v, 0)
}

/// :241-244.
pub fn l2_cntl2(v: u32) -> u32 {
    f::INVALIDATE_L2_CACHE.put(f::INVALIDATE_ALL_L1_TLBS.put(v, 1), 1)
}

/// :245-254 with `translate_further` off (gmc_v12_0.c never sets it), from the hub's own default.
pub const fn l2_cntl3(h: &Hub) -> u32 {
    f::L2_CACHE_BIGK_FRAGMENT_SIZE.put(f::BANK_SELECT.put(h.l2_cntl3_default, 9), 6)
}

/// :257-260.
pub const fn l2_cntl4() -> u32 {
    f::VMC_TAP_PTE_REQUEST_PHYSICAL.put(f::VMC_TAP_PDE_REQUEST_PHYSICAL.put(L2_CNTL4_DEFAULT, 0), 0)
}

/// :262-264.
pub const fn l2_cntl5() -> u32 {
    f::L2_CACHE_SMALLK_FRAGMENT_SIZE.put(L2_CNTL5_DEFAULT, 0)
}

/// `mmhub_v4_1_0_enable_system_domain` :267-277: context 0 on, a flat table.
pub fn context0_cntl(v: u32) -> u32 {
    f::RETRY_PERMISSION_OR_INVALID_PAGE_FAULT.put(f::PAGE_TABLE_DEPTH.put(f::ENABLE_CONTEXT.put(v, 1), 0), 0)
}

/// `mmhub_v4_1_0_setup_vmid_config` :312-335. `noretry` is on for this card (`amdgpu_gmc_noretry_set`,
/// A:amdgpu_gmc.c:1000-1020: every chip from GC 10.1 on), so RETRY_PERMISSION_OR_INVALID_PAGE_FAULT is 0.
pub fn context_n_cntl(v: u32) -> u32 {
    let v = f::PAGE_TABLE_DEPTH.put(f::ENABLE_CONTEXT.put(v, 1), NUM_LEVEL);
    let v = v | f::CONTEXT_FAULT_DEFAULTS;
    f::RETRY_PERMISSION_OR_INVALID_PAGE_FAULT.put(f::PAGE_TABLE_BLOCK_SIZE.put(v, BLOCK_SIZE - 9), 0)
}

/// `mmhub_v4_1_0_init_system_aperture_regs` :188-191.
pub fn fault_cntl2(v: u32) -> u32 {
    f::ACTIVE_PAGE_MIGRATION_PTE_READ_RETRY.put(v, 1)
}

/// `mmhub_v4_1_0_set_fault_enable_default(adev, true)` :415-457 (`amdgpu_vm_fault_stop` is "never" by default):
/// the eleven defaults on; the two crash bits only when `value` is false, so untouched here.
pub fn fault_cntl(v: u32) -> u32 {
    v | f::FAULT_DEFAULTS
}

/// `gfxhub_v12_0_set_fault_enable_default(adev, true)` on the fault control (gfxhub_v12_0.c:434-461): the same
/// eleven defaults, and CRASH_ON_NO_RETRY_FAULT and CRASH_ON_RETRY_FAULT always written, as `!value` = 0.
pub fn gc_fault_cntl(v: u32) -> u32 {
    f::CRASH_ON_RETRY_FAULT.put(f::CRASH_ON_NO_RETRY_FAULT.put(v | f::FAULT_DEFAULTS, 0), 0)
}

/// `gmc_v12_0_process_interrupt`'s `WREG32_P(hub->vm_l2_pro_fault_cntl, 1, ~1)` (gmc_v12_0.c:130): the fault's
/// status and address cleared.
pub fn fault_clear(v: u32) -> u32 {
    f::CLEAR_FAULT_STATUS.put(v, 1)
}

/// `get_invalidate_req` of either hub for a legacy flush of `vmid` (mmhub_v4_1_0.c:67-86, gfxhub_v12_0.c:61-79
/// with the flush type 0 every caller of this driver passes).
pub const fn invalidate_req(vmid: u32) -> u32 {
    let v = f::PER_VMID_INVALIDATE_REQ.put(0, 1 << vmid);
    let v = f::FLUSH_TYPE.put(v, 0);
    let v = f::INVALIDATE_L2_PTES.put(v, 1);
    let v = f::INVALIDATE_L2_PDE0.put(v, 1);
    let v = f::INVALIDATE_L2_PDE1.put(v, 1);
    let v = f::INVALIDATE_L2_PDE2.put(v, 1);
    let v = f::INVALIDATE_L1_PTES.put(v, 1);
    f::CLEAR_PROTECTION_FAULT_STATUS_ADDR.put(v, 0)
}

/// `amdgpu_gmc_pd_addr` (amdgpu_gmc.c:135-150) for the GART table: its VRAM offset counted from the base offset,
/// with AMDGPU_PTE_VALID.
pub const fn pd_addr(l: &Layout12, table: u64) -> u64 {
    (table + l.base_offset) | crate::pte12::VALID
}

/// A hub's `gart_enable` (mmhub_v4_1_0.c:368-382, gfxhub_v12_0.c:360-386) as its register operations in Linux's
/// order.
pub fn gart_enable<E>(h: &Hub, l: &Layout12, c: &Chosen, mut op: impl FnMut(Op) -> Result<(), E>) -> Result<(), E> {
    let (lo, hi) = (|v: u64| v as u32, |v: u64| (v >> 32) as u32);
    // init_gart_aperture_regs :135-150.
    let pd = pd_addr(l, c.table);
    op(Op::Write(h.ctx0_base_lo, lo(pd)))?;
    op(Op::Write(h.ctx0_base_hi, hi(pd)))?;
    op(Op::Write(h.ctx0_start_lo, (l.gart_start >> 12) as u32))?;
    op(Op::Write(member(h.ctx0_start_lo, 1, 1), (l.gart_start >> 44) as u32))?;
    op(Op::Write(h.ctx0_end_lo, (l.gart_end() >> 12) as u32))?;
    op(Op::Write(member(h.ctx0_end_lo, 1, 1), (l.gart_end() >> 44) as u32))?;
    // init_system_aperture_regs :152-194.
    op(Op::Write(h.agp_base, 0))?;
    op(Op::Write(h.agp_bot, (l.agp_start >> 24) as u32))?;
    op(Op::Write(h.agp_top, (l.agp_end >> 24) as u32))?;
    op(Op::Write(h.aperture_low, (l.vram_start.min(l.agp_start) >> 18) as u32))?;
    op(Op::Write(h.aperture_high, (l.vram_end().max(l.agp_end) >> 18) as u32))?;
    let default = c.scratch + l.base_offset;
    op(Op::Write(h.default_addr_lsb, (default >> 12) as u32))?;
    op(Op::Write(h.default_addr_msb, (default >> 44) as u32))?;
    op(Op::Write(h.fault_default_lo, (c.dummy >> 12) as u32))?;
    op(Op::Write(h.fault_default_hi, (c.dummy >> 44) as u32))?;
    op(Op::Modify(h.fault_cntl2, fault_cntl2))?;
    op(Op::Modify(h.l1_tlb, l1_tlb))?;
    op(Op::Modify(h.l2_cntl, l2_cntl))?;
    op(Op::Modify(h.l2_cntl2, l2_cntl2))?;
    op(Op::Write(h.l2_cntl3, l2_cntl3(h)))?;
    op(Op::Write(h.l2_cntl4, l2_cntl4()))?;
    op(Op::Write(h.l2_cntl5, l2_cntl5()))?;
    op(Op::Modify(h.context0_cntl, context0_cntl))?;
    // disable_identity_aperture :279-303.
    for (i, v) in [0xFFFF_FFFF, 0x0000_000F, 0, 0, 0, 0].into_iter().enumerate() {
        op(Op::Write(member(h.identity_aperture, i as u32, 1), v))?;
    }
    for i in 0..USER_CONTEXTS {
        op(Op::Modify(member(h.context1_cntl, i, 1), context_n_cntl))?;
        op(Op::Write(member(h.ctx1_start_lo, i, 2), 0))?;
        op(Op::Write(member(h.ctx1_start_lo, 2 * i + 1, 1), 0))?;
        op(Op::Write(member(h.ctx1_end_lo, i, 2), lo(MAX_PFN - 1)))?;
        op(Op::Write(member(h.ctx1_end_lo, 2 * i + 1, 1), hi(MAX_PFN - 1)))?;
    }
    for i in 0..ENGINES {
        op(Op::Write(member(h.eng0_range_lo, i, 2), 0xFFFF_FFFF))?;
        op(Op::Write(member(h.eng0_range_lo, 2 * i + 1, 1), 0x1F))?;
    }
    Ok(())
}

/// Every register `gart_enable` writes on this hub, in order (a stage's write list).
pub fn gart_enable_regs(h: &Hub, mut each: impl FnMut(Reg)) {
    let l = Layout12 { vram_start: 0, vram_bytes: 1, base_offset: 0, gart_start: 0, gart_bytes: 1, agp_start: 0, agp_end: 0 };
    let _ = gart_enable(h, &l, &Chosen { table: 0, scratch: 0, dummy: 0 }, |op| -> Result<(), ()> {
        each(op.reg());
        Ok(())
    });
}

/// A hub's `set_fault_enable_default(adev, true)`: MMHUB's is its fault control alone, GFXHUB's sets
/// CPG_UTCL1_ERROR_HALT_DISABLE in CP_DEBUG first (`Hub::before_fault_enable`).
pub fn fault_enable_default<E>(h: &Hub, mut op: impl FnMut(Op) -> Result<(), E>) -> Result<(), E> {
    if let Some((reg, change)) = h.before_fault_enable {
        op(Op::Modify(reg, change))?;
    }
    op(Op::Modify(h.fault_cntl, h.fault_enable))
}

/// A hub's L1 TLB as `gart_disable` leaves it.
pub fn l1_tlb_off(v: u32) -> u32 {
    f::ENABLE_ADVANCED_DRIVER_MODEL.put(f::ENABLE_L1_TLB.put(v, 0), 0)
}

/// A hub's L2 cache as `gart_disable` leaves it.
pub fn l2_cache_off(v: u32) -> u32 {
    f::ENABLE_L2_CACHE.put(v, 0)
}

/// A hub's `gart_disable` (mmhub_v4_1_0.c:384-407, gfxhub_v12_0.c:388-409): all sixteen contexts off, the L1 TLB
/// and the L2 cache off, L2_CNTL3 0.
pub fn gart_disable<E>(h: &Hub, mut op: impl FnMut(Op) -> Result<(), E>) -> Result<(), E> {
    for i in 0..=USER_CONTEXTS {
        op(Op::Write(member(h.context0_cntl, i, 1), 0))?;
    }
    op(Op::Modify(h.l1_tlb, l1_tlb_off))?;
    op(Op::Modify(h.l2_cntl, l2_cache_off))?;
    op(Op::Write(h.l2_cntl3, 0))
}
