//! The USB part of the Gaming-PC's report page, in plain German (plan/spec/gaming-pc.md §8). One line per
//! controller with its place and whether it runs, one line per device with what serves it. A controller that did
//! not come up says so with its reason, so a photo of the page tells where the bring-up stopped. That holds for
//! every xHCI function PCI found, also one whose probe ended before the driver listed it (`merge`): PCI keeps the
//! reason the probe returned (`pci::failed`).

use crate::driver::controllers;
use crate::hc::State;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

/// One USB device as the page shows it.
#[derive(Clone, Debug)]
pub struct Row {
    /// Root port, then the ports of the hubs on the way ("12.1").
    pub port: String,
    pub vendor: u16,
    pub product: u16,
    /// The class drivers bound to its interfaces.
    pub drivers: Vec<&'static str>,
}

/// One controller as the page shows it.
#[derive(Clone, Debug)]
pub struct Ctrl {
    /// PCI place ("80:14.0").
    pub place: String,
    pub running: bool,
    /// Why it does not run (empty when it does).
    pub reason: String,
    pub ports: u8,
    pub rows: Vec<Row>,
}

/// What a device is, from the drivers bound to it.
fn kind(drivers: &[&'static str]) -> &'static str {
    let has = |d: &str| drivers.contains(&d);
    if has("hub") {
        "Verteiler"
    } else if has("usb-msd") {
        "Speicher (Stick oder Laufwerk)"
    } else if has("usb-hid") && has("usb-audio") {
        "Tongerät mit Tasten"
    } else if has("usb-hid") {
        "Eingabegerät (Tastatur, Maus oder Ähnliches)"
    } else if has("usb-audio") {
        "Tongerät"
    } else {
        "erkannt, wird nicht benutzt"
    }
}

pub fn lines(ctrls: &[Ctrl]) -> Vec<String> {
    let mut out = Vec::new();
    if ctrls.is_empty() {
        out.push(String::from("USB: kein USB-Baustein gefunden. Tastatur, Maus und Stick können so nicht gehen."));
        return out;
    }
    for c in ctrls {
        if c.running {
            out.push(format!("USB-Baustein {}: läuft, {} Anschlüsse, {} Geräte", c.place, c.ports, c.rows.len()));
        } else {
            out.push(format!("USB-Baustein {}: läuft NICHT ({})", c.place, c.reason));
        }
        for r in &c.rows {
            out.push(format!("  Anschluss {}: {} [{:04x}:{:04x}]", r.port, kind(&r.drivers), r.vendor, r.product));
        }
    }
    let count = |d: &str| ctrls.iter().flat_map(|c| &c.rows).filter(|r| r.drivers.contains(&d)).count();
    out.push(format!("USB zusammen: {} Eingabegeräte, {} Speicher, {} Tongeräte", count("usb-hid"), count("usb-msd"), count("usb-audio")));
    out
}

/// One xHCI function as the PCI list has it. The driver's own list starts only once a controller's registers
/// are mapped and read; a probe that ends before that (switching the function on, BAR0 missing or of a wrong size,
/// the mapping, the capability registers) leaves no controller, and the page would have no line for it.
#[derive(Clone, Debug)]
pub struct Found {
    /// PCI place ("80:14.0").
    pub place: String,
    /// Held back by a switch: no driver is offered it.
    pub held: bool,
    /// The reason of the last failed probe, when one was tried and nothing is bound since.
    pub gave_up: Option<String>,
}

/// Every xHCI function PCI found gets a line, in PCI order: the driver's controller when there is one, else why
/// there is none. A controller PCI no longer lists keeps its line at the end.
pub fn merge(found: &[Found], mut live: Vec<Ctrl>) -> Vec<Ctrl> {
    let mut out = Vec::new();
    for f in found {
        let ctrl = match live.iter().position(|c| c.place == f.place) {
            Some(i) => live.remove(i),
            None => {
                let reason = match (&f.gave_up, f.held) {
                    (Some(e), _) => e.clone(),
                    (None, true) => String::from("abgeschaltet"),
                    (None, false) => String::from("kein Treiber wurde gestartet"),
                };
                Ctrl { place: f.place.clone(), running: false, reason, ports: 0, rows: Vec::new() }
            }
        };
        out.push(ctrl);
    }
    out.append(&mut live);
    out
}

/// The controllers the driver took over, with their devices, as they are now.
fn live() -> Vec<Ctrl> {
    let devices = usb_core::registry::devices();
    controllers()
        .iter()
        .map(|c| {
            let mut rows: Vec<(u8, u32, Row)> = devices
                .iter()
                .filter(|d| d.host_name() == c.name() && !d.is_gone())
                .map(|d| {
                    let mut drivers: Vec<&'static str> = usb_core::registry::bindings_of(d.id).into_iter().map(|(_, n)| n).collect();
                    drivers.dedup();
                    (d.place.root_port, d.place.route, Row { port: d.place.path(), vendor: d.vendor(), product: d.product(), drivers })
                })
                .collect();
            rows.sort_by_key(|r| (r.0, r.1));
            Ctrl {
                place: format!("{}", c.env().pci().loc()),
                running: c.state() == State::Running,
                reason: c.reason(),
                ports: c.caps().max_ports,
                rows: rows.into_iter().map(|r| r.2).collect(),
            }
        })
        .collect()
}

/// The xHCI functions of the PCI list, with what PCI knows about a probe that failed.
fn found() -> Vec<Found> {
    pci::nodes()
        .iter()
        .filter(|n| n.class == crate::driver::CLASS_XHCI)
        .map(|n| Found { place: format!("{}", n.loc), held: n.untouched.is_some(), gave_up: pci::failed::last(n.loc).map(|f| f.error) })
        .collect()
}

/// The controllers and devices as they are now.
pub fn gather() -> Vec<Ctrl> {
    merge(&found(), live())
}

/// The lines for the page, from the live state.
pub fn report_lines() -> Vec<String> {
    lines(&gather())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::vec;

    fn row(port: &str, vendor: u16, product: u16, drivers: &[&'static str]) -> Row {
        Row { port: String::from(port), vendor, product, drivers: drivers.to_vec() }
    }

    /// The Gaming-PC as Linux showed it on 14.11.2026: nothing on the CPU's controller; keyboard, printer, mouse,
    /// two board devices, two hubs, the board's sound, Bluetooth and the stick behind a hub on the chipset's.
    fn gaming_pc() -> Vec<Ctrl> {
        vec![
            Ctrl { place: String::from("00:0d.0"), running: true, reason: String::new(), ports: 3, rows: vec![] },
            Ctrl {
                place: String::from("80:14.0"),
                running: true,
                reason: String::new(),
                ports: 22,
                rows: vec![
                    row("3", 0x10f5, 0x501b, &["usb-hid"]),
                    row("5", 0x04f9, 0x0228, &["usb-msd"]),
                    row("7", 0x046d, 0xc08b, &["usb-hid"]),
                    row("9", 0x26ce, 0x01b2, &["usb-hid"]),
                    row("10", 0x05e3, 0x0610, &["hub"]),
                    row("12", 0x05e3, 0x0610, &["hub"]),
                    row("12.1", 0x26ce, 0x0a09, &["usb-hid"]),
                    row("13", 0x26ce, 0x01a2, &["usb-hid"]),
                    row("14", 0x8087, 0x0036, &[]),
                    row("22", 0x05e3, 0x0620, &["hub"]),
                    row("22.3", 0x090c, 0x1000, &["usb-msd"]),
                ],
            },
        ]
    }

    #[test]
    fn the_gaming_pcs_page() {
        let l = lines(&gaming_pc());
        assert_eq!(l[0], "USB-Baustein 00:0d.0: läuft, 3 Anschlüsse, 0 Geräte");
        assert_eq!(l[1], "USB-Baustein 80:14.0: läuft, 22 Anschlüsse, 11 Geräte");
        assert_eq!(l[2], "  Anschluss 3: Eingabegerät (Tastatur, Maus oder Ähnliches) [10f5:501b]");
        assert!(l.contains(&String::from("  Anschluss 10: Verteiler [05e3:0610]")));
        assert!(l.contains(&String::from("  Anschluss 14: erkannt, wird nicht benutzt [8087:0036]")));
        assert!(l.contains(&String::from("  Anschluss 22.3: Speicher (Stick oder Laufwerk) [090c:1000]")));
        assert_eq!(l.last().unwrap(), "USB zusammen: 5 Eingabegeräte, 2 Speicher, 0 Tongeräte");
        assert_eq!(l.len(), 2 + 11 + 1);
    }

    #[test]
    fn a_controller_that_did_not_come_up_says_why() {
        let mut c = gaming_pc();
        c[1].running = false;
        c[1].reason = String::from("halt (HCH): timeout after 32 ms");
        c[1].rows.clear();
        let l = lines(&c);
        assert_eq!(l[1], "USB-Baustein 80:14.0: läuft NICHT (halt (HCH): timeout after 32 ms)");
        assert_eq!(l.last().unwrap(), "USB zusammen: 0 Eingabegeräte, 0 Speicher, 0 Tongeräte");
    }

    fn found(place: &str, gave_up: Option<&str>) -> Found {
        Found { place: String::from(place), held: false, gave_up: gave_up.map(String::from) }
    }

    /// The keyboard's controller failed at an early step of the probe (its BAR lies above 512 GiB: the mapping is
    /// the step nothing before the first round can prove): no controller exists in the driver's list, and the page
    /// still names the place and the step.
    #[test]
    fn a_probe_that_ended_before_the_controller_was_listed_has_its_line() {
        let window = crate::driver::register_window(Some(pci::Bar::Io { port: 0x3000, size: 0x100 })).unwrap_err();
        assert_eq!(window, "BAR0 is not a memory BAR");
        for reason in [window.as_str(), "map BAR0: OutOfBounds", "enable_device: PowerStuck { state: 3 }"] {
            let live = vec![gaming_pc().remove(0)];
            let c = merge(&[found("00:0d.0", None), found("80:14.0", Some(reason))], live);
            let l = lines(&c);
            assert_eq!(l[0], "USB-Baustein 00:0d.0: läuft, 3 Anschlüsse, 0 Geräte");
            assert_eq!(l[1], format!("USB-Baustein 80:14.0: läuft NICHT ({reason})"));
            assert_eq!(l.last().unwrap(), "USB zusammen: 0 Eingabegeräte, 0 Speicher, 0 Tongeräte");
        }
        // Both failed early: two lines with their reasons, never "no USB controller found".
        let l = lines(&merge(&[found("00:0d.0", Some("BAR0 size 0x40 is not an xHCI register window")), found("80:14.0", Some("map BAR0: OutOfBounds"))], vec![]));
        assert_eq!(l[0], "USB-Baustein 00:0d.0: läuft NICHT (BAR0 size 0x40 is not an xHCI register window)");
        assert_eq!(l[1], "USB-Baustein 80:14.0: läuft NICHT (map BAR0: OutOfBounds)");
        assert!(!l.iter().any(|x| x.contains("kein USB-Baustein gefunden")));
    }

    #[test]
    fn every_function_pci_found_has_a_line_in_pci_order() {
        // The driver's list in another order than PCI's, one function held back, one nobody probed.
        let mut live = gaming_pc();
        live.reverse();
        let held = Found { place: String::from("90:00.0"), held: true, gave_up: None };
        let c = merge(&[found("00:0d.0", None), found("80:14.0", Some("stale: a driver is bound since")), held, found("a0:00.0", None)], live);
        let places: Vec<&str> = c.iter().map(|x| x.place.as_str()).collect();
        assert_eq!(places, ["00:0d.0", "80:14.0", "90:00.0", "a0:00.0"]);
        assert!(c[0].running && c[1].running && c[1].rows.len() == 11, "the driver's controller wins over a note");
        let l = lines(&c);
        assert!(l.contains(&String::from("USB-Baustein 90:00.0: läuft NICHT (abgeschaltet)")), "{l:?}");
        assert!(l.contains(&String::from("USB-Baustein a0:00.0: läuft NICHT (kein Treiber wurde gestartet)")), "{l:?}");
        // A controller PCI does not list (it left the tree) keeps its line.
        let c = merge(&[], gaming_pc());
        assert_eq!(c.len(), 2);
        // Nothing in PCI and nothing in the driver: only then the one plain line.
        assert_eq!(lines(&merge(&[], vec![])).len(), 1);
    }

    #[test]
    fn no_controller_at_all_is_one_plain_line() {
        assert_eq!(lines(&[]), ["USB: kein USB-Baustein gefunden. Tastatur, Maus und Stick können so nicht gehen."]);
    }

    #[test]
    fn a_headset_with_keys_is_a_sound_device() {
        assert_eq!(kind(&["usb-audio", "usb-hid"]), "Tongerät mit Tasten");
        assert_eq!(kind(&["usb-audio"]), "Tongerät");
        assert_eq!(kind(&[]), "erkannt, wird nicht benutzt");
    }
}
