//! Whose machine this is by its DMI names, read the same way by the loader and by the kernel (GP-GUARD): the
//! Gaming-PC's board must be recognised by both, because either may be older than the other (install_kernel
//! renews only the kernel, a bundle may renew only bitsboot), and a CONFIG line `profile=normal` that only the
//! loader refuses is no protection when that loader is an old one.
//!
//! Three grades, so that a firmware update which writes the names a little differently does not open the disks:
//! the exact names, the board family's name anywhere in the four strings, and the vendor alone (which counts only
//! together with an Intel CPU on bare metal and a stick that asks for `profile=normal`).

use super::{NEVER_RELAXED, PROFILE, REFUSED_KEY, is_gaming_pc, kernel_fallback};
use alloc::string::String;
use alloc::vec::Vec;

/// `/sys/class/dmi/id/{sys_vendor,board_vendor}` and `{product_name,board_name}` on CachyOS (read 14.11.2026).
pub const VENDOR: &str = "ASRock";
pub const PRODUCT: &str = "Z890 Taichi OCF";
/// What every spelling of the board's name keeps (compared without spaces, signs and case).
const FAMILY: &str = "z890taichi";
const VENDOR_START: &str = "asrock";

/// The four identity strings; `None` or blank when the table has none.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Names<'a> {
    pub sys_vendor: Option<&'a str>,
    pub product: Option<&'a str>,
    pub board_vendor: Option<&'a str>,
    pub board_name: Option<&'a str>,
}

impl Names<'_> {
    fn all(&self) -> [Option<&str>; 4] {
        [self.sys_vendor, self.product, self.board_vendor, self.board_name]
    }
    /// No name at all: no table, a cut table, or only blank strings.
    pub fn is_empty(&self) -> bool {
        self.all().iter().all(|s| s.is_none_or(|s| s.trim().is_empty()))
    }
}

/// How much the names look like the Gaming-PC's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Likeness {
    /// Vendor and product of the system or of the board structure are the Gaming-PC's.
    Exact,
    /// One of the four strings carries the board family's name ("ASRock Inc.", "Z890 Taichi OCF/BIOS 2.x", a cut
    /// table with only one structure).
    Family,
    /// Only the vendor fits.
    Vendor,
    None,
}

/// Letters and digits only, lower case.
fn squeezed(s: &str) -> String {
    s.chars().filter(char::is_ascii_alphanumeric).map(|c| c.to_ascii_lowercase()).collect()
}

fn same(a: Option<&str>, b: &str) -> bool {
    a.is_some_and(|a| a.trim().eq_ignore_ascii_case(b))
}

pub fn likeness(n: &Names) -> Likeness {
    if (same(n.sys_vendor, VENDOR) && same(n.product, PRODUCT)) || (same(n.board_vendor, VENDOR) && same(n.board_name, PRODUCT)) {
        return Likeness::Exact;
    }
    if n.all().iter().flatten().any(|s| squeezed(s).contains(FAMILY)) {
        return Likeness::Family;
    }
    if [n.sys_vendor, n.board_vendor].iter().flatten().any(|s| squeezed(s).starts_with(VENDOR_START)) {
        return Likeness::Vendor;
    }
    Likeness::None
}

/// The names say this is the Gaming-PC's board, whatever the CPU and whatever the stick says.
pub fn is_board(n: &Names) -> bool {
    matches!(likeness(n), Likeness::Exact | Likeness::Family)
}

/// A stick line `profile=normal` does not end safe mode on an Intel CPU on bare metal whose names could not be
/// read or carry the board's vendor: that may be the Gaming-PC after all, and its disks come first.
pub fn normal_refused_on_intel(n: Option<&Names>) -> bool {
    n.is_none_or(|n| n.is_empty() || likeness(n) != Likeness::None)
}

/// Why the kernel holds the profile up itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KernelGuard {
    /// The record says `profile=gaming-pc` (the loader decided): only the never-relaxed lines are imposed again.
    Record,
    /// The loader did not decide for the profile although this machine needs it: everything off, no tier.
    /// The text names the cause for the log (`no_profile`, `dmi`, `dmi_unreadable`, `dmi_vendor`, `profile_value`).
    Fallback(&'static str),
}

/// The kernel's own decision, from the CONFIG record's `profile=` value, the CPU and the DMI names
/// (`names: None` while they are not read yet, early in the boot: then only the record and the CPU count).
/// It mirrors the loader's rule (`bitsboot_pure::profile::decide`), so a record a current loader would never
/// have handed over on this machine is not trusted:
/// - the board's names beat every `profile=` value;
/// - on an Intel CPU on bare metal a current loader hands over `profile=gaming-pc`, or `profile=normal` only
///   when it read another vendor's names; no line, any other value, or `normal` without such names is an older
///   loader's record.
/// The Mini-PC (AMD, another board) and QEMU without the board's names never match.
pub fn kernel_guard(record: Option<&str>, intel: bool, hypervisor: bool, names: Option<&Names>) -> Option<KernelGuard> {
    if is_gaming_pc(record) {
        return Some(KernelGuard::Record);
    }
    let bare_intel = intel && !hypervisor;
    if record.is_none() && bare_intel {
        return Some(KernelGuard::Fallback("no_profile"));
    }
    let n = names?;
    if is_board(n) {
        return Some(KernelGuard::Fallback("dmi"));
    }
    if !bare_intel {
        return None;
    }
    match record.map(|r| r.trim().to_ascii_lowercase()).as_deref() {
        Some("normal") if n.is_empty() => Some(KernelGuard::Fallback("dmi_unreadable")),
        Some("normal") if likeness(n) == Likeness::Vendor => Some(KernelGuard::Fallback("dmi_vendor")),
        Some("normal") => None,
        // `auto`, an empty or an unknown value: a current loader would have chosen the profile here.
        _ => Some(KernelGuard::Fallback("profile_value")),
    }
}

