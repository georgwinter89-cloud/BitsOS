//! A Mini-PC-like configuration space (HARDWARE-MINIPC.md rounds 1-7): host bridge, root ports with the
//! NVMe disks, both I226-V, the WLAN, the ghost functions 00:03.2-00:03.4 (vendor 0x0000), the USB4 bridge
//! range, and the GPU's multi-function device with its 256 MiB BAR above 4 GiB.

use super::fake::{bridge, root_port, Fake, FakeFn};

pub const NVME0: (u8, u8, u8) = (0x01, 0, 0);
pub const I226_UNUSED: (u8, u8, u8) = (0x02, 0, 0);
pub const I226: (u8, u8, u8) = (0x03, 0, 0);
pub const WLAN: (u8, u8, u8) = (0x04, 0, 0);
pub const WLAN_BRIDGE: (u8, u8, u8) = (0x00, 0x02, 2);
pub const NVME1: (u8, u8, u8) = (0x05, 0, 0);
pub const GPU: (u8, u8, u8) = (0xC7, 0, 0);

fn nvme(device: u16, bar: u64) -> FakeFn {
    let mut f = FakeFn::new(if device == 0x2268 { 0x126F } else { 0x1D97 }, device, 0x01_08_02, 0);
    f.cmd(0x6).bar64(0, bar, 0x4000, false).pcie(0x80, 0, 0xB0).cap(0xB0, 0x11, 0);
    // MSI-X: 9 entries, table at BAR0 + 0x2000, PBA at BAR0 + 0x3000.
    f.w16(0xB2, 8).w32(0xB4, 0x2000).w32(0xB8, 0x3000);
    f.ext(0x100, 0x0001, 2, 0x150).ext(0x150, 0x001E, 1, 0);
    f
}

fn i226(bar0: u32, bar3: u32) -> FakeFn {
    let mut f = FakeFn::new(0x8086, 0x125C, 0x02_00_00, 0);
    f.cmd(0x0).bar32(0, bar0, 0x10_0000, false).bar32(3, bar3, 0x4000, false);
    // PM at 0x40, MSI at 0x50 (64-bit, per-vector mask), MSI-X at 0x70 (5 entries, table in BAR3), PCIe 0xA0.
    f.cap(0x40, 0x01, 0x50).cap(0x50, 0x05, 0x70).cap(0x70, 0x11, 0xA0).pcie(0xA0, 0, 0);
    f.w16(0x52, 0x0180 | 0x8);
    f.w16(0x72, 4).w32(0x74, 3).w32(0x78, 3 | 0x2000);
    // AER, DSN, L1SS (L1.2 on), PTM.
    f.ext(0x100, 0x0001, 2, 0x140).ext(0x140, 0x0003, 1, 0x1C0).ext(0x1C0, 0x001E, 1, 0x1F0).ext(0x1F0, 0x001F, 1, 0);
    f.w32(0x1C8, 0x5);
    f
}

