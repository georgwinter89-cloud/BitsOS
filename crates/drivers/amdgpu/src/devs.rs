//! GP-GPU1 (plan/spec/gpu-rdna4.md piece 3): one device object per GPU function, keyed by its PCI ADDRESS, in a registry with a
//! stable order. Two cards of the Gaming-PC share ids, table and register bases; a 780M and a card may later sit in
//! one machine (OCuLink). So nothing in the driver may mean "the GPU": a caller holds a `&'static Device`, taken from
//! here by address, by index, or by role.
//!
//! - **Order** (`order`): the function whose BAR0 holds the firmware's framebuffer is the boot display and comes
//!   first; the others follow by PCI address. A device's index is its place in that order for the whole boot: its
//!   DRM node is `renderD<128 + index>` (`card<index>` below /sys), whichever device comes up first.
//! - **Families**: `Rdna4` is keyed by PCI id (1002:7550, attached read-only, `rdna4`); every other AMD display
//!   function is tried as the 780M's driver always tried it (`Phoenix`: the first one found, decided by its own
//!   read gate), so the Mini-PC's path is the same function with the same reads as before.
//! - **State**: what used to be statics of the 780M's modules (the device object, the scanout pair, the render node
//!   and its job keeping, the boot's bring-up, the PSP run) lives in the device (`Device::hw`, `boot`, `apu`).
//!
//! Devices never go away (no hot-unplug of a GPU function), so they are `'static`.

use crate::attach::Gop;
use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use ksync::Once;

/// A PCI function's address, the only key of a device. Prints as Linux's `pci_name` (`0000:04:00.0`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Key {
    pub segment: u16,
    pub bus: u8,
    pub dev: u8,
    pub func: u8,
}

impl Key {
    pub const fn new(segment: u16, bus: u8, dev: u8, func: u8) -> Key {
        Key { segment, bus, dev, func }
    }

    /// `0000:04:00.0` or `04:00.0` (hex, as lspci prints it).
    pub fn parse(s: &str) -> Option<Key> {
        let parts: Vec<&str> = s.trim().split(':').collect();
        let (segment, bus, rest) = match parts.as_slice() {
            [bus, rest] => (0, *bus, *rest),
            [seg, bus, rest] => (u16::from_str_radix(seg, 16).ok()?, *bus, *rest),
            _ => return None,
        };
        let (dev, func) = rest.split_once('.')?;
        let key = Key { segment, bus: u8::from_str_radix(bus, 16).ok()?, dev: u8::from_str_radix(dev, 16).ok()?, func: func.parse().ok()? };
        (key.dev < 32 && key.func < 8 && bus.len() == 2 && dev.len() == 2).then_some(key)
    }
}

impl core::fmt::Display for Key {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{:04x}:{:02x}:{:02x}.{}", self.segment, self.bus, self.dev, self.func)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Family {
    /// The 780M's driver (Phoenix 1002:15BF, Hawk Point 1002:1900; decided by the display engine's read gate).
    Phoenix,
    /// RX 9070 XT (Navi 48), read-only in GP-GPU1.
    Rdna4,
}

/// The family that handles an AMD display function.
pub fn family_of(vendor: u16, device: u16) -> Option<Family> {
    match (vendor, device) {
        (rdna4_core::card::VENDOR, rdna4_core::card::DEVICE) => Some(Family::Rdna4),
        (0x1002, _) => Some(Family::Phoenix),
        _ => None,
    }
}

/// What the order needs to know of a function.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Seen {
    pub key: Key,
    pub family: Family,
    /// BAR0 (the graphics memory window): phys, bytes.
    pub bar0: Option<(u64, u64)>,
}

/// The stable order: `seen` sorted in place, the boot display first, then by address. Returns the boot display, if
/// exactly one function's BAR0 holds the whole framebuffer (two would be a firmware layout nobody can explain: then
/// none is the display and the order is by address alone).
pub fn order(seen: &mut [Seen], gop: Option<Gop>) -> Option<Key> {
    let holds = |s: &Seen| match (s.bar0, gop) {
        (Some((base, bytes)), Some(g)) if g.bytes != 0 => g.phys >= base && g.phys.checked_add(g.bytes).is_some_and(|e| base.checked_add(bytes).is_some_and(|b| e <= b)),
        _ => false,
    };
    let mut holders = seen.iter().filter(|s| holds(s));
    let display = match (holders.next(), holders.next()) {
        (Some(one), None) => Some(one.key),
        _ => None,
    };
    seen.sort_by_key(|s| (Some(s.key) != display, s.key));
    display
}

