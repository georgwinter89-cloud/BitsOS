//! GX-7c: a fake GC 11.0.1 power and boot model (GFXOFF, IMU, RLC autoload) for the bring-up tests.
//!
//! With PSP loading on this APU the driver never touches the IMU itself (gx-scout-bringup.md section 5): PMFW
//! starts the IMU on EnableGfxImu, the IMU powers GC, and the RLC bootloader places the images the PSP put in
//! the TMR once AUTOLOAD_RLC arrived. The bootload completes only when both happened, in either order, and every
//! image it needs was loaded; otherwise never (the scout's "autoload never completes").
//!
//! GFXOFF: while GC is not powered every GC register (GC's segments, SDMA0 and GFXHUB live there) reads all ones
//! and loses its writes. A write then, or a read before EnableGfxImu reached PMFW, is recorded (`off_access`), so
//! a missing guard fails a test; reads after EnableGfxImu while GC still powers up are what Linux's autoload poll
//! does (gfx_v11_0.c:2562-2577) and are only counted (`powering_reads`). SMUIO_GFX_MISC_CNTL (outside GC) keeps
//! the state in bits 2:1 (0 off, 1 leaving, 2 on, 3 entering: yellow_carp's reader, unverified on 13.0.4).
//!
//! GFX_IMU_MSG_FLAGS (GX-7f): the values the real 780M read on 28.10. (0x80000001 before EnableGfxImu,
//! 0x8000000d 3088 us after its answer, then 0x8000001f), with no meaning BitsOS relies on (Linux never reads
//! it: one 32-bit STATUS field, gc_11_0_0_sh_mask.h). The fake gives 0x8000000d while powered and not booted and
//! 0x8000001f once booted (an assumption about when 0x1f appears). EnableGfxImu's answer came after 30244 us, GC
//! powered 3088 us later (`wake_us` from the IMU start); the boot time is an assumption (`boot_us`: the capture
//! only shows it done by Linux's first poll, 200 ms after the send). Facts from the capture: bootload status
//! 0xc000001f and CP_STAT 0 once booted.

use super::disc::Discovery;
use super::hwid;
use crate::gpucore::ucode::UcodeId;
use crate::ipmod::{gfx11, smu13};
use alloc::collections::BTreeMap;
use alloc::vec::Vec;

/// GC 11.0.0's register extents per segment in dwords (gc_11_0_0_offset.h: highest offset + 1).
pub const GC_EXTENT: [u32; 2] = [0x2120, 0x5f92];
/// What the bootload status read once booted (the capture, after the load).
pub const BOOTLOAD_DONE: u32 = 0xc000_001f;
/// CP_STAT before the RLC booted the CP (assumption: busy).
pub const CP_STAT_BOOTING: u32 = 0x8000_0000;
/// GFX_IMU_MSG_FLAGS as the real 780M read it (28.10.): before EnableGfxImu, powering, later.
pub const IMU_FLAGS_COLD: u32 = 0x8000_0001;
pub const IMU_FLAGS_POWERED: u32 = 0x8000_000d;
pub const IMU_FLAGS_LATER: u32 = 0x8000_001f;
/// EnableGfxImu's answer on the real 780M (28.10.) plus the 3088 us until the flags changed.
pub const REAL_WAKE_US: u64 = 30_244 + 3_088;
/// CP_ME_CNTL with ME, PFP and CE halted; CP_MES_CNTL with MES halted (reset values as Linux finds them:
/// assumption, Linux writes them before reading).
pub const ME_HALTED: u32 = 0x1500_0000;
pub const MES_HALTED: u32 = 0x4000_0000;

