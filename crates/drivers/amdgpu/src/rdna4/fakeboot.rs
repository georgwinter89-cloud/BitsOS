//! GX-10 PS: the model's bootloader face, the card before it has a secure OS. It takes Linux's nine loads of a cold
//! start (`psp_hw_start`, amdgpu_psp.c:2961-3048; psp_v14_0.c:127-237) and refuses everything else, on its own
//! account (it does not ask the driver's gate): a command out of Linux's order or a second time, a value that is
//! none of the nine, a command or an address before the ready bit was read, a command without its address, an
//! address that names no megabyte of VRAM outside the picture and the firmware's region, a buffer that does not
//! hold exactly the part (`psp_copy_fw`: the part, then zeros to the megabyte's end; so a buffer that is not 1 MiB
//! aligned shows too), bytes that were not flushed through the HDP, and anything once the secure OS runs.
//!
//! It answers as the real card answered Linux (the trace's section psp-boot): C2PMSG_35 echoes the command for a
//! few reads, reads 0 for some more, then 0x80000000; after the secure OS command C2PMSG_81 stays 0 for some reads
//! and then shows 0x0e5b5a34, and only from then on is there a secure OS to make a ring on (C2PMSG_64 0x80c00000).

use super::fakemodel::{MC_BASE, Model};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use rdna4_core::bootseq::{BOOTLOADER_READY, BUFFER_BYTES, Mailbox};

/// The secure OS's sign of life as the card showed it to Linux (init.txt line 749).
pub const SOL: u32 = 0x0E5B_5A34;
/// C2PMSG_64 once the secure OS runs, before any ring (init.txt line 752).
pub const C64_READY: u32 = 0x80C0_0000;
/// Reads of C2PMSG_35 that echo a command, and that show 0, before the ready bit (the trace: 4 to 7, 64 to 466).
const ECHO_READS: u32 = 5;
const BUSY_READS: u32 = 9;
/// Reads of C2PMSG_81 that still show 0 after the secure OS command (the trace: 5131, the first of them Linux's
/// read before its wait).
const SOL_READS: u32 = 4;

pub struct Boot {
    /// Linux's nine in order: the command and the part's bytes.
    pub parts: Vec<(u32, Vec<u8>)>,
    /// How many of them the bootloader took.
    pub taken: usize,
    pub c36: u32,
    /// The command being answered and the reads left of its echo and of its busy 0.
    answering: Option<(u32, u32, u32)>,
    ready_seen: bool,
    addressed: bool,
    /// Reads of C2PMSG_81 left before the secure OS shows.
    sol_in: Option<u32>,
    /// Tests: the part with this index is taken and never answered (C2PMSG_35 stays 0).
    pub silent_at: Option<usize>,
    /// Tests: the secure OS shows at its command's first read of C2PMSG_81 (up within Linux's 20 ms).
    pub sos_at_once: bool,
    /// Tests: a secure OS shows at the first read of C2PMSG_81 after this many parts were taken (one that comes up
    /// between two of Linux's loads; no trace shows it, Linux's code allows for it at every part).
    pub alive_after: Option<usize>,
}

impl Boot {
    pub fn new(parts: Vec<(u32, Vec<u8>)>) -> Boot {
        Boot { parts, taken: 0, c36: 0, answering: None, ready_seen: false, addressed: false, sol_in: None, silent_at: None, sos_at_once: false, alive_after: None }
    }
}

impl Model {
    /// A read of C2PMSG_35.
    pub(super) fn boot_read35(&mut self) -> u32 {
        let b = &mut self.boot;
        let v = match b.answering {
            Some((command, echo, busy)) if echo > 0 => {
                b.answering = Some((command, echo - 1, busy));
                command
            }
            Some((command, 0, busy)) if busy > 0 => {
                b.answering = Some((command, 0, busy - 1));
                0
            }
            Some(_) if b.silent_at == Some(b.taken - 1) => 0,
            _ => {
                b.answering = None;
                BOOTLOADER_READY
            }
        };
        b.ready_seen |= v & BOOTLOADER_READY != 0;
        v
    }

