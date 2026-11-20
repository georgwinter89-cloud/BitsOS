//! GX-10 PS: the stage that loads the secure OS, and every refusal on the way to the bootloader, at three levels:
//! the stage (a missing or oversized part, a part that fails, a card not ready for it, a second run), the stage's
//! guard with the card's gate (`rdna4_core::bootseq`: nothing but Linux's nine loads at their step), and the model
//! on its own account (what a driver without that guard might send). The comparison with Linux's trace is
//! `linuxseq_ring`.

use super::rdna4fix::*;
use crate::gpucore::fwload::stick_path;
use crate::gpucore::psp::{Hw, PspError, Timeouts};
use crate::rdna4::fakemodel::{MC_BASE, Model};
use crate::rdna4::fwlist;
use crate::rdna4::guard::{Allow, Guard, Refusal};
use crate::rdna4::ring14::Ring14;
use crate::rdna4::stage::{Session, Sos, Stage};
use crate::rdna4::vram::Bump;
use rdna4_core::bootseq::{Gate, Part, SEQUENCE};
use std::vec::Vec;

/// A megabyte of the model's free VRAM holding `part` as `psp_copy_fw` leaves it, flushed; its word for C2PMSG_36.
fn put_part(m: &mut Model, part: &[u8]) -> u32 {
    let off = 32u64 << 20;
    assert!(m.mem_write(off, &std::vec![0u8; 1 << 20]) && m.mem_write(off, part));
    assert!(m.reg_write(m.regs.hdp_flush, 0));
    ((MC_BASE + off) >> 20) as u32
}

/// The model ready for its next command: the ready bit read.
fn ready(m: &mut Model) {
    let c35 = m.regs.psp.c2pmsg_35;
    for _ in 0..64 {
        if m.reg_read(c35) == Some(0x8000_0000) {
            return;
        }
    }
    panic!("the model's bootloader never got ready");
}

/// The first `n` of Linux's loads on the model alone, each as Linux does it, after the three blocks Linux starts
/// before them (`started`).
pub(super) fn loaded(n: usize) -> Model {
    let mut m = started();
    let (c35, c36) = (m.regs.psp.c2pmsg_35, m.regs.c2pmsg_36);
    for (command, part) in linux_boot().into_iter().take(n) {
        ready(&mut m);
        let w = put_part(&mut m, &part);
        assert!(m.reg_write(c36, w) && m.reg_write(c35, command), "{command:#x}: {:?}", m.refused);
    }
    assert!(m.refused.is_empty(), "{:?}", m.refused);
    m
}

#[test]
fn the_model_takes_linuxs_nine_and_only_then_has_a_secure_os() {
    for n in 0..9 {
        let mut m = loaded(n);
        assert_eq!((m.boot.taken, m.sol, m.c64), (n, 0, 0), "after {n}");
        // No secure OS: a ring create is refused, and its ready wait would never end (C2PMSG_64 reads 0).
        let p = m.regs.psp;
        assert_eq!(m.reg_read(p.c2pmsg_64), Some(0));
        for (b, v) in [(p.c2pmsg_69, 0x1000), (p.c2pmsg_70, 0x80), (p.c2pmsg_71, 0x1000), (p.c2pmsg_64, 0x2_0000), (p.c2pmsg_67, 0x10)] {
            assert!(!m.reg_write(b, v), "after {n}: {b:#x} taken without a secure OS");
        }
    }
    let mut m = loaded(9);
    let c81 = m.regs.psp.c2pmsg_81;
    // As the card answered Linux: still 0 at first, then the sign of life; from then on a ring can be made.
    assert_eq!(m.reg_read(c81), Some(0));
    let mut reads = 1;
    while m.reg_read(c81) == Some(0) {
        reads += 1;
        assert!(reads < 100);
    }
    assert_eq!((m.sol, m.c64), (0x0E5B_5A34, 0x80C0_0000));
}

