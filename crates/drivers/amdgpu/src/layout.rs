//! Where the attach gates and the snapshot look: BAR5 byte offsets of the per-pipe, per-OTG and per-DIG registers,
//! taken from the generated DCN 3.1.4 tables (`dcn314-regs`) as instance 0 plus the instance stride. The strides
//! are checked against every generated instance in the host tests (`tests::layout`), so a V1 hex literal can not
//! drift from the header. V1's constants: `P/gpu/dcnregs.asm:1-36`, spec/gpu.md §3.1.

use dcn314_regs as r;

/// HUBP instances (pipes) and OTG instances of DCN 3.1.4: a hardware fact of this IP version (four of each,
/// `dcn314_resource.c` pipe count), not a limit on anything V2 counts.
pub const PIPES: u32 = 4;
pub const OTGS: u32 = 4;
/// DIG / DP stream encoders of DCN 3.1.4 (DIG0..DIG4 in the header).
pub const DIGS: u32 = 5;

pub const HUBP_STRIDE: u32 = r::hubp1_dchubp_cntl::REG.byte() - r::hubp0_dchubp_cntl::REG.byte();
pub const OTG_STRIDE: u32 = r::otg1_otg_control::REG.byte() - r::otg0_otg_control::REG.byte();
pub const DTO_STRIDE: u32 = r::otg1_pixel_rate_cntl::REG.byte() - r::otg0_pixel_rate_cntl::REG.byte();
pub const DIG_STRIDE: u32 = r::dig1_dig_fe_cntl::REG.byte() - r::dig0_dig_fe_cntl::REG.byte();
pub const DP_STRIDE: u32 = r::dp1_dp_link_cntl::REG.byte() - r::dp0_dp_link_cntl::REG.byte();
/// DOMAINn_PG_STATUS sit two dwords apart (V1 `PG_STATUS + n * 8`).
pub const PG_STRIDE: u32 = r::domain1_pg_status::REG.byte() - r::domain0_pg_status::REG.byte();

/// HUBP/HUBPREQ register of pipe `p`, given as its pipe-0 byte offset.
pub const fn hubp(p: u32, reg0: u32) -> u32 {
    reg0 + p * HUBP_STRIDE
}
/// OTG register of timing generator `k`, given as its OTG0 byte offset.
pub const fn otg(k: u32, reg0: u32) -> u32 {
    reg0 + k * OTG_STRIDE
}
/// OTGk_PIXEL_RATE_CNTL, DP_DTOk_PHASE, DP_DTOk_MODULO (DCCG, segment 1).
pub const fn dto_cntl(k: u32) -> u32 {
    r::otg0_pixel_rate_cntl::REG.byte() + k * DTO_STRIDE
}
pub const fn dto_phase(k: u32) -> u32 {
    r::dp_dto0_phase::REG.byte() + k * DTO_STRIDE
}
pub const fn dto_modulo(k: u32) -> u32 {
    r::dp_dto0_modulo::REG.byte() + k * DTO_STRIDE
}
pub const fn dig(d: u32, reg0: u32) -> u32 {
    reg0 + d * DIG_STRIDE
}
pub const fn dp(d: u32, reg0: u32) -> u32 {
    reg0 + d * DP_STRIDE
}
pub const fn pg_status(domain: u32) -> u32 {
    r::domain0_pg_status::REG.byte() + domain * PG_STRIDE
}

