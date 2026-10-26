//! The display attach: V1's gates 1-8 (spec/gpu.md §1.2, `P/gpu/dcnfind.asm`, `dcnattach.asm`, `dcndmcub.asm`,
//! `dcnpipes.asm`) as pure functions over a register reader, so host tests feed them register images (the G-1
//! snapshot, saved-5.json, broken copies). **Read-only**: nothing here writes a register or PCI configuration.
//!
//! V1's gate 8 (`dcn_settle`) flipped a pipe left on A1 back to A0, a write. In V2 the attach only *reports*
//! it (`Settle`); GX-3's flip path settles before its first flip (decision in plan/notes/gx-GX-2.md).

use crate::layout as l;
use alloc::string::String;
use alloc::vec::Vec;

pub use crate::refusal::{Refusal, Why};

/// A1's distance from A0 in graphics memory (V1 `DCN_BACK`, dcn.asm:16): 32 MB, so A0 of any mode up to 32 MB
/// never overlaps A1.
pub const BACK_OFFSET: u64 = 32 << 20;
/// Gate 1: the GOP framebuffer lies at most 2^34 bytes above BAR0 (V1 `DCN_FAR`).
pub const FAR_BITS: u32 = 34;
/// Gate 4: graphics memory above 64 GB is not believable (V1 `dcn_sizes`).
pub const MEMSIZE_MAX_MB: u32 = 0x10000;
/// V1 maps A1 in 2 MB pages; the bus must pass the whole last page.
const PAGE_2M: u64 = 2 << 20;

/// What a register read gives: `None` if the offset is refused (outside BAR5, a seg0 alias, an index/data pair).
pub trait Regs {
    fn read(&self, byte: u32) -> Option<u32>;
}

impl<F: Fn(u32) -> Option<u32>> Regs for F {
    fn read(&self, byte: u32) -> Option<u32> {
        self(byte)
    }
}

/// The GOP framebuffer: its physical address and the bytes of the mode (stride × height × 4).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Gop {
    pub phys: u64,
    pub bytes: u64,
}

/// A bridge's two memory windows (inclusive ends, as `pci::Window`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BridgeWindows {
    pub mem: Option<(u64, u64)>,
    pub prefetch: Option<(u64, u64)>,
}

/// The PCI facts gates 1-2 decide on (read from configuration space by the caller; nothing is written).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PciFacts {
    pub function: String,
    pub bus: u8,
    pub vendor: u16,
    pub class_code: u8,
    pub command: u16,
    /// BAR0 base, if it is a memory BAR.
    pub bar0: Option<u64>,
    /// BAR5 (phys, size), if it is a 32-bit memory BAR.
    pub bar5: Option<(u64, u64)>,
    /// The bridges from the function up to bus 0, nearest first; `None` if a bus on the way has no bridge.
    pub bridges: Option<Vec<BridgeWindows>>,
}

/// What gates 1-2 found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PciOk {
    pub bar0: u64,
    pub bar5: (u64, u64),
    /// Bytes from BAR0 on that every bridge passes (`u64::MAX` on bus 0: the root complex decodes it all).
    pub reach: u64,
}

/// Gates 1-2 (V1 `dcn_scan`, `dcn_init`, `dcn_reach`, `dcn_window`).
pub fn pci_gates(f: &PciFacts, gop: Option<Gop>) -> Result<PciOk, Refusal> {
    let gop = gop.filter(|g| g.bytes != 0).ok_or(Refusal::new(Why::NotGop))?;
    if f.vendor != 0x1002 || f.class_code != 0x03 {
        return Err(Refusal::new(Why::NotGop));
    }
    let bar0 = f.bar0.ok_or(Refusal::new(Why::NotGop))?;
    if gop.phys < bar0 || (gop.phys - bar0) >> FAR_BITS != 0 {
        return Err(Refusal::new(Why::NotGop));
    }
    if f.command & 0x2 == 0 {
        return Err(Refusal::new(Why::Decode));
    }
    let bar5 = match f.bar5 {
        Some((phys, size)) if phys != 0 && size != 0 => (phys, size),
        _ => return Err(Refusal::new(Why::Bar5)),
    };
    let reach = if f.bus == 0 {
        u64::MAX
    } else {
        let bridges = f.bridges.as_ref().filter(|b| !b.is_empty()).ok_or(Refusal::new(Why::Bridge))?;
        bridges.iter().map(|b| window_reach(b, bar0)).min().unwrap_or(0)
    };
    Ok(PciOk { bar0, bar5, reach })
}

