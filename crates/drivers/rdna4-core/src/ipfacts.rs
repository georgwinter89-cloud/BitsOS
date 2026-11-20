//! The IP discovery table of the Gaming-PC's two cards, read from
//! `/sys/bus/pci/devices/0000:04:00.0/ip_discovery/die/0` on 5.11. under Linux 7.3.1. Card B (07:00.0) gave the same 46
//! entries, compared field by field. `harvest` read 0 on every entry, so the table has no column for it. This is what
//! Linux reports after parsing, not the blob's bytes: the blob itself needs root to read, and its size is unverified.
//!
//! Apart from `crates/drivers/amdgpu`, whose `ipmod` answers the same question for the running kernel: nothing here is
//! linked to it, so the 780M's paths cannot notice this table.

use core::fmt;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IpVersion {
    pub major: u8,
    pub minor: u8,
    pub rev: u8,
}

impl fmt::Display for IpVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.rev)
    }
}

pub const fn v(major: u8, minor: u8, rev: u8) -> IpVersion {
    IpVersion { major, minor, rev }
}

/// Hardware ids, `include/soc15_hw_ip.h`. 273, 282, 286 and 293 are in the table but named nowhere in that header, so
/// they stay numbers.
pub mod hw {
    pub const MP1: u16 = 1;
    pub const THM: u16 = 3;
    pub const SMUIO: u16 = 4;
    pub const FUSE: u16 = 5;
    pub const CLKA: u16 = 6;
    pub const PWR: u16 = 10;
    pub const GC: u16 = 11;
    /// UVD and VCN share the number.
    pub const UVD: u16 = 12;
    pub const MMHUB: u16 = 34;
    pub const ATHUB: u16 = 35;
    pub const DBGU_NBIO: u16 = 36;
    pub const DFX: u16 = 37;
    pub const OSSSYS: u16 = 40;
    pub const HDP: u16 = 41;
    pub const SDMA0: u16 = 42;
    pub const SDMA1: u16 = 43;
    pub const DF: u16 = 46;
    pub const DFX_DAP: u16 = 49;
    pub const PCIE: u16 = 70;
    pub const LSDMA: u16 = 91;
    pub const NBIF: u16 = 108;
    pub const SYSTEMHUB: u16 = 128;
    pub const UMC: u16 = 150;
    pub const MP0: u16 = 255;
    pub const DMU: u16 = 271;
    pub const DIO: u16 = 272;
    pub const DAZ: u16 = 274;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Entry {
    pub hw_id: u16,
    pub instance: u8,
    pub version: IpVersion,
    /// Register bases in dwords, by base index.
    pub bases: &'static [u32],
}

const fn e(hw_id: u16, instance: u8, ver: (u8, u8, u8), bases: &'static [u32]) -> Entry {
    Entry { hw_id, instance, version: v(ver.0, ver.1, ver.2), bases }
}

/// In order of hardware id, then instance.
pub static TABLE: [Entry; 46] = [
    e(
        hw::MP1,
        0,
        (14, 0, 3),
        &[
            0x16000, 0x16200, 0x1CE00, 0xDC0000, 0xE00000, 0xE40000, 0xE80000, 0xEC0000, 0xF00000, 0x2400400,
            0x243FC00, 0x244D400, 0x3200000, 0x3240000, 0x3280000,
        ],
    ),
    e(hw::THM, 0, (14, 0, 2), &[0x16600, 0x2400C00]),
    e(hw::SMUIO, 0, (14, 0, 2), &[0x16800, 0x16A00, 0x2401000, 0x3440000]),
    e(hw::FUSE, 0, (14, 0, 2), &[0x17400, 0x2401400]),
    e(hw::CLKA, 0, (14, 0, 2), &[0x16C00, 0x2401800]),
    e(hw::CLKA, 1, (14, 0, 2), &[0x16E00, 0x2401C00]),
    e(hw::CLKA, 2, (14, 0, 2), &[0x17000, 0x2402000]),
    e(hw::CLKA, 3, (14, 0, 2), &[0x17200, 0x2402400]),
    e(hw::CLKA, 4, (14, 0, 2), &[0x1B000, 0x242D800]),
    e(hw::CLKA, 5, (14, 0, 2), &[0x1B200, 0x242DC00]),
    e(hw::CLKA, 6, (14, 0, 2), &[0x1B400, 0x242E000]),
    e(hw::CLKA, 7, (14, 0, 2), &[0x1B600, 0x242E400]),
    e(hw::CLKA, 8, (14, 0, 2), &[0x1BC00, 0x2433C00]),
    e(hw::PWR, 0, (9, 0, 0), &[]),
    e(hw::GC, 0, (12, 0, 1), &[0x1260, 0xA000, 0x1C000, 0x2402C00]),
    e(hw::UVD, 0, (5, 0, 0), &[0x7800, 0x7E00]),
    e(hw::MMHUB, 0, (4, 1, 0), &[0x1A000, 0x2408800]),
    e(hw::ATHUB, 0, (4, 1, 0), &[0xC00, 0x2408C00]),
    e(hw::DBGU_NBIO, 0, (3, 0, 0), &[0x1C0, 0x2409000]),
    e(hw::DFX, 0, (5, 0, 0), &[0x580, 0x2409400]),
    e(hw::OSSSYS, 0, (7, 0, 0), &[0x10A0, 0x240A000]),
    e(hw::HDP, 0, (7, 0, 0), &[0xF20, 0x240A400]),
    e(hw::SDMA0, 0, (7, 0, 1), &[0x1260, 0xA000, 0x1C000, 0x2402C00]),
    e(hw::SDMA1, 0, (7, 0, 1), &[0x1260, 0xA000, 0x1C000, 0x2402C00]),
    e(hw::DF, 0, (4, 15, 1), &[0x7000, 0xC00000, 0x240B800, 0x7C00000, 0x12400000]),
    e(hw::DFX_DAP, 0, (2, 0, 0), &[0x5A0, 0xB80000, 0x240C400]),
    e(hw::PCIE, 0, (6, 1, 0), &[0x2411800, 0x68C0000, 0x20000000]),
    e(hw::LSDMA, 0, (7, 0, 1), &[0x11400, 0x2456800]),
    e(hw::NBIF, 0, (6, 3, 1), &[0x0, 0x14, 0xD20, 0x10400, 0x241B000, 0x4040000]),
    e(hw::SYSTEMHUB, 0, (2, 3, 0), &[0xEA0, 0x500000, 0x2420000]),
    e(hw::UMC, 0, (8, 14, 0), &[0x14000, 0x2D6000, 0x2425800]),
    e(hw::UMC, 1, (8, 14, 0), &[0x54000, 0x316000, 0x2425C00]),
    e(hw::UMC, 2, (8, 14, 0), &[0x94000, 0x356000, 0x2426000]),
    e(hw::UMC, 3, (8, 14, 0), &[0xD4000, 0x396000, 0x2426400]),
    e(hw::UMC, 4, (8, 14, 0), &[0x114000, 0x3D6000, 0x2426800]),
    e(hw::UMC, 5, (8, 14, 0), &[0x154000, 0x680000, 0x2426C00]),
    e(hw::UMC, 6, (8, 14, 0), &[0x194000, 0x6C0000, 0x2427000]),
    e(hw::UMC, 7, (8, 14, 0), &[0x1D4000, 0x700000, 0x2427400]),
    e(
        hw::MP0,
        0,
        (14, 0, 3),
        &[
            0x16000, 0x16200, 0x1CE00, 0xDC0000, 0xE00000, 0xE40000, 0xE80000, 0xEC0000, 0xF00000, 0x2400400,
            0x243FC00, 0x244D400, 0x3200000, 0x3240000, 0x3280000,
        ],
    ),
    e(hw::DMU, 0, (4, 1, 0), &[0x12, 0xC0, 0x34C0, 0x9000, 0x2403C00]),
    e(hw::DIO, 0, (127, 127, 63), &[0x2404000]),
    e(273, 0, (1, 0, 3), &[0x2458400, 0x7E00300]),
    e(hw::DAZ, 0, (127, 127, 63), &[0x4C0000, 0x2404800]),
    e(282, 0, (14, 0, 2), &[0xBC0000]),
    e(286, 0, (5, 0, 0), &[0x2403000]),
    e(293, 0, (1, 0, 0), &[0x2458000, 0x7E00200]),
];

pub fn find(table: &[Entry], hw_id: u16, instance: u8) -> Option<&Entry> {
    table.iter().find(|e| e.hw_id == hw_id && e.instance == instance)
}

/// The same twelve roles as `amdgpu::ipmod::Role`, in the same order, each with the IP that picks its module.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Psp,
    Smu,
    Nbio,
    Gmc,
    Gfxhub,
    Mmhub,
    Ih,
    Gfx,
    Mes,
    Sdma,
    Display,
    Vcn,
}