/// One GPU function.
pub struct Device {
    pub key: Key,
    /// Its place in the stable order (0 = the boot display when there is one).
    pub index: usize,
    pub family: Family,
    /// Its BAR0 holds the firmware's framebuffer.
    pub display: bool,
    /// The function as the PCI scan found it; None for a device of a test.
    pub pci: Option<pci::PciFunction>,
    /// The read-only attach of an RX 9070 XT (set once by its probe or by the first tool call).
    pub card: crate::rdna4::CardSlot,
    /// GP-GPU2: the card's bring-up stages of this boot (never touched for another family).
    pub stages: crate::rdna4::stage::SessionSlot,
    /// The render node, the jobs held over a stop, the tool gate.
    pub hw: crate::hw::live::Slots,
    /// The bring-up at boot.
    pub boot: crate::hw::boot::BootSlot,
    /// The 780M's PSP run.
    pub psp: crate::psplive::State,
    /// The 780M's register owner, device object and scanout pair, each made on first use.
    pub(crate) dcn: Once<Option<crate::dcn::DcnRegs>>,
    apu: Once<Option<crate::gpu::Gpu>>,
    pub(crate) scanout: Once<Option<crate::live::KScanout>>,
}

impl Device {
    fn new(key: Key, index: usize, family: Family, display: bool, pci: Option<pci::PciFunction>) -> Device {
        Device {
            key,
            index,
            family,
            display,
            pci,
            card: crate::rdna4::CardSlot::new(),
            stages: crate::rdna4::stage::SessionSlot::new(),
            hw: crate::hw::live::Slots::new(),
            boot: crate::hw::boot::BootSlot::new(),
            psp: crate::psplive::State::new(),
            dcn: Once::new(),
            apu: Once::new(),
            scanout: Once::new(),
        }
    }

    /// Leaked into its place; `wish`: the node number it asks for (`Slots::bind`).
    fn place(self, wish: Option<u32>) -> &'static Device {
        let d: &'static Device = Box::leak(Box::new(self));
        d.hw.bind(wish);
        d
    }

    /// The 780M's register owner (BAR5), attached on first use (thread context: it maps and logs).
    pub fn regs(&'static self) -> Option<&'static crate::dcn::DcnRegs> {
        self.dcn.get_or_init(|| crate::dcn::probe(self)).as_ref()
    }

    /// The 780M's device object, built on first use (thread context).
    pub fn gpu(&'static self) -> Option<&'static crate::gpu::Gpu> {
        self.apu.get_or_init(|| crate::gpu::build(self)).as_ref()
    }

    /// A device without a PCI function behind it (host tests, ktests on fakes): not in the registry.
    pub fn detached(key: Key, index: usize, family: Family, display: bool) -> &'static Device {
        Device::new(key, index, family, display, None).place(None)
    }

    /// `card0`, `card1`: the name below /sys/class/drm and in every tool.
    pub fn card_name(&self) -> String {
        format!("card{}", self.index)
    }

    /// The render minor the device's node gets (`drm::add_device_at`).
    pub fn render_minor(&self) -> u32 {
        drm::device::RENDER_BASE + self.index as u32
    }
}

/// Every device in the stable order.
pub struct Registry {
    devices: Vec<&'static Device>,
    /// The boot's bring-up where no device has one: no AMD function, the scenario's stand-in.
    pub machine: crate::hw::boot::BootSlot,
}

impl Registry {
    /// From the functions found (each with what `Device::pci` gets) and the firmware's framebuffer.
    pub fn build(mut found: Vec<(Seen, Option<pci::PciFunction>)>, gop: Option<Gop>) -> Registry {
        let mut seen: Vec<Seen> = found.iter().map(|(s, _)| *s).collect();
        let display = order(&mut seen, gop);
        // A lone device takes the lowest free render minor, as the 780M always did; several keep their places.
        let several = seen.len() > 1;
        let devices = seen
            .iter()
            .enumerate()
            .map(|(index, s)| {
                let pci = found.iter_mut().find(|(f, _)| f.key == s.key).and_then(|(_, p)| p.take());
                Device::new(s.key, index, s.family, Some(s.key) == display, pci).place(several.then_some(index as u32))
            })
            .collect();
        Registry { devices, machine: crate::hw::boot::BootSlot::new() }
    }

