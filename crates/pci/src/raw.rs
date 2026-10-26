//! PC-5: the guards behind the raw MCP tools `pci_read`, `pci_write`, `mmio_read`, `mmio_write` (pure,
//! host-tested; the kernel side with locks, mappings and events is `raw_kernel.rs`, the MCP handlers are in
//! `crates/mcp/src/tools/pc.rs`). Every refusal is decided here before any access.
//!
//! Configuration space (`pci_read`/`pci_write`): reads anywhere in the function's space, except that a
//! function left untouched by its key (R8d) is read only in its 64-byte header (what the scan reads anyway).
//! Writes never reach mass storage (class 01, R8a: only the NVMe driver touches a disk controller, whatever
//! is bound), nor a bridge with mass storage below it (its bus numbers, windows and command register decide
//! whether the disk is reachable), nor an untouched function. Widths 1, 2, 4, naturally aligned. On every
//! other function a write never reaches (`protected`): the BARs, the expansion ROM BAR, a bridge's bus
//! numbers and windows, the SR-IOV and Resizable BAR capabilities (addresses are the PCI core's: a moved BAR
//! would leave `Node::bars` stale, and the memory guards below decide by function from those addresses), nor
//! the MSI and MSI-X capabilities (an interrupt message aimed at RAM is a DMA write anywhere, the same reason
//! the MSI-X table is refused below). Bus master may be switched off but never on (`cmd_write_check`): a raw
//! tool never arms a function's DMA.
//!
//! Memory BARs (`mmio_read`/`mmio_write`): only inside an assigned memory BAR of a function that is neither
//! untouched nor mass storage; widths 1, 2, 4, 8, naturally aligned; at most `MMIO_MAX` bytes per read.
//! Writes never go into the MSI-X table or PBA (PC-3's; an MSI-X entry aimed at RAM is a DMA write anywhere).
//! The Radeon's BARs (vendor 0x1002, class 03): BAR5 is the display engine's register window (DCN 3.1.4
//! below `dcn314_regs::DISPLAY_WINDOW_END`, the SMU mailboxes) and BAR0/BAR2 its memory and doorbells; writes
//! there need no switch (R8c, Georg 21.10.). BAR5's index/data pairs (`PCIE_INDEX2/DATA2`, the NBIO RSMU alias,
//! X8/X10) are refused for reads and writes alike, since an access outside the owner's lock reads or changes
//! whatever register another user selected.
//! Before any memory access (`decode_check`, on values read just before): the function decodes memory, is in
//! D0, and every bridge above forwards memory and has the address in its window. Otherwise the address is not
//! claimed by the function, and an access would reach whatever else decodes it, past every guard here (they
//! are keyed on the function, not on the address).

use crate::access::Loc;
use crate::caps::{ext, id, CapInfo};
use crate::enable::{CMD_MASTER, CMD_MEM};
use crate::node::{ancestors, Bar, Node};
use core::ops::Range;

/// Bytes one `mmio_read` or `pci_read` returns at most (a bound on one request, not a count of things).
pub const MMIO_MAX: u64 = 64 * 1024;
/// The header every function answers in (R8d reads of an untouched function stay inside it).
pub const HEADER: u16 = 0x40;
const AMD: u16 = 0x1002;
/// The Radeon's register BAR (amdgpu: `rmmio_base` = BAR5 from Bonaire on).
pub const AMD_REG_BAR: u8 = 5;