/// The images the RLC autoload of GC 11.0.1 places (all of BitsOS's list but VCN; ucode.rs's PSP types).
pub const AUTOLOAD_NEEDS: &[UcodeId] = &[
    UcodeId::SdmaTh0,
    UcodeId::SdmaTh1,
    UcodeId::CpPfp,
    UcodeId::CpMe,
    UcodeId::CpMec1,
    UcodeId::CpMes,
    UcodeId::CpMesData,
    UcodeId::ImuI,
    UcodeId::ImuD,
    UcodeId::RlcRestoreListGpmMem,
    UcodeId::RlcRestoreListSrmMem,
    UcodeId::RlcIram,
    UcodeId::RlcDram,
    UcodeId::RlcP,
    UcodeId::RlcG,
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Power {
    Off,
    Leaving { until: u64 },
    On,
    Entering { until: u64 },
}

impl Power {
    /// PWR_GFXOFF_STATUS / IMU state encoding.
    pub fn status(self) -> u32 {
        match self {
            Power::Off => 0,
            Power::Leaving { .. } => 1,
            Power::On => 2,
            Power::Entering { .. } => 3,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GcHostile {
    None,
    /// The RLC bootload never completes.
    AutoloadNever,
    /// GC never leaves GFXOFF.
    StuckOff,
    /// GX-7e: the bootload completes, the CP never reads idle.
    CpBusy,
    /// GX-7f: GFX_IMU_MSG_FLAGS reads this value always (the flags must decide nothing).
    Flags(u32),
    /// GX-7f: GFX_IMU_MSG_FLAGS reads all ones once the IMU started (the device went away).
    GoneAfterImu,
}

pub struct GcFake {
    /// GC's register ranges as BAR5 bytes [lo, hi).
    pub ranges: Vec<(u32, u32)>,
    pub smuio_misc: u32,
    pub imu_flags: u32,
    pub bootload: u32,
    pub cp_stat: u32,
    pub rlc_cntl: u32,
    pub cp_me_cntl: u32,
    pub cp_mes_cntl: u32,
    pub power: Power,
    pub hostile: GcHostile,
    pub imu_started: bool,
    /// AUTOLOAD_RLC arrived; the needed images that were missing then.
    pub autoload: Option<Vec<UcodeId>>,
    pub boot_due: Option<u64>,
    pub booted: bool,
    pub gfxoff_allowed: bool,
    /// Every GC register write while GC was not powered, and every read before the IMU start: (byte, write).
    pub off_access: Vec<(u32, bool)>,
    /// GC register reads after the IMU start while GC was not yet powered (Linux's autoload poll does them).
    pub powering_reads: u32,
    /// Mode-2 resets taken.
    pub resets: u32,
    /// Leaving/entering GFXOFF and the boot after both preconditions, in us (assumptions).
    pub wake_us: u64,
    pub sleep_us: u64,
    pub boot_us: u64,
    regs: BTreeMap<u32, u32>,
}

impl GcFake {
    /// Cold: GC power-gated, the IMU not started, nothing loaded.
    pub fn new(d: &Discovery) -> GcFake {
        let b = |at: (usize, u32)| d.reg(hwid::GC, 0, at.0, at.1).and_then(|x| x.checked_mul(4)).unwrap_or(u32::MAX);
        let ranges = (0..2)
            .filter_map(|seg| {
                let lo = d.reg(hwid::GC, 0, seg, 0)?.checked_mul(4)?;
                Some((lo, lo.checked_add(GC_EXTENT[seg].checked_mul(4)?)?))
            })
            .collect();
        let smuio = d.reg(hwid::SMUIO, 0, smu13::SMUIO_GFX_MISC_CNTL.0, smu13::SMUIO_GFX_MISC_CNTL.1).and_then(|x| x.checked_mul(4)).unwrap_or(u32::MAX);
        GcFake {
            ranges,
            smuio_misc: smuio,
            imu_flags: b(gfx11::GFX_IMU_MSG_FLAGS),
            bootload: b(gfx11::RLC_RLCS_BOOTLOAD_STATUS_11_0_1),
            cp_stat: b(gfx11::CP_STAT),
            rlc_cntl: b(gfx11::RLC_CNTL),
            cp_me_cntl: b(gfx11::CP_ME_CNTL),
            cp_mes_cntl: b(gfx11::CP_MES_CNTL),
            power: Power::Off,
            hostile: GcHostile::None,
            imu_started: false,
            autoload: None,
            boot_due: None,
            booted: false,
            gfxoff_allowed: false,
            off_access: Vec::new(),
            powering_reads: 0,
            resets: 0,
            wake_us: REAL_WAKE_US,
            sleep_us: 200,
            boot_us: 60_000,
            regs: BTreeMap::new(),
        }
    }

    pub fn in_gc(&self, byte: u32) -> bool {
        self.ranges.iter().any(|&(lo, hi)| byte >= lo && byte < hi)
    }

    pub fn owns(&self, byte: u32) -> bool {
        byte == self.smuio_misc || self.in_gc(byte)
    }

    /// GX-7j: a GC register's value as the fake starts with it (powered or not).
    pub fn set(&mut self, byte: u32, value: u32) {
        if self.in_gc(byte) {
            self.regs.insert(byte, value);
        }
    }

    /// GC as a clean BitsOS stop or a Linux load left it: powered, booted, GFXOFF not allowed.
    pub fn set_booted(&mut self) {
        self.power = Power::On;
        self.imu_started = true;
        self.autoload = Some(Vec::new());
        self.booted = true;
        self.boot_due = None;
    }

    /// GC powered but the RLC never finished (a crash between EnableGfxImu and the boot).
    pub fn set_on_unbooted(&mut self) {
        self.power = Power::On;
        self.imu_started = true;
        self.booted = false;
        self.autoload = None;
    }

    /// GX-7j3: RLC_CNTL.RLC_ENABLE_F32 was cleared by a stop (and no autoload started the RLC since).
    pub fn rlc_stopped(&self) -> bool {
        self.regs.get(&self.rlc_cntl).is_some_and(|v| v & 1 == 0)
    }

    pub fn tick(&mut self, now: u64) {
        match self.power {
            Power::Leaving { until } if until <= now && self.hostile != GcHostile::StuckOff => self.power = Power::On,
            Power::Entering { until } if until <= now => self.power = Power::Off,
            _ => {}
        }
        if let Some(t) = self.boot_due
            && t <= now
            && self.power == Power::On
        {
            self.boot_due = None;
            self.booted = true;
        }
    }

    fn wake(&mut self, now: u64) {
        if matches!(self.power, Power::Off | Power::Entering { .. }) {
            self.power = Power::Leaving { until: now.saturating_add(self.wake_us) };
        }
    }

    fn arm_boot(&mut self, now: u64) {
        let ready = self.imu_started && self.autoload.as_ref().is_some_and(|m| m.is_empty());
        if ready && !self.booted && self.boot_due.is_none() && self.hostile != GcHostile::AutoloadNever {
            self.boot_due = Some(now.saturating_add(self.boot_us));
        }
    }

    /// PMFW took EnableGfxImu / Allow / Disallow / a mode-2 reset.
    pub fn smu_event(&mut self, ev: super::smufake::SmuEvent, now: u64) {
        use super::smufake::SmuEvent;
        match ev {
            SmuEvent::ImuStart => {
                self.imu_started = true;
                self.wake(now);
                self.arm_boot(now);
            }
            SmuEvent::AllowGfxOff => {
                self.gfxoff_allowed = true;
                if self.power == Power::On {
                    self.power = Power::Entering { until: now.saturating_add(self.sleep_us) };
                }
            }
            SmuEvent::DisallowGfxOff => {
                self.gfxoff_allowed = false;
                if self.imu_started {
                    self.wake(now);
                }
            }
            SmuEvent::GcReset => {
                self.resets += 1;
                // GX-7j3: the reset takes the IMU with GC (the next load is not a warm one).
                self.imu_started = false;
                self.booted = false;
                self.boot_due = None;
                self.autoload = None;
            }
            // GX-7j3: unknown what PMFW does to GC's power on it; the fake leaves GC as it is.
            SmuEvent::Unload => {}
        }
    }

    /// The PSP took AUTOLOAD_RLC with these images in its TMR.
    pub fn autoload_rlc(&mut self, loaded: &[u32], now: u64) {
        let missing = AUTOLOAD_NEEDS.iter().copied().filter(|u| !loaded.contains(&u.fw_type())).collect();
        self.autoload = Some(missing);
        // GX-7j3: the bootloader starts the RLC again (an earlier stop's RLC_CNTL write no longer holds).
        self.regs.remove(&self.rlc_cntl);
        self.arm_boot(now);
    }

    pub fn read(&mut self, byte: u32) -> Option<u32> {
        if byte == self.smuio_misc {
            return Some(self.power.status() << 1);
        }
        if !self.in_gc(byte) {
            return None;
        }
        if byte == self.imu_flags {
            match self.hostile {
                GcHostile::Flags(v) => return Some(v),
                GcHostile::GoneAfterImu if self.imu_started => return Some(u32::MAX),
                _ => {}
            }
            return Some(match self.power {
                Power::On if !self.booted => IMU_FLAGS_POWERED,
                Power::On | Power::Entering { .. } => IMU_FLAGS_LATER,
                Power::Off | Power::Leaving { .. } => IMU_FLAGS_COLD,
            });
        }
        if self.power != Power::On {
            if self.imu_started {
                self.powering_reads += 1;
            } else {
                self.off_access.push((byte, false));
            }
            return Some(u32::MAX);
        }
        Some(match byte {
            b if b == self.bootload => {
                if self.booted {
                    BOOTLOAD_DONE
                } else {
                    0
                }
            }
            b if b == self.cp_stat => {
                if self.booted && self.hostile != GcHostile::CpBusy {
                    0
                } else {
                    CP_STAT_BOOTING
                }
            }
            b if b == self.rlc_cntl => self.regs.get(&b).copied().unwrap_or(u32::from(self.booted)),
            b if b == self.cp_me_cntl => self.regs.get(&b).copied().unwrap_or(ME_HALTED),
            b if b == self.cp_mes_cntl => self.regs.get(&b).copied().unwrap_or(MES_HALTED),
            b => self.regs.get(&b).copied().unwrap_or(0),
        })
    }

    /// `false`: not GC's.
    pub fn write(&mut self, byte: u32, value: u32) -> bool {
        if byte == self.smuio_misc {
            return true; // status, read-only
        }
        if !self.in_gc(byte) {
            return false;
        }
        if self.power != Power::On {
            self.off_access.push((byte, true));
            return true; // lost
        }
        self.regs.insert(byte, value);
        true
    }
}