    pub fn all(&self) -> &[&'static Device] {
        &self.devices
    }

    pub fn by_key(&self, key: Key) -> Option<&'static Device> {
        self.devices.iter().copied().find(|d| d.key == key)
    }

    /// The boot display's device.
    pub fn display(&self) -> Option<&'static Device> {
        self.devices.iter().copied().find(|d| d.display)
    }

    /// The first device of a family, in the stable order.
    pub fn first(&self, family: Family) -> Option<&'static Device> {
        self.devices.iter().copied().find(|d| d.family == family)
    }

    /// A tool's `device` argument: `card1`, `1`, `07:00.0` or `0000:07:00.0`; None = the first device.
    pub fn pick(&self, arg: Option<&str>) -> Result<&'static Device, String> {
        let Some(a) = arg.map(str::trim).filter(|a| !a.is_empty()) else {
            return self.devices.first().copied().ok_or_else(|| String::from("no AMD GPU function on this machine"));
        };
        let found = match a.strip_prefix("card").unwrap_or(a).parse::<usize>() {
            Ok(i) => self.devices.get(i).copied(),
            Err(_) => Key::parse(a).and_then(|k| self.by_key(k)),
        };
        found.ok_or_else(|| format!("no GPU device '{a}': {}", self.names()))
    }

    /// `card0 0000:04:00.0, card1 0000:07:00.0`.
    pub fn names(&self) -> String {
        if self.devices.is_empty() {
            return String::from("this machine has none");
        }
        let v: Vec<String> = self.devices.iter().map(|d| format!("{} {}", d.card_name(), d.key)).collect();
        format!("there is {}", v.join(", "))
    }
}

static REGISTRY: Once<Registry> = Once::new();

/// The AMD display functions of the PCI scan. Every RX 9070 XT, and the first other one (the 780M's rule since
/// GX-2: `pci::devices()`'s first AMD display function).
fn scan() -> Vec<(Seen, Option<pci::PciFunction>)> {
    let mut out: Vec<(Seen, Option<pci::PciFunction>)> = Vec::new();
    let nodes = pci::tree();
    for f in pci::devices().into_iter().filter(|f| f.class_code() == 0x03) {
        let Some(family) = family_of(f.vendor, f.device) else { continue };
        if family == Family::Phoenix && out.iter().any(|(s, _)| s.family == Family::Phoenix) {
            klog::event!(Info, "gx", "devs.ignored", function = format!("{}", f.addr).as_str(), device = klog::Hex(u64::from(f.device));
                "a second AMD display function no driver here knows: left alone");
            continue;
        }
        // A card is only ever read: its BAR0 as the scan recorded it (`PciFunction::bar` may size, which writes).
        let bar0 = match if family == Family::Rdna4 { crate::rdna4::live::scan_bar(&nodes, f.loc(), 0) } else { f.bar(0) } {
            Some(b @ pci::Bar::Mem { .. }) => Some((b.start(), b.size())),
            _ => None,
        };
        let key = Key::new(f.segment, f.addr.bus, f.addr.dev, f.addr.func);
        out.push((Seen { key, family, bar0 }, Some(f)));
    }
    out
}

/// The registry, built on first use from the PCI scan (thread context, after the scan: as `DcnRegs::attached` was).
pub fn registry() -> &'static Registry {
    REGISTRY.get_or_init(|| {
        let r = Registry::build(scan(), crate::gpu::gop());
        for d in r.all() {
            klog::event!(Info, "gx", "devs.device", card = d.index, function = format!("{}", d.key).as_str(), family = format!("{:?}", d.family).as_str(), display = d.display;
                "GPU device object");
        }
        r
    })
}

/// The registry only if an earlier call built it: for Quiesce and interrupt context (never scans or logs).
pub fn registry_now() -> Option<&'static Registry> {
    REGISTRY.get()
}

pub fn all() -> &'static [&'static Device] {
    registry().all()
}

/// The device of a PCI function.
pub fn of(f: &pci::PciFunction) -> Option<&'static Device> {
    registry().by_key(Key::new(f.segment, f.addr.bus, f.addr.dev, f.addr.func))
}
