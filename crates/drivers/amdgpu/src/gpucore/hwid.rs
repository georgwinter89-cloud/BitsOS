//! Hardware IDs of the IP discovery table: the numbers the blob carries per IP (facts from
//! linux-6.6/drivers/gpu/drm/amd/include/soc15_hw_ip.h), their short names, and which numbers Linux folds into one
//! IP kind (`hw_id_map`, linux-6.6/drivers/gpu/drm/amd/amdgpu/amdgpu_discovery.c:178-205). An ID BitsOS does not
//! know keeps its number (`name` is `None`); nothing is dropped for being unknown.

pub const MP1: u16 = 1;
pub const MP2: u16 = 2;
pub const THM: u16 = 3;
pub const SMUIO: u16 = 4;
pub const FUSE: u16 = 5;
pub const CLKA: u16 = 6;
pub const PWR: u16 = 10;
pub const GC: u16 = 11;
/// UVD and VCN share the number.
pub const UVD: u16 = 12;
pub const VCN: u16 = UVD;
pub const ACP: u16 = 14;
pub const DCI: u16 = 15;
pub const IOHC: u16 = 24;
pub const VCE: u16 = 32;
pub const MMHUB: u16 = 34;
pub const ATHUB: u16 = 35;
pub const OSSSYS: u16 = 40;
pub const HDP: u16 = 41;
pub const SDMA0: u16 = 42;
pub const SDMA1: u16 = 43;
pub const ISP: u16 = 44;
pub const DBGU_IO: u16 = 45;
pub const DF: u16 = 46;
pub const FCH: u16 = 48;
pub const SDMA2: u16 = 68;
pub const SDMA3: u16 = 69;
pub const PCIE: u16 = 70;
pub const LSDMA: u16 = 91;
pub const NBIF: u16 = 108;
pub const UMC: u16 = 150;
pub const SATA: u16 = 168;
pub const XGMI: u16 = 200;
pub const XGBE: u16 = 216;
pub const MP0: u16 = 255;
pub const DMU: u16 = 271;
pub const DIO: u16 = 272;
pub const DAZ: u16 = 274;
/// Linux rejects IDs at or above this (`HW_ID_MAX`, soc15_hw_ip.h; `amdgpu_discovery_validate_ip` skips them).
pub const LINUX_MAX: u16 = 300;
/// Linux rejects instance numbers at or above this (`HWIP_MAX_INSTANCE`, linux-6.6 amdgpu.h:673).
pub const LINUX_MAX_INSTANCE: u8 = 44;

/// (number, name) in number order; the names are the ones Linux's sysfs view uses.
const NAMES: &[(u16, &str)] = &[
    (1, "MP1"),
    (2, "MP2"),
    (3, "THM"),
    (4, "SMUIO"),
    (5, "FUSE"),
    (6, "CLKA"),
    (10, "PWR"),
    (11, "GC"),
    (12, "UVD"),
    (13, "AUDIO_AZ"),
    (14, "ACP"),
    (15, "DCI"),
    (16, "DCO"),
    (17, "XDMA"),
    (18, "DCEAZ"),
    (19, "SDPMUX"),
    (20, "NTB"),
    (24, "IOHC"),
    (28, "L2IMU"),
    (32, "VCE"),
    (34, "MMHUB"),
    (35, "ATHUB"),
    (36, "DBGU_NBIO"),
    (37, "DFX"),
    (38, "DBGU0"),
    (39, "DBGU1"),
    (40, "OSSSYS"),
    (41, "HDP"),
    (42, "SDMA0"),
    (43, "SDMA1"),
    (44, "ISP"),
    (45, "DBGU_IO"),
    (46, "DF"),
    (47, "CLKB"),
    (48, "FCH"),
    (49, "DFX_DAP"),
    (50, "L1IMU_PCIE"),
    (51, "L1IMU_NBIF"),
    (52, "L1IMU_IOAGR"),
    (53, "L1IMU3"),
    (54, "L1IMU4"),
    (55, "L1IMU5"),
    (56, "L1IMU6"),
    (57, "L1IMU7"),
    (58, "L1IMU8"),
    (59, "L1IMU9"),
    (60, "L1IMU10"),
    (61, "L1IMU11"),
    (62, "L1IMU12"),
    (63, "L1IMU13"),
    (64, "L1IMU14"),
    (65, "L1IMU15"),
    (66, "WAFLC"),
    (67, "FCH_USB_PD"),
    (68, "SDMA2"),
    (69, "SDMA3"),
    (70, "PCIE"),
    (80, "PCS"),
    (89, "DDCL"),
    (90, "SST"),
    (91, "LSDMA"),
    (100, "IOAGR"),
    (108, "NBIF"),
    (124, "IOAPIC"),
    (128, "SYSTEMHUB"),
    (144, "NTBCCP"),
    (150, "UMC"),
    (168, "SATA"),
    (170, "USB"),
    (176, "CCXSEC"),
    (200, "XGMI"),
    (216, "XGBE"),
    (255, "MP0"),
    (271, "DMU"),
    (272, "DIO"),
    (274, "DAZ"),
];

/// The short name of a hardware ID, if BitsOS knows it.
pub fn name(hw_id: u16) -> Option<&'static str> {
    NAMES.binary_search_by_key(&hw_id, |&(n, _)| n).ok().map(|i| NAMES[i].1)
}

/// The number of a short name (the trace tool's and the tests' way back from Linux's sysfs names).
pub fn by_name(name: &str) -> Option<u16> {
    NAMES.iter().find(|&&(_, n)| n == name).map(|&(id, _)| id)
}

/// The SDMA engines: SDMA0..3 each have their own number; an engine's index is the number's slot plus the
/// instance (Linux counts both ways: Sienna Cichlid enumerates four IDs, Arcturus repeats two).
pub fn sdma_slot(hw_id: u16) -> Option<u8> {
    match hw_id {
        SDMA0 => Some(0),
        SDMA1 => Some(1),
        SDMA2 => Some(2),
        SDMA3 => Some(3),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn names_sorted_and_unique() {
        assert!(super::NAMES.windows(2).all(|w| w[0].0 < w[1].0));
        assert_eq!(super::name(super::DMU), Some("DMU"));
        assert_eq!(super::by_name("NBIF"), Some(super::NBIF));
        assert_eq!(super::name(282), None);
    }
}
