//! Offset traps from spec/gpu.md §2.4 and werkzeuge/plattform/gpu/doku/90-fallen.md, and the
//! constants V1 used (BitsOS-Plattform/gpu/dcnregs.asm), checked against the generated tables.

use dcn314_regs::*;

const S1: u32 = 0xC0;
const S2: u32 = 0x34C0;

fn byte(name: &str) -> u32 {
    lookup(name).unwrap_or_else(|| panic!("{name} missing")).byte
}

fn field_mask(reg: &str, field: &str) -> u32 {
    let r = lookup(reg).unwrap();
    r.fields.iter().find(|f| f.name == field).unwrap_or_else(|| panic!("{reg}.{field} missing")).mask
}

#[test]
fn segment_bases_are_v1s() {
    // dcnregs.asm:6-7, dcn314_resource.c:105-107.
    assert_eq!(SEG_BASES[1], S1);
    assert_eq!(SEG_BASES[2], S2);
    assert_eq!(SEG_BASES[3], 0x9000);
}

#[test]
fn v1_dcnregs_constants_per_pipe() {
    // dcnregs.asm:10-31: HUBP stride 0xDC dwords, OTG stride 0x80 dwords, DTO 16 bytes, PG 8 bytes.
    assert_eq!(byte("DCN_VM_FB_LOCATION_BASE"), (S2 + 0x475) * 4);
    assert_eq!(byte("DCN_VM_FB_LOCATION_TOP"), (S2 + 0x476) * 4);
    assert_eq!(byte("DMCUB_CNTL"), (S2 + 0x1F6) * 4);
    assert_eq!(field_mask("DMCUB_CNTL", "DMCUB_ENABLE"), 0x10000);
    for p in 0..4u32 {
        let hubp = (S2 + p * 0xDC) * 4;
        assert_eq!(byte(&format!("DOMAIN{p}_PG_STATUS")), (S2 + 0x81) * 4 + 8 * p);
        assert_eq!(byte(&format!("HUBP{p}_DCHUBP_CNTL")), hubp + 0x5F3 * 4);
        let req = |r: &str| byte(&format!("HUBPREQ{p}_{r}"));
        assert_eq!(req("VMID_SETTINGS_0"), hubp + 0x609 * 4);
        assert_eq!(req("DCSURF_PRIMARY_SURFACE_ADDRESS"), hubp + 0x60A * 4);
        assert_eq!(req("DCSURF_PRIMARY_SURFACE_ADDRESS_HIGH"), hubp + 0x60B * 4);
        assert_eq!(req("DCSURF_SURFACE_CONTROL"), hubp + 0x61A * 4);
        assert_eq!(req("DCSURF_FLIP_CONTROL"), hubp + 0x61B * 4);
        assert_eq!(req("DCSURF_FLIP_CONTROL2"), hubp + 0x61C * 4);
        assert_eq!(req("DCSURF_SURFACE_EARLIEST_INUSE"), hubp + 0x625 * 4);
        assert_eq!(req("DCSURF_SURFACE_EARLIEST_INUSE_HIGH"), hubp + 0x626 * 4);
        let otg = (S2 + p * 0x80) * 4;
        assert_eq!(byte(&format!("OTG{p}_OTG_CONTROL")), otg + 0x1B41 * 4);
        assert_eq!(byte(&format!("OTG{p}_OTG_MASTER_UPDATE_LOCK")), otg + 0x1B8A * 4);
        let dto = (S1 + 0x80) * 4 + 16 * p;
        assert_eq!(byte(&format!("OTG{p}_PIXEL_RATE_CNTL")), dto);
        assert_eq!(byte(&format!("DP_DTO{p}_PHASE")), dto + 4);
        assert_eq!(byte(&format!("DP_DTO{p}_MODULO")), dto + 8);
    }
    assert!(lookup("HUBP4_DCHUBP_CNTL").is_none(), "DCN 3.1.4 has four pipes");
}

#[test]
fn v1_flip_masks() {
    // dcnregs.asm:32-38.
    let flip = "HUBPREQ0_DCSURF_FLIP_CONTROL";
    assert_eq!(field_mask(flip, "SURFACE_FLIP_TYPE"), 2);
    assert_eq!(field_mask(flip, "SURFACE_FLIP_PENDING"), 0x100);
    let held = field_mask(flip, "SURFACE_UPDATE_LOCK")
        | field_mask(flip, "SURFACE_FLIP_VUPDATE_SKIP_NUM")
        | field_mask(flip, "HUBPREQ_MASTER_UPDATE_LOCK_STATUS");
    assert_eq!(held, 0x2F1);
    let flip2 = "HUBPREQ0_DCSURF_FLIP_CONTROL2";
    assert_eq!(field_mask(flip2, "SURFACE_GSL_ENABLE") | field_mask(flip2, "SURFACE_TRIPLE_BUFFER_ENABLE"), 0x500);
    let surf = "HUBPREQ0_DCSURF_SURFACE_CONTROL";
    assert_eq!(field_mask(surf, "PRIMARY_SURFACE_TMZ") | field_mask(surf, "PRIMARY_SURFACE_DCC_EN"), 3);
}

