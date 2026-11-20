//! GX-10 PS: the secure processor's stages held against the register trace of Linux's own cold start on this card,
//! whole sections, every line: the stage `sos` against psp-boot (the nine bootloader loads, init.txt lines
//! 673-751) and the stages ring, toc, tmr, fw, autoload, fwrest and ta against psp-ring (ring create and the 31
//! ring commands, lines 752-892). Equal means: the same registers in the same order with the same values, the same
//! waits with Linux's conditions and bounds, the same sleeps, and the fake card answering as the real one did; the
//! only differences are the declared ones (`stage_sos::linux_rules`, `stage_ring::linux_rules`). And the
//! comparison is no rubber stamp: taking a rule away or changing an access is named.

use super::linuxseq::bus;
use super::rdna4fix::*;
use crate::gpucore::psp::{Bound, Cond, cmd};
use crate::rdna4::linuxseq::compare::{Compare, Kind, render};
use crate::rdna4::linuxseq::fold::{Did, fold};
use crate::rdna4::linuxseq::rules::Rule;
use crate::rdna4::linuxseq::section;
use crate::rdna4::stage::{Done, Session, Sos, Stage};
use crate::rdna4::{stage_ring, stage_sos};
use std::string::ToString;
use std::vec::Vec;

/// The secure processor's stages on a cold model, after the three blocks before them: the session, each stage's
/// answer (`ORDER`: `sos` first) and the model with its writes from `sos` on. The whole chain against all of its
/// sections is `rdna4chain`.
fn chain() -> (Session, Vec<Done>, crate::rdna4::fakemodel::Model) {
    let rig = Rig::new();
    let mut s = Session::default();
    let (done, m) = rig.cold_pass(&mut s, model(), &mut Stick::full(), &ORDER);
    (s, done, m)
}

fn compare(name: &str, rules: &[Rule], done: &[Done]) -> Result<(), crate::rdna4::linuxseq::compare::Diff> {
    let sec = section(name).unwrap();
    let mut c = Compare::new(&sec, rules)?;
    for d in done {
        c.feed_trail(&d.trail, d.trail_lost, &bus())?;
    }
    c.finish()
}

#[test]
fn the_sos_stage_equals_linuxs_nine_bootloader_loads() {
    let (s, done, m) = chain();
    let l = s.layout.clone().unwrap();
    let text = render(&fold(&done[0].trail, &bus()).unwrap()).join("\n");
    compare("psp-boot", &stage_sos::linux_rules(&l), &done[..1]).unwrap_or_else(|d| panic!("{d}\n{text}"));
    assert_eq!(s.sos, Some(Sos::Loaded { sol: 0x0E5B_5A34 }));
    assert_eq!((m.boot.taken, s.boot_sent()), (9, 9));
    // The stage's writes: nine addresses, nine commands, nine flushes; nothing else.
    let p = m.regs.psp;
    let cmds: Vec<u32> = m.writes.iter().filter(|(b, _)| *b == p.c2pmsg_35).map(|(_, v)| *v).collect();
    assert_eq!(cmds, [0x0008_0000, 0x1000_0000, 0x0001_0000, 0x000B_0000, 0x000D_0000, 0x000C_0000, 0x000E_0000, 0x000F_0000, 0x0002_0000]);
    assert_eq!(done[0].totals.writes, 27, "{:?}", done[0].lines);
    // Its answer names each wait with what it took (the round holds these against the trace's reads and times).
    let waits: Vec<&str> = done[0].lines.iter().filter(|l| l.starts_with("WAIT ")).map(|l| l.as_str()).collect();
    assert_eq!(waits.len(), 18, "{:?}", done[0].lines);
    assert_eq!(waits[0], "WAIT 0x5818c # MPASP_SMN_C2PMSG_35 until (value & 0x80000000) == 0x80000000: 1 reads in 0 us, last 0x80000000");
    assert!(waits[17].starts_with("WAIT 0x58244 # MPASP_SMN_C2PMSG_81 until the value is no longer 0x00000000: ") && waits[17].ends_with("last 0x0e5b5a34"), "{}", waits[17]);
}