impl Role {
    pub const ALL: [Role; 12] = [
        Role::Psp,
        Role::Smu,
        Role::Nbio,
        Role::Gmc,
        Role::Gfxhub,
        Role::Mmhub,
        Role::Ih,
        Role::Gfx,
        Role::Mes,
        Role::Sdma,
        Role::Display,
        Role::Vcn,
    ];

    pub const fn key(self) -> u16 {
        match self {
            Role::Gfx | Role::Mes | Role::Gmc | Role::Gfxhub => hw::GC,
            Role::Sdma => hw::SDMA0,
            Role::Psp => hw::MP0,
            Role::Smu => hw::MP1,
            Role::Display => hw::DMU,
            Role::Vcn => hw::UVD,
            Role::Ih => hw::OSSSYS,
            Role::Mmhub => hw::MMHUB,
            Role::Nbio => hw::NBIF,
        }
    }

    /// The Linux 7.3.1 block that serves this card's version; the reference for whoever builds the module.
    pub const fn linux_block(self) -> &'static str {
        match self {
            Role::Psp => "psp_v14_0",
            Role::Smu => "smu_v14_0",
            Role::Nbio => "nbif_v6_3_1",
            Role::Gmc => "gmc_v12_0",
            Role::Gfxhub => "gfxhub_v12_0",
            Role::Mmhub => "mmhub_v4_1_0",
            Role::Ih => "ih_v7_0",
            Role::Gfx => "gfx_v12_0",
            Role::Mes => "mes_v12_0",
            Role::Sdma => "sdma_v7_0",
            Role::Display => "dcn401, sources not in the reference tree",
            Role::Vcn => "vcn_v5_0_0",
        }
    }
}