// Pipe-0 offsets of the registers the gates read.
pub const HUBP_CNTL: u32 = r::hubp0_dchubp_cntl::REG.byte();
pub const HUBP_VMID: u32 = r::hubpreq0_vmid_settings_0::REG.byte();
pub const HUBP_ADDR: u32 = r::hubpreq0_dcsurf_primary_surface_address::REG.byte();
pub const HUBP_ADDR_HI: u32 = r::hubpreq0_dcsurf_primary_surface_address_high::REG.byte();
pub const HUBP_SURF: u32 = r::hubpreq0_dcsurf_surface_control::REG.byte();
pub const HUBP_FLIP: u32 = r::hubpreq0_dcsurf_flip_control::REG.byte();
pub const HUBP_FLIP2: u32 = r::hubpreq0_dcsurf_flip_control2::REG.byte();
pub const HUBP_INUSE: u32 = r::hubpreq0_dcsurf_surface_earliest_inuse::REG.byte();
pub const HUBP_INUSE_HI: u32 = r::hubpreq0_dcsurf_surface_earliest_inuse_high::REG.byte();
pub const HUBP_VIEWPORT: u32 = r::hubp0_dcsurf_pri_viewport_dimension::REG.byte();
pub const HUBP_SURFACE_CONFIG: u32 = r::hubp0_dcsurf_surface_config::REG.byte();
pub const HUBP_PITCH: u32 = r::hubpreq0_dcsurf_surface_pitch::REG.byte();
pub const OTG_CONTROL: u32 = r::otg0_otg_control::REG.byte();
pub const OTG_LOCK: u32 = r::otg0_otg_master_update_lock::REG.byte();
pub const OTG_H_TOTAL: u32 = r::otg0_otg_h_total::REG.byte();
pub const OTG_V_TOTAL: u32 = r::otg0_otg_v_total::REG.byte();
pub const OTG_H_BLANK: u32 = r::otg0_otg_h_blank_start_end::REG.byte();
pub const OTG_V_BLANK: u32 = r::otg0_otg_v_blank_start_end::REG.byte();
// GX-3: flips, vblank timing, CRC (linux-6.6 dc/dcn10/dcn10_optc.c:655 frame count, :692 position, :1456 CRC;
// dc/dcn30/dcn30_optc.c:119 optc3_lock).
pub const OTG_POSITION: u32 = r::otg0_otg_status_position::REG.byte();
pub const OTG_FRAME_COUNT: u32 = r::otg0_otg_status_frame_count::REG.byte();
pub const OTG_GLOBAL_CONTROL2: u32 = r::otg0_otg_global_control2::REG.byte();
pub const OTG_CRC_CNTL: u32 = r::otg0_otg_crc_cntl::REG.byte();
pub const OTG_CRC0_WINDOWA_X: u32 = r::otg0_otg_crc0_windowa_x_control::REG.byte();
pub const OTG_CRC0_WINDOWA_Y: u32 = r::otg0_otg_crc0_windowa_y_control::REG.byte();
pub const OTG_CRC0_WINDOWB_X: u32 = r::otg0_otg_crc0_windowb_x_control::REG.byte();
pub const OTG_CRC0_WINDOWB_Y: u32 = r::otg0_otg_crc0_windowb_y_control::REG.byte();
pub const OTG_CRC0_DATA_RG: u32 = r::otg0_otg_crc0_data_rg::REG.byte();
pub const OTG_CRC0_DATA_B: u32 = r::otg0_otg_crc0_data_b::REG.byte();
// GX-4: the mode switch (scaled, G-3c R-min): viewports, the DPP scaler, the pipe update status, underflow.
/// DPP (DSCL) instance stride: DPP n sits behind HUBP n on DCN 3.1.4 (one plane per pipe).
pub const DPP_STRIDE: u32 = r::dscl1_scl_mode::REG.byte() - r::dscl0_scl_mode::REG.byte();
/// DSCL register of DPP `p`, given as its DPP-0 byte offset.
pub const fn dpp(p: u32, reg0: u32) -> u32 {
    reg0 + p * DPP_STRIDE
}
pub const HUBP_VIEWPORT_START: u32 = r::hubp0_dcsurf_pri_viewport_start::REG.byte();
pub const HUBP_SEC_VIEWPORT: u32 = r::hubp0_dcsurf_sec_viewport_dimension::REG.byte();
pub const HUBP_SEC_VIEWPORT_START: u32 = r::hubp0_dcsurf_sec_viewport_start::REG.byte();
pub const DSCL_AUTOCAL: u32 = r::dscl0_dscl_autocal::REG.byte();
pub const DSCL_CONTROL: u32 = r::dscl0_dscl_control::REG.byte();
pub const SCL_MODE: u32 = r::dscl0_scl_mode::REG.byte();
pub const LB_MEMORY_CTRL: u32 = r::dscl0_lb_memory_ctrl::REG.byte();
pub const LB_DATA_FORMAT: u32 = r::dscl0_lb_data_format::REG.byte();
pub const RECOUT_SIZE: u32 = r::dscl0_recout_size::REG.byte();
pub const MPC_SIZE: u32 = r::dscl0_mpc_size::REG.byte();
pub const DSCL_UPDATE: u32 = r::dscl0_dscl_update::REG.byte();
pub const DSCL_MEM_PWR_STATUS: u32 = r::dscl0_dscl_mem_pwr_status::REG.byte();
pub const OTG_PIPE_UPDATE_STATUS: u32 = r::otg0_otg_pipe_update_status::REG.byte();
/// PRI/SEC_VIEWPORT_WIDTH [13:0] and HEIGHT [29:16]; RECOUT and MPC_SIZE have the same layout.
pub const VIEWPORT_DIM: u32 = r::hubp0_dcsurf_pri_viewport_dimension::PRI_VIEWPORT_WIDTH.mask()
    | r::hubp0_dcsurf_pri_viewport_dimension::PRI_VIEWPORT_HEIGHT.mask();