#[test]
fn the_model_refuses_every_bootloader_step_but_linuxs_next() {
    let boot = linux_boot();
    for n in 0..9 {
        let (command, part) = boot[n].clone();
        let check = |what: &str, f: &dyn Fn(&mut Model) -> bool, why: &str| {
            let mut m = loaded(n);
            assert!(!f(&mut m), "after {n}: {what} was taken");
            assert!(m.refused.last().is_some_and(|r| r.contains(why)), "after {n}: {what}: {:?}", m.refused);
            assert_eq!(m.boot.taken, n, "after {n}: {what} counted");
        };
        let (c35, c36) = (model().regs.psp.c2pmsg_35, model().regs.c2pmsg_36);
        // Before the ready bit was read (the echo of the command before, or nothing read at all).
        check("a command before the ready bit", &|m| m.reg_write(c35, command), "before its ready bit was read");
        check("an address before the ready bit", &|m| m.reg_write(c36, 0x8_0020), "before its ready bit was read");
        // Without its address.
        check("a command without its address", &|m| { ready(m); m.reg_write(c35, command) }, "without its buffer's address");
        // The address twice.
        check("the address twice", &|m| { ready(m); let w = put_part(m, &part); m.reg_write(c36, w) && m.reg_write(c36, w) }, "address twice");
        // Every other command of the nine: out of order, or a second time.
        for (i, (other, _)) in boot.iter().enumerate().filter(|(i, _)| *i != n) {
            let why = if i < n { "a second time" } else { "out of Linux's order" };
            check("another of the nine", &|m| { ready(m); let w = put_part(m, &part); m.reg_write(c36, w) && m.reg_write(c35, *other) }, why);
        }
        // Values Linux never sends: memory training, SPDM, USB-PD, the ring's controls, nothing.
        for v in [0x0010_0000u32, 0x0020_0000, 0x2000_0000, 0x0480_0000, 0x0200_0000, 0x0003_0000, 0x0007_0000, 0, command | 1] {
            check("a value Linux never sends", &|m| { ready(m); let w = put_part(m, &part); m.reg_write(c36, w) && m.reg_write(c35, v) }, "which Linux never sends this card");
        }
        // The address: Linux's own GART megabyte, the picture, past VRAM, the firmware's region at the top.
        let vram_mib = (16304u64 << 20) >> 20;
        for w in [0x3u32, 0, 0x8_0000, 0x8_000E, (0x8_0000 + vram_mib) as u32, (0x8_0000 + vram_mib - 1) as u32, (0x8_0000 + vram_mib - 54) as u32, u32::MAX] {
            check("an address outside free VRAM", &|m| { ready(m); m.reg_write(c36, w) }, "no free megabyte of VRAM");
        }
        // The buffer: not this part, the part with something behind it, the part not at the megabyte's start (an
        // unaligned buffer whose address was cut to megabytes), bytes not flushed.
        check("another part's bytes", &|m| { ready(m); let w = put_part(m, &boot[(n + 1) % 9].1); m.reg_write(c36, w) && m.reg_write(c35, command) }, "does not hold that part");
        check("bytes behind the part", &|m| {
            ready(m);
            let w = put_part(m, &part);
            m.mem_write((32 << 20) + 0xF_F000, &[1]) && m.reg_write(m.regs.hdp_flush, 0) && m.reg_write(c36, w) && m.reg_write(c35, command)
        }, "does not hold that part");
        check("an unaligned buffer", &|m| {
            ready(m);
            let off = (32u64 << 20) + 0x1000;
            m.mem_write(off, &part) && m.reg_write(m.regs.hdp_flush, 0) && m.reg_write(c36, ((MC_BASE + off) >> 20) as u32) && m.reg_write(c35, command)
        }, "does not hold that part");
        check("bytes not flushed", &|m| {
            ready(m);
            let w = put_part(m, &part);
            m.mem_write(48 << 20, &[0]) && m.reg_write(c36, w) && m.reg_write(c35, command)
        }, "not flushed through the HDP");
    }
}

