//! What libdrm saw on card A under Linux on 5.11. (`host/tests/dr/fixtures/9070xt-info.txt`), as named constants, for the DRM
//! profile GX-10 item 19 will build. `crates/drm`'s `ChipProfile` is not used: this crate depends on nothing.

pub const DEVICE_ID: u32 = 0x7550;
pub const CHIP_REV: u32 = 0x1;
pub const EXTERNAL_REV: u32 = 0x51;
pub const PCI_REV: u32 = 0xC0;
/// `AMDGPU_FAMILY_GC_12_0_0`.
pub const FAMILY: u32 = 152;
pub const NUM_SHADER_ENGINES: u32 = 4;
pub const NUM_SHADER_ARRAYS_PER_ENGINE: u32 = 2;
pub const GPU_COUNTER_FREQ_KHZ: u32 = 100_000;
pub const MAX_ENGINE_CLOCK_KHZ: u64 = 2_400_000;
pub const MAX_MEMORY_CLOCK_KHZ: u64 = 1_258_000;
pub const CU_ACTIVE: u32 = 64;
pub const ENABLED_RB_PIPES_MASK: u32 = 0xFFFF;
pub const NUM_RB_PIPES: u32 = 16;
pub const NUM_HW_GFX_CONTEXTS: u32 = 8;
pub const PCIE_GEN: u32 = 5;
pub const IDS_FLAGS: u64 = 0x18;
pub const VRAM_TYPE: u32 = 9;
pub const VRAM_BIT_WIDTH: u32 = 256;
pub const NUM_CU_PER_SH: u32 = 8;
pub const GB_ADDR_CONFIG: u32 = 0x0820_0545;

/// `struct drm_amdgpu_info_hw_ip` as this card answers it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HwIp {
    pub query: u32,
    pub major: u32,
    pub minor: u32,
    pub ib_start_alignment: u32,
    pub ib_size_alignment: u32,
    pub rings_mask: u32,
    pub discovery_version: u32,
}

const fn ip(query: u32, major: u32, ib: (u32, u32), rings_mask: u32, discovery_version: u32) -> HwIp {
    HwIp { query, major, minor: 0, ib_start_alignment: ib.0, ib_size_alignment: ib.1, rings_mask, discovery_version }
}

/// The `AMDGPU_HW_IP_*` queries that answered; UVD, VCE, UVD_ENC and VPE (3, 4, 5, 9) answered with zeros.
pub const HW_IP: [HwIp; 6] = [
    ip(0, 12, (0x20, 0x20), 0x1, 0x000C_0001), // GFX
    ip(1, 12, (0x20, 0x20), 0xF, 0x000C_0001), // COMPUTE
    ip(2, 7, (0x100, 0x4), 0x3, 0x0007_0001),  // DMA
    ip(6, 5, (0x100, 0x40), 0x0, 0x0005_0000), // VCN_DEC, `available_rings` 0 in the capture
    ip(7, 5, (0x100, 0x4), 0x1, 0x0005_0000),  // VCN_ENC
    ip(8, 5, (0x100, 0x40), 0x1, 0x0005_0000), // VCN_JPEG
];
