//! The Gaming-PC profile's tiers (plan/spec/gaming-pc.md §2, wave GP-GUARD): which CONFIG lines the loader forces
//! after the stick's own file, and what a tier line in the stick's CONFIG.TXT releases. The profile starts with
//! everything off; a tier releases exactly one thing:
//! - `gp_net=on`: the cable network drivers (the PCI core holds every network function of the profile until this
//!   line, `pci::gp::network_key`; WLAN stays held for good);
//! - `gp_stick=on`: the boot volume itself (the loader hands STICK.ID to the kernel and follows the normal
//!   generation rule, so a new generation can be installed on it over the network). Booted from the stick that is
//!   the stick; booted from the `nvme_boot=` partition of the named SSD (GP-DISK) it is that partition, below
//!   `\BITSOS2\` and only under the Mini-PC's rules for it;
//! - `gp_nvme=<bb:dd.f>/<vvvv:dddd>` (GP-DISK): the one NVMe controller it names gets the driver, and on it the
//!   Mini-PC's three lines `nvme_write`, `nvme_data`, `nvme_boot` stand as the stick says (they are not forced away).
//!
//! No tier, no stick line and no `profile=normal` relaxes the other internal disks, other USB disks or WLAN (the PCI
//! core holds every wireless controller under the profile): [`never_relaxed`] is forced in every tier (`nvme_bind=off` too: it keeps every other storage function untouched). `stick_write` stays off under `gp_stick=on` too: that switch opens
//! the stick (and any USB disk that cannot be told apart from it) wholesale, while the handed-over STICK.ID opens
//! only the one identified stick through the FAT gate.

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;

pub mod board;

/// The profile's name in CONFIG `profile=`.
pub const PROFILE: &str = "gaming-pc";

pub const NET_KEY: &str = "gp_net";
pub const STICK_KEY: &str = "gp_stick";
pub use crate::disk_rule::named::NVME_KEY;
/// GP-DISK: the loader's own word that it read `gp_nvme=` and found it naming a controller. A bitsboot older than
/// GP-DISK hands the stick's `gp_nvme=` line through untouched while its screen says no internal disk gets a
/// driver; without this line from the loader the kernel releases nothing ([`Tiers::from_loader`]).
pub const NVME_LOADER_KEY: &str = "gp_nvme_loader";

/// What the stick's CONFIG.TXT released.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Tiers {
    pub net: bool,
    pub stick: bool,
    /// `gp_nvme=` names a controller (`disk_rule::named::named`).
    pub nvme: bool,
}

/// A tier line counts only as exactly `on` (spaces around ignored), like every opt-in key of the kernel.
fn on(v: Option<&str>) -> bool {
    v.is_some_and(|v| v.trim() == "on")
}

impl Tiers {
    /// Everything off: the profile without any tier line.
    pub const SAFE: Tiers = Tiers { net: false, stick: false, nvme: false };

    /// The tiers a CONFIG lookup (last line wins) names.
    pub fn read<'a>(get: impl Fn(&str) -> Option<&'a str>) -> Tiers {
        Tiers::from_values(get(NET_KEY), get(STICK_KEY), get(NVME_KEY))
    }

    /// The same from the three values (for a lookup that hands out owned strings).
    pub fn from_values(net: Option<&str>, stick: Option<&str>, nvme: Option<&str>) -> Tiers {
        Tiers { net: on(net), stick: on(stick), nvme: crate::disk_rule::named::named(nvme).is_some() }
    }

    /// The kernel's reading of a record the loader made: as [`Tiers::read`], but `gp_nvme` counts only when the loader
    /// wrote [`NVME_LOADER_KEY`]`=on` after it (the kernel acts on the loader's decision, not on the stick's line).
    pub fn from_loader<'a>(get: impl Fn(&str) -> Option<&'a str>) -> Tiers {
        let t = Tiers::read(&get);
        Tiers { nvme: t.nvme && on(get(NVME_LOADER_KEY)), ..t }
    }

    pub fn all() -> [Tiers; 8] {
        core::array::from_fn(|b| Tiers { net: b & 1 != 0, stick: b & 2 != 0, nvme: b & 4 != 0 })
    }

    /// For the loader's log line (without a named controller what it was before GP-DISK).
    pub fn name(self) -> &'static str {
        match (self.net, self.stick, self.nvme) {
            (false, false, false) => "none",
            (true, false, false) => "net",
            (false, true, false) => "stick",
            (true, true, false) => "net,stick",
            (false, false, true) => "nvme",
            (true, false, true) => "net,nvme",
            (false, true, true) => "stick,nvme",
            (true, true, true) => "net,stick,nvme",
        }
    }

    /// The loader keeps STICK.ID from the kernel, so /boot is mounted read-only.
    pub fn stick_id_withheld(self) -> bool {
        !self.stick
    }

    /// The loader writes no generation marker (a new generation keeps its one try for another machine).
    pub fn markers_read_only(self) -> bool {
        !self.stick
    }
}

