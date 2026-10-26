//! PC-5 on the machine: the raw configuration and memory accesses behind the MCP tools, each after `raw`'s
//! guards (decided before any access) and each write logged with the value before and after (B6). Memory
//! accesses also check what the function and the bridges above decode right before the access
//! (`raw::decode_check`).
//!
//! Memory BARs are reached through uncached windows of at most `CHUNK` bytes, mapped on first use and kept
//! (`pci.raw`): with the other CPUs online an unmapped window would sit in MM's quarantine until MM-6, so a
//! window per call would leak address space and a Warn per call. A BAR never moves (the kernel does not
//! reassign BARs), so a kept window stays right.

use crate::raw::{self, MmioSpan, Refusal};
use crate::{access, Loc, CFG_LOCK, NODES};
use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use hal::mmio::{IoMem, MmioError};
use ksync::Mutex;

/// Bytes per kept window (a BAR of the GPU is 256 MiB; only the parts used get mapped).
const CHUNK: u64 = 1 << 20;

/// Kept windows by (physical start, length).
static WINDOWS: Mutex<BTreeMap<(u64, u64), Arc<IoMem>>> = Mutex::named("pci.raw", BTreeMap::new());

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RawError {
    Refused(Refusal),
    /// The BAR could not be mapped (it overlaps RAM, no address space, before paging is up).
    Map(MmioError),
}

