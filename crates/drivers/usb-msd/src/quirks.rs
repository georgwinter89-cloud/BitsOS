//! Per-device limits and quirks.
//!
//! Transfer size per command (bytes; Linux keeps `max_sectors` in 512-byte sectors, so a 4K-block device gets the
//! same byte limit in fewer blocks):
//! - SuperSpeed and faster: 2048 sectors = 1 MiB (linux-6.6/drivers/usb/storage/scsiglue.c:117-123);
//! - high speed: 240 sectors = 120 KiB (the SCSI host default of usb-storage; uas.c:837; V1 `MSD_RD_HIGH`);
//! - full/low speed: 128 sectors = 64 KiB (V1: one 64 KiB command);
//! - `US_FL_MAX_SECTORS_64`: 64 sectors = 32 KiB (scsiglue.c:97-109).
//!
//! The SMI stick 090C:1000 (Georg's BITSOS stick, "SMI USB DISK") gets the 32 KiB limit for every revision, as V1
//! did (`usb/msd.asm:34`), but only below SuperSpeed: Georg's 128 GB "TESLA DRIVE" (Samsung Flash Drive, also
//! 090C:1000 rev 11.00) moved 1 MiB commands at 5 Gbit/s on CachyOS without an error (22.10.2026: 60 MB/s write,
//! 413 MB/s read). Note: linux-6.6's `unusual_devs.h` has no 090C:1000 entry (only 090C:1132, 3350,
//! 6000); the spec's "there only for bcdDevice 11.00" could not be found in 6.6. Linux users of such sticks set
//! `usb-storage.quirks=090c:1000:m` (usb.c:550, `m` = MAX_SECTORS_64). V1 measured the limit on the real stick.

use usb_core::Speed;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Quirks {
    /// At most 64 sectors (32 KiB) per command.
    pub max_sectors_64: bool,
    /// Ignore dCSWDataResidue from the start (Linux `US_FL_IGNORE_RESIDUE`).
    pub ignore_residue: bool,
}

struct Entry {
    vendor: u16,
    product: u16,
    /// bcdDevice range, inclusive.
    rev: (u16, u16),
    quirks: Quirks,
}

/// The table (Linux `UNUSUAL_DEV` order: vendor, product, bcdDevice range). Only devices BitsOS met so far;
/// entries from Linux's list get added when such a device shows up.
const TABLE: &[Entry] = &[
    Entry { vendor: 0x090C, product: 0x1000, rev: (0x0000, 0xFFFF), quirks: Quirks { max_sectors_64: true, ignore_residue: false } },
];

pub fn lookup(vendor: u16, product: u16, rev: u16) -> Quirks {
    TABLE
        .iter()
        .find(|e| e.vendor == vendor && e.product == product && (e.rev.0..=e.rev.1).contains(&rev))
        .map(|e| e.quirks)
        .unwrap_or_default()
}

/// Bytes one READ/WRITE command may move.
pub fn max_bytes(speed: Speed, q: Quirks) -> usize {
    if q.max_sectors_64 && matches!(speed, Speed::Low | Speed::Full | Speed::High) {
        return 64 * 512;
    }
    match speed {
        Speed::Low | Speed::Full => 128 * 512,
        Speed::High => 240 * 512,
        _ => 2048 * 512,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_smi_stick_gets_32_kib_below_superspeed_on_every_revision() {
        for rev in [0x0000, 0x1100, 0xFFFF] {
            let q = lookup(0x090C, 0x1000, rev);
            assert!(q.max_sectors_64);
            assert_eq!(max_bytes(Speed::High, q), 32 * 1024);
            assert_eq!(max_bytes(Speed::Full, q), 32 * 1024);
            assert_eq!(max_bytes(Speed::Super, q), 1 << 20);
        }
        assert_eq!(lookup(0x090C, 0x1001, 0x1100), Quirks::default());
        assert_eq!(lookup(0x46F4, 0x0001, 0), Quirks::default(), "QEMU's usb-storage has no quirk");
    }

    #[test]
    fn limits_by_speed() {
        let q = Quirks::default();
        assert_eq!(max_bytes(Speed::Full, q), 64 * 1024);
        assert_eq!(max_bytes(Speed::High, q), 120 * 1024);
        assert_eq!(max_bytes(Speed::Super, q), 1 << 20);
        assert_eq!(max_bytes(Speed::SuperPlus, q), 1 << 20);
    }
}
