//! The generated numbers against ones read by hand in Linux 7.3.1's headers (file:line in each comment) and against
//! the scout's arithmetic (plan/spec/gpu-rdna4.md section 1, register reach) on the card's measured bases.

use crate::*;
use rdna4_core::ipfacts::{TABLE, find, hw};
use std::vec::Vec;

mod gc12;
mod gc12cp;
mod gc12ring;
mod mes12;
mod sdma7;
mod wave42;

fn bases(hw_id: u16) -> &'static [u32] {
    find(&TABLE, hw_id, 0).expect("the card has this IP").bases
}

#[test]
fn hand_checked_offsets() {
    // (register, base index, dword): include/asic_reg/<dir>/<block>_offset.h:<line>.
    for (r, base, off) in [
        (mp_14_0_2::MPASP_SMN_C2PMSG_64, 0, 0x80),             // mp:387
        (mp_14_0_2::MPASP_SMN_C2PMSG_81, 0, 0x91),             // mp:421
        (mp_14_0_2::MP1_SMN_C2PMSG_64, 1, 0x80),               // mp:157
        (mp_14_0_2::MP1_SMN_C2PMSG_81, 1, 0x91),               // mp:191
        (nbif_6_3_1::RCC_DEV0_EPF0_RCC_CONFIG_MEMSIZE, 2, 0xC3), // nbif:1809
        (osssys_7_0_0::IH_RB_CNTL, 0, 0x80),                   // oss:112
        (osssys_7_0_0::IH_RB_WPTR, 0, 0x82),                   // oss:116
        (gc_12_0_0::SDMA0_QUEUE0_RB_CNTL, 0, 0x80),            // gc:182 (SDMA 7 lives in the GC header)
        (gc_12_0_0::GRBM_STATUS, 0, 0xDA4),                    // gc:2002
        (gc_12_0_0::CP_RB0_BASE, 0, 0x1DE0),                   // gc:3456 (a name that itself ends in _BASE)
        (mmhub_4_1_0::MMMC_VM_FB_LOCATION_BASE, 0, 0x554),     // mmhub:860
        (mmhub_4_1_0::MMVM_CONTEXT0_CNTL, 0, 0x564),           // mmhub:880
        (thm_14_0_2::CG_MULT_THERMAL_STATUS, 0, 0xF),          // thm:61
        (smuio_14_0_2::SMUIO_MCM_CONFIG, 0, 0x23),             // smuio:68
    ] {
        assert_eq!((r.base_idx, r.offset), (base, off), "{}", r.name);
    }
    // sh_mask.h: oss:171/187, oss:185/201, gc:5931/5952.
    assert_eq!(osssys_7_0_0::f::IH_RB_CNTL__RB_SIZE, Field::new(1, 0x3E));
    assert_eq!(osssys_7_0_0::f::IH_RB_CNTL__WPTR_OVERFLOW_CLEAR, Field::new(31, 0x8000_0000));
    assert_eq!(gc_12_0_0::f::GRBM_STATUS__GUI_ACTIVE, Field::new(31, 0x8000_0000));
}

#[test]
fn the_cards_bases_place_the_first_registers_where_the_scout_computed() {
    // PSP C2PMSG_64 = (0x16000 + 0x80) dwords = BAR5 byte 0x58200; Linux's fixed C2PMSG_33 is dword 0x16061.
    assert_eq!(mp_14_0_2::MPASP_SMN_C2PMSG_64.bar5_byte(bases(hw::MP0)), Some(0x58200));
    assert_eq!(mp_14_0_2::MPASP_SMN_C2PMSG_33.dword(bases(hw::MP0)), Some(rdna4_core::attach::C2PMSG_33_DWORD));
    // SMU message, argument, answer: base 1 (0x16200) + 0x82 / 0x92 / 0x9A.
    let smu = [mp_14_0_2::MP1_SMN_C2PMSG_66, mp_14_0_2::MP1_SMN_C2PMSG_82, mp_14_0_2::MP1_SMN_C2PMSG_90];
    assert_eq!(smu.map(|r| r.dword(bases(hw::MP1))), [Some(0x16282), Some(0x16292), Some(0x1629A)]);
    // MEMSIZE = 0xD20 + 0xC3.
    assert_eq!(nbif_6_3_1::RCC_DEV0_EPF0_RCC_CONFIG_MEMSIZE.dword(bases(hw::NBIF)), Some(rdna4_core::attach::MEMSIZE_DWORD));
}

