//! The looks outside Linux's chain: the card as it stands (`alive`), Linux's wait for the bootloader alone
//! (`bootloader`), the SMU's questions (`smu`), and what this boot did so far (`state`). None of them writes
//! anything the secure processor acts on, and no stage of the chain needs one of them.

use super::regs14::Regs14;
use super::smu14;
use super::stage::{Ctx, Session, Stop, psp_stop, read, ring_regs, smu_stop, stop};
use crate::gpucore::psp::{self, Cond, Hw, PspError};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use rdna4_core::bootseq::BOOTLOADER_READY;

/// `psp_v14_0_wait_for_bootloader` (psp_v14_0.c:106-125): bit 31 of C2PMSG_35, ten rounds of `psp_wait_for`, which
/// is ten times Linux's 100000 reads.
pub const BOOTLOADER_POLLS: u32 = 10 * psp::LINUX_POLLS;
pub const BOOTLOADER_IS_READY: Cond = Cond::Masked { mask: BOOTLOADER_READY, value: BOOTLOADER_READY };

/// What the card looks like now: whether a secure OS runs, the mailbox, a ring's registers, and where the HDP
/// flush's remap points. A look, with no place in Linux's chain and needed by no stage of it: the window is
/// planned by `gmc` and the remap is written by `nbif` (Linux has both from the same two blocks), so the stage
/// `sos` meets a card on which only Linux's own accesses were made.
pub(super) fn alive(s: &Session, hw: &mut dyn Hw, r: &Regs14, c: &Ctx, out: &mut Vec<String>) -> Result<(), Stop> {
    let sol = read(hw, r.psp.c2pmsg_81)?;
    let [w, size, lo, hi] = ring_regs(hw, r);
    out.push(format!("C2PMSG_81 {sol:#010x}, C2PMSG_64 {:#010x}, C2PMSG_35 {:#010x}, C2PMSG_33 {:#010x}", hw.reg_read(r.psp.c2pmsg_64).unwrap_or(u32::MAX), hw.reg_read(r.psp.c2pmsg_35).unwrap_or(u32::MAX), hw.reg_read(r.c2pmsg_33).unwrap_or(u32::MAX)));
    out.push(format!("ring registers: C2PMSG_67 {w:#x} C2PMSG_71 {size:#x} C2PMSG_69 {lo:#x} C2PMSG_70 {hi:#x}"));
    out.push(format!("pci command {:#06x} as the attach read it: bus master {} (the secure processor's buffers lie in VRAM, it needs none)", c.facts.command, if c.facts.command & 4 != 0 { "on, as the firmware left it" } else { "off" }));
    let remap = hw.reg_read(r.hdp_remap).unwrap_or(u32::MAX);
    let held = if remap == r.hdp_flush { "the flush is remapped to that byte of BAR5 as Linux has it" } else { "not Linux's 0x7f000" };
    out.push(format!("hdp flush: REMAP_HDP_MEM_FLUSH_CNTL {remap:#x}, {held}; in this boot the stage nbif {}", if s.nb.remapped { "wrote the remap" } else { "did not write it yet" }));
    match s.layout.as_ref() {
        Some(l) => out.push(format!("vram: mc base {:#x}, firmware picture {}, window {:#x}..{:#x}", l.mc_base, l.fb.as_ref().map_or(String::from("not on this card"), |f| format!("{:#x}..{:#x}", f.start, f.end)), l.window.start, l.window.end)),
        None => out.push(String::from("vram: not planned yet (the stage gmc plans it)")),
    }
    out.push(String::from(if sol == 0 { "secure OS: not running (a cold card); the stage sos loads it" } else { "secure OS: running" }));
    Ok(())
}

/// Linux waits for the bootloader before and after each of its commands and never with a secure OS alive
/// (psp_v14_0.c:136-142); this is that wait alone, in Linux's form, to see what the stage `sos` will meet.
pub(super) fn bootloader(hw: &mut dyn Hw, r: &Regs14, out: &mut Vec<String>) -> Result<(), Stop> {
    let sol = read(hw, r.psp.c2pmsg_81)?;
    if sol != 0 {
        out.push(format!("secure OS alive (C2PMSG_81 {sol:#x}): Linux does not wait for the bootloader, nothing to do"));
        return Ok(());
    }
    let seen = psp::wait_reads(hw, r.psp.c2pmsg_35, BOOTLOADER_IS_READY, BOOTLOADER_POLLS, "bootloader");
    out.push(format!("C2PMSG_35 after the wait: {seen:x?}"));
    seen.map(|_| ()).map_err(|e| match e {
        PspError::Timeout { .. } => stop("Startlader der Karte meldet sich nicht bereit", format!("{e:?}")),
        e => psp_stop(e),
    })
}

pub(super) fn smu(s: &mut Session, hw: &mut dyn Hw, r: &Regs14, out: &mut Vec<String>) -> Result<(), Stop> {
    let (f, res) = smu14::read_out(hw, r, !s.smu_asked);
    s.smu_asked |= f.sent;
    // The stage `pmfw` writes its first message without looking at the answer register, as Linux does: it must
    // know of a question that still runs.
    if f.sent {
        s.pmfw.owes_answer = !f.answered;
    }
    out.push(format!("smu: firmware flags {:#010x} at SMN {:#x} (index pair, as Linux reads them)", f.flags, rdna4_core::regmap::SMN_MP1_FIRMWARE_FLAGS));
    for (msg, arg, name, code, v) in &f.answers {
        out.push(format!("smu {name} ({msg:#04x}, argument {arg:#x}): answer {code:#x}, value {v:#010x}"));
    }
    if let (Some(v), Some(i)) = (f.answer(0x02, 0), f.answer(0x03, 0)) {
        out.push(format!("smu version {}.{}.{} (program {}), driver interface {i:#x} (Linux 7.3.1 is built for {:#x})", v >> 16 & 0xff, v >> 8 & 0xff, v & 0xff, v >> 24, smu14::DRIVER_IF));
    }
    if let (Some(lo), Some(hi)) = (f.answer(0x0C, 0), f.answer(0x0D, 0)) {
        out.push(format!("smu running features {:#018x}", u64::from(hi) << 32 | u64::from(lo)));
    }
    s.smu = Some(f);
    res.map_err(smu_stop)
}

pub(super) fn state(s: &Session, hw: &mut dyn Hw, r: &Regs14, out: &mut Vec<String>) -> Result<(), Stop> {
    let [w, size, lo, hi] = ring_regs(hw, r);
    out.push(format!("psp: C2PMSG_81 {:#x}, ring registers 67 {w:#x} 71 {size:#x} 69 {lo:#x} 70 {hi:#x}", hw.reg_read(r.psp.c2pmsg_81).unwrap_or(u32::MAX)));
    out.push(format!("session: {}", s.summary()));
    Ok(())
}
