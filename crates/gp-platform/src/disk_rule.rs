//! The Gaming-PC's disk rule (plan/spec/gaming-pc.md §2, Georg 5.11.: "the internal SSDs are not even read"): on this
//! machine no internal disk gets a driver, a queue or a bus master, whatever the stick's CONFIG.TXT says and
//! whatever was ticked at the keyboard. The wiring reads one CONFIG key, `nvme_bind=off`, which the loader's
//! profile forces after the stick's own lines (`tiers::NEVER_RELAXED`) and the kernel imposes again:
//! - PCI: a mass-storage function ([`kept_unbound`]) stays untouched (no BAR sizing, no command write, no driver);
//! - block layer: the full-access NVMe tick is refused ([`tick_refused`]) and `nvme_write` never answers on.
//!
//! [`answer`] is the whole rule as a truth table over machine, disk, the stick's switches and the tick; the
//! tests walk every combination. USB disks keep the rules they have on every machine.
//!
//! GP-DISK: one NVMe controller Georg names with `gp_nvme=` (`named`) is the exception; on it the Mini-PC's rules
//! apply (`nvme_write=on`, the `nvme_data` window, the `nvme_boot` partition), the full-access tick still opens nothing.

use crate::tiers::{self, Tiers};
use alloc::format;
use alloc::string::String;

pub mod marker;
pub mod named;

pub const BIND_KEY: &str = "nvme_bind";

/// PCI base class 01, every mass-storage controller: NVMe (0108), AHCI (0106), RAID and Intel VMD (0104), IDE,
/// SCSI, SAS (linux-7.3.1/include/linux/pci_ids.h:18-28, `PCI_BASE_CLASS_STORAGE` 0x01).
pub fn storage_class(class: u32) -> bool {
    class >> 16 == 0x01
}

/// CONFIG `nvme_bind=off` (spaces around ignored). Any other value, or no line, binds as before.
pub fn bind_off(nvme_bind: Option<&str>) -> bool {
    nvme_bind.is_some_and(|v| v.trim() == "off")
}

/// A mass-storage function the PCI core leaves untouched.
pub fn kept_unbound(class: u32, nvme_bind: Option<&str>) -> bool {
    storage_class(class) && bind_off(nvme_bind)
}

/// The full-access grant's NVMe tick (MC-10) opens nothing on a machine whose internal disks have no driver.
pub fn tick_refused(nvme_bind: Option<&str>) -> bool {
    bind_off(nvme_bind)
}

/// `nvme_write` as the block layer may answer it: CONFIG's line or the tick, and never under `nvme_bind=off`.
pub fn nvme_write_now(nvme_bind: Option<&str>, config_on: bool, tick_open: bool) -> bool {
    !bind_off(nvme_bind) && (config_on || tick_open)
}

