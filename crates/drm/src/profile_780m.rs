//! The Radeon 780M (Phoenix, GC 11.0.1, family 148) as Linux's amdgpu driver showed it on a real machine: the INFO
//! answers Mesa got at device creation and the driver's debugfs after the load (firmware versions, gca config,
//! power). Only chip data; the memory numbers of that machine are the fake's defaults in `fake.rs`.

use crate::gpu::{ChipProfile, Codec, Firmware, Gds, HwIp, Register, Vbios};
use crate::uapi::*;

/// DEV_INFO as captured (448 bytes, libdrm 2.4.134's struct). ids_flags 0x1d = FUSION | TMZ |
/// CONFORMANT_TRUNC_COORD | 0x10 (a flag of a newer kernel); external_rev 0xd.
const DEV_INFO: drm_amdgpu_info_device = drm_amdgpu_info_device {
    device_id: 0x1900,
    chip_rev: 0xc,
    external_rev: 0xd,
    pci_rev: 0xd5,
    family: 0x94,
    num_shader_engines: 1,
    num_shader_arrays_per_engine: 2,
    gpu_counter_freq: 99_810,
    max_engine_clock: 2_700_000,
    max_memory_clock: 2_800_000,
    cu_active_number: 12,
    cu_ao_mask: 0,
    cu_bitmap: [[0x3f, 0x3f, 0, 0], [0; 4], [0; 4], [0; 4]],
    enabled_rb_pipes_mask: 0xf,
    num_rb_pipes: 4,
    num_hw_gfx_contexts: 8,
    pcie_gen: 4,
    ids_flags: 0x1d,
    virtual_address_offset: 0x1_0000,
    virtual_address_max: 0x8000_0000_0000,
    virtual_address_alignment: 0x1000,
    pte_fragment_size: 0x20_0000,
    gart_page_size: 0x1000,
    ce_ram_size: 0,
    vram_type: 0xa,
    vram_bit_width: 128,
    vce_harvest_config: 0,
    gc_double_offchip_lds_buf: 0,
    prim_buf_gpu_addr: 0,
    pos_buf_gpu_addr: 0,
    cntl_sb_buf_gpu_addr: 0,
    param_buf_gpu_addr: 0,
    prim_buf_size: 0,
    pos_buf_size: 0,
    cntl_sb_buf_size: 0,
    param_buf_size: 0,
    wave_front_size: 32,
    num_shader_visible_vgprs: 0x400,
    num_cu_per_sh: 6,
    num_tcc_blocks: 4,
    gs_vgt_table_depth: 32,
    gs_prim_buffer_depth: 0x700,
    max_gs_waves_per_vgt: 32,
    pcie_num_lanes: 16,
    cu_ao_bitmap: [[0; 4]; 4],
    high_va_offset: 0xffff_8000_0000_0000,
    high_va_max: 0xffff_ffff_ffbf_0000,
    pa_sc_tile_steering_override: 0,
    tcc_disabled_mask: 0,
    min_engine_clock: 800_000,
    min_memory_clock: 1_000_000,
    tcp_cache_size: 32,
    num_sqc_per_wgp: 1,
    sqc_data_cache_size: 16,
    sqc_inst_cache_size: 32,
    gl1c_cache_size: 256,
    gl2c_cache_size: 2048,
    mall_size: 0,
    enabled_rb_pipes_mask_hi: 0,
    shadow_size: 0,
    shadow_alignment: 0,
    csa_size: 0,
    csa_alignment: 0,
    userq_ip_mask: 0,
    pad: 0,
};

const fn ip(major: u32, discovery: u32, start: u32, size: u32, rings: u32) -> Option<HwIp> {
    Some(HwIp { major, minor: 0, discovery, ib_start_alignment: start, ib_size_alignment: size, rings })
}

const fn fw(fw_type: u32, index: u32, ver: u32, feature: u32) -> Firmware {
    Firmware { fw_type, index, ver, feature }
}