/// BAR5 ranges refused for reads too: index/data pairs that need their owner's lock.
pub fn amd_index_pairs() -> [Range<u64>; 2] {
    let b = dcn314_regs::bar5::PCIE_INDEX2 as u64;
    let r = &dcn314_regs::bar5::NBIO_RSMU_ALIAS;
    [b..(dcn314_regs::bar5::PCIE_DATA2 as u64 + 4), r.start as u64..r.end as u64]
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    NotFound,
    /// R8d: untouched until this key is on.
    Untouched(&'static str),
    /// R8a: class 01.
    Storage,
    /// A bridge with mass storage below it.
    StoragePath,
    BadWidth,
    Unaligned,
    /// Past the function's configuration space or the BAR's end, or longer than one request may be.
    OutOfRange,
    /// No such BAR, or the firmware gave it no address.
    NoBar,
    /// An I/O BAR (port I/O has no MCP tool).
    IoBar,
    /// The function was not sized (in D3hot at the scan): BAR sizes unknown.
    NotSized,
    /// An index/data pair of the Radeon's BAR5.
    IndexPair,
    /// The MSI-X table or PBA.
    MsixTable,
    /// A write narrower than 4 bytes without ECAM (0xCF8 moves dwords only).
    NarrowWrite,
    /// A BAR, the ROM BAR, a bridge's bus numbers or windows, SR-IOV or Resizable BAR: the PCI core's.
    Addresses,
    /// The MSI or MSI-X capability: an interrupt message is a DMA write.
    Interrupts,
    /// Bus master switched on by a raw write.
    BusMaster,
    /// The function does not decode memory, or a bridge above does not forward the address.
    DecodeOff,
    /// The function is not in D0.
    NotD0,
}

impl Refusal {
    /// Short name for events.
    pub fn name(self) -> &'static str {
        match self {
            Refusal::NotFound => "not_found",
            Refusal::Untouched(_) => "untouched",
            Refusal::Storage => "storage",
            Refusal::StoragePath => "storage_path",
            Refusal::BadWidth => "bad_width",
            Refusal::Unaligned => "unaligned",
            Refusal::OutOfRange => "out_of_range",
            Refusal::NoBar => "no_bar",
            Refusal::IoBar => "io_bar",
            Refusal::NotSized => "not_sized",
            Refusal::IndexPair => "index_pair",
            Refusal::MsixTable => "msix_table",
            Refusal::NarrowWrite => "narrow_write",
            Refusal::Addresses => "addresses",
            Refusal::Interrupts => "interrupts",
            Refusal::BusMaster => "bus_master",
            Refusal::DecodeOff => "decode_off",
            Refusal::NotD0 => "not_d0",
        }
    }
    /// Whether the refusal is a safety rule (the tool answers `Refused`) rather than a bad argument.
    pub fn is_rule(self) -> bool {
        matches!(
            self,
            Refusal::Untouched(_) | Refusal::Storage | Refusal::StoragePath | Refusal::IndexPair
                | Refusal::MsixTable
                | Refusal::NarrowWrite
                | Refusal::Addresses
                | Refusal::Interrupts
                | Refusal::BusMaster
                | Refusal::DecodeOff
                | Refusal::NotD0
        )
    }
}

impl core::fmt::Display for Refusal {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Refusal::NotFound => write!(f, "no such function in the scanned tree"),
            Refusal::Untouched(k) => write!(f, "the function is untouched {} (R8d): only its 64-byte header may be read", crate::registry::Until(k)),
            Refusal::Storage => write!(f, "mass storage (class 01) is never written or mapped by a raw tool, only by the NVMe driver (R8a)"),
            Refusal::StoragePath => write!(f, "a bridge with mass storage below it is never written by a raw tool (R8a)"),
            Refusal::BadWidth => write!(f, "width must be 1, 2 or 4 (configuration space) or 1, 2, 4 or 8 (memory)"),
            Refusal::Unaligned => write!(f, "the offset must be a multiple of the width"),
            Refusal::OutOfRange => write!(f, "past the end of the configuration space or the BAR, or more than {MMIO_MAX} bytes"),
            Refusal::NoBar => write!(f, "no such memory BAR, or it has no address"),
            Refusal::IoBar => write!(f, "an I/O BAR: there is no port I/O tool"),
            Refusal::NotSized => write!(f, "the function's BARs were not sized (it was in D3hot at the scan)"),
            Refusal::IndexPair => write!(f, "an index/data register pair of the Radeon's BAR5 (PCIE_INDEX2/DATA2, NBIO RSMU): only its owner, under its lock (X8/X10)"),
            Refusal::MsixTable => write!(f, "the MSI-X table and PBA belong to the PCI core (an entry aimed at RAM would be a DMA write anywhere)"),
            Refusal::NarrowWrite => write!(f, "a write narrower than 4 bytes needs ECAM (0xCF8 moves whole dwords; a read-modify-write would clear status bits)"),
            Refusal::Addresses => write!(f, "BARs, the ROM BAR, bridge bus numbers and windows, SR-IOV and Resizable BAR belong to the PCI core (a moved BAR would leave the raw guards deciding from the old address)"),
            Refusal::Interrupts => write!(f, "the MSI and MSI-X capabilities belong to the PCI core (an interrupt message aimed at RAM would be a DMA write anywhere)"),
            Refusal::BusMaster => write!(f, "a raw write may switch bus master off but never on (that arms the function's DMA; its driver does it through enable_device)"),
            Refusal::DecodeOff => write!(f, "the function does not decode memory, or a bridge above does not forward this address: the access would reach whatever else decodes it"),
            Refusal::NotD0 => write!(f, "the function is not in D0: it does not answer at its BARs"),
        }
    }
}

