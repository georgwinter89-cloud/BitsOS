//! The controller's link: reset, codec detection, the command rings (CORB/RIRB) and the immediate command
//! registers as fallback. Sequences after `linux-6.6/sound/hda/hdac_controller.c` (`init_cmd_io` :43,
//! `stop_cmd_io` :112, `update_rirb` :190, `reset_link` :422) and V1 `snd/hda.asm` (`hda_reset` :180) and
//! `snd/verb.asm` (`hda_cmd` :48, `hda_rings` :95), without klog: the caller logs (B6).

use crate::hw::{until, Hw, Mem};
use crate::regs::*;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

/// CORB at 0, RIRB at 2048 of one buffer (Linux `rb`): 256 entries of 4 and 8 bytes at most.
pub const RINGS_BYTES: usize = 4096;
const RIRB_OFF: usize = 2048;
/// V1 waits 50 ms for an answer (`verb.asm:63`).
pub const ANSWER_US: u64 = 50_000;
/// Reset steps: 100 ms each (Linux `enter/exit_link_reset`, V1 `hda_reset`).
const RESET_US: u64 = 100_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// CORB/RIRB by DMA.
    Rings,
    /// IC/IR/ICS: no DMA at all (the GPU's controller, which must stay without bus master; a dead CORB).
    Immediate,
}

impl Mode {
    pub fn name(self) -> &'static str {
        match self {
            Mode::Rings => "corb",
            Mode::Immediate => "immediate",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CmdError {
    /// Codec, node or verb out of range (never sent).
    Bad,
    Timeout,
    /// The controller reads all ones.
    Gone,
}

/// An unsolicited response: codec, response word (tag in bits 31:26).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Unsol {
    pub cad: u8,
    pub res: u32,
}

impl Unsol {
    pub fn tag(&self) -> u8 {
        (self.res >> 26) as u8
    }
}

pub struct Link<M: Mem> {
    pub mode: Mode,
    rings: Option<M>,
    corb_n: usize,
    rirb_n: usize,
    corb_wp: usize,
    rirb_rp: usize,
    /// Unsolicited responses seen, oldest first; the service thread takes them.
    pub unsol: Vec<Unsol>,
    /// Answers that never came (each one logged by the caller).
    pub timeouts: u64,
    /// Answers from a codec other than the one asked (late answers after a timeout), dropped.
    pub strays: u64,
}

/// Entries for a CORBSIZE/RIRBSIZE value: the largest the capability bits 7:4 offer -> (size code, entries).
/// A controller that reports no capability gets 256 (Linux writes 0x02 for all).
pub fn ring_size(v: u8) -> (u8, usize) {
    if v & 0x40 != 0 || v & 0x70 == 0 {
        (2, 256)
    } else if v & 0x20 != 0 {
        (1, 16)
    } else {
        (0, 2)
    }
}

/// Stops every stream descriptor's DMA and the command rings, interrupts off. Lock-free and bounded (1 ms
/// per wait, spinning): runs at probe (a Reload may leave a running controller) and in Quiesce.
/// Returns whether every engine was seen stopped (or the window reads all ones: the device is gone).
pub fn stop_all<H: Hw + ?Sized>(hw: &H, descriptors: usize) -> bool {
    hw.wr32(INTCTL, 0);
    for i in 0..descriptors {
        let ctl = hw.rd8(sd(i) + SD_CTL);
        if ctl != 0xFF && ctl & (SD_CTL_RUN | SD_CTL_INT) != 0 {
            hw.wr8(sd(i) + SD_CTL, ctl & !(SD_CTL_RUN | SD_CTL_INT));
        }
        hw.wr8(sd(i) + SD_STS, SD_STS_MASK);
    }
    hw.wr8(RIRBCTL, 0);
    hw.wr8(CORBCTL, 0);
    let rings = until(hw, 1000, 10, false, || {
        let c = hw.rd8(CORBCTL) | hw.rd8(RIRBCTL);
        c == 0xFF || c & (CORBCTL_RUN | RIRBCTL_DMA) == 0
    });
    let streams = until(hw, 1000, 10, false, || (0..descriptors).all(|i| matches!(hw.rd8(sd(i) + SD_CTL), 0xFF) || hw.rd8(sd(i) + SD_CTL) & SD_CTL_RUN == 0));
    hw.wr8(RIRBSTS, RIRBSTS_MASK);
    hw.wr32(INTSTS, INT_CTRL_EN | INT_ALL_STREAM);
    rings && streams
}

/// Link reset and codec detection -> the STATESTS mask of codecs that reported. `sleep`: waits may sleep.
pub fn reset<H: Hw + ?Sized>(hw: &H, sleep: bool) -> Result<u16, String> {
    if hw.rd32(GCTL) == !0 {
        return Err("controller reads all ones".into());
    }
    // Old reports out of the way; an answer left unacknowledged too (QEMU gives no more before it).
    hw.wr16(STATESTS, STATESTS_MASK);
    hw.wr8(RIRBSTS, RIRBSTS_MASK);
    hw.wr32(GCTL, hw.rd32(GCTL) & !GCTL_CRST);
    if !until(hw, RESET_US, 100, sleep, || hw.rd32(GCTL) & GCTL_CRST == 0) {
        return Err("CRST did not go to 0 (enter reset)".into());
    }
    // >= 100 us in reset for the codec PLLs (HDA spec 5.5.1; Linux 500-1000 us, V1 1 ms).
    wait(hw, 1000, sleep);
    hw.wr32(GCTL, hw.rd32(GCTL) | GCTL_CRST);
    if !until(hw, RESET_US, 100, sleep, || hw.rd32(GCTL) & GCTL_CRST != 0) {
        return Err("CRST did not come back to 1 (leave reset)".into());
    }
    // Codecs report within 521 us; V1 waits up to 100 ms for the first and 2 ms more for a second one.
    wait(hw, 1000, sleep);
    until(hw, RESET_US, 500, sleep, || hw.rd16(STATESTS) & STATESTS_MASK != 0);
    wait(hw, 2000, sleep);
    let mask = hw.rd16(STATESTS) & STATESTS_MASK;
    hw.wr16(STATESTS, STATESTS_MASK);
    Ok(mask)
}

fn wait<H: Hw + ?Sized>(hw: &H, us: u64, sleep: bool) {
    if sleep { hw.pause_us(us) } else { hw.delay_us(us) }
}

impl<M: Mem> Link<M> {
    /// Commands through the immediate registers only (no DMA buffer).
    pub fn immediate() -> Link<M> {
        Link { mode: Mode::Immediate, rings: None, corb_n: 0, rirb_n: 0, corb_wp: 0, rirb_rp: 0, unsol: Vec::new(), timeouts: 0, strays: 0 }
    }

