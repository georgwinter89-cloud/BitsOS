//! The report's rows, from the getters the five waves of 14.11. named (their notes' report sections). Each row's
//! wording is `gp_report::rows`' or the getter's own; this file only reads facts and puts rows in their group.

use super::Facts;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use gp_report::rows::{self, Clock, Cpus, FileState, McpDown, Remote, Watchdog};
use gp_report::{Group, Report};

/// The cable card of the Gaming-PC.
const RTL: (u16, u16) = (0x10ec, 0x8126);

fn tier(key: &str) -> bool {
    crate::config::get(key).as_deref().map(str::trim) == Some("on")
}

/// The network's self-check has said where it stands: no network tier, a card's stage 6 (address or the 30 s
/// note) or a stop, a stand-in card with an address (QEMU), or after 20 s no card at all.
pub(super) fn network_settled(elapsed_s: u64) -> bool {
    if !tier(gp_platform::tiers::NET_KEY) {
        return true;
    }
    let l = rtl8126::report_lines();
    if l.iter().any(|x| x.starts_with("Netzwerk 6/8") || x.starts_with("Netzwerk 7/8") || gp_report::page::must_keep(x)) {
        return true;
    }
    if l.is_empty() && interfaces().iter().any(|i| i.info.addr.is_some()) {
        return true;
    }
    l.is_empty() && elapsed_s >= 20
}

/// A card has an address now (the late DHCP answer the page waits for after its one redraw).
pub(super) fn has_address() -> bool {
    interfaces().iter().any(|i| i.info.addr.is_some())
}

/// The file's row before anything waits for the stick (`rows::file_first`): USB runs when an xHCI function has no
/// failure note from its probe.
pub(super) fn file_first() -> FileState {
    let usb = pci::tree().iter().any(|n| n.class & 0xFF_FFFF == 0x0C_0330 && pci::failed::last(n.loc).is_none());
    rows::file_first(tier(gp_platform::tiers::STICK_KEY), acpi::dmar::verdict().blocks_dma(), usb)
}

fn interfaces() -> Vec<net::stack::IfSummary> {
    net::stack::kernel().map(|s| s.interfaces()).unwrap_or_default()
}

fn ip(a: net::Ipv4) -> String {
    let [a, b, c, d] = a.0;
    format!("{a}.{b}.{c}.{d}")
}

pub(super) fn report(f: &Facts, mcp: &Result<u16, McpDown>, file: FileState, trace: bool) -> Report {
    let up = time::now_ns() / 1_000_000_000;
    let version = f.generation.map_or(String::new(), |(n, _)| format!(", Version {n}"));
    let mut r = Report::new(format!("BitsOS-Bericht für den Gaming-PC{version}, {up} Sekunden nach dem Start"));
    // The first collection names each slow getter before it asks: a getter that blocks leaves its name as the last
    // line on the quiet screen (`console::logfilter::PROFILE_IMPORTANT`).
    let reading = |what: &'static str| {
        if trace {
            klog::event!(Info, "kernel", "gp.report.reading", part = what);
        }
    };
    start(&mut r, f, &file);
    reading("cpu");
    cpu(&mut r);
    guard(&mut r);
    reading("pci");
    r.extend(Group::Devices, pci::gp_report_lines());
    disks(&mut r);
    r.push(Group::Picture, pci::gp_picture_line(f.fb));
    r.push(Group::Picture, rows::picture_speed(write_combining()));
    reading("usb");
    r.extend(Group::Usb, xhci::gp_report::report_lines());
    reading("network");
    network(&mut r, mcp);
    reading("gpu");
    let gpu = amdgpu::rdna4::live::report_lines();
    if gpu.is_empty() {
        r.push(Group::Gpu, "Grafikkarte: keine RX 9070 XT gefunden");
    }
    r.extend(Group::Gpu, gpu);
    r.push(Group::Watchdog, rows::watchdog(watchdog()));
    // GP-SOUND's getter names the mixer's own choice, every USB output (headset and the board's jacks) and `snd ton`.
    r.push(Group::Sound, snd::tone::report_line());
    r
}