/// `bb:dd.f`, `ssss:bb:dd.f` (hex, as lspci and hw_info write them).
pub fn parse_loc(s: &str) -> Option<Loc> {
    let s = s.trim();
    let (seg, rest) = match s.matches(':').count() {
        1 => (0u16, s),
        2 => {
            let (a, b) = s.split_once(':')?;
            if a.is_empty() || a.len() > 4 {
                return None;
            }
            (u16::from_str_radix(a, 16).ok()?, b)
        }
        _ => return None,
    };
    let (bus, df) = rest.split_once(':')?;
    let (dev, func) = df.split_once('.')?;
    let hex = |x: &str, max: u32| u32::from_str_radix(x, 16).ok().filter(|&v| v <= max && !x.is_empty() && x.len() <= 4);
    Some(Loc::new(seg, hex(bus, 0xFF)? as u8, hex(dev, 0x1F)? as u8, hex(func, 7)? as u8))
}

fn span(off: u64, width: u8, count: u64, widths: &[u8]) -> Result<u64, Refusal> {
    if !widths.contains(&width) {
        return Err(Refusal::BadWidth);
    }
    if !off.is_multiple_of(width as u64) {
        return Err(Refusal::Unaligned);
    }
    let len = (width as u64).checked_mul(count).filter(|&l| l != 0 && l <= MMIO_MAX).ok_or(Refusal::OutOfRange)?;
    off.checked_add(len).ok_or(Refusal::OutOfRange)
}

/// `pci_read`: `count` values of `width` bytes from `off` in a space of `space` bytes.
pub fn cfg_read_check(n: &Node, space: u16, off: u64, width: u8, count: u64) -> Result<(), Refusal> {
    let end = span(off, width, count, &[1, 2, 4])?;
    let limit = if n.untouched.is_some() { HEADER.min(space) } else { space };
    if end > limit as u64 {
        return Err(match n.untouched {
            Some(k) if end <= space as u64 => Refusal::Untouched(k),
            _ => Refusal::OutOfRange,
        });
    }
    Ok(())
}

/// Mass storage anywhere below `nodes[idx]` (a bridge).
pub fn storage_below(nodes: &[Node], idx: usize) -> bool {
    (0..nodes.len()).any(|i| i != idx && nodes[i].is_storage() && ancestors(nodes, i).contains(&idx))
}

/// `pci_write`: one value of `width` bytes at `off`. `narrow_ok`: the back end writes bytes and words itself
/// (ECAM); 0xCF8 only moves dwords.
pub fn cfg_write_check(nodes: &[Node], idx: usize, space: u16, off: u64, width: u8, narrow_ok: bool) -> Result<(), Refusal> {
    let n = nodes.get(idx).ok_or(Refusal::NotFound)?;
    if let Some(k) = n.untouched {
        return Err(Refusal::Untouched(k));
    }
    if n.is_storage() {
        return Err(Refusal::Storage);
    }
    if n.is_bridge() && storage_below(nodes, idx) {
        return Err(Refusal::StoragePath);
    }
    let end = span(off, width, 1, &[1, 2, 4])?;
    if end > space as u64 {
        return Err(Refusal::OutOfRange);
    }
    let r = off..end;
    if let Some((_, why)) = protected(n).into_iter().find(|(p, _)| overlaps(&r, p)) {
        return Err(why);
    }
    if width < 4 && !narrow_ok {
        return Err(Refusal::NarrowWrite);
    }
    Ok(())
}

