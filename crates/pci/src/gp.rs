//! What the PCI core does differently on Georg's Gaming-PC (plan/spec/gaming-pc.md §2 and §5), all of it behind one
//! question: does CONFIG say `profile=gaming-pc`? bitsboot writes that line when the DMI names the board or an Intel
//! CPU runs on bare metal (`boot/bitsboot/src/pure/profile.rs`), and `kernel/src/config.rs` adds it for an older
//! bitsboot. The Mini-PC's CONFIG has no such line, so nothing in this file runs there.

use crate::node::Node;
use alloc::string::String;
use alloc::vec::Vec;
use gp_platform::never_touch::{self, Verdict};

pub const PROFILE_KEY: &str = "profile";
pub const PROFILE: &str = "gaming-pc";

pub fn active() -> bool {
    crate::registry::config_value(PROFILE_KEY).is_some_and(|p| p.trim() == PROFILE)
}

/// The untouched key of a function on the never-touch list (B4), under the profile only. These keys are no
/// switches: no CONFIG line releases them.
pub fn never_touch_key(active: bool, vendor: u16, device: u16, class: u32) -> Option<&'static str> {
    if !active {
        return None;
    }
    never_touch::key(vendor, device, class)
}

/// A wireless controller: class 02/80 (how Realtek and Intel WLAN cards report) or class 0D ("wireless").
pub fn is_wlan_class(class: u32) -> bool {
    class & 0xFF_FF_00 == 0x02_80_00 || class >> 16 == 0x0D
}

/// The profile's network rule (plan/spec/gaming-pc.md §2), under the profile only: every wireless controller is
/// held for good (this PC is always on the cable, Georg 5.11.2026; the never-touch key `gp_wlan`), every other
/// network function until Georg's tier line `gp_net=on` (`net`: the tier as CONFIG has it).
pub fn network_key(active: bool, net: bool, class: u32) -> Option<&'static str> {
    if !active || class >> 16 != 0x02 && class >> 16 != 0x0D {
        return None;
    }
    if is_wlan_class(class) {
        return Some(never_touch::Why::Wlan.key());
    }
    (!net).then_some(gp_platform::tiers::NET_KEY)
}

/// The VMD functions of a tree (report line only: BitsOS has no VMD driver and never touches one).
pub fn vmd_functions(nodes: &[Node]) -> Vec<&Node> {
    nodes.iter().filter(|n| matches!(never_touch::classify(n.vendor, n.device, n.class), Verdict::ReportOnly(never_touch::Why::Vmd))).collect()
}

/// Boot lines after the scan, under the profile: one per VMD function.
pub fn log_after_scan(nodes: &[Node]) {
    if !active() {
        return;
    }
    let nvme = nodes.iter().filter(|n| n.class == 0x01_08_02).count();
    for n in vmd_functions(nodes) {
        klog::event!(Info, "pci", "vmd", function = alloc::format!("{}", n.loc), device = klog::Hex(n.device as u64), nvme_seen = nvme;
            "Intel VMD is switched on and left alone: a drive mapped under it would be invisible to this list");
    }
    log_named(nodes);
}

/// GP-DISK: what the scan made of a `gp_nvme=` line, per storage function (the report page's "Platten" rows).
pub fn storage_verdict(n: &Node) -> gp_platform::disk_rule::named::Verdict {
    use crate::registry::config_value;
    let place = (n.loc.segment == 0).then_some((n.loc.bus, n.loc.dev, n.loc.func));
    let (bind, line) = (config_value("nvme_bind"), config_value(gp_platform::tiers::NVME_KEY));
    match place {
        Some(p) => gp_platform::disk_rule::named::verdict(n.class, bind.as_deref(), line.as_deref(), p, (n.vendor, n.device)),
        None if crate::registry::storage_kept_off(n.class, bind.as_deref()) => gp_platform::disk_rule::named::Verdict::Kept,
        None => gp_platform::disk_rule::named::Verdict::Free,
    }
}

