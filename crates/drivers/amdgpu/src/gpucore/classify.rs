//! GX-7b: what state the GPU is in when this kernel starts. The next kernel after a cold boot, a CachyOS boot,
//! a BitsOS Reload (GC left on, rings and TMR destroyed: the desk's bring-up rule) or a crash finds very
//! different hardware, and the bring-up (GX-7d on) must know which before its first write.
//!
//! Pure: `classify` reads through a closure (host tests feed the Mini-PC's captured values, the live probe the
//! BAR5 owner) and only the registers of its own tables, each a plain status or configuration register without
//! read side effects, at the addresses the discovery table gives. GC's core registers are read only when SMUIO's
//! GFXOFF status (no GC access) says GC is powered (reading GC while it is power-gated returns garbage).
//! GFX_IMU_MSG_FLAGS is read always and only recorded (GX-7f: it has no GFXOFF bit field on the 780M; the
//! Mini-PC read it thousands of times in every state on 28.10.). Every value comes with its meaning.
//!
//! GX-7f: the PSP keeps C2PMSG_67/71 after DESTROY_RINGS, so a clean BitsOS stop is told from a crash only by
//! the note the kernel before handed on (`handoff::StopNote`, `classify_with`), trusted only while the PSP's
//! ring registers still read what it saw.

use super::disc::Discovery;
use super::handoff::{GcFate, RingFate, StopNote};
use super::hwid;
use crate::ipmod::{gfx11, gmc11, ih6, nbio7, psp13, smu13};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Group {
    Psp,
    Smu,
    Nbio,
    Mmhub,
    Ih,
    Gc,
    Gfxhub,
}

impl Group {
    pub fn name(self) -> &'static str {
        match self {
            Group::Psp => "psp",
            Group::Smu => "smu",
            Group::Nbio => "nbio",
            Group::Mmhub => "mmhub",
            Group::Ih => "ih",
            Group::Gc => "gc",
            Group::Gfxhub => "gfxhub",
        }
    }
}

struct Reg {
    group: Group,
    name: &'static str,
    hw: u16,
    at: (usize, u32),
    meaning: fn(u32) -> String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Finding {
    pub group: Group,
    pub name: &'static str,
    /// BAR5 byte offset (`None`: the discovery table has no such IP or segment).
    pub byte: Option<u32>,
    pub value: Option<u32>,
    pub meaning: String,
}

/// GC's power as found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GcPower {
    On,
    /// SMUIO's PWR_GFXOFF_STATUS (0 off, 1 leaving, 3 entering): GC not read further.
    Off { status: u32 },
    /// SMUIO could not be read: GC not read.
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Start {
    /// Firmware state: PSP sOS and PMFW run, no ring, no IH, no doorbells, no GART, GC not booted.
    Cold,
    /// A Linux amdgpu ran (IH ring, doorbell aperture or GART on: BitsOS's bring-up sets none of them yet).
    AfterLinux,
    /// A BitsOS kernel brought GC up and stopped cleanly (GC on, RLC booted, no KM ring left).
    AfterBitsOs,
    /// GX-7f: a BitsOS kernel stopped its PSP cleanly (its note: rings and TMR destroyed), GC not booted by it.
    AfterBitsOsNoGc,
    /// GX-7f: the kernel before sent EnableGfxImu and GC did not boot; it left TMR and ring (its note): a reboot.
    GcUnsettled,
    /// Something stopped halfway: a KM ring left behind, a PMFW message without answer, GC on without RLC.
    AfterCrash,
    /// C2PMSG_81 reads 0: the PSP secure OS does not run (Linux and Windows refuse the GPU then).
    PspDown,
    /// A register the verdict needs could not be read.
    Unknown,
}

impl Start {
    pub fn name(self) -> &'static str {
        match self {
            Start::Cold => "cold (firmware state)",
            Start::AfterLinux => "after Linux (amdgpu ran)",
            Start::AfterBitsOs => "after a BitsOS Reload (GC on, rings and TMR destroyed)",
            Start::AfterBitsOsNoGc => "after a clean BitsOS stop (rings and TMR destroyed, GC not booted)",
            Start::GcUnsettled => "after a BitsOS GC stage that did not boot GC (TMR and ring left): reboot",
            Start::AfterCrash => "after a crash or an unclean stop",
            Start::PspDown => "PSP secure OS not running",
            Start::Unknown => "unknown",
        }
    }
}