    /// Sets up and starts CORB and RIRB in `mem` (`RINGS_BYTES`, zeroed); bus master must be on. Unsolicited
    /// responses on; the RIRB raises its interrupt flag per answer (`RINTCNT` 1), which QEMU waits on.
    pub fn start_rings<H: Hw + ?Sized>(hw: &H, mem: M) -> Result<Link<M>, String> {
        hw.wr8(CORBCTL, 0);
        hw.wr8(RIRBCTL, 0);
        if !until(hw, 10_000, 10, false, || (hw.rd8(CORBCTL) | hw.rd8(RIRBCTL)) & (CORBCTL_RUN | RIRBCTL_DMA) == 0) {
            return Err("CORB/RIRB DMA did not stop".into());
        }
        let (corb_code, corb_n) = ring_size(hw.rd8(CORBSIZE));
        let (rirb_code, rirb_n) = ring_size(hw.rd8(RIRBSIZE));
        let a = mem.dev_addr();
        hw.wr32(CORBLBASE, a as u32);
        hw.wr32(CORBUBASE, (a >> 32) as u32);
        hw.wr8(CORBSIZE, corb_code);
        hw.wr16(CORBWP, 0);
        // Read pointer reset: set, wait for it to read back (QEMU clears it at once), clear, wait for 0.
        hw.wr16(CORBRP, CORBRP_RST);
        until(hw, 2000, 10, false, || hw.rd16(CORBRP) & CORBRP_RST != 0);
        hw.wr16(CORBRP, 0);
        if !until(hw, 10_000, 10, false, || hw.rd16(CORBRP) == 0) {
            return Err(format!("CORBRP did not clear ({:#x})", hw.rd16(CORBRP)));
        }
        let r = a + RIRB_OFF as u64;
        hw.wr32(RIRBLBASE, r as u32);
        hw.wr32(RIRBUBASE, (r >> 32) as u32);
        hw.wr8(RIRBSIZE, rirb_code);
        hw.wr16(RIRBWP, RIRBWP_RST);
        hw.wr16(RINTCNT, 1);
        hw.wr8(RIRBSTS, RIRBSTS_MASK);
        hw.wr8(RIRBCTL, RIRBCTL_DMA | RIRBCTL_IRQ);
        hw.wr8(CORBCTL, CORBCTL_RUN);
        if !until(hw, 10_000, 10, false, || hw.rd8(CORBCTL) & CORBCTL_RUN != 0) {
            return Err("CORB did not start".into());
        }
        hw.wr32(GCTL, hw.rd32(GCTL) | GCTL_UNSOL);
        let rp = (hw.rd16(RIRBWP) & 0xFF) as usize % rirb_n;
        Ok(Link { mode: Mode::Rings, rings: Some(mem), corb_n, rirb_n, corb_wp: 0, rirb_rp: rp, unsol: Vec::new(), timeouts: 0, strays: 0 })
    }