#[test]
fn the_model_refuses_the_bootloader_once_the_secure_os_runs() {
    // Right after the ninth command the secure OS is not up yet, and the bootloader takes nothing more either.
    let mut m = loaded(9);
    let (c35, c36, c81) = (m.regs.psp.c2pmsg_35, m.regs.c2pmsg_36, m.regs.psp.c2pmsg_81);
    ready(&mut m);
    let w = put_part(&mut m, &linux_boot()[0].1);
    assert!(m.reg_write(c36, w));
    assert!(!m.reg_write(c35, 0x8_0000) && m.refused.last().unwrap().contains("after all nine"), "{:?}", m.refused);
    while m.reg_read(c81) == Some(0) {}
    for (b, v) in [(c36, w), (c35, 0x8_0000), (c35, 0x2_0000), (c35, 0)] {
        assert!(!m.reg_write(b, v));
        assert!(m.refused.last().unwrap().contains("while the secure OS runs"), "{:?}", m.refused);
    }
    // A card that was warm from the start.
    let mut m = warm();
    ready(&mut m);
    assert!(!m.reg_write(c36, w) && !m.reg_write(c35, 0x8_0000));
    assert_eq!(m.boot.taken, 9);
}

/// Through the stage's guard nothing reaches the card's bootloader but Linux's next step: the guard refuses before
/// the model is even asked.
#[test]
fn the_guard_lets_only_linuxs_next_step_through() {
    let m = started();
    let r = m.regs.clone();
    let (c35, c36, c81) = (r.psp.c2pmsg_35, r.c2pmsg_36, r.psp.c2pmsg_81);
    let window = (MC_BASE + (32 << 20))..(MC_BASE + (96 << 20));
    let allow = Allow { stage: "test", regs: std::vec![c35, c36, r.hdp_flush], mem: std::vec![window.clone()] };
    let boot = linux_boot();
    let mut g = Guard::new(m, allow, r.clone()).with_gate(Some(Gate::new(window)));
    for (n, (command, part)) in boot.iter().enumerate() {
        // Nothing before the sign of life was read as 0 and the bootloader as ready.
        assert!(!g.reg_write(c36, 0x8_0020) && !g.reg_write(c35, *command));
        assert_eq!(g.reg_read(c81), Some(0));
        assert!(!g.reg_write(c36, 0x8_0020));
        while g.reg_read(c35) != Some(0x8000_0000) {}
        // An address outside the window, a command before its address.
        for w in [0x3, 0x8_001F, 0x8_0060] {
            assert!(!g.reg_write(c36, w), "{w:#x}");
        }
        assert!(!g.reg_write(c35, *command));
        let w = put_part(&mut g.hw, part);
        assert!(g.reg_write(c36, w));
        // Every value but Linux's next.
        for (i, (other, _)) in boot.iter().enumerate() {
            assert_eq!(g.verdict(c35, *other).is_none(), i == n, "after {n}: {other:#x}");
        }
        for v in [0x0010_0000, 0x0020_0000, 0x2000_0000, 0x0480_0000, 0x0200_0000, 0, 1, u32::MAX] {
            assert!(!g.reg_write(c35, v), "after {n}: {v:#x}");
        }
        assert!(g.reg_write(c35, *command), "after {n}: {:?} {:?}", g.refused, g.hw.refused);
        assert!(!g.reg_write(c35, *command), "a second time");
    }
    assert!(g.hw.refused.is_empty(), "the model saw only Linux's sequence: {:?}", g.hw.refused);
    assert_eq!((g.hw.boot.taken, g.boot.as_ref().map(Gate::sent)), (9, Some(9)));
    assert!(g.refused.iter().all(|r| matches!(r, Refusal::Reg { .. })));
    // The secure OS comes up: the gate is shut for good, whatever is read later.
    while g.reg_read(c81) == Some(0) {}
    while g.reg_read(c35) != Some(0x8000_0000) {}
    assert!(!g.reg_write(c36, 0x8_0020) && !g.reg_write(c35, 0x8_0000));
    assert!(g.boot.as_ref().is_some_and(Gate::alive));
}

fn cut(stick: &mut Stick, change: impl Fn(&mut Vec<u8>)) {
    change(stick.0.get_mut(&stick_path(fwlist::SOS)).unwrap());
}

/// The descriptor of this type in the fixture's secure OS file: the byte its entry starts at.
fn desc_at(file: &[u8], kind: u32) -> usize {
    (0..11).map(|i| 36 + i * 16).find(|at| file[*at..*at + 4] == kind.to_le_bytes()).unwrap()
}