fn start(r: &mut Report, f: &Facts, file: &FileState) {
    use gp_platform::tiers::{NET_KEY, NVME_KEY, STICK_KEY, Tiers};
    let t = Tiers::from_values(crate::config::get(NET_KEY).as_deref(), crate::config::get(STICK_KEY).as_deref(), crate::config::get(NVME_KEY).as_deref());
    let reason = crate::config::get("profile_reason").unwrap_or_default();
    let refused = crate::config::get(gp_platform::tiers::REFUSED_KEY).is_some();
    r.extend(Group::Start, gp_platform::tiers::report_lines(t, reason.trim(), refused));
    // GP-DISK: started from the SSD's own partition, the "Start-Stick" lines above mean that partition.
    if fs_fat::stick::on_nvme() {
        r.push(Group::Start, gp_platform::disk_rule::BOOT_FROM_SSD);
    }
    if let Some((n, why)) = f.generation {
        r.push(Group::Start, format!("Laufende Version: {n} ({why})"));
    }
    r.push(Group::Start, rows::version_number(bitsver::NUMBER, bitsver::DATE));
    for n in &f.unguarded {
        r.push(Group::Start, rows::unguarded(*n));
    }
    r.push(Group::Start, rows::file(file));
}

fn cpu(r: &mut Report) {
    let o = platform::smp::outcome().unwrap_or_default();
    let topos: Vec<hal::smp::Topology> = (0..hal::percpu::cpu_count()).filter_map(hal::smp::topology).collect();
    let shape = platform::report::shape(&topos);
    let hwp_on = usize::from(o.bsp_hwp.is_on()) + o.up.iter().filter(|a| a.hwp.is_on()).count();
    let planned = if o.ran { o.planned + 1 } else { 1 };
    r.extend(Group::Cpu, rows::cpus(Cpus { up: hal::percpu::cpu_count(), planned, p_cores: shape.p_cores, e_cores: shape.e_cores, hwp_on }));
    if let Some(c) = time::calibration() {
        let (checked, ppm) = match c.settled {
            Some((_, s)) => (s.from == gp_platform::clock::RateFrom::Cpuid15Checked, s.ppm),
            None => (false, c.ppm_vs_cpuid()),
        };
        r.extend(Group::Cpu, rows::clock(Clock { hz: c.hz, cpuid15_hz: c.cpuid.leaf15_hz.unwrap_or(0), ppm, cpuid15_checked: checked, hpet_left_out: c.hpet_trust.untrusted() }));
    }
    if let Some(a) = acpi::live::tables() {
        if let Some(m) = &a.madt {
            r.push(Group::Cpu, rows::madt(m.enabled_cpus(), m.empty_slots, a.notes.total() as usize));
        }
        let s5 = a.s5.as_ref().map(|(s, _, _)| s.typ_a);
        r.push(Group::Cpu, rows::power(s5, a.fadt.as_ref().is_some_and(|f| f.reset.is_some())));
    }
    let faults = hal::msr::faults();
    let regs: Vec<u32> = faults.registers.iter().take(faults.distinct).copied().collect();
    r.push(Group::Cpu, rows::msr_faults(faults.count, &regs));
    if hal::cpu::detect().intel {
        r.push(Group::Cpu, rows::spec_ctrl(hal::msr::read_checked(hal::msr::IA32_SPEC_CTRL)));
    }
}

fn guard(r: &mut Report) {
    r.push(Group::Guard, rows::dma(acpi::dmar::verdict().name()));
    // Drivers the DMA gate stopped: their probe's note names the IOMMU (pci's `DmaBlocked` text).
    let stopped: Vec<String> = pci::tree()
        .iter()
        .filter_map(|n| pci::failed::last(n.loc).filter(|x| x.error.contains("IOMMU")).map(|x| format!("{} ({})", n.loc, x.driver)))
        .collect();
    if let Some(l) = rows::dma_stopped(&stopped) {
        r.push(Group::Guard, l);
    }
    if let Some(s) = acpi::dmar::found() {
        r.push(Group::Guard, format!("Speicherschutz-Bausteine: {} gefunden (die Werte stehen in der Datei)", s.units.len()));
    }
}