#[test]
fn the_ring_stages_equal_linuxs_ring_create_and_its_31_commands() {
    let (s, done, m) = chain();
    let l = s.layout.clone().unwrap();
    let text: Vec<std::string::String> = done[1..].iter().flat_map(|d| render(&fold(&d.trail, &bus()).unwrap())).collect();
    compare("psp-ring", &stage_ring::linux_rules(&l), &done[1..]).unwrap_or_else(|d| panic!("{d}\n{}", text.join("\n")));
    // The 31 commands Linux sends this card during its init, by hand from psp_load_fw (amdgpu_psp.c:3595-3660).
    let mut want = std::vec![0x50, 0x51, cmd::LOAD_TOC];
    want.extend([cmd::LOAD_IP_FW; 20]);
    want.push(cmd::AUTOLOAD_RLC);
    want.extend([cmd::LOAD_IP_FW, cmd::LOAD_IP_FW, 0x04, cmd::LOAD_IP_FW, cmd::LOAD_TA, cmd::LOAD_TA, cmd::LOAD_TA]);
    assert_eq!(want.len(), 31);
    assert_eq!(m.commands, want);
    assert_eq!(m.next(), None, "the model's secure OS got all of Linux's sequence");
    // The write pointer went 0x10 to 0x1f0, as the trace's.
    let p = m.regs.psp;
    let ptrs: Vec<u32> = m.writes.iter().filter(|(b, _)| *b == p.c2pmsg_67).map(|(_, v)| *v).collect();
    assert_eq!(ptrs, (1..=31).map(|i| i * 0x10).collect::<Vec<u32>>());
    // tmr sends nothing, on the bus too.
    assert!(done[3].stage == Stage::Tmr && done[3].trail.is_empty() && done[3].totals.writes == 0);
}

/// The sections are cut at Linux's blocks: the ring's stages do not fit psp-boot, a stage short of the section's end
/// is said to be short, and each stage alone is a part of it.
#[test]
fn a_part_of_the_chain_is_not_the_section() {
    let (s, done, _) = chain();
    let l = s.layout.clone().unwrap();
    let d = compare("psp-ring", &stage_ring::linux_rules(&l), &done[1..7]).unwrap_err();
    assert_eq!((d.kind, d.at.map(|a| a.1)), (Kind::Missing, Some(867)), "{d}");
    assert!(d.to_string().contains("Linux goes on and reads MPASP_SMN_C2PMSG_67"), "{d}");
    let d = compare("psp-boot", &stage_sos::linux_rules(&l), &done[..2]).unwrap_err();
    assert_eq!(d.kind, Kind::Extra, "{d}");
    // The ring stage alone: lines 752 to 765, then Linux's next is the TOC's frame.
    let sec = section("psp-ring").unwrap();
    let rules = stage_ring::linux_rules(&l);
    let mut c = Compare::new(&sec, &rules).unwrap();
    c.feed_trail(&done[1].trail, 0, &bus()).unwrap();
    assert_eq!(c.stop().unwrap(), Some(771));
}

/// Without its declared rules the bootloader stage differs exactly where the rules say it does.
#[test]
fn the_declared_differences_are_the_only_ones() {
    let (s, done, _) = chain();
    let l = s.layout.clone().unwrap();
    let rules = stage_sos::linux_rules(&l);
    // No rule at all: the flush before the address is an access Linux does not make.
    let d = compare("psp-boot", &[], &done[..1]).unwrap_err();
    assert_eq!((d.kind, d.at.map(|a| a.1)), (Kind::Extra, Some(675)), "{d}");
    // The flush declared, the address not: BitsOS's megabyte is not Linux's 0x3.
    let d = compare("psp-boot", &rules[..1], &done[..1]).unwrap_err();
    assert_eq!((d.kind, d.at.map(|a| a.1)), (Kind::Value, Some(675)), "{d}");
    assert!(d.to_string().contains("Linux writes 0x00000003 to MPASP_SMN_C2PMSG_36"), "{d}");
    // The address declared, the flush not.
    let d = compare("psp-boot", &rules[1..], &done[..1]).unwrap_err();
    assert_eq!(d.kind, Kind::Extra, "{d}");
    // The ring without its address rule.
    let d = compare("psp-ring", &[], &done[1..]).unwrap_err();
    assert_eq!((d.kind, d.at.map(|a| a.1)), (Kind::Value, Some(753)), "{d}");
}