pub const AUTOCAL_MODE: u32 = r::dscl0_dscl_autocal::AUTOCAL_MODE.mask();
/// AUTOCAL_MODE, AUTOCAL_NUM_PIPE, AUTOCAL_PIPE_ID.
pub const AUTOCAL_FIELDS: u32 = AUTOCAL_MODE
    | r::dscl0_dscl_autocal::AUTOCAL_NUM_PIPE.mask()
    | r::dscl0_dscl_autocal::AUTOCAL_PIPE_ID.mask();
pub const DSCL_MODE: u32 = r::dscl0_scl_mode::DSCL_MODE.mask();
pub const SCL_BOUNDARY_MODE: u32 = r::dscl0_dscl_control::SCL_BOUNDARY_MODE.mask();
pub const SCL_UPDATE_PENDING: u32 = r::dscl0_dscl_update::SCL_UPDATE_PENDING.mask();
pub const LUT_MEM_PWR_STATE: u32 = r::dscl0_dscl_mem_pwr_status::LUT_MEM_PWR_STATE.mask();
/// GX-4b: the manual scaler's ratios, initial phases and taps (DPP-0 offset, bits held), in the order of
/// linux-6.6/drivers/gpu/drm/amd/display/dc/dcn10/dcn10_dpp_dscl.c:510-560 (dpp1_dscl_set_manual_ratio_init)
/// and :684 (SCL_TAP_CONTROL). Index 0 and 1 are the luma H and V ratios.
pub const SCL_FILTER: [(u32, u32); 11] = {
    use r::{
        dscl0_scl_horz_filter_init as hi, dscl0_scl_horz_filter_init_c as hic,
        dscl0_scl_horz_filter_scale_ratio as hr, dscl0_scl_horz_filter_scale_ratio_c as hrc,
        dscl0_scl_tap_control as tap, dscl0_scl_vert_filter_init as vi, dscl0_scl_vert_filter_init_bot as vib,
        dscl0_scl_vert_filter_init_bot_c as vibc, dscl0_scl_vert_filter_init_c as vic,
        dscl0_scl_vert_filter_scale_ratio as vr, dscl0_scl_vert_filter_scale_ratio_c as vrc,
    };
    [
        (hr::REG.byte(), hr::SCL_H_SCALE_RATIO.mask()),
        (vr::REG.byte(), vr::SCL_V_SCALE_RATIO.mask()),
        (hrc::REG.byte(), hrc::SCL_H_SCALE_RATIO_C.mask()),
        (vrc::REG.byte(), vrc::SCL_V_SCALE_RATIO_C.mask()),
        (hi::REG.byte(), hi::SCL_H_INIT_FRAC.mask() | hi::SCL_H_INIT_INT.mask()),
        (hic::REG.byte(), hic::SCL_H_INIT_FRAC_C.mask() | hic::SCL_H_INIT_INT_C.mask()),
        (vi::REG.byte(), vi::SCL_V_INIT_FRAC.mask() | vi::SCL_V_INIT_INT.mask()),
        (vib::REG.byte(), vib::SCL_V_INIT_FRAC_BOT.mask() | vib::SCL_V_INIT_INT_BOT.mask()),
        (vic::REG.byte(), vic::SCL_V_INIT_FRAC_C.mask() | vic::SCL_V_INIT_INT_C.mask()),
        (vibc::REG.byte(), vibc::SCL_V_INIT_FRAC_BOT_C.mask() | vibc::SCL_V_INIT_INT_BOT_C.mask()),
        (
            tap::REG.byte(),
            tap::SCL_V_NUM_TAPS.mask() | tap::SCL_H_NUM_TAPS.mask() | tap::SCL_V_NUM_TAPS_C.mask() | tap::SCL_H_NUM_TAPS_C.mask(),
        ),
    ]
};
/// A scale ratio of 1.0 as the ratio fields hold it (u3.19 shifted left by 5, dcn10_dpp_dscl.c:516).
pub const SCALE_ONE: u32 = 1 << 24;
pub const OTG_DC_REG_UPDATE_PENDING: u32 = r::otg0_otg_pipe_update_status::OTG_DC_REG_UPDATE_PENDING.mask();
pub const HUBP_UNDERFLOW_STATUS: u32 = r::hubp0_dchubp_cntl::HUBP_UNDERFLOW_STATUS.mask();
pub const DIG_FE_CNTL: u32 = r::dig0_dig_fe_cntl::REG.byte();
pub const DIG_BE_CNTL: u32 = r::dig0_dig_be_cntl::REG.byte();
pub const DIG_BE_EN_CNTL: u32 = r::dig0_dig_be_en_cntl::REG.byte();
pub const DP_LINK_CNTL: u32 = r::dp0_dp_link_cntl::REG.byte();
pub const DP_VID_STREAM_CNTL: u32 = r::dp0_dp_vid_stream_cntl::REG.byte();
pub const FB_BASE: u32 = r::dcn_vm_fb_location_base::REG.byte();
pub const FB_TOP: u32 = r::dcn_vm_fb_location_top::REG.byte();
pub const DMCUB_CNTL: u32 = r::dmcub_cntl::REG.byte();
pub const MEMSIZE: u32 = r::bar5::RCC_CONFIG_MEMSIZE;
pub const APER_SIZE: u32 = r::bar5::RCC_CONFIG_APER_SIZE;