pub struct State {
    pub findings: Vec<Finding>,
    pub gc: GcPower,
    pub start: Start,
    /// The evidence for `start`, one plain sentence each.
    pub why: Vec<String>,
}

impl State {
    pub fn value(&self, name: &str) -> Option<u32> {
        self.findings.iter().find(|f| f.name == name).and_then(|f| f.value)
    }
}

fn bit(v: u32, n: u32) -> bool {
    v >> n & 1 != 0
}

fn on_off(v: u32, n: u32, what: &str) -> String {
    format!("{what} {}", if bit(v, n) { "on" } else { "off" })
}

/// Bits 23:0 of an FB/AGP register are MC (or CPU) address bits 47:24.
fn mb24(v: u32) -> u64 {
    u64::from(v & 0xFF_FFFF) << 24
}

pub fn smu_msg_name(id: u32) -> &'static str {
    // PPSMC message ids of PMFW 13.0.4 (smu_v13_0_4_ppsmc.h:54-94), the ones Linux sends.
    match id {
        0x01 => "TestMessage",
        0x02 => "GetPmfwVersion",
        0x03 => "GetDriverIfVersion",
        0x06 => "PowerDownVcn",
        0x07 => "PowerUpVcn",
        0x0C => "PrepareMp1ForUnload",
        0x0D => "SetDriverDramAddrHigh",
        0x0E => "SetDriverDramAddrLow",
        0x0F => "TransferTableSmu2Dram",
        0x10 => "TransferTableDram2Smu",
        0x11 => "GfxDeviceDriverReset",
        0x12 => "GetEnabledSmuFeatures",
        0x16 => "EnableGfxImu",
        0x19 => "AllowGfxOff",
        0x1A => "DisallowGfxOff",
        0x21 => "PowerDownJpeg",
        0x22 => "PowerUpJpeg",
        0x25 => "AllowZstates",
        _ => "?",
    }
}

/// PMFW's answer codes (smu_cmn.c:81-87).
pub fn smu_resp(v: u32) -> String {
    let s = match v {
        0 => "no answer (a message in flight, or none since reset)",
        1 => "OK",
        0xFF => "failed",
        0xFE => "unknown message",
        0xFD => "refused: prerequisites not met",
        0xFC => "busy with another message",
        0xFB => "debug end",
        _ => "not a PMFW answer code",
    };
    format!("{s} ({v:#x})")
}

pub fn gfxoff_status(s: u32) -> &'static str {
    match s & 3 {
        0 => "in GFXOFF (GC power-gated)",
        1 => "leaving GFXOFF",
        2 => "not in GFXOFF (GC powered)",
        _ => "entering GFXOFF",
    }
}

