//! A `/sys/bus/pci`-like view of the device tree (PC-4; data only: FS-2 mounts it later). The layout and
//! the file formats follow Linux (`drivers/pci/pci-sysfs.c`, `drivers/pci/pci-driver.c`), so programs and
//! tools written for CachyOS read it unchanged (libpciaccess, lspci's sysfs back end, Mesa's device lookup):
//!
//! - `devices/pci0000:00/` one directory per root bus; below it one directory per function, nested under
//!   its bridges (`devices/pci0000:00/0000:00:02.1/0000:03:00.0/`), with the files `vendor`, `device`,
//!   `subsystem_vendor`, `subsystem_device`, `class`, `revision`, `modalias`, `resource`, `irq`,
//!   `enable`, `config` (binary, read live; not for untouched functions), `driver` (a link, when bound),
//!   the PCIe link files and the bridges' bus numbers;
//! - `bus/pci/devices/<name>`: links to those directories;
//! - `bus/pci/drivers/<driver>/`: one directory per registered driver, a link per bound function.
//!
//! `irq` is the first vector of the function's MSI/MSI-X set (PC-3; 0 without one). Still to come with
//! their items: the writable `remove`/`rescan`/`bind`
//! files (FS-2 wires them to `pci::remove`, `pci::rescan`, `pci::probe`), `numa_node`, `local_cpus`.

use crate::node::{ancestors, Bar, Node};
use crate::Loc;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SysKind {
    Dir,
    /// A text file with this content.
    Text(String),
    /// A symbolic link with this (relative) target.
    Link(String),
    /// The function's configuration space, read live with `read_config` (`size` bytes).
    Config { loc: Loc, size: u16 },
}

/// One entry; `path` is relative to the sysfs root (`bus/pci/devices/0000:00:00.0`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SysEntry {
    pub path: String,
    pub kind: SysKind,
}

/// What the view needs to know about a function beyond its node.
#[derive(Clone, Copy, Debug, Default)]
pub struct FnInfo {
    pub driver: Option<&'static str>,
    pub enabled: bool,
    /// PC-3: the first vector of the function's live MSI/MSI-X set (Linux shows the first MSI's irq; 0 when
    /// the function has none, as Linux for a function without an interrupt).
    pub irq: u32,
}

/// Linux's device name: `0000:03:00.0`.
pub fn name(at: Loc) -> String {
    format!("{:04x}:{:02x}:{:02x}.{}", at.segment, at.bus, at.dev, at.func)
}

/// The function's directory below `devices/`, nested under its bridges.
pub fn dir(nodes: &[Node], i: usize) -> String {
    let chain = ancestors(nodes, i);
    let top = chain.last().copied().unwrap_or(i);
    let root = nodes[top].loc;
    let mut p = format!("devices/pci{:04x}:{:02x}", root.segment, root.bus);
    for &a in chain.iter().rev() {
        p.push('/');
        p.push_str(&name(nodes[a].loc));
    }
    p.push('/');
    p.push_str(&name(nodes[i].loc));
    p
}

/// A link target from the directory `from` to `to` (both relative to the sysfs root).
fn rel(from: &str, to: &str) -> String {
    let up = from.split('/').filter(|c| !c.is_empty()).count();
    let mut s = "../".repeat(up);
    s.push_str(to);
    s
}

/// `pci:v00008086d000010C9sv00008086sd0000A03Cbc02sc00i00` (Linux `modalias_show`).
pub fn modalias(n: &Node) -> String {
    format!(
        "pci:v{:08X}d{:08X}sv{:08X}sd{:08X}bc{:02X}sc{:02X}i{:02X}\n",
        n.vendor,
        n.device,
        n.subsystem.0,
        n.subsystem.1,
        (n.class >> 16) as u8,
        (n.class >> 8) as u8,
        n.class as u8
    )
}

