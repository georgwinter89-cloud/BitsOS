//! The link limits Linux hands the power controller with OverridePcieParameters, from configuration space reads
//! alone: `amdgpu_device_get_pcie_info` (`A:` amdgpu/amdgpu_device.c:5911-6052, called at :3938 before any block
//! starts), `pcie_bandwidth_available` (`L:` drivers/pci/pci.c:6030-6070), `smu_smc_hw_setup`
//! (`S:` pm/swsmu/amdgpu_smu.c:1815-1843) and `smu_v14_0_2_update_pcie_parameters`
//! (`T:` pm/swsmu/smu14/smu_v14_0_2_ppt.c:1411-1489).
//!
//! On the Gaming-PC Linux does not let the PMFW change the link's speed by itself: the host is an Intel CPU, for
//! which `amdgpu_device_pcie_dynamic_switching_supported` says no (A:1279-1309) and PP_PCIE_DPM_MASK is cleared
//! (A:2087-2088). Linux then takes the link as it runs NOW (the slowest link between the card and the root port)
//! as the limit and forces every level of the PMFW's table to it. That path is built here. The other one (a host
//! where Linux leaves the switching on: the limits are the capabilities of the port above the card's own switch,
//! and only levels above them are overridden) is `Host::Switching`, which the stage refuses: no trace shows it.

use crate::pptable14::{LINK_LEVELS, PcieLevels};

/// `PCI_EXP_LNKSTA` of one function: the current speed code (bits 3:0) and the negotiated width (bits 9:4).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LinkNow {
    pub speed: u8,
    pub width: u8,
}

/// `PCIE_SPEED2MBS_ENC` (drivers/pci/pci.h:615-622) of a Link Status speed code, through `pcie_link_speed[]`
/// (drivers/pci/probe.c:756-773): 1 is 2.5 GT/s, 6 is 64 GT/s, every other code is unknown and counts 0.
pub const fn mbs(speed: u8) -> u32 {
    match speed {
        1 => 2500 * 8 / 10,
        2 => 5000 * 8 / 10,
        3 => 8000 * 128 / 130,
        4 => 16000 * 128 / 130,
        5 => 32000 * 128 / 130,
        6 => 64000,
        _ => 0,
    }
}

/// `pcie_bandwidth_available` (L:6030-6070) over `path`, the card first and then every bridge up to the root
/// port: the link with the least width times speed; of equals the one nearest the root (`next_bw <= bw`), and a
/// link that counts 0 is replaced by the next (`!bw`). `None` for an empty path.
pub fn limiting(path: &[LinkNow]) -> Option<LinkNow> {
    let mut bw = 0u32;
    let mut at = None;
    for l in path {
        let next = u32::from(l.width) * mbs(l.speed);
        if bw == 0 || next <= bw {
            (bw, at) = (next, Some(*l));
        }
    }
    at
}

/// The PMFW's code for the platform's link speed: the highest `CAIL_PCIE_LINK_SPEED_SUPPORT_GENn` Linux sets for
/// the limiting link's speed (A:5966-5992), as `smu_smc_hw_setup` reads it (S:1815-1824): 0 Gen1 to 4 Gen5. An
/// unknown speed gives Gen1 and Gen2; so does 64 GT/s give Gen1 alone, as the source's last `else` has it.
pub const fn gen_cap(limit: Option<LinkNow>) -> u8 {
    match limit {
        None => 1,
        Some(l) => match l.speed {
            5 => 4,
            4 => 3,
            3 => 2,
            2 => 1,
            1 | 6 => 0,
            _ => 1,
        },
    }
}