/// Forced in every tier without a named controller, after everything else: internal disks (no driver, no write, no
/// window, no boot partition there; an empty value takes the stick's line away), other USB disks, the raw stick
/// switch. With `gp_nvme=` naming one, [`never_relaxed`] leaves out [`NVME_RELEASED`].
pub const NEVER_RELAXED: &[(&str, &str)] = &[
    ("nvme_bind", "off"),
    ("nvme_write", "off"),
    ("nvme_data", ""),
    ("nvme_boot", ""),
    ("usb_msd_write", "off"),
    ("stick_write", "off"),
];

/// GP-DISK: the Mini-PC's lines for its SSD, which stand on the named controller as the stick says them.
pub const NVME_RELEASED: &[&str] = &["nvme_write", "nvme_data", "nvme_boot"];

/// What is forced in every case for the tiers `t`: [`NEVER_RELAXED`], less [`NVME_RELEASED`] with a named
/// controller, and without one a `gp_nvme=` line taken away (garbled, or naming Georg's Windows or CachyOS drive).
/// `nvme_bind=off` stays: every storage function but the named one stays untouched.
pub fn never_relaxed(t: Tiers) -> Vec<(&'static str, &'static str)> {
    let mut v: Vec<_> = NEVER_RELAXED.iter().copied().filter(|(k, _)| !(t.nvme && NVME_RELEASED.contains(k))).collect();
    if !t.nvme {
        v.extend([(NVME_KEY, ""), (NVME_LOADER_KEY, "")]);
    }
    v
}

/// GP-DISK: what the kernel takes away from a record whose profile is not gaming-pc: `gp_nvme` is that profile's
/// tier and releases nothing elsewhere (without the line, as on the Mini-PC, nothing changes).
pub const OUTSIDE_PROFILE: &[(&str, &str)] = &[(NVME_KEY, ""), (NVME_LOADER_KEY, "")];

/// GP-DISK: whether a CONFIG lookup releases the controller `gp_nvme=` names: the Gaming-PC profile, a line naming a
/// controller, and the loader's word behind it (for readers of the record itself, such as the block layer; the
/// kernel's own map has the line taken away otherwise).
pub fn nvme_released(profile: Option<&str>, gp_nvme: Option<&str>, loader: Option<&str>) -> bool {
    is_gaming_pc(profile) && crate::disk_rule::named::named(gp_nvme).is_some() && on(loader)
}

/// Forced in every tier: the loader keeps the firmware's picture (read by the loader itself, not by the kernel).
pub const SCREEN_FORCED: (&str, &str) = ("screen", "firmware");

fn word(on: bool) -> &'static str {
    if on { "on" } else { "off" }
}

/// The lines the loader appends after the stick's CONFIG.TXT for `t`, in order. The two on/off tier lines come
/// back normalised, so the kernel reads the loader's decision and not the stick's spelling. A `gp_nvme=` line that
/// names no controller (garbled, or one of Georg's two system drives) is taken away; one that does stays as the
/// stick's own line (the kernel compares it with the scan itself) and gets [`NVME_LOADER_KEY`]`=on` behind it.
pub fn forced(t: Tiers) -> Vec<(&'static str, &'static str)> {
    let mut v = Vec::with_capacity(NEVER_RELAXED.len() + 5);
    v.push((NET_KEY, word(t.net)));
    v.push((STICK_KEY, word(t.stick)));
    if t.nvme {
        v.push((NVME_LOADER_KEY, "on"));
    }
    v.extend(never_relaxed(t));
    v.push(SCREEN_FORCED);
    v
}

