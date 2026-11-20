//! GX-8v: the VCN stage's register sequences besides the start (`gpucore::vcn` re-exports them): the indirect SRAM
//! list, the stop (vcn_v4_0_stop with DPG), the Quiesce halt, and the answer's lines.

use super::bringup::Bringup;
use super::psp::Hw;
use super::smu::{self, SmuTimeouts};
use super::vcn::{Layout, Stopped, VcnAddrs, VcnReport, lay, queue_mode, rd, wr};
use super::watch::Watch;
use crate::bo::PAGE;
use crate::ipmod::smu13::{self, msg};
use crate::ipmod::vcn4::{R, f};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use amdgpu_fmt::vcn::{self as pkt, Sram, shared};

/// The SRAM list of `vcn_v4_0_start_dpg_mode` with `indirect` and PSP loading, in Linux's order (clock gating left
/// out: the 780M has VCN_MGCG, soc21.c:630; RAS left out: amdgpu_ras_is_supported is false on this APU).
pub fn sram_list(a: &VcnAddrs, l: &Layout, gb_addr_config: u32) -> Option<Sram> {
    let mut s = Sram::default();
    let mut put = |r: R, v: u32| a.byte(r).map(|b| s.push(b, v));
    let ctx = l.bo_mc + lay::CONTEXT;
    let vcpu = 0xff << f::VCPU_PRB_TIMEOUT_SHIFT | f::VCPU_CLK_EN;
    put(R::VcpuCntl, vcpu | f::VCPU_BLK_RST)?;
    put(R::MastintEn, 0)?;
    put(R::LmiCtrl, f::LMI_CTRL)?;
    put(R::MpcCntl, f::MPC_CNTL)?;
    put(R::MpcSetMuxa0, f::MPC_MUXA0)?;
    put(R::MpcSetMuxb0, f::MPC_MUXB0)?;
    put(R::MpcSetMux, f::MPC_MUX)?;
    // mc_resume_dpg_mode, indirect with PSP loading: windows 0 and 1 are the PSP's (zeros here).
    for r in [R::CacheBarLo, R::CacheBarHi, R::CacheOffset0, R::CacheSize0, R::Cache1BarLo, R::Cache1BarHi, R::CacheOffset1] {
        put(r, 0)?;
    }
    put(R::CacheSize1, pkt::STACK_BYTES as u32)?;
    put(R::Cache2BarLo, ctx as u32)?;
    put(R::Cache2BarHi, (ctx >> 32) as u32)?;
    put(R::CacheOffset2, 0)?;
    put(R::CacheSize2, pkt::CONTEXT_BYTES as u32)?;
    put(R::Nc0BarLo, l.shared as u32)?;
    put(R::Nc0BarHi, (l.shared >> 32) as u32)?;
    put(R::NcOffset0, 0)?;
    put(R::NcSize0, PAGE as u32)?;
    put(R::AddrConfig, gb_addr_config)?;
    put(R::VcpuCntl, vcpu)?;
    put(R::LmiCtrl2, f::LMI_CTRL2)?;
    put(R::MastintEn, f::MASTINT_VCPU_EN)?;
    Some(s)
}

