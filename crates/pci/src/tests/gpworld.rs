//! The Gaming-PC as a configuration space, built from `gp_platform::facts` (the look of 14.11.2026): two root buses
//! in one ECAM range 00-df, root ports and the graphics card's switch as PCIe ports, the BARs that matter at the
//! addresses Linux showed.

use super::fake::{Fake, FakeFn};
use gp_platform::facts::{self, Function};

pub const XHCI_PCH: (u8, u8, u8) = (0x80, 0x14, 0);
pub const XHCI_CPU: (u8, u8, u8) = (0x00, 0x0d, 0);
pub const GPU_A: (u8, u8, u8) = (0x04, 0x00, 0);
pub const NIC: (u8, u8, u8) = (0x83, 0x00, 0);
pub const WLAN: (u8, u8, u8) = (0x85, 0x00, 0);
pub const WLAN_PORT: (u8, u8, u8) = (0x80, 0x1c, 3);
pub const PCH_AUDIO: (u8, u8, u8) = (0x80, 0x1f, 3);
pub const GPU_AUDIO: (u8, u8, u8) = (0x04, 0x00, 1);
pub const FLASH_PCH: (u8, u8, u8) = (0x80, 0x1f, 5);
pub const FLASH_CPU: (u8, u8, u8) = (0x00, 0x1f, 5);
pub const VMD: (u8, u8, u8) = (0x00, 0x0e, 0);
pub const NHI: (u8, u8, u8) = (0x00, 0x0d, 2);
pub const MEI: (u8, u8, u8) = (0x80, 0x16, 0);
/// The eight functions of the never-touch list, in walk order.
pub const NEVER: [(u8, u8, u8); 8] = [NHI, VMD, FLASH_CPU, GPU_AUDIO, MEI, PCH_AUDIO, FLASH_PCH, WLAN];

fn fake_fn(x: &Function, table: &[Function]) -> FakeFn {
    let bridge = x.class == 0x06_04_00;
    let multi = x.func == 0 && table.iter().any(|o| o.bus == x.bus && o.dev == x.dev && o.func != 0);
    let mut f = FakeFn::new(x.vendor, x.device, x.class, u8::from(bridge) | if multi { 0x80 } else { 0 });
    if bridge {
        // Root ports on the two root buses; the card's switch is an upstream port above a downstream port.
        let port_type = match x.bus {
            0x00 | 0x80 => 4,
            0x02 => 5,
            _ => 6,
        };
        f.buses(x.bus, x.below.0, x.below.1).cmd(0x7).pcie(0x40, port_type, 0);
    }
    match (x.bus, x.dev, x.func) {
        XHCI_PCH => drop(f.cmd(0x6).bar64(0, 0x80_0020_0000, 0x1_0000, false).msi(0x80, 0, false)),
        XHCI_CPU => drop(f.cmd(0x6).bar64(0, 0xb4_3306_0000, 0x1_0000, false).msi(0x80, 0, false)),
        GPU_A => drop(f.cmd(0x7).bar64(0, 0xa0_0000_0000, 16 << 30, true).bar64(2, 0xa4_0000_0000, 256 << 20, true).bar32(5, 0x8d00_0000, 0x8_0000, false)),
        GPU_AUDIO => drop(f.cmd(0x6).bar32(0, 0x8d0a_0000, 0x4000, false)),
        NIC => drop(f.cmd(0x7).bario(0, 0x3000, 0x100, true).bar64(2, 0xbb10_0000, 0x1_0000, false).bar64(4, 0xbb11_0000, 0x4000, false)),
        WLAN => drop(f.cmd(0x6).bar64(0, 0xbb00_0000, 0x4000, false)),
        PCH_AUDIO => drop(f.cmd(0x6).bar64(0, 0x80_0021_0000, 0x4000, false).bar64(4, 0x80_0000_0000, 0x20_0000, false)),
        FLASH_PCH => drop(f.cmd(0x6).bar32(0, 0xfe01_0000, 0x1000, false)),
        FLASH_CPU => drop(f.cmd(0x6).bar32(0, 0xfe01_1000, 0x1000, false)),
        VMD => drop(f.cmd(0x6).bar64(0, 0xb4_3000_0000, 32 << 20, false).bar32(2, 0x8c00_0000, 16 << 20, false).bar64(4, 0xb4_3200_0000, 16 << 20, false)),
        NHI => drop(f.cmd(0x6).bar64(0, 0xb4_3300_0000, 0x4_0000, false).bar64(2, 0xb4_3307_8000, 0x1000, false)),
        MEI => drop(f.cmd(0x6).bar64(0, 0x80_0021_5000, 0x1000, false)),
        // The I2C functions: a 64-bit BAR the firmware left unassigned, decoding off.
        (0x80, 0x15, _) => drop(f.cmd(0).bar64(0, 0, 0x1000, false)),
        _ if x.class == 0x01_08_02 => drop(f.cmd(0x6).bar64(0, 0x8d20_0000 + ((x.bus as u64) << 16), 0x4000, false)),
        _ if x.class == 0x06_00_00 => drop(f.cmd(0x6)),
        _ => {}
    }
    f
}

/// The machine with the functions of `table`; ECAM covers buses 00-df as the board's MCFG does.
pub fn machine(table: &[Function]) -> Fake {
    let mut w = Fake::new();
    w.reach = (facts::ECAM_FIRST_BUS, facts::ECAM_LAST_BUS);
    for x in table {
        w.add(x.bus, x.dev, x.func, fake_fn(x, table));
    }
    w
}

pub fn gaming_pc() -> Fake {
    machine(facts::FUNCTIONS)
}

/// The one MCFG range of the board.
pub const WINDOW: crate::enumerate::Root = crate::enumerate::Root { segment: 0, bus_start: facts::ECAM_FIRST_BUS, bus_end: facts::ECAM_LAST_BUS };