pub fn minipc() -> Fake {
    let w = Fake::new();
    let mut hb = FakeFn::new(0x1022, 0x14E8, 0x06_00_00, 0x80);
    hb.cmd(0x6);
    w.add(0, 0, 0, hb);
    w.add(0, 0, 2, FakeFn::new(0x1022, 0x14E9, 0x08_06_00, 0)); // IOMMU
    let mut d1 = FakeFn::new(0x1022, 0x14EA, 0x06_00_00, 0x80);
    d1.cmd(0);
    w.add(0, 1, 0, d1);
    w.add(0, 1, 1, root_port(0, 0x01, 0x01));
    w.add(0, 1, 3, root_port(0, 0x02, 0x02));
    let mut d2 = FakeFn::new(0x1022, 0x14EA, 0x06_00_00, 0x80);
    d2.cmd(0);
    w.add(0, 2, 0, d2);
    w.add(0, 2, 1, root_port(0, 0x03, 0x03));
    w.add(0, 2, 2, root_port(0, 0x04, 0x04));
    w.add(0, 2, 4, root_port(0, 0x05, 0x05));
    w.add(0, 2, 5, root_port(0, 0x06, 0x06));
    let mut d3 = FakeFn::new(0x1022, 0x14EA, 0x06_00_00, 0x80);
    d3.cmd(0);
    w.add(0, 3, 0, d3);
    w.add(0, 3, 1, root_port(0, 0x07, 0x66));
    // The ghosts: vendor 0x0000 (device id nonzero to make sure the vendor alone decides).
    for func in 2..=4 {
        w.add(0, 3, func, FakeFn::new(0x0000, 0x1234, 0, 0));
    }
    let mut d4 = FakeFn::new(0x1022, 0x14EA, 0x06_00_00, 0x80);
    d4.cmd(0);
    w.add(0, 4, 0, d4);
    w.add(0, 4, 1, root_port(0, 0x67, 0xC6));
    let mut d8 = FakeFn::new(0x1022, 0x14EA, 0x06_00_00, 0x80);
    d8.cmd(0);
    w.add(0, 8, 0, d8);
    w.add(0, 8, 1, bridge(0x1022, 0x14EB, 0, 0xC7, 0xC7));
    w.add(0, 8, 2, bridge(0x1022, 0x14EB, 0, 0xC8, 0xC8));
    w.add(0, 8, 3, bridge(0x1022, 0x14EB, 0, 0xC9, 0xC9));
    let mut smbus = FakeFn::new(0x1022, 0x790B, 0x0C_05_00, 0x80);
    smbus.cmd(0x3);
    w.add(0, 0x14, 0, smbus);
    w.add(0, 0x14, 3, FakeFn::new(0x1022, 0x790E, 0x06_01_00, 0));

    w.add(NVME0.0, 0, 0, nvme(0x2268, 0xDCF0_0000));
    w.add(I226_UNUSED.0, 0, 0, i226(0xDCD0_0000, 0xDCE0_0000));
    w.add(I226.0, 0, 0, i226(0xDCA0_0000, 0xDCB0_0000));
    let mut wl = FakeFn::new(0x10EC, 0xB852, 0x02_80_00, 0);
    wl.cmd(0).bario(0, 0xF000, 0x100, true).bar64(2, 0xDD10_0000, 0x10_0000, false).cap(0x50, 0x05, 0);
    w.add(WLAN.0, 0, 0, wl);
    w.add(NVME1.0, 0, 0, nvme(0x5236, 0xDC90_0000));

    let mut gpu = FakeFn::new(0x1002, 0x1900, 0x03_00_00, 0x80);
    gpu.cmd(0x7).bar64(0, 0x7C_0000_0000, 0x1000_0000, true).bar64(2, 0x7C_1000_0000, 0x20_0000, true).bario(4, 0xE000, 0x100, false).bar32(5, 0xDC50_0000, 0x8_0000, false);
    w.add(GPU.0, 0, 0, gpu);
    let mut hdmi = FakeFn::new(0x1002, 0x1640, 0x04_03_00, 0);
    hdmi.bar32(0, 0xDC5C_8000, 0x4000, false).cap(0xA0, 0x05, 0);
    w.add(GPU.0, 0, 1, hdmi);
    let mut xhci = FakeFn::new(0x1022, 0x15B9, 0x0C_03_30, 0);
    xhci.cmd(0x6).bar64(0, 0xDC00_0000, 0x10_0000, false);
    w.add(GPU.0, 0, 3, xhci);
    let mut hda = FakeFn::new(0x1022, 0x15E3, 0x04_03_00, 0);
    hda.bar32(0, 0xDC5C_0000, 0x8000, false);
    w.add(GPU.0, 0, 6, hda);
    w.add(0xC8, 0, 0, FakeFn::new(0x1022, 0x14EC, 0x13_00_00, 0));
    let mut usb4 = FakeFn::new(0x1022, 0x1668, 0x0C_03_40, 0x80);
    usb4.bar64(0, 0xDB00_0000, 0x8_0000, false);
    w.add(0xC9, 0, 0, FakeFn::new(0x1022, 0x14EC, 0x13_00_00, 0x80));
    w.add(0xC9, 0, 5, usb4);
    w
}

/// Every present function of `minipc()`, in walk order.
pub const MINIPC_COUNT: usize = 32;