/// A module someone has built, for this role and exactly this version.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Module {
    pub role: Role,
    pub version: IpVersion,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Missing {
    pub role: Role,
    pub version: IpVersion,
}

/// Roles the table asks for and no module serves. A neighbouring version never counts; an IP the table lacks needs no
/// module (a headless card has no DMU).
pub fn missing_roles<'a>(table: &'a [Entry], built: &'a [Module]) -> impl Iterator<Item = Missing> + 'a {
    Role::ALL.iter().filter_map(move |&role| {
        let version = find(table, role.key(), 0)?.version;
        let served = built.iter().any(|m| m.role == role && m.version == version);
        (!served).then_some(Missing { role, version })
    })
}

/// `amdgpu_discovery.c:2688-2690`: this card reports DMU 4.1.0 and Linux's DC code runs it as 4.0.1, which is why the
/// firmware is `dcn_4_0_1_dmcub`.
pub const fn display_version(reported: IpVersion) -> IpVersion {
    if reported.major == 4 && reported.minor == 1 && reported.rev == 0 { v(4, 0, 1) } else { reported }
}

/// Linux's blocks on this card in init order, as its journal printed them on 5.11. ("detected ip block number").
pub const LINUX_BLOCKS: [&str; 11] = [
    "soc24_common",
    "gmc_v12_0",
    "ih_v7_0",
    "psp",
    "smu",
    "dm",
    "gfx_v12_0",
    "sdma_v7_0",
    "vcn_v5_0_0",
    "jpeg_v5_0_0",
    "mes_v12_0",
];