    /// A read of C2PMSG_81: the secure OS comes up some reads after its command.
    pub(super) fn boot_read81(&mut self) -> u32 {
        match self.boot.sol_in {
            Some(0) => {
                self.boot.sol_in = None;
                (self.sol, self.c64) = (SOL, C64_READY);
            }
            Some(n) => self.boot.sol_in = Some(n - 1),
            None => {}
        }
        self.sol
    }

    /// The megabyte at `mc` holds exactly `part` and zeros behind it.
    fn megabyte_holds(&self, mc: u64, part: &[u8]) -> bool {
        let mut got = alloc::vec![0u8; BUFFER_BYTES as usize];
        self.peek(mc - MC_BASE, &mut got);
        got[..part.len()] == *part && got[part.len()..].iter().all(|b| *b == 0)
    }

    /// Whether a megabyte at `mc` is memory the driver may use: VRAM, outside the picture, the firmware's region
    /// and what GMC holds (the GART table, the default page).
    pub(super) fn free_megabyte(&self, mc: u64) -> bool {
        let Some(off) = mc.checked_sub(MC_BASE) else { return false };
        let end = off.saturating_add(BUFFER_BYTES);
        let top = self.vram_bytes().saturating_sub(u64::from(self.reserved) + u64::from(self.reserved_ext));
        end <= top && (off >= self.picture.end || end <= self.picture.start) && self.psp_may_have(mc, BUFFER_BYTES)
    }

    pub(super) fn boot_write(&mut self, to: Mailbox, v: u32) -> bool {
        if self.sol != 0 {
            return self.refuse(format!("bootloader mailbox written ({to:?} {v:#x}) while the secure OS runs"));
        }
        // Linux turns to the secure processor after the common block, GMC and IH (amdgpu_device.c:2397, :2406 and
        // :2469 before `amdgpu_device_fw_loading` :2473).
        if !self.nb.started() {
            return self.refuse(format!("bootloader mailbox written ({to:?} {v:#x}) before GMC and IH were started: not Linux's order of blocks"));
        }
        if !self.boot.ready_seen {
            return self.refuse(format!("bootloader mailbox written ({to:?} {v:#x}) before its ready bit was read"));
        }
        match to {
            Mailbox::Address => {
                if self.boot.addressed {
                    return self.refuse(String::from("the bootloader's address twice"));
                }
                let mc = u64::from(v) << 20;
                if !self.free_megabyte(mc) {
                    return self.refuse(format!("the bootloader's buffer at {mc:#x}: no free megabyte of VRAM (this card is driven without a GART)"));
                }
                (self.boot.c36, self.boot.addressed) = (v, true);
                true
            }
            Mailbox::Command => {
                let taken = self.boot.taken;
                let Some((command, part)) = self.boot.parts.get(taken).cloned() else { return self.refuse(format!("bootloader command {v:#x} after all nine")) };
                if v != command {
                    let why = match self.boot.parts.iter().position(|(c, _)| *c == v) {
                        Some(i) if i < taken => "a second time",
                        Some(_) => "out of Linux's order",
                        None => "which Linux never sends this card",
                    };
                    return self.refuse(format!("bootloader command {v:#x} {why} (Linux's next is {command:#x})"));
                }
                if !self.boot.addressed {
                    return self.refuse(format!("bootloader command {v:#x} without its buffer's address"));
                }
                if self.unflushed {
                    return self.refuse(format!("bootloader command {v:#x} with the buffer's bytes not flushed through the HDP"));
                }
                let mc = u64::from(self.boot.c36) << 20;
                if !self.megabyte_holds(mc, &part) {
                    return self.refuse(format!("bootloader command {v:#x}: the megabyte at {mc:#x} does not hold that part and zeros behind it"));
                }
                let b = &mut self.boot;
                (b.taken, b.addressed, b.ready_seen, b.answering) = (taken + 1, false, false, Some((v, ECHO_READS, BUSY_READS)));
                if b.taken == b.parts.len() && b.silent_at != Some(taken) {
                    b.sol_in = Some(if b.sos_at_once { 0 } else { SOL_READS });
                } else if b.alive_after == Some(b.taken) {
                    b.sol_in = Some(0);
                }
                true
            }
        }
    }
}
