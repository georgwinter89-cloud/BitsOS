//! GX-7e/GX-7f: M3, GC powered (gx-scout-bringup.md :82-102, :246-249), generic over the device. After the PSP
//! took the firmware and AUTOLOAD_RLC (M2, kept), the SMU stage of Linux's smu_hw_init (amdgpu_smu.c:1457-1480)
//! and the RLC autoload wait of gfx_v11_0 (gfx_v11_0.c:2555-2580), and nothing more:
//!
//! 1. FIRMWARE_FLAGS bit 0 through the SMN window (smu_v13_0_check_fw_status);
//! 2. GC's state before: GFX_IMU_MSG_FLAGS and SMUIO_GFX_MISC_CNTL; the RLC/CP only when SMUIO's
//!    PWR_GFXOFF_STATUS says powered (Linux reads no GC core register before the IMU start either);
//! 3. GetDriverIfVersion (must be 8: the message table is for that interface), GetPmfwVersion;
//! 4. EnableGfxImu(1) WITHOUT waiting (smu_v13_0.c:2303-2309): PMFW starts the IMU, the IMU powers GC, the RLC
//!    bootloader then places the images the PSP put in the TMR. Its answer is what the next pre-poll sees
//!    (the capture: 29.3 ms; the Mini-PC 28.10.: 30244 us);
//! 5. on a start that is not cold, DisallowGfxOff (an earlier kernel may have allowed GFXOFF; Linux's counter
//!    starts at "not allowed", amdgpu_gfx.c's gfx_off_req_count = 1). AllowGfxOff is never sent here;
//! 6. GX-7f, as Linux: CP_STAT == 0 and RLC_RLCS_BOOTLOAD_STATUS bit 31 (the 11.0.1 alias 0x4e7e), polled
//!    directly from EnableGfxImu's answer on (gfx_v11_0.c:2562-2577). Nothing gates these reads: a GC still
//!    powering reads all ones there, and CP_STAT all ones is not 0, so it never looks booted. GFX_IMU_MSG_FLAGS
//!    is only sampled and printed (the 28.10. values 0x80000001, 0x8000000d, 0x8000001f; Linux never reads it:
//!    gc_11_0_0_sh_mask.h has one 32-bit STATUS field); all ones there is the device gone.
//!
//! GX-7e read "bits 2:1 == 2" of the IMU flags as "GC on" (SMUIO's PWR_GFXOFF_STATUS rule, yellow_carp): wrong
//! on the 780M, it skipped every GC read and timed out a boot it never looked at (desk 28.10.).
//!
//! Polling the bootload status before EnableGfxImu times out by design (gx-scout-bringup.md :101-102), so the
//! order is fixed. With PSP loading Linux never writes GFX_IMU_CORE_CTRL (imu_v11_0_start is the direct-load
//! path only, gfx_v11_0.c:4270-4277); BitsOS does not either. `mode2` (GfxDeviceDriverReset with RESET_MODE_2)
//! is kept for the fakes; the live tool refuses it (`MODE2_REFUSED`).

use super::disc::Discovery;
use super::hwid;
use super::psp::Hw;
use super::smu::{Sent, Smu, SmuError, SmuTimeouts};
use crate::ipmod::smu13::{self, msg, resp};
use crate::ipmod::{gfx11, smu13::SMUIO_GFX_MISC_CNTL};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

/// RLC_RLCS_BOOTLOAD_STATUS.BOOTLOAD_COMPLETE (gfx_v11_0.c:2570).
pub const BOOTLOAD_COMPLETE: u32 = 1 << 31;
/// SMUIO_GFX_MISC_CNTL.PWR_GFXOFF_STATUS (bits 2:1) when GC is powered (yellow_carp_ppt.c:45-48, :622-624, the
/// Rembrandt reader; unverified on 13.0.4). It decides only whether GC's core registers are read before
/// EnableGfxImu (here and in the classifier), never the boot wait.
pub const SMUIO_GC_ON: u32 = 2;
/// What PMFW answered on the 780M under Linux (27.10. capture): printed beside ours, not a pass rule (a BIOS
/// update changes it).
pub const LINUX_780M_PMFW: u32 = 0x004c_5700;
/// Linux's budget for the autoload poll (`usec_timeout` rounds of 1 us, gfx_v11_0.c:2562).
pub const LINUX_BOOT_BUDGET_US: u64 = 100_000;
/// The capture: Linux's first autoload poll came 200654 us after EnableGfxImu's write (other IPs' hw_init in
/// between) and found the boot complete. With its budget: a boot Linux accepts is done 300 ms after the send.
pub const LINUX_FIRST_POLL_US: u64 = 200_000;
/// The capture's EnableGfxImu answer time.
pub const LINUX_IMU_ANSWER_US: u64 = 29_300;
/// GFX_IMU_MSG_FLAGS is sampled every this many polls of the boot wait (and at the first).
pub const FLAGS_EVERY: u32 = 16;
/// The flag values the Mini-PC read on 28.10. (recorded only: no meaning is used).
pub const IMU_FLAGS_SEEN: &[(u32, &str)] = &[
    (0x8000_0001, "the value before EnableGfxImu on 28.10."),
    (0x8000_000d, "seen 3 ms after EnableGfxImu's answer on 28.10."),
    (0x8000_001f, "seen after that on 28.10."),
];

