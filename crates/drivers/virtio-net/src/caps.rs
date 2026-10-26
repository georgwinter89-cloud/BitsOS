//! virtio over PCI, "modern" interface (virtio 1.2 §4.1.4): the vendor capabilities that say where the common
//! configuration, the notify area, the ISR byte and the device configuration live, and feature negotiation.
//! Pure functions over config-space readers, so host tests feed them made-up config spaces.

/// PCI capability id of the virtio structures.
pub const PCI_CAP_VENDOR: u8 = 0x09;
pub const CFG_COMMON: u8 = 1;
pub const CFG_NOTIFY: u8 = 2;
pub const CFG_ISR: u8 = 3;
pub const CFG_DEVICE: u8 = 4;

/// Feature bits (virtio 1.2 §6, §5.1.3).
pub mod feat {
    pub const NET_MTU: u64 = 1 << 3;
    pub const NET_MAC: u64 = 1 << 5;
    pub const NET_STATUS: u64 = 1 << 16;
    pub const VERSION_1: u64 = 1 << 32;
    /// DMA goes through the platform's translation: with the IOMMU off that is the physical address we use.
    pub const ACCESS_PLATFORM: u64 = 1 << 33;
    pub const NET_SPEED_DUPLEX: u64 = 1 << 63;
}

/// Everything we accept when offered. Not taken on purpose: checksum/TSO offloads (the stack checksums, NW-7
/// may add them), mergeable RX buffers (one 2 KiB buffer per frame suffices at MTU 1500), control queue,
/// multiqueue, event index, packed rings.
pub const WANTED: u64 = feat::NET_MTU | feat::NET_MAC | feat::NET_STATUS | feat::ACCESS_PLATFORM | feat::NET_SPEED_DUPLEX;

/// The features to write back, or why the device cannot be driven.
pub fn negotiate(offered: u64) -> Result<u64, &'static str> {
    if offered & feat::VERSION_1 == 0 {
        return Err("device offers no VIRTIO_F_VERSION_1 (legacy-only interface, not supported)");
    }
    Ok(feat::VERSION_1 | (offered & WANTED))
}

/// One structure: `length` bytes at `offset` inside BAR `bar`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Region {
    pub bar: u8,
    pub offset: u32,
    pub length: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VirtioCaps {
    pub common: Region,
    pub notify: Region,
    pub notify_mult: u32,
    pub isr: Option<Region>,
    pub device: Option<Region>,
}

