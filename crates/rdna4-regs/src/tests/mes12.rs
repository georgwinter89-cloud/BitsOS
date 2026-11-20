//! Wave 44, item ME: every register and field `rdna4_core::mes12` names by hand against the generated numbers.

use crate::*;

#[test]
fn the_mes_registers_and_fields_of_rdna4_core_equal_the_generated_ones() {
    use gc_12_0_0::f as g;
    use rdna4_core::mes12::{self as m, f};
    let regs = [m::CP_MES_CNTL, m::CP_MES_PRGRM_CNTR_START, m::CP_MES_PRGRM_CNTR_START_HI, m::CP_MES_GP3_LO, m::CP_MES_DOORBELL_CONTROL1, m::CP_UNMAPPED_DOORBELL, m::GRBM_GFX_CNTL];
    for r in regs.iter().chain(&m::HQD) {
        assert_eq!(by_name("gc_12_0_0", r.name).map(|x| (x.base_idx, x.offset)), Some((r.base_index, r.offset)), "{}", r.name);
    }
    for (i, name) in ["CP_MES_DOORBELL_CONTROL2", "CP_MES_DOORBELL_CONTROL3", "CP_MES_DOORBELL_CONTROL4", "CP_MES_DOORBELL_CONTROL5"].into_iter().enumerate() {
        assert_eq!(by_name("gc_12_0_0", name).map(|x| x.offset), Some(m::doorbell_control(i as u32 + 1).offset), "{name}");
    }
    let fields: [(rdna4_core::nbreg::Field, Field); 23] = [
        (f::MES_INVALIDATE_ICACHE, g::CP_MES_CNTL__MES_INVALIDATE_ICACHE),
        (f::MES_PIPE0_RESET, g::CP_MES_CNTL__MES_PIPE0_RESET),
        (f::MES_PIPE1_RESET, g::CP_MES_CNTL__MES_PIPE1_RESET),
        (f::MES_PIPE0_ACTIVE, g::CP_MES_CNTL__MES_PIPE0_ACTIVE),
        (f::MES_PIPE1_ACTIVE, g::CP_MES_CNTL__MES_PIPE1_ACTIVE),
        (f::MES_HALT, g::CP_MES_CNTL__MES_HALT),
        (f::QUEUE_SIZE, g::CP_HQD_PQ_CONTROL__QUEUE_SIZE),
        (f::RPTR_BLOCK_SIZE, g::CP_HQD_PQ_CONTROL__RPTR_BLOCK_SIZE),
        (f::NO_UPDATE_RPTR, g::CP_HQD_PQ_CONTROL__NO_UPDATE_RPTR),
        (f::UNORD_DISPATCH, g::CP_HQD_PQ_CONTROL__UNORD_DISPATCH),
        (f::TUNNEL_DISPATCH, g::CP_HQD_PQ_CONTROL__TUNNEL_DISPATCH),
        (f::PRIV_STATE, g::CP_HQD_PQ_CONTROL__PRIV_STATE),
        (f::KMD_QUEUE, g::CP_HQD_PQ_CONTROL__KMD_QUEUE),
        (f::DOORBELL_OFFSET, g::CP_HQD_PQ_DOORBELL_CONTROL__DOORBELL_OFFSET),
        (f::DOORBELL_SOURCE, g::CP_HQD_PQ_DOORBELL_CONTROL__DOORBELL_SOURCE),
        (f::DOORBELL_EN, g::CP_HQD_PQ_DOORBELL_CONTROL__DOORBELL_EN),
        (f::DOORBELL_HIT, g::CP_HQD_PQ_DOORBELL_CONTROL__DOORBELL_HIT),
        (f::PRELOAD_SIZE, g::CP_HQD_PERSISTENT_STATE__PRELOAD_SIZE),
        (f::EOP_SIZE, g::CP_HQD_EOP_CONTROL__EOP_SIZE),
        (f::MQD_VMID, g::CP_MQD_CONTROL__VMID),
        (f::UNMAPPED_ENABLE, g::CP_UNMAPPED_DOORBELL__ENABLE),
        (f::PROC_LSB, g::CP_UNMAPPED_DOORBELL__PROC_LSB),
        (f::DB_UPDATED_MSG_EN, g::CP_HQD_GFX_CONTROL__DB_UPDATED_MSG_EN),
    ];
    for (k, (core, made)) in fields.into_iter().enumerate() {
        assert_eq!((core.shift, core.mask), (made.shift, made.mask), "field {k}");
    }
    // The aggregated doorbells' fields lie where the queue doorbell's do, which `mes12::aggregated` relies on.
    for (q, a) in [(f::DOORBELL_OFFSET, g::CP_MES_DOORBELL_CONTROL1__DOORBELL_OFFSET), (f::DOORBELL_EN, g::CP_MES_DOORBELL_CONTROL1__DOORBELL_EN), (f::DOORBELL_HIT, g::CP_MES_DOORBELL_CONTROL1__DOORBELL_HIT), (f::HQD_VMID, g::CP_HQD_VMID__VMID)] {
        assert_eq!((q.shift, q.mask), (a.shift, a.mask));
    }
}