fn flags_note(f: u32) -> &'static str {
    IMU_FLAGS_SEEN.iter().find(|(v, _)| *v == f).map_or("a value not seen on 28.10.", |(_, n)| n)
}

/// SMUIO says GC is powered (all ones: unreadable, not powered).
pub fn smuio_gc_on(v: u32) -> bool {
    v != u32::MAX && v >> 1 & 3 == SMUIO_GC_ON
}

/// GC's registers of this stage as BAR5 bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GcAddrs {
    pub imu_flags: u32,
    pub bootload: u32,
    pub cp_stat: u32,
    /// SMUIO_GFX_MISC_CNTL (its meaning on 13.0.4 is unverified, gx-GX-7b.md).
    pub smuio_misc: Option<u32>,
    /// GX-7j3: RLC_CNTL (the unload's RLC stop, gfx_v11_0_rlc_stop).
    pub rlc_cntl: u32,
    /// GX-9b: gfx_v11_cntl_pg's registers.
    pub rlc_safe_mode: u32,
    pub rlc_pg_cntl: u32,
    pub rlc_pg_delay_3: u32,
}

impl GcAddrs {
    pub fn from_discovery(d: &Discovery) -> Option<GcAddrs> {
        let gc = |at: (usize, u32)| d.reg(hwid::GC, 0, at.0, at.1)?.checked_mul(4);
        Some(GcAddrs {
            imu_flags: gc(gfx11::GFX_IMU_MSG_FLAGS)?,
            bootload: gc(gfx11::RLC_RLCS_BOOTLOAD_STATUS_11_0_1)?,
            cp_stat: gc(gfx11::CP_STAT)?,
            smuio_misc: d.reg(hwid::SMUIO, 0, SMUIO_GFX_MISC_CNTL.0, SMUIO_GFX_MISC_CNTL.1).and_then(|x| x.checked_mul(4)),
            rlc_cntl: gc(gfx11::RLC_CNTL)?,
            rlc_safe_mode: gc(gfx11::RLC_SAFE_MODE)?,
            rlc_pg_cntl: gc(gfx11::RLC_PG_CNTL)?,
            rlc_pg_delay_3: gc(gfx11::RLC_PG_DELAY_3)?,
        })
    }

    /// Every register this stage (and the unload) reads.
    pub fn readable(&self) -> Vec<u32> {
        let mut v = alloc::vec![self.imu_flags, self.bootload, self.cp_stat, self.rlc_cntl, self.rlc_safe_mode, self.rlc_pg_cntl];
        v.extend(self.smuio_misc);
        v
    }

    /// The GC registers this owner writes: the unload's RLC_CNTL.RLC_ENABLE_F32 and GX-9b's gfx_v11_cntl_pg.
    pub fn writable(&self) -> [u32; 4] {
        [self.rlc_cntl, self.rlc_safe_mode, self.rlc_pg_cntl, self.rlc_pg_delay_3]
    }
}

/// RLC_CNTL.RLC_ENABLE_F32 (gfx_v11_0_rlc_stop, gfx_v11_0.c:1770-1776).
pub const RLC_ENABLE_F32: u32 = 1;

/// GX-7j3: what the unload told PMFW and did to the RLC.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Unload {
    /// PrepareMp1ForUnload's argument register on OK, or the error.
    pub prepared: Result<u32, SmuError>,
    /// RLC_CNTL as read before the stop and whether the write went through (`None`: it read all ones or did
    /// not read, nothing written).
    pub rlc: Option<(u32, bool)>,
}

impl Unload {
    /// PMFW took the unload: the next GC stage meets it as Linux's next load does.
    pub fn told(&self) -> bool {
        self.prepared.is_ok()
    }

