//! GX-7n: the chip's side of the IH ring, over `Hw`: `ih_v6_0_irq_init` (linux-6.6 amdgpu/ih_v6_0.c:293) with
//! `nbio_v7_7_ih_control` and `_ih_doorbell_range`, the sources' enable bits (sdma_v6_0.c:1449, gfx_v11_0.c:5707, :5752,
//! :5894, amdgpu_gmc.c:663), `ih_v6_0_irq_disable` (:371) and the Quiesce hook's raw form. Runs on the kept chain, so every
//! write passes the GX-D9 watch: nothing here names a DCN register, and MMHUB's contexts are not touched (a display
//! fault on MMHUB stays what it is today, no interrupt).
//!
//! Where it differs from Linux: the ring lies in the carve-out, not in GTT (Linux keeps it there only because it needs a
//! CPU mapping; here the carve-out is what needs no GART on MMHUB, which the display uses), so `IH_REQ_NONSNOOP_EN` as
//! Linux's comment says for such memory. No ring 1, no soft ring, so no `force_update_wptr_for_self_int`. `IH_CHICKEN`
//! stays (only for a bus-address ring without PSP loading).

use super::disc::Discovery;
use super::gmc::HubAddrs;
use super::hwid;
use super::psp::{BoAlloc, Hw};
use super::pspbringup::Pieces;
use super::sdma::SdmaAddrs;
use crate::ipmod::gmc11::{self, Hub};
use crate::ipmod::ih6::{self, f};
use crate::ipmod::sdma6;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

/// gc_11_0_0_offset.h, segment 0.
const CP_INT_CNTL_RING0: u32 = 0x1e0a;
const CP_ME1_PIPE0_INT_CNTL: u32 = 0x1e25;
/// CP_INT_CNTL_RING0's and CP_MEn_PIPEn_INT_CNTL's bits.
pub const TIME_STAMP_INT_ENABLE: u32 = 1 << 26;
pub const GENERIC0_INT_ENABLE: u32 = 1 << 31;
pub const PRIV_INSTR_INT_ENABLE: u32 = 1 << 22;
pub const PRIV_REG_INT_ENABLE: u32 = 1 << 23;
/// SDMA0_CNTL's TRAP_ENABLE.
pub const TRAP_ENABLE: u32 = 1 << 0;
/// `hub->vm_cntx_cntl_vm_fault` (gfxhub_v3_0.c:495-501): range, dummy page, PDE0, valid, read, write, execute.
pub const VM_FAULT_INTERRUPTS: u32 = 1 << 9 | 1 << 11 | 1 << 13 | 1 << 15 | 1 << 17 | 1 << 19 | 1 << 21;

/// The MEC's pipes: MES chooses the pipe of a mapped queue, so all four (the bit does nothing on a pipe with no queue).
const ME1_PIPES: usize = 4;
/// `gfx_v11_0` and `gmc` both write every context.
const CONTEXTS: usize = gmc11::CONTEXTS as usize;

/// The registers as BAR5 bytes, the RSMU pair and what goes through it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IhAddrs {
    pub cntl: u32,
    pub base: u32,
    pub base_hi: u32,
    pub rptr: u32,
    pub wptr: u32,
    pub wptr_hi: u32,
    pub wptr_lo: u32,
    pub doorbell_rptr: u32,
    pub flood: u32,
    pub storm_list: u32,
    pub msi_storm: u32,
    pub rsmu_index: u32,
    pub rsmu_data: u32,
    /// RSMU index values.
    pub range: u32,
    pub int_cntl: u32,
    pub int_cntl2: u32,
    pub sdma0_cntl: u32,
    pub cp_int_ring0: u32,
    pub cp_me1_pipe: [u32; ME1_PIPES],
    pub gc_ctx: [u32; CONTEXTS],
}

