//! What the scan found about one function: the device tree's node (exposed for `hw_info` and
//! `/sys/bus/pci` through `tree()`), and the problems it met on the way (each becomes a Warn event).

use crate::access::Loc;
use crate::caps::{Cap, ExtCap};
use alloc::vec::Vec;

/// One BAR, as the contract has it since G0.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Bar {
    Mem { phys: u64, size: u64, prefetch: bool, is64: bool },
    Io { port: u16, size: u32 },
}

impl Bar {
    pub fn start(&self) -> u64 {
        match *self {
            Bar::Mem { phys, .. } => phys,
            Bar::Io { port, .. } => port as u64,
        }
    }
    pub fn size(&self) -> u64 {
        match *self {
            Bar::Mem { size, .. } => size,
            Bar::Io { size, .. } => size as u64,
        }
    }
    /// The firmware gave it an address (0 = not assigned; such a BAR decodes nothing useful).
    pub fn assigned(&self) -> bool {
        self.start() != 0
    }
}

/// The six BAR slots of a type-0 header (two of a bridge). The upper half of a 64-bit BAR stays `None`.
pub type Bars = [Option<Bar>; 6];

/// An address window a bridge forwards (inclusive end). `None` in `BridgeInfo` when the window is closed
/// (base above limit).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Window {
    pub start: u64,
    pub end: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BridgeInfo {
    pub primary: u8,
    pub secondary: u8,
    pub subordinate: u8,
    pub io: Option<Window>,
    pub mem: Option<Window>,
    pub prefetch: Option<Window>,
    pub control: u16,
}

/// The PCI Express capability's facts (capability 0x10).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PcieInfo {
    pub version: u8,
    /// Device/port type (0 endpoint, 1 legacy endpoint, 4 root port, 5 upstream, 6 downstream, 7 PCIe-to-PCI
    /// bridge, 8 PCI-to-PCIe bridge, 9 RC integrated endpoint, 10 RC event collector).
    pub port_type: u8,
    /// Link speed (1 = 2.5 GT/s ... 6 = 64 GT/s) and width; 0 when there is no link register.
    pub link_speed: u8,
    pub link_width: u8,
    pub max_speed: u8,
    pub max_width: u8,
    /// Device status (UR detected, transactions pending: the Mini-PC's bus-master trap diagnosis).
    pub dev_status: u16,
}

impl PcieInfo {
    pub fn is_root_port(&self) -> bool {
        self.port_type == 4
    }
    pub fn is_downstream_port(&self) -> bool {
        self.port_type == 6
    }
    pub fn port_name(&self) -> &'static str {
        match self.port_type {
            0 => "endpoint",
            1 => "legacy endpoint",
            4 => "root port",
            5 => "upstream port",
            6 => "downstream port",
            7 => "pcie-to-pci bridge",
            8 => "pci-to-pcie bridge",
            9 => "integrated endpoint",
            10 => "event collector",
            _ => "unknown port type",
        }
    }
}

/// Something the scan did not accept or had to work around. Firmware and devices are hostile input: none
/// of these stops the scan, each is logged once with the function it belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Problem {
    /// A capability pointer seen twice (a loop in the list).
    CapLoop { at: u16 },
    /// A pointer into the header (below 0x40), past the space, or not dword-aligned in the extended space.
    CapBadPointer { at: u16, ptr: u16 },
    /// A capability whose length runs past its space or is smaller than its header.
    CapBadLength { id: u16, at: u16, len: u16 },
    /// MSI-X table or PBA in a BIR that is not a memory BAR, or past that BAR's end.
    MsixOutside { bir: u8, offset: u32 },
    /// BAR memory type 3 (reserved).
    BarReservedType { index: u8 },
    /// A 64-bit BAR in the last slot (no upper half).
    Bar64InLastSlot { index: u8 },
    /// An I/O BAR above 0xFFFF (x86 has 64 Ki ports).
    BarIoTooHigh { index: u8, addr: u64 },
    /// A bridge the firmware did not number (secondary 0 or subordinate below secondary). No renumbering.
    BridgeUnconfigured { secondary: u8, subordinate: u8 },
    /// A bridge whose primary bus is not the bus it sits on (logged, its secondary is still followed).
    BridgePrimary { primary: u8 },
    /// Secondary bus not below the bridge, outside the parent's range, or already scanned: not followed.
    BridgeBadBus { secondary: u8, subordinate: u8 },
    /// The secondary bus is outside every ECAM window.
    BusUnreachable { bus: u8 },
    /// A memory BAR past the CPU's physical address bits or wrapping around: the slot is left empty.
    BarBeyondPhys { index: u8, phys: u64, size: u64 },
    /// The function is in a low power state (3 = D3hot): its BARs were not sized (PC-2 wakes it first).
    BarPowerState { state: u8 },
    /// The function's ARI chain points at a function number of 8 or more and the port above forwards ARI:
    /// those functions are not enumerated.
    AriNotEnumerated,
}