/// GP-DISK: one line for a `gp_nvme=` line: the controller released, or why nothing was.
fn log_named(nodes: &[Node]) {
    use gp_platform::disk_rule::named::{self, Verdict};
    let Some(line) = crate::registry::config_value(gp_platform::tiers::NVME_KEY).filter(|l| !l.trim().is_empty()) else { return };
    let Some(Ok(want)) = named::read(Some(&line)) else {
        // The loader takes such a line away; only an older one hands it over.
        klog::event!(Warn, "pci", "gp_nvme.unreadable", line = line.trim(); "gp_nvme= names no controller (or Georg's Windows or CachyOS drive): every storage function stays untouched");
        return;
    };
    let at = nodes.iter().find(|n| n.loc.segment == 0 && (n.loc.bus, n.loc.dev, n.loc.func) == want.place());
    match at.map(|n| (n, storage_verdict(n))) {
        Some((n, Verdict::Released)) => klog::event!(Info, "pci", "gp_nvme.released", function = alloc::format!("{}", n.loc), vendor = klog::Hex(n.vendor as u64), device = klog::Hex(n.device as u64);
            "the NVMe controller gp_nvme= names may get its driver; every other storage function stays untouched"),
        Some((n, _)) => klog::event!(Warn, "pci", "gp_nvme.refused", function = alloc::format!("{}", n.loc), wanted = want.render(), vendor = klog::Hex(n.vendor as u64), device = klog::Hex(n.device as u64), class = klog::Hex(n.class as u64);
            "the function at the place gp_nvme= names is another one: it stays untouched, no storage function gets a driver"),
        None => klog::event!(Warn, "pci", "gp_nvme.absent", wanted = want.render(); "no function at the place gp_nvme= names: no storage function gets a driver"),
    }
}

/// The place of a function as the report page writes it.
fn place(n: &Node) -> String {
    alloc::format!("{:02x}:{:02x}.{}", n.loc.bus, n.loc.dev, n.loc.func)
}

/// What a function is, in plain German (the report page; no code words).
pub fn german_name(n: &Node) -> &'static str {
    if let Some(why) = never_touch::classify(n.vendor, n.device, n.class).why() {
        return match why {
            never_touch::Why::Flash => "Baustein mit der Firmware des Mainboards",
            never_touch::Why::Vmd => "Intel VMD (Laufwerks-Verwaltung)",
            never_touch::Why::Thunderbolt => "Thunderbolt/USB4",
            never_touch::Why::ManagementEngine => "Intel-Verwaltungschip",
            never_touch::Why::ChipsetAudio => "Ton vom Mainboard-Chip",
            never_touch::Why::GraphicsAudio => "Ton der Grafikkarte",
            never_touch::Why::Wlan => "WLAN-Karte",
        };
    }
    match (n.class >> 16, (n.class >> 8) & 0xff, n.class & 0xff) {
        (0x01, 0x08, _) => "SSD",
        (0x01, 0x06, _) => "SATA-Anschlüsse",
        (0x01, _, _) => "Laufwerks-Anschluss",
        (0x02, 0x00, _) => "Netzwerkkarte (Kabel)",
        (0x02, _, _) => "Funk-Netzwerkkarte",
        (0x03, _, _) => "Grafik",
        (0x04, _, _) => "Ton",
        (0x06, 0x00, _) => "Hauptknoten",
        (0x06, 0x04, _) => "Abzweig",
        (0x06, _, _) => "Mainboard-Baustein",
        (0x0c, 0x03, 0x30) => "USB-Anschlüsse",
        (0x0c, 0x03, _) => "USB (andere Art)",
        (0x0c, 0x05, _) => "Mainboard-Sensorleitung",
        _ => "Mainboard-Baustein",
    }
}

/// A function with a row of its own on the report page: not a bridge and not a host bridge (they only connect).
fn has_row(n: &Node) -> bool {
    !n.is_bridge() && n.class >> 8 != 0x0600
}

fn many(n: usize, one: &str, more: &str) -> String {
    alloc::format!("{n} {}", if n == 1 { one } else { more })
}

/// What a strand has besides its rows: bridges and host bridges, which the page counts but does not list.
fn unlisted(bridges: usize, hosts: usize) -> String {
    match (bridges, hosts) {
        (0, 0) => String::new(),
        (b, 0) => alloc::format!(", dazu {} ohne eigene Zeile", many(b, "Abzweig", "Abzweige")),
        (0, h) => alloc::format!(", dazu {} ohne eigene Zeile", many(h, "Hauptknoten", "Hauptknoten")),
        (b, h) => alloc::format!(", dazu {} und {} ohne eigene Zeile", many(b, "Abzweig", "Abzweige"), many(h, "Hauptknoten", "Hauptknoten")),
    }
}

