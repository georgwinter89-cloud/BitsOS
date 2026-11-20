//! Wave 43, the graphics block and GFXHUB: what `rdna4_core` names by hand against the generated numbers.

use crate::*;

/// Wave 43, item HB: every register of GFXHUB's set (`rdna4_core::hub41::GC`) and of the graphics block's start
/// (`rdna4_core::gc12`) equals the generated one by name, and every field `hub41::f` names for MMHUB lies at the
/// same bits in GFXHUB's registers, so one hub implementation serves both sets.
#[test]
fn the_wave_43_gc_registers_and_fields_of_rdna4_core_equal_the_generated_ones() {
    use gc_12_0_0::f as g;
    use rdna4_core::{fault12, gc12, hub41, nbreg};
    let h = &hub41::GC;
    let hub = [
        h.fb_offset, h.default_addr_lsb, h.default_addr_msb, h.l2_cntl, h.l2_cntl2, h.l2_cntl3, h.fault_cntl, h.fault_cntl2, h.fault_status, h.fault_default_lo, h.fault_default_hi, h.identity_aperture, h.l2_cntl4, h.l2_cntl5,
        h.agp_top, h.agp_bot, h.agp_base, h.aperture_low, h.aperture_high, h.l1_tlb, h.context0_cntl, h.context1_cntl, h.eng0_sem, h.eng0_req, h.eng0_ack, h.eng0_range_lo, h.ctx0_base_lo, h.ctx0_base_hi, h.ctx0_start_lo, h.ctx1_start_lo,
        h.ctx0_end_lo, h.ctx1_end_lo,
    ];
    let boot = [
        gc12::CP_STAT, gc12::RLC_RLCS_BOOTLOAD_STATUS, gc12::DB_MEM_CONFIG, gc12::GB_ADDR_CONFIG, gc12::CP_DEBUG, gc12::CP_PFP_PRGRM_CNTR_START, gc12::CP_PFP_PRGRM_CNTR_START_HI, gc12::CP_ME_PRGRM_CNTR_START,
        gc12::CP_ME_PRGRM_CNTR_START_HI, gc12::CP_ME_CNTL, gc12::GRBM_GFX_CNTL, gc12::CP_MEC_RS64_PRGRM_CNTR_START, gc12::CP_MEC_RS64_PRGRM_CNTR_START_HI, gc12::CP_MEC_RS64_CNTL,
    ];
    for r in hub.iter().chain(&boot) {
        assert_eq!(by_name("gc_12_0_0", r.name).map(|m| (m.base_idx, m.offset)), Some((r.base_index, r.offset)), "{}", r.name);
    }
    // MMHUB's two fault registers, new with the fault decode.
    for r in [hub41::MM.fault_status, hub41::MM.fault_cntl] {
        assert_eq!(by_name("mmhub_4_1_0", r.name).map(|m| (m.base_idx, m.offset)), Some((r.base_index, r.offset)), "{}", r.name);
    }
    // A family's members on GFXHUB lie where the header puts the next one, as on MMHUB.
    for (first, i, stride, name) in [
        (h.context1_cntl, 14, 1, "GCVM_CONTEXT15_CNTL"),
        (h.ctx1_start_lo, 1, 2, "GCVM_CONTEXT2_PAGE_TABLE_START_ADDR_LO32"),
        (h.ctx1_start_lo, 1, 1, "GCVM_CONTEXT1_PAGE_TABLE_START_ADDR_HI32"),
        (h.ctx1_end_lo, 14, 2, "GCVM_CONTEXT15_PAGE_TABLE_END_ADDR_LO32"),
        (h.ctx0_start_lo, 1, 1, "GCVM_CONTEXT0_PAGE_TABLE_START_ADDR_HI32"),
        (h.ctx0_end_lo, 1, 1, "GCVM_CONTEXT0_PAGE_TABLE_END_ADDR_HI32"),
        (h.eng0_req, 17, 1, "GCVM_INVALIDATE_ENG17_REQ"),
        (h.eng0_ack, 17, 1, "GCVM_INVALIDATE_ENG17_ACK"),
        (h.eng0_sem, 17, 1, "GCVM_INVALIDATE_ENG17_SEM"),
        (h.eng0_range_lo, 17, 2, "GCVM_INVALIDATE_ENG17_ADDR_RANGE_LO32"),
        (h.eng0_range_lo, 35, 1, "GCVM_INVALIDATE_ENG17_ADDR_RANGE_HI32"),
        (h.identity_aperture, 5, 1, "GCVM_L2_CONTEXT_IDENTITY_PHYSICAL_OFFSET_HI32"),
    ] {
        assert_eq!(by_name("gc_12_0_0", name).map(|m| m.offset), Some(nbreg::member(first, i, stride).offset), "{name}");
    }
    let x = hub41::f::ENABLE_L1_TLB;
    let fields: [(nbreg::Field, Field); 57] = [
        (x, g::GCMC_VM_MX_L1_TLB_CNTL__ENABLE_L1_TLB),
        (hub41::f::SYSTEM_ACCESS_MODE, g::GCMC_VM_MX_L1_TLB_CNTL__SYSTEM_ACCESS_MODE),
        (hub41::f::SYSTEM_APERTURE_UNMAPPED_ACCESS, g::GCMC_VM_MX_L1_TLB_CNTL__SYSTEM_APERTURE_UNMAPPED_ACCESS),
        (hub41::f::ENABLE_ADVANCED_DRIVER_MODEL, g::GCMC_VM_MX_L1_TLB_CNTL__ENABLE_ADVANCED_DRIVER_MODEL),
        (hub41::f::ECO_BITS, g::GCMC_VM_MX_L1_TLB_CNTL__ECO_BITS),
        (hub41::f::L1_MTYPE, g::GCMC_VM_MX_L1_TLB_CNTL__MTYPE),
        (hub41::f::ENABLE_L2_CACHE, g::GCVM_L2_CNTL__ENABLE_L2_CACHE),
        (hub41::f::ENABLE_L2_FRAGMENT_PROCESSING, g::GCVM_L2_CNTL__ENABLE_L2_FRAGMENT_PROCESSING),
        (hub41::f::L2_PDE0_CACHE_TAG_GENERATION_MODE, g::GCVM_L2_CNTL__L2_PDE0_CACHE_TAG_GENERATION_MODE),
        (hub41::f::ENABLE_DEFAULT_PAGE_OUT_TO_SYSTEM_MEMORY, g::GCVM_L2_CNTL__ENABLE_DEFAULT_PAGE_OUT_TO_SYSTEM_MEMORY),
        (hub41::f::PDE_FAULT_CLASSIFICATION, g::GCVM_L2_CNTL__PDE_FAULT_CLASSIFICATION),
        (hub41::f::CONTEXT1_IDENTITY_ACCESS_MODE, g::GCVM_L2_CNTL__CONTEXT1_IDENTITY_ACCESS_MODE),
        (hub41::f::IDENTITY_MODE_FRAGMENT_SIZE, g::GCVM_L2_CNTL__IDENTITY_MODE_FRAGMENT_SIZE),
        (hub41::f::INVALIDATE_ALL_L1_TLBS, g::GCVM_L2_CNTL2__INVALIDATE_ALL_L1_TLBS),
        (hub41::f::INVALIDATE_L2_CACHE, g::GCVM_L2_CNTL2__INVALIDATE_L2_CACHE),
        (hub41::f::BANK_SELECT, g::GCVM_L2_CNTL3__BANK_SELECT),
        (hub41::f::L2_CACHE_BIGK_FRAGMENT_SIZE, g::GCVM_L2_CNTL3__L2_CACHE_BIGK_FRAGMENT_SIZE),
        (hub41::f::VMC_TAP_PDE_REQUEST_PHYSICAL, g::GCVM_L2_CNTL4__VMC_TAP_PDE_REQUEST_PHYSICAL),
        (hub41::f::VMC_TAP_PTE_REQUEST_PHYSICAL, g::GCVM_L2_CNTL4__VMC_TAP_PTE_REQUEST_PHYSICAL),
        (hub41::f::L2_CACHE_SMALLK_FRAGMENT_SIZE, g::GCVM_L2_CNTL5__L2_CACHE_SMALLK_FRAGMENT_SIZE),
        (hub41::f::ENABLE_CONTEXT, g::GCVM_CONTEXT0_CNTL__ENABLE_CONTEXT),
        (hub41::f::PAGE_TABLE_DEPTH, g::GCVM_CONTEXT0_CNTL__PAGE_TABLE_DEPTH),
        (hub41::f::PAGE_TABLE_BLOCK_SIZE, g::GCVM_CONTEXT1_CNTL__PAGE_TABLE_BLOCK_SIZE),
        (hub41::f::RETRY_PERMISSION_OR_INVALID_PAGE_FAULT, g::GCVM_CONTEXT1_CNTL__RETRY_PERMISSION_OR_INVALID_PAGE_FAULT),
        (hub41::f::ACTIVE_PAGE_MIGRATION_PTE_READ_RETRY, g::GCVM_L2_PROTECTION_FAULT_CNTL2__ACTIVE_PAGE_MIGRATION_PTE_READ_RETRY),
        (hub41::f::PER_VMID_INVALIDATE_REQ, g::GCVM_INVALIDATE_ENG0_REQ__PER_VMID_INVALIDATE_REQ),
        (hub41::f::FLUSH_TYPE, g::GCVM_INVALIDATE_ENG0_REQ__FLUSH_TYPE),
        (hub41::f::INVALIDATE_L2_PTES, g::GCVM_INVALIDATE_ENG0_REQ__INVALIDATE_L2_PTES),
        (hub41::f::INVALIDATE_L2_PDE0, g::GCVM_INVALIDATE_ENG0_REQ__INVALIDATE_L2_PDE0),
        (hub41::f::INVALIDATE_L2_PDE1, g::GCVM_INVALIDATE_ENG0_REQ__INVALIDATE_L2_PDE1),
        (hub41::f::INVALIDATE_L2_PDE2, g::GCVM_INVALIDATE_ENG0_REQ__INVALIDATE_L2_PDE2),
        (hub41::f::INVALIDATE_L1_PTES, g::GCVM_INVALIDATE_ENG0_REQ__INVALIDATE_L1_PTES),
        (hub41::f::CLEAR_PROTECTION_FAULT_STATUS_ADDR, g::GCVM_INVALIDATE_ENG0_REQ__CLEAR_PROTECTION_FAULT_STATUS_ADDR),
        (hub41::f::CLEAR_FAULT_STATUS, g::GCVM_L2_PROTECTION_FAULT_CNTL__CLEAR_PROTECTION_FAULT_STATUS_ADDR),
        (hub41::f::CRASH_ON_NO_RETRY_FAULT, g::GCVM_L2_PROTECTION_FAULT_CNTL__CRASH_ON_NO_RETRY_FAULT),
        (hub41::f::CRASH_ON_RETRY_FAULT, g::GCVM_L2_PROTECTION_FAULT_CNTL__CRASH_ON_RETRY_FAULT),
        (fault12::st::MORE_FAULTS, g::GCVM_L2_PROTECTION_FAULT_STATUS_LO32__MORE_FAULTS),
        (fault12::st::WALKER_ERROR, g::GCVM_L2_PROTECTION_FAULT_STATUS_LO32__WALKER_ERROR),
        (fault12::st::PERMISSION_FAULTS, g::GCVM_L2_PROTECTION_FAULT_STATUS_LO32__PERMISSION_FAULTS),
        (fault12::st::MAPPING_ERROR, g::GCVM_L2_PROTECTION_FAULT_STATUS_LO32__MAPPING_ERROR),
        (fault12::st::CID, g::GCVM_L2_PROTECTION_FAULT_STATUS_LO32__CID),
        (fault12::st::RW, g::GCVM_L2_PROTECTION_FAULT_STATUS_LO32__RW),
        (fault12::st::VMID, g::GCVM_L2_PROTECTION_FAULT_STATUS_LO32__VMID),
        (fault12::st::VFID, g::GCVM_L2_PROTECTION_FAULT_STATUS_LO32__VFID),
        (fault12::st::PRT, g::GCVM_L2_PROTECTION_FAULT_STATUS_LO32__PRT),
        (gc12::f::BOOTLOAD_COMPLETE, g::RLC_RLCS_BOOTLOAD_STATUS__BOOTLOAD_COMPLETE),
        (gc12::f::CPG_UTCL1_ERROR_HALT_DISABLE, g::CP_DEBUG__CPG_UTCL1_ERROR_HALT_DISABLE),
        (gc12::f::PIPEID, g::GRBM_GFX_CNTL__PIPEID),
        (gc12::f::MEID, g::GRBM_GFX_CNTL__MEID),
        (gc12::f::VMID, g::GRBM_GFX_CNTL__VMID),
        (gc12::f::QUEUEID, g::GRBM_GFX_CNTL__QUEUEID),
        (gc12::f::PFP_PIPE0_RESET, g::CP_ME_CNTL__PFP_PIPE0_RESET),
        (gc12::f::PFP_PIPE1_RESET, g::CP_ME_CNTL__PFP_PIPE1_RESET),
        (gc12::f::ME_PIPE0_RESET, g::CP_ME_CNTL__ME_PIPE0_RESET),
        (gc12::f::ME_PIPE1_RESET, g::CP_ME_CNTL__ME_PIPE1_RESET),
        (gc12::f::CE_HALT, g::CP_ME_CNTL__CE_HALT),
        (gc12::f::PFP_HALT, g::CP_ME_CNTL__PFP_HALT),
    ];
    for (k, (core, made)) in fields.into_iter().enumerate() {
        assert_eq!((core.shift, core.mask), (made.shift, made.mask), "field {k}");
    }
    for (core, made) in [
        (gc12::f::ME_HALT, g::CP_ME_CNTL__ME_HALT),
        (gc12::f::MEC_PIPE0_RESET, g::CP_MEC_RS64_CNTL__MEC_PIPE0_RESET),
        (gc12::f::MEC_PIPE1_RESET, g::CP_MEC_RS64_CNTL__MEC_PIPE1_RESET),
        (gc12::f::MEC_PIPE2_RESET, g::CP_MEC_RS64_CNTL__MEC_PIPE2_RESET),
        (gc12::f::MEC_PIPE3_RESET, g::CP_MEC_RS64_CNTL__MEC_PIPE3_RESET),
        (gc12::f::MEC_HALT, g::CP_MEC_RS64_CNTL__MEC_HALT),
    ] {
        assert_eq!((core.shift, core.mask), (made.shift, made.mask));
    }
    // MMHUB's status has the bits GFXHUB's has, and the fault defaults are the same eleven bits on both hubs.
    use mmhub_4_1_0::f as m;
    for (a, b) in [(m::MMVM_L2_PROTECTION_FAULT_STATUS_LO32__CID, fault12::st::CID), (m::MMVM_L2_PROTECTION_FAULT_STATUS_LO32__RW, fault12::st::RW), (m::MMVM_L2_PROTECTION_FAULT_STATUS_LO32__MAPPING_ERROR, fault12::st::MAPPING_ERROR), (m::MMVM_L2_PROTECTION_FAULT_STATUS_LO32__VFID, fault12::st::VFID)] {
        assert_eq!((a.shift, a.mask), (b.shift, b.mask));
    }
    let defaults = [
        g::GCVM_L2_PROTECTION_FAULT_CNTL__RANGE_PROTECTION_FAULT_ENABLE_DEFAULT,
        g::GCVM_L2_PROTECTION_FAULT_CNTL__PDE0_PROTECTION_FAULT_ENABLE_DEFAULT,
        g::GCVM_L2_PROTECTION_FAULT_CNTL__PDE1_PROTECTION_FAULT_ENABLE_DEFAULT,
        g::GCVM_L2_PROTECTION_FAULT_CNTL__PDE2_PROTECTION_FAULT_ENABLE_DEFAULT,
        g::GCVM_L2_PROTECTION_FAULT_CNTL__TRANSLATE_FURTHER_PROTECTION_FAULT_ENABLE_DEFAULT,
        g::GCVM_L2_PROTECTION_FAULT_CNTL__NACK_PROTECTION_FAULT_ENABLE_DEFAULT,
        g::GCVM_L2_PROTECTION_FAULT_CNTL__DUMMY_PAGE_PROTECTION_FAULT_ENABLE_DEFAULT,
        g::GCVM_L2_PROTECTION_FAULT_CNTL__VALID_PROTECTION_FAULT_ENABLE_DEFAULT,
        g::GCVM_L2_PROTECTION_FAULT_CNTL__READ_PROTECTION_FAULT_ENABLE_DEFAULT,
        g::GCVM_L2_PROTECTION_FAULT_CNTL__WRITE_PROTECTION_FAULT_ENABLE_DEFAULT,
        g::GCVM_L2_PROTECTION_FAULT_CNTL__EXECUTE_PROTECTION_FAULT_ENABLE_DEFAULT,
    ];
    assert_eq!(hub41::f::FAULT_DEFAULTS, defaults.iter().fold(0, |acc, f| acc | f.mask));
    let ctx = [g::GCVM_CONTEXT1_CNTL__RANGE_PROTECTION_FAULT_ENABLE_DEFAULT, g::GCVM_CONTEXT1_CNTL__DUMMY_PAGE_PROTECTION_FAULT_ENABLE_DEFAULT, g::GCVM_CONTEXT1_CNTL__PDE0_PROTECTION_FAULT_ENABLE_DEFAULT, g::GCVM_CONTEXT1_CNTL__VALID_PROTECTION_FAULT_ENABLE_DEFAULT, g::GCVM_CONTEXT1_CNTL__READ_PROTECTION_FAULT_ENABLE_DEFAULT, g::GCVM_CONTEXT1_CNTL__WRITE_PROTECTION_FAULT_ENABLE_DEFAULT, g::GCVM_CONTEXT1_CNTL__EXECUTE_PROTECTION_FAULT_ENABLE_DEFAULT];
    assert_eq!(hub41::f::CONTEXT_FAULT_DEFAULTS, ctx.iter().fold(0, |acc, f| acc | f.mask));
}