#[test]
fn bar5_reaches_what_the_first_modules_need() {
    // GC: base index 0 and 1 only (2131 and 3028 registers), all inside BAR5 on this card.
    let count = |i: u8| gc_12_0_0::REGS.iter().filter(|r| r.base_idx == i).count();
    assert_eq!((count(0), count(1), gc_12_0_0::REGS.len()), (2131, 3028, 5159));
    assert!(gc_12_0_0::REGS.iter().all(|r| r.bar5_byte(bases(hw::GC)).is_some()));
    for (regs, hw_id) in [(&osssys_7_0_0::REGS[..], hw::OSSSYS), (&mmhub_4_1_0::REGS[..], hw::MMHUB), (&hdp_7_0_0::REGS[..], hw::HDP)] {
        let outside: Vec<&str> = regs.iter().filter(|r| r.bar5_byte(bases(hw_id)).is_none()).map(|r| r.name).collect();
        assert_eq!(outside, [""; 0], "past BAR5");
    }
    // The MP header holds two IPs. Linux places each register by its own IP's bases (SOC15_REG_OFFSET(MP0, ..) for
    // MPASP_*, amdgpu/psp_v14_0.c:117; SOC15_REG_OFFSET(MP1, ..) for MP1_*, pm/swsmu/smu14/smu_v14_0_2_ppt.c:2149), so the header is split by
    // the name's prefix before anything is placed.
    fn of(prefix: &'static str) -> impl Iterator<Item = &'static Reg> {
        mp_14_0_2::REGS.iter().filter(move |r| r.name.starts_with(prefix))
    }
    assert_eq!((of("MPASP_").count(), of("MP1_").count(), mp_14_0_2::REGS.len()), (48, 166, 214));
    let outside = |prefix: &'static str, hw_id: u16| -> Vec<(&str, Option<u32>)> {
        of(prefix).filter(|r| r.bar5_byte(bases(hw_id)).is_none()).map(|r| (r.name, r.dword(bases(hw_id)))).collect()
    };
    assert_eq!(outside("MPASP_", hw::MP0), []);
    // One MP1 register lies past BAR5 by MP1's own bases (base 7 = 0xEC0000, + 0x4009): the SMU's firmware flags.
    // Linux does not take it from the table at all: it reads the fixed SMN address smnMP1_FIRMWARE_FLAGS = 0x3010024
    // through the PCIE index pair (pm/swsmu/inc/smu_v14_0.h:38, smu14/smu_v14_0.c:147-152), and so must G2. The
    // table's place and Linux's constant are NOT the same number, so G2 takes the constant.
    assert_eq!(outside("MP1_", hw::MP1), [("MP1_CRU1_MP1_FIRMWARE_FLAGS", Some(0xEC_4009))]);
    assert_ne!(0xEC_4009u32 * 4, 0x301_0024);
    // On this card the two IPs list the same bases, so placing MP1_* by MP0's bases gave the same answer; a card
    // where they differ is caught by the split above, not by luck.
    assert_eq!(bases(hw::MP0), bases(hw::MP1));
    // NBIF's base index 5 (0x04040000 dwords) is far past BAR5: such a register needs the indirect pair.
    let far = nbif_6_3_1::REGS.iter().find(|r| r.base_idx == 5).expect("NBIF has registers on base 5");
    assert!(far.dword(bases(hw::NBIF)).is_some_and(|d| d >= BAR5_DWORDS) && far.bar5_byte(bases(hw::NBIF)).is_none());
    // A table without the base gives no address, never a guess.
    assert_eq!(mp_14_0_2::MP1_SMN_C2PMSG_66.dword(&[0x16000]), None);
}

#[test]
fn names_and_fields() {
    assert_eq!(by_name("mp_14_0_2", "MPASP_SMN_C2PMSG_81"), Some(mp_14_0_2::MPASP_SMN_C2PMSG_81));
    assert_eq!(by_name("mp_14_0_2", "NOPE"), None);
    assert_eq!(by_name("nope", "MPASP_SMN_C2PMSG_81"), None);
    assert_eq!(BLOCKS.len(), 10);
    // Every name is unique within its block: a lookup cannot hit a neighbour.
    for (block, regs) in BLOCKS {
        let mut names: Vec<&str> = regs.iter().map(|r| r.name).collect();
        names.sort_unstable();
        let n = names.len();
        names.dedup();
        assert_eq!(names.len(), n, "{block}");
    }
    let f = osssys_7_0_0::f::IH_RB_CNTL__RB_SIZE;
    assert_eq!(f.get(0x0000_0022), 0x11);
    assert_eq!(f.put(0xFFFF_FFFF, 0), 0xFFFF_FFC1);
    assert_eq!(f.put(0, 0xFF), 0x3E, "bits past the field are dropped");
    assert_eq!(nbif_6_3_1::cfg::BIF_CFG_DEV0_EPF0_VENDOR_ID.offset, 0);
}