impl Problem {
    pub fn name(&self) -> &'static str {
        match self {
            Problem::CapLoop { .. } => "cap.loop",
            Problem::CapBadPointer { .. } => "cap.bad_pointer",
            Problem::CapBadLength { .. } => "cap.bad_length",
            Problem::MsixOutside { .. } => "msix.outside_bar",
            Problem::BarReservedType { .. } => "bar.reserved_type",
            Problem::Bar64InLastSlot { .. } => "bar.64_in_last_slot",
            Problem::BarIoTooHigh { .. } => "bar.io_too_high",
            Problem::BridgeUnconfigured { .. } => "bridge.unconfigured",
            Problem::BridgePrimary { .. } => "bridge.primary_mismatch",
            Problem::BridgeBadBus { .. } => "bridge.bad_bus",
            Problem::BusUnreachable { .. } => "bus.unreachable",
            Problem::BarBeyondPhys { .. } => "bar.beyond_phys",
            Problem::BarPowerState { .. } => "bar.power_state",
            Problem::AriNotEnumerated => "ari.not_enumerated",
        }
    }
}

/// One function in the device tree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Node {
    pub loc: Loc,
    pub vendor: u16,
    pub device: u16,
    /// class << 16 | subclass << 8 | prog-if
    pub class: u32,
    pub revision: u8,
    pub header_type: u8,
    /// Subsystem vendor/device (type 0 header; 0 otherwise).
    pub subsystem: (u16, u16),
    /// Command register as the scan found it (before any sizing; the opted-out check compares with it).
    pub command_at_scan: u16,
    pub status_at_scan: u16,
    pub interrupt_pin: u8,
    /// Index (in the scan's node list) of the bridge whose secondary bus this function sits on.
    pub parent: Option<usize>,
    /// Bytes of configuration space reachable (4096 ECAM, 256 legacy).
    pub space: u16,
    pub bridge: Option<BridgeInfo>,
    pub caps: Vec<Cap>,
    pub ext_caps: Vec<ExtCap>,
    pub pcie: Option<PcieInfo>,
    /// `None`: not sized (function left untouched by an opt-in key, or not a type 0/1 header).
    pub bars: Option<Bars>,
    /// The CONFIG key that keeps this function untouched (R8d) until it is `on`.
    pub untouched: Option<&'static str>,
    pub problems: Vec<Problem>,
}

impl Node {
    pub fn class_code(&self) -> u8 {
        (self.class >> 16) as u8
    }
    pub fn is_bridge(&self) -> bool {
        self.bridge.is_some()
    }
    pub fn multifunction(&self) -> bool {
        self.header_type & 0x80 != 0
    }
    pub fn cap(&self, id: u8) -> Option<&Cap> {
        self.caps.iter().find(|c| c.id == id)
    }
    pub fn ext_cap(&self, id: u16) -> Option<&ExtCap> {
        self.ext_caps.iter().find(|c| c.id == id)
    }
    /// Mass storage (class 01): only the NVMe driver binds, R8a.
    pub fn is_storage(&self) -> bool {
        self.class_code() == 0x01
    }
}

/// Indices of the nodes on the path from `i` up to its root bus (nearest bridge first).
pub fn ancestors(nodes: &[Node], i: usize) -> Vec<usize> {
    let mut out = Vec::new();
    let mut last = i;
    let mut cur = nodes.get(i).and_then(|n| n.parent);
    // Parents always have a smaller index (the walk pushes a bridge before what sits below it), so this ends.
    while let Some(p) = cur {
        if p >= last {
            break;
        }
        out.push(p);
        last = p;
        cur = nodes[p].parent;
    }
    out
}
