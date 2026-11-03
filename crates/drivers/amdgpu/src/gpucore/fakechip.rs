//! GX-7c: the whole-chip side of the fakes. `discovery_780m` is the 780M's discovery table as far as the fakes
//! use it (the bases Linux's ip_discovery sysfs showed on 27.10., tests/data/minipc-780m-ip.txt; a host test
//! checks them against the real blob), so a fake has every block where the real chip has it and the state
//! classifier (`classify`) and the scanout watcher read the fake at the real addresses.
//!
//! The start states (what GX-7b's classifier can see): `cold_image` is the firmware's state the capture read
//! before amdgpu (MMHUB, IH, NBIO from minipc-780m-state.txt; MEMSIZE, FB location and offset derived from the
//! fake's carve-out, so any size works), `start_as` turns a cold fake into what an earlier kernel left: after a
//! Linux load (rings, TMR, IH, GART, GC booted), after a clean BitsOS stop (GC on, rings and TMR destroyed: the
//! desk's Reload rule), after a crash (a ring left with frames, a PMFW message never answered, GC on without
//! RLC, a TMR left), or with the PSP's secure OS down.

use super::disc::{Discovery, IpEntry};
use super::hwid;
use super::pspfake::{FakeGpu, TOC_TMR};
use super::smufake::{SmuHostile, msg};
use crate::ip::IpVersion;
use crate::ipmod::{gmc11, ih6, nbio7, v};
use alloc::vec;
use alloc::vec::Vec;

/// (hw id, version, segment bases in dwords) of the 780M's blocks the fakes model.
const IPS_780M: &[(u16, IpVersion, &[u32])] = &[
    (hwid::MP1, v(13, 0, 4), &[0x4000, 0x16000, 0xDC_0000, 0xE0_0000, 0xE4_0000, 0x243_FC00]),
    (hwid::SMUIO, v(13, 0, 7), &[0x16800, 0x16A00, 0x44_0000, 0x240_1000]),
    (hwid::GC, v(11, 0, 1), &[0x1260, 0xA000, 0x240_2C00]),
    (hwid::MMHUB, v(3, 0, 1), &[0x13200, 0x1A000, 0x240_8800]),
    (hwid::OSSSYS, v(6, 0, 1), &[0x10A0, 0x240_A000]),
    (hwid::HDP, v(5, 2, 1), &[0xF20, 0x240_A400]),
    (hwid::SDMA0, v(6, 0, 1), &[0x1260, 0xA000, 0x240_2C00]),
    (hwid::NBIF, v(7, 7, 0), &[0x0, 0x14, 0xD20, 0x1_0400, 0x241_B000, 0x404_0000]),
    (hwid::MP0, v(13, 0, 4), &[0x4000, 0x16000, 0xDC_0000, 0xE0_0000, 0xE4_0000, 0x243_FC00]),
    (hwid::DMU, v(3, 1, 4), &[0x12, 0xC0, 0x34C0, 0x9000, 0x240_3C00]),
];

pub fn discovery_780m() -> Discovery {
    Discovery {
        binary_major: 2,
        binary_minor: 0,
        binary_size: 0,
        table_version: 4,
        dies: 1,
        bases_64: false,
        ips: IPS_780M
            .iter()
            .map(|&(hw_id, version, bases)| IpEntry { die: 0, hw_id, instance: 0, version, byte7: 0, vcn_config: 0, bases: bases.to_vec() })
            .collect(),
        gc: None,
        harvest: None,
        vcn: None,
        mall: None,
        notes: Vec::new(),
    }
}

fn byte(d: &Discovery, hw: u16, at: (usize, u32)) -> Option<u32> {
    d.reg(hw, 0, at.0, at.1)?.checked_mul(4)
}

/// Bits 47:24 of an address (FB/AGP registers); bits 47:18 (system aperture).
fn mb24(a: u64) -> u32 {
    (a >> 24) as u32 & 0xFF_FFFF
}
fn mb18(a: u64) -> u32 {
    (a >> 18) as u32 & 0x3FFF_FFFF
}

