//! What is known about Georg's Gaming-PC (ASRock Z890 Taichi OCF, Core Ultra 7 265K), typed from read-only looks at
//! the running CachyOS (`plan/hw/gaming-pc-265k-2026-11-05/`, sysfs and the journal on 14.11.2026). No serial number,
//! no MAC address, no BAR address: addresses moved between the two looks, so nothing may depend on them.
//!
//! The table is a fixture for tests and the "expected" side of the report page. Kernel behaviour is never keyed by a
//! bus number from here: the CPU side renumbers when a card or the BIOS setup changes (5.11.: two graphics cards on
//! buses 02-04 and 05-07, Thunderbolt ports on 08 and 09, no Intel graphics; 14.11.: one card, the Thunderbolt ports
//! on 05-2e and 2f-58, Intel graphics on). The chipset side (80-86) was the same both times.

/// One PCI function as `lspci -n` shows it. `class` is class << 16 | subclass << 8 | prog-if.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Function {
    pub bus: u8,
    pub dev: u8,
    pub func: u8,
    pub vendor: u16,
    pub device: u16,
    pub class: u32,
    /// The bridge's secondary and subordinate bus (0, 0 for everything else).
    pub below: (u8, u8),
    pub what: &'static str,
}

const fn f(bus: u8, dev: u8, func: u8, vendor: u16, device: u16, class: u32, what: &'static str) -> Function {
    Function { bus, dev, func, vendor, device, class, below: (0, 0), what }
}

const fn b(bus: u8, dev: u8, func: u8, vendor: u16, device: u16, below: (u8, u8), what: &'static str) -> Function {
    Function { bus, dev, func, vendor, device, class: 0x06_04_00, below, what }
}

