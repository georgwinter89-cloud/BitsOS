//! The device tree as text for `hw_info` (MC registers the tool; `pci::tree_lines()` is its PCI section):
//! one line per function, indented below its bridge, with its driver (PC-4), then one line per BAR, the
//! capability list and one line per problem the scan met.

use crate::caps::{ext_name, std_name, CapInfo, ExtInfo};
use crate::node::{Bar, Node};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

/// A short class name (the classes BitsOS meets; V1 `hw/pciname.asm`).
pub fn class_name(class: u32) -> &'static str {
    match class >> 8 {
        0x0100 => "scsi",
        0x0101 => "ide",
        0x0106 => "sata",
        0x0107 => "sas",
        0x0108 => "nvme",
        0x0180 => "storage",
        0x0200 => "ethernet",
        0x0280 => "network (wlan)",
        0x0300 => "vga",
        0x0380 => "display",
        0x0401 => "audio",
        0x0403 => "hd audio",
        0x0480 => "multimedia",
        0x0500 => "ram",
        0x0600 => "host bridge",
        0x0601 => "isa bridge",
        0x0604 => "pci bridge",
        0x0680 => "bridge",
        0x0700 => "serial",
        0x0805 => "sd host",
        0x0806 => "iommu",
        0x0880 => "system",
        0x0C03 => match class & 0xFF {
            0x30 => "xhci",
            0x40 => "usb4",
            _ => "usb",
        },
        0x0C05 => "smbus",
        0x0D80 => "wireless",
        0x1080 => "crypto",
        0x1101 => "signal processing",
        0x1200 => "accelerator",
        0x1300 => "non-essential",
        _ => match class >> 16 {
            0x00 => "unclassified",
            0x01 => "storage",
            0x02 => "network",
            0x03 => "display",
            0x04 => "multimedia",
            0x06 => "bridge",
            0x08 => "system",
            0x0C => "serial bus",
            0x0D => "wireless",
            0x11 => "signal processing",
            0x12 => "accelerator",
            _ => "other",
        },
    }
}

fn human(size: u64) -> String {
    match size {
        s if s >= 1 << 30 && s % (1 << 30) == 0 => format!("{}G", s >> 30),
        s if s >= 1 << 20 && s % (1 << 20) == 0 => format!("{}M", s >> 20),
        s if s >= 1 << 10 && s % (1 << 10) == 0 => format!("{}K", s >> 10),
        s => format!("{s}"),
    }
}

fn speed(generation: u8) -> &'static str {
    match generation {
        1 => "2.5GT/s",
        2 => "5GT/s",
        3 => "8GT/s",
        4 => "16GT/s",
        5 => "32GT/s",
        6 => "64GT/s",
        _ => "?",
    }
}

/// The heading and one line per function, BAR and capability list, indented by bridge depth.
pub fn lines(nodes: &[Node], method: &str) -> Vec<String> {
    lines_with(nodes, method, &|_| None)
}

/// `lines` with a note per function (PC-4: the bound driver, "driver igc").
pub fn lines_with(nodes: &[Node], method: &str, note: &dyn Fn(crate::Loc) -> Option<String>) -> Vec<String> {
    let mut out = Vec::new();
    let bridges = nodes.iter().filter(|n| n.is_bridge()).count();
    out.push(format!("pci: {} functions, {} bridges, config access {}", nodes.len(), bridges, method));
    for (i, n) in nodes.iter().enumerate() {
        let depth = crate::node::ancestors(nodes, i).len();
        let ind = "  ".repeat(depth);
        let mut l = format!("{ind}{} [{:04x}:{:04x}] {:06x} {}", n.loc, n.vendor, n.device, n.class, class_name(n.class));
        if let Some(b) = n.bridge {
            l += &format!(" -> bus {:02x}-{:02x}", b.secondary, b.subordinate);
        }
        if let Some(p) = n.pcie {
            l += &format!(", {}", p.port_name());
            if p.link_width != 0 {
                l += &format!(" link {} x{}", speed(p.link_speed), p.link_width);
            }
        }
        if let Some(k) = n.untouched {
            l += &format!(", untouched {}", crate::registry::Until(k));
        }
        if let Some(d) = note(n.loc) {
            l += &format!(", {d}");
        }
        out.push(l);
        for (bi, b) in n.bars.iter().flat_map(|bars| bars.iter().enumerate()) {
            let Some(b) = *b else { continue };
            out.push(match b {
                Bar::Mem { phys, size, prefetch, is64 } => format!(
                    "{ind}    bar{bi} mem {phys:#x} {}{}{}{}",
                    human(size),
                    if is64 { " 64bit" } else { "" },
                    if prefetch { " prefetch" } else { "" },
                    if phys == 0 { " unassigned" } else { "" }
                ),
                Bar::Io { port, size } => format!("{ind}    bar{bi} io {port:#x} {size}{}", if port == 0 { " unassigned" } else { "" }),
            });
        }
        if let Some(b) = n.bridge {
            let w = |name: &str, x: Option<crate::node::Window>| x.map(|x| format!(" {name} {:#x}-{:#x}", x.start, x.end)).unwrap_or_default();
            let all = w("mem", b.mem) + &w("prefetch", b.prefetch) + &w("io", b.io);
            if !all.is_empty() {
                out.push(format!("{ind}    window{all}"));
            }
        }
        let mut caps: Vec<String> = n
            .caps
            .iter()
            .map(|c| match c.info {
                CapInfo::Pm { state, .. } => format!("pm(D{state})"),
                CapInfo::Msi { enabled, vectors, .. } => format!("msi({vectors}{})", if enabled { ",on" } else { "" }),
                CapInfo::MsiX { enabled, table_size, .. } => format!("msix({table_size}{})", if enabled { ",on" } else { "" }),
                _ if std_name(c.id) == "cap" => format!("cap{:#04x}", c.id),
                _ => String::from(std_name(c.id)),
            })
            .collect();
        caps.extend(n.ext_caps.iter().map(|e| match e.info {
            // The offset: `pci_read` of offset + 8 shows Control 1 now (PC-5 check list).
            ExtInfo::L1ss { ctrl1, .. } => format!("l1ss@{:#x}({:#x})", e.offset, ctrl1 & 0xF),
            ExtInfo::Ptm { ctrl, .. } => format!("ptm({})", if ctrl & 1 != 0 { "on" } else { "off" }),
            ExtInfo::Vsec { vsec_id, .. } => format!("vsec({vsec_id:#x})"),
            _ if ext_name(e.id) == "ext" => format!("ext{:#06x}", e.id),
            _ => String::from(ext_name(e.id)),
        }));
        if !caps.is_empty() {
            out.push(format!("{ind}    caps {}", caps.join(" ")));
        }
        // Whatever the scan did not accept (a dropped bridge subtree, a refused BAR) stays visible.
        for p in &n.problems {
            out.push(format!("{ind}    problem {} {p:?}", p.name()));
        }
    }
    out
}