/// The firmware's register state before any driver (the capture's first reads; MMHUB and NBIO from the fake's
/// carve-out). Called by `FakeGpu::with`.
pub fn cold_image(f: &mut FakeGpu, d: &Discovery) {
    let top = f.fb_mc.saturating_add(f.fb_size).saturating_sub(1);
    let set: Vec<(u16, (usize, u32), u32)> = vec![
        (hwid::MP0, crate::ipmod::psp13::C2PMSG_33, 0x8000_0000),
        (hwid::NBIF, nbio7::RCC_CONFIG_MEMSIZE, u32::try_from(f.fb_size >> 20).unwrap_or(u32::MAX)),
        (hwid::NBIF, nbio7::RCC_DOORBELL_APER_EN, 0),
        (hwid::MMHUB, gmc11::MMHUB_FB_OFFSET, mb24(f.fb_phys)),
        (hwid::MMHUB, gmc11::MMHUB_FB_LOCATION_BASE, mb24(f.fb_mc)),
        (hwid::MMHUB, gmc11::MMHUB_FB_LOCATION_TOP, mb24(top)),
        (hwid::MMHUB, gmc11::MMHUB_MX_L1_TLB_CNTL, 0x501),
        (hwid::MMHUB, gmc11::MMVM_L2_CNTL, 0x8_0602),
        (hwid::MMHUB, gmc11::MMVM_CONTEXT0_CNTL, 0x01ff_fe00),
        (hwid::OSSSYS, ih6::IH_RB_CNTL, 0x4061_0000),
        // GX-7h assumptions (the capture never read them): the firmware's system aperture covers the carve-out
        // (the display fetches through it) and GFXHUB's FB copies equal MMHUB's (Linux never writes them on bare
        // metal, gfxhub_v3_0.c:355-364, yet GFX works). The rest of both hubs: `hubfake`'s cold values.
        (hwid::MMHUB, gmc11::MMHUB_SYSTEM_APERTURE_LOW, mb18(f.fb_mc)),
        (hwid::MMHUB, gmc11::MMHUB_SYSTEM_APERTURE_HIGH, mb18(top)),
        (hwid::GC, gmc11::GCMC_VM_FB_OFFSET, mb24(f.fb_phys)),
        (hwid::GC, gmc11::GCMC_VM_FB_LOCATION_BASE, mb24(f.fb_mc)),
        (hwid::GC, gmc11::GCMC_VM_FB_LOCATION_TOP, mb24(top)),
    ];
    for (hw, at, val) in set {
        if let Some(b) = byte(d, hw, at) {
            f.set_reg(b, val);
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Crash {
    /// A KM ring with frames and its TMR left (the kernel died mid-bring-up).
    RingLeft,
    /// A PMFW message sent and never answered (AllowGfxOff, PMFW hung on it).
    SmuUnanswered,
    /// GC powered by EnableGfxImu, the RLC never booted.
    GcWithoutRlc,
    /// The ring destroyed, the TMR not.
    TmrLeft,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StartAs {
    Cold,
    AfterLinux,
    AfterBitsOs,
    AfterCrash(Crash),
    PspDown,
}

/// Linux's own placement on 27.10. (dmesg): KM ring at +0x339000, TMR at +0x18000000 (64 MiB); for smaller
/// carve-outs the TMR goes to the top 64 MiB below 4 MiB of the end (any size: the fake only needs it inside).
fn linux_places(f: &FakeGpu) -> (u64, u64) {
    let ring = f.fb_mc + 0x33_9000;
    let tmr_size = u64::from(TOC_TMR);
    let tmr = if f.fb_size >= 0x1800_0000 + tmr_size { f.fb_mc + 0x1800_0000 } else { f.fb_mc + f.fb_size.saturating_sub(tmr_size + (4 << 20)) };
    (ring, tmr)
}

impl FakeGpu {
    /// Turns a cold fake into the state `s` leaves (registers, PSP ring and TMR, PMFW mailbox, GC).
    pub fn start_as(&mut self, d: &Discovery, s: StartAs) {
        let set = |f: &mut FakeGpu, hw: u16, at: (usize, u32), val: u32| {
            if let Some(b) = byte(d, hw, at) {
                f.set_reg(b, val);
            }
        };
        let (ring, tmr) = linux_places(self);
        match s {
            StartAs::Cold => {}
            StartAs::AfterLinux => {
                // The capture's read-back after the load (minipc-780m-state.txt, after_linux).
                self.stale_ring(ring, 0x1000, 0x1b0);
                self.stale_tmr(tmr, TOC_TMR);
                self.gc.set_booted();
                self.sdma.as_linux_left();
                // GX-7j: the CP unhalted, MES running (it maps the queues in Linux 7.1).
                self.gfx.as_linux_left();
                if let Some(b) = byte(d, hwid::GC, crate::ipmod::gfx11::CP_MES_CNTL) {
                    self.gc.set(b, 0x0c00_0000);
                }
                // GX-7k: the MES fake answers CP_MES_CNTL while GC is on: both pipes running, their HQDs active.
                self.mes.as_linux_left();
                self.smu.drv.msg = msg::POWER_DOWN_VCN;
                self.smu.drv.arg = 7;
                // GX-7j3: Linux's shutdown before the kexec sent PrepareMp1ForUnload (amdgpu_pci_shutdown, mp1
                // state UNLOAD, smu_v13_0_4_system_features_control).
                self.smu.unloaded = true;
                let top = self.fb_mc + self.fb_size - 1;
                for (hw, at, val) in [
                    (hwid::NBIF, nbio7::RCC_DOORBELL_APER_EN, 1),
                    (hwid::MMHUB, gmc11::MMHUB_MX_L1_TLB_CNTL, 0x1859),
                    (hwid::MMHUB, gmc11::MMVM_L2_CNTL, 0x8_0e01),
                    (hwid::MMHUB, gmc11::MMVM_CONTEXT0_CNTL, 0x01ff_fe01),
                    (hwid::MMHUB, gmc11::MMHUB_AGP_TOP, 0),
                    (hwid::MMHUB, gmc11::MMHUB_AGP_BOT, 0xFF_FFFF),
                    (hwid::MMHUB, gmc11::MMHUB_AGP_BASE, 0),
                    (hwid::MMHUB, gmc11::MMHUB_SYSTEM_APERTURE_LOW, mb18(self.fb_mc)),
                    (hwid::MMHUB, gmc11::MMHUB_SYSTEM_APERTURE_HIGH, mb18(top)),
                    (hwid::OSSSYS, ih6::IH_RB_CNTL, 0x4033_0121),
                    (hwid::OSSSYS, ih6::IH_DOORBELL_RPTR, 0x1000_02f0),
                ] {
                    set(self, hw, at, val);
                }
            }
            StartAs::AfterBitsOs => {
                // Rings and TMR destroyed; the PSP keeps the write pointer (0x10 after one frame) and the size
                // (the Mini-PC 28.10.: not cleared by DESTROY_RINGS), so only the stop note tells it from a crash.
                self.stale_ring(ring, 0x1000, 0x10);
                self.ring = None;
                self.gc.set_booted();
                self.smu.drv.msg = msg::GET_ENABLED_SMU_FEATURES;
            }
            StartAs::AfterCrash(c) => match c {
                Crash::RingLeft => {
                    self.stale_ring(ring, 0x1000, 0x40);
                    self.stale_tmr(tmr, TOC_TMR);
                }
                Crash::SmuUnanswered => {
                    self.smu.stale_message(msg::ALLOW_GFXOFF, 0);
                    self.smu.hostile.push(SmuHostile::Never { id: msg::ALLOW_GFXOFF });
                }
                Crash::GcWithoutRlc => self.gc.set_on_unbooted(),
                Crash::TmrLeft => {
                    set(self, hwid::MP0, crate::ipmod::psp13::C2PMSG_71, 0x1000);
                    self.stale_tmr(tmr, TOC_TMR);
                }
            },
            StartAs::PspDown => set(self, hwid::MP0, crate::ipmod::psp13::C2PMSG_81, 0),
        }
    }
}

impl FakeGpu {
    /// GX-7f: the stop note a clean BitsOS stop hands on for the state `s` left (`None` for the others), with the
    /// PSP ring registers as they read now.
    pub fn stop_note(&mut self, s: StartAs) -> Option<super::handoff::StopNote> {
        use super::handoff::{GcFate, RingFate, StopNote};
        use super::psp::Hw;
        if s != StartAs::AfterBitsOs {
            return None;
        }
        let a = self.a;
        let [wptr, size, addr_lo, addr_hi] = [a.c2pmsg_67, a.c2pmsg_71, a.c2pmsg_69, a.c2pmsg_70].map(|b| self.reg_read(b).unwrap_or(u32::MAX));
        Some(StopNote { ring: RingFate::Destroyed, gc: GcFate::Booted, tmr_destroyed: true, bus_master_left_on: false, wptr, size, addr_lo, addr_hi })
    }
}
