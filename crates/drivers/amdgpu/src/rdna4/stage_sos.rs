//! GX-10 PS: the stage `sos`, the card from cold to a running secure OS: Linux's nine bootloader loads of
//! `psp_hw_start` (amdgpu_psp.c:2961-3048), each as `psp_v14_0_bootloader_load_component` does it
//! (psp_v14_0.c:127-159), the last as `psp_v14_0_bootloader_load_sos` (:202-237). The trace of Linux's own cold
//! start on this card is its measure (section psp-boot, init.txt lines 673-751): C2PMSG_81 reads 0 there, and the
//! stage's accesses equal that section, register for register.
//!
//! Per part: return at once when C2PMSG_81 is not 0; wait for the bootloader (bit 31 of C2PMSG_35); the part into
//! the 1 MiB firmware buffer (`psp_copy_fw`); the buffer's address >> 20 into C2PMSG_36, the command into
//! C2PMSG_35; wait for the bootloader again. The secure OS instead: 20 ms, then C2PMSG_81 until it changes.
//!
//! Two declared differences (the test's rules): the buffer's address, which is BitsOS's own (VRAM, where Linux's
//! is system memory behind the GART at MC 0x300000), and one HDP flush before the address is written (the bytes
//! went through BAR0 and must have reached the card's memory before the bootloader reads them).
//!
//! Stricter than Linux: a part the file does not hold stops the stage before any write (Linux skips it,
//! amdgpu_psp.c:2961), and a part that failed ends the loads until the card was off (the bootloader keeps what it
//! took, and no command is ever sent twice: `rdna4_core::bootseq` in this boot, `rdna4_core::bootnote` across a
//! Reload).

use super::fwlist;
use super::linuxseq::rules::{Address, Rule};
use super::pspfiles::SosFile;
use super::regs14::Regs14;
use super::stage::{Ctx, Session, Sos, Stop, need_pri, psp_stop, read, read_file, stop};
use super::stage_ro::{BOOTLOADER_IS_READY, BOOTLOADER_POLLS};
use super::vram::Layout;
use crate::gpucore::fwload::FwSource;
use crate::gpucore::psp::{self, Cond, Hw, PspError};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::ops::Range;
use rdna4_core::bootnote::BootNote;
use rdna4_core::bootseq::{self, Part, SEQUENCE};

/// `mdelay(20)` after the secure OS command (psp_v14_0.c:229-230).
pub const SOS_SETTLE_US: u64 = 20_000;

/// Where this stage differs from Linux's trace (section psp-boot) and says so: the comparison's rules.
pub fn linux_rules(l: &Layout) -> [Rule; 2] {
    [
        Rule::HdpBefore { reg: "MPASP_SMN_C2PMSG_36", why: "the firmware buffer lies in VRAM and was written through BAR0; Linux's is system memory behind the GART" },
        Rule::Address(Address {
            lo: "MPASP_SMN_C2PMSG_36",
            hi: None,
            shift: bootseq::ADDRESS_SHIFT as u8,
            keep: 0,
            align: bootseq::BUFFER_BYTES,
            within: l.window_mc(),
            bytes: bootseq::BUFFER_BYTES,
            why: "the 1 MiB firmware buffer lies in BitsOS's window of VRAM (spec section 5); Linux's is GART MC 0x300000",
        }),
    ]
}

/// What one of Linux's load functions found when it asked for the secure OS first.
enum Asked {
    /// C2PMSG_81 was not 0: Linux returns at once. The value is the first such that was read.
    Alive(u32),
    /// The part is in the buffer and its command went out.
    Sent,
}

/// One run of the nine: what is the same for every part, and what the run saw so far.
struct Run<'a> {
    r: &'a Regs14,
    file: &'a [u8],
    pri_mc: u64,
    word: u32,
    /// A command of this run reached the bootloader, so whatever stops the stage from then on ends the loads for
    /// this boot (the session remembers it; the gate would refuse a repeat anyway).
    took: bool,
    /// The secure OS's sign of life, once C2PMSG_81 read non-zero.
    warm: Option<u32>,
}