/// GP-DISK: one row per storage function, saying which one has a driver and why the others have none.
fn disks(r: &mut Report) {
    let tree = pci::tree();
    let line = crate::config::get(gp_platform::tiers::NVME_KEY);
    let list: Vec<String> = tree
        .iter()
        .filter(|n| n.class >> 16 == 0x01 && (n.untouched.is_some() || pci::gp::storage_verdict(n) == gp_platform::disk_rule::named::Verdict::Released))
        .map(|n| gp_platform::disk_rule::report_row((n.loc.bus, n.loc.dev, n.loc.func), n.vendor, n.device, n.class, pci::gp::storage_verdict(n), line.as_deref()))
        .collect();
    if list.is_empty() {
        r.push(Group::Disks, "Eingebaute Platten: keine gefunden");
    }
    r.extend(Group::Disks, list);
    if let Some(l) = line.as_deref() {
        let place = gp_platform::disk_rule::named::named(Some(l)).map(|n| n.place());
        let class = place.and_then(|p| tree.iter().find(|n| n.loc.segment == 0 && (n.loc.bus, n.loc.dev, n.loc.func) == p).map(|n| n.class));
        if let Some(row) = gp_platform::disk_rule::report_unmatched(l, class) {
            r.push(Group::Disks, row);
        }
    }
}

/// gopfb's live window is mapped write-combining (gpx note U11): the PAT entry and the first page's cache type.
fn write_combining() -> Option<bool> {
    let (_, target) = gopfb::live()?;
    let (virt, _) = target.window()?;
    let root = hal::paging::kernel_root().unwrap_or_else(hal::paging::current_root);
    let leaf = hal::paging::translate_leaf(root, virt)?;
    let pat_wc = hal::pat::pat_entry(hal::pat::read_pat(), 1) == hal::pat::MT_WC;
    Some(pat_wc && leaf.flags.cache() == Some(hal::paging::CacheType::WriteCombining))
}

fn network(r: &mut Report, mcp: &Result<u16, McpDown>) {
    let net_on = tier(gp_platform::tiers::NET_KEY);
    let card_here = pci::tree().iter().any(|n| (n.vendor, n.device) == RTL);
    let stages = rtl8126::report_lines();
    if stages.is_empty() {
        r.push(Group::Network, rows::net_silent(card_here, !net_on));
    }
    r.extend(Group::Network, stages);
    let ifs = interfaces();
    for i in &ifs {
        let addr = i.info.addr.map_or(String::from("noch keine Adresse"), |(a, p)| format!("Adresse {}/{p}", ip(a)));
        let link = if i.info.link_up { format!("Kabel verbunden ({} Mbit/s)", i.info.speed_mbps) } else { String::from("kein Kabel") };
        r.push(Group::Network, format!("Karte {}: {link}, {addr}", i.name));
    }
    let remote = match mcp {
        Err(why) => Remote::NotStarted(*why),
        Ok(_) if !net_on => Remote::NetOff,
        Ok(port) => match ifs.iter().find_map(|i| i.info.addr) {
            Some((a, _)) => Remote::Reachable { addr: ip(a), port: *port },
            None => Remote::NoAddress,
        },
    };
    r.push(Group::Network, rows::remote(&remote));
}

fn watchdog() -> Watchdog {
    match acpi::tco::found() {
        None => Watchdog::Absent,
        Some(t) => match t.regs {
            None => Watchdog::Unreadable,
            Some(x) if x.cnt1 & acpi::tco::CNT_TMR_HLT != 0 => Watchdog::Halted,
            Some(_) => Watchdog::Running { timeout_s: t.timeout_s().unwrap_or(0) },
        },
    }
}
