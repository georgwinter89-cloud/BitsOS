//! Functions of the Gaming-PC that BitsOS must leave alone (plan/spec/gaming-pc.md §2): no BAR sizing, no
//! command-register write, no driver. The PCI core applies it through its existing "untouched" state, under the
//! `gaming-pc` profile only. Every id here is absent on the Mini-PC (its device list has no
//! 8086:7fxx/aexx/7ecx/ad0b/272b and no 1002:ab40; the test below walks that list), so even a mistaken call would
//! change nothing there.
//!
//! Matching is by vendor and device id. The class is only checked to make sure the id still names the function this
//! list was written for; an id with another class is `Normal` and shows up in the device list like anything else.

use crate::vmd;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Why {
    /// SPI flash controller: a stray write could damage the firmware chip.
    Flash,
    /// Intel VMD: touching it can hide or reset drives mapped under it.
    Vmd,
    /// Thunderbolt/USB4 host interface: a path for DMA from a plugged-in device.
    Thunderbolt,
    /// Management engine interface.
    ManagementEngine,
    /// The chipset's HD Audio function: no codec hangs on it, and its reset needs a clock step BitsOS lacks.
    ChipsetAudio,
    /// The graphics cards' HDMI/DP audio: not before the card itself is driven.
    GraphicsAudio,
    /// The WLAN card: this PC is always on the cable (Georg, 5.11.2026).
    Wlan,
}

impl Why {
    /// The name the PCI core stores as the function's untouched key. None of them is a CONFIG key anyone reads as
    /// a switch: the `gp_` prefix keeps them apart from the disk guard's keys and the test keys.
    pub fn key(self) -> &'static str {
        match self {
            Why::Flash => "gp_flash",
            Why::Vmd => "gp_vmd",
            Why::Thunderbolt => "gp_thunderbolt",
            Why::ManagementEngine => "gp_mei",
            Why::ChipsetAudio => "gp_audio",
            Why::GraphicsAudio => "gp_gpu_audio",
            Why::Wlan => "gp_wlan",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Never touched, listed like any untouched function.
    Never(Why),
    /// Never touched, and the report says so in a line of its own (VMD: a drive under it would be invisible).
    ReportOnly(Why),
    Normal,
}

impl Verdict {
    pub fn why(self) -> Option<Why> {
        match self {
            Verdict::Never(w) | Verdict::ReportOnly(w) => Some(w),
            Verdict::Normal => None,
        }
    }
}

const INTEL: u16 = 0x8086;
const ATI: u16 = 0x1002;

/// (vendor, device, class << 8 | subclass, why). Ids from this machine's `lspci -nn`, with the place Linux 7.3.1
/// names the same id where it does.
const LIST: &[(u16, u16, u16, Why)] = &[
    // 80:1f.5: drivers/spi/spi-intel-pci.c:89.
    (INTEL, 0x7f24, 0x0c80, Why::Flash),
    // 00:1f.5: the CPU side's flash controller; Linux 7.3.1 has no driver entry for it.
    (INTEL, 0xae23, 0x0c80, Why::Flash),
    // 00:0d.2: PCI_DEVICE_ID_INTEL_MTL_P_NHI0, drivers/thunderbolt/nhi.h:102.
    (INTEL, 0x7ec2, 0x0c03, Why::Thunderbolt),
    // 80:16.0: PCI_DEVICE_ID_INTEL_MEI_ARL_S, drivers/misc/mei/hw-me-regs.h:117.
    (INTEL, 0x7f68, 0x0780, Why::ManagementEngine),
    // 80:1f.3: PCI_DEVICE_ID_INTEL_HDA_ARL_S, include/linux/pci_ids.h:3128.
    (INTEL, 0x7f50, 0x0403, Why::ChipsetAudio),
    // 04:00.1 (and 07:00.1 with the second card in): sound/hda/controllers/intel.c:2767.
    (ATI, 0xab40, 0x0403, Why::GraphicsAudio),
    // 85:00.0: drivers/net/wireless/intel/iwlwifi/pcie/drv.c:506.
    (INTEL, 0x272b, 0x0280, Why::Wlan),
];

/// `class`: class << 16 | subclass << 8 | prog-if, as the PCI core keeps it.
pub fn classify(vendor: u16, device: u16, class: u32) -> Verdict {
    if vmd::is_vmd(vendor, device) {
        return Verdict::ReportOnly(Why::Vmd);
    }
    let sub = (class >> 8) as u16;
    match LIST.iter().find(|e| e.0 == vendor && e.1 == device && e.2 == sub) {
        Some(e) => Verdict::Never(e.3),
        None => Verdict::Normal,
    }
}

/// The untouched key for a function, if it is on the list.
pub fn key(vendor: u16, device: u16, class: u32) -> Option<&'static str> {
    classify(vendor, device, class).why().map(Why::key)
}

