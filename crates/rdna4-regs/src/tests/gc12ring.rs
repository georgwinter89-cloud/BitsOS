//! Wave 44, item RG: what `rdna4_core::gc12ring` names by hand for the rings' start, against the generated numbers.

use crate::*;

#[test]
fn the_ring_registers_and_fields_of_rdna4_core_equal_the_generated_ones() {
    use gc_12_0_0::f as g;
    use rdna4_core::gc12ring as r;
    for x in [r::CP_MAX_CONTEXT, r::CP_DEVICE_ID, r::CP_ME1_PIPE0_INT_CNTL, r::CP_ME1_PIPE1_INT_CNTL, r::GRBM_STATUS, r::CP_INT_CNTL_RING0, r::SCRATCH_REG0, r::CP_STAT] {
        assert_eq!(by_name("gc_12_0_0", x.name).map(|m| (m.base_idx, m.offset)), Some((x.base_index, x.offset)), "{}", x.name);
    }
    let pairs = [
        (r::f::PRIV_INSTR_INT_ENABLE, g::CP_INT_CNTL_RING0__PRIV_INSTR_INT_ENABLE),
        (r::f::PRIV_REG_INT_ENABLE, g::CP_INT_CNTL_RING0__PRIV_REG_INT_ENABLE),
        (r::f::OPCODE_ERROR_INT_ENABLE, g::CP_INT_CNTL_RING0__OPCODE_ERROR_INT_ENABLE),
        (r::f::TIME_STAMP_INT_ENABLE, g::CP_INT_CNTL_RING0__TIME_STAMP_INT_ENABLE),
        (r::f::GENERIC0_INT_ENABLE, g::CP_INT_CNTL_RING0__GENERIC0_INT_ENABLE),
        (r::f::PRIV_REG_INT_ENABLE, g::CP_ME1_PIPE0_INT_CNTL__PRIV_REG_INT_ENABLE),
        (r::f::OPCODE_ERROR_INT_ENABLE, g::CP_ME1_PIPE0_INT_CNTL__OPCODE_ERROR_INT_ENABLE),
        (r::f::TIME_STAMP_INT_ENABLE, g::CP_ME1_PIPE0_INT_CNTL__TIME_STAMP_INT_ENABLE),
        (r::f::GENERIC0_INT_ENABLE, g::CP_ME1_PIPE0_INT_CNTL__GENERIC0_INT_ENABLE),
        (r::f::PRIV_REG_INT_ENABLE, g::CP_ME1_PIPE1_INT_CNTL__PRIV_REG_INT_ENABLE),
        (r::f::OPCODE_ERROR_INT_ENABLE, g::CP_ME1_PIPE1_INT_CNTL__OPCODE_ERROR_INT_ENABLE),
        (r::f::CP_BUSY, g::GRBM_STATUS__CP_BUSY),
        (r::f::GUI_ACTIVE, g::GRBM_STATUS__GUI_ACTIVE),
        (r::f::CP_STAT_BUSY, g::CP_STAT__CP_BUSY),
    ];
    for (k, (a, b)) in pairs.into_iter().enumerate() {
        assert_eq!((a.shift, a.mask), (b.shift, b.mask), "field {k}");
    }
}
