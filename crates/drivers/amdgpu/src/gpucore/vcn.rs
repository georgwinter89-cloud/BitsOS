//! GX-8v: the VCN stage of every boot's chain (`until=backend`): VCN 0 of the 780M started as Linux 7.3.1 starts it
//! on an APU with PSP loading and dynamic power gating (vcn_v4_0.c hw_init :318-353, start :1136 -> start_dpg_mode
//! :996-1130), with no self-test after it (Georg 18.11.: no video self-test, the fastest boot):
//! 1. the NBIO doorbell range for VCN 0 (nbio_v7_7.c:87-105), before anything of VCN (hw_init's order);
//! 2. PowerUpVcn through the SMU (smu_v13_0.c:1857, argument `instance << 16` = 0);
//! 3. the shared page (`vcn_v4_0_fw_shared_init`), UVD_POWER_STATUS's two writes, the indirect SRAM list (VCPU clock,
//!    LMI, MPC, the cache windows of `mc_resume_dpg_mode` with the PSP's zeros for windows 0 and 1, the context and
//!    the shared page) loaded by the PSP as GFX_FW_TYPE_VCN0_RAM, then the ring's registers and the doorbell;
//! 4. MMHUB's contexts 1-7 and invalidation engine 0's range for programs' VMIDs (`vcnvm`, as
//!    `mmhub_v3_0_1_setup_vmid_config` and `program_invalidation` write them; none a display field).
//!
//! The stage passes when every step wrote without an error, the PSP took the SRAM list with status 0 and MMHUB's
//! display fields read as before. Nothing proves the VCPU runs before a program's first job: that job's fence does.
//!
//! Memory (plan/spec/gpu.md 11.2, GX-D9): everything VCN reads or writes for the kernel lies in one carve-out BO and
//! is reached at VMID 0 through MMHUB's system aperture, which the firmware left as the display uses it (Linux puts
//! the ring in GTT through MMHUB's GART instead). The PSP puts VCN's firmware in its TMR (carve-out) and points cache
//! window 0 at it itself. Every register write goes through the bring-up owner and the scanout watcher; the stage
//! reads MMHUB's display fields before and after and fails when one moved.

use super::bringup::{Bringup, Outcome as WriteOutcome};
use super::disc::Discovery;
use super::hwid;
use super::psp::{BoAlloc, Hw, Mailbox, Psp, PspError};
use super::smu::{self, SmuError, SmuTimeouts};
use super::vcnring::{VcnQueue, mem};
use super::watch::{MMHUB_DISPLAY, Watch};
use crate::bo::{BoError, PAGE};
use crate::ipmod::smu13::{self, msg};
use crate::ipmod::vcn4::{self, R, f};
use alloc::format;
use alloc::vec::Vec;
use amdgpu_fmt::addr::Carveout;
use amdgpu_fmt::vcn::{self as pkt, shared};

pub use super::vcnaddr::{BO_BYTES, Layout, VcnAddrs, VcnBo, VcnNbio, WB_JOB_FENCE, WB_WPTR, lay};
pub use super::vcnseq::{halt, render, sram_list, stop};

#[derive(Clone, Copy)]
pub struct VcnInputs<'a> {
    pub addrs: VcnAddrs,
    pub smu: smu13::Addrs,
    pub smu_t: SmuTimeouts,
    pub carve: Carveout,
    /// GB_ADDR_CONFIG as GC reads it (UVD_GFX10_ADDR_CONFIG gets it).
    pub gb_addr_config: u32,
    /// Where the PSP put VCN's firmware (LOAD_IP_FW's answer in this run); `None`: not loaded, the stage refuses.
    pub fw_tmr: Option<u64>,
    /// VCN 4.0.2's DRM key workaround in the shared page (vcn_v4_0.c:160).
    pub drm_key: bool,
    pub timeout_us: u64,
    /// MMHUB's display fields (BAR5 bytes): read before and after, equal or the stage fails.
    pub mm_display: &'a [u32],
    /// MMHUB's registers: its contexts 1-7 for programs' jobs (`vcnvm`, no display field).
    pub mm: super::gmc::HubAddrs,
    /// The BO of an earlier run in this boot (reused).
    pub bo: Option<VcnBo>,
    /// Called once, before the stage's first VCN write: the live side arms its Quiesce halt.
    pub arm: Option<&'a dyn Fn(VcnAddrs)>,
}