/// The report page's PCI part in plain German (`plan/spec/gaming-pc.md` section 8; section 5 has the
/// expected text for this machine). `driver`: the driver bound to a function, if any. `failed`: the last driver
/// whose probe of a function ended in an error (`failed::last`): "tried and gave up" is not "no driver exists".
pub fn report_lines(
    nodes: &[Node],
    roots: &[crate::enumerate::Root],
    planned: &[crate::peer::Planned],
    driver: &dyn Fn(crate::Loc) -> Option<String>,
    failed: &dyn Fn(crate::Loc) -> Option<crate::failed::Failure>,
) -> Vec<String> {
    let mut out = Vec::new();
    let strands: Vec<String> = roots.iter().map(|r| alloc::format!("{:02x} bis {:02x}", r.bus_start, r.bus_end)).collect();
    out.push(alloc::format!("Anschluss-Stränge: {} (Bus {}), {} Geräte und Abzweige gefunden", roots.len(), strands.join(" und "), nodes.len()));
    for p in planned.iter().filter(|p| p.plan.refused.is_some()) {
        let how = if p.from_facts { "die bekannte Aufteilung dieses Mainboards wurde genommen" } else { "es wurde nur der erste Strang abgesucht" };
        out.push(alloc::format!("Achtung: Die Suche nach dem zweiten Strang war nicht eindeutig, {how}."));
    }
    for (i, r) in roots.iter().enumerate() {
        let on = |n: &&Node| n.loc.segment == r.segment && (r.bus_start..=r.bus_end).contains(&n.loc.bus);
        let rows: Vec<&Node> = nodes.iter().filter(on).filter(|n| has_row(n)).collect();
        let bridges = nodes.iter().filter(on).filter(|n| n.is_bridge()).count();
        let hosts = nodes.iter().filter(on).filter(|n| !n.is_bridge() && !has_row(n)).count();
        // GP-R2: the number is the rows below it; the functions without a row are counted beside it, so the strands
        // add up to the first line's total.
        out.push(alloc::format!("Strang {} (ab Bus {:02x}): {}{}", i + 1, r.bus_start, many(rows.len(), "Gerät", "Geräte"), unlisted(bridges, hosts)));
        for n in rows {
            let state = match (n.untouched, driver(n.loc)) {
                (Some(k), _) if never_touch::is_key(k) => String::from("wird nie angefasst"),
                (Some(_), _) => String::from("abgeschaltet, nicht angefasst"),
                (None, Some(d)) => alloc::format!("läuft mit {d}"),
                (None, None) => match failed(n.loc) {
                    // The reason is the driver's own text: the one fragment of the line that is not German.
                    Some(f) => alloc::format!("Treiber {} hat aufgegeben ({})", f.driver, f.error),
                    None => String::from("erkannt, ohne Treiber"),
                },
            };
            out.push(alloc::format!("  {} {} [{:04x}:{:04x}]: {state}", place(n), german_name(n), n.vendor, n.device));
        }
    }
    let never = |row: bool| nodes.iter().filter(|n| n.untouched.is_some_and(never_touch::is_key) && has_row(n) == row).map(place).collect::<Vec<String>>();
    let (listed, bridges) = (never(true), never(false));
    let beside = if bridges.is_empty() { String::new() } else { alloc::format!(", dazu {} ohne eigene Zeile ({})", many(bridges.len(), "Abzweig", "Abzweige"), bridges.join(", ")) };
    out.push(alloc::format!("Nie angefasst: {} ({}){beside}", many(listed.len(), "Gerät", "Geräte"), listed.join(", ")));
    let nvme = nodes.iter().filter(|n| n.class == 0x01_08_02).count();
    for n in vmd_functions(nodes) {
        out.push(alloc::format!("Intel VMD ist im BIOS eingeschaltet ({}). BitsOS fasst es nicht an. SSDs hinter VMD wären unsichtbar; gefunden wurden {nvme} SSDs.", place(n)));
    }
    out
}

/// The function whose memory BAR holds all of `[phys, phys + size)`, with the BAR's index.
pub fn bar_owner(nodes: &[Node], phys: u64, size: u64) -> Option<(&Node, usize)> {
    let end = phys.checked_add(size)?;
    nodes.iter().find_map(|n| {
        let bars = n.bars?;
        bars.iter().position(|b| matches!(b, Some(crate::node::Bar::Mem { phys: start, size: len, .. }) if *start != 0 && *start <= phys && end <= start.saturating_add(*len))).map(|i| (n, i))
    })
}

/// The picture line of the report page. `fb`: the framebuffer the loader handed over (address, bytes, width,
/// height), `None` when BitsOS runs without a picture.
pub fn picture_line(nodes: &[Node], fb: Option<(u64, u64, u32, u32)>) -> String {
    let Some((phys, size, width, height)) = fb else {
        return String::from("Bild: keins übergeben, BitsOS läuft ohne Bild");
    };
    let high = if phys >= 512 << 30 { ", oberhalb von 512 GiB" } else { "" };
    match bar_owner(nodes, phys, size) {
        Some((n, _)) => alloc::format!("Bild: {width} x {height}, liegt im Speicher von {} {} (Adresse {phys:#x}{high})", german_name(n), place(n)),
        None => alloc::format!("Bild: {width} x {height} an Adresse {phys:#x}{high}, gehört zu keinem gefundenen Gerät"),
    }
}