/// What the comparison would say to a stage that is not Linux's: each of these is one access changed in the
/// bootloader stage's folded trail.
#[test]
fn a_changed_access_is_named() {
    let (s, done, _) = chain();
    let l = s.layout.clone().unwrap();
    let rules = stage_sos::linux_rules(&l);
    let sec = section("psp-boot").unwrap();
    let did = fold(&done[0].trail, &bus()).unwrap();
    let run = |d: &[Did]| {
        let mut c = Compare::new(&sec, &rules).unwrap();
        c.feed(d).and_then(|_| c.finish())
    };
    run(&did).unwrap();
    let pos = |f: &dyn Fn(&Did) -> bool, nth: usize| did.iter().enumerate().filter(|(_, d)| f(d)).nth(nth).unwrap().0;
    let cmd_at = |n| pos(&|d| matches!(d, Did::Write { dword: 0x16063, .. }), n);
    // Two commands the other way round.
    let mut v = did.clone();
    let (a, b) = (cmd_at(0), cmd_at(1));
    v.swap(a, b);
    assert_eq!(run(&v).unwrap_err().kind, Kind::Value);
    // The secure OS's sleep left out, and made 10 ms.
    let sleep = pos(&|d| matches!(d, Did::Sleep { .. }), 0);
    let mut v = did.clone();
    v.remove(sleep);
    assert_eq!(run(&v).unwrap_err().kind, Kind::Missing);
    let mut v = did.clone();
    v[sleep] = Did::Sleep { us: 10_000 };
    assert_eq!(run(&v).unwrap_err().kind, Kind::Value);
    // The sign-of-life read before a part left out.
    let mut v = did.clone();
    v.remove(pos(&|d| matches!(d, Did::Read { dword: 0x16091, .. }), 3));
    assert_eq!(run(&v).unwrap_err().kind, Kind::Missing);
    // A wait bounded by the clock, or by fewer reads than Linux's ten rounds.
    let wait = pos(&|d| matches!(d, Did::Wait { dword: 0x16063, .. }), 2);
    for bound in [Bound::Time { us: 1_000_000, step_us: 10 }, Bound::Reads(100_000)] {
        let mut v = did.clone();
        if let Did::Wait { bound: b, .. } = &mut v[wait] {
            *b = bound;
        }
        assert_eq!(run(&v).unwrap_err().kind, Kind::Bound);
    }
    // The wait after the secure OS command for the bootloader's ready bit instead of the sign of life.
    let last = pos(&|d| matches!(d, Did::Wait { dword: 0x16091, .. }), 0);
    let mut v = did.clone();
    v[last] = Did::Wait { dword: 0x16063, cond: Cond::Masked { mask: 0x8000_0000, value: 0x8000_0000 }, bound: Bound::Reads(1_000_000), reads: 1, last: Some(0x8000_0000) };
    assert_eq!(run(&v).unwrap_err().kind, Kind::Other);
    // A tenth command.
    let mut v = did.clone();
    v.push(Did::Write { dword: 0x16063, value: 0x2000_0000, reached: true });
    assert_eq!(run(&v).unwrap_err().kind, Kind::Extra);
}

/// A card whose secure OS runs already: Linux's nine load functions each read the sign of life and return
/// (psp_v14_0.c:136-137, :210-211), and so does the stage: nine reads, no write, no wait.
#[test]
fn a_warm_card_gets_nine_reads_and_no_write() {
    let rig = Rig::new();
    let mut s = Session::default();
    let mut stick = Stick::full();
    let (done, m) = rig.cold_pass(&mut s, warm(), &mut stick, &[Stage::Sos]);
    let did = fold(&done[0].trail, &bus()).unwrap();
    assert_eq!(did, std::vec![Did::Read { dword: 0x16091, value: Some(0x0E5B_5A34) }; 9]);
    assert!(m.writes.is_empty() && done[0].totals.writes == 0);
    assert_eq!((s.sos, s.boot_sent()), (Some(Sos::Warm { sol: 0x0E5B_5A34 }), 0));
}
