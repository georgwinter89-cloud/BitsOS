//! GX-10 PS: the stage `sos` where no good run goes: a secure OS that shows between two of Linux's loads, a sign of
//! life that goes away again, a firmware buffer the bootloader could not be told of, and a bootloader that got
//! parts from the kernel before a Reload (`rdna4_core::bootnote`).

use super::rdna4boot::loaded;
use super::rdna4fix::*;
use crate::devs::Key;
use crate::gpucore::handoff::{NoteIo, TAG, boot_tag_for, tag_for};
use crate::gpucore::psp::Hw;
use crate::gpucore::trail::Access;
use crate::rdna4::bootnote;
use crate::rdna4::fakemodel::{MC_BASE, Model};
use crate::rdna4::guard::{Allow, Guard};
use crate::rdna4::stage::{Session, Sos, Stage};
use rdna4_core::bootnote::BootNote;
use rdna4_core::bootseq::{Gate, Part, SEQUENCE};
use std::vec::Vec;

const SOL: u32 = 0x0E5B_5A34;

/// The model with its register reads answered by the test: `f` gets the register and what the model said.
struct Reads<F: FnMut(u32, Option<u32>) -> Option<u32>> {
    m: Model,
    f: F,
}

// The stage `sos` asks nothing of the machine's side.
impl<F: FnMut(u32, Option<u32>) -> Option<u32>> crate::rdna4::host::Host for Reads<F> {}