/// GP-DISK: the same with `gp_nvme=` naming a controller (`named`): under `nvme_bind=off` only CONFIG's own
/// `nvme_write=on` counts then (the only NVMe disks registered sit on the named controller), never the tick.
/// Without `nvme_bind=off` this is [`nvme_write_now`].
pub fn nvme_write_named(nvme_bind: Option<&str>, named: bool, config_on: bool, tick_open: bool) -> bool {
    if bind_off(nvme_bind) { named && config_on } else { nvme_write_now(nvme_bind, config_on, tick_open) }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Machine {
    /// The loader chose the profile `gaming-pc` (DMI names of the board, or an Intel CPU on bare metal).
    GamingPc,
    /// Any other machine (the Mini-PC, QEMU): the stick's lines stand as they are.
    Other,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Disk {
    /// An NVMe controller `gp_nvme=` does not name (on the Gaming-PC: the two Samsungs, always).
    Nvme,
    /// The NVMe controller `gp_nvme=` names, at its place with its ids (on another machine: any NVMe).
    NvmeNamed,
    Ahci,
    /// RAID, VMD, IDE, SCSI, SAS: class 01 without a BitsOS driver.
    OtherStorage,
    /// The USB stick the system started from, identified by its STICK.ID.
    UsbBootStick,
    UsbOther,
}

impl Disk {
    pub const ALL: [Disk; 6] = [Disk::Nvme, Disk::NvmeNamed, Disk::Ahci, Disk::OtherStorage, Disk::UsbBootStick, Disk::UsbOther];
    pub fn internal(self) -> bool {
        matches!(self, Disk::Nvme | Disk::NvmeNamed | Disk::Ahci | Disk::OtherStorage)
    }
}

/// What the stick's CONFIG.TXT says (each line present and `on`, or for the two NVMe keys: present and valid).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stick {
    pub nvme_write: bool,
    pub nvme_data: bool,
    pub nvme_boot: bool,
    pub stick_write: bool,
    pub usb_msd_write: bool,
    pub profile_normal: bool,
    pub gp_net: bool,
    pub gp_stick: bool,
    /// `gp_nvme=` names a controller (readable, not one of [`named::NEVER_NAMED`]).
    pub gp_nvme: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Answer {
    /// A driver binds the controller (queues, bus master).
    pub driver: bool,
    pub read: bool,
    pub write: bool,
}

const NEVER: Answer = Answer { driver: false, read: false, write: false };

/// The switches the kernel sees: the stick's own on any other machine; on the Gaming-PC the profile's forced
/// lines come last and win. `profile=normal` changes nothing there (the DMI match beats it in the loader).
fn kernel_view(m: Machine, s: Stick) -> (Stick, bool, bool) {
    match m {
        Machine::Other => (s, false, true),
        Machine::GamingPc => {
            let t = Tiers { net: s.gp_net, stick: s.gp_stick, nvme: s.gp_nvme };
            let forced = tiers::forced(t);
            let off = |key: &str, own: bool| own && !forced.iter().any(|(k, v)| *k == key && (*v == "off" || v.is_empty()));
            let seen = Stick {
                nvme_write: off("nvme_write", s.nvme_write),
                nvme_data: off("nvme_data", s.nvme_data),
                nvme_boot: off("nvme_boot", s.nvme_boot),
                stick_write: off("stick_write", s.stick_write),
                usb_msd_write: off("usb_msd_write", s.usb_msd_write),
                ..s
            };
            let bind_forced_off = forced.iter().any(|(k, v)| *k == BIND_KEY && *v == "off");
            (seen, bind_forced_off, !t.stick_id_withheld())
        }
    }
}

/// The rule: may `d` get a driver, be read, be written, on `m` with the stick's lines `s` and the full-access
/// NVMe tick `tick`? The generic part repeats the kernel's own rules (PCI's storage rule: only the NVMe driver
/// binds class 01; `block::policy::decide` step 3; NV-2p: an NVMe write needs `nvme_data`'s window).
pub fn answer(m: Machine, d: Disk, s: Stick, tick: bool) -> Answer {
    let (seen, bind_forced_off, stick_id_handed) = kernel_view(m, s);
    let nvme_bind = bind_forced_off.then_some("off");
    match d {
        Disk::Nvme | Disk::NvmeNamed => {
            // GP-DISK: the place and ids are compared at the scan (`named::verdict`); here the disk is named or not.
            let released = d == Disk::NvmeNamed && seen.gp_nvme;
            if kept_unbound(named::CLASS_NVME, nvme_bind) && !released {
                return NEVER;
            }
            let write = nvme_write_named(nvme_bind, released, seen.nvme_write, tick) && seen.nvme_data;
            Answer { driver: true, read: true, write }
        }
        // No driver for these exists on any machine; on the Gaming-PC they are not even sized.
        Disk::Ahci | Disk::OtherStorage => NEVER,
        Disk::UsbBootStick => Answer { driver: true, read: true, write: seen.stick_write || stick_id_handed },
        Disk::UsbOther => Answer { driver: true, read: true, write: seen.usb_msd_write },
    }
}

/// The internal disks of Georg's Gaming-PC by PCI id, with the name the report shows. The disks are never read,
/// so the name cannot come from the drive itself (`lspci -nn` on the machine, 14.11.2026; no serial numbers).
pub const KNOWN: &[(u16, u16, &str)] = &[
    (0x144D, 0xA80D, "Samsung 990 EVO Plus 2TB"),
    (0x144D, 0xA80A, "Samsung 980 PRO 1TB"),
    (0x1D97, 0x5236, "Lexar SSD NQ790 2TB"),
    (0x1B21, 0x0612, "SATA-Anschlüsse (ASMedia, ohne Platte)"),
    (0x8086, 0xAD0B, "Intel VMD (leer)"),
];

/// The report's name for a storage function: the known model, else its ids.
pub fn model(vendor: u16, device: u16) -> String {
    match KNOWN.iter().find(|(v, d, _)| (*v, *d) == (vendor, device)) {
        Some((_, _, name)) => String::from(*name),
        None => format!("Datenträger-Anschluss {vendor:04x}:{device:04x}"),
    }
}

/// The screen report's line for one internal disk left untouched (plain German; wave B10 prints it).
pub fn report_line(vendor: u16, device: u16) -> String {
    format!("{}: kein Treiber, nicht gelesen", model(vendor, device))
}

/// GP-DISK: the report page's row for one storage function, with why it has a driver or not. `place` as
/// `bb:dd.f`, `class` the PCI class, `verdict` the scan's (`named::verdict`), `line` CONFIG's `gp_nvme` value.
pub fn report_row(place: (u8, u8, u8), vendor: u16, device: u16, class: u32, verdict: named::Verdict, line: Option<&str>) -> String {
    use named::Verdict;
    let at = format!("{:02x}:{:02x}.{}", place.0, place.1, place.2);
    let name = model(vendor, device);
    let here = named::Named { bus: place.0, dev: place.1, func: place.2, vendor, device };
    match verdict {
        Verdict::Released => format!("{at} {name}: Treiber an (in CONFIG.TXT mit gp_nvme freigegeben)"),
        Verdict::Mismatch => {
            let said = named::named(line).map(|n| n.render()).unwrap_or_default();
            format!("{at} {name}: kein Treiber, nicht gelesen (gp_nvme={said} passt nicht: hier steckt {vendor:04x}:{device:04x})")
        }
        Verdict::Kept | Verdict::Free if named::NEVER_NAMED.contains(&(vendor, device)) => {
            format!("{at} {name}: kein Treiber, nicht gelesen (Georgs Windows- oder CachyOS-Platte, nie freigebbar)")
        }
        Verdict::Kept | Verdict::Free if class == named::CLASS_NVME && named::named(line).is_none() => {
            format!("{at} {name}: kein Treiber, nicht gelesen (freigeben mit der Zeile gp_nvme={})", here.render())
        }
        Verdict::Kept | Verdict::Free if class == named::CLASS_NVME => format!("{at} {name}: kein Treiber, nicht gelesen (nicht in gp_nvme genannt)"),
        Verdict::Kept | Verdict::Free => format!("{at} {name}: kein Treiber, nicht gelesen (BitsOS hat dafür keinen Treiber)"),
    }
}

/// GP-DISK: the report page's line when /boot is the `nvme_boot=` partition of the named SSD.
pub const BOOT_FROM_SSD: &str = "Gestartet von der SSD (Partition BITSOS): „Start-Stick“ heißt hier diese Partition, beschrieben wird nur \\BITSOS2\\ darauf. Der Stick ist nur zur Rettung da und wird nur gelesen.";

/// GP-DISK: the report row for a `gp_nvme=` line that names no storage function the scan found, or cannot be read.
/// `class_at_place`: the class code of the function at the named place, if the scan found one there (after a bus
/// renumbering that may be a card or a bridge, which has no "Platten" row of its own).
pub fn report_unmatched(line: &str, class_at_place: Option<u32>) -> Option<String> {
    match named::read(Some(line))? {
        Err(named::Refusal::NeverNamed) => Some(String::from("gp_nvme nennt Georgs Windows- oder CachyOS-Platte: abgelehnt, keine Platte bekommt einen Treiber")),
        Err(named::Refusal::Unreadable(_)) => Some(format!("gp_nvme={} ist nicht lesbar (Form: 81:00.0/1d97:5236): keine Platte bekommt einen Treiber", line.trim())),
        Ok(n) => match class_at_place {
            None => Some(format!("gp_nvme={}: an dieser Stelle steckt nichts, keine Platte bekommt einen Treiber", n.render())),
            Some(c) if c >> 16 != 0x01 => Some(format!("gp_nvme={}: an dieser Stelle steckt keine Platte, keine Platte bekommt einen Treiber", n.render())),
            // A storage function: its own row says Released or why not.
            Some(_) => None,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    extern crate std;
    use std::vec::Vec;

    fn sticks() -> Vec<Stick> {
        (0u32..512)
            .map(|b| Stick {
                nvme_write: b & 1 != 0,
                nvme_data: b & 2 != 0,
                nvme_boot: b & 4 != 0,
                stick_write: b & 8 != 0,
                usb_msd_write: b & 16 != 0,
                profile_normal: b & 32 != 0,
                gp_net: b & 64 != 0,
                gp_stick: b & 128 != 0,
                gp_nvme: b & 256 != 0,
            })
            .collect()
    }

    /// Georg's rule: on this machine every internal disk refuses in every combination, but the one controller
    /// `gp_nvme=` names (GP-DISK).
    #[test]
    fn the_gaming_pc_never_binds_reads_or_writes_an_internal_disk() {
        let mut seen = 0;
        for s in sticks() {
            for tick in [false, true] {
                for d in Disk::ALL.into_iter().filter(|d| d.internal() && !(*d == Disk::NvmeNamed && s.gp_nvme)) {
                    assert_eq!(answer(Machine::GamingPc, d, s, tick), NEVER, "{d:?} {s:?} tick={tick}");
                    seen += 1;
                }
            }
        }
        assert_eq!(seen, 256 * 2 * 4 + 256 * 2 * 3);
    }

    /// GP-DISK: the named controller gets the driver and the Mini-PC's write rule (CONFIG's `nvme_write=on` and the
    /// `nvme_data` window), never the full-access tick; the other NVMe drives stay untouched beside it.
    #[test]
    fn the_named_controller_follows_the_mini_pcs_rules() {
        for s in sticks().into_iter().filter(|s| s.gp_nvme) {
            for tick in [false, true] {
                let a = answer(Machine::GamingPc, Disk::NvmeNamed, s, tick);
                assert_eq!(a, Answer { driver: true, read: true, write: s.nvme_write && s.nvme_data }, "{s:?} tick={tick}");
                assert_eq!(answer(Machine::GamingPc, Disk::Nvme, s, tick), NEVER, "the Samsungs beside it");
                assert_eq!(answer(Machine::GamingPc, Disk::Ahci, s, tick), NEVER);
                assert_eq!(answer(Machine::GamingPc, Disk::OtherStorage, s, tick), NEVER);
                // The Mini-PC with the same lines: the same answer for that disk but the tick, which counts there.
                let mini = answer(Machine::Other, Disk::Nvme, Stick { gp_nvme: false, ..s }, false);
                assert_eq!(a, mini, "{s:?}");
            }
        }
        for (bind, named, config, tick) in (0..16).map(|b| (b & 1 != 0, b & 2 != 0, b & 4 != 0, b & 8 != 0)) {
            let v = bind.then_some("off");
            let want = if bind { named && config } else { config || tick };
            assert_eq!(nvme_write_named(v, named, config, tick), want);
            if !bind {
                assert_eq!(nvme_write_named(v, named, config, tick), nvme_write_now(v, config, tick), "as before without the guard");
            }
        }
    }

    /// Other USB disks are never written on the Gaming-PC; the boot stick only under `gp_stick=on`, and reading
    /// both stays as everywhere.
    #[test]
    fn usb_on_the_gaming_pc() {
        for s in sticks() {
            for tick in [false, true] {
                let other = answer(Machine::GamingPc, Disk::UsbOther, s, tick);
                assert_eq!(other, Answer { driver: true, read: true, write: false }, "{s:?}");
                let stick = answer(Machine::GamingPc, Disk::UsbBootStick, s, tick);
                assert_eq!(stick, Answer { driver: true, read: true, write: s.gp_stick }, "{s:?}");
            }
        }
    }

    /// The Mini-PC's truth table is what it was: the tier lines, `profile=normal` and this module change nothing.
    #[test]
    fn other_machines_are_unchanged() {
        for s in sticks() {
            for tick in [false, true] {
                let plain = Stick { profile_normal: false, gp_net: false, gp_stick: false, gp_nvme: false, ..s };
                for d in Disk::ALL {
                    assert_eq!(answer(Machine::Other, d, s, tick), answer(Machine::Other, d, plain, tick), "{d:?} {s:?}");
                }
                let nvme = answer(Machine::Other, Disk::Nvme, s, tick);
                assert_eq!(nvme, Answer { driver: true, read: true, write: (s.nvme_write || tick) && s.nvme_data });
                assert_eq!(answer(Machine::Other, Disk::NvmeNamed, s, tick), nvme, "gp_nvme means nothing there");
                assert_eq!(answer(Machine::Other, Disk::UsbOther, s, tick).write, s.usb_msd_write);
                assert!(answer(Machine::Other, Disk::UsbBootStick, s, tick).write, "the identified stick takes FAT writes");
                assert_eq!(answer(Machine::Other, Disk::Ahci, s, tick), NEVER, "no AHCI driver exists");
            }
        }
    }

    /// USB unchanged: with the same switches reaching the kernel, a USB disk gets the same answer on both
    /// machines; only what reaches the kernel differs (forced lines, the withheld STICK.ID).
    #[test]
    fn usb_rules_are_the_same_function_on_both_machines() {
        for s in sticks() {
            let (seen, _, handed) = kernel_view(Machine::GamingPc, s);
            assert!(!seen.usb_msd_write && !seen.stick_write);
            // GP-DISK: the three NVMe lines stand only with a named controller (they then act on it alone).
            assert_eq!((seen.nvme_write, seen.nvme_data, seen.nvme_boot), if s.gp_nvme { (s.nvme_write, s.nvme_data, s.nvme_boot) } else { (false, false, false) });
            assert_eq!(handed, s.gp_stick);
            let there = answer(Machine::GamingPc, Disk::UsbOther, s, false);
            let here = answer(Machine::Other, Disk::UsbOther, seen, false);
            assert_eq!(there, here);
        }
    }

    #[test]
    fn the_wiring_predicates() {
        // NVMe, AHCI, VMD (RAID), IDE, SCSI, SAS: all class 01.
        for class in [0x01_08_02, 0x01_06_01, 0x01_04_00, 0x01_01_8A, 0x01_00_00, 0x01_07_00, 0x01_80_00] {
            assert!(storage_class(class) && kept_unbound(class, Some("off")) && kept_unbound(class, Some(" off\r")), "{class:#x}");
            assert!(!kept_unbound(class, None) && !kept_unbound(class, Some("on")) && !kept_unbound(class, Some("")), "{class:#x}");
        }
        // The stick's own controller (xHCI), network, display, bridges: never by this key.
        for class in [0x0C_03_30, 0x02_00_00, 0x02_80_00, 0x03_00_00, 0x06_04_00, 0x00_01_08, 0x10_80_00] {
            assert!(!kept_unbound(class, Some("off")), "{class:#x}");
        }
        for odd in ["OFF", "Off", "0", "no", "false", "of f"] {
            assert!(!bind_off(Some(odd)), "{odd:?}: only the exact word");
        }
        assert!(tick_refused(Some("off")) && !tick_refused(None) && !tick_refused(Some("on")));
        for (config, tick) in [(false, false), (true, false), (false, true), (true, true)] {
            assert!(!nvme_write_now(Some("off"), config, tick), "never under nvme_bind=off");
            assert_eq!(nvme_write_now(None, config, tick), config || tick, "as before without the line");
            assert_eq!(nvme_write_now(Some("on"), config, tick), config || tick);
        }
    }

    #[test]
    fn report_names() {
        assert_eq!(report_line(0x144D, 0xA80D), "Samsung 990 EVO Plus 2TB: kein Treiber, nicht gelesen");
        assert_eq!(report_line(0x1D97, 0x5236), "Lexar SSD NQ790 2TB: kein Treiber, nicht gelesen");
        assert_eq!(report_line(0x1234, 0x0001), "Datenträger-Anschluss 1234:0001: kein Treiber, nicht gelesen");
        for (_, _, name) in KNOWN {
            // Model names only: nothing that looks like a serial number.
            assert!(!name.chars().any(|c| c == '_') && name.len() < 48);
        }
    }

    /// GP-DISK: the "Platten" rows, per drive which one got a driver and why the others did not.
    #[test]
    fn report_rows_say_why() {
        use named::Verdict::*;
        let line = Some("81:00.0/1d97:5236");
        let nv = named::CLASS_NVME;
        assert_eq!(report_row((0x81, 0, 0), 0x1D97, 0x5236, nv, Released, line), "81:00.0 Lexar SSD NQ790 2TB: Treiber an (in CONFIG.TXT mit gp_nvme freigegeben)");
        assert_eq!(report_row((0x81, 0, 0), 0x1D97, 0x5236, nv, Kept, None), "81:00.0 Lexar SSD NQ790 2TB: kein Treiber, nicht gelesen (freigeben mit der Zeile gp_nvme=81:00.0/1d97:5236)");
        for (place, d) in [((0x01, 0, 0), 0xA80D), ((0x86, 0, 0), 0xA80A)] {
            for l in [None, line] {
                let r = report_row(place, 0x144D, d, nv, Kept, l);
                assert!(r.ends_with("(Georgs Windows- oder CachyOS-Platte, nie freigebbar)") && !r.contains("gp_nvme="), "{r}");
            }
        }
        assert!(report_row((0x81, 0, 0), 0x1D97, 0x5237, nv, Mismatch, line).ends_with("(gp_nvme=81:00.0/1d97:5236 passt nicht: hier steckt 1d97:5237)"));
        assert!(report_row((0x82, 0, 0), 0x1B21, 0x0612, 0x01_06_01, Kept, line).ends_with("(BitsOS hat dafür keinen Treiber)"));
        assert!(report_row((0x00, 0x0e, 0), 0x8086, 0xAD0B, 0x01_04_00, Kept, None).starts_with("00:0e.0 Intel VMD (leer): kein Treiber"));
        assert!(report_row((0x05, 0, 0), 0x1234, 0x0001, nv, Kept, line).ends_with("(nicht in gp_nvme genannt)"));
        assert_eq!(report_unmatched("81:00.0/1d97:5236", Some(nv)), None);
        assert_eq!(report_unmatched("81:00.0/1d97:5236", Some(0x01_06_01)), None, "a SATA controller there has its own row (passt nicht)");
        assert!(report_unmatched("81:00.0/1d97:5236", None).unwrap().contains("steckt nichts"));
        // Buses renumbered: a graphics card or a bridge at the named place has no Platten row, so this one says it.
        for other in [0x03_00_00, 0x06_04_00] {
            assert_eq!(report_unmatched("81:00.0/1d97:5236", Some(other)).unwrap(), "gp_nvme=81:00.0/1d97:5236: an dieser Stelle steckt keine Platte, keine Platte bekommt einen Treiber");
        }
        assert!(report_unmatched("01:00.0/144d:a80d", Some(nv)).unwrap().contains("abgelehnt"));
        assert!(report_unmatched("81:00.0", Some(nv)).unwrap().contains("nicht lesbar"));
        assert_eq!(report_unmatched("", Some(nv)), None);
    }
}