    pub fn text(&self) -> String {
        format!(
            "PrepareMp1ForUnload {}; RLC {}",
            match self.prepared {
                Ok(_) => String::from("answered OK"),
                Err(e) => format!("FAILED {e:?}"),
            },
            match self.rlc {
                Some((v, true)) => format!("stopped (RLC_CNTL {v:#x} -> {:#x})", v & !RLC_ENABLE_F32),
                Some((v, false)) => format!("not stopped (RLC_CNTL {v:#x}, the write was refused)"),
                None => String::from("not touched (GC's power unknown, or RLC_CNTL did not read)"),
            }
        )
    }
}

/// GX-7j3: Linux's GC side of an unload toward PMFW, after the CP, MES and SDMA are halted and before the PSP's
/// DESTROY_TMR/DESTROY_RINGS (the reverse IP order of amdgpu_device_ip_fini_early: gfx hw_fini, then smu_hw_fini,
/// then psp_hw_fini): PrepareMp1ForUnload (smu_v13_0_4_system_features_control, waited), then the RLC stop
/// (smu_disable_dpms, amdgpu_smu.c:1607-1609, done even when the message failed, as Linux does). GFXOFF was never
/// allowed and no GC powergating was enabled, so Linux's DisallowGfxOff and gfx_v11_cntl_pg(false) of its
/// fini_early have nothing to undo; VCN/JPEG were never powered up. No allocation, no log: the Quiesce runs it.
///
/// Why (the Mini-PC 29.10.): after a stop that destroyed the TMR under a running RLC/IMU without telling PMFW,
/// the next load's AUTOLOAD_RLC was followed by a PMFW that never answered again (GetDriverIfVersion, C2PMSG_90
/// stuck at 0) until a cold boot.
///
/// `gc`: `None` when GC's power is unknown (a start after a crash: SMUIO's GFXOFF status reads 0 on the 780M
/// even with GC booted, so nothing tells a gated GC, whose registers must not be touched): the RLC is left.
pub fn unload_quiet(hw: &mut dyn Hw, smu: &smu13::Addrs, gc: Option<&GcAddrs>, t: SmuTimeouts) -> Unload {
    let prepared = super::smu::send_quiet(hw, smu, t, msg::PREPARE_MP1_FOR_UNLOAD, 0);
    let Some(gc) = gc else { return Unload { prepared, rlc: None } };
    let rlc = hw.reg_read(gc.rlc_cntl).filter(|&v| v != u32::MAX).map(|v| (v, hw.reg_write(gc.rlc_cntl, v & !RLC_ENABLE_F32)));
    Unload { prepared, rlc }
}

/// `unload_quiet` with its events (the stop and the start's catch-up; never the Quiesce).
pub fn unload(hw: &mut dyn Hw, smu: &smu13::Addrs, gc: Option<&GcAddrs>, t: SmuTimeouts) -> Unload {
    let u = unload_quiet(hw, smu, gc, t);
    match u.prepared {
        Ok(_) => klog::event!(Info, "gx", "smu.unload", rlc_stopped = u.rlc.is_some_and(|r| r.1); "PrepareMp1ForUnload answered: PMFW knows the GFX driver is gone"),
        Err(e) => klog::event!(Error, "gx", "smu.unload_failed", error = format!("{e:?}").as_str(), rlc_stopped = u.rlc.is_some_and(|r| r.1)),
    }
    u
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GfxTimeouts {
    pub smu: SmuTimeouts,
    /// From EnableGfxImu's write to CP idle and the bootload complete, and at least Linux's budget after its
    /// answer: Linux's deadline as the capture shows it (first poll 200 ms after the send, 100 ms budget).
    pub boot_us: u64,
}

impl GfxTimeouts {
    pub const LINUX: GfxTimeouts = GfxTimeouts { smu: SmuTimeouts::LINUX, boot_us: LINUX_FIRST_POLL_US + LINUX_BOOT_BUDGET_US };
}

/// GC as read at one moment.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GcSnap {
    pub flags: Option<u32>,
    pub smuio: Option<u32>,
    pub bootload: Option<u32>,
    pub cp_stat: Option<u32>,
}

impl GcSnap {
    pub fn booted(&self) -> bool {
        self.bootload.is_some_and(|b| b != u32::MAX && b & BOOTLOAD_COMPLETE != 0)
    }

    pub fn idle(&self) -> bool {
        self.cp_stat == Some(0)
    }