// Linux resource flags (include/linux/ioport.h) plus the BAR's own low bits, as `resource` prints them.
const IORESOURCE_IO: u64 = 0x100;
const IORESOURCE_MEM: u64 = 0x200;
const IORESOURCE_PREFETCH: u64 = 0x2000;
const IORESOURCE_SIZEALIGN: u64 = 0x4_0000;
const IORESOURCE_MEM_64: u64 = 0x10_0000;
const IORESOURCE_WINDOW: u64 = 0x20_0000;

fn res_line(start: u64, size: u64, flags: u64) -> String {
    if size == 0 {
        return format!("0x{:016x} 0x{:016x} 0x{:016x}\n", 0, 0, 0);
    }
    format!("0x{:016x} 0x{:016x} 0x{:016x}\n", start, start.saturating_add(size - 1), flags)
}

/// The `resource` file: 13 lines for a function (BARs 0-5, ROM, six SR-IOV slots), 17 for a bridge (plus
/// its I/O, memory and prefetchable windows at 13-15). Unsized or unassigned slots read as zeros; the ROM
/// is not sized yet (PC-2 open point 4).
pub fn resource(n: &Node) -> String {
    let mut s = String::new();
    let bars = n.bars.unwrap_or([None; 6]);
    for b in bars.iter() {
        s += &match b.filter(Bar::assigned) {
            Some(Bar::Mem { phys, size, prefetch, is64 }) => {
                let mut f = IORESOURCE_MEM | IORESOURCE_SIZEALIGN;
                if prefetch {
                    f |= IORESOURCE_PREFETCH | 0x8;
                }
                if is64 {
                    f |= IORESOURCE_MEM_64 | 0x4;
                }
                res_line(phys, size, f)
            }
            Some(Bar::Io { port, size }) => res_line(port as u64, size as u64, IORESOURCE_IO | IORESOURCE_SIZEALIGN | 0x1),
            None => res_line(0, 0, 0),
        };
    }
    // ROM and the six SR-IOV slots.
    for _ in 6..13 {
        s += &res_line(0, 0, 0);
    }
    if let Some(b) = n.bridge {
        let w = |x: Option<crate::node::Window>, f: u64| x.map_or_else(|| res_line(0, 0, 0), |w| res_line(w.start, w.end - w.start + 1, f));
        s += &w(b.io, IORESOURCE_IO | IORESOURCE_WINDOW);
        s += &w(b.mem, IORESOURCE_MEM | IORESOURCE_WINDOW);
        let pf = IORESOURCE_MEM | IORESOURCE_PREFETCH | IORESOURCE_WINDOW | if b.prefetch.is_some_and(|w| w.end > u32::MAX as u64) { IORESOURCE_MEM_64 } else { 0 };
        s += &w(b.prefetch, pf);
        s += &res_line(0, 0, 0);
    }
    s
}

/// Linux `pci_speed_string` for the link files.
fn link_speed(generation: u8) -> &'static str {
    match generation {
        1 => "2.5 GT/s PCIe",
        2 => "5.0 GT/s PCIe",
        3 => "8.0 GT/s PCIe",
        4 => "16.0 GT/s PCIe",
        5 => "32.0 GT/s PCIe",
        6 => "64.0 GT/s PCIe",
        _ => "Unknown",
    }
}

