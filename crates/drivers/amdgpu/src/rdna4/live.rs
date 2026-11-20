//! The kernel glue of the read-only attach: the PCI match for 1002:7550, the function's facts from the scan, BAR5
//! mapped uncached (512 KiB) and BAR0 read through a window of the pages that hold the discovery binary (three at
//! the default place, at most seventeen at a place the card's mailbox names). The 16 GiB of BAR0 are never mapped as
//! one object. Nothing here writes: the `Hw` below refuses every write itself,
//! behind the owner's shut gate, and no configuration register is written (the function is not even enabled: the
//! firmware left memory decode on, and a card found with it off is reported, not switched on).

use super::attach::{BarFact, Link, LinkNow, PciFacts};
use super::{Card, fw, snapshot};
use crate::devs::{self, Device, Family, Key};
use crate::gpucore::fwload::StickSource;
use crate::gpucore::psp::Hw;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use hal::mmio::IoMem;
use hal::paging::CacheType;

/// PCI capability ids.
const CAP_PM: u8 = 0x01;
const CAP_MSI: u8 = 0x05;
const CAP_MSIX: u8 = 0x11;

/// A BAR as the PCI scan recorded it. Not `PciFunction::bar`: that sizes a function the scan left unsized, and sizing
/// writes the BAR. A card the scan did not size shows no BAR here and the attach stops at step 1.
pub fn scan_bar(nodes: &[pci::Node], at: pci::Loc, i: usize) -> Option<pci::Bar> {
    nodes.iter().find(|n| n.loc == at).and_then(|n| n.bars).and_then(|b| b.get(i).copied().flatten())
}

fn mem_bar(nodes: &[pci::Node], at: pci::Loc, i: usize) -> Option<BarFact> {
    match scan_bar(nodes, at, i) {
        Some(pci::Bar::Mem { phys, size, prefetch, is64 }) => Some(BarFact { phys, bytes: size, is64, prefetch }),
        _ => None,
    }
}

fn key_of(l: pci::Loc) -> Key {
    Key::new(l.segment, l.bus, l.dev, l.func)
}

fn link_of(n: &pci::Node) -> Option<Link> {
    let p = n.pcie.as_ref().filter(|p| p.link_width != 0)?;
    Some(Link { speed: p.link_speed, width: p.link_width, max_speed: p.max_speed, max_width: p.max_width })
}

/// The function's facts: the scan's record and configuration reads.
pub fn facts(f: &pci::PciFunction) -> PciFacts {
    let nodes = pci::tree();
    let at = nodes.iter().position(|n| n.loc == f.loc());
    let link = at.and_then(|i| link_of(&nodes[i]));
    // The narrowest link on the way up (the card's own switch, then the CPU's root port).
    let upstream = at.and_then(|i| {
        pci::node::ancestors(&nodes, i)
            .into_iter()
            .filter_map(|p| link_of(&nodes[p]).map(|l| (key_of(nodes[p].loc), l)))
            .min_by_key(|(_, l)| (l.width, l.speed))
    });
    // Linux's walk for the power controller's link limits (`pcie_bandwidth_available`): the card, then every
    // bridge up to the root port, each with its Link Status as the scan read it (0 without a link).
    let now = |i: usize| nodes[i].pcie.map_or(LinkNow { speed: 0, width: 0 }, |p| LinkNow { speed: p.link_speed, width: p.link_width });
    let path = at.map_or(Vec::new(), |i| core::iter::once(i).chain(pci::node::ancestors(&nodes, i)).map(now).collect());
    let (vendor, signature) = (hal::cpu::cpuid(0, 0), hal::cpu::cpuid(1, 0));
    let link_host = rdna4_core::pcielink::host([vendor.ebx, vendor.edx, vendor.ecx], signature.eax);
    let power_state = f.find_capability(CAP_PM).map(|c| (f.read16(c + 4) & 3) as u8);
    PciFacts {
        key: Key::new(f.segment, f.addr.bus, f.addr.dev, f.addr.func),
        vendor: f.vendor,
        device: f.device,
        revision: f.revision,
        subsystem: f.subsystem,
        class: f.class,
        command: f.command(),
        power_state,
        bar0: mem_bar(&nodes, f.loc(), 0),
        bar2: mem_bar(&nodes, f.loc(), 2),
        bar5: mem_bar(&nodes, f.loc(), 5),
        link,
        upstream,
        path,
        link_host,
        msi: f.find_capability(CAP_MSI).is_some(),
        msix: f.find_capability(CAP_MSIX).is_some(),
    }
}

/// `at`: the byte offset in VRAM. Maps the pages around the read, reads, unmaps (also the thermal taker's read
/// of the driver table, `liveih`).
pub(super) fn window_read(bar0: &BarFact, at: u64, buf: &mut [u8]) -> bool {
    let Some(end) = at.checked_add(buf.len() as u64).filter(|e| *e <= bar0.bytes) else { return false };
    let first = at & !0xFFF;
    let len = (end - first).next_multiple_of(0x1000) as usize;
    let io = match hal::mmio::map_device(bar0.phys + first, len, CacheType::Uncached) {
        Ok(io) => io,
        Err(e) => {
            klog::event!(Warn, "gx", "rdna4.window", phys = klog::Hex(bar0.phys + first), bytes = len, why = format!("{e:?}").as_str(); "the BAR0 window could not be mapped");
            return false;
        }
    };
    let skip = (at - first) as usize;
    for (i, chunk) in buf.chunks_mut(4).enumerate() {
        let Ok(v) = io.read32(skip + i * 4) else { return false };
        chunk.copy_from_slice(&v.to_le_bytes()[..chunk.len()]);
    }
    true
}