#[test]
fn a_part_missing_or_larger_than_the_buffer_stops_before_any_write() {
    let rig = Rig::new();
    for (part, _) in SEQUENCE {
        // Its size 0 (Linux would skip it), its descriptor gone (another type in its place).
        for gone in [false, true] {
            let mut stick = Stick::full();
            cut(&mut stick, |b| {
                let at = desc_at(b, part as u32);
                if gone {
                    b[at..at + 4].copy_from_slice(&0x77u32.to_le_bytes());
                } else {
                    b[at + 12..at + 16].copy_from_slice(&0u32.to_le_bytes());
                }
            });
            let mut s = Session::default();
            let (done, m) = rig.cold(&mut s, model(), &mut stick, &[Stage::Sos]);
            assert!(!done[0].pass && done[0].line.contains("passt nicht"), "{part:?}: {}", done[0].line);
            assert!(done[0].lines.last().unwrap().contains(part.name()), "{part:?}: {:?}", done[0].lines);
            assert!(m.writes.is_empty() && done[0].totals.writes == 0 && s.sos.is_none() && s.boot_sent() == 0, "{part:?}");
        }
    }
    // A part of more than a megabyte: the file made that large, the last part's descriptor naming all of it.
    let mut stick = Stick::full();
    cut(&mut stick, |b| {
        let old = b.len();
        b.resize(old + (1 << 20) + 16, 0x5A);
        let len = b.len() as u32;
        b[0..4].copy_from_slice(&len.to_le_bytes());
        let at = desc_at(b, Part::IpKeyMgr as u32);
        b[at + 12..at + 16].copy_from_slice(&((1u32 << 20) + 1).to_le_bytes());
        b[at + 8..at + 12].copy_from_slice(&(old as u32 - 256).to_le_bytes());
    });
    let mut s = Session::default();
    let (done, m) = rig.cold(&mut s, model(), &mut stick, &[Stage::Sos]);
    assert!(!done[0].pass && done[0].lines.last().unwrap().contains("more than the 1 MiB buffer"), "{:?}", done[0].lines);
    assert!(m.writes.is_empty() && s.sos.is_none());
    // No file at all, and a file that is no v2.0 secure OS file.
    let mut stick = Stick::full();
    stick.0.remove(&stick_path(fwlist::SOS));
    let (done, m) = rig.cold(&mut Session::default(), model(), &mut stick, &[Stage::Sos]);
    assert!(!done[0].pass && done[0].line.contains("fehlt auf dem Stick") && m.writes.is_empty(), "{}", done[0].line);
    let mut stick = Stick::full();
    cut(&mut stick, |b| b[8] = 1);
    let (done, m) = rig.cold(&mut Session::default(), model(), &mut stick, &[Stage::Sos]);
    assert!(!done[0].pass && m.writes.is_empty(), "{}", done[0].line);
}

#[test]
fn a_part_the_bootloader_does_not_answer_ends_the_loads_and_says_which() {
    let rig = Rig::new();
    for (i, plain, name) in [(3usize, "Chipteil", "SOC driver"), (0, "Schlüsselliste", "key database")] {
        let mut s = Session::default();
        let mut stick = Stick::full();
        let mut m = model();
        m.boot.silent_at = Some(i);
        let (done, m) = rig.cold(&mut s, m, &mut stick, &[Stage::Sos]);
        let d = &done[0];
        assert!(!d.pass && d.line.contains(plain), "{}", d.line);
        // One line: the part and what C2PMSG_35 held when Linux's 1000000 reads were through.
        let last = d.lines.last().unwrap();
        assert!(last.starts_with(&std::format!("stopped: the {name} failed, C2PMSG_35 held 0x00000000: ")), "{last}");
        assert_eq!(s.sos, Some(Sos::Failed { part: SEQUENCE[i].0, c35: 0 }));
        // The card is left as it is: exactly the parts up to this one went out, nothing after it.
        assert_eq!((m.boot.taken, s.boot_sent()), (i + 1, i + 1));
        assert!(m.refused.is_empty() && d.refused.is_empty(), "{:?}", m.refused);
        let writes = m.writes.len();
        // Nothing more in this boot: not the loads again, not a ring.
        let (again, m) = rig.all(&mut s, m, &mut stick, &[Stage::Sos, Stage::Ring, Stage::Toc]);
        assert!(again.iter().all(|d| !d.pass && d.totals.writes == 0) && m.writes.len() == writes, "{:?}", again.iter().map(|d| &d.line).collect::<Vec<_>>());
        assert!(again[0].line.contains("aus- und einschalten"), "{}", again[0].line);
    }
}