/// The whole view, sorted by path. `drivers`: every registered driver name (each gets its directory, as
/// in Linux, bound or not).
pub fn view(nodes: &[Node], info: &dyn Fn(Loc) -> FnInfo, drivers: &[&'static str]) -> Vec<SysEntry> {
    let mut out = Vec::new();
    let mut push = |path: String, kind: SysKind| out.push(SysEntry { path, kind });
    for d in ["bus", "bus/pci", "bus/pci/devices", "bus/pci/drivers", "devices"] {
        push(String::from(d), SysKind::Dir);
    }
    let mut names: Vec<&'static str> = drivers.to_vec();
    names.sort_unstable();
    names.dedup();
    for d in &names {
        push(format!("bus/pci/drivers/{d}"), SysKind::Dir);
    }
    let mut roots: Vec<String> = Vec::new();
    for (i, n) in nodes.iter().enumerate() {
        let path = dir(nodes, i);
        let root: String = path.split('/').take(2).collect::<Vec<_>>().join("/");
        if !roots.contains(&root) {
            push(root.clone(), SysKind::Dir);
            roots.push(root);
        }
        let fi = info(n.loc);
        let txt = |s: String| SysKind::Text(s);
        push(path.clone(), SysKind::Dir);
        let file = |f: &str| format!("{path}/{f}");
        push(file("vendor"), txt(format!("0x{:04x}\n", n.vendor)));
        push(file("device"), txt(format!("0x{:04x}\n", n.device)));
        push(file("subsystem_vendor"), txt(format!("0x{:04x}\n", n.subsystem.0)));
        push(file("subsystem_device"), txt(format!("0x{:04x}\n", n.subsystem.1)));
        push(file("class"), txt(format!("0x{:06x}\n", n.class)));
        push(file("revision"), txt(format!("0x{:02x}\n", n.revision)));
        push(file("modalias"), txt(modalias(n)));
        push(file("resource"), txt(resource(n)));
        push(file("irq"), txt(format!("{}\n", fi.irq)));
        push(file("enable"), txt(String::from(if fi.enabled { "1\n" } else { "0\n" })));
        // An untouched function's configuration space is not read beyond the walk (R8d).
        if n.untouched.is_none() {
            push(file("config"), SysKind::Config { loc: n.loc, size: n.space });
        }
        if let Some(p) = n.pcie.filter(|p| p.max_width != 0) {
            push(file("current_link_speed"), txt(format!("{}\n", link_speed(p.link_speed))));
            push(file("current_link_width"), txt(format!("{}\n", p.link_width)));
            push(file("max_link_speed"), txt(format!("{}\n", link_speed(p.max_speed))));
            push(file("max_link_width"), txt(format!("{}\n", p.max_width)));
        }
        if let Some(b) = n.bridge {
            push(file("secondary_bus_number"), txt(format!("{}\n", b.secondary)));
            push(file("subordinate_bus_number"), txt(format!("{}\n", b.subordinate)));
        }
        let nm = name(n.loc);
        push(format!("bus/pci/devices/{nm}"), SysKind::Link(rel("bus/pci/devices", &path)));
        if let Some(d) = fi.driver {
            let ddir = format!("bus/pci/drivers/{d}");
            push(file("driver"), SysKind::Link(rel(&path, &ddir)));
            if !names.contains(&d) {
                push(ddir.clone(), SysKind::Dir);
            }
            push(format!("{ddir}/{nm}"), SysKind::Link(rel(&ddir, &path)));
        }
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out.dedup_by(|b, a| a.path == b.path);
    out
}

/// The live view of the current tree, with the bound drivers and every registered driver's directory.
pub fn entries() -> Vec<SysEntry> {
    let nodes = crate::tree();
    let bound = crate::bindings();
    let info = |at: Loc| FnInfo {
        driver: bound.iter().find(|b| b.function == at && b.state == crate::BindState::Bound).map(|b| b.driver),
        enabled: crate::device::is_enabled(at),
        irq: crate::irq::first_vector(at).map_or(0, |v| v.vector as u32),
    };
    let drivers: Vec<&'static str> = crate::DRIVERS.entries().iter().map(|d| d.name).collect();
    view(&nodes, &info, &drivers)
}

/// Reads configuration space for the `config` file: up to `buf.len()` bytes from `off`, stopping at the
/// function's space. 0 for an untouched function (R8d) or one not in the tree.
pub fn read_config(at: Loc, off: usize, buf: &mut [u8]) -> usize {
    let Some(f) = crate::function(at) else { return 0 };
    if f.untouched().is_some() {
        return 0;
    }
    let size = f.config_size() as usize;
    let end = size.min(off.saturating_add(buf.len()));
    let mut n = 0;
    let mut pos = off;
    while pos < end {
        let Some(d) = f.read32_ext((pos & !3) as u16) else { break };
        let bytes = d.to_le_bytes();
        let take = (4 - (pos & 3)).min(end - pos);
        buf[n..n + take].copy_from_slice(&bytes[pos & 3..(pos & 3) + take]);
        n += take;
        pos += take;
    }
    n
}