const PLAIN: &[Reg] = &[
    Reg { group: Group::Psp, name: "MP0_C2PMSG_81", hw: hwid::MP0, at: psp13::C2PMSG_81, meaning: |v| {
        if v != 0 { format!("PSP secure OS runs (sign {v:#x})") } else { String::from("0: the PSP secure OS does NOT run (Linux refuses the GPU)") }
    } },
    Reg { group: Group::Psp, name: "MP0_C2PMSG_33", hw: hwid::MP0, at: psp13::C2PMSG_33, meaning: |v| {
        format!("firmware (IFWI) init {}", if bit(v, 31) { "done" } else { "NOT done" })
    } },
    Reg { group: Group::Psp, name: "MP0_C2PMSG_35", hw: hwid::MP0, at: psp13::C2PMSG_35, meaning: |v| {
        format!("bootloader {} (used only when the sOS does not run)", if bit(v, 31) { "ready" } else { "busy" })
    } },
    Reg { group: Group::Psp, name: "MP0_C2PMSG_64", hw: hwid::MP0, at: psp13::C2PMSG_64, meaning: |v| {
        format!("KM ring control: {}, status bits {:#06x}", if bit(v, 31) { "ready for a command" } else { "BUSY with a command" }, v & 0xFFFF)
    } },
    Reg { group: Group::Psp, name: "MP0_C2PMSG_67", hw: hwid::MP0, at: psp13::C2PMSG_67, meaning: |v| {
        if v == 0 { String::from("KM ring write pointer 0: no frame since the ring was made (or no ring)") } else { format!("KM ring write pointer {v} dwords ({} frames): a driver used a ring", v / 16) }
    } },
    Reg { group: Group::Psp, name: "MP0_C2PMSG_69", hw: hwid::MP0, at: psp13::C2PMSG_69, meaning: |v| format!("ring address bits 31:0 as last written ({v:#x})") },
    Reg { group: Group::Psp, name: "MP0_C2PMSG_70", hw: hwid::MP0, at: psp13::C2PMSG_70, meaning: |v| format!("ring address bits 63:32 as last written ({v:#x})") },
    Reg { group: Group::Psp, name: "MP0_C2PMSG_71", hw: hwid::MP0, at: psp13::C2PMSG_71, meaning: |v| {
        if v == 0 { String::from("ring size 0: no ring was created") } else { format!("ring size {v} bytes: a ring was created (destroyed or not)") }
    } },
    Reg { group: Group::Smu, name: "MP1_C2PMSG_66", hw: hwid::MP1, at: smu13::MSG, meaning: |v| format!("last message to PMFW: {v:#x} {}", smu_msg_name(v)) },
    Reg { group: Group::Smu, name: "MP1_C2PMSG_82", hw: hwid::MP1, at: smu13::ARG, meaning: |v| format!("argument or answer of that message ({v:#x})") },
    Reg { group: Group::Smu, name: "MP1_C2PMSG_90", hw: hwid::MP1, at: smu13::RESP, meaning: |v| format!("PMFW answer: {}", smu_resp(v)) },
    Reg { group: Group::Smu, name: "MP1_C2PMSG_91", hw: hwid::MP1, at: smu13::DCN_RESP, meaning: |v| format!("display's PMFW mailbox answer: {}", smu_resp(v)) },
    Reg { group: Group::Smu, name: "SMUIO_GFX_MISC_CNTL", hw: hwid::SMUIO, at: smu13::SMUIO_GFX_MISC_CNTL, meaning: |v| {
        format!("PWR_GFXOFF_STATUS {}: {}", v >> 1 & 3, gfxoff_status(v >> 1))
    } },
    Reg { group: Group::Nbio, name: "RCC_CONFIG_MEMSIZE", hw: hwid::NBIF, at: nbio7::RCC_CONFIG_MEMSIZE, meaning: |v| format!("{v} MiB of graphics memory (the carve-out's size)") },
    Reg { group: Group::Nbio, name: "RCC_DOORBELL_APER_EN", hw: hwid::NBIF, at: nbio7::RCC_DOORBELL_APER_EN, meaning: |v| on_off(v, 0, "doorbell aperture") },
    Reg { group: Group::Mmhub, name: "MMMC_VM_FB_OFFSET", hw: hwid::MMHUB, at: gmc11::MMHUB_FB_OFFSET, meaning: |v| format!("carve-out at CPU {:#x} (Linux's APU aperture base)", mb24(v)) },
    Reg { group: Group::Mmhub, name: "MMMC_VM_FB_LOCATION_BASE", hw: hwid::MMHUB, at: gmc11::MMHUB_FB_LOCATION_BASE, meaning: |v| format!("graphics memory from MC {:#x}", mb24(v)) },
    Reg { group: Group::Mmhub, name: "MMMC_VM_FB_LOCATION_TOP", hw: hwid::MMHUB, at: gmc11::MMHUB_FB_LOCATION_TOP, meaning: |v| format!("to MC {:#x}", mb24(v) | 0xFF_FFFF) },
    Reg { group: Group::Mmhub, name: "MMMC_VM_AGP_BASE", hw: hwid::MMHUB, at: gmc11::MMHUB_AGP_BASE, meaning: |v| format!("AGP aperture base {:#x}", mb24(v)) },
    Reg { group: Group::Mmhub, name: "MMMC_VM_AGP_BOT", hw: hwid::MMHUB, at: gmc11::MMHUB_AGP_BOT, meaning: |v| format!("AGP aperture from MC {:#x}", mb24(v)) },
    Reg { group: Group::Mmhub, name: "MMMC_VM_AGP_TOP", hw: hwid::MMHUB, at: gmc11::MMHUB_AGP_TOP, meaning: |v| {
        if v & 0xFF_FFFF == 0 { String::from("AGP aperture top 0: AGP off (as Linux programs it)") } else { format!("AGP aperture to MC {:#x}", mb24(v) | 0xFF_FFFF) }
    } },
    Reg { group: Group::Mmhub, name: "MMMC_VM_SYSTEM_APERTURE_LOW_ADDR", hw: hwid::MMHUB, at: gmc11::MMHUB_SYSTEM_APERTURE_LOW, meaning: |v| format!("system aperture from MC {:#x}", u64::from(v & 0x3FFF_FFFF) << 18) },
    Reg { group: Group::Mmhub, name: "MMMC_VM_SYSTEM_APERTURE_HIGH_ADDR", hw: hwid::MMHUB, at: gmc11::MMHUB_SYSTEM_APERTURE_HIGH, meaning: |v| format!("system aperture to MC {:#x}", u64::from(v & 0x3FFF_FFFF) << 18 | 0x3_FFFF) },
    Reg { group: Group::Mmhub, name: "MMMC_VM_MX_L1_TLB_CNTL", hw: hwid::MMHUB, at: gmc11::MMHUB_MX_L1_TLB_CNTL, meaning: |v| on_off(v, 0, "MMHUB L1 TLB") },
    Reg { group: Group::Mmhub, name: "MMVM_L2_CNTL", hw: hwid::MMHUB, at: gmc11::MMVM_L2_CNTL, meaning: |v| on_off(v, 0, "MMHUB L2 cache") },
    Reg { group: Group::Mmhub, name: "MMVM_L2_PROTECTION_FAULT_STATUS", hw: hwid::MMHUB, at: gmc11::MMVM_L2_PROTECTION_FAULT_STATUS, meaning: |v| {
        if v == 0 { String::from("no VM fault recorded") } else { format!("a VM fault is recorded ({v:#x})") }
    } },
    Reg { group: Group::Mmhub, name: "MMVM_CONTEXT0_CNTL", hw: hwid::MMHUB, at: gmc11::MMVM_CONTEXT0_CNTL, meaning: |v| on_off(v, 0, "VMID 0 (GART)") },
    // GX-7h: where VMID 0's table is (a Reload hands a GART-enabled hub on; the GMC stage reprograms it).
    Reg { group: Group::Mmhub, name: "MMVM_CONTEXT0_PAGE_TABLE_BASE_ADDR_LO32", hw: hwid::MMHUB, at: MMVM_PT_BASE_LO, meaning: pt_base_lo },
    Reg { group: Group::Mmhub, name: "MMVM_CONTEXT0_PAGE_TABLE_BASE_ADDR_HI32", hw: hwid::MMHUB, at: MMVM_PT_BASE_HI, meaning: pt_base_hi },
    Reg { group: Group::Ih, name: "IH_RB_CNTL", hw: hwid::OSSSYS, at: ih6::IH_RB_CNTL, meaning: |v| on_off(v, 0, "IH ring") },
    Reg { group: Group::Ih, name: "IH_RB_BASE", hw: hwid::OSSSYS, at: ih6::IH_RB_BASE, meaning: |v| format!("IH ring address bits 39:8 ({:#x})", u64::from(v) << 8) },
    Reg { group: Group::Ih, name: "IH_RB_BASE_HI", hw: hwid::OSSSYS, at: ih6::IH_RB_BASE_HI, meaning: |v| format!("IH ring address bits 47:40 ({:#x})", v & 0xFF) },
    Reg { group: Group::Ih, name: "IH_RB_RPTR", hw: hwid::OSSSYS, at: ih6::IH_RB_RPTR, meaning: |v| format!("IH read pointer {v:#x}") },
    Reg { group: Group::Ih, name: "IH_RB_WPTR", hw: hwid::OSSSYS, at: ih6::IH_RB_WPTR, meaning: |v| format!("IH write pointer {v:#x}") },
    Reg { group: Group::Ih, name: "IH_DOORBELL_RPTR", hw: hwid::OSSSYS, at: ih6::IH_DOORBELL_RPTR, meaning: |v| {
        format!("IH read pointer by doorbell {} (offset {:#x})", if bit(v, 28) { "on" } else { "off" }, v & 0x03FF_FFFF)
    } },
];