/// MMHUB's display fields as BAR5 bytes (the watcher's list).
pub fn mm_display_bytes(d: &Discovery) -> Vec<u32> {
    MMHUB_DISPLAY.iter().filter_map(|&(_, (seg, dw))| d.reg(hwid::MMHUB, 0, seg, dw).and_then(|b| b.checked_mul(4))).collect()
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VcnError {
    NoAddress,
    /// VCN's firmware was not loaded in this PSP run.
    NoFirmware,
    Smu(SmuError),
    /// A VCN register reads all ones after PowerUpVcn: the block is not powered.
    NotPowered { byte: u32 },
    Read { byte: u32 },
    Write { byte: u32 },
    Bo(BoError),
    BoTooSmall { have: u64, need: u64 },
    Mem { mc: u64 },
    Psp(PspError),
    /// The PSP answered the SRAM load with this status (the ring still answers: VCN alone failed).
    SramStatus(u32),
    Ring(amdgpu_fmt::ring::RingError),
    /// The doorbell write was refused (no BAR2, or not this owner's).
    NoDoorbell,
    /// An MMHUB field the display uses read differently after the stage (GX-D9).
    DisplayMoved { byte: u32, before: Option<u32>, after: Option<u32> },
    /// MMHUB's contexts for programs' VMIDs could not be set (nothing of the display written).
    Contexts(super::vcnvm::CtxError),
}

impl VcnError {
    pub fn advice(&self) -> &'static str {
        match self {
            VcnError::NoFirmware => ": VCN's firmware was not loaded by this run's PSP (the stick's firmware folder: vcn_4_0_2.bin)",
            VcnError::Smu(_) => ": PMFW did not power VCN up; nothing of VCN was written",
            VcnError::SramStatus(_) => ": the PSP refused VCN's SRAM list; VCN is powered down again, the PSP ring still answers",
            VcnError::DisplayMoved { .. } => ": a display field of MMHUB moved: stop, tell Georg",
            _ => "",
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct VcnReport {
    pub layout: Option<Layout>,
    pub bo: Option<VcnBo>,
    pub reused_bo: bool,
    /// NBIO's VCN doorbell range before and after, the aperture as found.
    pub range: Option<(Option<u32>, Option<u32>)>,
    pub aperture: Option<u32>,
    /// PowerUpVcn's answer (the argument register), when sent.
    pub power_up: Option<Result<u32, SmuError>>,
    /// The registers after PowerUpVcn, before the stage's first VCN write.
    pub found: Vec<(R, Option<u32>)>,
    /// The SRAM list (offset, value) and the PSP's status for it.
    pub sram: Vec<(u32, u32)>,
    pub sram_status: Option<u32>,
    pub doorbell: bool,
    /// The ring's write pointer after the start (a counter): where the job half begins.
    pub wptr: u64,
    /// The MMHUB context registers written for programs' VMIDs (none of them a display field).
    pub contexts: Vec<super::vcnvm::Ctx>,
    /// MMHUB's display fields before and after (byte, value).
    pub mm_before: Vec<(u32, Option<u32>)>,
    pub mm_after: Vec<(u32, Option<u32>)>,
    /// The owner's write count when the stage began and ended.
    pub writes: (u64, u64),
    /// VCN stays running (Linux's state after hw_init); otherwise it was stopped and powered down.
    pub left_running: bool,
    pub stop: Option<Stopped>,
    pub error: Option<VcnError>,
}

impl VcnReport {
    pub fn passed(&self) -> bool {
        self.error.is_none() && self.left_running && self.mm_before == self.mm_after
    }
}

/// What a stop did (vcn_v4_0_stop_dpg_mode, then PowerDownVcn).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Stopped {
    /// The ring drained (RB_RPTR reached RB_WPTR) and UVD_POWER_STATUS read 1 again.
    pub idle: bool,
    pub pg_mode_off: bool,
    pub power_down: Option<Result<u32, SmuError>>,
}

impl Stopped {
    pub fn ok(&self) -> bool {
        self.pg_mode_off && matches!(self.power_down, Some(Ok(_)))
    }
}

pub(super) fn rd(o: &mut dyn Hw, a: &VcnAddrs, r: R) -> Result<u32, VcnError> {
    let b = a.byte(r).ok_or(VcnError::NoAddress)?;
    o.reg_read(b).ok_or(VcnError::Read { byte: b })
}

pub(super) fn wr<H: Hw>(o: &mut Bringup<Watch<H>>, a: &VcnAddrs, r: R, v: u32) -> Result<(), VcnError> {
    let b = a.byte(r).ok_or(VcnError::NoAddress)?;
    match o.write(b, v).outcome {
        WriteOutcome::Ok | WriteOutcome::Mismatch => Ok(()),
        _ => Err(VcnError::Write { byte: b }),
    }
}

fn port_read<H: Hw>(o: &mut Bringup<Watch<H>>, n: &VcnNbio) -> Result<u32, VcnError> {
    if !matches!(o.write(n.rsmu_index, n.range).outcome, WriteOutcome::Ok | WriteOutcome::Mismatch) {
        return Err(VcnError::Write { byte: n.rsmu_index });
    }
    o.reg_read(n.rsmu_data).ok_or(VcnError::Read { byte: n.rsmu_data })
}

fn port_write<H: Hw>(o: &mut Bringup<Watch<H>>, n: &VcnNbio, v: u32) -> Result<(), VcnError> {
    if !matches!(o.write(n.rsmu_index, n.range).outcome, WriteOutcome::Ok | WriteOutcome::Mismatch) {
        return Err(VcnError::Write { byte: n.rsmu_index });
    }
    if !matches!(o.write(n.rsmu_data, v).outcome, WriteOutcome::Ok | WriteOutcome::Mismatch) {
        return Err(VcnError::Write { byte: n.rsmu_data });
    }
    Ok(())
}

fn read_mm(o: &mut dyn Hw, bytes: &[u32]) -> Vec<(u32, Option<u32>)> {
    bytes.iter().map(|&b| (b, o.reg_read(b))).collect()
}

/// The stage. No VCN register is read before PowerUpVcn answered.
pub fn enable<H: Hw, M: Mailbox>(owner: &mut Bringup<Watch<H>>, bo: &mut dyn BoAlloc, psp: &mut Psp<M>, inp: &VcnInputs) -> VcnReport {
    let mut r = VcnReport::default();
    r.writes.0 = owner.log().totals().writes;
    r.mm_before = read_mm(owner, inp.mm_display);
    let res = run(owner, bo, psp, inp, &mut r);
    if let Err(e) = res {
        klog::event!(Error, "gx", "vcn.failed", error = format!("{e:?}").as_str());
        r.error = Some(e);
    }
    let all = r.error.is_none();
    if !all && r.power_up.as_ref().is_some_and(Result::is_ok) {
        // A failed step after the power-up: Linux's stop, then PowerDownVcn (no VCPU left on our buffers).
        let l = r.layout;
        r.stop = Some(stop(owner, &inp.addrs, l.as_ref(), &inp.smu, inp.smu_t, inp.timeout_us));
        klog::event!(Warn, "gx", "vcn.halted"; "a step failed: VCN stopped and powered down again");
    }
    r.left_running = all;
    r.mm_after = read_mm(owner, inp.mm_display);
    if r.error.is_none()
        && let Some((&(byte, before), &(_, after))) = r.mm_before.iter().zip(&r.mm_after).find(|(x, y)| x != y)
    {
        klog::event!(Error, "gx", "vcn.display_moved", byte = klog::Hex(u64::from(byte)); "an MMHUB display field moved");
        r.error = Some(VcnError::DisplayMoved { byte, before, after });
    }
    r.writes.1 = owner.log().totals().writes;
    if r.passed() {
        klog::event!(Info, "gx", "vcn.enabled", contexts = r.contexts.len(), doorbell = r.doorbell; "VCN 0 is started for programs' video jobs");
    }
    r
}

fn run<H: Hw, M: Mailbox>(owner: &mut Bringup<Watch<H>>, bo: &mut dyn BoAlloc, psp: &mut Psp<M>, inp: &VcnInputs, r: &mut VcnReport) -> Result<(), VcnError> {
    let a = inp.addrs;
    let fw = inp.fw_tmr.ok_or(VcnError::NoFirmware)?;
    // 1. The buffer (kept for the boot; an earlier run's reused).
    let b = match inp.bo {
        Some(b) if b.size >= lay::END => {
            r.reused_bo = true;
            b
        }
        Some(b) => return Err(VcnError::BoTooSmall { have: b.size, need: lay::END }),
        None => {
            let (handle, range) = bo.alloc(BO_BYTES, PAGE).map_err(VcnError::Bo)?;
            VcnBo { handle, offset: range.start, size: range.end - range.start }
        }
    };
    r.bo = Some(b);
    if b.size < lay::END {
        return Err(VcnError::BoTooSmall { have: b.size, need: lay::END });
    }
    let l = Layout::new(inp.carve.mc_base() + b.offset);
    r.layout = Some(l);
    klog::event!(Info, "gx", "vcn.bo", mc = klog::Hex(l.bo_mc), reused = r.reused_bo, fw = klog::Hex(fw));
    // 2. hw_init: NBIO routes VCN 0's doorbells (only when the range differs; the aperture tells doorbell or MMIO).
    if let Some(n) = a.nbio {
        let aper = owner.reg_read(n.aper_en);
        r.aperture = aper;
        let before = port_read(owner, &n)?;
        let want = vcn4::doorbell_range(before, vcn4::DOORBELL_RANGE_START_DW, vcn4::DOORBELL_RANGE_SIZE);
        if want != before {
            port_write(owner, &n, want)?;
        }
        r.range = Some((Some(before), port_read(owner, &n).ok()));
        r.doorbell = aper.is_some_and(|v| v != u32::MAX && v & 1 != 0);
    }
    // 3. begin_use -> vcn_v4_0_start: PowerUpVcn first; VCN's registers mean nothing before it.
    let up = smu::send_video(owner, &inp.smu, inp.smu_t, msg::POWER_UP_VCN, 0);
    klog::event!(Info, "gx", "vcn.power_up", ok = up.is_ok());
    r.power_up = Some(up);
    up.map_err(VcnError::Smu)?;
    for x in R::ALL {
        let v = a.byte(x).and_then(|bt| owner.reg_read(bt));
        r.found.push((x, v));
    }
    let ps = rd(owner, &a, R::PowerStatus)?;
    if ps == u32::MAX {
        return Err(VcnError::NotPowered { byte: a.byte(R::PowerStatus).unwrap_or(0) });
    }
    if let Some(arm) = inp.arm {
        arm(a);
    }
    // 4. The memory VCN reads: the shared page as sw_init leaves it, ring and slots zeroed (the job half's fence too).
    mem(owner, l.shared, &pkt::fw_shared(inp.drm_key))?;
    mem(owner, l.bo_mc + lay::WB, &[0u8; 0x1000])?;
    // 5. vcn_v4_0_start_dpg_mode's CPU writes: the anti-hang off (POWER_STATUS 1), then PG_MODE and PG_EN.
    wr(owner, &a, R::PowerStatus, (ps & !f::POWER_STATUS_MASK) | 1)?;
    let ps = rd(owner, &a, R::PowerStatus)?;
    wr(owner, &a, R::PowerStatus, ps | f::PG_MODE | f::PG_EN)?;
    // 6. The SRAM list through the PSP (amdgpu_vcn_psp_update_sram).
    let s = sram_list(&a, &l, inp.gb_addr_config).ok_or(VcnError::NoAddress)?;
    r.sram = s.0.chunks_exact(2).map(|p| (p[0], p[1])).collect();
    mem(owner, l.sram, &s.to_bytes())?;
    owner.store_barrier();
    let resp = psp.load_vcn_sram(owner, l.sram, s.bytes()).map_err(VcnError::Psp)?;
    r.sram_status = Some(resp.status);
    if resp.status != 0 {
        return Err(VcnError::SramStatus(resp.status));
    }
    // 7. The ring (vcn_v4_0.c:1095-1123): base and size, RB1 off, the shared page's RING_RESET, pointers at 0, RB1 on.
    let mut q = VcnQueue::new(a, l.ring, l.wptr, 0, r.doorbell)?;
    q.clear(owner)?;
    wr(owner, &a, R::RbBaseLo, l.ring as u32)?;
    wr(owner, &a, R::RbBaseHi, (l.ring >> 32) as u32)?;
    wr(owner, &a, R::RbSize, vcn4::RING_DW as u32)?;
    let en = rd(owner, &a, R::RbEnable)?;
    wr(owner, &a, R::RbEnable, en & !f::RB1_EN)?;
    queue_mode(owner, &l, shared::QUEUE_RING_RESET, true)?;
    wr(owner, &a, R::RbRptr, 0)?;
    wr(owner, &a, R::RbWptr, 0)?;
    let rp = rd(owner, &a, R::RbRptr)?;
    wr(owner, &a, R::RbWptr, rp)?;
    let en = rd(owner, &a, R::RbEnable)?;
    wr(owner, &a, R::RbEnable, en | f::RB1_EN)?;
    queue_mode(owner, &l, shared::QUEUE_RING_RESET | shared::QUEUE_DPG_HOLD_OFF, false)?;
    wr(owner, &a, R::Rb1DbCtrl, vcn4::rb1_db_ctrl(vcn4::DOORBELL_DW))?;
    // One read-back so the writes landed (UVD_STATUS reads 0xdeadbeef in DPG mode: never data).
    let _ = rd(owner, &a, R::Status)?;
    r.wptr = q.wptr();
    klog::event!(Info, "gx", "vcn.started", sram_pairs = r.sram.len(), doorbell = r.doorbell);
    // 8. Programs' VMIDs on MMHUB (setup_vmid_config's non-display part and engine 0's range).
    let guard = owner.hw().guard().clone();
    r.contexts = super::vcnvm::enable(owner, &inp.mm, &guard).map_err(VcnError::Contexts)?;
    Ok(())
}

/// `fw_shared->sq.queue_mode`'s bits set or cleared (a CPU store into the shared page).
pub(super) fn queue_mode(hw: &mut dyn Hw, l: &Layout, bits: u8, set: bool) -> Result<(), VcnError> {
    let at = l.shared + shared::SQ_QUEUE_MODE as u64;
    let mut b = [0u8; 1];
    if !hw.mem_read(at, &mut b) {
        return Err(VcnError::Mem { mc: at });
    }
    b[0] = if set { b[0] | bits } else { b[0] & !bits };
    mem(hw, at, &b)?;
    hw.store_barrier();
    Ok(())
}

/// The VCN firmware's TMR address from a PSP run's report (LOAD_IP_FW's answer for type 13).
pub fn fw_tmr(rep: &super::psp::Report) -> Option<u64> {
    rep.loaded.iter().find(|x| x.id == super::ucode::UcodeId::Vcn && x.status == 0).map(|x| x.tmr_addr)
}
