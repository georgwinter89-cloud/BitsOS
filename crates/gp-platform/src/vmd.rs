//! Intel Volume Management Device: the ids Linux's driver binds (linux-7.3.1 drivers/pci/controller/vmd.c:1235-1269
//! `vmd_ids`). BitsOS has no VMD driver and never touches the function; it only says that one is switched on,
//! because a drive mapped under it is invisible to the bus walk (it lives behind the function's own configuration
//! window, Linux's domain 0x10000).

pub const VENDOR_INTEL: u16 = 0x8086;

/// Device ids of `vmd_ids`, in its order (vmd.c:1236-1267; the three named ones are pci_ids.h:2787, :2900, :2901,
/// :3146).
pub const DEVICE_IDS: [u16; 14] = [
    0x201d, // PCI_DEVICE_ID_INTEL_VMD_201D
    0x28c0, // PCI_DEVICE_ID_INTEL_VMD_28C0
    0x28c1, // PCI_DEVICE_ID_INTEL_VMD_28C1
    0x467f, 0x4c3d, 0xa77f, 0x7d0b, //
    0xad0b, // this machine's 00:0e.0
    0x9a0b, // PCI_DEVICE_ID_INTEL_VMD_9A0B
    0xb60b, 0xb06f, 0xb07f, 0xd70b, 0xd73b,
];

/// Is this function a VMD controller Linux would bind? By id only, as Linux: the class (RAID, 0x0104) is shared
/// with real RAID controllers.
pub fn is_vmd(vendor: u16, device: u16) -> bool {
    vendor == VENDOR_INTEL && DEVICE_IDS.contains(&device)
}

/// Buses behind a VMD: its configuration BAR holds 1 MiB per bus (vmd.c:1116 refuses a BAR under 1 MiB; this
/// machine's 32 MiB BAR gives the 32 buses e0-ff Linux logs).
pub fn buses_behind(cfgbar_bytes: u64) -> u32 {
    (cfgbar_bytes >> 20).min(256) as u32
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::facts;

    #[test]
    fn the_table_is_linuxs_list() {
        // Typed a second time from vmd.c:1236-1267, in source order.
        let linux = [0x201d, 0x28c0, 0x28c1, 0x467f, 0x4c3d, 0xa77f, 0x7d0b, 0xad0b, 0x9a0b, 0xb60b, 0xb06f, 0xb07f, 0xd70b, 0xd73b];
        assert_eq!(DEVICE_IDS, linux);
        let mut sorted = DEVICE_IDS;
        sorted.sort_unstable();
        assert!(sorted.windows(2).all(|w| w[0] != w[1]), "no id twice");
    }

    #[test]
    fn this_machines_function_is_one_and_only_that() {
        let vmds: alloc::vec::Vec<_> = facts::FUNCTIONS.iter().filter(|x| is_vmd(x.vendor, x.device)).collect();
        assert_eq!(vmds.len(), 1);
        assert_eq!((vmds[0].bus, vmds[0].dev, vmds[0].func, vmds[0].class), (0, 0x0e, 0, 0x01_04_00));
    }

    #[test]
    fn the_id_decides_not_the_class_or_another_vendor() {
        assert!(is_vmd(0x8086, 0xad0b));
        assert!(!is_vmd(0x1022, 0xad0b), "another vendor's device with the same number");
        assert!(!is_vmd(0x8086, 0x2822), "an Intel RAID controller of class 0104 is not VMD");
        assert!(!is_vmd(0x144d, 0xa80a), "an NVMe controller");
    }

    #[test]
    fn a_32_mib_window_is_32_buses() {
        assert_eq!(buses_behind(32 << 20), 32);
        assert_eq!(buses_behind(0), 0);
        assert_eq!(buses_behind(u64::MAX), 256);
    }
}