impl core::fmt::Display for RawError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            RawError::Refused(r) => write!(f, "{r}"),
            RawError::Map(e) => write!(f, "the BAR could not be mapped: {e:?}"),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CfgRead {
    pub at: Loc,
    pub off: u64,
    pub width: u8,
    pub values: Vec<u32>,
    /// The function's configuration space as reachable (4096 or 256).
    pub space: u16,
    pub untouched: Option<&'static str>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CfgWrite {
    pub at: Loc,
    pub off: u64,
    pub width: u8,
    pub before: u32,
    pub value: u32,
    pub after: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MmioRead {
    pub at: Loc,
    pub bar: u8,
    pub span: MmioSpan,
    pub width: u8,
    pub values: Vec<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MmioWrite {
    pub at: Loc,
    pub bar: u8,
    pub span: MmioSpan,
    pub width: u8,
    pub value: u64,
    /// Read back after the write when asked (a read can have side effects: off by default).
    pub after: Option<u64>,
}

fn refused(tool: &str, at: Loc, r: Refusal, detail: String) -> RawError {
    klog::event!(Warn, "pci", "raw.refused", tool = tool, function = format!("{at}"), reason = r.name(), detail = detail.as_str());
    RawError::Refused(r)
}

fn node_index(nodes: &[crate::Node], at: Loc) -> Option<usize> {
    nodes.iter().position(|n| n.loc == at)
}

/// `pci_read`: `count` values of `width` bytes from `off`.
pub fn config_read(at: Loc, off: u64, width: u8, count: u64) -> Result<CfgRead, RawError> {
    let nodes = NODES.lock().clone();
    let Some(i) = node_index(&nodes, at) else { return Err(refused("pci_read", at, Refusal::NotFound, String::new())) };
    let n = &nodes[i];
    let space = raw::cfg_space(n, access().space_size(at.segment, at.bus));
    raw::cfg_read_check(n, space, off, width, count).map_err(|r| refused("pci_read", at, r, format!("offset={off:#x} width={width} count={count}")))?;
    let mut values = Vec::new();
    let mut cached: Option<(u64, u32)> = None;
    for k in 0..count {
        let o = off + k * width as u64;
        let dw = o & !3;
        let d = match cached {
            Some((at_dw, d)) if at_dw == dw => d,
            _ => {
                let d = {
                    let _g = CFG_LOCK.lock();
                    access().read32(at, dw as u16)
                };
                cached = Some((dw, d));
                d
            }
        };
        values.push(raw::extract(d, o, width));
    }
    klog::event!(Debug, "pci", "raw.cfg_read", function = format!("{at}"), offset = klog::Hex(off), width = width, count = count);
    Ok(CfgRead { at, off, width, values, space, untouched: n.untouched })
}

/// `pci_write`: one value of `width` bytes at `off`, read back afterwards.
pub fn config_write(at: Loc, off: u64, width: u8, value: u32) -> Result<CfgWrite, RawError> {
    let nodes = NODES.lock().clone();
    let Some(i) = node_index(&nodes, at) else { return Err(refused("pci_write", at, Refusal::NotFound, String::new())) };
    let c = access();
    let space = raw::cfg_space(&nodes[i], c.space_size(at.segment, at.bus));
    raw::cfg_write_check(&nodes, i, space, off, width, c.narrow_writes())
        .map_err(|r| refused("pci_write", at, r, format!("offset={off:#x} width={width} value={value:#x}")))?;
    let dw = (off & !3) as u16;
    let value = raw::extract(value, 0, width);
    let (before, now) = {
        let _g = CFG_LOCK.lock();
        let before = c.read32(at, dw);
        if let Err(r) = raw::cmd_write_check(before, off, value) {
            drop(_g);
            return Err(refused("pci_write", at, r, format!("offset={off:#x} width={width} value={value:#x} command={:#x}", before & 0xFFFF)));
        }
        if width == 4 {
            c.write32(at, off as u16, value);
        } else {
            c.write_narrow(at, off as u16, width, value);
        }
        (before, c.read32(at, dw))
    };
    let (before, after) = (raw::extract(before, off, width), raw::extract(now, off, width));
    // The command register without bus master: the function no longer counts as enabled for DMA.
    if dw == 4 && now & crate::enable::CMD_MASTER as u32 == 0 {
        crate::device::note_master_off(at);
    }
    let driver = crate::device::driver_of_loc(at).unwrap_or("-");
    klog::event!(Info, "pci", "raw.cfg_write", function = format!("{at}"), driver = driver, offset = klog::Hex(off), width = width,
        before = klog::Hex(before as u64), value = klog::Hex(value as u64), after = klog::Hex(after as u64); "MCP pci_write");
    Ok(CfgWrite { at, off, width, before, value, after })
}

/// The kept window holding `span.off` and the offset of it inside the window. Accesses are at most 8 bytes
/// and naturally aligned, so none crosses a window (CHUNK is a multiple of 8).
fn window(span: &MmioSpan, off: u64) -> Result<(Arc<IoMem>, usize), MmioError> {
    let chunk = off / CHUNK * CHUNK;
    let start = span.bar_phys + chunk;
    let len = CHUNK.min(span.bar_size - chunk);
    let mut w = WINDOWS.lock();
    if let Some(m) = w.get(&(start, len)) {
        return Ok((m.clone(), (off - chunk) as usize));
    }
    let m = Arc::new(hal::mmio::map_device(start, len as usize, hal::paging::CacheType::Uncached)?);
    w.insert((start, len), m.clone());
    drop(w);
    klog::event!(Debug, "pci", "raw.window", phys = klog::Hex(start), len = klog::Hex(len));
    Ok((m, (off - chunk) as usize))
}

/// What the function and the bridges above decode now (`raw::decode_check`).
fn decode_state(nodes: &[crate::Node], i: usize) -> raw::DecodeState {
    let c = access();
    let path = crate::node::ancestors(nodes, i);
    let pm = nodes[i].cap(crate::caps::id::PM).map(|p| p.offset as u16 + 4);
    let _g = CFG_LOCK.lock();
    let cmd = |at: Loc| c.read32(at, 4) as u16;
    raw::DecodeState {
        command: cmd(nodes[i].loc),
        d_state: pm.map(|o| (c.read32(nodes[i].loc, o) & 3) as u8),
        bridges: path.into_iter().map(|p| (p, cmd(nodes[p].loc))).collect(),
    }
}

#[allow(clippy::too_many_arguments)]
fn mmio_span(tool: &str, at: Loc, bar: u8, off: u64, width: u8, count: u64, write: bool) -> Result<MmioSpan, RawError> {
    let nodes = NODES.lock().clone();
    let Some(i) = node_index(&nodes, at) else { return Err(refused(tool, at, Refusal::NotFound, String::new())) };
    let detail = || format!("bar={bar} offset={off:#x} width={width} count={count}");
    let span = raw::mmio_check(&nodes[i], bar, off, width, count, write).map_err(|r| refused(tool, at, r, detail()))?;
    let st = decode_state(&nodes, i);
    raw::decode_check(&nodes, &span, &st).map_err(|r| {
        let bridges: Vec<String> = st.bridges.iter().map(|&(b, cmd)| format!("{}:{cmd:#x}", nodes[b].loc)).collect();
        refused(tool, at, r, format!("{} command={:#x} d_state={:?} bridges={}", detail(), st.command, st.d_state, bridges.join(",")))
    })?;
    Ok(span)
}

/// `mmio_read`: `count` values of `width` bytes from `off` in BAR `bar`.
pub fn mmio_read(at: Loc, bar: u8, off: u64, width: u8, count: u64) -> Result<MmioRead, RawError> {
    let span = mmio_span("mmio_read", at, bar, off, width, count, false)?;
    let mut values = Vec::new();
    for k in 0..count {
        let o = off + k * width as u64;
        let (w, i) = window(&span, o).map_err(RawError::Map)?;
        let v = match width {
            1 => w.read8(i).map(u64::from),
            2 => w.read16(i).map(u64::from),
            4 => w.read32(i).map(u64::from),
            _ => w.read64(i),
        };
        values.push(v.map_err(RawError::Map)?);
    }
    klog::event!(Debug, "pci", "raw.mmio_read", function = format!("{at}"), bar = bar, offset = klog::Hex(off), width = width, count = count);
    Ok(MmioRead { at, bar, span, width, values })
}

/// `mmio_write`: one value of `width` bytes at `off` in BAR `bar`; `read_back`: read the register again
/// afterwards.
pub fn mmio_write(at: Loc, bar: u8, off: u64, width: u8, value: u64, read_back: bool) -> Result<MmioWrite, RawError> {
    let span = mmio_span("mmio_write", at, bar, off, width, 1, true)?;
    let (w, i) = window(&span, off).map_err(RawError::Map)?;
    let r = match width {
        1 => w.write8(i, value as u8),
        2 => w.write16(i, value as u16),
        4 => w.write32(i, value as u32),
        _ => w.write64(i, value),
    };
    r.map_err(RawError::Map)?;
    let after = if read_back {
        Some(match width {
            1 => w.read8(i).map(u64::from),
            2 => w.read16(i).map(u64::from),
            4 => w.read32(i).map(u64::from),
            _ => w.read64(i),
        }
        .map_err(RawError::Map)?)
    } else {
        None
    };
    klog::event!(Info, "pci", "raw.mmio_write", function = format!("{at}"), bar = bar, offset = klog::Hex(off), width = width,
        value = klog::Hex(value), phys = klog::Hex(span.phys()), after = klog::Hex(after.unwrap_or(0)), read_back = read_back; "MCP mmio_write");
    Ok(MmioWrite { at, bar, span, width, value, after })
}