impl IhAddrs {
    pub fn from_discovery(d: &Discovery) -> Option<IhAddrs> {
        let oss = |at: (usize, u32)| d.reg(hwid::OSSSYS, 0, at.0, at.1).and_then(|x| x.checked_mul(4));
        let nb = |at: (usize, u32)| d.reg(hwid::NBIF, 0, at.0, at.1).and_then(|x| x.checked_mul(4));
        let gc = |dw: u32| d.reg(hwid::GC, 0, 0, dw).and_then(|x| x.checked_mul(4));
        let hub = HubAddrs::from_discovery(d, Hub::Gfx)?;
        let mut gc_ctx = [0; CONTEXTS];
        for (n, c) in gc_ctx.iter_mut().enumerate() {
            *c = hub.byte(gmc11::R::Ctx(n as u8))?;
        }
        let mut cp_me1_pipe = [0; ME1_PIPES];
        for (n, c) in cp_me1_pipe.iter_mut().enumerate() {
            *c = gc(CP_ME1_PIPE0_INT_CNTL + n as u32)?;
        }
        Some(IhAddrs {
            cntl: oss(ih6::IH_RB_CNTL)?,
            base: oss(ih6::IH_RB_BASE)?,
            base_hi: oss(ih6::IH_RB_BASE_HI)?,
            rptr: oss(ih6::IH_RB_RPTR)?,
            wptr: oss(ih6::IH_RB_WPTR)?,
            wptr_hi: oss(ih6::IH_RB_WPTR_ADDR_HI)?,
            wptr_lo: oss(ih6::IH_RB_WPTR_ADDR_LO)?,
            doorbell_rptr: oss(ih6::IH_DOORBELL_RPTR)?,
            flood: oss(ih6::IH_INT_FLOOD_CNTL)?,
            storm_list: oss(ih6::IH_STORM_CLIENT_LIST_CNTL)?,
            msi_storm: oss(ih6::IH_MSI_STORM_CTRL)?,
            rsmu_index: nb(sdma6::RSMU_INDEX)?,
            rsmu_data: nb(sdma6::RSMU_DATA)?,
            range: nb(ih6::IH_DOORBELL_RANGE)?,
            int_cntl: nb(ih6::BIF_INTERRUPT_CNTL)?,
            int_cntl2: nb(ih6::BIF_INTERRUPT_CNTL2)?,
            sdma0_cntl: SdmaAddrs::from_discovery(d)?.byte(sdma6::R::Cntl)?,
            cp_int_ring0: gc(CP_INT_CNTL_RING0)?,
            cp_me1_pipe,
            gc_ctx,
        })
    }

    pub fn readable(&self) -> Vec<u32> {
        let mut v = alloc::vec![
            self.cntl,
            self.base,
            self.base_hi,
            self.rptr,
            self.wptr,
            self.wptr_hi,
            self.wptr_lo,
            self.doorbell_rptr,
            self.flood,
            self.storm_list,
            self.msi_storm,
            self.rsmu_index,
            self.rsmu_data,
            self.sdma0_cntl,
            self.cp_int_ring0,
        ];
        v.extend(self.cp_me1_pipe);
        v.extend(self.gc_ctx);
        v
    }
    pub fn writable(&self) -> Vec<u32> {
        let mut v = self.readable();
        v.retain(|&b| b != self.rsmu_index);
        v
    }
    /// The only values the RSMU index may take for this stage.
    pub fn rsmu_values(&self) -> [u32; 3] {
        [self.range, self.int_cntl, self.int_cntl2]
    }
}

/// Where the ring lies.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IhPlace {
    pub ring_mc: u64,
    pub wptr_mc: u64,
    /// IH_DUMMY read's address: what the CPU never needs, and MSI switches that read off.
    pub dummy_phys: u64,
}

/// The ring, its write-back page, the dummy page.
pub const WB_OFFSET: u64 = ih6::RING_BYTES as u64;
pub const BO_BYTES: u64 = ih6::RING_BYTES as u64 + 2 * 4096;

/// The ring's buffer, in whole pieces as every other buffer of the chain: `start`'s write to the write-back maps its
/// piece uncached for the boot, and a piece may hold nothing mapped with another memory type (`pspbringup::Pieces`). At
/// 4 KiB it would land in the free tail of the piece that holds the GOP's surface, and that write would be refused.
pub fn alloc_bo(bos: &mut dyn BoAlloc) -> Result<(u32, core::ops::Range<u64>), crate::bo::BoError> {
    Pieces(bos).alloc(BO_BYTES, 4096)
}