    pub fn text(&self) -> String {
        let h = |v: Option<u32>| v.map_or(String::from("-"), |x| format!("{x:#x}"));
        format!(
            "GFX_IMU_MSG_FLAGS={} (recorded only) SMUIO_GFX_MISC_CNTL={}{} RLC_RLCS_BOOTLOAD_STATUS={} CP_STAT={}",
            h(self.flags),
            h(self.smuio),
            self.smuio.map_or(String::new(), |v| format!(" (PWR_GFXOFF_STATUS {})", v >> 1 & 3)),
            h(self.bootload),
            h(self.cp_stat)
        )
    }
}

/// GC's state before EnableGfxImu: the flags and SMUIO always; the RLC/CP only between two SMUIO readings that
/// say GC is powered (values read while GC may have gated are dropped).
pub fn read_prior(hw: &mut dyn Hw, a: &GcAddrs) -> GcSnap {
    let smuio = |hw: &mut dyn Hw| a.smuio_misc.and_then(|b| hw.reg_read(b));
    let mut s = GcSnap { flags: hw.reg_read(a.imu_flags), smuio: smuio(hw), ..GcSnap::default() };
    if s.smuio.is_some_and(smuio_gc_on) {
        s.bootload = hw.reg_read(a.bootload);
        s.cp_stat = hw.reg_read(a.cp_stat);
        let again = smuio(hw);
        if !again.is_some_and(smuio_gc_on) {
            s.smuio = again;
            s.bootload = None;
            s.cp_stat = None;
        }
    }
    s
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GfxStep {
    FwFlags(u32),
    Prior(GcSnap),
    DriverIf(u32),
    PmfwVersion(u32),
    ImuSent,
    /// EnableGfxImu's answer, seen this long after it was written.
    ImuAnswered { code: u32, after_us: u64 },
    /// DisallowGfxOff answered OK (a start that was not cold).
    GfxOffDisallowed,
    /// GFX_IMU_MSG_FLAGS read a new value this long after EnableGfxImu's write (recorded only).
    ImuFlags { after_us: u64, flags: u32 },
    /// CP idle and the bootload complete, after `polls` reads of both.
    Booted { after_us: u64, snap: GcSnap, polls: u32 },
    /// Mode-2 reset answered.
    Mode2 { after: GcSnap },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GfxError {
    Smu(SmuError),
    /// PMFW's driver interface is not the one the message table is for: nothing but the two queries was sent.
    Interface { got: u32 },
    /// GFX_IMU_MSG_FLAGS read all ones: the device is gone.
    Gone { byte: u32 },
    /// The RLC bootload or the CP never finished within Linux's deadline (gx-roadmap.md risk 1).
    BootTimeout { last: GcSnap },
}

impl GfxError {
    pub fn errno(&self) -> i32 {
        match self {
            GfxError::Smu(e) => e.errno(),
            GfxError::Interface { .. } => 22,
            GfxError::Gone { .. } => 19,
            GfxError::BootTimeout { .. } => 62,
        }
    }

    /// PMFW may still be busy with our message, never answers, the device is gone, or GC was left booting (mode
    /// 2 is refused on 13.0.4, `MODE2_REFUSED`): a reboot.
    pub fn needs_reboot(&self) -> bool {
        matches!(self, GfxError::Smu(e) if e.needs_reboot()) || matches!(self, GfxError::Gone { .. } | GfxError::BootTimeout { .. })
    }

    pub fn advice(&self) -> &'static str {
        match self {
            GfxError::Smu(SmuError::PrePoll { .. } | SmuError::Timeout { .. }) => ": PMFW does not answer (its mailbox is stuck): reboot",
            GfxError::Smu(SmuError::NotRunning { .. }) => ": PMFW does not run: nothing sent",
            GfxError::Smu(SmuError::Answer { id: msg::ENABLE_GFX_IMU, .. }) => ": PMFW refused EnableGfxImu: GC may be half started; TMR and ring stay; reboot",
            GfxError::Smu(SmuError::Forbidden { .. }) => ": a message not on BitsOS's list: nothing written (a bug)",
            GfxError::Smu(_) => ": PMFW refused a message: nothing more sent",
            GfxError::Interface { .. } => ": PMFW's driver interface is not 8: no EnableGfxImu sent; check the 13.0.4 message table first",
            GfxError::Gone { .. } => ": GC's flags read all ones: the device is gone; reboot",
            GfxError::BootTimeout { .. } => ": the RLC autoload never completed (gx-roadmap risk 1): compare the W lines with the capture; TMR and ring stay; reboot",
        }
    }
}

impl From<SmuError> for GfxError {
    fn from(e: SmuError) -> GfxError {
        GfxError::Smu(e)
    }
}

/// What the stage did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GfxReport {
    pub steps: Vec<GfxStep>,
    pub sent: Vec<Sent>,
    pub prior: Option<GcSnap>,
    pub last: GcSnap,
    pub error: Option<GfxError>,
    /// EnableGfxImu (or the mode-2 reset) reached PMFW: GC may now be in a state only that message explains.
    pub gc_touched: bool,
    pub stale: bool,
    /// The classifier knew GC booted before this stage (after Linux, after a BitsOS stop with GC booted).
    pub known_booted: bool,
    pub fw_flags: Option<u32>,
    pub driver_if: Option<u32>,
    pub pmfw: Option<u32>,
    /// Times from EnableGfxImu's write (us).
    pub answered_us: Option<u64>,
    pub booted_us: Option<u64>,
    /// Reads of CP_STAT and RLC_RLCS_BOOTLOAD_STATUS (one each per poll) from EnableGfxImu's answer on.
    pub polls: u32,
    /// Each new GFX_IMU_MSG_FLAGS value with its time from EnableGfxImu's write.
    pub flags_seen: Vec<(u64, u32)>,
}