#[test]
fn a_secure_os_that_never_shows_fails_after_linuxs_100000_reads() {
    let rig = Rig::new();
    let mut s = Session::default();
    let mut m = model();
    m.boot.silent_at = Some(8);
    let (done, m) = rig.cold(&mut s, m, &mut Stick::full(), &[Stage::Sos]);
    let d = &done[0];
    assert!(!d.pass && d.line.contains("sichere Betriebssystem ist nicht gestartet"), "{}", d.line);
    assert!(d.lines.iter().any(|l| l.contains("MPASP_SMN_C2PMSG_81 until the value is no longer 0x00000000: 100000 reads")), "{:?}", d.lines);
    assert!(matches!(s.sos, Some(Sos::Failed { part: Part::Sos, .. })) && m.boot.taken == 9 && m.sol == 0);
}

#[test]
fn a_secure_os_up_within_the_20_ms_is_taken_without_linuxs_wait() {
    let rig = Rig::new();
    let mut s = Session::default();
    let mut m = model();
    m.boot.sos_at_once = true;
    let (done, m) = rig.cold_pass(&mut s, m, &mut Stick::full(), &[Stage::Sos]);
    assert_eq!(s.sos, Some(Sos::Loaded { sol: 0x0E5B_5A34 }));
    assert!(done[0].lines.iter().any(|l| l.contains("up within the 20 ms")), "{:?}", done[0].lines);
    assert_eq!(done[0].lines.iter().filter(|l| l.starts_with("WAIT ")).count(), 17, "no wait on C2PMSG_81");
    assert_eq!(m.sol, 0x0E5B_5A34);
}

#[test]
fn the_stage_needs_the_three_blocks_before_it_and_runs_once() {
    let rig = Rig::new();
    // Linux's order of blocks: nothing ran, then each of the three missing in turn. No access at all.
    for ran in 0..NB.len() {
        let mut s = Session::default();
        let (_, m) = rig.pass(&mut s, model(), &mut Stick::full(), &NB[..ran]);
        let writes = m.writes.len();
        let (d, m) = rig.run(&mut s, Stage::Sos, m, &mut Stick::full());
        assert!(!d.pass && d.line.contains("zuerst die Meldungen der Karte einschalten") && d.trail.is_empty() && m.writes.len() == writes, "after {ran}: {}", d.line);
        assert!(d.lines.last().unwrap().contains("sos needs the ih stage of this boot"), "{:?}", d.lines);
        assert_eq!((s.before(Stage::Sos), s.next()), (Some((Stage::Ih, false)), Some(NB[ran])));
    }
    // A second run after a good one: nothing is written.
    let (mut s, m) = rig.up_to(&[Stage::Sos]);
    let writes = m.writes.len();
    let (d, m) = rig.run(&mut s, Stage::Sos, m, &mut Stick::full());
    assert!(!d.pass && d.line.contains("läuft schon") && d.totals.writes == 0 && m.writes.len() == writes, "{}", d.line);
    // The second card stays untouched.
    let mut rig2 = Rig::new();
    rig2.display = false;
    let (d, m) = rig2.run(&mut Session::default(), Stage::Sos, model(), &mut Stick::full());
    assert!(!d.pass && d.line.contains("unberührt") && m.writes.is_empty(), "{}", d.line);
}

/// The model on its own account, for a driver that skipped the chain: before `nbif` wrote the remap Linux's
/// flush place flushes nothing, and the bootloader takes nothing before GMC and IH are started. And once the
/// remap is written the flush register itself is not Linux's place.
#[test]
fn a_flush_without_the_remap_does_not_count() {
    let mut m = model();
    ready(&mut m);
    let (c35, c36, flush) = (m.regs.psp.c2pmsg_35, m.regs.c2pmsg_36, m.regs.hdp_flush);
    assert!(m.mem_write(32 << 20, &linux_boot()[0].1) && !m.reg_write(flush, 0));
    assert!(m.refused.last().unwrap().contains("before the remap was written") && m.flushes == 0, "{:?}", m.refused);
    assert!(!m.reg_write(c36, 0x8_0020) && !m.reg_write(c35, 0x8_0000));
    assert!(m.refused.last().unwrap().contains("before GMC and IH were started"), "{:?}", m.refused);
    assert_eq!(m.boot.taken, 0);
    let mut m = started();
    let direct = m.regs.hdp_direct;
    assert_eq!(m.regs.name(direct), "BIF_BX_PF0_HDP_MEM_COHERENCY_FLUSH_CNTL");
    assert!(!m.reg_write(direct, 0) && m.refused.last().unwrap().contains("through the register itself after the remap"), "{:?}", m.refused);
}