/// Every reason, in the order of the enum.
pub const WHYS: [Why; 7] = [Why::Flash, Why::Vmd, Why::Thunderbolt, Why::ManagementEngine, Why::ChipsetAudio, Why::GraphicsAudio, Why::Wlan];

/// True for a key this module hands out (the PCI core words the device line differently for these: held for
/// good, no switch). Exactly those keys: other waves have real switches that start with `gp_` too (`gp_net`,
/// `gp_stick`), and a function held until such a switch must not read as held for good.
pub fn is_key(key: &str) -> bool {
    WHYS.iter().any(|w| w.key() == key)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::facts;
    extern crate std;
    use std::vec::Vec;

    fn at(bus: u8, dev: u8, func: u8) -> Verdict {
        let x = facts::function(bus, dev, func).unwrap();
        classify(x.vendor, x.device, x.class)
    }

    #[test]
    fn the_listed_functions_of_this_machine() {
        assert_eq!(at(0x80, 0x1f, 5), Verdict::Never(Why::Flash));
        assert_eq!(at(0x00, 0x1f, 5), Verdict::Never(Why::Flash));
        assert_eq!(at(0x00, 0x0e, 0), Verdict::ReportOnly(Why::Vmd));
        assert_eq!(at(0x00, 0x0d, 2), Verdict::Never(Why::Thunderbolt));
        assert_eq!(at(0x80, 0x16, 0), Verdict::Never(Why::ManagementEngine));
        assert_eq!(at(0x80, 0x1f, 3), Verdict::Never(Why::ChipsetAudio));
        assert_eq!(at(0x04, 0x00, 1), Verdict::Never(Why::GraphicsAudio));
        assert_eq!(at(0x85, 0x00, 0), Verdict::Never(Why::Wlan));
        let b = &facts::CARD_B_2026_11_05[4];
        assert_eq!(classify(b.vendor, b.device, b.class), Verdict::Never(Why::GraphicsAudio));
    }

    #[test]
    fn exactly_eight_functions_of_today_and_nothing_bitsos_needs() {
        let listed: Vec<(u8, u8, u8)> =
            facts::FUNCTIONS.iter().filter(|x| classify(x.vendor, x.device, x.class) != Verdict::Normal).map(|x| (x.bus, x.dev, x.func)).collect();
        assert_eq!(listed, [(0x00, 0x0d, 2), (0x00, 0x0e, 0), (0x00, 0x1f, 5), (0x04, 0x00, 1), (0x80, 0x16, 0), (0x80, 0x1f, 3), (0x80, 0x1f, 5), (0x85, 0x00, 0)]);
        // What the first round must drive stays normal: both xHCIs, the network card, the graphics card, every
        // bridge and host bridge, and the disks (the disk guard owns those, not this list).
        for x in facts::FUNCTIONS {
            let needed = matches!(x.class, 0x0c_03_30 | 0x02_00_00 | 0x03_00_00 | 0x06_04_00 | 0x06_00_00 | 0x01_08_02 | 0x01_06_01);
            if needed {
                assert_eq!(classify(x.vendor, x.device, x.class), Verdict::Normal, "{x:?}");
            }
        }
    }

    #[test]
    fn a_bridge_is_never_on_the_list() {
        // An untouched bridge would be fine for the walk, but its bus-master bit is set on the way to a device.
        for e in LIST {
            assert_ne!(e.2 >> 8, 0x06, "{e:?}");
        }
        assert!(!vmd::DEVICE_IDS.iter().any(|&d| LIST.iter().any(|e| e.0 == INTEL && e.1 == d)));
    }

    #[test]
    fn the_id_must_come_with_its_class() {
        assert_eq!(classify(INTEL, 0x7f24, 0x0c_03_30), Verdict::Normal, "the flash id on an xHCI class is not the flash controller");
        assert_eq!(classify(INTEL, 0x7f24, 0x0c_80_00), Verdict::Never(Why::Flash));
        assert_eq!(classify(INTEL, 0x7f24, 0x0c_80_ff), Verdict::Never(Why::Flash), "the prog-if does not matter");
        assert_eq!(classify(0x1022, 0x7f24, 0x0c_80_00), Verdict::Normal, "another vendor");
    }

    #[test]
    fn the_mini_pc_and_qemu_have_none_of_them() {
        // Every id of the Mini-PC's device list (plan/hw/gx-780m-minipc-2026-10-30/c1/hw_info.txt), then QEMU q35's.
        let mini_pc: &[(u16, u16, u32)] = &[
            (0x1022, 0x14e8, 0x06_00_00),
            (0x1022, 0x14e9, 0x08_06_00),
            (0x1022, 0x14ea, 0x06_00_00),
            (0x1022, 0x14ed, 0x06_04_00),
            (0x1022, 0x14ee, 0x06_04_00),
            (0x1022, 0x14ef, 0x06_04_00),
            (0x1022, 0x14eb, 0x06_04_00),
            (0x1022, 0x790b, 0x0c_05_00),
            (0x1022, 0x790e, 0x06_01_00),
            (0x1022, 0x14f0, 0x06_00_00),
            (0x1022, 0x14f7, 0x06_00_00),
            (0x126f, 0x2268, 0x01_08_02),
            (0x8086, 0x125c, 0x02_00_00),
            (0x10ec, 0xb852, 0x02_80_00),
            (0x1d97, 0x5236, 0x01_08_02),
            (0x1002, 0x1900, 0x03_00_00),
            (0x1002, 0x1640, 0x04_03_00),
            (0x1022, 0x15c7, 0x10_80_00),
            (0x1022, 0x15b9, 0x0c_03_30),
            (0x1022, 0x15ba, 0x0c_03_30),
            (0x1022, 0x15e2, 0x04_80_00),
            (0x1022, 0x15e3, 0x04_03_00),
            (0x1022, 0x14ec, 0x13_00_00),
            (0x1022, 0x1502, 0x11_80_00),
            (0x1022, 0x15c0, 0x0c_03_30),
            (0x1022, 0x15c1, 0x0c_03_30),
            (0x1022, 0x1668, 0x0c_03_40),
            (0x1022, 0x1669, 0x0c_03_40),
        ];
        let qemu: &[(u16, u16, u32)] = &[
            (0x8086, 0x29c0, 0x06_00_00),
            (0x8086, 0x2918, 0x06_01_00),
            (0x8086, 0x2922, 0x01_06_01),
            (0x8086, 0x2930, 0x0c_05_00),
            (0x8086, 0x293e, 0x04_03_00),
            (0x8086, 0x2668, 0x04_03_00),
            (0x8086, 0x10c9, 0x02_00_00),
            (0x1b36, 0x000d, 0x0c_03_30),
            (0x1b36, 0x000c, 0x06_04_00),
            (0x1b36, 0x000b, 0x06_00_00),
            (0x1b36, 0x0010, 0x01_08_02),
            (0x1af4, 0x1041, 0x02_00_00),
            (0x1234, 0x1111, 0x03_00_00),
        ];
        for &(v, d, c) in mini_pc.iter().chain(qemu) {
            assert_eq!(classify(v, d, c), Verdict::Normal, "{v:04x}:{d:04x}");
        }
    }

    #[test]
    fn keys_are_distinct_marked_and_no_switch_name() {
        let whys = WHYS;
        // `WHYS` has every variant: this match stops compiling when one is added without a look at the list.
        for w in whys {
            match w {
                Why::Flash | Why::Vmd | Why::Thunderbolt | Why::ManagementEngine | Why::ChipsetAudio | Why::GraphicsAudio | Why::Wlan => {}
            }
        }
        let mut keys: Vec<&str> = whys.iter().map(|w| w.key()).collect();
        assert!(keys.iter().all(|k| is_key(k) && k.starts_with("gp_")));
        keys.sort_unstable();
        keys.dedup();
        assert_eq!(keys.len(), whys.len());
        // Real switches, GP-GUARD's `gp_net` and `gp_stick` among them: none is a never-touch key, so a function
        // held until one of them is never worded as held for good.
        for switch in ["nvme_write", "nvme_bind", "gp_net", "gp_stick", "gp_", "gp_flash2", ""] {
            assert!(!keys.contains(&switch));
            assert!(!is_key(switch), "{switch}");
        }
        assert_eq!(key(INTEL, 0xad0b, 0x01_04_00), Some("gp_vmd"));
        assert_eq!(key(INTEL, 0x7f6e, 0x0c_03_30), None);
    }
}