const IMU_FLAGS: Reg = Reg { group: Group::Gc, name: "GFX_IMU_MSG_FLAGS", hw: hwid::GC, at: gfx11::GFX_IMU_MSG_FLAGS, meaning: |v| {
    let seen = super::gfxup::IMU_FLAGS_SEEN.iter().find(|(x, _)| *x == v).map_or("a value not seen on 28.10.", |(_, n)| n);
    format!("IMU message flags (recorded only, no meaning used): {seen}")
} };

const GC: &[Reg] = &[
    Reg { group: Group::Gc, name: "RLC_RLCS_BOOTLOAD_STATUS", hw: hwid::GC, at: gfx11::RLC_RLCS_BOOTLOAD_STATUS_11_0_1, meaning: |v| {
        format!("RLC bootload {}", if bit(v, 31) { "complete: GC firmware is loaded" } else { "not complete" })
    } },
    Reg { group: Group::Gc, name: "RLC_CNTL", hw: hwid::GC, at: gfx11::RLC_CNTL, meaning: |v| on_off(v, 0, "RLC F32 core") },
    Reg { group: Group::Gc, name: "CP_STAT", hw: hwid::GC, at: gfx11::CP_STAT, meaning: |v| if v == 0 { String::from("CP idle") } else { format!("CP busy ({v:#x})") } },
    Reg { group: Group::Gc, name: "GRBM_STATUS", hw: hwid::GC, at: gfx11::GRBM_STATUS, meaning: |v| format!("graphics {}", if bit(v, 31) { "active" } else { "idle" }) },
    Reg { group: Group::Gc, name: "CP_ME_CNTL", hw: hwid::GC, at: gfx11::CP_ME_CNTL, meaning: |v| {
        format!("ME {}, PFP {}", if bit(v, 28) { "halted" } else { "running" }, if bit(v, 26) { "halted" } else { "running" })
    } },
    Reg { group: Group::Gc, name: "CP_MES_CNTL", hw: hwid::GC, at: gfx11::CP_MES_CNTL, meaning: |v| format!("MES {}", if bit(v, 30) { "halted" } else { "running" }) },
    Reg { group: Group::Gfxhub, name: "GCMC_VM_FB_OFFSET", hw: hwid::GC, at: gmc11::GCMC_VM_FB_OFFSET, meaning: |v| format!("GFXHUB's carve-out copy: CPU {:#x}", mb24(v)) },
    Reg { group: Group::Gfxhub, name: "GCMC_VM_FB_LOCATION_BASE", hw: hwid::GC, at: gmc11::GCMC_VM_FB_LOCATION_BASE, meaning: |v| format!("GFXHUB graphics memory from MC {:#x}", mb24(v)) },
    Reg { group: Group::Gfxhub, name: "GCMC_VM_FB_LOCATION_TOP", hw: hwid::GC, at: gmc11::GCMC_VM_FB_LOCATION_TOP, meaning: |v| format!("to MC {:#x}", mb24(v) | 0xFF_FFFF) },
    Reg { group: Group::Gfxhub, name: "GCVM_CONTEXT0_CNTL", hw: hwid::GC, at: gmc11::GCVM_CONTEXT0_CNTL, meaning: |v| on_off(v, 0, "GFXHUB VMID 0 (GART)") },
    Reg { group: Group::Gfxhub, name: "GCVM_CONTEXT0_PAGE_TABLE_BASE_ADDR_LO32", hw: hwid::GC, at: GCVM_PT_BASE_LO, meaning: pt_base_lo },
    Reg { group: Group::Gfxhub, name: "GCVM_CONTEXT0_PAGE_TABLE_BASE_ADDR_HI32", hw: hwid::GC, at: GCVM_PT_BASE_HI, meaning: pt_base_hi },
];

