use crate::ipfacts::{TABLE, hw};
use crate::regmap::*;

fn byte(reg: Reg) -> u32 {
    match reach(&TABLE, reg).unwrap() {
        Reach::Bar5(byte) => byte,
        other => panic!("{}: {other:?}", reg.name),
    }
}

#[test]
fn the_psp_registers_the_note_names() {
    assert_eq!(byte(PSP_C2PMSG_64), 0x58200);
    assert_eq!(dword(&TABLE, PSP_C2PMSG_33), Some(0x16061));
    assert_eq!(dword(&TABLE, PSP_C2PMSG_35), Some(0x16063));
    assert_eq!(dword(&TABLE, PSP_C2PMSG_36), Some(0x16064));
    assert_eq!(dword(&TABLE, PSP_C2PMSG_69), Some(0x16085));
    assert_eq!(dword(&TABLE, PSP_C2PMSG_70), Some(0x16086));
    assert_eq!(dword(&TABLE, PSP_C2PMSG_71), Some(0x16087));
    assert_eq!(dword(&TABLE, PSP_C2PMSG_81), Some(0x16091));
}

/// `mmMP0_SMN_C2PMSG_33` and `mmRCC_CONFIG_MEMSIZE`: Linux's own fixed numbers for the moment before it has a table
/// (A:amdgpu_discovery.c:142-143). The table must lead to the same place.
#[test]
fn the_table_reaches_the_numbers_linux_hardcodes_before_it_has_a_table() {
    assert_eq!(dword(&TABLE, PSP_C2PMSG_33), Some(0x16061));
    assert_eq!(dword(&TABLE, MEMSIZE), Some(0xDE3));
}

#[test]
fn the_smu_mailbox_is_on_base_index_1() {
    assert_eq!(dword(&TABLE, SMU_MESSAGE), Some(0x16282));
    assert_eq!(dword(&TABLE, SMU_ARGUMENT), Some(0x16292));
    assert_eq!(dword(&TABLE, SMU_ANSWER), Some(0x1629A));
    assert_eq!(byte(SMU_MESSAGE), 0x58A08);
}

#[test]
fn memsize_is_the_dword_the_780m_driver_also_reads() {
    assert_eq!(byte(MEMSIZE), 0xDE3 * 4);
}

#[test]
fn bar5_ends_at_0x20000_dwords() {
    assert_eq!(BAR5_BYTES / 4, 0x20000);
    assert_eq!(reach_of(0x1FFFF), Reach::Bar5(0x7FFFC));
    assert_eq!(reach_of(0x20000), Reach::NeedsIndirect(0x20000));
}

#[test]
fn a_high_base_index_says_it_needs_the_indirect_window() {
    let gc3 = Reg { name: "a GC register on base index 3", hw_id: hw::GC, base_index: 3, offset: 0x10 };
    assert_eq!(reach(&TABLE, gc3), Some(Reach::NeedsIndirect(0x0240_2C10)));
    let gc1 = Reg { name: "a GC register on base index 1", hw_id: hw::GC, base_index: 1, offset: 0x10 };
    assert_eq!(reach(&TABLE, gc1), Some(Reach::Bar5(0xA010 * 4)));
}

#[test]
fn what_the_table_does_not_have_has_no_address() {
    let no_ip = Reg { name: "no such IP", hw_id: 9999, base_index: 0, offset: 0 };
    assert_eq!(dword(&TABLE, no_ip), None);
    let no_base = Reg { name: "no such base index", hw_id: hw::OSSSYS, base_index: 2, offset: 0 };
    assert_eq!(dword(&TABLE, no_base), None);
    let no_bases = Reg { name: "an IP with no bases", hw_id: hw::PWR, base_index: 0, offset: 0 };
    assert_eq!(reach(&TABLE, no_bases), None);
    let over = Reg { name: "offset past 32 bits", hw_id: hw::GC, base_index: 3, offset: u32::MAX };
    assert_eq!(dword(&TABLE, over), None);
}

/// The registers G2 adds, at the places Linux's headers give them on this card's bases.
#[test]
fn the_g2_registers_lie_in_bar5() {
    assert_eq!(byte(PSP_C2PMSG_67), 0x5820C);
    assert_eq!(byte(RSMU_INDEX), 0x50);
    assert_eq!(byte(RSMU_DATA), 0x54);
    assert_eq!(byte(MMHUB_FB_LOCATION_BASE), 0x69550);
    assert_eq!(byte(MMHUB_FB_LOCATION_TOP), 0x69554);
    assert_eq!(byte(GC_CP_STAT), 0x8680);
    assert_eq!(byte(GC_RLC_BOOTLOAD_STATUS), 0x3B9F0);
    assert_eq!(byte(SMU_DEBUG_MESSAGE), (0x16200 + 0x8B) * 4);
    assert_eq!(SMN_MP1_FIRMWARE_FLAGS, 0x03B1_0024);
    assert_eq!(byte(HDP_MEM_FLUSH), 0x385C);
    // The remap register (the trace's dword 0xe4d) and the place it points the flush at (dword 0x1fc00, the last
    // page of BAR5: Linux's fixed number, which this card's NBIF base 0 also gives).
    assert_eq!(byte(REMAP_HDP_MEM_FLUSH), 0xE4D * 4);
    assert_eq!((HDP_FLUSH_REMAPPED_BYTE, byte(HDP_FLUSH_REMAPPED)), (0x1FC00 * 4, 0x7F000));
    assert_eq!(HDP_FLUSH_REMAPPED_BYTE, BAR5_BYTES - 0x1000);
    assert_eq!(byte(PSP_C2PMSG_36), 0x16064 * 4);
    assert_eq!(byte(MEMSIZE), 0x378C);
}