/// What the kernel imposes itself when the CONFIG record names no profile on an Intel CPU on bare metal (a stick
/// whose loader is older than the profile): no tier at all. The picture is the loader's business.
pub fn kernel_fallback() -> Vec<(&'static str, &'static str)> {
    forced(Tiers::SAFE).into_iter().filter(|(k, _)| *k != SCREEN_FORCED.0).collect()
}

/// CONFIG `profile=` names the Gaming-PC profile (any case, as the Reload's check reads it).
pub fn is_gaming_pc(profile: Option<&str>) -> bool {
    profile.is_some_and(|p| p.trim().eq_ignore_ascii_case(PROFILE))
}

/// Puts `lines` over the kernel's CONFIG map (an empty value takes the key away) and returns the keys whose
/// value changed. The kernel imposes [`NEVER_RELAXED`] itself under the profile, so a loader older than a
/// forced line cannot leave it open. A key the loader already emptied (`nvme_data=`) goes without counting as
/// a change: that is this loader's own way of taking it away.
pub fn impose(map: &mut BTreeMap<String, String>, lines: &[(&'static str, &'static str)]) -> Vec<&'static str> {
    let mut changed = Vec::new();
    for (k, v) in lines {
        if v.is_empty() {
            if map.remove(*k).is_some_and(|old| !old.is_empty()) {
                changed.push(*k);
            }
        } else if map.get(*k).is_none_or(|old| old != v) {
            map.insert(String::from(*k), String::from(*v));
            changed.push(*k);
        }
    }
    changed
}

/// The switches that could let a write reach an internal disk or another USB disk if they were `on`.
pub const WRITE_SWITCHES: &[&str] = &["nvme_write", "usb_msd_write", "stick_write"];

/// The write switches forced off for `t` (with a named controller `nvme_write` is the stick's, for that one).
pub fn write_switches(t: Tiers) -> impl Iterator<Item = &'static str> {
    WRITE_SWITCHES.iter().copied().filter(move |k| !(t.nvme && NVME_RELEASED.contains(k)))
}

/// CONFIG key the loader adds when the stick asked for `profile=normal` and the machine did not honour it.
pub const REFUSED_KEY: &str = "profile_refused";

