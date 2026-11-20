//! GX-8v live glue: the VCN stage on the kept chain (every boot's `until=backend`), its stop before the PSP's, the
//! Quiesce halt, and the job half's start. No self-test runs (Georg 18.11.: no video self-test, the fastest boot):
//! the ring is offered to programs once the stage passed.
//!
//! The owner's lists grow by VCN's registers (`VcnAddrs::writable`: the CPU's part of the DPG start and stop), NBIO's
//! VCN doorbell range as one more RSMU index value, VCN's doorbell (BAR2 dword 0x312) and MMHUB's context registers
//! 1-7 and engine 0's range (`vcnvm::writable`: none a display field). MMHUB's 23 display fields are read before and
//! after; the DCN snapshot, the display's write count and the OTG CRC are compared around the stage (GX-D9).

use super::*;
use crate::gpucore::gmc::HubAddrs;
use crate::gpucore::smu::SmuTimeouts;
use crate::gpucore::vcn::{self, Layout, VcnAddrs, VcnInputs};
use crate::gpucore::vcnvm;
use crate::ipmod::gmc11::Hub;
use crate::ipmod::{smu13, vcn4};

/// What a passed VCN stage left on the kept run (until=stop and the job half read it).
#[derive(Clone, Copy, Debug)]
pub(super) struct VcnKept {
    pub a: VcnAddrs,
    pub layout: Layout,
    pub smu: smu13::Addrs,
    pub start: u64,
    pub doorbell: bool,
    pub mm: HubAddrs,
    /// The stage passed (else it is kept only for until=stop to power VCN down).
    pub passed: bool,
}

/// The backend chain's VCN stage: PASS, FAIL, or REFUSED when nothing was written.
pub(super) fn run(g: &'static Gpu, lines: &mut Vec<String>) -> Result<bool, ()> {
    let dv = g.st();
    if dv.reboot_needed.load(Ordering::Acquire) || dv.quarantined.load(Ordering::Acquire) > 0 {
        return refuse(lines, String::from("an earlier run left the PSP or PMFW in a state only a reboot clears: no VCN start"));
    }
    let (Ok(a), Some(bos)) = (&g.display, &g.bos) else {
        return refuse(lines, String::from("display or BO manager gone"));
    };
    let found = probe::kept(g.regs).map(Ok).unwrap_or_else(|| probe::fetch_indirect(g.regs));
    let d = match found.and_then(|f| disc::parse(&f.bytes).map_err(|w| format!("{w}"))) {
        Ok(d) => d,
        Err(e) => return refuse(lines, format!("no discovery table: {e}")),
    };
    let win = match carve_window(g, a, &d) {
        Ok(w) => w,
        Err(e) => return refuse(lines, e),
    };
    let Some(carve) = amdgpu_fmt::addr::Carveout::new(win.mc_base, win.phys_base, win.size) else {
        return refuse(lines, format!("carve-out {win:?} is not page aligned"));
    };
    let (Some(va), Some(gi), Some(mm)) = (VcnAddrs::from_discovery(&d), GfxInputs::from_discovery(&d), HubAddrs::from_discovery(&d, Hub::Mm)) else {
        return refuse(lines, String::from("no VCN, SMU or MMHUB bases in the discovery table"));
    };
    let Some(mut guard) = dv.live.lock_timeout(WAIT_NS) else {
        return refuse(lines, String::from("busy: another run holds the PSP"));
    };
    let Some(l) = guard.as_mut() else {
        return refuse(lines, String::from("no kept run: until=backend (or a kept until=sdma) first; VCN's firmware comes with that run's list"));
    };
    if l.vcn.is_some() {
        lines.push(String::from("vcn: a VCN stage is kept on this run (until=stop stops it first; then the chain again for another start)"));
        return Ok(true);
    }
    let Some(fw) = vcn::fw_tmr(&l.psp.last) else {
        return refuse(lines, String::from("the kept run's PSP did not load VCN's firmware (missing on the stick, or its load failed)"));
    };
    let gb = drm::profile_780m::GB_ADDR_CONFIG * 4;
    let mm_display = vcn::mm_display_bytes(&d);
    {
        let hw = l.owner.hw().hw();
        hw.allow_gmc([alloc::vec![gb], mm_display.clone()].concat(), Vec::new());
        hw.allow_vcn(&va, bar2(g), vcnvm::writable(&mm));
    }
    let Some(gb_value) = l.owner.reg_read(gb).filter(|&v| v != 0 && v != u32::MAX) else {
        return refuse(lines, String::from("GB_ADDR_CONFIG did not read (GC asleep?)"));
    };
    register_quiesce();
    dv.vcn_armed.get_or_init(|| (va, gi.smu));
    let before = snapshot::take(g.regs, false);
    let dcn0 = dcn::marks();
    let crc0 = crc_now(g.regs, a);
    let (seq0, v0) = (l.owner.log().totals().writes, l.owner.hw().total());
    let inp = VcnInputs {
        addrs: va,
        smu: gi.smu,
        smu_t: SmuTimeouts::RUNNING,
        carve,
        gb_addr_config: gb_value,
        fw_tmr: Some(fw),
        drm_key: true,
        timeout_us: vcn4::LINUX_TIMEOUT_US,
        mm_display: &mm_display,
        mm,
        bo: *dv.vcn_bo.lock(),
        arm: None,
    };
    lines.push(format!("vcn: VCN's firmware in the TMR at {fw:#x}"));
    let r = vcn::enable(&mut l.owner, &mut pspbringup::Pieces(&mut SharedBos(bos, &dv.pool)), &mut l.psp, &inp);
    *dv.vcn_bo.lock() = r.bo;
    lines.extend(vcn::render(&r));
    // A passed stage stays for the backend and until=stop; one whose own stop did not prove VCN down stays for
    // until=stop to try again.
    let down_unproven = r.stop.as_ref().is_some_and(|s| !s.ok());
    if (r.passed() || down_unproven)
        && let Some(layout) = r.layout
    {
        l.vcn = Some(VcnKept { a: va, layout, smu: gi.smu, start: r.wptr, doorbell: r.doorbell, mm, passed: r.passed() });
    }
    if l.psp.wedged() {
        lines.push(String::from("vcn: the PSP ring is wedged after the SRAM load: reboot before more GPU work"));
        dv.reboot_needed.store(true, Ordering::Release);
    }
    let violations = l.owner.hw().total() - v0;
    lines.push(format!("watch: {violations} writes to display registers or the scanout blocked"));
    let entries: Vec<_> = l.owner.log().since(seq0).copied().collect();
    lines.push(format!("owner: {} writes in this stage", entries.len()));
    for e in &entries {
        let name = va.name_at(e.byte).map(String::from).unwrap_or_else(|| pspbringup::reg_name(&l.addrs, Some(&gi), None, e.byte));
        lines.push(pspbringup::w_line(e, &name));
    }
    drop(guard);
    let after = snapshot::take(g.regs, false);
    let (same, pl) = picture(before.regs, &after.regs, &dcn0, &dcn::marks());
    lines.extend(pl);
    let (crc_same, cl) = crc_compare(&crc0, &crc_now(g.regs, a));
    lines.extend(cl);
    let pass = r.passed() && violations == 0 && same && crc_same;
    lines.push(format!("vcn verdict: {}", if pass { "PASS" } else { "FAIL" }));
    Ok(pass)
}