/// The bytes from `bar0` on that one bridge passes: the larger of its windows that hold `bar0` (V1 `dcn_window`).
pub fn window_reach(b: &BridgeWindows, bar0: u64) -> u64 {
    [b.mem, b.prefetch]
        .into_iter()
        .flatten()
        .filter(|&(s, e)| s <= e && s <= bar0 && bar0 <= e)
        .map(|(_, e)| (e - bar0).saturating_add(1))
        .max()
        .unwrap_or(0)
}

/// A pipe that shows A0 or A1 with its OTG running (V1 pipe table).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Pipe {
    pub hubp: u8,
    pub otg: u8,
    /// SURFACE_FLIP_TYPE as found (1 = immediate); GX-3 puts it back.
    pub flip_type: u8,
    /// The surface address the pipe scans (A0 or A1).
    pub address: u64,
}

/// A DMCUB memory window that is on (GPU addresses, inclusive end).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Window {
    pub name: &'static str,
    pub first: u64,
    pub last: u64,
}

/// What V1's gate 8 would have done (V2 reports, GX-3 acts).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Settle {
    /// Every pipe shows A0 and no flip is pending.
    OnA0,
    /// A pipe asks for A1 (a kernel before this one flipped and did not restore): GX-3 flips back first.
    OnA1,
    /// A flip is pending on a pipe (it will land at the next VUPDATE).
    Pending,
}

/// The attached display: graphics memory, both images, the DMCUB's windows, the pipes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Attach {
    pub memsize_mb: u32,
    pub aper_mb: u32,
    /// Graphics memory in GPU addresses (DCN_VM_FB_LOCATION_BASE/TOP), inclusive.
    pub fb_first: u64,
    pub fb_last: u64,
    pub bar0: u64,
    pub reach: u64,
    /// A0 = the GOP framebuffer, A1 = BACK_OFFSET after it: GPU addresses; `bytes` each.
    pub a0: u64,
    pub a1: u64,
    pub bytes: u64,
    /// A0's offset in graphics memory (= GOP framebuffer − BAR0).
    pub a0_offset: u64,
    pub dmcub_running: bool,
    pub windows: Vec<Window>,
    pub pipes: Vec<Pipe>,
    pub settle: Settle,
}

fn rd(r: &impl Regs, byte: u32) -> Result<u32, Refusal> {
    r.read(byte).ok_or(Refusal::at(Why::Read, byte, 0))
}

