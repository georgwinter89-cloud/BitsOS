//! Driver registry (contract `pci_driver!` with `Match` and `opt_in_key`, unchanged since G0), the opt-in
//! keys, and the defined order in which drivers are tried (PC-4): an exact ID match before any class
//! match, a narrower class mask before a wider one, then the driver name. Binding itself is `bind.rs`.

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
    pub opt_in_key: Option<&'static str>,
    pub probe: fn(&PciFunction) -> Result<(), String>,
}

impl Candidate {
    pub fn of(d: &DriverEntry) -> Candidate {
        Candidate { name: d.name, matches: d.matches, opt_in_key: d.opt_in_key, probe: d.probe }
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
    /// R8d: the driver attaches only if this CONFIG key is `on` (e.g. "wlan"); until then the PCI core never
    /// touches the function (no enable, no BAR sizing).
    pub opt_in_key: Option<&'static str>,
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

/// The kernel hands in its CONFIG lookup (opt-in keys).
pub fn set_config(f: &'static (dyn Fn(&str) -> Option<String> + Sync)) {
    CONFIG.set(f);
}

/// The value of a CONFIG key, as the kernel's lookup gives it.
pub fn config_value(key: &str) -> Option<String> {
    CONFIG.get().and_then(|f| f(key))
}

pub fn opted_in(key: &str) -> bool {
    CONFIG.get().and_then(|f| f(key)).is_some_and(|v| v.trim() == "on")
}

/// True for a network-class function (class 02) while CONFIG says `net_drivers=off`.
/// How a function left untouched by key `k` is released, for messages: "until wlan=on" for an opt-in key,
/// "while net_drivers=off" for BT-5's safe mode (any other value of net_drivers binds; INT-W4).
#[derive(Clone, Copy, Debug)]
pub struct Until(pub &'static str);

impl core::fmt::Display for Until {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        if self.0 == "net_drivers" { write!(f, "while net_drivers=off") } else { write!(f, "until {}=on", self.0) }
    }
}

pub fn net_kept_off(class: u32, net_drivers: Option<&str>) -> bool {
    class >> 16 == 0x02 && net_drivers.is_some_and(|v| v.trim() == "off")
}

/// The key that keeps a function untouched, if any: an opt-in driver that matches it with its key off, or
/// the core list (`optin::CORE_OPT_IN`).
pub fn untouched_key(vendor: u16, device: u16, class: u32) -> Option<&'static str> {
    // BT-5 safe mode: `net_drivers=off` keeps every network-class function untouched, whatever driver
    // would match it, so no later NIC driver can bind on a machine booted that way.
    if net_kept_off(class, config_value("net_drivers").as_deref()) {
        return Some("net_drivers");
    }
    let drivers = DRIVERS.entries().iter().filter_map(|d| d.opt_in_key.map(|k| (d.matches, k)));
    for (ms, key) in drivers {
        if ms.iter().any(|m| m.hits(vendor, device, class)) && !opted_in(key) {
            return Some(key);
        }
    }
    crate::optin::CORE_OPT_IN.iter().find(|(m, k)| m.hits(vendor, device, class) && !opted_in(k)).map(|(_, k)| *k)
}