/// Every function of the look on 14.11.2026, in `lspci` order.
pub const FUNCTIONS: &[Function] = &[
    f(0x00, 0x00, 0, 0x8086, 0x7d1b, 0x06_00_00, "host bridge"),
    b(0x00, 0x01, 0, 0x8086, 0x7ecc, (0x01, 0x01), "root port to the Samsung 990 EVO Plus"),
    f(0x00, 0x02, 0, 0x8086, 0x7d67, 0x03_00_00, "Intel graphics (not the boot display)"),
    f(0x00, 0x04, 0, 0x8086, 0xad03, 0x11_80_00, "thermal tuning"),
    b(0x00, 0x06, 0, 0x8086, 0xae4d, (0x02, 0x04), "root port to graphics card A"),
    b(0x00, 0x07, 0, 0x8086, 0x7ec4, (0x05, 0x2e), "Thunderbolt root port, nothing below"),
    b(0x00, 0x07, 1, 0x8086, 0x7ec5, (0x2f, 0x58), "Thunderbolt root port, nothing below"),
    f(0x00, 0x08, 0, 0x8086, 0xae4c, 0x08_80_00, "GNA"),
    f(0x00, 0x0b, 0, 0x8086, 0xad1d, 0x12_00_00, "NPU"),
    f(0x00, 0x0d, 0, 0x8086, 0x7ec0, 0x0c_03_30, "xHCI of the CPU (nothing plugged in)"),
    f(0x00, 0x0d, 2, 0x8086, 0x7ec2, 0x0c_03_40, "Thunderbolt/USB4 host interface"),
    f(0x00, 0x0e, 0, 0x8086, 0xad0b, 0x01_04_00, "VMD (no drive behind it)"),
    f(0x00, 0x14, 0, 0x8086, 0xae7f, 0x05_00_00, "shared SRAM"),
    f(0x00, 0x1f, 0, 0x8086, 0xae0d, 0x06_01_00, "ISA bridge"),
    f(0x00, 0x1f, 5, 0x8086, 0xae23, 0x0c_80_00, "SPI flash controller"),
    f(0x01, 0x00, 0, 0x144d, 0xa80d, 0x01_08_02, "NVMe Samsung 990 EVO Plus 2TB"),
    b(0x02, 0x00, 0, 0x1002, 0x1478, (0x03, 0x04), "card A switch, upstream"),
    b(0x03, 0x00, 0, 0x1002, 0x1479, (0x04, 0x04), "card A switch, downstream"),
    f(0x04, 0x00, 0, 0x1002, 0x7550, 0x03_00_00, "RX 9070 XT, card A (boot display)"),
    f(0x04, 0x00, 1, 0x1002, 0xab40, 0x04_03_00, "card A HDMI/DP audio"),
    f(0x80, 0x14, 0, 0x8086, 0x7f6e, 0x0c_03_30, "xHCI of the chipset (keyboard, mouse, headset, stick)"),
    f(0x80, 0x14, 5, 0x8086, 0x7f2f, 0x00_00_00, "chipset function without a class"),
    f(0x80, 0x15, 0, 0x8086, 0x7f4c, 0x0c_80_00, "I2C 0 (BAR unassigned)"),
    f(0x80, 0x15, 2, 0x8086, 0x7f4e, 0x0c_80_00, "I2C 2 (BAR unassigned)"),
    f(0x80, 0x16, 0, 0x8086, 0x7f68, 0x07_80_00, "management engine interface"),
    b(0x80, 0x1b, 0, 0x8086, 0x7f40, (0x81, 0x81), "root port to the Lexar"),
    b(0x80, 0x1c, 0, 0x8086, 0x7f38, (0x82, 0x82), "root port to SATA 1"),
    b(0x80, 0x1c, 1, 0x8086, 0x7f39, (0x83, 0x83), "root port to the network card"),
    b(0x80, 0x1c, 2, 0x8086, 0x7f3a, (0x84, 0x84), "root port to SATA 2"),
    b(0x80, 0x1c, 3, 0x8086, 0x7f3b, (0x85, 0x85), "root port to the WLAN card"),
    b(0x80, 0x1d, 0, 0x8086, 0x7f30, (0x86, 0x86), "root port to the Samsung 980 PRO"),
    f(0x80, 0x1f, 0, 0x8086, 0x7f04, 0x06_01_00, "eSPI bridge"),
    f(0x80, 0x1f, 3, 0x8086, 0x7f50, 0x04_03_00, "chipset audio (no codec)"),
    f(0x80, 0x1f, 4, 0x8086, 0x7f23, 0x0c_05_00, "SMBus"),
    f(0x80, 0x1f, 5, 0x8086, 0x7f24, 0x0c_80_00, "SPI flash controller"),
    f(0x81, 0x00, 0, 0x1d97, 0x5236, 0x01_08_02, "NVMe Lexar NQ790 2TB"),
    f(0x82, 0x00, 0, 0x1b21, 0x0612, 0x01_06_01, "SATA ASM1061/62 (no disk)"),
    f(0x83, 0x00, 0, 0x10ec, 0x8126, 0x02_00_00, "network RTL8126"),
    f(0x84, 0x00, 0, 0x1b21, 0x0612, 0x01_06_01, "SATA ASM1061/62 (no disk)"),
    f(0x85, 0x00, 0, 0x8086, 0x272b, 0x02_80_00, "WLAN BE200"),
    f(0x86, 0x00, 0, 0x144d, 0xa80a, 0x01_08_02, "NVMe Samsung 980 PRO 1TB"),
];

/// The second graphics card as the look of 5.11.2026 had it (root port 00:06.3, buses 05-07); out on 14.11.
pub const CARD_B_2026_11_05: &[Function] = &[
    b(0x00, 0x06, 3, 0x8086, 0xae4e, (0x05, 0x07), "root port to graphics card B"),
    b(0x05, 0x00, 0, 0x1002, 0x1478, (0x06, 0x07), "card B switch, upstream"),
    b(0x06, 0x00, 0, 0x1002, 0x1479, (0x07, 0x07), "card B switch, downstream"),
    f(0x07, 0x00, 0, 0x1002, 0x7550, 0x03_00_00, "RX 9070 XT, card B"),
    f(0x07, 0x00, 1, 0x1002, 0xab40, 0x04_03_00, "card B HDMI/DP audio"),
];