/// GP-GPU2: every register `rdna4_core::regmap` names for the G2 stages (the driver links rdna4-core, not this crate)
/// equals the generated one, name, base index and offset.
#[test]
fn the_g2_registers_of_rdna4_core_equal_the_generated_ones() {
    use rdna4_core::regmap as rm;
    let pairs: [(rm::Reg, Reg); 24] = [
        (rm::PSP_C2PMSG_33, mp_14_0_2::MPASP_SMN_C2PMSG_33),
        (rm::PSP_C2PMSG_35, mp_14_0_2::MPASP_SMN_C2PMSG_35),
        (rm::PSP_C2PMSG_36, mp_14_0_2::MPASP_SMN_C2PMSG_36),
        (rm::PSP_C2PMSG_64, mp_14_0_2::MPASP_SMN_C2PMSG_64),
        (rm::PSP_C2PMSG_67, mp_14_0_2::MPASP_SMN_C2PMSG_67),
        (rm::PSP_C2PMSG_69, mp_14_0_2::MPASP_SMN_C2PMSG_69),
        (rm::PSP_C2PMSG_70, mp_14_0_2::MPASP_SMN_C2PMSG_70),
        (rm::PSP_C2PMSG_71, mp_14_0_2::MPASP_SMN_C2PMSG_71),
        (rm::PSP_C2PMSG_81, mp_14_0_2::MPASP_SMN_C2PMSG_81),
        (rm::PSP_C2PMSG_101, mp_14_0_2::MPASP_SMN_C2PMSG_101),
        (rm::PSP_C2PMSG_102, mp_14_0_2::MPASP_SMN_C2PMSG_102),
        (rm::PSP_C2PMSG_103, mp_14_0_2::MPASP_SMN_C2PMSG_103),
        (rm::SMU_MESSAGE, mp_14_0_2::MP1_SMN_C2PMSG_66),
        (rm::SMU_ARGUMENT, mp_14_0_2::MP1_SMN_C2PMSG_82),
        (rm::SMU_ANSWER, mp_14_0_2::MP1_SMN_C2PMSG_90),
        (rm::SMU_DEBUG_ARGUMENT, mp_14_0_2::MP1_SMN_C2PMSG_53),
        (rm::SMU_DEBUG_ANSWER, mp_14_0_2::MP1_SMN_C2PMSG_54),
        (rm::SMU_DEBUG_MESSAGE, mp_14_0_2::MP1_SMN_C2PMSG_75),
        (rm::RSMU_INDEX, nbif_6_3_1::BIF_BX_PF0_RSMU_INDEX),
        (rm::RSMU_DATA, nbif_6_3_1::BIF_BX_PF0_RSMU_DATA),
        (rm::MMHUB_FB_LOCATION_BASE, mmhub_4_1_0::MMMC_VM_FB_LOCATION_BASE),
        (rm::GC_RLC_BOOTLOAD_STATUS, gc_12_0_0::RLC_RLCS_BOOTLOAD_STATUS),
        (rm::HDP_MEM_FLUSH, nbif_6_3_1::BIF_BX_PF0_HDP_MEM_COHERENCY_FLUSH_CNTL),
        (rm::REMAP_HDP_MEM_FLUSH, nbif_6_3_1::BIF_BX0_REMAP_HDP_MEM_FLUSH_CNTL),
    ];
    for (core, made) in pairs {
        assert_eq!((core.name, core.base_index, core.offset), (made.name, made.base_idx, made.offset), "{}", core.name);
    }
    assert_eq!((rm::MMHUB_FB_LOCATION_TOP.offset, rm::MMHUB_FB_LOCATION_TOP.base_index), (mmhub_4_1_0::MMMC_VM_FB_LOCATION_TOP.offset, mmhub_4_1_0::MMMC_VM_FB_LOCATION_TOP.base_idx));
    assert_eq!((rm::GC_CP_STAT.name, rm::GC_CP_STAT.offset, rm::GC_CP_STAT.base_index), (gc_12_0_0::CP_STAT.name, gc_12_0_0::CP_STAT.offset, gc_12_0_0::CP_STAT.base_idx));
    for (r, _) in rdna4_core::refuse::NEVER_WRITTEN {
        assert_eq!(by_name("mp_14_0_2", r.name).map(|g| (g.base_idx, g.offset)), Some((r.base_index, r.offset)), "{}", r.name);
    }
}