/// The screen report's lines for the profile and its tiers (plain German for Georg; wave B10 prints them).
/// `reason`: CONFIG `profile_reason` (`dmi`, `cpuid`, `config`, `kernel`); `normal_refused`: CONFIG has
/// [`REFUSED_KEY`].
pub fn report_lines(t: Tiers, reason: &str, normal_refused: bool) -> Vec<&'static str> {
    let mut v = Vec::with_capacity(6);
    v.push(match reason {
        "dmi" => "Sicherer Modus für den Gaming-PC (am Mainboard erkannt).",
        "cpuid" => "Sicherer Modus (Intel-Prozessor, das Mainboard wurde nicht erkannt).",
        "config" => "Sicherer Modus (so steht es in CONFIG.TXT auf dem Stick).",
        "kernel" => "Sicherer Modus (das Startprogramm auf dem Stick ist zu alt, alles bleibt aus).",
        _ => "Sicherer Modus.",
    });
    if normal_refused {
        v.push("Auf dem Stick steht profile=normal. Das gilt an diesem PC nicht.");
    }
    v.push(if t.net { "Netzwerk über Kabel: an. WLAN bleibt aus." } else { "Netzwerk: aus (Kabel und WLAN)." });
    v.push(if t.stick { "Start-Stick: darf beschrieben werden (neue Versionen aus der Ferne)." } else { "Start-Stick: wird nur gelesen." });
    v.push("Andere USB-Datenträger: werden nur gelesen.");
    v.push(if t.nvme {
        "Eingebaute Platten: nur die mit gp_nvme genannte bekommt einen Treiber (Regeln wie am Mini-PC), die anderen nicht."
    } else {
        "Eingebaute Platten: kein Treiber, nicht gelesen."
    });
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    extern crate std;

    /// Last line wins, an empty value takes the key away (how the kernel applies the never-relaxed lines).
    fn effective(lines: &[(&str, &str)]) -> BTreeMap<std::string::String, std::string::String> {
        let mut m = BTreeMap::new();
        for (k, v) in lines {
            if v.is_empty() {
                m.remove(*k);
            } else {
                m.insert(std::string::String::from(*k), std::string::String::from(*v));
            }
        }
        m
    }

    #[test]
    fn tier_lines_count_only_as_exactly_on() {
        let read = |net: Option<&'static str>, stick: Option<&'static str>| {
            Tiers::read(move |k| if k == NET_KEY { net } else if k == STICK_KEY { stick } else { None })
        };
        assert_eq!(read(None, None), Tiers::SAFE);
        assert_eq!(read(Some("on"), None), Tiers { net: true, ..Tiers::SAFE });
        assert_eq!(read(Some(" on\r"), Some("on")), Tiers { net: true, stick: true, nvme: false });
        for odd in ["ON", "On", "1", "yes", "true", "", "off", "on!", "o n"] {
            assert_eq!(read(Some(odd), Some(odd)), Tiers::SAFE, "{odd:?}");
        }
        // Each key releases only its own tier.
        assert_eq!(read(None, Some("on")), Tiers { stick: true, ..Tiers::SAFE });
        // GP-DISK: `gp_nvme` counts only as a readable line that does not name Georg's Windows or CachyOS drive.
        let nvme = |v: &'static str| Tiers::read(move |k| (k == NVME_KEY).then_some(v));
        assert_eq!(nvme("81:00.0/1d97:5236"), Tiers { nvme: true, ..Tiers::SAFE });
        for odd in ["on", "", "81:00.0", "01:00.0/144d:a80d", "86:00.0/144d:a80a", "81:00.0/1d97:5236 x"] {
            assert_eq!(nvme(odd), Tiers::SAFE, "{odd:?}");
        }
        assert_eq!(Tiers::from_values(Some("on"), None, Some("81:00.0/1d97:5236")), Tiers { net: true, stick: false, nvme: true });
    }

    /// Between two tiers only the named switch differs.
    #[test]
    fn a_tier_changes_only_its_own_switch() {
        for t in Tiers::all() {
            let base = effective(&forced(t));
            let with_net = effective(&forced(Tiers { net: !t.net, ..t }));
            let diff: Vec<&str> = base.keys().chain(with_net.keys()).filter(|k| base.get(*k) != with_net.get(*k)).map(|k| k.as_str()).collect();
            let mut diff = diff;
            diff.sort_unstable();
            diff.dedup();
            assert_eq!(diff, [NET_KEY], "net tier from {t:?}: no CONFIG switch but its own line");
            assert_eq!(Tiers { net: !t.net, ..t }.stick_id_withheld(), t.stick_id_withheld());
            assert_eq!(Tiers { net: !t.net, ..t }.markers_read_only(), t.markers_read_only());

            let with_stick = effective(&forced(Tiers { stick: !t.stick, ..t }));
            let diff: Vec<&str> = base.keys().filter(|k| base.get(*k) != with_stick.get(*k)).map(|k| k.as_str()).collect();
            assert_eq!(diff, [STICK_KEY], "stick tier from {t:?}: no CONFIG switch but its own line");
            assert_eq!(base.len(), with_stick.len());
            assert_ne!(Tiers { stick: !t.stick, ..t }.stick_id_withheld(), t.stick_id_withheld());
            assert_ne!(Tiers { stick: !t.stick, ..t }.markers_read_only(), t.markers_read_only());

            // GP-DISK: a named controller releases the Mini-PC's three NVMe lines and nothing else (the two window
            // lines are taken away either way when the stick has none; `gp_nvme` itself is the stick's line).
            let with_nvme = effective(&forced(Tiers { nvme: !t.nvme, ..t }));
            let mut diff: Vec<&str> = base.keys().chain(with_nvme.keys()).filter(|k| base.get(*k) != with_nvme.get(*k)).map(|k| k.as_str()).collect();
            diff.sort_unstable();
            // and the loader's own word that it released the controller
            assert_eq!(diff, [NVME_LOADER_KEY, "nvme_write"], "nvme tier from {t:?}");
            let lines = |t: Tiers| forced(t).into_iter().map(|(k, _)| k).collect::<Vec<_>>();
            let (on, off) = if t.nvme { (lines(t), lines(Tiers { nvme: false, ..t })) } else { (lines(Tiers { nvme: true, ..t }), lines(t)) };
            let mut gone: Vec<&str> = off.iter().copied().filter(|k| !on.contains(k)).collect();
            gone.sort_unstable();
            assert_eq!(gone, [NVME_KEY, "nvme_boot", "nvme_data", "nvme_write"], "{t:?}");
            assert_eq!(on.iter().filter(|k| **k == NVME_LOADER_KEY).count(), 1);
            assert!(on.contains(&"nvme_bind"), "every other storage function stays untouched");
        }
    }

    /// Whatever the stick's file says, after the forced lines no write switch is on, the internal disks have no
    /// driver, no window and no boot partition: in every tier (WLAN and the cable are the PCI core's profile rule).
    #[test]
    fn no_write_switch_can_turn_on() {
        let hostile: &[(&str, &str)] = &[
            ("nvme_bind", "on"),
            ("nvme_write", "on"),
            ("nvme_data", "S1:GUID:2048:1000"),
            ("nvme_boot", "S1:GUID:1:2"),
            ("usb_msd_write", "on"),
            ("stick_write", "on"),
            ("profile", "normal"),
            ("screen", "2560x1440"),
        ];
        for t in Tiers::all() {
            // Every subset of the hostile lines, before the forced ones.
            for mask in 0u32..(1 << hostile.len()) {
                let mut lines: Vec<(&str, &str)> = hostile.iter().enumerate().filter(|(i, _)| mask & (1 << i) != 0).map(|(_, l)| *l).collect();
                lines.extend(forced(t));
                let m = effective(&lines);
                for k in write_switches(t) {
                    assert_eq!(m.get(k).map(|s| s.as_str()), Some("off"), "{k} in {t:?} mask {mask:#x}");
                }
                assert_eq!(m.get("nvme_bind").map(|s| s.as_str()), Some("off"));
                if t.nvme {
                    // The stick's own lines, for the named controller alone (PCI keeps every other one untouched).
                    assert_eq!(m.get("nvme_write").map(|s| s.as_str()), (mask & 2 != 0).then_some("on"));
                    assert_eq!((m.contains_key("nvme_data"), m.contains_key("nvme_boot")), (mask & 4 != 0, mask & 8 != 0));
                } else {
                    assert!(!m.contains_key("nvme_data") && !m.contains_key("nvme_boot"));
                    assert_eq!(m.get("nvme_write").map(|s| s.as_str()), Some("off"));
                }
                assert_eq!(m.get("screen").map(|s| s.as_str()), Some("firmware"));
                assert_eq!(m.get(NET_KEY).map(|s| s.as_str()), Some(if t.net { "on" } else { "off" }), "the loader's word on the cable");
            }
        }
    }

    /// A profile only turns things off (R8a/R8d): no forced line says `on` except the two tier lines, which only
    /// repeat what the stick's file released.
    #[test]
    fn forced_lines_never_say_on() {
        for t in Tiers::all() {
            for (k, v) in forced(t) {
                if k == NET_KEY {
                    assert_eq!(v == "on", t.net);
                } else if k == STICK_KEY {
                    assert_eq!(v == "on", t.stick);
                } else if k == NVME_LOADER_KEY {
                    assert_eq!(v == "on", t.nvme, "the loader's word repeats what gp_nvme released");
                } else {
                    assert_ne!(v, "on", "{k}");
                }
            }
        }
        assert_eq!(forced(Tiers::SAFE).iter().filter(|(k, _)| *k == NET_KEY).count(), 1);
        for (k, v) in kernel_fallback() {
            assert!(k != "screen" && v != "on", "{k}={v}");
        }
        assert!(kernel_fallback().contains(&("nvme_bind", "off")) && kernel_fallback().contains(&(NET_KEY, "off")));
    }

    /// The kernel's own imposition: after it the map holds every never-relaxed line, whatever it held before,
    /// and a map the current loader made is left as it is (nothing changed, nothing to warn about).
    #[test]
    fn impose_closes_what_an_older_loader_left_open() {
        let s = |x: &str| std::string::String::from(x);
        // BT-5's loader: no nvme_bind line, the stick's own window lines still there.
        let mut old: BTreeMap<_, _> = [("profile", "gaming-pc"), ("nvme_write", "off"), ("nvme_data", "S:G:1:2"), ("nvme_boot", "S:G:3:4"), ("stick_write", "off"), ("usb_msd_write", "off"), ("token", "t")]
            .into_iter()
            .map(|(k, v)| (s(k), s(v)))
            .collect();
        assert_eq!(impose(&mut old, NEVER_RELAXED), ["nvme_bind", "nvme_data", "nvme_boot"]);
        assert_eq!(old.get("nvme_bind"), Some(&s("off")));
        assert!(!old.contains_key("nvme_data") && !old.contains_key("nvme_boot"));
        assert_eq!(old.get("token"), Some(&s("t")), "other keys stay");
        assert!(impose(&mut old, NEVER_RELAXED).is_empty(), "a second time changes nothing");
        // What this wave's loader hands over (its forced lines last, the two window keys emptied): nothing to
        // report, and the emptied keys are gone from the map.
        let mut now: BTreeMap<_, _> = forced(Tiers::SAFE).into_iter().chain([("profile", "gaming-pc")]).map(|(k, v)| (s(k), s(v))).collect();
        assert_eq!(now.get("nvme_data"), Some(&s("")));
        assert!(impose(&mut now, NEVER_RELAXED).is_empty(), "the current loader leaves the kernel nothing to add");
        assert!(!now.contains_key("nvme_data") && !now.contains_key("nvme_boot"));
        // Everything hostile.
        let mut bad: BTreeMap<_, _> = NEVER_RELAXED.iter().map(|(k, _)| (s(k), s("on"))).collect();
        assert_eq!(impose(&mut bad, NEVER_RELAXED).len(), NEVER_RELAXED.len());
        for (k, v) in NEVER_RELAXED {
            assert_eq!(bad.get(*k).map(|x| x.as_str()), (!v.is_empty()).then_some(*v), "{k}");
        }
        // The fallback for a loader that names no profile at all.
        let mut none = BTreeMap::new();
        impose(&mut none, &kernel_fallback());
        assert_eq!(none.get(NET_KEY), Some(&s("off")));
        assert_eq!(none.get("gp_stick"), Some(&s("off")));
        assert!(is_gaming_pc(Some("gaming-pc")) && is_gaming_pc(Some(" Gaming-PC ")) && !is_gaming_pc(Some("normal")) && !is_gaming_pc(None));
    }

    /// GP-DISK: the kernel releases the named controller on the loader's word only. Georg's stick today carries the
    /// bitsboot of generation 107, which knows no `gp_nvme` and hands the stick's line through (install_kernel never
    /// renews bitsboot); its forced lines still take the NVMe write and both windows away.
    #[test]
    fn an_older_loader_releases_no_controller() {
        let s = |x: &str| std::string::String::from(x);
        let stick = [("gp_net", "on"), ("gp_stick", "on"), (NVME_KEY, "81:00.0/1d97:5236"), ("nvme_write", "on"), ("nvme_boot", "S:G:3:4")];
        // The kernel's map keeps an emptied key with an empty value until `impose` takes it away.
        let map = |lines: &[(&str, &str)]| lines.iter().map(|(k, v)| (s(k), s(v))).collect::<BTreeMap<_, _>>();
        let old_forced = [("gp_net", "on"), ("gp_stick", "on")].into_iter().chain(NEVER_RELAXED.iter().copied()).chain([SCREEN_FORCED]);
        let mut old = map(&stick.iter().copied().chain(old_forced).collect::<Vec<_>>());
        let t = Tiers::from_loader(|k| old.get(k).map(|v| v.as_str()));
        assert_eq!(t, Tiers { net: true, stick: true, nvme: false }, "the stick's spelling alone releases nothing");
        assert_eq!(impose(&mut old, &never_relaxed(t)), [NVME_KEY], "taken away, and said (profile.guard)");
        assert!(!old.contains_key(NVME_KEY) && !old.contains_key("nvme_boot"));
        assert_eq!(old.get("nvme_write"), Some(&s("off")));
        // This wave's loader: its word stands behind the stick's line, the kernel has nothing to change.
        let t = Tiers::read(|k| stick.iter().rev().find(|(x, _)| *x == k).map(|(_, v)| *v));
        let mut now = map(&stick.iter().copied().chain(forced(t)).collect::<Vec<_>>());
        let t = Tiers::from_loader(|k| now.get(k).map(|v| v.as_str()));
        assert_eq!(t, Tiers { net: true, stick: true, nvme: true });
        assert!(impose(&mut now, &never_relaxed(t)).is_empty());
        assert_eq!((now.get(NVME_KEY), now.get("nvme_write")), (Some(&s("81:00.0/1d97:5236")), Some(&s("on"))));
        assert!(nvme_released(Some("gaming-pc"), Some("81:00.0/1d97:5236"), Some("on")));
        assert!(!nvme_released(Some("gaming-pc"), Some("81:00.0/1d97:5236"), None), "an older loader's record");
        assert!(!nvme_released(Some("normal"), Some("81:00.0/1d97:5236"), Some("on")) && !nvme_released(None, Some("81:00.0/1d97:5236"), Some("on")));
        // The loader's word alone (no readable gp_nvme) names nothing either.
        let lone = map(&[(NVME_LOADER_KEY, "on"), (NVME_KEY, "")]);
        assert!(!Tiers::from_loader(|k| lone.get(k).map(|v| v.as_str())).nvme);
    }

    /// GP-DISK: outside the profile the kernel takes `gp_nvme` away; a record without the line (the Mini-PC's) stays
    /// exactly as it was.
    #[test]
    fn gp_nvme_counts_only_under_the_profile() {
        let s = |x: &str| std::string::String::from(x);
        let mut other: BTreeMap<_, _> = [("profile", "normal"), ("nvme_bind", "off"), (NVME_KEY, "81:00.0/1d97:5236"), (NVME_LOADER_KEY, "on"), ("nvme_write", "on")]
            .into_iter()
            .map(|(k, v)| (s(k), s(v)))
            .collect();
        assert_eq!(impose(&mut other, OUTSIDE_PROFILE), [NVME_KEY, NVME_LOADER_KEY]);
        assert!(!other.contains_key(NVME_KEY) && !other.contains_key(NVME_LOADER_KEY));
        assert_eq!(other.get("nvme_write"), Some(&s("on")), "nothing else changes");
        let mini: BTreeMap<_, _> = [("nvme_write", "on"), ("nvme_data", "S:G:1:2"), ("nvme_boot", "S:G:3:4"), ("token", "t")].into_iter().map(|(k, v)| (s(k), s(v))).collect();
        let mut after = mini.clone();
        assert!(impose(&mut after, OUTSIDE_PROFILE).is_empty());
        assert_eq!(after, mini);
    }

    #[test]
    fn names_and_report_lines() {
        assert_eq!(Tiers::all().map(Tiers::name), ["none", "net", "stick", "net,stick", "nvme", "net,nvme", "stick,nvme", "net,stick,nvme"]);
        let safe = report_lines(Tiers::SAFE, "dmi", false);
        assert_eq!(safe.len(), 5);
        assert!(safe[0].contains("Mainboard erkannt") && safe[1].contains("aus") && safe[2].contains("nur gelesen"));
        let all = report_lines(Tiers { net: true, stick: true, nvme: false }, "dmi", true);
        assert_eq!(all.len(), 6);
        assert!(all[1].contains("profile=normal") && all[2].contains("Kabel: an") && all[2].contains("WLAN bleibt aus") && all[3].contains("beschrieben"));
        for t in Tiers::all() {
            for reason in ["dmi", "cpuid", "config", "kernel", "?"] {
                let l = report_lines(t, reason, false);
                assert!(l[0].starts_with("Sicherer Modus"));
                assert_eq!(l.last().unwrap().contains("kein Treiber, nicht gelesen"), !t.nvme);
                assert_eq!(l.last().unwrap().contains("gp_nvme"), t.nvme);
                assert!(l[l.len() - 2].contains("Andere USB-Datenträger"));
            }
        }
    }
}