#[test]
fn x26_r31_dword_1b8a_is_the_lock_not_global_sync_status() {
    use otg0_otg_master_update_lock as lock;
    assert_eq!(lock::REG.dword(), S2 + 0x1B8A);
    assert_eq!(lock::REG.byte(), 0x14128);
    let at: Vec<&str> = at_byte(0x14128).map(|r| r.name).collect();
    assert_eq!(at, ["OTG0_OTG_MASTER_UPDATE_LOCK"]);
    assert_eq!(otg0_otg_global_sync_status::REG.byte(), 0x14124);
    assert_eq!(lookup("OTG0_OTG_GLOBAL_SYNC_STATUS").unwrap().dword, 0x1B89);
    // X1/X28: lock bit 0, status bit 8 (V1 LOCK_HELD 0x101).
    assert_eq!(lock::OTG_MASTER_UPDATE_LOCK.mask() | lock::UPDATE_LOCK_STATUS.mask(), 0x101);
}

#[test]
fn x8_r4_nbio_rsmu_window_has_no_dcn_register() {
    for b in bar5::NBIO_RSMU_ALIAS {
        assert_eq!(at_byte(b).count(), 0, "{b:#x}");
    }
    assert!(REGISTERS.iter().all(|r| r.seg != 0 && !bar5::NBIO_RSMU_ALIAS.contains(&r.byte)));
    let seg0: Vec<_> = EXCLUDED.iter().filter(|e| e.seg == Some(0)).collect();
    assert!(seg0.iter().any(|e| e.name == "AZCONTROLLER0_CORB_WRITE_POINTER"));
    assert!(seg0.iter().all(|e| e.reason.contains("X8")));
    assert_eq!(bar5_byte(0, 2), None);
    for b in [bar5::PCIE_INDEX2, bar5::PCIE_DATA2, bar5::RSMU_INDEX, bar5::RSMU_DATA] {
        assert_eq!(at_byte(b).count(), 0);
    }
}

#[test]
fn r5_x9_nothing_beyond_the_display_window() {
    assert!(REGISTERS.iter().all(|r| r.byte < DISPLAY_WINDOW_END));
    // The AZ*1 registers carry garbage offsets 0x4B70xx in seg3 (R5).
    let e = excluded("AZCONTROLLER1_CORB_WRITE_POINTER").expect("AZCONTROLLER1 excluded");
    assert_eq!(e.seg, Some(3));
    assert!(e.reason.contains("R5"));
    assert!(lookup("AZCONTROLLER1_CORB_WRITE_POINTER").is_none());
}

#[test]
fn r19_phyasymclk_is_seg2_unlike_the_other_dccg_registers() {
    let r = lookup("PHYASYMCLK_CLOCK_CNTL").unwrap();
    assert_eq!((r.seg, r.dword, r.byte), (2, 0x52, 0xD448));
    assert_eq!(lookup("OTG1_PIXEL_RATE_CNTL").unwrap().seg, 1);
}

#[test]
fn dchubp_cntl_bits_of_the_underflow_clear() {
    // spec 2.4 / X5 / X30: clear only these; `(v & !0x8400_0000) | 0x8000_0000`.
    use hubp0_dchubp_cntl as c;
    assert_eq!(c::REG.byte(), 0xEACC);
    assert_eq!(c::HUBP_UNDERFLOW_CLEAR.mask(), 0x8000_0000);
    assert_eq!(c::HUBP_TIMEOUT_STATUS_CLEAR.mask(), 0x0400_0000);
    assert_eq!(c::HUBP_TTU_DISABLE.mask(), 0x1000);
    assert_eq!(c::HUBP_SOFT_RESET.mask() | c::HUBP_BLANK_EN.mask(), 0x5);
    let gop = c::REG.value(0x300F_1002);
    assert_eq!(gop.get(c::HUBP_TTU_DISABLE), 1);
    assert_eq!(gop.get(c::HUBP_UNDERFLOW_STATUS), 3);
    let cleared = c::HUBP_TIMEOUT_STATUS_CLEAR.clear(gop).with(c::HUBP_UNDERFLOW_CLEAR, 1).unwrap();
    assert_eq!(cleared.raw(), (0x300F_1002 & !0x8400_0000) | 0x8000_0000);
}