impl IhPlace {
    /// `offset`: carve-out offset of a `BO_BYTES` buffer.
    pub fn in_carve(c: amdgpu_fmt::addr::Carveout, offset: u64) -> Option<IhPlace> {
        let mc = c.mc_base().checked_add(offset)?;
        Some(IhPlace { ring_mc: mc, wptr_mc: mc + WB_OFFSET, dummy_phys: c.phys_of(offset + WB_OFFSET + 4096)? })
    }
}

/// What `start` found: `stop` puts it back.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Found {
    pub cntl: u32,
    pub doorbell_rptr: u32,
    pub range: u32,
    pub int_cntl: u32,
    pub int_cntl2: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IhError {
    Read { byte: u32 },
    Write { byte: u32 },
    Memory { mc: u64 },
}

fn rd(hw: &mut dyn Hw, byte: u32) -> Result<u32, IhError> {
    hw.reg_read(byte).ok_or(IhError::Read { byte })
}

fn wr(hw: &mut dyn Hw, byte: u32, value: u32) -> Result<(), IhError> {
    if hw.reg_write(byte, value) { Ok(()) } else { Err(IhError::Write { byte }) }
}

fn port_rd(hw: &mut dyn Hw, a: &IhAddrs, index: u32) -> Result<u32, IhError> {
    wr(hw, a.rsmu_index, index)?;
    rd(hw, a.rsmu_data)
}

fn port_wr(hw: &mut dyn Hw, a: &IhAddrs, index: u32, value: u32) -> Result<(), IhError> {
    wr(hw, a.rsmu_index, index)?;
    wr(hw, a.rsmu_data, value)
}

/// ih_v6_0_toggle_ring_interrupts.
fn toggle(hw: &mut dyn Hw, a: &IhAddrs, on: bool) -> Result<(), IhError> {
    let v = rd(hw, a.cntl)?;
    wr(hw, a.cntl, ih6::toggled(v, on))?;
    if !on {
        wr(hw, a.rptr, 0)?;
        wr(hw, a.wptr, 0)?;
    }
    Ok(())
}

/// `ih_v6_0_irq_init`: the ring empty and running, its write-back zero. The sources stay as they are (`sources`). MSI
/// must stand before this: with `msi`, a message goes out for the first entry.
pub fn start(hw: &mut dyn Hw, a: &IhAddrs, p: &IhPlace, msi: bool) -> Result<Found, IhError> {
    let cntl = rd(hw, a.cntl)?;
    let doorbell_rptr = rd(hw, a.doorbell_rptr)?;
    toggle(hw, a, false)?;
    // An older run's write pointer must not read as new entries.
    if !hw.mem_write(p.wptr_mc, &[0u8; 8]) {
        return Err(IhError::Memory { mc: p.wptr_mc });
    }
    hw.store_barrier();
    // nbio ih_control.
    let int_cntl2 = port_rd(hw, a, a.int_cntl2)?;
    port_wr(hw, a, a.int_cntl2, (p.dummy_phys >> 8) as u32)?;
    let int_cntl = port_rd(hw, a, a.int_cntl)?;
    port_wr(hw, a, a.int_cntl, ih6::bif_interrupt_cntl(int_cntl))?;
    // enable_ring.
    wr(hw, a.base, (p.ring_mc >> 8) as u32)?;
    wr(hw, a.base_hi, (p.ring_mc >> 40) as u32 & 0xff)?;
    let v = rd(hw, a.cntl)?;
    wr(hw, a.cntl, ih6::rb_cntl(v, ih6::RING_BYTES, msi))?;
    wr(hw, a.wptr_lo, p.wptr_mc as u32)?;
    wr(hw, a.wptr_hi, (p.wptr_mc >> 32) as u32 & 0xffff)?;
    wr(hw, a.wptr, 0)?;
    wr(hw, a.rptr, 0)?;
    wr(hw, a.doorbell_rptr, ih6::doorbell_rptr())?;
    // ih_doorbell_range.
    let range = port_rd(hw, a, a.range)?;
    port_wr(hw, a, a.range, ih6::doorbell_range(range))?;
    let v = rd(hw, a.storm_list)?;
    wr(hw, a.storm_list, v | f::CLIENT18_IS_STORM_CLIENT)?;
    let v = rd(hw, a.flood)?;
    wr(hw, a.flood, v | f::FLOOD_CNTL_ENABLE)?;
    let v = rd(hw, a.msi_storm)?;
    wr(hw, a.msi_storm, (v & !f::MSI_STORM_DELAY_MASK) | f::MSI_STORM_DELAY)?;
    toggle(hw, a, true)?;
    Ok(Found { cntl, doorbell_rptr, range, int_cntl, int_cntl2 })
}

