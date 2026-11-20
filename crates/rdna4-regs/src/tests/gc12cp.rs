//! Wave 44, item CP: what `rdna4_core` names by hand for the command processor's resume, its doorbells and its
//! queue descriptors, against the generated numbers.

use crate::*;

#[test]
fn the_wave_44_cp_registers_and_fields_of_rdna4_core_equal_the_generated_ones() {
    use gc_12_0_0::f as g;
    use rdna4_core::{doorbell12 as db, gc12cp as cp, mes12 as m12, mqd12, nbreg::Field as F};
    for r in cp::WRITTEN.iter().chain(&[
        cp::GRBM_CC_GC_SA_UNIT_DISABLE,
        cp::GRBM_GC_USER_SA_UNIT_DISABLE,
        cp::CC_RB_BACKEND_DISABLE,
        cp::GC_USER_RB_BACKEND_DISABLE,
        cp::CC_GC_SHADER_ARRAY_CONFIG,
        cp::GC_USER_SHADER_ARRAY_CONFIG,
        cp::TA_CNTL2,
        cp::SCRATCH_REG0,
        rdna4_core::gc12::CP_STAT,
    ]) {
        assert_eq!(by_name("gc_12_0_0", r.name).map(|m| (m.base_idx, m.offset)), Some((r.base_index, r.offset)), "{}", r.name);
    }
    for r in [db::ENTRY_0, db::ENTRY_3, db::STRAP0] {
        assert_eq!(by_name("nbif_6_3_1", r.name).map(|m| (m.base_idx, m.offset)), Some((r.base_index, r.offset)), "{}", r.name);
    }
    let same = |a: F, b: Field| (a.shift, a.mask) == (b.shift, b.mask);
    let pairs: [(F, Field); 53] = [
        (cp::f::READ_TIMEOUT, g::GRBM_CNTL__READ_TIMEOUT),
        (cp::f::INSTANCE_INDEX, g::GRBM_GFX_INDEX__INSTANCE_INDEX),
        (cp::f::SA_INDEX, g::GRBM_GFX_INDEX__SA_INDEX),
        (cp::f::SE_INDEX, g::GRBM_GFX_INDEX__SE_INDEX),
        (cp::f::SA_BROADCAST_WRITES, g::GRBM_GFX_INDEX__SA_BROADCAST_WRITES),
        (cp::f::INSTANCE_BROADCAST_WRITES, g::GRBM_GFX_INDEX__INSTANCE_BROADCAST_WRITES),
        (cp::f::SE_BROADCAST_WRITES, g::GRBM_GFX_INDEX__SE_BROADCAST_WRITES),
        (cp::f::SA_DISABLE, g::GRBM_CC_GC_SA_UNIT_DISABLE__SA_DISABLE),
        (cp::f::SA_DISABLE, g::GRBM_GC_USER_SA_UNIT_DISABLE__SA_DISABLE),
        (cp::f::BACKEND_DISABLE, g::CC_RB_BACKEND_DISABLE__BACKEND_DISABLE),
        (cp::f::BACKEND_DISABLE, g::GC_USER_RB_BACKEND_DISABLE__BACKEND_DISABLE),
        (cp::f::INACTIVE_WGPS, g::CC_GC_SHADER_ARRAY_CONFIG__INACTIVE_WGPS),
        (cp::f::INACTIVE_WGPS, g::GC_USER_SHADER_ARRAY_CONFIG__INACTIVE_WGPS),
        (cp::f::TRUNCATE_COORD_MODE, g::TA_CNTL2__TRUNCATE_COORD_MODE),
        (cp::f::COUNTER_CONTROL, g::DB_RING_CONTROL__COUNTER_CONTROL),
        (cp::f::ADDRESS_MODE, g::SH_MEM_CONFIG__ADDRESS_MODE),
        (cp::f::ALIGNMENT_MODE, g::SH_MEM_CONFIG__ALIGNMENT_MODE),
        (cp::f::INITIAL_INST_PREFETCH, g::SH_MEM_CONFIG__INITIAL_INST_PREFETCH),
        (cp::f::PRIVATE_BASE, g::SH_MEM_BASES__PRIVATE_BASE),
        (cp::f::SHARED_BASE, g::SH_MEM_BASES__SHARED_BASE),
        (cp::f::TRAP_EN, g::SPI_GDBG_PER_VMID_CNTL__TRAP_EN),
        (cp::f::SRM_ENABLE, g::RLC_SRM_CNTL__SRM_ENABLE),
        (cp::f::AUTO_INCR_ADDR, g::RLC_SRM_CNTL__AUTO_INCR_ADDR),
        (cp::f::CMP_BUSY_INT_ENABLE, g::CP_INT_CNTL_RING0__CMP_BUSY_INT_ENABLE),
        (cp::f::CNTX_BUSY_INT_ENABLE, g::CP_INT_CNTL_RING0__CNTX_BUSY_INT_ENABLE),
        (cp::f::CNTX_EMPTY_INT_ENABLE, g::CP_INT_CNTL_RING0__CNTX_EMPTY_INT_ENABLE),
        (cp::f::GFX_IDLE_INT_ENABLE, g::CP_INT_CNTL_RING0__GFX_IDLE_INT_ENABLE),
        (cp::f::MEC_INVALIDATE_ICACHE, g::CP_MEC_RS64_CNTL__MEC_INVALIDATE_ICACHE),
        (cp::f::MEC_PIPE0_ACTIVE, g::CP_MEC_RS64_CNTL__MEC_PIPE0_ACTIVE),
        (cp::f::MEC_PIPE1_ACTIVE, g::CP_MEC_RS64_CNTL__MEC_PIPE1_ACTIVE),
        (cp::f::MEC_PIPE2_ACTIVE, g::CP_MEC_RS64_CNTL__MEC_PIPE2_ACTIVE),
        (cp::f::MEC_PIPE3_ACTIVE, g::CP_MEC_RS64_CNTL__MEC_PIPE3_ACTIVE),
        (m12::f::MQD_VMID, g::CP_GFX_MQD_CONTROL__VMID),
        (mqd12::GFX_PRIV_STATE, g::CP_GFX_MQD_CONTROL__PRIV_STATE),
        (mqd12::GFX_CACHE_POLICY, g::CP_GFX_MQD_CONTROL__CACHE_POLICY),
        (mqd12::QUANTUM_EN, g::CP_GFX_HQD_QUANTUM__QUANTUM_EN),
        (mqd12::RB_BUFSZ, g::CP_GFX_HQD_CNTL__RB_BUFSZ),
        (mqd12::RB_BLKSZ, g::CP_GFX_HQD_CNTL__RB_BLKSZ),
        (mqd12::GFX_KMD_QUEUE, g::CP_GFX_HQD_CNTL__KMD_QUEUE),
        (m12::f::DOORBELL_OFFSET, g::CP_RB_DOORBELL_CONTROL__DOORBELL_OFFSET),
        (m12::f::DOORBELL_EN, g::CP_RB_DOORBELL_CONTROL__DOORBELL_EN),
        (m12::f::DOORBELL_OFFSET, g::CP_HQD_PQ_DOORBELL_CONTROL__DOORBELL_OFFSET),
        (m12::f::DOORBELL_SOURCE, g::CP_HQD_PQ_DOORBELL_CONTROL__DOORBELL_SOURCE),
        (m12::f::DOORBELL_EN, g::CP_HQD_PQ_DOORBELL_CONTROL__DOORBELL_EN),
        (m12::f::DOORBELL_HIT, g::CP_HQD_PQ_DOORBELL_CONTROL__DOORBELL_HIT),
        (m12::f::EOP_SIZE, g::CP_HQD_EOP_CONTROL__EOP_SIZE),
        (m12::f::MQD_VMID, g::CP_MQD_CONTROL__VMID),
        (m12::f::QUEUE_SIZE, g::CP_HQD_PQ_CONTROL__QUEUE_SIZE),
        (m12::f::RPTR_BLOCK_SIZE, g::CP_HQD_PQ_CONTROL__RPTR_BLOCK_SIZE),
        (m12::f::UNORD_DISPATCH, g::CP_HQD_PQ_CONTROL__UNORD_DISPATCH),
        (m12::f::PRELOAD_SIZE, g::CP_HQD_PERSISTENT_STATE__PRELOAD_SIZE),
        (mqd12::MIN_IB_AVAIL_SIZE, g::CP_HQD_IB_CONTROL__MIN_IB_AVAIL_SIZE),
        (mqd12::QUANTUM_DURATION, g::CP_HQD_QUANTUM__QUANTUM_DURATION),
    ];
    for (k, (a, b)) in pairs.into_iter().enumerate() {
        assert!(same(a, b), "field {k}");
    }
    for (a, b) in [
        (m12::f::TUNNEL_DISPATCH, g::CP_HQD_PQ_CONTROL__TUNNEL_DISPATCH),
        (m12::f::PRIV_STATE, g::CP_HQD_PQ_CONTROL__PRIV_STATE),
        (m12::f::KMD_QUEUE, g::CP_HQD_PQ_CONTROL__KMD_QUEUE),
        (mqd12::QUANTUM_EN, g::CP_HQD_QUANTUM__QUANTUM_EN),
        (mqd12::QUANTUM_SCALE, g::CP_HQD_QUANTUM__QUANTUM_SCALE),
        (db::ATI_REV_ID, nbif_6_3_1::f::RCC_STRAP0_RCC_DEV0_EPF0_STRAP0__STRAP_ATI_REV_ID_DEV0_F0),
    ] {
        assert!(same(a, b));
    }
}