/// A root bus as the firmware's ACPI names it (Linux journal: "PCI Root Bridge [PC00] (domain 0000 [bus 00-7f])").
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RootBus {
    pub name: &'static str,
    pub first: u8,
    pub last: u8,
    /// The lowest device number that answers on the root bus (bus 80 has no device 0).
    pub first_device: u8,
}

pub const ROOTS: &[RootBus] =
    &[RootBus { name: "PC00", first: 0x00, last: 0x7f, first_device: 0x00 }, RootBus { name: "PC02", first: 0x80, last: 0xdf, first_device: 0x14 }];

/// The one MCFG entry: buses 00-df (e0-ff belong to VMD and are not in it).
pub const ECAM_BASE: u64 = 0xc000_0000;
pub const ECAM_FIRST_BUS: u8 = 0x00;
pub const ECAM_LAST_BUS: u8 = 0xdf;

/// APIC ids of the 20 enabled CPUs in MADT order of their uid (8 P cores, 12 E cores).
pub const P_CORE_APIC_IDS: [u32; 8] = [0, 8, 32, 40, 48, 56, 80, 88];
pub const E_CORE_APIC_IDS: [u32; 12] = [24, 26, 28, 30, 64, 66, 68, 70, 72, 74, 76, 78];
/// MADT slots with APIC id 0xFFFFFFFF and flags 0 (uid 0x14-0x2f).
pub const MADT_EMPTY_SLOTS: usize = 28;

pub fn apic_ids() -> impl Iterator<Item = u32> {
    P_CORE_APIC_IDS.into_iter().chain(E_CORE_APIC_IDS)
}

/// CPUID 0x15: 38.4 MHz crystal x 202 / 2.
pub const TSC_HZ: u64 = 3_878_400_000;
/// ACPI PM timer I/O port (journal "PM-Timer IO Port: 0x1808").
pub const PM_TIMER_PORT: u16 = 0x1808;
/// CPUID 0x80000008: physical address bits.
pub const PHYS_ADDR_BITS: u32 = 46;

/// The picture Linux's simple-framebuffer took over from the firmware (started through rEFInd).
pub const GOP_WIDTH: u32 = 2560;
pub const GOP_HEIGHT: u32 = 1440;
pub const GOP_BYTES_PER_PIXEL: u32 = 4;
pub const GOP_STRIDE_BYTES: u32 = 10_240;
/// Card A's BAR0 is 16 GiB and sat above 512 GiB both times (0xa8_0000_0000 on 5.11., 0xa0_0000_0000 on 14.11.).
pub const CARD_A_BAR0_SIZE: u64 = 16 << 30;
pub const CARD_A_BAR0_SEEN: [u64; 2] = [0xa8_0000_0000, 0xa0_0000_0000];

/// An internal disk by its place, with the model only.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Disk {
    pub bus: u8,
    pub model: &'static str,
    pub holds: &'static str,
}

pub const DISKS: &[Disk] = &[
    Disk { bus: 0x01, model: "Samsung SSD 990 EVO Plus 2TB", holds: "Windows" },
    Disk { bus: 0x81, model: "Lexar SSD NQ790 2TB", holds: "data (ext4)" },
    Disk { bus: 0x86, model: "Samsung SSD 980 PRO 1TB", holds: "CachyOS" },
];

/// The function at a place, if the table has one.
pub fn function(bus: u8, dev: u8, func: u8) -> Option<&'static Function> {
    FUNCTIONS.iter().find(|x| x.bus == bus && x.dev == dev && x.func == func)
}

/// The root bus a bus number belongs to.
pub fn root_of(bus: u8) -> Option<&'static RootBus> {
    ROOTS.iter().find(|r| (r.first..=r.last).contains(&bus))
}

#[cfg(test)]
mod tests {
    use super::*;
    extern crate std;
    use std::collections::BTreeSet;

    #[test]
    fn the_table_has_no_function_twice_and_is_in_lspci_order() {
        let places: std::vec::Vec<(u8, u8, u8)> = FUNCTIONS.iter().map(|x| (x.bus, x.dev, x.func)).collect();
        let mut sorted = places.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(places, sorted);
        assert_eq!(FUNCTIONS.len(), 41);
    }