impl GfxReport {
    /// GC was booted before EnableGfxImu (an earlier kernel's boot): the stage then shows that the load path
    /// does not break a booted GC, not that our firmware booted it (that takes a cold start).
    pub fn booted_before(&self) -> bool {
        self.known_booted || self.prior.is_some_and(|p| p.booted())
    }

    /// Records a flags reading (all ones: the device is gone).
    fn flags(&mut self, f: Option<u32>, after_us: u64, byte: u32) -> Result<(), GfxError> {
        let Some(f) = f else { return Ok(()) };
        if f == u32::MAX {
            return Err(GfxError::Gone { byte });
        }
        if self.flags_seen.last().is_none_or(|&(_, v)| v != f) {
            self.flags_seen.push((after_us, f));
            self.steps.push(GfxStep::ImuFlags { after_us, flags: f });
        }
        self.last.flags = Some(f);
        Ok(())
    }
}

/// The SMU/GC stage (M3). `stale`: the start was not cold (DisallowGfxOff is then sent too); `known_booted`: the
/// classifier knew GC booted before (decision D6's note).
pub fn power_up(hw: &mut dyn Hw, smu: smu13::Addrs, gc: &GcAddrs, stale: bool, known_booted: bool, t: &GfxTimeouts) -> GfxReport {
    let mut s = Smu::new(smu, t.smu);
    let mut r = GfxReport { stale, known_booted, ..GfxReport::default() };
    if let Err(e) = up(hw, &mut s, gc, stale, t, &mut r) {
        klog::event!(Error, "gx", "gfx.failed", error = format!("{e:?}").as_str(), touched = r.gc_touched, polls = r.polls);
        r.error = Some(e);
    } else {
        klog::event!(Info, "gx", "gfx.booted", booted_us = r.booted_us.unwrap_or(0), answered_us = r.answered_us.unwrap_or(0), polls = r.polls);
    }
    r.sent = s.sent;
    r
}