/// The card's registers and memory as the attach reads them.
struct LiveHw {
    bar5: IoMem,
    bar0: BarFact,
}

impl Hw for LiveHw {
    fn reg_read(&mut self, byte: u32) -> Option<u32> {
        self.bar5.read32(byte as usize).ok()
    }
    fn reg_write(&mut self, byte: u32, value: u32) -> bool {
        klog::event!(Error, "gx", "rdna4.write_refused", byte = klog::Hex(u64::from(byte)), value = klog::Hex(u64::from(value)); "a register write reached a read-only card's glue: refused");
        false
    }
    fn mem_read(&mut self, at: u64, buf: &mut [u8]) -> bool {
        window_read(&self.bar0, at, buf)
    }
    fn mem_write(&mut self, at: u64, data: &[u8]) -> bool {
        klog::event!(Error, "gx", "rdna4.write_refused", at = klog::Hex(at), bytes = data.len(); "a memory write reached a read-only card's glue: refused");
        false
    }
    fn now_us(&mut self) -> u64 {
        time::now_ns() / 1000
    }
    fn delay_us(&mut self, us: u64) {
        sched::sleep_ns(us.saturating_mul(1000));
    }
}

fn run(dev: &Device, facts: PciFacts) -> Card {
    let card = match super::attach::pci_gate(&facts) {
        Err(stop) => Card { facts, state: Err(stop), totals: Default::default(), took_us: 0 },
        Ok((bar0, bar5)) => match hal::mmio::map_device(bar5.phys, bar5.bytes as usize, CacheType::Uncached) {
            Ok(io) => Card::attach(facts, LiveHw { bar5: io, bar0 }).0,
            Err(e) => {
                klog::event!(Warn, "gx", "rdna4.bar5", why = format!("{e:?}").as_str());
                Card { facts, state: Err(super::attach::Stop::Bar5("could not be mapped")), totals: Default::default(), took_us: 0 }
            }
        },
    };
    let function = format!("{}", dev.key);
    match &card.state {
        Ok(a) => klog::event!(Info, "gx", "rdna4.attached", card = dev.index, function = function.as_str(), display = dev.display, memsize_mib = a.memsize_mib,
            entries = a.discovery.ips.len(), as_measured = a.differs.is_empty(), writes = card.totals.writes, refused = card.totals.refused;
            "RX 9070 XT attached read-only: nothing written"),
        Err(stop) => klog::event!(Warn, "gx", "rdna4.stopped", card = dev.index, function = function.as_str(), step = stop.step().0, why = format!("{stop}").as_str();
            "RX 9070 XT: the read-only attach stopped; the card was left as found"),
    }
    card
}

/// The card of a device, attached on first use (thread context: it maps and may sleep up to 2 s for a card whose
/// firmware is not ready; a second caller sleeps meanwhile, `CardSlot`). None for a device that is no RX 9070 XT or
/// has no PCI function.
pub fn card(dev: &'static Device) -> Option<&'static Card> {
    if dev.family != Family::Rdna4 {
        return None;
    }
    let f = dev.pci.as_ref()?;
    if let Some(card) = dev.card.get() {
        return Some(card);
    }
    // Configuration space is read before the slot's lock is taken: PCI's locks lie outside every driver lock
    // (ksync::order), and a second caller's reads of it change nothing.
    let facts = facts(f);
    Some(dev.card.get_or_attach(|| run(dev, facts)))
}

/// The card if an earlier call attached it (never maps).
pub fn card_now(dev: &Device) -> Option<&Card> {
    dev.card.get()
}

/// MCP `gpu` for a card: the snapshot, with the firmware files found on the stick.
pub fn answer(dev: &'static Device) -> Result<String, String> {
    let card = card(dev).ok_or_else(|| format!("{} is no RX 9070 XT with a PCI function", dev.card_name()))?;
    let files = fw::on_stick(&fw::wanted(card.facts.device, card.facts.revision), &mut StickSource);
    let mut text = snapshot::text(dev, card, &files);
    for l in super::liveih::thermal_look(dev).lines {
        text.push_str(&l);
        text.push('\n');
    }
    Ok(text)
}

/// The cards' lines for the Gaming-PC's report page, in the stable order (attaches what is not attached yet).
pub fn report_lines() -> Vec<String> {
    devs::all().iter().filter_map(|d| card(d).map(|c| snapshot::report_line(d, c))).collect()
}

const MATCHES: [pci::Match; 1] = [pci::Match::Id { vendor: rdna4_core::card::VENDOR, device: rdna4_core::card::DEVICE }];

fn probe(f: &pci::PciFunction) -> Result<(), String> {
    let dev = devs::of(f).ok_or_else(|| String::from("not in the GPU registry (the scan changed?)"))?;
    // A note crosses one Reload: a kernel in which the stage sos never runs hands the bootloader's on from here.
    if let Some((n, kept)) = crate::psplive::note_io().and_then(|io| super::bootnote::keep(io, dev.key)) {
        klog::event!(Info, "gx", "rdna4.bootnote", card = dev.index, sent = n.sent, kept = kept; "the kernel before this Reload sent to the card's bootloader: its note is handed on, and the stage sos sends nothing while the secure OS does not run");
    }
    let card = card(dev).ok_or_else(|| String::from("not an RX 9070 XT"))?;
    match &card.state {
        Ok(_) => Ok(()),
        Err(stop) => Err(format!("read-only attach stopped at step {}: {stop}", stop.step().0)),
    }
}
pci::pci_driver!(pci::DriverEntry { name: "amdgpu-rdna4", matches: &MATCHES, probe });