    #[test]
    fn every_bus_but_the_two_roots_hangs_below_exactly_one_bridge() {
        let buses: BTreeSet<u8> = FUNCTIONS.iter().map(|x| x.bus).collect();
        for bus in buses {
            let above = FUNCTIONS.iter().filter(|x| x.class == 0x06_04_00 && x.below.0 == bus).count();
            let root = ROOTS.iter().any(|r| r.first == bus);
            assert_eq!(above, usize::from(!root), "bus {bus:02x}");
        }
    }

    #[test]
    fn bridges_forward_inside_their_own_root_and_above_their_own_bus() {
        for x in FUNCTIONS.iter().chain(CARD_B_2026_11_05).filter(|x| x.class == 0x06_04_00) {
            let r = root_of(x.bus).unwrap();
            assert!(x.below.0 > x.bus && x.below.0 <= x.below.1 && x.below.1 <= r.last, "{x:?}");
        }
        // No bridge of the first root reaches the second: only a cut finds bus 80.
        assert!(FUNCTIONS.iter().filter(|x| x.bus < 0x80).all(|x| x.below.1 < 0x80));
    }

    #[test]
    fn bus_80_has_no_device_0_and_starts_at_device_14() {
        assert!(function(0x80, 0, 0).is_none());
        let first = FUNCTIONS.iter().filter(|x| x.bus == 0x80).map(|x| x.dev).min();
        assert_eq!(first, Some(ROOTS[1].first_device));
        assert_eq!(FUNCTIONS.iter().filter(|x| x.bus == 0).map(|x| x.dev).min(), Some(ROOTS[0].first_device));
    }

    #[test]
    fn the_roots_tile_the_ecam_range() {
        assert_eq!(ROOTS[0].first, ECAM_FIRST_BUS);
        assert_eq!(ROOTS[0].last + 1, ROOTS[1].first);
        assert_eq!(ROOTS[1].last, ECAM_LAST_BUS);
        assert!(FUNCTIONS.iter().all(|x| root_of(x.bus).is_some()));
    }

    #[test]
    fn twenty_cpus_with_distinct_apic_ids() {
        let ids: BTreeSet<u32> = apic_ids().collect();
        assert_eq!(ids.len(), 20);
        assert!(ids.iter().all(|&i| i < 0xff));
    }

    #[test]
    fn the_clock_and_picture_numbers_agree_with_each_other() {
        assert_eq!(38_400_000u64 * 202 / 2, TSC_HZ);
        assert_eq!(GOP_WIDTH * GOP_BYTES_PER_PIXEL, GOP_STRIDE_BYTES);
        for base in CARD_A_BAR0_SEEN {
            assert!(base >= 512 << 30, "the BAR sits above 512 GiB");
            assert!(base + CARD_A_BAR0_SIZE <= 1 << PHYS_ADDR_BITS);
            assert_eq!(base % CARD_A_BAR0_SIZE, 0);
        }
    }

    #[test]
    fn no_ehci_controller_so_intels_port_routing_has_nothing_to_do() {
        // linux-7.3.1 drivers/usb/host/pci-quirks.c `usb_enable_intel_xhci_ports` returns before its first write
        // unless an Intel EHCI (class 0c0320) exists. Both USB controllers here are xHCI (0c0330).
        assert!(!FUNCTIONS.iter().chain(CARD_B_2026_11_05).any(|x| x.class == 0x0c_03_20));
        let xhci: std::vec::Vec<_> = FUNCTIONS.iter().filter(|x| x.class == 0x0c_03_30).map(|x| (x.bus, x.dev, x.func, x.device)).collect();
        assert_eq!(xhci, [(0x00, 0x0d, 0, 0x7ec0), (0x80, 0x14, 0, 0x7f6e)]);
    }

    #[test]
    fn three_disks_each_at_an_nvme_function() {
        for d in DISKS {
            assert_eq!(function(d.bus, 0, 0).map(|x| x.class), Some(0x01_08_02), "{}", d.model);
        }
        assert_eq!(FUNCTIONS.iter().filter(|x| x.class == 0x01_08_02).count(), DISKS.len());
    }
}
