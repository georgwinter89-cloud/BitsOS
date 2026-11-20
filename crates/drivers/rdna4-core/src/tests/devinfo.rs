use crate::devinfo::*;
use std::vec::Vec;

/// `Q 0x16` of `host/tests/dr/fixtures/9070xt-info.txt`: the 448 bytes of `drm_amdgpu_info_device` on card A.
const DEV_INFO: &str = concat!(
    "507500000100000051000000c0000000980000000400000002000000a0860100009f2400000000001032130000000000",
    "4000000000000000ff000000ff0000000000000000000000ff000000ff0000000000000000000000ff000000ff000000",
    "0000000000000000ff000000ff0000000000000000000000ffff00001000000008000000050000001800000000000000",
    "000001000000000000000000008000000010000000002000001000000000000009000000000100000000000040000000",
    "000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000",
    "200000000006000008000000200000002000000000070000200000000800000000000000000000000000000000000000",
    "000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000",
    "000000000080ffff0000bfffffffffff0000000000000000000000000000000020a10700000000000077010000000000",
    "200000000100000010000000200000000001000000200000000000040000000000000000000000000000000000000000",
    "0000000000000000a5a5a5a5a5a5a5a5",
);

fn bytes(hex: &str) -> Vec<u8> {
    (0..hex.len() / 2).map(|i| u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).unwrap()).collect()
}

fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
}

fn u64_at(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().unwrap())
}

/// Offsets follow `struct drm_amdgpu_info_device` in `include/uapi/drm/amdgpu_drm.h`.
#[test]
fn the_constants_are_what_the_card_answered() {
    let b = bytes(DEV_INFO);
    assert_eq!(b.len(), 448);
    assert_eq!(u32_at(&b, 0), DEVICE_ID);
    assert_eq!(u32_at(&b, 4), CHIP_REV);
    assert_eq!(u32_at(&b, 8), EXTERNAL_REV);
    assert_eq!(u32_at(&b, 12), PCI_REV);
    assert_eq!(u32_at(&b, 16), FAMILY);
    assert_eq!(u32_at(&b, 20), NUM_SHADER_ENGINES);
    assert_eq!(u32_at(&b, 24), NUM_SHADER_ARRAYS_PER_ENGINE);
    assert_eq!(u32_at(&b, 28), GPU_COUNTER_FREQ_KHZ);
    assert_eq!(u64_at(&b, 32), MAX_ENGINE_CLOCK_KHZ);
    assert_eq!(u64_at(&b, 40), MAX_MEMORY_CLOCK_KHZ);
    assert_eq!(u32_at(&b, 48), CU_ACTIVE);
    assert_eq!(u32_at(&b, 120), ENABLED_RB_PIPES_MASK);
    assert_eq!(u32_at(&b, 124), NUM_RB_PIPES);
    assert_eq!(u32_at(&b, 128), NUM_HW_GFX_CONTEXTS);
    assert_eq!(u32_at(&b, 132), PCIE_GEN);
    assert_eq!(u64_at(&b, 136), IDS_FLAGS);
    assert_eq!(u32_at(&b, 176), VRAM_TYPE);
    assert_eq!(u32_at(&b, 180), VRAM_BIT_WIDTH);
    assert_eq!(u32_at(&b, 248), NUM_CU_PER_SH);
}

#[test]
fn the_compute_units_add_up() {
    assert_eq!(NUM_SHADER_ENGINES * NUM_SHADER_ARRAYS_PER_ENGINE * NUM_CU_PER_SH, CU_ACTIVE);
    assert_eq!(ENABLED_RB_PIPES_MASK.count_ones(), NUM_RB_PIPES);
}

const HW_IP_CAPTURE: [(u32, &str); 6] = [
    (0, "0c00000000000000000000000000000020000000200000000100000001000c00"),
    (1, "0c00000000000000000000000000000020000000200000000f00000001000c00"),
    (2, "0700000000000000000000000000000000010000040000000300000001000700"),
    (6, "0500000000000000000000000000000000010000400000000000000000000500"),
    (7, "0500000000000000000000000000000000010000040000000100000000000500"),
    (8, "0500000000000000000000000000000000010000400000000100000000000500"),
];

/// Offsets follow `struct drm_amdgpu_info_hw_ip`.
#[test]
fn the_hw_ip_table_is_what_the_card_answered() {
    assert_eq!(HW_IP.len(), HW_IP_CAPTURE.len());
    for (ip, (query, hex)) in HW_IP.iter().zip(HW_IP_CAPTURE) {
        let b = bytes(hex);
        assert_eq!(b.len(), 32);
        assert_eq!(ip.query, query);
        assert_eq!(u32_at(&b, 0), ip.major, "query {query}");
        assert_eq!(u32_at(&b, 4), ip.minor, "query {query}");
        assert_eq!(u64_at(&b, 8), 0, "query {query}: capabilities");
        assert_eq!(u32_at(&b, 16), ip.ib_start_alignment, "query {query}");
        assert_eq!(u32_at(&b, 20), ip.ib_size_alignment, "query {query}");
        assert_eq!(u32_at(&b, 24), ip.rings_mask, "query {query}");
        assert_eq!(u32_at(&b, 28), ip.discovery_version, "query {query}");
    }
}

#[test]
fn four_compute_rings_and_two_copy_rings() {
    assert_eq!(HW_IP[1].rings_mask.count_ones(), 4);
    assert_eq!(HW_IP[2].rings_mask.count_ones(), 2);
}