fn up(hw: &mut dyn Hw, s: &mut Smu, gc: &GcAddrs, stale: bool, t: &GfxTimeouts, r: &mut GfxReport) -> Result<(), GfxError> {
    let f = s.fw_flags(hw)?;
    r.fw_flags = Some(f);
    r.steps.push(GfxStep::FwFlags(f));
    let prior = read_prior(hw, gc);
    if prior.flags == Some(u32::MAX) {
        return Err(GfxError::Gone { byte: gc.imu_flags });
    }
    r.prior = Some(prior);
    r.last = prior;
    r.steps.push(GfxStep::Prior(prior));
    // smu_cmn_get_smc_version's order (the capture: 0x03, then 0x02).
    let ifv = s.send(hw, msg::GET_DRIVER_IF_VERSION, 0, true)?;
    r.driver_if = Some(ifv);
    r.steps.push(GfxStep::DriverIf(ifv));
    if ifv != smu13::DRIVER_IF_VERSION {
        klog::event!(Error, "gx", "smu.interface", got = ifv, want = smu13::DRIVER_IF_VERSION; "stopped before EnableGfxImu");
        return Err(GfxError::Interface { got: ifv });
    }
    let v = s.send(hw, msg::GET_PMFW_VERSION, 0, true)?;
    r.pmfw = Some(v);
    r.steps.push(GfxStep::PmfwVersion(v));
    if v != LINUX_780M_PMFW {
        klog::event!(Info, "gx", "smu.version", got = klog::Hex(u64::from(v)), capture = klog::Hex(u64::from(LINUX_780M_PMFW)); "another PMFW than the capture's");
    }
    // Param 1: PMFW also enables its GFXOFF feature (smu_v13_0.c:2307); entering it still needs AllowGfxOff.
    s.send(hw, msg::ENABLE_GFX_IMU, 1, false)?;
    r.gc_touched = true;
    r.steps.push(GfxStep::ImuSent);
    let t0 = s.pending().map_or_else(|| hw.now_us(), |p| p.at_us);
    // Its answer is what the next message's pre-poll waits for: wait for it here, the flags sampled meanwhile
    // (recorded only).
    let tries = t.smu.pre_poll_us / 10 + 2;
    let mut answered = None;
    for _ in 0..tries {
        let code = hw.reg_read(s.a.resp).ok_or(SmuError::Read { byte: s.a.resp })?;
        let fl = hw.reg_read(gc.imu_flags);
        r.flags(fl, hw.now_us().saturating_sub(t0), gc.imu_flags)?;
        if code != resp::NONE {
            let c = s.pre_poll(hw)?; // records the answer on EnableGfxImu; non-OK is an error
            answered = Some((c, hw.now_us().saturating_sub(t0)));
            break;
        }
        let el = hw.now_us().saturating_sub(t0);
        if el >= t.smu.pre_poll_us {
            break;
        }
        hw.delay_us(super::smu::step_us(el).min(t.smu.pre_poll_us - el).max(1));
    }
    let Some((code, after_us)) = answered else {
        klog::event!(Error, "gx", "smu.imu_unanswered", waited_us = t.smu.pre_poll_us; "EnableGfxImu never answered");
        return Err(GfxError::Smu(SmuError::PrePoll { last: msg::ENABLE_GFX_IMU }));
    };
    r.answered_us = Some(after_us);
    r.steps.push(GfxStep::ImuAnswered { code, after_us });
    if stale {
        s.send(hw, msg::DISALLOW_GFXOFF, 0, true)?;
        r.steps.push(GfxStep::GfxOffDisallowed);
    }
    // Linux's autoload wait (gfx_v11_0.c:2562-2577): CP_STAT, then the bootload status, until CP_STAT is 0 and
    // BOOTLOAD_COMPLETE is set. Nothing gates the reads; the deadline is Linux's as the capture shows it.
    let deadline = t.boot_us.max(after_us.saturating_add(LINUX_BOOT_BUDGET_US));
    let tries = deadline / 10 + 2;
    for _ in 0..tries {
        let cp = hw.reg_read(gc.cp_stat);
        let bl = hw.reg_read(gc.bootload);
        if r.polls % FLAGS_EVERY == 0 {
            let fl = hw.reg_read(gc.imu_flags);
            r.flags(fl, hw.now_us().saturating_sub(t0), gc.imu_flags)?;
        }
        r.polls = r.polls.saturating_add(1);
        r.last.cp_stat = cp;
        r.last.bootload = bl;
        let el = hw.now_us().saturating_sub(t0);
        if r.last.idle() && r.last.booted() {
            r.booted_us = Some(el);
            r.steps.push(GfxStep::Booted { after_us: el, snap: r.last, polls: r.polls });
            return Ok(());
        }
        if el >= deadline {
            break;
        }
        hw.delay_us(super::smu::step_us(el).min(deadline - el).max(1));
    }
    klog::event!(Error, "gx", "gfx.boot_timeout", bootload = klog::Hex(u64::from(r.last.bootload.unwrap_or(0))), cp_stat = klog::Hex(u64::from(r.last.cp_stat.unwrap_or(0))),
        flags = klog::Hex(u64::from(r.last.flags.unwrap_or(0))), waited_us = deadline, polls = r.polls);
    Err(GfxError::BootTimeout { last: r.last })
}

/// Why the live tool refuses the mode-2 reset (GX-7f, desk 28.10.: GfxDeviceDriverReset(2) after a failed GC
/// stage got no answer and the Mini-PC hung until the watchdog). Linux sends the same message with no extra
/// argument (smu_v13_0_4_ppt.c:1079-1083, soc21.c:381-383 picks mode 2 for MP1 13.0.4), but only inside its
/// whole reset: every IP suspended first (amdgpu_device.c:4772 amdgpu_device_ip_suspend: PG/CG ungated and so
/// GFXOFF disallowed, the CP and RLC halted by gfx hw_fini, the PSP's rings stopped), the TMR and firmware it
/// loads again afterwards (psp resume, AUTOLOAD_RLC, the autoload wait). BitsOS has none of that around it.
pub const MODE2_REFUSED: &str = "mode 2 is refused on SMU 13.0.4: Linux sends GfxDeviceDriverReset(2) only between a full IP suspend (GFXOFF off, CP/RLC halted, PSP rings stopped) and a full firmware reload, which BitsOS does not have; on 28.10. it went unanswered and the Mini-PC hung. The recovery after a failed GC stage is a reboot";