const MMVM_PT_BASE_LO: (usize, u32) = (1, 0x7ab);
const MMVM_PT_BASE_HI: (usize, u32) = (1, 0x7ac);
const GCVM_PT_BASE_LO: (usize, u32) = (0, 0x16f3);
const GCVM_PT_BASE_HI: (usize, u32) = (0, 0x16f4);

fn pt_base_lo(v: u32) -> String {
    if v & 1 == 0 { format!("VMID 0 table base bits 31:0 {v:#x}: no valid table") } else { format!("VMID 0 table at CPU bits 31:12 {:#x} (valid): a GART an earlier driver set up", v & !0xfff) }
}

fn pt_base_hi(v: u32) -> String {
    format!("VMID 0 table address bits 47:32 {:#x}", v & 0xffff)
}

fn byte_of(d: &Discovery, r: &Reg) -> Option<u32> {
    d.reg(r.hw, 0, r.at.0, r.at.1)?.checked_mul(4)
}

fn read_one(d: &Discovery, r: &Reg, read: &mut dyn FnMut(u32) -> Option<u32>) -> Finding {
    let byte = byte_of(d, r);
    let value = byte.and_then(&mut *read);
    let meaning = match (byte, value) {
        (_, Some(v)) => (r.meaning)(v),
        (Some(_), None) => String::from("not readable"),
        (None, _) => String::from("no address (IP or segment not in the discovery table)"),
    };
    Finding { group: r.group, name: r.name, byte, value, meaning }
}

