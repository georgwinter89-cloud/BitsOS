//! Driver registry (contract `pci_driver!` with `Match`), the rules that keep a function untouched, and the
//! defined order in which drivers are tried (PC-4): an exact ID match before any class match, a narrower class
//! mask before a wider one, then the driver name. Binding itself is `bind.rs`.

use crate::PciFunction;
use alloc::string::String;
use ksync::OnceRef;

#[derive(Clone, Copy, Debug)]
pub enum Match {
    Id { vendor: u16, device: u16 },
    /// class/subclass/prog-if with a mask (e.g. xHCI 0x0C0330, mask 0xFFFFFF).
    Class { class: u32, mask: u32 },
}

impl Match {
    pub fn hits(&self, vendor: u16, device: u16, class: u32) -> bool {
        match *self {
            Match::Id { vendor: v, device: d } => vendor == v && device == d,
            Match::Class { class: c, mask } => class & mask == c & mask,
        }
    }
}

/// How well a driver's match list fits a function: `None` = no match; smaller = tried earlier. An exact ID
/// is (0, 0); a class match is (1, unmatched bits), so 0x0C0330/0xFFFFFF comes before 0x0C0000/0xFF0000.
pub fn rank(matches: &[Match], vendor: u16, device: u16, class: u32) -> Option<(u8, u8)> {
    matches
        .iter()
        .filter(|m| m.hits(vendor, device, class))
        .map(|m| match *m {
            Match::Id { .. } => (0, 0),
            Match::Class { mask, .. } => (1, 32 - (mask & 0xFF_FFFF).count_ones() as u8),
        })
        .min()
}

/// A driver as the binder sees it (from `DRIVERS`, or a ktest's own list that never reaches the registry).
#[derive(Clone, Copy)]
pub struct Candidate {
    pub name: &'static str,
    pub matches: &'static [Match],
    pub probe: fn(&PciFunction) -> Result<(), String>,
}

impl Candidate {
    pub fn of(d: &DriverEntry) -> Candidate {
        Candidate { name: d.name, matches: d.matches, probe: d.probe }
    }
}

/// The drivers that match a function, in the order they are tried: rank, then name; a second entry with a
/// name already listed is dropped (`duplicates` reports it once at boot).
pub fn ordered(cands: &[Candidate], vendor: u16, device: u16, class: u32) -> alloc::vec::Vec<(Candidate, (u8, u8))> {
    let mut v: alloc::vec::Vec<(Candidate, (u8, u8))> =
        cands.iter().filter_map(|c| rank(c.matches, vendor, device, class).map(|r| (*c, r))).collect();
    // Stable sort; a duplicate name keeps only its best-ranked entry.
    v.sort_by(|a, b| (a.1, a.0.name).cmp(&(b.1, b.0.name)));
    let mut seen: alloc::vec::Vec<&'static str> = alloc::vec::Vec::new();
    v.retain(|(c, _)| {
        let first = !seen.contains(&c.name);
        seen.push(c.name);
        first
    });
    v
}

/// Driver names registered more than once (a registry mistake: only the first entry is ever tried).
pub fn duplicates(cands: &[Candidate]) -> alloc::vec::Vec<&'static str> {
    let mut names: alloc::vec::Vec<&'static str> = cands.iter().map(|c| c.name).collect();
    names.sort_unstable();
    let mut d: alloc::vec::Vec<&'static str> = names.windows(2).filter(|w| w[0] == w[1]).map(|w| w[0]).collect();
    d.dedup();
    d
}

/// Every registered driver as a candidate.
pub fn registered() -> alloc::vec::Vec<Candidate> {
    DRIVERS.entries().iter().map(Candidate::of).collect()
}

pub struct DriverEntry {
    pub name: &'static str,
    pub matches: &'static [Match],
    pub probe: fn(&PciFunction) -> Result<(), String>,
}

kreg::declare!(pub DRIVERS: DriverEntry = "pcidrv");

#[macro_export]
macro_rules! pci_driver {
    ($entry:expr) => {
        $crate::__kreg::register!("pcidrv", $crate::DriverEntry, $entry);
    };
}

static CONFIG: OnceRef<dyn Fn(&str) -> Option<String> + Sync> = OnceRef::new();

/// The kernel hands in its CONFIG lookup (the profile, the guards, test keys).
pub fn set_config(f: &'static (dyn Fn(&str) -> Option<String> + Sync)) {
    CONFIG.set(f);
}

/// The value of a CONFIG key, as the kernel's lookup gives it.
pub fn config_value(key: &str) -> Option<String> {
    CONFIG.get().and_then(|f| f(key))
}