/// A mode-2 reset of GC (GfxDeviceDriverReset, RESET_MODE_2, waited as Linux does, smu_v13_0_4_ppt.c:1079-1083),
/// with GC's state before and after. Only the fakes run it (the live tool refuses, `MODE2_REFUSED`). No answer
/// within the SMU wait: `Smu(Timeout)`, a reboot, and nothing is read or written after it.
pub fn mode2(hw: &mut dyn Hw, smu: smu13::Addrs, gc: &GcAddrs, t: &GfxTimeouts) -> GfxReport {
    let mut s = Smu::new(smu, t.smu);
    let mut r = GfxReport::default();
    let res = (|| -> Result<(), GfxError> {
        let f = s.fw_flags(hw)?;
        r.fw_flags = Some(f);
        r.steps.push(GfxStep::FwFlags(f));
        let prior = read_prior(hw, gc);
        r.prior = Some(prior);
        r.steps.push(GfxStep::Prior(prior));
        klog::event!(Warn, "gx", "gfx.mode2", flags = klog::Hex(u64::from(prior.flags.unwrap_or(0))); "GC mode-2 reset (recovery)");
        r.gc_touched = true;
        s.send(hw, msg::GFX_DEVICE_DRIVER_RESET, smu13::RESET_MODE_2, true)?;
        let after = read_prior(hw, gc);
        r.last = after;
        r.steps.push(GfxStep::Mode2 { after });
        Ok(())
    })();
    r.error = res.err();
    r.sent = s.sent;
    r
}

/// The bootload check's name when GC was on and booted before EnableGfxImu.
pub const BOOT_NOT_PROVEN: &str = "M3 RLC bootload complete (GC booted before: not proven)";

/// One milestone check (as `pspbringup::Check`).
pub struct GfxCheck {
    pub name: &'static str,
    pub pass: bool,
    pub why: String,
}

/// The M3 pass rules (gx-scout-bringup.md :246-249).
pub fn checks(r: &GfxReport) -> Vec<GfxCheck> {
    let mut v = Vec::new();
    let h = |x: Option<u32>| x.map_or(String::from("unread"), |y| format!("{y:#x}"));
    v.push(GfxCheck { name: "M3 PMFW runs", pass: r.fw_flags.is_some_and(|f| f & smu13::FIRMWARE_FLAGS_UP != 0), why: format!("FIRMWARE_FLAGS {}", h(r.fw_flags)) });
    v.push(GfxCheck { name: "M3 driver interface", pass: r.driver_if == Some(smu13::DRIVER_IF_VERSION), why: format!("{} (table for {})", h(r.driver_if), smu13::DRIVER_IF_VERSION) });
    v.push(GfxCheck { name: "M3 PMFW version", pass: r.pmfw.is_some(), why: format!("{} (Linux on the 780M: {LINUX_780M_PMFW:#x})", h(r.pmfw)) });
    let imu = r.sent.iter().find(|x| x.id == msg::ENABLE_GFX_IMU);
    v.push(GfxCheck {
        name: "M3 EnableGfxImu answered",
        pass: imu.is_some_and(|x| x.code == Some(resp::OK)),
        why: match (imu, r.answered_us) {
            (None, _) => String::from("not sent"),
            (Some(x), a) => format!("argument {}, answer {} after {} us (the capture: {LINUX_IMU_ANSWER_US} us), not waited for at the send", x.arg, h(x.code), a.map_or(String::from("-"), |u| format!("{u}"))),
        },
    });
    let booted = r.steps.iter().find_map(|s| if let GfxStep::Booted { snap, .. } = s { Some(*snap) } else { None });
    let polls = format!("{} polls from EnableGfxImu's answer", r.polls);
    v.push(GfxCheck { name: "M3 CP_STAT 0", pass: booted.is_some_and(|b| b.idle()), why: format!("CP_STAT {} ({polls})", h(booted.map_or(r.last.cp_stat, |b| b.cp_stat))) });
    // A GC booted before EnableGfxImu (decision D6) passes, but under a name that says it proves no boot of ours.
    v.push(GfxCheck {
        name: if r.booted_before() { BOOT_NOT_PROVEN } else { "M3 RLC bootload complete" },
        pass: booted.is_some_and(|b| b.booted()),
        why: format!("RLC_RLCS_BOOTLOAD_STATUS {} (bit 31) after {} us", h(booted.map_or(r.last.bootload, |b| b.bootload)), r.booted_us.map_or(String::from("-"), |u| format!("{u}"))),
    });
    let allow = r.sent.iter().any(|x| x.id == msg::ALLOW_GFXOFF);
    let ids: Vec<String> = r.sent.iter().map(|x| format!("{:#x} {}", x.id, smu13::msg_name(x.id))).collect();
    v.push(GfxCheck { name: "M3 GFXOFF not allowed", pass: !allow && r.sent.iter().all(|x| super::smu::allowed(x.id, x.arg)), why: format!("messages sent: {}", ids.join(", ")) });
    v
}