    /// Stops the rings (thread context) and drops to immediate commands. The buffer goes back to MM only once
    /// the DMA stopped; else it is handed back and the caller must keep it until a link reset (the controller
    /// may still write it).
    #[must_use]
    pub fn stop_rings<H: Hw + ?Sized>(&mut self, hw: &H) -> Option<M> {
        hw.wr8(RIRBCTL, 0);
        hw.wr8(CORBCTL, 0);
        let stopped = until(hw, 100_000, 10, false, || (hw.rd8(CORBCTL) | hw.rd8(RIRBCTL)) & (CORBCTL_RUN | RIRBCTL_DMA) == 0);
        self.mode = Mode::Immediate;
        let mem = self.rings.take();
        if stopped { None } else { mem }
    }

    /// The ring buffer, without stopping anything (the caller keeps it until a link reset).
    pub fn take_rings(&mut self) -> Option<M> {
        self.mode = Mode::Immediate;
        self.rings.take()
    }

    /// Sends `verb` to node `nid` of codec `cad` and returns the answer.
    pub fn cmd<H: Hw + ?Sized>(&mut self, hw: &H, cad: u8, nid: u8, verb: u32) -> Result<u32, CmdError> {
        let word = hdacodec::verb::command(cad, nid, verb).ok_or(CmdError::Bad)?;
        let r = match self.mode {
            Mode::Rings => self.cmd_rings(hw, cad, word),
            Mode::Immediate => self.cmd_immediate(hw, word),
        };
        if r == Err(CmdError::Timeout) {
            self.timeouts += 1;
        }
        r
    }

    fn cmd_rings<H: Hw + ?Sized>(&mut self, hw: &H, cad: u8, word: u32) -> Result<u32, CmdError> {
        if self.rings.is_none() {
            return Err(CmdError::Bad);
        }
        // Collect what came before (unsolicited, late answers) so the next answer is ours.
        self.drain(hw, None);
        let Some(mem) = self.rings.as_ref() else { return Err(CmdError::Bad) };
        let wp = (self.corb_wp + 1) % self.corb_n;
        if !mem.wr32(wp * 4, word) {
            return Err(CmdError::Bad);
        }
        mem.barrier();
        self.corb_wp = wp;
        hw.wr16(CORBWP, wp as u16);
        let mut answer = None;
        let ok = until(hw, ANSWER_US, 2, false, || {
            if answer.is_none() {
                answer = self.drain(hw, Some(cad));
            }
            answer.is_some()
        });
        match (ok, answer) {
            (_, Some(v)) => Ok(v),
            _ if hw.rd32(GCTL) == !0 => Err(CmdError::Gone),
            _ => Err(CmdError::Timeout),
        }
    }

    /// Reads new RIRB entries: unsolicited ones go to `unsol`; the first solicited one from `want` is
    /// returned (others are strays). Also clears the RIRB status (QEMU passes no further answer before).
    pub fn drain<H: Hw + ?Sized>(&mut self, hw: &H, want: Option<u8>) -> Option<u32> {
        let Some(mem) = self.rings.as_ref() else { return None };
        let wp = hw.rd16(RIRBWP);
        if wp == 0xFFFF {
            return None;
        }
        let wp = (wp & 0xFF) as usize % self.rirb_n;
        let mut got = None;
        while self.rirb_rp != wp {
            self.rirb_rp = (self.rirb_rp + 1) % self.rirb_n;
            let off = RIRB_OFF + self.rirb_rp * 8;
            let (Some(res), Some(ex)) = (mem.rd32(off), mem.rd32(off + 4)) else { break };
            let from = (ex & 0xF) as u8;
            if ex & RIRB_EX_UNSOL != 0 {
                self.unsol.push(Unsol { cad: from, res });
            } else if got.is_none() && want == Some(from) {
                got = Some(res);
            } else {
                self.strays += 1;
            }
        }
        hw.wr8(RIRBSTS, RIRBSTS_MASK);
        got
    }

    fn cmd_immediate<H: Hw + ?Sized>(&mut self, hw: &H, word: u32) -> Result<u32, CmdError> {
        if !until(hw, 1000, 2, false, || hw.rd16(ICS) & ICS_BUSY == 0) {
            return Err(if hw.rd16(ICS) == 0xFFFF { CmdError::Gone } else { CmdError::Timeout });
        }
        hw.wr16(ICS, hw.rd16(ICS) | ICS_VALID);
        hw.wr32(IC, word);
        hw.wr16(ICS, (hw.rd16(ICS) & !ICS_VALID) | ICS_BUSY);
        if !until(hw, ANSWER_US, 2, false, || hw.rd16(ICS) & ICS_VALID != 0) {
            return Err(CmdError::Timeout);
        }
        Ok(hw.rd32(IR))
    }
}