#[test]
fn spec_2_2_and_3_offsets() {
    assert_eq!(byte("DSCL0_DSCL_AUTOCAL"), 0x10730);
    assert_eq!(byte("DCHUBBUB_ARB_DRAM_STATE_CNTL"), 0xE6F0);
    assert_eq!(byte("DCHUBBUB_GLOBAL_TIMER_CNTL"), 0xE790);
    assert_eq!(byte("OTG0_OTG_STATUS_FRAME_COUNT"), 0x14030);
    // G-3c R-min (spec 3.2): high before low.
    assert_eq!(byte("HUBP0_DCSURF_PRI_VIEWPORT_DIMENSION"), 0xEAA8);
    assert_eq!(byte("HUBP0_DCSURF_SEC_VIEWPORT_DIMENSION"), 0xEAB8);
    assert_eq!(byte("HUBPREQ0_DCSURF_SURFACE_PITCH"), 0xEB1C);
    assert_eq!(field_mask("HUBPREQ0_DCSURF_SURFACE_PITCH", "PITCH"), 0x3FFF);
    assert_eq!(byte("HUBPREQ0_DCSURF_PRIMARY_SURFACE_ADDRESS_HIGH"), 0xEB2C);
    assert_eq!(byte("HUBPREQ0_DCSURF_PRIMARY_SURFACE_ADDRESS"), 0xEB28);
}

#[test]
fn indirect_registers_are_not_bar5_offsets() {
    let ix = lookup_indirect("AZF0ENDPOINT0_AZALIA_F0_AUDIO_ENABLE_STATUS").unwrap();
    assert_eq!(ix.index, 0x6B);
    assert!(lookup(ix.name).is_none());
    assert_eq!(ix_azf0endpoint0_azalia_f0_audio_enable_status::INDEX.index(), 0x6B);
}

#[test]
fn excluded_and_usable_do_not_overlap() {
    for e in EXCLUDED {
        assert!(lookup(e.name).is_none(), "{}", e.name);
    }
}

#[test]
fn r3_seg0_base_is_dmubs_and_unused() {
    // dmub_dcn314.c:33-38 adds SEG0 0x12; DC itself never uses seg0 (R3, R4).
    assert_eq!(SEG_BASES[0], 0x12);
    assert_eq!(bar5_byte(0, 0), None);
}

#[test]
fn r12_instance_strides() {
    // 90-fallen R12: strides in dwords between instance 0 and 1 of the same register.
    let stride = |a: &str, b: &str| (byte(b) - byte(a)) / 4;
    assert_eq!(stride("HUBP0_DCHUBP_CNTL", "HUBP1_DCHUBP_CNTL"), 0xDC);
    assert_eq!(stride("DSCL0_DSCL_AUTOCAL", "DSCL1_DSCL_AUTOCAL"), 0x16B);
    assert_eq!(stride("OTG0_OTG_CONTROL", "OTG1_OTG_CONTROL"), 0x80);
    assert_eq!(stride("FMT0_FMT_CONTROL", "FMT1_FMT_CONTROL"), 0x5A);
    assert_eq!(stride("ODM0_OPTC_DATA_SOURCE_SELECT", "ODM1_OPTC_DATA_SOURCE_SELECT"), 0x10);
    assert_eq!(stride("DIG0_DIG_FE_CNTL", "DIG1_DIG_FE_CNTL"), 0x100);
    assert_eq!(stride("DP_AUX0_AUX_CONTROL", "DP_AUX1_AUX_CONTROL"), 0x1C);
    assert_eq!(stride("HPD0_DC_HPD_CONTROL", "HPD1_DC_HPD_CONTROL"), 8);
    assert_eq!(stride("MPCC0_MPCC_CONTROL", "MPCC1_MPCC_CONTROL"), 0x20);
    assert_eq!(lookup("MPCC0_MPCC_CONTROL").unwrap().seg, 3);
    assert_eq!(stride("OTG0_PIXEL_RATE_CNTL", "OTG1_PIXEL_RATE_CNTL"), 4);
    assert_eq!(stride("DOMAIN0_PG_STATUS", "DOMAIN1_PG_STATUS"), 2);
}

#[test]
fn r2_6_r32_absolute_dwords_match_windows_tables() {
    // R2-6 / R32: OTG_H_TOTAL abs 0x4FEA, PRIMARY_SURFACE_ADDRESS abs 0x3ACA; BAR5 = dword * 4.
    assert_eq!(otg0_otg_h_total::REG.dword(), 0x4FEA);
    assert_eq!(hubpreq0_dcsurf_primary_surface_address::REG.dword(), 0x3ACA);
    // DCN seg0 holds exactly 25 header registers; all of them stay excluded (R2-6, R4).
    assert_eq!(EXCLUDED.iter().filter(|e| e.seg == Some(0)).count(), 25);
}