/// Without a secure OS there is no ring: the stage refuses in the session, and the ring's own ready wait runs out
/// after Linux's 100000 reads without a write.
#[test]
fn a_ring_before_the_secure_os_fails() {
    let rig = Rig::new();
    let (mut s, m) = rig.up_to(&[]);
    let (d, m) = rig.run(&mut s, Stage::Ring, m, &mut Stick::full());
    assert!(!d.pass && d.line.contains("zuerst das sichere Betriebssystem laden") && d.totals.writes == 0 && d.trail.is_empty(), "{}", d.line);
    let l = s.layout.clone().unwrap();
    let mut bump = Bump::new(&l);
    let mut ring = Ring14::new(&mut bump, &l, m.regs.psp, Timeouts::LINUX).unwrap();
    let mut hw = crate::rdna4::guard::Mc { hw: m, base: MC_BASE, bytes: 16304 << 20 };
    assert_eq!(ring.create(&mut hw), Err(PspError::Timeout { what: "ring.ready", last: 0 }));
    assert!(hw.hw.writes.is_empty() && !ring.sent() && !ring.created());
    assert_eq!(hw.hw.card.now_us, 100_000, "Linux's 100000 reads, 1 us apart");
}

/// A card that stops answering in the middle of the loads: the session knows parts went out, and nothing is sent
/// to that bootloader again in this boot.
#[test]
fn a_card_that_goes_silent_between_two_parts_ends_the_loads() {
    struct Gone {
        m: Model,
        /// Reads of C2PMSG_81 left before the card reads all ones.
        left: u32,
    }
    // The stage `sos` asks nothing of the machine's side.
    impl crate::rdna4::host::Host for Gone {}
    impl Hw for Gone {
        fn reg_read(&mut self, byte: u32) -> Option<u32> {
            if byte == self.m.regs.psp.c2pmsg_81 {
                if self.left == 0 {
                    return Some(u32::MAX);
                }
                self.left -= 1;
            }
            self.m.reg_read(byte)
        }
        fn reg_write(&mut self, byte: u32, value: u32) -> bool {
            self.m.reg_write(byte, value)
        }
        fn mem_read(&mut self, at: u64, buf: &mut [u8]) -> bool {
            self.m.mem_read(at, buf)
        }
        fn mem_write(&mut self, at: u64, data: &[u8]) -> bool {
            self.m.mem_write(at, data)
        }
        fn now_us(&mut self) -> u64 {
            self.m.now_us()
        }
        fn delay_us(&mut self, us: u64) {
            self.m.delay_us(us)
        }
    }
    let rig = Rig::new();
    let (mut s, m) = rig.up_to(&[]);
    let (d, hw) = rig.run_on(&mut s, Stage::Sos, Gone { m, left: 3 }, &mut Stick::full());
    assert!(!d.pass && d.line.contains("antwortet beim Lesen nicht") && hw.m.boot.taken == 3, "{} after {}", d.line, hw.m.boot.taken);
    assert!(d.lines.last().unwrap().starts_with("stopped: the SOC driver failed, C2PMSG_35 held 0x80000000: Gone"), "{:?}", d.lines.last());
    assert_eq!((s.sos, s.boot_sent()), (Some(Sos::Failed { part: Part::SocDrv, c35: 0x8000_0000 }), 3));
    let writes = hw.m.writes.len();
    let (d, m) = rig.run(&mut s, Stage::Sos, hw.m, &mut Stick::full());
    assert!(!d.pass && d.totals.writes == 0 && m.writes.len() == writes && m.refused.is_empty(), "{}", d.line);
}
