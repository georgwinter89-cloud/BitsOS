//! Wave 43, SDMA 7: what `rdna4_core` names by hand against the generated numbers.

use crate::*;

/// Wave 43: every register and field `rdna4_core::sdma7` names equals the generated one (SDMA 7's registers lie
/// in GC 12's header; NBIF's doorbell entry and the in-ring HDP flush's two registers in NBIF 6.3.1's).
#[test]
fn the_sdma_7_registers_and_fields_of_rdna4_core_equal_the_generated_ones() {
    use gc_12_0_0 as g;
    use nbif_6_3_1 as nb;
    use rdna4_core::sdma7 as s;
    let regs: [(rdna4_core::regmap::Reg, Reg); 30] = [
        (s::UCODE_REV, g::SDMA0_UCODE_REV),
        (s::CNTL, g::SDMA0_CNTL),
        (s::RB_RPTR_FETCH, g::SDMA0_RB_RPTR_FETCH),
        (s::RB_RPTR_FETCH_HI, g::SDMA0_RB_RPTR_FETCH_HI),
        (s::STATUS_REG, g::SDMA0_STATUS_REG),
        (s::WATCHDOG_CNTL, g::SDMA0_WATCHDOG_CNTL),
        (s::UTCL1_CNTL, g::SDMA0_UTCL1_CNTL),
        (s::UTCL1_PAGE, g::SDMA0_UTCL1_PAGE),
        (s::UTCL1_RD_STATUS, g::SDMA0_UTCL1_RD_STATUS),
        (s::UTCL1_WR_STATUS, g::SDMA0_UTCL1_WR_STATUS),
        (s::INT_STATUS, g::SDMA0_INT_STATUS),
        (s::RB_CNTL, g::SDMA0_QUEUE0_RB_CNTL),
        (s::RB_BASE, g::SDMA0_QUEUE0_RB_BASE),
        (s::RB_BASE_HI, g::SDMA0_QUEUE0_RB_BASE_HI),
        (s::RB_RPTR, g::SDMA0_QUEUE0_RB_RPTR),
        (s::RB_RPTR_HI, g::SDMA0_QUEUE0_RB_RPTR_HI),
        (s::RB_WPTR, g::SDMA0_QUEUE0_RB_WPTR),
        (s::RB_WPTR_HI, g::SDMA0_QUEUE0_RB_WPTR_HI),
        (s::RB_RPTR_ADDR_LO, g::SDMA0_QUEUE0_RB_RPTR_ADDR_LO),
        (s::RB_RPTR_ADDR_HI, g::SDMA0_QUEUE0_RB_RPTR_ADDR_HI),
        (s::IB_CNTL, g::SDMA0_QUEUE0_IB_CNTL),
        (s::DOORBELL, g::SDMA0_QUEUE0_DOORBELL),
        (s::DOORBELL_OFFSET, g::SDMA0_QUEUE0_DOORBELL_OFFSET),
        (s::RB_WPTR_POLL_ADDR_LO, g::SDMA0_QUEUE0_RB_WPTR_POLL_ADDR_LO),
        (s::RB_WPTR_POLL_ADDR_HI, g::SDMA0_QUEUE0_RB_WPTR_POLL_ADDR_HI),
        (s::MINOR_PTR_UPDATE, g::SDMA0_QUEUE0_MINOR_PTR_UPDATE),
        (s::MCU_CNTL, g::SDMA0_MCU_CNTL),
        (s::DOORBELL_ENTRY_2, nb::GDC_S2A0_S2A_DOORBELL_ENTRY_2_CTRL),
        (s::GPU_HDP_FLUSH_REQ, nb::BIF_BX_PF0_GPU_HDP_FLUSH_REQ),
        (s::GPU_HDP_FLUSH_DONE, nb::BIF_BX_PF0_GPU_HDP_FLUSH_DONE),
    ];
    for (core, made) in regs {
        assert_eq!((core.name, core.base_index, core.offset), (made.name, made.base_idx, made.offset), "{}", core.name);
    }
    // The model's table of an engine holds exactly the GC registers above.
    assert_eq!(s::ALL.len(), 27);
    let fields: [(rdna4_core::nbreg::Field, Field); 26] = [
        (s::f::TRAP_ENABLE, g::f::SDMA0_CNTL__TRAP_ENABLE),
        (s::f::PAGE_FAULT_INT_ENABLE, g::f::SDMA0_CNTL__PAGE_FAULT_INT_ENABLE),
        (s::f::IDLE, g::f::SDMA0_STATUS_REG__IDLE),
        (s::f::RB_EMPTY, g::f::SDMA0_STATUS_REG__RB_EMPTY),
        (s::f::QUEUE_HANG_COUNT, g::f::SDMA0_WATCHDOG_CNTL__QUEUE_HANG_COUNT),
        (s::f::REDO_DELAY, g::f::SDMA0_UTCL1_CNTL__REDO_DELAY),
        (s::f::RESP_MODE, g::f::SDMA0_UTCL1_CNTL__RESP_MODE),
        (s::f::RD_L2_POLICY, g::f::SDMA0_UTCL1_PAGE__RD_L2_POLICY),
        (s::f::WR_L2_POLICY, g::f::SDMA0_UTCL1_PAGE__WR_L2_POLICY),
        (s::f::RB_ENABLE, g::f::SDMA0_QUEUE0_RB_CNTL__RB_ENABLE),
        (s::f::RB_SIZE, g::f::SDMA0_QUEUE0_RB_CNTL__RB_SIZE),
        (s::f::WPTR_POLL_ENABLE, g::f::SDMA0_QUEUE0_RB_CNTL__WPTR_POLL_ENABLE),
        (s::f::MCU_WPTR_POLL_ENABLE, g::f::SDMA0_QUEUE0_RB_CNTL__MCU_WPTR_POLL_ENABLE),
        (s::f::RPTR_WRITEBACK_ENABLE, g::f::SDMA0_QUEUE0_RB_CNTL__RPTR_WRITEBACK_ENABLE),
        (s::f::RB_PRIV, g::f::SDMA0_QUEUE0_RB_CNTL__RB_PRIV),
        (s::f::IB_ENABLE, g::f::SDMA0_QUEUE0_IB_CNTL__IB_ENABLE),
        (s::f::DOORBELL_ENABLE, g::f::SDMA0_QUEUE0_DOORBELL__ENABLE),
        (s::f::DOORBELL_OFFSET, g::f::SDMA0_QUEUE0_DOORBELL_OFFSET__OFFSET),
        (s::f::MINOR_PTR_UPDATE_ENABLE, g::f::SDMA0_QUEUE0_MINOR_PTR_UPDATE__ENABLE),
        (s::f::HALT, g::f::SDMA0_MCU_CNTL__HALT),
        (s::f::RESET, g::f::SDMA0_MCU_CNTL__RESET),
        (s::PORT2_ENABLE, nb::f::GDC_S2A0_S2A_DOORBELL_ENTRY_2_CTRL__S2A_DOORBELL_PORT2_ENABLE),
        (s::PORT2_AWID, nb::f::GDC_S2A0_S2A_DOORBELL_ENTRY_2_CTRL__S2A_DOORBELL_PORT2_AWID),
        (s::PORT2_RANGE_OFFSET, nb::f::GDC_S2A0_S2A_DOORBELL_ENTRY_2_CTRL__S2A_DOORBELL_PORT2_RANGE_OFFSET),
        (s::PORT2_RANGE_SIZE, nb::f::GDC_S2A0_S2A_DOORBELL_ENTRY_2_CTRL__S2A_DOORBELL_PORT2_RANGE_SIZE),
        (s::PORT2_AWADDR_31_28_VALUE, nb::f::GDC_S2A0_S2A_DOORBELL_ENTRY_2_CTRL__S2A_DOORBELL_PORT2_AWADDR_31_28_VALUE),
    ];
    for (core, made) in fields {
        assert_eq!((core.shift, core.mask), (made.shift, made.mask));
    }
    assert_eq!((s::hdp_flush_bit(0), s::hdp_flush_bit(1)), (nb::f::BIF_BX_PF0_GPU_HDP_FLUSH_DONE__SDMA0.mask, nb::f::BIF_BX_PF0_GPU_HDP_FLUSH_DONE__SDMA1.mask));
    // Engine 1's registers exist under their own names in the header, 0x600 (0x20 for the MCU control) on.
    assert_eq!(s::instance(s::RB_CNTL, 1).offset, g::SDMA1_QUEUE0_RB_CNTL.offset);
    assert_eq!(s::instance(s::CNTL, 1).offset, g::SDMA1_CNTL.offset);
    assert_eq!((s::instance(s::MCU_CNTL, 1).offset, s::MCU_CNTL.base_index), (g::SDMA1_MCU_CNTL.offset, g::SDMA1_MCU_CNTL.base_idx));
}