/// until=stop's first step (Linux's fini runs VCN before the PSP): stop_dpg_mode and PowerDownVcn. False: VCN not
/// proven down.
pub(super) fn stop(l: &mut Live, lines: &mut Vec<String>) -> bool {
    let Some(v) = l.vcn.take() else { return true };
    let s = vcn::stop(&mut l.owner, &v.a, Some(&v.layout), &v.smu, SmuTimeouts::RUNNING, vcn4::LINUX_TIMEOUT_US);
    lines.push(format!("stop: VCN stopped first (DPG off, PowerDownVcn): idle {}, PG_MODE off {}, PowerDownVcn {:?}", s.idle, s.pg_mode_off, s.power_down));
    s.ok()
}

/// The Quiesce path's VCN halt through the raw registers (no owner, no lock, no allocation; bounded waits).
pub(super) fn quiesce(dv: &State) {
    let Some(regs) = DcnRegs::attached_now() else { return };
    if let Some(&(a, s)) = dv.vcn_armed.get() {
        let ok = vcn::halt(&mut RawRegs(regs), &a, &s);
        klog::try_event!(Info, "gx", "vcn.quiesce_halt", ok = ok; "VCN powered down before the hand-over");
    }
}

/// The backend's VCN half (after the GFX jobs' proof): the job half on the kept ring, offered to programs at once. A
/// failure costs no backend: the ring is not offered, and the line says so.
pub(super) fn backend_half(g: &'static Gpu, gpu: &Arc<crate::hw::HardwareGpu>, lines: &mut Vec<String>) {
    let kept = g.st().live.lock_timeout(WAIT_NS).and_then(|l| l.as_ref().and_then(|l| l.vcn));
    let Some(v) = kept.filter(|v| v.passed) else {
        lines.push(String::from("backend VCN: no passed VCN stage: the GPU goes on without video; report the journal"));
        return;
    };
    let up = crate::hw::vcn::VcnUp { addrs: v.a, layout: v.layout, start: v.start, doorbell: v.doorbell, mm: v.mm, smu: v.smu, timeout_us: amdgpu_fmt::watch::TimeoutClass::Video.linux_us() };
    if let Err(why) = gpu.start_vcn(Arc::new(LiveChain(g)), up) {
        lines.push(format!("backend VCN: the job half did not start ({why}): the GPU goes on without video; report the journal"));
        return;
    }
    if let Some(j) = gpu.vcn() {
        lines.extend(j.describe());
    }
}