impl<F: FnMut(u32, Option<u32>) -> Option<u32>> Hw for Reads<F> {
    fn reg_read(&mut self, byte: u32) -> Option<u32> {
        let real = self.m.reg_read(byte);
        (self.f)(byte, real)
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

/// The session of a kernel started by a Reload, with what the kernel before handed on about the bootloader.
fn after_reload(handed: &BootNote) -> Session {
    let mut s = Session::default();
    s.boot_before = BootNote::handed(Some(&handed.bytes()));
    s
}

/// The writes that reached the model's two bootloader registers.
fn to_bootloader(m: &Model) -> usize {
    m.writes.iter().filter(|(b, _)| *b == m.regs.psp.c2pmsg_35 || *b == m.regs.c2pmsg_36).count()
}

/// Linux asks for the secure OS before each of its nine loads and returns at once when it runs
/// (psp_v14_0.c:136, :209): one that shows after k parts gets nothing more, and Linux goes on to the ring.
#[test]
fn a_secure_os_that_shows_between_two_parts_gets_nothing_more() {
    let rig = Rig::new();
    for k in [1usize, 4, 8] {
        let mut s = Session::default();
        let mut stick = Stick::full();
        let mut m = model();
        m.boot.alive_after = Some(k);
        let (done, m) = rig.cold_pass(&mut s, m, &mut stick, &[Stage::Sos]);
        let d = &done[0];
        assert_eq!((s.sos, s.boot_sent(), m.boot.taken), (Some(Sos::Warm { sol: SOL }), k, k), "after {k}");
        // Exactly k addresses and k commands went out, and all nine load functions asked first.
        assert_eq!(to_bootloader(&m), 2 * k, "after {k}");
        let c81 = m.regs.psp.c2pmsg_81;
        let asked: Vec<Option<u32>> = d
            .trail
            .iter()
            .filter_map(|a| match a {
                Access::Read { byte, value } if *byte == c81 => Some(*value),
                _ => None,
            })
            .collect();
        let want: Vec<Option<u32>> = (0..9).map(|i| Some(if i < k { 0 } else { SOL })).collect();
        assert_eq!(asked, want, "after {k}");
        // The trail ends with those reads: nothing is written or waited for once the secure OS showed.
        assert!(d.trail[d.trail.len() - (9 - k)..].iter().all(|a| matches!(a, Access::Read { byte, .. } if *byte == c81)), "after {k}");
        let said = std::format!("secure OS alive (C2PMSG_81 0x0e5b5a34) before the {}", SEQUENCE[k].0.name());
        assert_eq!(d.lines.iter().filter(|l| l.starts_with("secure OS alive")).count(), 1, "{:?}", d.lines);
        assert!(d.lines.iter().any(|l| l.starts_with(&said)), "{:?}", d.lines);
        assert_eq!(bootnote::hand_on(&s), BootNote { sent: k as u8, failed: None });
        // As Linux: on to the ring. And the loads do not run a second time.
        let (_, m) = rig.pass(&mut s, m, &mut stick, &[Stage::Ring]);
        assert!(s.ring_up() && m.ring.is_some());
        let writes = m.writes.len();
        let (d, m) = rig.run(&mut s, Stage::Sos, m, &mut stick);
        assert!(!d.pass && d.line.contains("läuft schon") && m.writes.len() == writes, "{}", d.line);
    }
}

/// C2PMSG_81 non-zero and then 0 again is no state Linux knows: the stage stops, and once a part went out the
/// loads are over for this boot.
#[test]
fn a_sign_of_life_that_goes_away_again_stops_the_stage() {
    let rig = Rig::new();
    // The third of the stage's reads shows a secure OS (before the system driver), the fourth none.
    let (mut s, m) = rig.up_to(&[]);
    let c81 = m.regs.psp.c2pmsg_81;
    let mut n = 0;
    let flicker = move |byte: u32, real: Option<u32>| {
        if byte != c81 {
            return real;
        }
        n += 1;
        if n == 3 { Some(SOL) } else { real }
    };
    let mut stick = Stick::full();
    let (d, hw) = rig.run_on(&mut s, Stage::Sos, Reads { m, f: flicker }, &mut stick);
    let m = hw.m;
    assert!(!d.pass && d.line.contains("verhält sich unerwartet"), "{}", d.line);
    assert_eq!(d.lines.last().unwrap(), "stopped: C2PMSG_81 read non-zero and then 0 again before the SOC driver");
    // Two parts went out before, nothing after: the last access is the read that showed 0 again.
    assert_eq!((m.boot.taken, s.boot_sent(), to_bootloader(&m)), (2, 2, 4));
    assert!(m.refused.is_empty() && d.refused.is_empty(), "{:?} {:?}", m.refused, d.refused);
    assert_eq!(s.sos, Some(Sos::Failed { part: Part::SocDrv, c35: 0x8000_0000 }));
    assert_eq!(bootnote::hand_on(&s), BootNote { sent: 2, failed: Some((Part::SocDrv, 0x8000_0000)) });
    let writes = m.writes.len();
    let (again, m) = rig.all(&mut s, m, &mut stick, &[Stage::Sos, Stage::Ring]);
    assert!(again.iter().all(|d| !d.pass && d.totals.writes == 0) && m.writes.len() == writes);
    assert!(again[0].line.contains("aus- und einschalten"), "{}", again[0].line);

    // The same at the very first read, before anything went out: the stage stops and the session records no failed
    // part, but the gate saw a sign of life and stays shut, so a second run reaches the bootloader with nothing.
    let (mut s, m) = rig.up_to(&[]);
    let mut n = 0;
    let first = move |byte: u32, real: Option<u32>| {
        if byte != c81 {
            return real;
        }
        n += 1;
        if n == 1 { Some(SOL) } else { real }
    };
    let (d, hw) = rig.run_on(&mut s, Stage::Sos, Reads { m, f: first }, &mut stick);
    assert!(!d.pass && d.lines.last().unwrap().ends_with("then 0 again before the SPL table"), "{:?}", d.lines.last());
    assert!(s.sos.is_none() && s.boot_sent() == 0 && to_bootloader(&hw.m) == 0);
    let (d, m) = rig.run(&mut s, Stage::Sos, hw.m, &mut stick);
    assert!(!d.pass && d.line.contains("nicht erlaubt") && to_bootloader(&m) == 0 && m.boot.taken == 0 && m.refused.is_empty(), "{}", d.line);
    assert_eq!(d.refused.len(), 1, "{:?}", d.refused);
}

/// The word for C2PMSG_36 is checked before the first access of the loads: a buffer whose MC address is not 1 MiB
/// aligned, or that lies outside the window, stops the stage with nothing written.
#[test]
fn a_buffer_the_bootloader_could_not_be_told_of_stops_before_any_write() {
    let rig = Rig::new();
    let mut stick = Stick::full();
    // VRAM whose MC base is not on a megabyte: the buffer's offset is aligned, its address is not.
    let (mut s, m) = rig.up_to(&[]);
    s.layout.as_mut().unwrap().mc_base += 0x1000;
    let (d, m) = rig.run(&mut s, Stage::Sos, m, &mut stick);
    assert!(!d.pass && d.line.contains("kein sicherer Platz"), "{}", d.line);
    assert_eq!(d.lines.last().unwrap(), "stopped: the firmware buffer is not 1 MiB aligned");
    assert!(m.writes.is_empty() && d.totals.writes == 0 && d.trail.is_empty() && s.sos.is_none() && s.boot_sent() == 0);

    // A buffer made by a run that sent nothing (the bootloader's register did not answer), and a window that is
    // elsewhere afterwards.
    let (mut s, m) = rig.up_to(&[]);
    let c35 = m.regs.psp.c2pmsg_35;
    let deaf = move |byte: u32, real: Option<u32>| if byte == c35 { None } else { real };
    let (d, hw) = rig.run_on(&mut s, Stage::Sos, Reads { m, f: deaf }, &mut stick);
    assert!(!d.pass && d.line.contains("antwortet beim Lesen nicht") && s.sos.is_none() && hw.m.writes.is_empty(), "{}", d.line);
    let pri = s.pri_mc().expect("the first run made the buffer");
    let l = s.layout.as_mut().unwrap();
    l.window = l.window.start + (8 << 20)..l.window.end + (8 << 20);
    assert!(pri < l.window_mc().start);
    let (d, m) = rig.run(&mut s, Stage::Sos, hw.m, &mut stick);
    assert!(!d.pass && d.line.contains("kein sicherer Platz"), "{}", d.line);
    assert_eq!(d.lines.last().unwrap(), "stopped: the firmware buffer lies outside the window");
    assert!(m.writes.is_empty() && d.trail.is_empty() && s.sos.is_none() && s.boot_sent() == 0);
}

/// A kernel after a Reload has a fresh session and a fresh gate. What the kernel before sent to the bootloader
/// reaches it as a note, and with the secure OS not running nothing is sent a second time: not by the stage, and
/// not through the stage's guard either.
#[test]
fn a_kernel_after_a_reload_sends_nothing_to_a_bootloader_that_got_parts() {
    let rig = Rig::new();
    let mut stick = Stick::full();
    // The kernel before: the SOC driver was not answered.
    let mut s = Session::default();
    let mut m = model();
    m.boot.silent_at = Some(3);
    let (_, m) = rig.cold(&mut s, m, &mut stick, &[Stage::Sos]);
    let note = bootnote::hand_on(&s);
    assert_eq!((note, m.boot.taken), (BootNote { sent: 4, failed: Some((Part::SocDrv, 0)) }, 4));
    // What a kernel that ended in the middle of the loads left, and bytes of a version this kernel cannot read.
    let unread = BootNote::handed(Some(b"GXBOOT02")).unwrap();
    for (before, words) in [(note, "sent 4 of the nine bootloader commands, and the SOC driver was not taken (C2PMSG_35 0x00000000)"), (BootNote::IN_FLIGHT, "was sending to the bootloader when it ended"), (unread, "was sending")] {
        let mut s2 = after_reload(&before);
        // The card as that kernel left it: four parts in its bootloader, no secure OS. This kernel starts the three
        // blocks before the secure processor again, as Linux does at every start.
        let (done, cold) = rig.cold(&mut s2, loaded(4), &mut stick, &[Stage::Sos, Stage::Ring, Stage::Sos]);
        for d in &done {
            assert!(!d.pass && d.totals.writes == 0 && d.refused.is_empty(), "{:?}: {}", d.stage, d.line);
        }
        assert!(done[0].line.contains("hat schon Teile bekommen: erst die Karte aus- und einschalten"), "{}", done[0].line);
        let last = done[0].lines.last().unwrap();
        assert!(last.starts_with("stopped: the kernel before this Reload ") && last.contains(words) && last.contains("C2PMSG_81 reads 0"), "{last}");
        // One read (the sign of life) and nothing else reached the card.
        let c81 = cold.regs.psp.c2pmsg_81;
        assert_eq!(done[0].trail, [Access::Read { byte: c81, value: Some(0) }]);
        assert!(cold.writes.is_empty() && cold.boot.taken == 4 && cold.refused.is_empty());
        // The chain of Reloads: this kernel sent nothing and hands on what it inherited.
        assert_eq!((s2.sos, s2.boot_sent(), bootnote::hand_on(&s2)), (None, 0, before));
        assert!(done[1].line.contains("zuerst das sichere Betriebssystem laden"), "{}", done[1].line);
    }
    // A stage that did not look at the note would still not get through: the guard's gate is a spent one.
    let m = loaded(4);
    let r = m.regs.clone();
    let (c35, c36, c81) = (r.psp.c2pmsg_35, r.c2pmsg_36, r.psp.c2pmsg_81);
    let window = (MC_BASE + (32 << 20))..(MC_BASE + (96 << 20));
    let allow = Allow { stage: "test", regs: std::vec![c35, c36, r.hdp_flush], mem: std::vec![window.clone()] };
    let writes = m.writes.len();
    let mut g = Guard::new(m, allow, r).with_gate(Some(Gate::spent(window)));
    assert_eq!(g.reg_read(c81), Some(0));
    while g.reg_read(c35) != Some(0x8000_0000) {}
    assert!(!g.reg_write(c36, 0x8_0020));
    for (_, command) in SEQUENCE {
        assert!(!g.reg_write(c35, command), "{command:#x}");
    }
    assert!(g.hw.writes.len() == writes && g.hw.refused.is_empty() && g.refused.len() == 10);
    // A kernel before that sent nothing leaves a note that changes nothing.
    let mut s3 = after_reload(&BootNote::NOTHING);
    let (_, m) = rig.cold_pass(&mut s3, model(), &mut stick, &[Stage::Sos]);
    assert_eq!((s3.sos, m.boot.taken, bootnote::hand_on(&s3)), (Some(Sos::Loaded { sol: SOL }), 9, BootNote { sent: 9, failed: None }));
}

/// With the secure OS running the note is moot, as it is for Linux, and it is still handed on; a note that says
/// all nine went out on a card whose secure OS does not run is a bootloader that got parts.
#[test]
fn a_secure_os_that_runs_after_the_reload_needs_no_bootloader() {
    let rig = Rig::new();
    let mut stick = Stick::full();
    let (s, m) = rig.up_to(&[Stage::Sos]);
    let note = bootnote::hand_on(&s);
    assert_eq!(note, BootNote { sent: 9, failed: None });
    let mut s2 = after_reload(&note);
    let (done, m) = rig.cold_pass(&mut s2, m, &mut stick, &[Stage::Sos, Stage::Ring, Stage::Destroy]);
    let c35 = m.regs.psp.c2pmsg_35;
    assert!(done[0].totals.writes == 0 && !m.writes.is_empty() && m.writes.iter().all(|(b, _)| *b != c35), "the ring's writes and none to the bootloader");
    // The destroy empties the session of the ring's things only.
    assert_eq!((s2.sos, s2.boot_sent(), s2.boot_before, bootnote::hand_on(&s2)), (Some(Sos::Warm { sol: SOL }), 0, Some(note), note));
    let mut s3 = after_reload(&note);
    let (done, m) = rig.cold(&mut s3, model(), &mut stick, &[Stage::Sos]);
    assert!(!done[0].pass && done[0].line.contains("hat schon Teile bekommen") && m.writes.is_empty(), "{}", done[0].line);
    let (d, _) = rig.run(&mut s3, Stage::State, m, &mut stick);
    assert!(d.lines.iter().any(|l| l.contains("bootloader commands 0 of 9 (the kernel before this Reload sent 9 of the nine bootloader commands)")), "{:?}", d.lines);
}

/// The bootloader's note has a slot of its own per card, beside the card's stop note and the 780M's, and a kernel
/// that never runs the stage hands a spent one on.
#[test]
fn the_bootloader_note_is_keyed_by_the_pci_address_and_kept_across_reloads() {
    use std::sync::Mutex;
    static SLOTS: Mutex<Vec<(u32, Vec<u8>)>> = Mutex::new(Vec::new());
    static PUTS: Mutex<u32> = Mutex::new(0);
    fn put(t: u32, b: &[u8]) -> bool {
        let mut s = SLOTS.lock().unwrap();
        s.retain(|(x, _)| *x != t);
        s.push((t, b.to_vec()));
        *PUTS.lock().unwrap() += 1;
        true
    }
    fn take(t: u32) -> Option<Vec<u8>> {
        SLOTS.lock().unwrap().iter().find(|(x, _)| *x == t).map(|(_, b)| b.clone())
    }
    let io = NoteIo { put, take, from_reload: || true };
    let (a, b) = (Key::new(0, 4, 0, 0), Key::new(0, 7, 0, 0));
    let keys = [a, b, Key::new(0, 0xc7, 0, 0), Key::new(0, 4, 0, 1)];
    let mut tags: Vec<u32> = keys.iter().map(|k| boot_tag_for(*k).unwrap()).collect();
    tags.extend(keys.iter().map(|k| tag_for(*k).unwrap()));
    tags.push(TAG);
    for (i, t) in tags.iter().enumerate() {
        assert!(tags[i + 1..].iter().all(|u| u != t), "{t:#x} twice");
    }
    assert_eq!(boot_tag_for(Key::new(1, 4, 0, 0)), None);
    assert!(!io.put_boot_for(Key::new(1, 4, 0, 0), &BootNote::IN_FLIGHT.bytes()));
    // Nothing handed on, a note that says nothing went out: nothing to keep.
    assert_eq!((bootnote::inherited(&io, a), bootnote::keep(&io, a)), (None, None));
    assert!(bootnote::put(&io, a, &BootNote::NOTHING));
    assert_eq!((bootnote::inherited(&io, a), bootnote::keep(&io, a)), (Some(BootNote::NOTHING), None));
    // A spent one is written again at the attach, for this card only.
    let n = BootNote { sent: 4, failed: Some((Part::SocDrv, 0)) };
    assert!(bootnote::put(&io, a, &n));
    let puts = *PUTS.lock().unwrap();
    assert_eq!(bootnote::keep(&io, a), Some((n, true)));
    assert_eq!((*PUTS.lock().unwrap(), bootnote::inherited(&io, a), bootnote::inherited(&io, b), bootnote::keep(&io, b)), (puts + 1, Some(n), None, None));
    assert_eq!(io.take_note_for(a), None, "the stop note's slot is another");
    // Bytes of another version under the tag count as a sequence in flight, and that is kept too.
    assert!(io.put_boot_for(b, b"GXBOOT09 whatever"));
    assert_eq!(bootnote::keep(&io, b), Some((BootNote::IN_FLIGHT, true)));
    assert_eq!(io.take_boot_for(b).as_deref(), Some(&BootNote::IN_FLIGHT.bytes()[..]));
}

/// Linux writes the remap register without reading it (init.txt line 11), so what a cold card holds there is not
/// known. Whatever it holds: the secure processor's stage does not look at it and makes no access until the
/// chain's stages before it passed in this boot, and after them it holds Linux's byte and the loads go through.
#[test]
fn whatever_the_remap_register_holds_the_loads_wait_for_the_chain_before_them() {
    let rig = Rig::new();
    for cold in [0u32, 0x7_F004, 0x3_F000, 0x7_F001, 0x7_F000 << 1, u32::MAX, 0x7_F000] {
        let mut m = model();
        m.nb.remap_holds(cold);
        let mut s = Session::default();
        let (done, m) = rig.all(&mut s, m, &mut Stick::full(), &[Stage::Alive, Stage::Sos]);
        let says = if cold == 0x7_F000 { "as Linux has it; in this boot the stage nbif did not write it yet" } else { "not Linux's 0x7f000; in this boot the stage nbif did not write it yet" };
        assert!(done[0].pass && done[0].lines.iter().any(|l| l.contains(says)), "{cold:#x}: {:?}", done[0].lines);
        assert!(!done[1].pass && done[1].line.contains("zuerst die Meldungen der Karte einschalten") && done[1].trail.is_empty(), "{cold:#x}: {}", done[1].line);
        assert!(m.writes.is_empty() && m.flushes == 0 && s.boot_sent() == 0 && s.sos.is_none() && !s.nb.remapped, "{cold:#x}");
        let (_, mut m) = rig.cold_pass(&mut s, m, &mut Stick::full(), &[Stage::Sos]);
        let remap = m.regs.hdp_remap;
        assert_eq!((m.reg_read(remap), s.nb.remapped, m.boot.taken), (Some(0x7_F000), true, 9), "{cold:#x}");
    }
}

/// `alive` is a look at the card, outside Linux's chain and needed by no stage of it: these reads, once each, no
/// write, and nothing of the session changed (the window is `gmc`'s to plan, the remap `nbif`'s to write).
#[test]
fn the_look_at_the_card_is_these_reads_and_nothing_else() {
    let rig = Rig::new();
    let mut s = Session::default();
    let (d, m) = rig.run(&mut s, Stage::Alive, model(), &mut Stick::full());
    assert!(s.layout.is_none() && s.bump.is_none() && !s.nb.remapped && s.next() == Some(Stage::Nbif));
    let (p, r) = (m.regs.psp, &m.regs);
    let want = [p.c2pmsg_81, p.c2pmsg_67, p.c2pmsg_71, p.c2pmsg_69, p.c2pmsg_70, p.c2pmsg_64, p.c2pmsg_35, r.c2pmsg_33, r.hdp_remap];
    let got: Vec<u32> = d
        .trail
        .iter()
        .map(|a| match a {
            Access::Read { byte, .. } => *byte,
            other => panic!("the look did more than read: {other:?}"),
        })
        .collect();
    assert_eq!(got, want);
    assert!(d.pass && d.trail_lost == 0 && d.totals.writes == 0 && m.writes.is_empty());
    let names: Vec<&str> = want.iter().map(|b| r.name(*b)).collect();
    assert_eq!(names, ["MPASP_SMN_C2PMSG_81", "MPASP_SMN_C2PMSG_67", "MPASP_SMN_C2PMSG_71", "MPASP_SMN_C2PMSG_69", "MPASP_SMN_C2PMSG_70", "MPASP_SMN_C2PMSG_64", "MPASP_SMN_C2PMSG_35", "MPASP_SMN_C2PMSG_33", "BIF_BX0_REMAP_HDP_MEM_FLUSH_CNTL"]);
}