/// Configuration ranges of `n` no raw write reaches (see the module text), each with its refusal.
pub fn protected(n: &Node) -> alloc::vec::Vec<(Range<u64>, Refusal)> {
    let a = Refusal::Addresses;
    let mut v = match n.header_type & 0x7F {
        0 => alloc::vec![(0x10..0x28, a), (0x30..0x34, a)],
        // Bridge: BAR0/1, bus numbers, I/O and memory windows with their upper halves, ROM BAR (secondary
        // status 0x1E is write-1-to-clear and stays writable).
        1 => alloc::vec![(0x10..0x1E, a), (0x20..0x34, a), (0x38..0x3C, a)],
        // CardBus: socket base, bus numbers, four windows, legacy base.
        _ => alloc::vec![(0x10..0x48, a)],
    };
    // A capability reaches at most up to the next one (its length is not always known without a read).
    let next_std = |off: u64| n.caps.iter().map(|c| c.offset as u64).filter(|&o| o > off).min().unwrap_or(0x100);
    for c in &n.caps {
        let off = c.offset as u64;
        let len = match (c.id, c.info) {
            (id::MSI, CapInfo::Msi { is64, per_vector_mask, .. }) => match (is64, per_vector_mask) {
                (false, false) => 0x0A,
                (true, false) => 0x0E,
                (false, true) => 0x14,
                (true, true) => 0x18,
            },
            (id::MSI, _) => 0x18,
            (id::MSIX, _) => 0x0C,
            _ => continue,
        };
        v.push((off..(off + len).min(next_std(off)), Refusal::Interrupts));
    }
    let next_ext = |off: u64| n.ext_caps.iter().map(|c| c.offset as u64).filter(|&o| o > off).min().unwrap_or(0x1000);
    for c in &n.ext_caps {
        let off = c.offset as u64;
        // SR-IOV: 0x40 bytes (VF enable, VF BARs); Resizable BAR: header, then up to six capability/control
        // pairs.
        let len = match c.id {
            ext::SRIOV => 0x40,
            ext::REBAR | ext::VF_REBAR => 0x04 + 6 * 8,
            _ => continue,
        };
        v.push((off..(off + len).min(next_ext(off)), a));
    }
    v
}

/// `pci_write` with the configuration dword at 0x04 as it is now (`cmd_dword`): bus master never goes on.
/// `value` is the value as written at `off` (width already applied).
pub fn cmd_write_check(cmd_dword: u32, off: u64, value: u32) -> Result<(), Refusal> {
    let m = CMD_MASTER as u32;
    if off == 4 && value & m != 0 && cmd_dword & m == 0 {
        return Err(Refusal::BusMaster);
    }
    Ok(())
}

/// What the kernel read just before a memory access: the function's command register, its D state (when it
/// has power management) and the command register of each bridge on `ancestors` (same order).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DecodeState {
    pub command: u16,
    pub d_state: Option<u8>,
    pub bridges: alloc::vec::Vec<(usize, u16)>,
}

/// The access at `s` reaches `nodes[idx]`: it decodes memory, is in D0, and every bridge above forwards
/// memory with the address in a memory window (subtractive-decode bridges forward everything). Windows are
/// the scanned ones: no raw write moves them (`protected`), and the kernel does not reassign them.
pub fn decode_check(nodes: &[Node], s: &MmioSpan, st: &DecodeState) -> Result<(), Refusal> {
    if st.command == 0xFFFF {
        return Err(Refusal::NotFound);
    }
    if st.command & CMD_MEM == 0 {
        return Err(Refusal::DecodeOff);
    }
    if st.d_state.is_some_and(|d| d != 0) {
        return Err(Refusal::NotD0);
    }
    let (start, last) = (s.phys(), s.phys() + s.len - 1);
    for &(i, cmd) in &st.bridges {
        let Some(br) = nodes.get(i) else { return Err(Refusal::NotFound) };
        let Some(info) = br.bridge else { continue };
        if cmd == 0xFFFF || cmd & CMD_MEM == 0 {
            return Err(Refusal::DecodeOff);
        }
        if br.class == 0x06_04_01 {
            continue;
        }
        let inside = |w: Option<crate::node::Window>| w.is_some_and(|w| w.start <= start && last <= w.end);
        if !inside(info.mem) && !inside(info.prefetch) {
            return Err(Refusal::DecodeOff);
        }
    }
    Ok(())
}