/// debugfs amdgpu_firmware_info (the same function answers INFO FW_VERSION); MEC2 and CAP are not loaded (0).
const FIRMWARE: &[Firmware] = &[
    fw(AMDGPU_INFO_FW_VCE, 0, 0, 0),
    fw(AMDGPU_INFO_FW_UVD, 0, 0, 0),
    fw(AMDGPU_INFO_FW_GMC, 0, 0, 0),
    fw(AMDGPU_INFO_FW_GFX_ME, 0, 0x67, 35),
    fw(AMDGPU_INFO_FW_GFX_PFP, 0, 0x6f, 35),
    fw(AMDGPU_INFO_FW_GFX_CE, 0, 0, 0),
    fw(AMDGPU_INFO_FW_GFX_RLC, 0, 0x8c, 1),
    fw(AMDGPU_INFO_FW_GFX_RLC_RESTORE_LIST_CNTL, 0, 0, 0),
    fw(AMDGPU_INFO_FW_GFX_RLC_RESTORE_LIST_GPM_MEM, 0, 0, 0),
    fw(AMDGPU_INFO_FW_GFX_RLC_RESTORE_LIST_SRM_MEM, 0, 0, 0),
    fw(AMDGPU_INFO_FW_GFX_RLCP, 0, 0xf, 1),
    fw(AMDGPU_INFO_FW_GFX_RLCV, 0, 0, 0),
    fw(AMDGPU_INFO_FW_GFX_MEC, 0, 0x46, 35),
    fw(AMDGPU_INFO_FW_GFX_MEC, 1, 0, 0),
    fw(AMDGPU_INFO_FW_SMC, 0, 0x004c_5700, 0),
    // TA by TA_FW_TYPE_PSP_*: XGMI 2, RAS 3, HDCP 4, DTM 5, RAP 6, SECUREDISPLAY 7.
    fw(AMDGPU_INFO_FW_TA, 2, 0, 0),
    fw(AMDGPU_INFO_FW_TA, 3, 0, 0),
    fw(AMDGPU_INFO_FW_TA, 4, 0x1700_0050, 0),
    fw(AMDGPU_INFO_FW_TA, 5, 0x1200_001d, 0),
    fw(AMDGPU_INFO_FW_TA, 6, 0, 0),
    fw(AMDGPU_INFO_FW_TA, 7, 0, 0),
    fw(AMDGPU_INFO_FW_SDMA, 0, 0x18, 60),
    fw(AMDGPU_INFO_FW_SOS, 0, 0, 0),
    fw(AMDGPU_INFO_FW_ASD, 0, 0x2100_0110, 0x2100_0110),
    fw(AMDGPU_INFO_FW_VCN, 0, 0x0911_8022, 0),
    fw(AMDGPU_INFO_FW_DMCU, 0, 0, 0),
    fw(AMDGPU_INFO_FW_DMCUB, 0, 0x0800_5d00, 0),
    fw(AMDGPU_INFO_FW_TOC, 0, 0xb, 0),
    fw(AMDGPU_INFO_FW_CAP, 0, 0, 0),
    fw(AMDGPU_INFO_FW_MES_KIQ, 0, 0x110, 6),
    fw(AMDGPU_INFO_FW_MES, 0, 0x8b, 1),
    fw(AMDGPU_INFO_FW_IMU, 0, 0x0b01_2d00, 0),
];

/// GB_ADDR_CONFIG (GC segment 0 at 0x1260 + 0x13de), the one register Mesa reads.
pub const GB_ADDR_CONFIG: u32 = 0x263e;

const fn reg(offset: u32) -> Register {
    Register { offset, indexed: false }
}