/// The PMFW's code for the platform's link width (A:6044-6100, S:1830-1843): 1 x1, 2 x2, 3 x4, 4 x8, 5 x12, 6 x16,
/// 7 x32. No link at all gives Linux's default mask, whose widest is x16; a width that is none of these sets no
/// bit, and the code stays 0.
pub const fn width_cap(limit: Option<LinkNow>) -> u8 {
    match limit {
        None => 6,
        Some(l) => match l.width {
            32 => 7,
            16 => 6,
            12 => 5,
            8 => 4,
            4 => 3,
            2 => 2,
            1 => 1,
            _ => 0,
        },
    }
}

/// `AMDGPU_DEFAULT_PCIE_GEN_MASK` and `AMDGPU_DEFAULT_PCIE_MLW_MASK` as codes (include/amd_pcie.h:45-49, :74-78):
/// what a card directly on the root bus gets (A:5922-5928).
pub const ROOT_BUS_CAPS: (u8, u8) = (1, 6);

/// (generation code, width code) for a card whose path to the root port reads as `path` (the card's own link
/// first). A path of the card alone is a card on the root bus.
pub fn caps(path: &[LinkNow]) -> (u8, u8) {
    if path.len() <= 1 {
        return ROOT_BUS_CAPS;
    }
    let limit = limiting(path);
    (gen_cap(limit), width_cap(limit))
}

/// Whether Linux lets the PMFW switch the link's speed on this host.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Host {
    /// `amdgpu_pcie_dynamic_switching_quirks` matches (A:1279-1288): every Intel CPU, and AMD family 0x17 model
    /// 0x08. The limits are the links as they run now, and every level is forced to them.
    Fixed,
    /// Any other host: not built (the module's header).
    Switching,
}

/// The quirk table over CPUID: `vendor` is leaf 0's EBX, EDX, ECX, `signature` leaf 1's EAX. Linux's first test,
/// a removable device (an external GPU, A:1301-1302), is not asked: BitsOS's PCI scan does not know removable
/// ports, and this card sits in a slot.
pub fn host(vendor: [u32; 3], signature: u32) -> Host {
    let name = |s: &[u8; 12]| [u32::from_le_bytes([s[0], s[1], s[2], s[3]]), u32::from_le_bytes([s[4], s[5], s[6], s[7]]), u32::from_le_bytes([s[8], s[9], s[10], s[11]])];
    // arch/x86/lib/cpu.c `x86_family`, `x86_model`.
    let mut family = (signature >> 8) & 0xF;
    if family == 0xF {
        family += (signature >> 20) & 0xFF;
    }
    let mut model = (signature >> 4) & 0xF;
    if family >= 6 {
        model |= ((signature >> 16) & 0xF) << 4;
    }
    if vendor == name(b"GenuineIntel") || (vendor == name(b"AuthenticAMD") && family == 0x17 && model == 0x08) {
        Host::Fixed
    } else {
        Host::Switching
    }
}

/// OverridePcieParameters' arguments on a `Host::Fixed` host (T:1446-1467): the levels of the table that are not
/// all zero, each forced to the lesser of the table's last level and the platform's limit; bits 31:16 the level,
/// 15:8 the generation code, 7:0 the width code. The arguments in order and how many there are.
pub fn forced(table: &PcieLevels, gen_cap: u8, width_cap: u8) -> ([u32; LINK_LEVELS], usize) {
    let mut levels = [(0u8, 0u8); LINK_LEVELS];
    let mut n = 0;
    for i in 0..LINK_LEVELS {
        if table.gen_speed[i] == 0 && table.lane_count[i] == 0 && table.lclk_freq[i] == 0 {
            continue;
        }
        levels[n] = (table.gen_speed[i], table.lane_count[i]);
        n += 1;
    }
    let mut args = [0u32; LINK_LEVELS];
    if n == 0 {
        return (args, 0);
    }
    let (last_gen, last_lane) = levels[n - 1];
    let (generation, lane) = (last_gen.min(gen_cap), last_lane.min(width_cap));
    for (i, a) in args.iter_mut().enumerate().take(n) {
        *a = (i as u32) << 16 | u32::from(generation) << 8 | u32::from(lane);
    }
    (args, n)
}