/// Where an `mmio_read`/`mmio_write` lands: the BAR's physical start and size, and the offset in it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MmioSpan {
    pub bar_phys: u64,
    pub bar_size: u64,
    pub off: u64,
    pub len: u64,
}

impl MmioSpan {
    pub fn phys(&self) -> u64 {
        self.bar_phys + self.off
    }
}

/// The Radeon (vendor 0x1002, display class 03).
pub fn is_amd_display(n: &Node) -> bool {
    n.vendor == AMD && n.class_code() == 0x03
}

fn overlaps(a: &Range<u64>, b: &Range<u64>) -> bool {
    a.start < b.end && b.start < a.end
}

/// The MSI-X table and PBA ranges of `n` inside BAR `bar` (offsets in the BAR).
pub fn msix_ranges(n: &Node, bar: u8) -> alloc::vec::Vec<Range<u64>> {
    let mut v = alloc::vec::Vec::new();
    for c in &n.caps {
        let CapInfo::MsiX { table_size, table_bir, table_offset, pba_bir, pba_offset, .. } = c.info else { continue };
        let n_vec = table_size as u64;
        if table_bir == bar {
            v.push(table_offset as u64..table_offset as u64 + n_vec * 16);
        }
        if pba_bir == bar {
            v.push(pba_offset as u64..pba_offset as u64 + n_vec.div_ceil(64) * 8);
        }
    }
    v
}

/// `mmio_read` (`write` false, `count` values) or `mmio_write` (`write` true, `count` 1) of `width` bytes at
/// `off` in BAR `bar` of `n`.
pub fn mmio_check(n: &Node, bar: u8, off: u64, width: u8, count: u64, write: bool) -> Result<MmioSpan, Refusal> {
    if let Some(k) = n.untouched {
        return Err(Refusal::Untouched(k));
    }
    if n.is_storage() {
        return Err(Refusal::Storage);
    }
    if bar > 5 {
        return Err(Refusal::NoBar);
    }
    let bars = n.bars.ok_or(Refusal::NotSized)?;
    let b = bars[bar as usize].filter(Bar::assigned).ok_or(Refusal::NoBar)?;
    let Bar::Mem { phys, size, .. } = b else { return Err(Refusal::IoBar) };
    let end = span(off, width, count, &[1, 2, 4, 8])?;
    if end > size {
        return Err(Refusal::OutOfRange);
    }
    let r = off..end;
    if is_amd_display(n) && bar == AMD_REG_BAR && amd_index_pairs().iter().any(|p| overlaps(&r, p)) {
        return Err(Refusal::IndexPair);
    }
    if write && msix_ranges(n, bar).iter().any(|m| overlaps(&r, m)) {
        return Err(Refusal::MsixTable);
    }
    Ok(MmioSpan { bar_phys: phys, bar_size: size, off, len: end - off })
}

/// The configuration space a raw read may cover: the function's own (4096 with ECAM for PCIe, else 256).
pub fn cfg_space(n: &Node, backend: u16) -> u16 {
    n.space.min(backend)
}

/// Picks `width` bytes at `off` out of the dword read at `off & !3`.
pub fn extract(dword: u32, off: u64, width: u8) -> u32 {
    let shift = (off & 3) * 8;
    let v = dword >> shift;
    match width {
        1 => v & 0xFF,
        2 => v & 0xFFFF,
        _ => v,
    }
}
