//! The Radeon RX 9070 XT (Navi 48, GC 12.0.1, family 152) as Linux 7.3.1's amdgpu driver showed it on the Gaming-PC
//! (card A, 5.11.2026): the INFO answers libdrm got (host/tests/dr/fixtures/9070xt-info.txt, taken with
//! host/tests/dr/libdrm/info.c as a normal user). Only chip data; that boot's memory use is `fake::FakeMemory::SEEN_9070XT`.
//! Firmware versions that only debugfs shows (root) are not captured: they answer 0 here, as an unloaded firmware
//! does, until Georg's capture fills them in (plan/spec/gpu-rdna4.md piece 21); Mesa reads ME, PFP, MEC and VCN.

use crate::gpu::{ChipProfile, Codec, Firmware, Gds, HwIp, Register, Vbios};
use crate::uapi::*;

/// DEV_INFO as captured (Linux 7.3.1 writes 440 bytes: its struct ends at userq_ip_mask). ids_flags 0x18 =
/// CONFORMANT_TRUNC_COORD | 0x10; no FUSION (a card, not an APU), no TMZ ("Trusted Memory Zone not supported").
const DEV_INFO: drm_amdgpu_info_device = drm_amdgpu_info_device {
    device_id: 0x7550,
    chip_rev: 1,
    external_rev: 0x51,
    pci_rev: 0xc0,
    family: 0x98,
    num_shader_engines: 4,
    num_shader_arrays_per_engine: 2,
    gpu_counter_freq: 0x1_86a0,
    max_engine_clock: 0x24_9f00,
    max_memory_clock: 0x13_3210,
    cu_active_number: 0x40,
    cu_ao_mask: 0,
    cu_bitmap: [[0xff, 0xff, 0, 0], [0xff, 0xff, 0, 0], [0xff, 0xff, 0, 0], [0xff, 0xff, 0, 0]],
    enabled_rb_pipes_mask: 0xffff,
    num_rb_pipes: 0x10,
    num_hw_gfx_contexts: 8,
    pcie_gen: 5,
    ids_flags: 0x18,
    virtual_address_offset: 0x1_0000,
    virtual_address_max: 0x8000_0000_0000,
    virtual_address_alignment: 0x1000,
    pte_fragment_size: 0x20_0000,
    gart_page_size: 0x1000,
    ce_ram_size: 0,
    vram_type: 9,
    vram_bit_width: 0x100,
    vce_harvest_config: 0,
    gc_double_offchip_lds_buf: 0x40,
    prim_buf_gpu_addr: 0,
    pos_buf_gpu_addr: 0,
    cntl_sb_buf_gpu_addr: 0,
    param_buf_gpu_addr: 0,
    prim_buf_size: 0,
    pos_buf_size: 0,
    cntl_sb_buf_size: 0,
    param_buf_size: 0,
    wave_front_size: 0x20,
    num_shader_visible_vgprs: 0x600,
    num_cu_per_sh: 8,
    num_tcc_blocks: 0x20,
    gs_vgt_table_depth: 0x20,
    gs_prim_buffer_depth: 0x700,
    max_gs_waves_per_vgt: 0x20,
    pcie_num_lanes: 8,
    cu_ao_bitmap: [[0, 0, 0, 0], [0, 0, 0, 0], [0, 0, 0, 0], [0, 0, 0, 0]],
    high_va_offset: 0xffff_8000_0000_0000,
    high_va_max: 0xffff_ffff_ffbf_0000,
    pa_sc_tile_steering_override: 0,
    tcc_disabled_mask: 0,
    min_engine_clock: 0x7_a120,
    min_memory_clock: 0x1_7700,
    tcp_cache_size: 0x20,
    num_sqc_per_wgp: 1,
    sqc_data_cache_size: 0x10,
    sqc_inst_cache_size: 0x20,
    gl1c_cache_size: 0x100,
    gl2c_cache_size: 0x2000,
    mall_size: 0x400_0000,
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

/// INFO FW_VERSION: ME, PFP, MEC and VCN as captured; SMC and DMCUB from the journal of that boot ("smu fw version
/// 0x00685000 (104.80.0)", "DMUB ... version=0x0A005000"). GC 12 has no CE and no MEC2.
const FIRMWARE: &[Firmware] = &[
    fw(AMDGPU_INFO_FW_GFX_ME, 0, 0xc26, 29),
    fw(AMDGPU_INFO_FW_GFX_PFP, 0, 0xc94, 29),
    fw(AMDGPU_INFO_FW_GFX_CE, 0, 0, 0),
    fw(AMDGPU_INFO_FW_GFX_MEC, 0, 0xda2, 29),
    fw(AMDGPU_INFO_FW_GFX_MEC, 1, 0, 0),
    fw(AMDGPU_INFO_FW_SMC, 0, 0x0068_5000, 0),
    fw(AMDGPU_INFO_FW_VCN, 0, 0x0910_d021, 0),
    fw(AMDGPU_INFO_FW_DMCUB, 0, 0x0a00_5000, 0),
];

/// GB_ADDR_CONFIG (GC base 0x1260 + 0x13de, gc_12_0_0_offset.h), the one register Mesa reads; the same dword as on
/// the 780M.
pub const GB_ADDR_CONFIG: u32 = 0x263e;
/// What the card answered (libdrm's gb_addr_cfg).
pub const GB_ADDR_CONFIG_VALUE: u32 = 0x0820_0545;

const fn reg(offset: u32) -> Register {
    Register { offset, indexed: false }
}

/// soc24's allowed list (linux-7.3.1 amdgpu/soc24.c:114-134) with the card's GC and SDMA base 0x1260: GRBM_STATUS,
/// GRBM_STATUS2, GRBM_STATUS_SE0..3, SDMA0_STATUS_REG, SDMA1_STATUS_REG, CP_STAT, CP_STALLED_STAT1..3,
/// CP_CPF_BUSY_STAT, CP_CPF_STALLED_STAT1, CP_CPF_STATUS, CP_CPC_BUSY_STAT, CP_CPC_STALLED_STAT1, CP_CPC_STATUS,
/// GB_ADDR_CONFIG (offsets: gc_12_0_0_offset.h, crates/rdna4-regs).
const REGISTERS: &[Register] = &[
    reg(0x2004),
    reg(0x2002),
    reg(0x2005),
    reg(0x2006),
    reg(0x200e),
    reg(0x200f),
    reg(0x1284),
    reg(0x1884),
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

pub const RADEON_9070XT: ChipProfile = ChipProfile {
    name: "Radeon RX 9070 XT (GC 12.0.1)",
    dev_info: DEV_INFO,
    // GFX (1 ring), COMPUTE (4 rings), DMA (2 rings), UVD, VCE, UVD_ENC, VCN_DEC (VCN 5 has no decode-only ring),
    // VCN_ENC (the unified ring), JPEG, VPE. Alignments as captured.
    hw_ip: [
        ip(12, 0x000c_0001, 32, 32, 1),
        ip(12, 0x000c_0001, 32, 32, 4),
        ip(7, 0x0007_0001, 256, 4, 2),
        None,
        None,
        None,
        ip(5, 0x0005_0000, 256, 64, 0),
        ip(5, 0x0005_0000, 256, 4, 1),
        ip(5, 0x0005_0000, 256, 64, 1),
        None,
    ],
    firmware: FIRMWARE,
    registers: REGISTERS,
    video: Some((DECODE, ENCODE)),
    max_ibs: Some([0xc0, 0x7d, 0x31, 0x31, 0x31, 0x31, 0x31, 0x31, 0x10, 0x31]),
    // No image: the VBIOS comes from ACPI VFCT on this board and nothing reads it yet; only the part number.
    vbios: Vbios { size: 0, name: "", pn: "113-EXT112333-100", version: 0, ver_str: "", date: "" },
    gds: Gds { gds_size: 0x1000, gws_size: 64, oa_size: 16 },
    ras_features: None,
    // Unverified for GC 12 (user queues are off in the capture: userq_ip_mask 0): no such query until G4/G5 read
    // gfx_v12_0's and sdma_v7_0's numbers.
    uq_fw_areas: crate::gpu::UqFwAreas { gfx: None, compute: None, sdma: None },
    // BAR2 is 256 MiB on the card (lspci); the kernel's own doorbell pages as on the 780M until GC 12's are read
    // (unverified; nothing hands out a DOORBELL object before G5).
    doorbell_room: (256 << 20) - 2 * 4096,
};

/// Card A idle under Linux (hwmon, pp_dpm on 5.11.): MHz, millidegrees, %, W, mV. Temperature and voltages were not
/// in the capture: the fake answers plausible idle values, unverified.
pub const SENSORS: &[(crate::gpu::Sensor, u32)] = {
    use crate::gpu::Sensor::*;
    &[(GfxSclk, 1661), (GfxMclk, 456), (GpuTemp, 40_000), (GpuLoad, 9), (GpuAvgPower, 29), (Vddnb, 0), (Vddgfx, 0), (StablePstateSclk, 0), (StablePstateMclk, 0), (GpuInputPower, 29)]
};