// Masks (V1 `dcnregs.asm:27-35`; sh_mask.h names in the generated fields).
pub const BLANKED: u32 = r::hubp0_dchubp_cntl::HUBP_BLANK_EN.mask();
pub const FLIP_TYPE: u32 = r::hubpreq0_dcsurf_flip_control::SURFACE_FLIP_TYPE.mask();
pub const FLIP_PENDING: u32 = r::hubpreq0_dcsurf_flip_control::SURFACE_FLIP_PENDING.mask();
/// SURFACE_UPDATE_LOCK, VUPDATE_SKIP_NUM, HUBPREQ_MASTER_UPDATE_LOCK_STATUS.
pub const FLIP_HELD: u32 = 0x2F1;
/// Swap lock group (GSL) and triple buffering: a write over a pending flip would queue.
pub const FLIP2_HELD: u32 = 0x500;
/// OTG_MASTER_UPDATE_LOCK and UPDATE_LOCK_STATUS.
pub const LOCK_HELD: u32 = 0x101;
pub const LOCK: u32 = r::otg0_otg_master_update_lock::OTG_MASTER_UPDATE_LOCK.mask();
pub const LOCK_STATUS: u32 = r::otg0_otg_master_update_lock::UPDATE_LOCK_STATUS.mask();
pub const LOCK_SEL: u32 = r::otg0_otg_global_control2::OTG_MASTER_UPDATE_LOCK_SEL.mask();
pub const LOCK_SEL_SHIFT: u32 = r::otg0_otg_global_control2::OTG_MASTER_UPDATE_LOCK_SEL.shift();
pub const VERT_COUNT: u32 = r::otg0_otg_status_position::OTG_VERT_COUNT.mask();
pub const FRAME_COUNT: u32 = r::otg0_otg_status_frame_count::OTG_FRAME_COUNT.mask();
pub const V_BLANK_START: u32 = r::otg0_otg_v_blank_start_end::OTG_V_BLANK_START.mask();
pub const V_BLANK_START_SHIFT: u32 = r::otg0_otg_v_blank_start_end::OTG_V_BLANK_START.shift();
pub const CRC_EN: u32 = r::otg0_otg_crc_cntl::OTG_CRC_EN.mask();
pub const CRC_CONT_EN: u32 = r::otg0_otg_crc_cntl::OTG_CRC_CONT_EN.mask();
/// HUBPREQ SURFACE_EARLIEST_INUSE_ADDRESS_HIGH and PRIMARY_SURFACE_ADDRESS_HIGH: bits [15:0].
pub const ADDR_HI: u32 = 0xFFFF;
/// TMZ or DCC.
pub const SURF_ODD: u32 = 3;
pub const DMCUB_ENABLE: u32 = r::dmcub_cntl::DMCUB_ENABLE.mask();
/// DP_DTO enable and its status bit (sh_mask.h DP_DTO0_ENABLE, DPDTO0_ENABLE_STATUS).
pub const DTO_ON: u32 = 0x90;
/// DP DTO modulo = dprefclk 600 MHz as Linux programs it on DCN 3.1.4 (`dcn314_clk_mgr.c`): the signature.
pub const DTO_MODULO: u32 = 600_000_000;
/// DOMAIN_PGFSM_PWR_STATUS: 0 = powered.
pub const PG_GATED: u32 = r::domain0_pg_status::DOMAIN_PGFSM_PWR_STATUS.mask();
pub const OTG_MASTER_EN: u32 = 1;