fn skipped(d: &Discovery, r: &Reg, why: &str) -> Finding {
    Finding { group: r.group, name: r.name, byte: byte_of(d, r), value: None, meaning: format!("not read: {why}") }
}

/// The BAR5 byte offsets `classify` may read (for the live owner's safe-read rule and the tests).
pub fn addresses(d: &Discovery) -> Vec<(&'static str, Option<u32>)> {
    PLAIN.iter().chain(core::iter::once(&IMU_FLAGS)).chain(GC).map(|r| (r.name, byte_of(d, r))).collect()
}

/// Reads the tables (GC only when powered) and decides the start state.
pub fn classify(d: &Discovery, read: &mut dyn FnMut(u32) -> Option<u32>) -> State {
    classify_with(d, read, None)
}

/// GX-7i2: what this boot's BitsOS driver itself turned on (read before the Linux signs).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Own {
    /// This boot's SDMA stage turned the doorbell aperture on (RCC_DOORBELL_APER_EN bit 0) and no stop put it back.
    pub doorbell_aperture: bool,
}

/// `classify` with the note the kernel before handed on (a Reload), if any.
pub fn classify_with(d: &Discovery, read: &mut dyn FnMut(u32) -> Option<u32>, note: Option<&StopNote>) -> State {
    classify_traced(d, read, note, Own::default())
}

