//! Which APs to start (pure; host-tested): every enabled MADT CPU except the BSP, in MADT order (the Mini-PC's
//! firmware lists one thread of each core first, then the siblings; spec §6.1). No maximum (R1).

use acpi::madt::Madt;
use alloc::vec::Vec;

/// One AP the BSP will try to start.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PlannedAp {
    pub apic_id: u32,
    pub uid: u32,
    /// Listed by an x2APIC entry (type 9).
    pub x2apic_entry: bool,
}

/// Why a MADT CPU is not started.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Skip {
    /// The BSP's own entry.
    Bsp,
    /// Off now but online-capable (ACPI 6.3): hot-add is not supported, so it stays off.
    OnlineCapable,
    /// Disabled and not online-capable.
    Disabled,
    /// A second enabled entry with an APIC ID seen before (firmware bug; the first wins, like Linux).
    Duplicate,
    /// APIC ID above 0xFE while the LAPICs run in xAPIC mode (8-bit destinations).
    NeedsX2Apic,
}

impl Skip {
    pub fn name(self) -> &'static str {
        match self {
            Skip::Bsp => "bsp",
            Skip::OnlineCapable => "online_capable_not_started",
            Skip::Disabled => "disabled",
            Skip::Duplicate => "duplicate_apic_id",
            Skip::NeedsX2Apic => "needs_x2apic",
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Plan {
    pub aps: Vec<PlannedAp>,
    /// (APIC ID, why) for every MADT CPU entry not started, the BSP's included.
    pub skipped: Vec<(u32, Skip)>,
}

/// The APs to start from `madt`, given the BSP's APIC ID and whether the LAPICs run in x2APIC mode.
pub fn plan(madt: &Madt, bsp_apic: u32, x2apic: bool) -> Plan {
    let mut p = Plan::default();
    let mut seen: Vec<u32> = Vec::new();
    for c in &madt.cpus {
        let why = if !c.enabled {
            Some(if c.online_capable { Skip::OnlineCapable } else { Skip::Disabled })
        } else if c.apic_id == bsp_apic {
            if seen.contains(&c.apic_id) { Some(Skip::Duplicate) } else { Some(Skip::Bsp) }
        } else if seen.contains(&c.apic_id) {
            Some(Skip::Duplicate)
        } else if !x2apic && c.apic_id > 0xFE {
            Some(Skip::NeedsX2Apic)
        } else {
            None
        };
        if c.enabled {
            seen.push(c.apic_id);
        }
        match why {
            Some(w) => p.skipped.push((c.apic_id, w)),
            None => p.aps.push(PlannedAp { apic_id: c.apic_id, uid: c.uid, x2apic_entry: c.x2apic }),
        }
    }
    p
}

/// The SRAT proximity domain of `apic` (0 without an SRAT or an entry for it).
pub fn node_of(srat: Option<&acpi::srat::Srat>, apic: u32) -> u32 {
    srat.and_then(|s| s.cpus.iter().find(|c| c.enabled && c.apic_id == apic)).map_or(0, |c| c.domain)
}

#[cfg(test)]
mod tests {
    use super::*;
    use acpi::madt::Cpu;

    fn cpu(apic_id: u32, enabled: bool) -> Cpu {
        Cpu { apic_id, uid: apic_id + 100, enabled, online_capable: false, x2apic: false }
    }

    #[test]
    fn mini_pc_order() {
        // Even APIC IDs first, then the odd ones (HW-MINIPC round 5).
        let order: Vec<u32> = (0..16).step_by(2).chain((1..16).step_by(2)).collect();
        let m = Madt { cpus: order.iter().map(|&a| cpu(a, true)).collect(), ..Default::default() };
        let p = plan(&m, 0, true);
        assert_eq!(p.aps.len(), 15);
        assert_eq!(p.aps[0].apic_id, 2);
        assert_eq!(p.aps[6].apic_id, 14);
        assert_eq!(p.aps[7].apic_id, 1);
        assert_eq!(p.skipped, alloc::vec![(0, Skip::Bsp)]);
        assert_eq!(p.aps[0].uid, 102);
    }

    #[test]
    fn odd_entries() {
        let mut cap = cpu(7, false);
        cap.online_capable = true;
        let mut big = cpu(0x1FF, true);
        big.x2apic = true;
        let m = Madt { cpus: alloc::vec![cpu(3, true), cpu(1, true), cpu(1, true), cpu(5, false), cap, big, cpu(3, true)], ..Default::default() };
        let p = plan(&m, 3, false);
        assert_eq!(p.aps.iter().map(|a| a.apic_id).collect::<Vec<_>>(), alloc::vec![1]);
        assert_eq!(
            p.skipped,
            alloc::vec![(3, Skip::Bsp), (1, Skip::Duplicate), (5, Skip::Disabled), (7, Skip::OnlineCapable), (0x1FF, Skip::NeedsX2Apic), (3, Skip::Duplicate)]
        );
        let p = plan(&m, 3, true);
        assert_eq!(p.aps.iter().map(|a| (a.apic_id, a.x2apic_entry)).collect::<Vec<_>>(), alloc::vec![(1, false), (0x1FF, true)]);
    }

    #[test]
    fn empty_and_single() {
        assert_eq!(plan(&Madt::default(), 0, true), Plan::default());
        let m = Madt { cpus: alloc::vec![cpu(0, true)], ..Default::default() };
        assert!(plan(&m, 0, true).aps.is_empty());
    }

    #[test]
    fn nodes() {
        use acpi::srat::{CpuAffinity, Srat};
        let s = Srat { cpus: alloc::vec![CpuAffinity { apic_id: 4, domain: 1, enabled: true, x2apic: false }, CpuAffinity { apic_id: 5, domain: 2, enabled: false, x2apic: false }], ..Default::default() };
        assert_eq!(node_of(Some(&s), 4), 1);
        assert_eq!(node_of(Some(&s), 5), 0, "disabled entry");
        assert_eq!(node_of(None, 4), 0);
    }
}