pub(super) fn sos(s: &mut Session, c: &Ctx, hw: &mut dyn Hw, r: &Regs14, files: &mut dyn FwSource, out: &mut Vec<String>) -> Result<(), Stop> {
    match s.sos {
        Some(Sos::Failed { part, c35 }) => {
            return Err(stop("ein Teil wurde schon abgelehnt: erst die Karte aus- und einschalten", format!("the {} was not taken in this boot (C2PMSG_35 {c35:#010x}); the bootloader keeps what it got, nothing is sent twice", part.name())));
        }
        Some(_) => return Err(stop("das sichere Betriebssystem läuft schon", "the sos stage ran in this boot")),
        None => {}
    }
    // The chain's order check came before this (`stage::in_order`): `ih` passed on a GART that `gmc` made, with the
    // window it planned, after `nbif` moved the HDP flush to the place this stage flushes at.
    // The bootloader keeps what the kernel before this Reload sent it (`rdna4_core::bootnote`): only a secure OS
    // that runs after all makes that moot. Otherwise the card must have been off before anything is sent again.
    if let Some(n) = s.boot_before.filter(BootNote::spent)
        && read(hw, r.psp.c2pmsg_81)? == 0
    {
        return Err(stop("der Startlader der Karte hat schon Teile bekommen: erst die Karte aus- und einschalten", format!("the kernel before this Reload {n}; C2PMSG_81 reads 0, and nothing is sent to a bootloader twice in one power cycle")));
    }
    let l = s.layout.clone().ok_or_else(|| stop("kein sicherer Platz im Speicher der Karte", "no window planned: the stage gmc plans it"))?;
    // Everything the nine commands need is checked before the first of them: a part missing or too large must
    // not leave the bootloader with half a set.
    let name = fwlist::name_for(fwlist::SOS, c.facts.device, c.facts.revision);
    let file = read_file(files, name)?;
    let parsed = SosFile::parse(&file).map_err(|e| stop("eine Datei auf dem Stick passt nicht", format!("{name}: {e:?}")))?;
    let mut found: [(Range<usize>, u32); 9] = core::array::from_fn(|_| (0..0, 0));
    for (slot, (part, _)) in found.iter_mut().zip(SEQUENCE) {
        let d = parsed.boot(part).ok_or_else(|| stop("eine Datei auf dem Stick passt nicht", format!("{name} holds no {} (descriptor type {})", part.name(), part as u32)))?;
        if d.range.len() as u64 > bootseq::BUFFER_BYTES {
            return Err(stop("eine Datei auf dem Stick passt nicht", format!("{name}: the {} has {} bytes, more than the 1 MiB buffer", part.name(), d.range.len())));
        }
        *slot = (d.range.clone(), d.version);
    }
    let pri_mc = need_pri(s)?.buf.mc;
    let word = bootseq::address_word(pri_mc, &l.window_mc()).map_err(|why| stop("kein sicherer Platz im Speicher der Karte", why))?;
    out.push(format!("firmware buffer: 1 MiB at mc {pri_mc:#x} (C2PMSG_36 {word:#x}), in the window {:#x}..{:#x}", l.window.start, l.window.end));
    let mut run = Run { r, file: &file, pri_mc, word, took: false, warm: None };
    // Eight parts the bootloader answers itself, then the secure OS, which answers in C2PMSG_81 (`SEQUENCE` ends
    // with it): the stage has no way to end without one of the secure OS's two outcomes.
    let [drivers @ .., (os, os_command)] = SEQUENCE;
    let [ranges @ .., (os_range, version)] = found;
    for ((part, command), (range, _)) in drivers.into_iter().zip(ranges) {
        if let Asked::Sent = run.command(s, hw, part, command, &range, out)? {
            if let Err(e) = psp::wait_reads(hw, r.psp.c2pmsg_35, BOOTLOADER_IS_READY, BOOTLOADER_POLLS, "bootloader") {
                return Err(run.failed(s, hw, part, e));
            }
            out.push(format!("{} loaded: {} bytes, command {command:#010x}", part.name(), range.len()));
            klog::event!(Info, "gx", "rdna4.sos.part", card = c.index, part = part.name(), command = klog::Hex(u64::from(command)), bytes = range.len(); "the bootloader took a part");
        }
    }
    match run.command(s, hw, os, os_command, &os_range, out)? {
        // Alive from before, or from between two parts: Linux goes on to the ring; here the ring stage looks for
        // a ring from before first.
        Asked::Alive(sol) => s.sos = Some(Sos::Warm { sol }),
        Asked::Sent => {
            hw.delay_us(SOS_SETTLE_US);
            let from = match psp::read(hw, r.psp.c2pmsg_81) {
                Ok(v) => v,
                Err(e) => return Err(run.failed(s, hw, os, e)),
            };
            // Linux waits for a change from what it reads after its 20 ms. A secure OS that came up within them
            // shows here already (Linux's wait would then run out on a live secure OS); the sign of life itself
            // is the answer.
            let sol = if from != 0 {
                out.push(String::from("the secure OS was up within the 20 ms: no wait for a change"));
                from
            } else {
                match psp::wait_reads(hw, r.psp.c2pmsg_81, Cond::Changed { from }, psp::LINUX_POLLS, "secure os") {
                    Ok(v) => v,
                    Err(e) => return Err(run.failed(s, hw, os, e)),
                }
            };
            out.push(format!("secure OS loaded: {} bytes, version {version:#x}; C2PMSG_81 {sol:#010x}", os_range.len()));
            klog::event!(Info, "gx", "rdna4.sos.up", card = c.index, sol = klog::Hex(u64::from(sol)), version = klog::Hex(u64::from(version)); "the card's secure OS runs, loaded through its bootloader");
            s.sos = Some(Sos::Loaded { sol });
        }
    }
    Ok(())
}