/// vcn_v4_0_stop: DPG_HOLD_OFF in the shared page, `stop_dpg_mode` (UVD_POWER_STATUS back to 1, the ring drained,
/// PG_MODE off), then PowerDownVcn. Each wait is bounded; the power-down is sent whatever the waits found.
pub fn stop<H: Hw>(owner: &mut Bringup<Watch<H>>, a: &VcnAddrs, l: Option<&Layout>, s: &smu13::Addrs, t: SmuTimeouts, us: u64) -> Stopped {
    let mut out = Stopped::default();
    if let Some(l) = l {
        let _ = queue_mode(owner, l, shared::QUEUE_DPG_HOLD_OFF, true);
    }
    let ps_one = |h: &mut dyn Hw| rd(h, a, R::PowerStatus).is_ok_and(|v| v != u32::MAX && v & f::POWER_STATUS_MASK == 1);
    let mut polls = 0;
    let p1 = super::gmc::poll(owner, us, &mut polls, &mut |h| ps_one(h));
    let wp = rd(owner, a, R::RbWptr).ok();
    let drained = wp.is_some_and(|w| super::gmc::poll(owner, us, &mut polls, &mut |h| rd(h, a, R::RbRptr).ok() == Some(w)));
    let p2 = super::gmc::poll(owner, us, &mut polls, &mut |h| ps_one(h));
    out.idle = p1 && drained && p2;
    if let Ok(v) = rd(owner, a, R::PowerStatus)
        && v != u32::MAX
    {
        out.pg_mode_off = wr(owner, a, R::PowerStatus, v & !f::PG_MODE).is_ok();
    }
    let _ = rd(owner, a, R::Status);
    out.power_down = Some(smu::send_video(owner, s, t, msg::POWER_DOWN_VCN, 0));
    klog::event!(Info, "gx", "vcn.stopped", idle = out.idle, pg_mode_off = out.pg_mode_off, powered_down = out.ok());
    out
}

/// The Quiesce hook's halt (interrupts off, no lock, no allocation, bounded): PG_MODE off and PowerDownVcn on raw
/// registers, so no VCPU reads this kernel's memory after the hand-over (armed only by a stage that ran). True:
/// both landed.
pub fn halt(hw: &mut dyn Hw, a: &VcnAddrs, s: &smu13::Addrs) -> bool {
    let pg = a.byte(R::PowerStatus).and_then(|b| hw.reg_read(b).filter(|&v| v != u32::MAX).map(|v| hw.reg_write(b, v & !f::PG_MODE))).unwrap_or(false);
    let down = smu::send_video(hw, s, SmuTimeouts::QUIESCE, msg::POWER_DOWN_VCN, 0).is_ok();
    pg && down
}

/// The answer's lines.
pub fn render(r: &VcnReport) -> Vec<String> {
    let hx = |v: Option<u32>| v.map_or(String::from("-"), |v| format!("{v:#x}"));
    let mut v = Vec::new();
    if let Some(l) = r.layout {
        v.push(format!("vcn buffer at MC {:#x}{} (ring {:#x}, shared page {:#x}), VMID 0 through MMHUB's system aperture", l.bo_mc, if r.reused_bo { " (reused)" } else { "" }, l.ring, l.shared));
    }
    if let Some((b, a)) = r.range {
        v.push(format!("vcn NBIO GDC0_BIF_VCN0_DOORBELL_RANGE {} -> {}, doorbell aperture {} ({})", hx(b), hx(a), hx(r.aperture), if r.doorbell { "doorbell" } else { "UVD_RB_WPTR" }));
    }
    if let Some(p) = &r.power_up {
        v.push(format!("vcn PowerUpVcn: {}", match p { Ok(x) => format!("OK (argument {x:#x})"), Err(e) => format!("{e:?}") }));
    }
    for (x, val) in &r.found {
        v.push(format!("vcn found {} = {}", x.name(), hx(*val)));
    }
    if !r.sram.is_empty() {
        v.push(format!("vcn SRAM list: {} pairs, PSP status {}", r.sram.len(), hx(r.sram_status)));
    }
    let moved = r.mm_before.iter().zip(&r.mm_after).filter(|(a, b)| a != b).count();
    v.push(format!("vcn MMHUB display fields: {} read, {moved} moved; MMHUB registers written by this stage: {} (contexts 1-7 and engine 0's range, none the display's)", r.mm_before.len(), r.contexts.len()));
    if let Some(s) = &r.stop {
        v.push(format!("vcn stop: idle {}, PG_MODE off {}, PowerDownVcn {:?}", s.idle, s.pg_mode_off, s.power_down));
    }
    v.push(format!("vcn writes: {}; left running: {}", r.writes.1.saturating_sub(r.writes.0), r.left_running));
    if let Some(e) = &r.error {
        v.push(format!("vcn error: {e:?}{}", e.advice()));
    }
    v
}