/// The stage's lines for the answer.
pub fn render(r: &GfxReport) -> Vec<String> {
    let mut v = Vec::new();
    for (i, s) in r.steps.iter().enumerate() {
        v.push(format!(
            "gfx step {}: {}",
            i + 1,
            match s {
                GfxStep::FwFlags(f) => format!("FIRMWARE_FLAGS {f:#x} (bit 0: PMFW runs)"),
                GfxStep::Prior(p) => format!("GC before: {}", p.text()),
                GfxStep::DriverIf(x) => format!("GetDriverIfVersion: {x}"),
                GfxStep::PmfwVersion(x) => format!("GetPmfwVersion: {x:#x}"),
                GfxStep::ImuSent => String::from("EnableGfxImu(1) sent, not waited for"),
                GfxStep::ImuAnswered { code, after_us } => format!("EnableGfxImu answered {code:#x} ({}) after {after_us} us", smu13::resp_name(*code)),
                GfxStep::GfxOffDisallowed => String::from("DisallowGfxOff answered OK (the start was not cold)"),
                GfxStep::ImuFlags { after_us, flags } => format!("GFX_IMU_MSG_FLAGS {flags:#x} after {after_us} us (recorded only, nothing waits on it: {})", flags_note(*flags)),
                GfxStep::Booted { after_us, snap, polls } => format!("GC booted after {after_us} us ({polls} polls of CP_STAT and RLC_RLCS_BOOTLOAD_STATUS): {}", snap.text()),
                GfxStep::Mode2 { after } => format!("mode-2 reset answered; GC after: {}", after.text()),
            }
        ));
    }
    for x in &r.sent {
        v.push(format!(
            "smu: {:#x} {} arg {:#x} -> {}{}",
            x.id,
            smu13::msg_name(x.id),
            x.arg,
            x.code.map_or(String::from("no answer seen"), |c| format!("{c:#x} {}", smu13::resp_name(c))),
            match (x.out, x.answered_us) {
                (Some(o), Some(a)) => format!(", argument register {o:#x}, after {} us", a.saturating_sub(x.at_us)),
                _ => String::new(),
            }
        ));
    }
    if r.prior.is_some() && r.error.is_none() && !r.steps.iter().any(|s| matches!(s, GfxStep::Mode2 { .. })) {
        v.push(if r.booted_before() {
            String::from("note: GC was on and booted before EnableGfxImu (an earlier kernel): this run shows the load path keeps it booted; proving our boot takes a cold start")
        } else {
            String::from("note: GC was not booted before EnableGfxImu: this boot is ours")
        });
        if let Some(b) = r.booted_us {
            let a = r.answered_us.unwrap_or(0);
            v.push(format!(
                "timing (judged on the Mini-PC only): booted {b} us after EnableGfxImu, {} us after its answer, {} polls; Linux: first poll {LINUX_FIRST_POLL_US} us after the send, budget {LINUX_BOOT_BUDGET_US} us: {}",
                b.saturating_sub(a),
                r.polls,
                if b <= LINUX_FIRST_POLL_US + LINUX_BOOT_BUDGET_US { "within" } else { "over" }
            ));
        }
    }
    let seen: Vec<String> = r.flags_seen.iter().map(|(t, f)| format!("{f:#x}@{t}us")).collect();
    v.push(format!("gfx: last GC state {}; {} polls of CP_STAT/RLC_RLCS_BOOTLOAD_STATUS; IMU flags seen: {}", r.last.text(), r.polls, if seen.is_empty() { String::from("-") } else { seen.join(" -> ") }));
    if let Some(e) = &r.error {
        v.push(format!("gfx error: {e:?} (errno {}){}", e.errno(), e.advice()));
    }
    v
}
