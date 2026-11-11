//! W35 MM-1: `/proc/pagetypeinfo` (Linux 7.x mm/vmstat.c `pagetypeinfo_show`, `pagetypeinfo_showfree_print`,
//! `pagetypeinfo_showblockcount_print`): free blocks per zone, migrate type and order, then pageblocks per type.
//! The rows and columns are CachyOS's (CONFIG_CMA and CONFIG_MEMORY_ISOLATION on: six types); BitsOS keeps four
//! types apart (HighAtomic is the reserve of `mm_core::buddy::highatomic`), CMA and Isolate read 0. Orders above 10 fold as in /proc/buddyinfo. Linux makes the
//! file 0400; procfs files here have one mode for all (open point in the W35 MM-1 note).

use super::buddyinfo::{fold, populated, zone_name, zones, LINUX_ORDERS};
use alloc::string::String;
use core::fmt::Write;
use mm_core::{MigrateType, ZoneInfo, PAGEBLOCK_ORDER};

/// Linux `migratetype_names` with CONFIG_CMA and CONFIG_MEMORY_ISOLATION.
const NAMES: [&str; 6] = ["Unmovable", "Movable", "Reclaimable", "HighAtomic", "CMA", "Isolate"];
/// Linux caps the count of one free list at this (`pagetypeinfo_showfree_print`) and prints `>` before it.
const CAP: u64 = 100_000;

/// The type of row `i` (None: a type BitsOS does not have).
fn kept(i: usize) -> Option<MigrateType> {
    MigrateType::ALL.get(i).copied()
}

pub(super) fn pagetypeinfo(zones: &[ZoneInfo]) -> String {
    let mut s = String::new();
    let _ = writeln!(s, "Page block order: {PAGEBLOCK_ORDER}");
    let _ = writeln!(s, "Pages per block:  {}", 1u64 << PAGEBLOCK_ORDER);
    s.push('\n');
    let _ = write!(s, "{:<43} ", "Free pages count per migrate type at order");
    for o in 0..LINUX_ORDERS {
        let _ = write!(s, "{o:>6} ");
    }
    s.push('\n');
    for z in populated(zones) {
        for (i, name) in NAMES.iter().enumerate() {
            let _ = write!(s, "Node {:>4}, zone {:>8}, type {:>12} ", 0, zone_name(z), name);
            let row = kept(i).map_or([0; LINUX_ORDERS], |mt| fold(&z.free_by_type[mt as usize]));
            for n in row {
                if n >= CAP {
                    let _ = write!(s, ">{CAP:>6} ");
                } else {
                    let _ = write!(s, "{n:>6} ");
                }
            }
            s.push('\n');
        }
    }
    let _ = write!(s, "\n{:<23}", "Number of blocks type ");
    for name in NAMES {
        let _ = write!(s, "{name:>12} ");
    }
    s.push('\n');
    for z in populated(zones) {
        let _ = write!(s, "Node 0, zone {:>8} ", zone_name(z));
        for i in 0..NAMES.len() {
            let _ = write!(s, "{:>12} ", kept(i).map_or(0, |mt| z.pageblocks[mt as usize]));
        }
        s.push('\n');
    }
    s
}

fn render_pagetypeinfo() -> String {
    pagetypeinfo(&zones())
}
vfs::procfile!("/proc/pagetypeinfo", render_pagetypeinfo, 0o400);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_and_columns_as_linux() {
        let mut d = ZoneInfo { name: "dma32", managed_frames: 1000, ..Default::default() };
        d.free_by_type[MigrateType::Movable as usize][0] = 3;
        d.free_by_type[MigrateType::Unmovable as usize][11] = 1;
        d.free_by_type[MigrateType::Movable as usize][1] = 200_000;
        d.free_by_type[MigrateType::HighAtomic as usize][8] = 4;
        d.pageblocks = [5, 2041, 0, 2];
        let n = ZoneInfo { name: "normal", managed_frames: 0, ..Default::default() };
        let s = pagetypeinfo(&[d, n]);
        let want = "Page block order: 9\n\
Pages per block:  512\n\
\n\
Free pages count per migrate type at order       0      1      2      3      4      5      6      7      8      9     10 \n\
Node    0, zone    DMA32, type    Unmovable      0      0      0      0      0      0      0      0      0      0      2 \n\
Node    0, zone    DMA32, type      Movable      3 >100000      0      0      0      0      0      0      0      0      0 \n\
Node    0, zone    DMA32, type  Reclaimable      0      0      0      0      0      0      0      0      0      0      0 \n\
Node    0, zone    DMA32, type   HighAtomic      0      0      0      0      0      0      0      0      4      0      0 \n\
Node    0, zone    DMA32, type          CMA      0      0      0      0      0      0      0      0      0      0      0 \n\
Node    0, zone    DMA32, type      Isolate      0      0      0      0      0      0      0      0      0      0      0 \n\
\n\
Number of blocks type     Unmovable      Movable  Reclaimable   HighAtomic          CMA      Isolate \n\
Node 0, zone    DMA32            5         2041            0            2            0            0 \n";
        assert_eq!(s, want);
    }
}