impl Run<'_> {
    /// One of Linux's load functions up to its command (psp_v14_0.c:127-150, :202-228).
    fn command(&mut self, s: &mut Session, hw: &mut dyn Hw, part: Part, command: u32, range: &Range<usize>, out: &mut Vec<String>) -> Result<Asked, Stop> {
        let r = self.r;
        // psp_v14_0_is_sos_alive: every one of Linux's nine load functions asks first and returns at once.
        let sol = match psp::read(hw, r.psp.c2pmsg_81) {
            Ok(v) => v,
            Err(e) => return Err(self.failed(s, hw, part, e)),
        };
        if sol != 0 {
            if self.warm.is_none() {
                out.push(format!("secure OS alive (C2PMSG_81 {sol:#010x}) before the {}: nothing is sent from here on, as Linux", part.name()));
            }
            return Ok(Asked::Alive(*self.warm.get_or_insert(sol)));
        }
        if self.warm.is_some() {
            // A sign of life that went away again is no state Linux knows: nothing more goes to this bootloader.
            let c35 = hw.reg_read(r.psp.c2pmsg_35).unwrap_or(u32::MAX);
            self.ended(s, part, c35);
            return Err(stop("Sicherheitsprozessor verhält sich unerwartet", format!("C2PMSG_81 read non-zero and then 0 again before the {}", part.name())));
        }
        if let Err(e) = psp::wait_reads(hw, r.psp.c2pmsg_35, BOOTLOADER_IS_READY, BOOTLOADER_POLLS, "bootloader") {
            return Err(self.failed(s, hw, part, e));
        }
        let sent = (|| {
            s.pri.as_mut().ok_or(PspError::Mem { mc: self.pri_mc })?.load(hw, &self.file[range.clone()])?;
            hw.flush_hdp();
            psp::write(hw, r.c2pmsg_36, self.word)?;
            psp::write(hw, r.psp.c2pmsg_35, command)
        })();
        if let Err(e) = sent {
            return Err(self.failed(s, hw, part, e));
        }
        self.took = true;
        Ok(Asked::Sent)
    }

    /// The loads end at this part: once a command of this run went out, for this boot.
    fn ended(&self, s: &mut Session, part: Part, c35: u32) {
        if self.took {
            s.sos = Some(Sos::Failed { part, c35 });
        }
        klog::event!(Error, "gx", "rdna4.sos.failed", part = part.name(), c35 = klog::Hex(u64::from(c35)), took = self.took; "a bootloader load failed: nothing more is sent to this bootloader in this boot once a part went out");
    }

    /// A part that was not taken: the card stays as it is, and the one line says which part and what C2PMSG_35
    /// held.
    fn failed(&self, s: &mut Session, hw: &mut dyn Hw, part: Part, e: PspError) -> Stop {
        let c35 = match e {
            PspError::Timeout { what: "bootloader", last } => last,
            _ => hw.reg_read(self.r.psp.c2pmsg_35).unwrap_or(u32::MAX),
        };
        self.ended(s, part, c35);
        // The part's own failure is a wait that ran out (Linux knows no other sign); anything else keeps its own
        // words.
        let own = matches!(e, PspError::Timeout { .. });
        let Stop(plain, detail) = psp_stop(e);
        Stop(if own { part_plain(part) } else { plain }, format!("the {} failed, C2PMSG_35 held {c35:#010x}: {detail}", part.name()))
    }
}

/// The plain cause, naming the part in Georg's words.
fn part_plain(part: Part) -> &'static str {
    match part {
        Part::Kdb => "die Karte hat die Schlüsselliste nicht angenommen",
        Part::Spl => "die Karte hat die Sicherheitstabelle nicht angenommen",
        Part::SysDrv => "die Karte hat den Systemteil nicht angenommen",
        Part::SocDrv => "die Karte hat den Chipteil nicht angenommen",
        Part::IntfDrv => "die Karte hat den Schnittstellenteil nicht angenommen",
        Part::DbgDrv => "die Karte hat den Prüfteil nicht angenommen",
        Part::RasDrv => "die Karte hat den Fehlerteil nicht angenommen",
        Part::IpKeyMgr => "die Karte hat die Schlüsselverwaltung nicht angenommen",
        Part::Sos => "das sichere Betriebssystem ist nicht gestartet",
    }
}