/// Power domains pipe 0..3 may need (V1 `dcn_domains: db 3,14,4,8`, dcnpipes.asm:91): Linux maps HUBP n to
/// DOMAIN n, its DCN 3.1.4 comments to 2n and 2n+1; V1 checks the union.
pub const PIPE_DOMAINS: [u8; PIPES as usize] = [0b0011, 0b1110, 0b0100, 0b1000];

/// The DMCUB's memory windows (V1 `dcn_windows`, dcndmcub.asm:58-71): name, TOP, BASE (cache windows only),
/// OFFSET (OFFSET_HIGH is the next dword). REGION3_CW0..7, then REGION0..2 and 4..7 (REGION3 is the cache
/// windows' region).
pub struct DmcubWindow {
    pub name: &'static str,
    pub top: u32,
    pub base: Option<u32>,
    pub offset: u32,
    pub offset_high: u32,
}

macro_rules! cw {
    ($n:literal, $t:ident, $b:ident, $o:ident, $h:ident) => {
        DmcubWindow {
            name: concat!("REGION3_CW", $n),
            top: r::$t::REG.byte(),
            base: Some(r::$b::REG.byte()),
            offset: r::$o::REG.byte(),
            offset_high: r::$h::REG.byte(),
        }
    };
}
macro_rules! region {
    ($n:literal, $t:ident, $o:ident, $h:ident) => {
        DmcubWindow {
            name: concat!("REGION", $n),
            top: r::$t::REG.byte(),
            base: None,
            offset: r::$o::REG.byte(),
            offset_high: r::$h::REG.byte(),
        }
    };
}