/// The lines the kernel puts over the record for `g` (`record`: the record's own `profile=` value).
pub fn kernel_lines(g: KernelGuard, record: Option<&str>) -> Vec<(&'static str, &'static str)> {
    match g {
        KernelGuard::Record => NEVER_RELAXED.to_vec(),
        KernelGuard::Fallback(_) => {
            let mut l = kernel_fallback();
            l.extend([("profile", PROFILE), ("profile_reason", "kernel")]);
            if record.is_some_and(|r| r.trim().eq_ignore_ascii_case("normal")) {
                l.push((REFUSED_KEY, "normal"));
            }
            l
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tiers::impose;
    use alloc::collections::BTreeMap;
    extern crate std;

    fn n<'a>(sv: &'a str, p: &'a str, bv: &'a str, bn: &'a str) -> Names<'a> {
        let o = |s: &'a str| (!s.is_empty()).then_some(s);
        Names { sys_vendor: o(sv), product: o(p), board_vendor: o(bv), board_name: o(bn) }
    }

    const BOARD: Names<'static> = Names { sys_vendor: Some("ASRock"), product: Some("Z890 Taichi OCF"), board_vendor: Some("ASRock"), board_name: Some("Z890 Taichi OCF") };
    const MINI: Names<'static> = Names { sys_vendor: Some("AZW"), product: Some("SER8"), board_vendor: Some("AZW"), board_name: Some("SER8") };
    const QEMU: Names<'static> = Names { sys_vendor: Some("QEMU"), product: Some("Standard PC (Q35 + ICH9, 2009)"), board_vendor: None, board_name: None };

    #[test]
    fn the_board_in_every_spelling() {
        assert_eq!(likeness(&BOARD), Likeness::Exact);
        assert_eq!(likeness(&n("To Be Filled By O.E.M.", "To Be Filled By O.E.M.", " asrock ", "z890 taichi ocf")), Likeness::Exact);
        assert_eq!(likeness(&n("ASRock", "Z890 Taichi OCF ", "", "")), Likeness::Exact, "padding is no other name");
        // What a firmware update or a cut table could leave: still the board.
        for names in [
            n("ASRock Inc.", "Z890 Taichi OCF", "ASRock Inc.", "Z890 Taichi OCF"),
            n("ASRock", "Z890 Taichi OCF/BIOS 3.10", "", ""),
            n("ASRock", "Z890-Taichi-OCF", "", ""),
            n("ASRock", "Z890  TAICHI  OCF", "", ""),
            n("", "", "ASRockRack", "Z890 Taichi"),
            n("", "Z890 Taichi OCF", "", ""),
            n("", "", "", "Z890 Taichi Lite"),
            n("System manufacturer", "System Product Name", "", "ASRock Z890 Taichi OCF"),
        ] {
            assert_eq!(likeness(&names), Likeness::Family, "{names:?}");
            assert!(is_board(&names));
        }
        // The vendor alone, in its spellings.
        for names in [n("ASRock", "B650M Pro RS", "ASRock", "B650M Pro RS"), n("ASRock Inc.", "", "", ""), n("", "", "ASROCK INCORPORATION", "Z790 Nova")] {
            assert_eq!(likeness(&names), Likeness::Vendor, "{names:?}");
            assert!(!is_board(&names));
        }
        for names in [MINI, QEMU, n("", "", "", ""), n("Micro-Star", "MS-7E06", "", ""), n("ASUS", "ROG Z890", "", "Taichi")] {
            assert_eq!(likeness(&names), Likeness::None, "{names:?}");
        }
        assert!(Names::default().is_empty() && n(" ", "", "", "").is_empty() && !MINI.is_empty());
    }

    #[test]
    fn profile_normal_on_intel_needs_another_vendors_names() {
        assert!(normal_refused_on_intel(None));
        assert!(normal_refused_on_intel(Some(&Names::default())));
        assert!(normal_refused_on_intel(Some(&n("ASRock Inc.", "Z790 Nova", "", ""))));
        assert!(normal_refused_on_intel(Some(&BOARD)));
        assert!(!normal_refused_on_intel(Some(&n("Micro-Star", "MS-7E06", "", ""))));
        assert!(!normal_refused_on_intel(Some(&QEMU)));
    }

    /// The finding this closes: a record that says `profile=normal` (or anything else) on the board is an older
    /// loader's; the kernel imposes the profile whatever the key says. The Mini-PC and plain QEMU never match.
    #[test]
    fn the_kernel_does_not_trust_the_profile_key_on_the_board() {
        use KernelGuard::{Fallback, Record};
        let s = |x: &str| std::string::String::from(x);
        for record in [Some("normal"), Some(" NORMAL "), Some("auto"), Some(""), Some("nomral"), None] {
            for (intel, hv) in [(true, false), (true, true), (false, false), (false, true)] {
                let g = kernel_guard(record, intel, hv, Some(&BOARD));
                let want = if record.is_none() && intel && !hv { Fallback("no_profile") } else { Fallback("dmi") };
                assert_eq!(g, Some(want), "{record:?} intel={intel} hv={hv}");
                // What the kernel then holds, over a record that asks for everything.
                let mut m: BTreeMap<_, _> = [("nvme_bind", "on"), ("nvme_write", "on"), ("nvme_data", "S:G:1:2"), ("nvme_boot", "S:G:3:4"), ("usb_msd_write", "on"), ("stick_write", "on"), ("gp_net", "on"), ("gp_stick", "on")]
                    .into_iter()
                    .map(|(k, v)| (s(k), s(v)))
                    .collect();
                if let Some(r) = record {
                    m.insert(s("profile"), s(r));
                }
                impose(&mut m, &kernel_lines(g.unwrap(), record));
                for (k, v) in [("nvme_bind", "off"), ("nvme_write", "off"), ("usb_msd_write", "off"), ("stick_write", "off"), ("gp_net", "off"), ("gp_stick", "off"), ("profile", "gaming-pc"), ("profile_reason", "kernel")] {
                    assert_eq!(m.get(k).map(|x| x.as_str()), Some(v), "{k} after {record:?}");
                }
                assert!(!m.contains_key("nvme_data") && !m.contains_key("nvme_boot"));
                assert_eq!(m.contains_key(REFUSED_KEY), record.is_some_and(|r| r.trim().eq_ignore_ascii_case("normal")));
            }
        }
        // The loader's own decision: only the never-relaxed lines again, the tiers stay the loader's.
        for names in [None, Some(&BOARD), Some(&MINI)] {
            assert_eq!(kernel_guard(Some("gaming-pc"), false, true, names), Some(Record));
            assert_eq!(kernel_guard(Some(" Gaming-PC"), true, false, names), Some(Record));
        }
        assert_eq!(kernel_lines(Record, Some("gaming-pc")), NEVER_RELAXED.to_vec());
        // Family spellings count like the exact names.
        assert_eq!(kernel_guard(Some("normal"), false, false, Some(&n("ASRock Inc.", "Z890 Taichi OCF", "", ""))), Some(Fallback("dmi")));
    }

    #[test]
    fn other_machines_are_left_alone() {
        use KernelGuard::Fallback;
        // The Mini-PC: AMD on bare metal, its own names, with or without any profile line.
        for record in [None, Some("normal"), Some("auto"), Some("nomral")] {
            assert_eq!(kernel_guard(record, false, false, Some(&MINI)), None, "{record:?}");
            assert_eq!(kernel_guard(record, false, false, None), None);
            assert_eq!(kernel_guard(record, false, false, Some(&Names::default())), None, "no names on AMD: nothing to go by");
        }
        // QEMU on the Gaming-PC's own Intel CPU (every scenario): the hypervisor bit keeps the normal profile.
        for record in [None, Some("normal"), Some("auto")] {
            assert_eq!(kernel_guard(record, true, true, Some(&QEMU)), None, "{record:?}");
            assert_eq!(kernel_guard(record, true, true, None), None);
            assert_eq!(kernel_guard(record, true, true, Some(&Names::default())), None);
        }
        // A tester's Intel PC of another vendor: `profile=normal` ends safe mode, as the loader has it.
        let msi = n("Micro-Star", "MS-7E06", "Micro-Star", "MAG Z890");
        assert_eq!(kernel_guard(Some("normal"), true, false, Some(&msi)), None);
        // The same PC with a record only an older loader hands over: the profile.
        assert_eq!(kernel_guard(None, true, false, Some(&msi)), Some(Fallback("no_profile")));
        assert_eq!(kernel_guard(None, true, false, None), Some(Fallback("no_profile")), "before the names are read");
        assert_eq!(kernel_guard(Some("auto"), true, false, Some(&msi)), Some(Fallback("profile_value")));
        assert_eq!(kernel_guard(Some("auto"), true, false, None), None, "early: wait for the names");
        // Intel on bare metal, `profile=normal`, names unreadable or the board's vendor: may be the Gaming-PC.
        assert_eq!(kernel_guard(Some("normal"), true, false, Some(&Names::default())), Some(Fallback("dmi_unreadable")));
        assert_eq!(kernel_guard(Some("normal"), true, false, Some(&n("ASRock", "Z790 Nova", "", ""))), Some(Fallback("dmi_vendor")));
        assert_eq!(kernel_guard(Some("normal"), true, false, None), None, "early: wait for the names");
    }
}