/// Gates 4-8 over the registers (V1 `dcn_sizes`, `dcn_dmcub`, `dcn_pipes`, `dcn_gates`, `dcn_settle`).
pub fn reg_gates(r: &impl Regs, pci: &PciOk, gop: Gop) -> Result<Attach, Refusal> {
    // Gate 4: graphics memory, A0 and A1.
    let memsize = rd(r, l::MEMSIZE)?;
    if memsize == 0 || memsize > MEMSIZE_MAX_MB {
        return Err(Refusal::at(Why::MemSize, l::MEMSIZE, memsize));
    }
    let base = rd(r, l::FB_BASE)? & 0xFF_FFFF;
    let top = rd(r, l::FB_TOP)? & 0xFF_FFFF;
    if top < base {
        return Err(Refusal::at(Why::MemSize, l::FB_TOP, top));
    }
    let fb_first = u64::from(base) << 24;
    let fb_last = u64::from(top) << 24 | 0xFF_FFFF;
    if gop.bytes > BACK_OFFSET {
        return Err(Refusal::new(Why::Mode));
    }
    let a0_offset = gop.phys.checked_sub(pci.bar0).ok_or(Refusal::new(Why::NotGop))?;
    let a1_end = a0_offset + BACK_OFFSET + gop.bytes;
    if a1_end > u64::from(memsize) << 20 {
        return Err(Refusal::at(Why::Past, l::MEMSIZE, memsize));
    }
    let a0 = fb_first + a0_offset;
    let a1 = a0 + BACK_OFFSET;
    if fb_first + a1_end - 1 > fb_last {
        return Err(Refusal::at(Why::Past, l::FB_TOP, top));
    }
    let pages = a1_end.div_ceil(PAGE_2M) * PAGE_2M;
    if pages > pci.reach {
        return Err(Refusal::new(Why::Aper));
    }
    let aper = rd(r, l::APER_SIZE)?;
    if pages > u64::from(aper) << 20 {
        return Err(Refusal::at(Why::Aper, l::APER_SIZE, aper));
    }

    // Gate 5: the DMCUB's windows must not touch A1 (it is cleared before the first flip).
    let cntl = rd(r, l::DMCUB_CNTL)?;
    let dmcub_running = cntl & l::DMCUB_ENABLE != 0;
    let mut windows = Vec::new();
    if dmcub_running {
        let (a1_last, a1_first) = (a1 + gop.bytes - 1, a1);
        for w in l::DMCUB_WINDOWS {
            let t = rd(r, w.top)?;
            if t & l::WINDOW_ON == 0 {
                continue;
            }
            let mut span = t & l::WINDOW_TOP;
            if let Some(b) = w.base {
                // A cache window's TOP less BASE; a BASE above TOP wraps to ~4 GB as in V1 and is refused below.
                span = span.wrapping_sub(rd(r, b)? & l::WINDOW_TOP);
            }
            let hi = rd(r, w.offset_high)? & 0xFFFF;
            let first = u64::from(hi) << 32 | u64::from(rd(r, w.offset)?);
            let last = first + u64::from(span);
            if first <= a1_last && last >= a1_first {
                return Err(Refusal::at(Why::Dmcub, w.top, t));
            }
            windows.push(Window { name: w.name, first, last });
        }
    }

    // Gate 6: the pipes that show A0 or A1 with their OTG running.
    let mut pipes = Vec::new();
    let mut otg_off = false;
    for p in 0..l::PIPES {
        if !powered(r, p)? {
            continue;
        }
        let c = rd(r, l::hubp(p, l::HUBP_CNTL))?;
        if c & l::BLANKED != 0 {
            continue;
        }
        let address = surface(r, p)?;
        if address != a0 && address != a1 {
            continue;
        }
        let k = (c >> 4) & 0xF;
        if k >= l::OTGS {
            continue;
        }
        if rd(r, l::otg(k, l::OTG_CONTROL))? & l::OTG_MASTER_EN == 0 {
            otg_off = true;
            continue;
        }
        let flip = rd(r, l::hubp(p, l::HUBP_FLIP))?;
        pipes.push(Pipe { hubp: p as u8, otg: k as u8, flip_type: ((flip & l::FLIP_TYPE) >> 1) as u8, address });
    }
    if pipes.is_empty() {
        return Err(Refusal::new(if otg_off { Why::OtgOff } else { Why::NoPipe }));
    }

    // Gate 7: nothing holds a flip, the surface is plain, the DP DTO signature.
    let mut pending = false;
    for p in &pipes {
        let (h, k) = (u32::from(p.hubp), u32::from(p.otg));
        let flip = rd(r, l::hubp(h, l::HUBP_FLIP))?;
        if flip & l::FLIP_HELD != 0 {
            return Err(Refusal::at(Why::Held, l::hubp(h, l::HUBP_FLIP), flip));
        }
        pending |= flip & l::FLIP_PENDING != 0;
        let flip2 = rd(r, l::hubp(h, l::HUBP_FLIP2))?;
        if flip2 & l::FLIP2_HELD != 0 {
            return Err(Refusal::at(Why::Held, l::hubp(h, l::HUBP_FLIP2), flip2));
        }
        let lock = rd(r, l::otg(k, l::OTG_LOCK))?;
        if lock & l::LOCK_HELD != 0 {
            return Err(Refusal::at(Why::Held, l::otg(k, l::OTG_LOCK), lock));
        }
        let vmid = rd(r, l::hubp(h, l::HUBP_VMID))?;
        if vmid & 0xF != 0 {
            return Err(Refusal::at(Why::Surf, l::hubp(h, l::HUBP_VMID), vmid));
        }
        let surf = rd(r, l::hubp(h, l::HUBP_SURF))?;
        if surf & l::SURF_ODD != 0 {
            return Err(Refusal::at(Why::Surf, l::hubp(h, l::HUBP_SURF), surf));
        }
        let dto = rd(r, l::dto_cntl(k))?;
        if dto & l::DTO_ON != l::DTO_ON {
            return Err(Refusal::at(Why::Sign, l::dto_cntl(k), dto));
        }
        let phase = rd(r, l::dto_phase(k))?;
        if phase == 0 {
            return Err(Refusal::at(Why::Sign, l::dto_phase(k), phase));
        }
        let modulo = rd(r, l::dto_modulo(k))?;
        if modulo != l::DTO_MODULO {
            return Err(Refusal::at(Why::Sign, l::dto_modulo(k), modulo));
        }
    }

    // Gate 8, read-only: where the pipes are.
    let settle = if pipes.iter().any(|p| p.address == a1) {
        Settle::OnA1
    } else if pending {
        Settle::Pending
    } else {
        Settle::OnA0
    };
    Ok(Attach {
        memsize_mb: memsize,
        aper_mb: aper,
        fb_first,
        fb_last,
        bar0: pci.bar0,
        reach: pci.reach,
        a0,
        a1,
        bytes: gop.bytes,
        a0_offset,
        dmcub_running,
        windows,
        pipes,
        settle,
    })
}