/// A test key (`usb_xfer_test=on` and the like) is `on` in this boot's CONFIG.
pub fn opted_in(key: &str) -> bool {
    CONFIG.get().and_then(|f| f(key)).is_some_and(|v| v.trim() == "on")
}

/// The test key `ktest_poll=<driver>,...` names `driver` (`nvme`, `hda`): the scenarios that test the polled path a
/// device without a working interrupt takes boot with it.
pub fn poll_test(driver: &str) -> bool {
    poll_named(config_value("ktest_poll").as_deref(), driver)
}

/// [`poll_test`] over a value (pure).
pub fn poll_named(value: Option<&str>, driver: &str) -> bool {
    value.is_some_and(|v| v.split(',').any(|d| d.trim() == driver))
}

/// How a function left untouched by key `k` is released, for messages: never for the Gaming-PC's never-touch
/// list (and its WLAN rule), by Georg's tier line `gp_net=on` for the network there, while `nvme_bind=off` for
/// the disk guard.
#[derive(Clone, Copy, Debug)]
pub struct Until(pub &'static str);

impl core::fmt::Display for Until {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self.0 {
            // B4: no switch releases a function of the Gaming-PC's never-touch list.
            k if gp_platform::never_touch::is_key(k) => write!(f, "for good on this PC ({k})"),
            k if k == gp_platform::tiers::NET_KEY => write!(f, "until {k}=on"),
            "nvme_bind" => write!(f, "while nvme_bind=off"),
            k => write!(f, "by {k}"),
        }
    }
}

/// GP-GUARD (plan/spec/gaming-pc.md §2): true for a mass-storage function (class 01: NVMe, AHCI, RAID and Intel VMD,
/// IDE, SCSI, SAS; linux-7.3.1/include/linux/pci_ids.h:18-28) while CONFIG says `nvme_bind=off`. The Gaming-PC
/// profile forces that line: Georg's internal disks get no driver, no BAR sizing and no bus master there. The same
/// rule, with its truth table, is `gp_platform::disk_rule::kept_unbound` (nvme's host test holds the two together).
pub fn storage_kept_off(class: u32, nvme_bind: Option<&str>) -> bool {
    class >> 16 == 0x01 && nvme_bind.is_some_and(|v| v.trim() == "off")
}

/// GP-DISK: [`storage_kept_off`] for the function at `place` with `ids`: the one NVMe controller CONFIG's `gp_nvme=`
/// names (place and ids) is let go, every other storage function stays as `nvme_bind=off` keeps it
/// (`gp_platform::disk_rule::named::verdict`). Without a place, or without `nvme_bind=off`, this is
/// [`storage_kept_off`].
pub fn storage_kept_off_at(class: u32, nvme_bind: Option<&str>, gp_nvme: Option<&str>, place: Option<(u8, u8, u8)>, ids: (u16, u16)) -> bool {
    use gp_platform::disk_rule::named::{Verdict, verdict};
    storage_kept_off(class, nvme_bind) && !place.is_some_and(|p| verdict(class, nvme_bind, gp_nvme, p, ids) == Verdict::Released)
}

/// The key that keeps a function untouched, if any: the Gaming-PC profile's rules (`gp`), the disk guard, or the
/// test function (`optin::TEST_HELD`).
pub fn untouched_key(vendor: u16, device: u16, class: u32) -> Option<&'static str> {
    untouched_key_at(None, vendor, device, class)
}

/// [`untouched_key`] for a function whose place the scan knows (GP-DISK: `gp_nvme=` names a place).
pub fn untouched_key_at(place: Option<(u8, u8, u8)>, vendor: u16, device: u16, class: u32) -> Option<&'static str> {
    // B4: the Gaming-PC's never-touch list comes first, then the profile's network rule (WLAN never, the cable
    // only with Georg's `gp_net=on`). Without `profile=gaming-pc` both are `None`.
    let active = crate::gp::active();
    if let Some(k) = crate::gp::never_touch_key(active, vendor, device, class) {
        return Some(k);
    }
    let net = config_value(gp_platform::tiers::NET_KEY);
    if let Some(k) = crate::gp::network_key(active, gp_platform::tiers::Tiers::from_values(net.as_deref(), None, None).net, class) {
        return Some(k);
    }
    // GP-GUARD: `nvme_bind=off` keeps every mass-storage function untouched. Without the line (the Mini-PC, QEMU)
    // nothing changes here.
    let bind = config_value("nvme_bind");
    if storage_kept_off(class, bind.as_deref()) && storage_kept_off_at(class, bind.as_deref(), config_value(gp_platform::tiers::NVME_KEY).as_deref(), place, (vendor, device)) {
        return Some("nvme_bind");
    }
    crate::optin::TEST_HELD.iter().find(|(m, _)| m.hits(vendor, device, class)).map(|(_, k)| *k)
}