/// The enable bits of what this driver handles, and only those: a read, a changed value, a write.
pub fn sources(hw: &mut dyn Hw, a: &IhAddrs, on: bool) -> Result<(), IhError> {
    let set = |hw: &mut dyn Hw, byte: u32, bits: u32| -> Result<(), IhError> {
        let v = rd(hw, byte)?;
        wr(hw, byte, if on { v | bits } else { v & !bits })
    };
    if on {
        for c in a.gc_ctx {
            set(hw, c, VM_FAULT_INTERRUPTS)?;
        }
    }
    set(hw, a.sdma0_cntl, TRAP_ENABLE)?;
    for p in a.cp_me1_pipe {
        set(hw, p, TIME_STAMP_INT_ENABLE | GENERIC0_INT_ENABLE)?;
    }
    set(hw, a.cp_int_ring0, TIME_STAMP_INT_ENABLE | GENERIC0_INT_ENABLE | PRIV_REG_INT_ENABLE | PRIV_INSTR_INT_ENABLE)?;
    if !on {
        for c in a.gc_ctx {
            set(hw, c, VM_FAULT_INTERRUPTS)?;
        }
    }
    Ok(())
}

/// `until=stop`: the sources off, the ring off, then what `start` found. Runs before the engines halt and before any of
/// the ring's memory could go. `(every write landed, lines)`.
pub fn stop(hw: &mut dyn Hw, a: &IhAddrs, found: &Found) -> (bool, Vec<String>) {
    let mut lines = Vec::new();
    let mut ok = true;
    if let Err(e) = sources(hw, a, false) {
        ok = false;
        lines.push(format!("stop: the interrupt sources' bits: {e:?}"));
    }
    let off = toggle(hw, a, false);
    ok &= off.is_ok();
    hw.delay_us(1000);
    let back = wr(hw, a.doorbell_rptr, found.doorbell_rptr)
        .and_then(|()| port_wr(hw, a, a.range, found.range))
        .and_then(|()| port_wr(hw, a, a.int_cntl, found.int_cntl))
        .and_then(|()| port_wr(hw, a, a.int_cntl2, found.int_cntl2))
        .and_then(|()| wr(hw, a.cntl, found.cntl & !(f::RB_ENABLE | f::ENABLE_INTR)));
    ok &= back.is_ok();
    lines.push(format!("stop: interrupts off first (sources, IH ring, doorbell side as found): {}", if ok { String::from("done") } else { format!("a step failed ({off:?}, {back:?})") }));
    (ok, lines)
}

/// The Quiesce hook: the ring off through the registers alone (no allocation, no wait): no IH write goes into the next
/// kernel's memory. The sources stay, with nothing behind them.
pub fn quiesce(hw: &mut dyn Hw, a: &IhAddrs) -> bool {
    toggle(hw, a, false).is_ok()
}

/// One line for the tool.
pub fn describe(hw: &mut dyn Hw, a: &IhAddrs) -> String {
    let h = |v: Option<u32>| v.map_or(String::from("-"), |x| format!("{x:#x}"));
    let (c, r, w, d) = (hw.reg_read(a.cntl), hw.reg_read(a.rptr), hw.reg_read(a.wptr), hw.reg_read(a.doorbell_rptr));
    format!("IH_RB_CNTL {}, RPTR {}, WPTR {}, DOORBELL_RPTR {}", h(c), h(r), h(w), h(d))
}