/// `classify_with` and this boot's own traces: the note and the traces come before the Linux signs.
pub fn classify_traced(d: &Discovery, read: &mut dyn FnMut(u32) -> Option<u32>, note: Option<&StopNote>, own: Own) -> State {
    let mut findings: Vec<Finding> = PLAIN.iter().map(|r| read_one(d, r, read)).collect();
    let smuio = findings.iter().find(|f| f.name == "SMUIO_GFX_MISC_CNTL").and_then(|f| f.value);
    findings.push(read_one(d, &IMU_FLAGS, read));
    let mut gc = match smuio {
        None => GcPower::Unknown,
        Some(v) if super::gfxup::smuio_gc_on(v) => GcPower::On,
        Some(v) => GcPower::Off { status: v >> 1 & 3 },
    };
    match gc {
        GcPower::On => {
            findings.extend(GC.iter().map(|r| read_one(d, r, read)));
            // Nothing keeps GC powered between the check and the reads (DisallowGfxOff is a write, GX-7e): SMUIO
            // is read again after them, and GC values read while it may have power-gated are thrown away. The
            // window left is that GC gates and comes back between the two SMUIO reads (then the values may mix).
            let smuio = PLAIN.iter().find(|r| r.name == "SMUIO_GFX_MISC_CNTL").map(|r| read_one(d, r, read)).and_then(|f| f.value);
            if !smuio.is_some_and(super::gfxup::smuio_gc_on) {
                let why = format!(
                    "GC left the powered state during the reads (SMUIO_GFX_MISC_CNTL now {}): the GC values are not used",
                    smuio.map_or(String::from("unreadable"), |v| format!("{v:#x}, {}", gfxoff_status(v >> 1)))
                );
                klog::event!(Warn, "gx", "classify.gc_power_changed", smuio = klog::Hex(u64::from(smuio.unwrap_or(u32::MAX))));
                for f in findings.iter_mut().filter(|f| matches!(f.group, Group::Gc | Group::Gfxhub) && f.name != IMU_FLAGS.name) {
                    f.value = None;
                    f.meaning = format!("not used: {why}");
                }
                gc = GcPower::Unknown;
            }
        }
        GcPower::Off { status } => {
            let why = format!("GC not powered (state {status}: {})", gfxoff_status(status));
            findings.extend(GC.iter().map(|r| skipped(d, r, &why)));
        }
        GcPower::Unknown => {
            findings.extend(GC.iter().map(|r| skipped(d, r, "GC's power state is unknown (SMUIO unreadable)")));
        }
    }
    let mut st = State { findings, gc, start: Start::Unknown, why: Vec::new() };
    let (start, why) = verdict(&st, note, own);
    st.start = start;
    st.why = why;
    st
}