pub const DMCUB_WINDOWS: &[DmcubWindow] = &[
    cw!(
        "0",
        dmcub_region3_cw0_top_address,
        dmcub_region3_cw0_base_address,
        dmcub_region3_cw0_offset,
        dmcub_region3_cw0_offset_high
    ),
    cw!(
        "1",
        dmcub_region3_cw1_top_address,
        dmcub_region3_cw1_base_address,
        dmcub_region3_cw1_offset,
        dmcub_region3_cw1_offset_high
    ),
    cw!(
        "2",
        dmcub_region3_cw2_top_address,
        dmcub_region3_cw2_base_address,
        dmcub_region3_cw2_offset,
        dmcub_region3_cw2_offset_high
    ),
    cw!(
        "3",
        dmcub_region3_cw3_top_address,
        dmcub_region3_cw3_base_address,
        dmcub_region3_cw3_offset,
        dmcub_region3_cw3_offset_high
    ),
    cw!(
        "4",
        dmcub_region3_cw4_top_address,
        dmcub_region3_cw4_base_address,
        dmcub_region3_cw4_offset,
        dmcub_region3_cw4_offset_high
    ),
    cw!(
        "5",
        dmcub_region3_cw5_top_address,
        dmcub_region3_cw5_base_address,
        dmcub_region3_cw5_offset,
        dmcub_region3_cw5_offset_high
    ),
    cw!(
        "6",
        dmcub_region3_cw6_top_address,
        dmcub_region3_cw6_base_address,
        dmcub_region3_cw6_offset,
        dmcub_region3_cw6_offset_high
    ),
    cw!(
        "7",
        dmcub_region3_cw7_top_address,
        dmcub_region3_cw7_base_address,
        dmcub_region3_cw7_offset,
        dmcub_region3_cw7_offset_high
    ),
    region!("0", dmcub_region0_top_address, dmcub_region0_offset, dmcub_region0_offset_high),
    region!("1", dmcub_region1_top_address, dmcub_region1_offset, dmcub_region1_offset_high),
    region!("2", dmcub_region2_top_address, dmcub_region2_offset, dmcub_region2_offset_high),
    region!("4", dmcub_region4_top_address, dmcub_region4_offset, dmcub_region4_offset_high),
    region!("5", dmcub_region5_top_address, dmcub_region5_offset, dmcub_region5_offset_high),
    region!("6", dmcub_region6_top_address, dmcub_region6_offset, dmcub_region6_offset_high),
    region!("7", dmcub_region7_top_address, dmcub_region7_offset, dmcub_region7_offset_high),
];
/// TOP bit 31: the window is on; [28:0] its TOP (V1 masks 0x1FFFFFFF).
pub const WINDOW_ON: u32 = 1 << 31;
pub const WINDOW_TOP: u32 = 0x1FFF_FFFF;