/// soc21's allowed list in GC segment 0 (base 0x1260): GRBM_STATUS, GRBM_STATUS2, GRBM_STATUS_SE0..3, CP_STAT,
/// CP_STALLED_STAT1..3, CP_CPF_BUSY_STAT, CP_CPF_STALLED_STAT1, CP_CPF_STATUS, CP_CPC_BUSY_STAT,
/// CP_CPC_STALLED_STAT1, CP_CPC_STATUS, GB_ADDR_CONFIG. The SDMA status registers are left out until the SDMA
/// register base of the 780M is known (GX-7i).
const REGISTERS: &[Register] = &[
    reg(0x2004),
    reg(0x2002),
    reg(0x2005),
    reg(0x2006),
    reg(0x200e),
    reg(0x200f),
    reg(0x21a0),
    reg(0x219d),
    reg(0x219e),
    reg(0x219c),
    reg(0x2088),
    reg(0x2089),
    reg(0x2087),
    reg(0x2085),
    reg(0x2086),
    reg(0x2084),
    reg(GB_ADDR_CONFIG),
];

/// gca_config's gb_addr_config: what Linux answers for GB_ADDR_CONFIG (its cached value, soc21.c:295).
pub const GB_ADDR_CONFIG_VALUE: u32 = 0x242;

const fn codec(max_width: u32, max_height: u32, max_pixels_per_frame: u32, max_level: u32) -> Option<Codec> {
    Some(Codec { max_width, max_height, max_pixels_per_frame, max_level })
}

/// VIDEO_CAPS by codec index (MPEG2, MPEG4, VC1, AVC, HEVC, JPEG, VP9, AV1) as captured.
const DECODE: [Option<Codec>; 8] = [
    None,
    None,
    None,
    codec(4096, 4096, 0x100_0000, 52),
    codec(8192, 4352, 0x220_0000, 186),
    codec(16384, 16384, 0x1000_0000, 0),
    codec(8192, 4352, 0x220_0000, 0),
    codec(8192, 4352, 0x220_0000, 0),
];
const ENCODE: [Option<Codec>; 8] = [
    None,
    None,
    None,
    codec(4096, 4096, 0x100_0000, 0),
    codec(8192, 4352, 0x220_0000, 0),
    None,
    None,
    codec(8192, 4352, 0x220_0000, 0),
];

pub const RADEON_780M: ChipProfile = ChipProfile {
    name: "Radeon 780M (GC 11.0.1)",
    dev_info: DEV_INFO,
    // GFX, COMPUTE (8 rings up, 4 context entities), DMA, UVD, VCE, UVD_ENC, VCN_DEC (VCN 4 has no decode-only
    // ring), VCN_ENC (the unified ring), JPEG, VPE. Alignments as captured (newer than Linux 6.6's for VCN).
    hw_ip: [
        ip(11, 0x000b_0001, 32, 32, 1),
        ip(11, 0x000b_0001, 32, 32, 8),
        ip(6, 0x0006_0001, 256, 4, 1),
        None,
        None,
        None,
        ip(4, 0x0004_0002, 256, 64, 0),
        ip(4, 0x0004_0002, 256, 4, 1),
        ip(4, 0x0004_0002, 256, 64, 1),
        None,
    ],
    firmware: FIRMWARE,
    registers: REGISTERS,
    video: Some((DECODE, ENCODE)),
    max_ibs: Some([0xc0, 0x7d, 0x31, 0x31, 0x31, 0x31, 0x31, 0x31, 0x10, 0x31]),
    // The fake carries no image: size 0, only the part number.
    vbios: Vbios { size: 0, name: "", pn: "113-PHXGENERIC-001", version: 0, ver_str: "", date: "" },
    gds: Gds { gds_size: 0x1000, gws_size: 64, oa_size: 16 },
    ras_features: None,
};

/// pm_info after the load (MHz, millidegrees, %, W, mV); peak pstate clocks were not reported.
pub const SENSORS: &[(crate::gpu::Sensor, u32)] = {
    use crate::gpu::Sensor::*;
    &[
        (GfxSclk, 800),
        (GfxMclk, 2800),
        (GpuTemp, 39_000),
        (GpuLoad, 0),
        (GpuAvgPower, 9),
        (Vddnb, 762),
        (Vddgfx, 1165),
        (StablePstateSclk, 0),
        (StablePstateMclk, 0),
    ]
};