#[test]
fn r2_19_cursor_offsets() {
    // R2-19: HUBP0 cursor 0x3B1C/38/39/3A/3B/3C/3D/3F, DPP0 0x41B1..0x41B4, MPC 0x9513 (absolute dwords).
    let at = |abs: u32| -> Vec<&'static str> { at_byte(abs * 4).map(|r| r.name).collect() };
    assert_eq!(at(0x3B1C), ["HUBPREQ0_CURSOR_SETTINGS"]);
    assert_eq!(at(0x3B38), ["CURSOR0_0_CURSOR_CONTROL"]);
    assert_eq!(at(0x3B39), ["CURSOR0_0_CURSOR_SURFACE_ADDRESS"]);
    assert_eq!(at(0x3B3A), ["CURSOR0_0_CURSOR_SURFACE_ADDRESS_HIGH"]);
    assert_eq!(at(0x3B3B), ["CURSOR0_0_CURSOR_SIZE"]);
    assert_eq!(at(0x3B3C), ["CURSOR0_0_CURSOR_POSITION"]);
    assert_eq!(at(0x3B3D), ["CURSOR0_0_CURSOR_HOT_SPOT"]);
    assert_eq!(at(0x3B3F), ["CURSOR0_0_CURSOR_DST_OFFSET"]);
    assert_eq!(at(0x41B1), ["CNVC_CUR0_CURSOR0_CONTROL"]);
    assert_eq!(at(0x41B2), ["CNVC_CUR0_CURSOR0_COLOR0"]);
    assert_eq!(at(0x41B3), ["CNVC_CUR0_CURSOR0_COLOR1"]);
    assert_eq!(at(0x41B4), ["CNVC_CUR0_CURSOR0_FP_SCALE_BIAS"]);
    assert_eq!(at(0x9513), ["CUR_VUPDATE_LOCK_SET0"]);
}

#[test]
fn r31_absolute_dwords() {
    assert_eq!(otg0_otg_global_sync_status::REG.dword(), 0x5049);
    assert_eq!(otg0_otg_master_update_lock::REG.dword(), 0x504A);
}

#[test]
fn r7_r29_x9_display_registers_are_direct_mmio() {
    // Everything DC touches lies below 512 KB of BAR5 and is reached directly, never via PCIE_INDEX2.
    assert_eq!(DISPLAY_WINDOW_END, 0x80000);
    for b in [bar5::DC_SMU_MSG, bar5::DC_SMU_ARG, bar5::DC_SMU_RESP, bar5::MP0_C2PMSG_33, 0x6C92C] {
        assert!(b < DISPLAY_WINDOW_END, "{b:#x}");
    }
    assert!(REGISTERS.iter().all(|r| r.byte >= 0x78 && r.byte < DISPLAY_WINDOW_END));
}

#[test]
fn r6_r22_x25_mp_base_pairings() {
    // Right pairings: mp_13_0_4 idx1 + Phoenix list, mp_13_0_5 idx0 + DC's copy; both seg = 0x16000.
    const PHOENIX_MP: [u32; 2] = [0x4000, 0x16000];
    const DC_COPY_MP: [u32; 2] = [0x16000, 0x0243_FC00];
    let resp = 0x240 + 91;
    assert_eq!((PHOENIX_MP[1] + resp) * 4, bar5::DC_SMU_RESP);
    assert_eq!((DC_COPY_MP[0] + resp) * 4, bar5::DC_SMU_RESP);
    assert_eq!(bar5::MP_SEG1_PHOENIX, PHOENIX_MP[1]);
    // Wrong mixes: 0x4000 + off is a valid but different BAR5 address; the other lands far beyond BAR5.
    assert_ne!((PHOENIX_MP[0] + resp) * 4, bar5::DC_SMU_RESP);
    assert!((PHOENIX_MP[0] + resp) * 4 < DISPLAY_WINDOW_END);
    assert!((DC_COPY_MP[1] as u64 + resp as u64) * 4 >= DISPLAY_WINDOW_END as u64);
    // The DC and PMFW mailboxes never share an address (X15).
    let dc = [bar5::DC_SMU_MSG, bar5::DC_SMU_ARG, bar5::DC_SMU_RESP];
    assert!([bar5::PMFW_MSG, bar5::PMFW_ARG, bar5::PMFW_RESP].iter().all(|p| !dc.contains(p)));
}