// GX-5a: the hardware cursor (linux-6.6 dc/dcn20/dcn20_hubp.c:589 hubp2_cursor_set_attributes, :970
// hubp2_cursor_set_position; dc/dcn30/dcn30_dpp.c:351 dpp3_set_cursor_attributes; dc/dcn10/dcn10_dpp.c:431
// dpp1_set_cursor_position; dc/dcn10/dcn10_mpc.c:476 mpc1_cursor_lock). The CURSOR0_n block sits in the HUBP
// stride, CNVC_CURn in the DPP stride (tests::layout checks both against every instance).
pub const CUR_CONTROL: u32 = r::cursor0_0_cursor_control::REG.byte();
pub const CUR_ADDR: u32 = r::cursor0_0_cursor_surface_address::REG.byte();
pub const CUR_ADDR_HI: u32 = r::cursor0_0_cursor_surface_address_high::REG.byte();
pub const CUR_SIZE: u32 = r::cursor0_0_cursor_size::REG.byte();
pub const CUR_POSITION: u32 = r::cursor0_0_cursor_position::REG.byte();
pub const CUR_HOT_SPOT: u32 = r::cursor0_0_cursor_hot_spot::REG.byte();
pub const CUR_DST_OFFSET: u32 = r::cursor0_0_cursor_dst_offset::REG.byte();
pub const CUR_SETTINGS: u32 = r::hubpreq0_cursor_settings::REG.byte();
pub const REF_FREQ_TO_PIX_FREQ: u32 = r::hubpreq0_ref_freq_to_pix_freq::REG.byte();
pub const CNVC_CUR_CONTROL: u32 = r::cnvc_cur0_cursor0_control::REG.byte();
pub const CNVC_CUR_FP_SCALE_BIAS: u32 = r::cnvc_cur0_cursor0_fp_scale_bias::REG.byte();
pub const CUR_ENABLE: u32 = r::cursor0_0_cursor_control::CURSOR_ENABLE.mask();
pub const CUR_2X_MAGNIFY: u32 = r::cursor0_0_cursor_control::CURSOR_2X_MAGNIFY.mask();
pub const CUR_MODE: u32 = r::cursor0_0_cursor_control::CURSOR_MODE.mask();
pub const CUR_MODE_SHIFT: u32 = 8;
pub const CUR_PITCH: u32 = r::cursor0_0_cursor_control::CURSOR_PITCH.mask();
pub const CUR_PITCH_SHIFT: u32 = 16;
pub const CUR_LPC: u32 = r::cursor0_0_cursor_control::CURSOR_LINES_PER_CHUNK.mask();
pub const CUR_LPC_SHIFT: u32 = 24;
pub const CUR_ADDR_HI_MASK: u32 = r::cursor0_0_cursor_surface_address_high::CURSOR_SURFACE_ADDRESS_HIGH.mask();
pub const CUR0_ENABLE: u32 = r::cnvc_cur0_cursor0_control::CUR0_ENABLE.mask();
pub const CUR0_EXPANSION_MODE: u32 = r::cnvc_cur0_cursor0_control::CUR0_EXPANSION_MODE.mask();
pub const CUR0_ROM_EN: u32 = r::cnvc_cur0_cursor0_control::CUR0_ROM_EN.mask();
pub const CUR0_MODE: u32 = r::cnvc_cur0_cursor0_control::CUR0_MODE.mask();
pub const CUR0_MODE_SHIFT: u32 = 4;
/// CUR0_UPDATE_PENDING [16] is status: never part of a read-back check.
pub const CUR0_UPDATE_PENDING: u32 = r::cnvc_cur0_cursor0_control::CUR0_UPDATE_PENDING.mask();
/// MPCCn_MPCC_OPP_ID: the OPP whose MPC tree takes DPP n (0xF: none); the cursor lock is per OPP.
pub const MPCC_OPP_ID: u32 = r::mpcc0_mpcc_opp_id::REG.byte();
pub const MPCC_OPP_ID_MASK: u32 = r::mpcc0_mpcc_opp_id::MPCC_OPP_ID.mask();
pub const MPCC_STRIDE: u32 = r::mpcc1_mpcc_opp_id::REG.byte() - r::mpcc0_mpcc_opp_id::REG.byte();
pub const CUR_LOCK_STRIDE: u32 = r::cur_vupdate_lock_set1::REG.byte() - r::cur_vupdate_lock_set0::REG.byte();
/// OPPs of DCN 3.1.4 (one per pipe; CUR_VUPDATE_LOCK_SET0..3 in the header).
pub const OPPS: u32 = 4;
pub const fn mpcc(m: u32, reg0: u32) -> u32 {
    reg0 + m * MPCC_STRIDE
}
/// CUR_VUPDATE_LOCK_SETn of OPP `opp` (bit 0: cursor updates held until the unlock, then taken at VUPDATE).
pub const fn cur_lock(opp: u32) -> u32 {
    r::cur_vupdate_lock_set0::REG.byte() + opp * CUR_LOCK_STRIDE
}
