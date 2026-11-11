//! W35 MM-1: `/proc/buddyinfo` (Linux 7.x mm/vmstat.c `frag_show_print`): free blocks per zone and order. Linux has
//! orders 0..=10 (`NR_PAGE_ORDERS` 11 on x86-64); BitsOS's buddy goes up to order 18, so a larger free block is shown
//! as the order-10 blocks it holds (the same 11 columns as CachyOS, the free memory they add up to unchanged).

use alloc::string::String;
use core::fmt::Write;
use mm_core::ZoneInfo;

/// Linux `NR_PAGE_ORDERS` on x86-64.
pub(super) const LINUX_ORDERS: usize = 11;

/// The zone name as Linux prints it.
pub(super) fn zone_name(z: &ZoneInfo) -> &'static str {
    match z.name {
        "dma32" => "DMA32",
        "normal" => "Normal",
        n => n,
    }
}

/// `blocks` per order folded into Linux's 11 orders (blocks above order 10 count as order-10 blocks).
pub(super) fn fold(blocks: &[u64]) -> [u64; LINUX_ORDERS] {
    let mut out = [0u64; LINUX_ORDERS];
    for (o, &n) in blocks.iter().enumerate() {
        let (col, shift) = if o < LINUX_ORDERS { (o, 0) } else { (LINUX_ORDERS - 1, o - (LINUX_ORDERS - 1)) };
        out[col] = out[col].saturating_add(n << shift);
    }
    out
}

/// The zones with memory, in Linux's order (DMA32 below Normal).
pub(super) fn populated(zones: &[ZoneInfo]) -> impl Iterator<Item = &ZoneInfo> {
    zones.iter().filter(|z| z.managed_frames > 0)
}

pub(super) fn buddyinfo(zones: &[ZoneInfo]) -> String {
    let mut s = String::new();
    for z in populated(zones) {
        let _ = write!(s, "Node 0, zone {:>8} ", zone_name(z));
        for n in fold(&z.free_blocks) {
            let _ = write!(s, "{n:>6} ");
        }
        s.push('\n');
    }
    s
}

pub(super) fn zones() -> [ZoneInfo; 2] {
    match mm_core::boot_buddy() {
        Some(b) => [b.zone_info(0), b.zone_info(1)],
        None => [ZoneInfo::default(); 2],
    }
}

fn render_buddyinfo() -> String {
    buddyinfo(&zones())
}
vfs::procfile!("/proc/buddyinfo", render_buddyinfo);

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn sample() -> [ZoneInfo; 2] {
        let mut d = ZoneInfo { name: "dma32", managed_frames: 1000, ..Default::default() };
        d.free_blocks[0] = 384;
        d.free_blocks[1] = 1428;
        d.free_blocks[10] = 8;
        let mut n = ZoneInfo { name: "normal", managed_frames: 5000, ..Default::default() };
        n.free_blocks[0] = 46107;
        n.free_blocks[9] = 1649;
        n.free_blocks[12] = 2; // 2 blocks of 16 MiB = 8 blocks of 4 MiB
        n.free_blocks[10] = 30;
        [d, n]
    }

    /// The layout of CachyOS's /proc/buddyinfo (Linux 7.3 on the gaming PC, 6.11.2026): "Node 0, zone    DMA32    384   1428 ...".
    #[test]
    fn columns_as_linux() {
        let s = buddyinfo(&sample());
        let mut l = s.lines();
        assert_eq!(l.next(), Some("Node 0, zone    DMA32    384   1428      0      0      0      0      0      0      0      0      8 "));
        assert_eq!(l.next(), Some("Node 0, zone   Normal  46107      0      0      0      0      0      0      0      0   1649     38 "));
        assert_eq!(l.next(), None);
        assert!(s.ends_with(" \n"));
    }

    #[test]
    fn empty_zones_are_left_out() {
        let mut z = sample();
        z[1].managed_frames = 0;
        assert_eq!(buddyinfo(&z).lines().count(), 1);
    }
}