/// Whether every power domain pipe `p` may need is on (V1 `dcn_powered`): a gated pipe's registers are not read.
pub fn powered(r: &impl Regs, p: u32) -> Result<bool, Refusal> {
    let mask = l::PIPE_DOMAINS.get(p as usize).copied().unwrap_or(0);
    for d in 0..4 {
        if mask & (1 << d) != 0 && rd(r, l::pg_status(d))? & l::PG_GATED != 0 {
            return Ok(false);
        }
    }
    Ok(true)
}

/// The OTGs that a powered, unblanked pipe drives (HUBP_CNTL's OTG select, as gate 6 reads it), as a bit mask. An
/// unreadable register counts as not driving.
pub fn driven_otgs(r: &impl Regs) -> u32 {
    (0..l::PIPES)
        .filter(|&p| powered(r, p).unwrap_or(false))
        .filter_map(|p| r.read(l::hubp(p, l::HUBP_CNTL)))
        .filter(|c| c & l::BLANKED == 0)
        .map(|c| (c >> 4) & 0xF)
        .filter(|&k| k < l::OTGS)
        .fold(0, |m, k| m | 1 << k)
}

/// The surface address pair of pipe `p` (48 bits, V1 `dcn_pair`).
pub fn surface(r: &impl Regs, p: u32) -> Result<u64, Refusal> {
    let hi = rd(r, l::hubp(p, l::HUBP_ADDR_HI))? & 0xFFFF;
    let lo = rd(r, l::hubp(p, l::HUBP_ADDR))?;
    Ok(u64::from(hi) << 32 | u64::from(lo))
}

/// Every gate from PCI facts on: what the kernel glue and the host tests both call.
pub fn gates(f: &PciFacts, gop: Option<Gop>, r: &impl Regs) -> Result<Attach, Refusal> {
    let pci = pci_gates(f, gop)?;
    let gop = gop.ok_or(Refusal::new(Why::NotGop))?;
    let codec = rd(r, crate::dcn::GATE_REG)?;
    if codec != crate::dcn::GATE_VALUE {
        return Err(Refusal::at(Why::Codec, crate::dcn::GATE_REG, codec));
    }
    reg_gates(r, &pci, gop)
}