/// Walks the capability list `(offset, id)` and takes the first usable structure of each type (the spec lets
/// a device offer several; the first one is its preferred). Missing common or notify = not a modern device.
pub fn parse(caps: &[(u8, u8)], rd8: impl Fn(u8) -> u8, rd32: impl Fn(u8) -> u32) -> Result<VirtioCaps, &'static str> {
    let (mut common, mut notify, mut isr, mut device) = (None, None, None, None);
    for &(off, id) in caps {
        if id != PCI_CAP_VENDOR {
            continue;
        }
        let Some(end) = off.checked_add(16) else { continue };
        let (len, ty, bar) = (rd8(off + 2), rd8(off + 3), rd8(off + 4));
        if len < 16 || bar > 5 {
            continue;
        }
        let r = Region { bar, offset: rd32(off + 8), length: rd32(off + 12) };
        if r.length == 0 {
            continue;
        }
        match ty {
            CFG_COMMON if common.is_none() && r.length >= 0x38 => common = Some(r),
            CFG_NOTIFY if notify.is_none() && len >= 20 && end.checked_add(4).is_some() => {
                notify = Some((r, rd32(end)));
            }
            CFG_ISR if isr.is_none() => isr = Some(r),
            CFG_DEVICE if device.is_none() => device = Some(r),
            _ => {}
        }
    }
    let common = common.ok_or("no virtio common configuration capability (legacy-only device?)")?;
    let (notify, notify_mult) = notify.ok_or("no virtio notify capability")?;
    Ok(VirtioCaps { common, notify, notify_mult, isr, device })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// QEMU's virtio-net-pci layout: MSI-X at 0x98, then vendor caps for common/isr/device/notify/pci-cfg, all
    /// in BAR 4.
    fn qemu_space() -> ([u8; 256], alloc::vec::Vec<(u8, u8)>) {
        let mut s = [0u8; 256];
        let mut caps = alloc::vec::Vec::new();
        let mut put = |off: usize, ty: u8, len: u8, bar: u8, offset: u32, length: u32, extra: Option<u32>| {
            s[off] = PCI_CAP_VENDOR;
            s[off + 2] = len;
            s[off + 3] = ty;
            s[off + 4] = bar;
            s[off + 8..off + 12].copy_from_slice(&offset.to_le_bytes());
            s[off + 12..off + 16].copy_from_slice(&length.to_le_bytes());
            if let Some(e) = extra {
                s[off + 16..off + 20].copy_from_slice(&e.to_le_bytes());
            }
            caps.push((off as u8, PCI_CAP_VENDOR));
        };
        put(0x84, 5, 20, 0, 0, 4, Some(0)); // PCI cfg access (ignored)
        put(0x70, CFG_NOTIFY, 20, 4, 0x3000, 0x1000, Some(4));
        put(0x60, CFG_DEVICE, 16, 4, 0x2000, 0x1000, None);
        put(0x50, CFG_ISR, 16, 4, 0x1000, 0x1000, None);
        put(0x40, CFG_COMMON, 16, 4, 0x0000, 0x1000, None);
        caps.reverse();
        caps.insert(0, (0x98, 0x11));
        (s, caps)
    }

    fn rd(s: &[u8; 256]) -> (impl Fn(u8) -> u8 + '_, impl Fn(u8) -> u32 + '_) {
        (move |o: u8| s[o as usize], move |o: u8| {
            let o = o as usize;
            if o + 4 > 256 { 0xFFFF_FFFF } else { u32::from_le_bytes(s[o..o + 4].try_into().unwrap()) }
        })
    }

    #[test]
    fn parses_qemu_layout() {
        let (s, caps) = qemu_space();
        let (r8, r32) = rd(&s);
        let c = parse(&caps, r8, r32).unwrap();
        assert_eq!(c.common, Region { bar: 4, offset: 0, length: 0x1000 });
        assert_eq!(c.notify, Region { bar: 4, offset: 0x3000, length: 0x1000 });
        assert_eq!(c.notify_mult, 4);
        assert_eq!(c.isr.unwrap().offset, 0x1000);
        assert_eq!(c.device.unwrap().offset, 0x2000);
    }

    #[test]
    fn legacy_only_or_broken_caps_are_refused() {
        let s = [0u8; 256];
        let (r8, r32) = rd(&s);
        assert!(parse(&[(0x98, 0x11)], r8, r32).is_err());
        // A bad BAR number and a too-short capability are skipped, so common is missing.
        let (mut s, caps) = qemu_space();
        s[0x40 + 4] = 7;
        let (r8, r32) = rd(&s);
        assert!(parse(&caps, r8, r32).unwrap_err().contains("common"));
        let (mut s, caps) = qemu_space();
        s[0x70 + 2] = 16; // notify without the multiplier
        let (r8, r32) = rd(&s);
        assert!(parse(&caps, r8, r32).unwrap_err().contains("notify"));
        // A capability at the very end of config space does not overflow.
        let (r8, r32) = rd(&s);
        assert!(parse(&[(0xFC, PCI_CAP_VENDOR)], r8, r32).is_err());
    }

    #[test]
    fn negotiation() {
        assert!(negotiate(feat::NET_MAC).is_err());
        let all = u64::MAX;
        let f = negotiate(all).unwrap();
        assert_eq!(f, feat::VERSION_1 | WANTED);
        assert_eq!(f & (1 << 15), 0, "no mergeable RX buffers");
        assert_eq!(f & (1 << 0), 0, "no checksum offload");
        assert_eq!(negotiate(feat::VERSION_1 | feat::NET_MAC).unwrap(), feat::VERSION_1 | feat::NET_MAC);
    }
}