/// The rules, in order (each sentence names the register that decided).
fn verdict(st: &State, note: Option<&StopNote>, own: Own) -> (Start, Vec<String>) {
    let v = |n: &str| st.value(n);
    let mut why = Vec::new();
    let Some(sos) = v("MP0_C2PMSG_81") else {
        why.push(String::from("MP0_C2PMSG_81 unreadable: the PSP state is unknown"));
        return (Start::Unknown, why);
    };
    if sos == 0 {
        why.push(String::from("MP0_C2PMSG_81 is 0"));
        return (Start::PspDown, why);
    }
    if sos == u32::MAX {
        // A PCIe read nobody answers (function gone, in reset, bus error) completes as all ones.
        why.push(String::from("MP0_C2PMSG_81 reads all ones: the GPU does not answer (gone, in reset or a bus error)"));
        return (Start::Unknown, why);
    }
    let wptr = v("MP0_C2PMSG_67");
    let size = v("MP0_C2PMSG_71");
    let resp = v("MP1_C2PMSG_90");
    let msg = v("MP1_C2PMSG_66");
    let mut crash = false;
    // GX-7i2: BitsOS's own word first. The note counts only while the ring registers read what it saw (a later
    // writer, Linux included, changes them); this boot's own traces say what its stages turned on.
    let note = note.filter(|n| n.matches(wptr, size, v("MP0_C2PMSG_69"), v("MP0_C2PMSG_70")));
    let aperture = v("RCC_DOORBELL_APER_EN").is_some_and(|x| bit(x, 0));
    let aperture_ours = aperture && (own.doorbell_aperture || note.is_some());
    if aperture_ours {
        why.push(String::from(if own.doorbell_aperture {
            "doorbell aperture on: this boot's SDMA or GFX ring stage turned it on (BitsOS's own trace), not a Linux sign"
        } else {
            "doorbell aperture on: left by a BitsOS kernel's SDMA or GFX ring stage (its stop note matches the PSP registers), not a Linux sign"
        }));
    }
    // Then the Linux signs: what Linux's amdgpu sets and BitsOS (by its note and traces) did not.
    let linux: Vec<&str> = [("IH_RB_CNTL", "IH ring on"), ("RCC_DOORBELL_APER_EN", "doorbell aperture on"), ("MMVM_CONTEXT0_CNTL", "GART (MMHUB VMID 0) on")]
        .into_iter()
        .filter(|(n, _)| v(n).is_some_and(|x| bit(x, 0)) && !(*n == "RCC_DOORBELL_APER_EN" && aperture_ours))
        .map(|(_, s)| s)
        .collect();
    if !linux.is_empty() {
        for s in linux {
            why.push(format!("{s}: Linux's amdgpu sets it, BitsOS's bring-up does not (or no note or trace of this boot says BitsOS did)"));
        }
        return (Start::AfterLinux, why);
    }
    if let Some(n) = note {
        why.push(format!("the stop note (this boot's last stop, or the one the kernel before handed on): {}", n.text()));
        if n.gc == GcFate::Unsettled {
            why.push(String::from("EnableGfxImu was sent and GC did not boot; TMR and ring were left: only a reboot clears that (mode 2 is refused)"));
            return (Start::GcUnsettled, why);
        }
    }
    let destroyed = note.is_some_and(|n| n.ring == RingFate::Destroyed);
    if wptr.is_some_and(|w| w != 0) {
        if destroyed {
            why.push(format!("the KM ring (write pointer {}) was destroyed cleanly by the last stop (its note)", wptr.unwrap_or(0)));
        } else if note.is_some_and(|n| n.ring == RingFate::NotDestroyed) {
            why.push(format!("a KM ring was used (write pointer {}) and the last stop left it up (its note)", wptr.unwrap_or(0)));
            crash = true;
        } else {
            why.push(format!("a KM ring was used (write pointer {}) and is not known destroyed", wptr.unwrap_or(0)));
            crash = true;
        }
    }
    // A zero answer alone is also the state after reset (no message yet): only with a message id it is one
    // sent and never answered.
    if resp == Some(0) && msg.is_some_and(|m| m != 0) {
        why.push(format!("PMFW message {:#x} {} has no answer (MP1_C2PMSG_90 is 0)", msg.unwrap_or(0), smu_msg_name(msg.unwrap_or(0))));
        crash = true;
    }
    let rlc = v("RLC_RLCS_BOOTLOAD_STATUS").map(|x| bit(x, 31));
    if st.gc == GcPower::On && rlc == Some(false) {
        why.push(String::from("GC is powered but the RLC never finished its bootload"));
        crash = true;
    }
    if crash {
        return (Start::AfterCrash, why);
    }
    if st.gc == GcPower::On && rlc == Some(true) {
        why.push(String::from("GC powered with the RLC booted, no KM ring, IH, doorbells or GART: a clean BitsOS stop"));
        return (Start::AfterBitsOs, why);
    }
    if destroyed {
        return match note.map(|n| n.gc) {
            Some(GcFate::Booted) => {
                why.push(String::from("GC was booted at the last stop (its note), no IH, doorbells or GART"));
                why.push(String::from("that stop did not tell PMFW of the unload (no PrepareMp1ForUnload in its note): a GC stage does it first"));
                (Start::AfterBitsOs, why)
            }
            Some(GcFate::Unloaded) => {
                why.push(String::from("GC was booted and then unloaded at the last stop (PrepareMp1ForUnload answered, RLC stopped: its note), no IH, doorbells or GART"));
                (Start::AfterBitsOs, why)
            }
            _ => {
                why.push(String::from("GC was not booted at the last stop (its note)"));
                (Start::AfterBitsOsNoGc, why)
            }
        };
    }
    if wptr.is_none() || size.is_none() {
        why.push(String::from("the KM ring registers are unreadable"));
        return (Start::Unknown, why);
    }
    why.push(String::from("PSP sOS runs, no KM ring was used, no IH, doorbells or GART"));
    if size.is_some_and(|s| s != 0) {
        why.push(String::from("(a ring size is set: a ring was made but never written; GX-7d destroys it first)"));
    }
    match st.gc {
        GcPower::On => why.push(String::from("GC powered but not read further")),
        GcPower::Off { .. } => why.push(String::from("GC power-gated (not read)")),
        GcPower::Unknown => why.push(String::from("GC's power state unknown (GC not read)")),
    }
    (Start::Cold, why)
}
