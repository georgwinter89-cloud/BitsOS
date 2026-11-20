//! GX-10: the comparison of a stage with Linux's trace (`rdna4::linuxseq`). Linux's own sequence, played back as a
//! trail, equals each of the kernel's sections with no rule at all; from there each test changes one thing and the
//! comparer must name exactly that: a swapped pair, another value, a missing or an extra access, an address no rule
//! declares, a rule whose check fails, a rule nothing uses, a wait for another condition or on another register, a
//! flush through another register, a refused write, an answer the real card did not give, a trail that lost
//! accesses. Bounds and sleeps are in `linuxseq_time`, the rules' own refusals and the bind flush in
//! `linuxseq_rules`, the fixture's own form and what the kernel's sections hold in `linuxseq_parse`.

use super::rdna4fix::Rig;
use crate::gpucore::psp::{Bound, Cond, LINUX_POLLS};
use crate::gpucore::trail::Access;
use crate::rdna4::linuxseq::compare::{Compare, Diff, Kind};
use crate::rdna4::linuxseq::fold::{Bus, Did, fold};
use crate::rdna4::linuxseq::rules::{Address, Rule};
use crate::rdna4::linuxseq::{SECTIONS, Section, What, section};
use crate::rdna4::regs14::Regs14;
use std::string::ToString;
use std::vec::Vec;

pub fn bus() -> Bus {
    let rig = Rig::new();
    Bus::of(&Regs14::from(&rig.att.discovery).expect("the card's table names the registers"))
}

/// Linux's sequence as the bring-up owner's trail would hold it, done exactly as Linux does it on the bus.
pub fn as_linux(s: &Section, bus: &Bus) -> Vec<Access> {
    let mut out = Vec::new();
    for l in &s.lines {
        match l.what {
            What::Write { dword, value, .. } if dword * 4 >= bus.bar5_bytes => out.extend([
                Access::Write { byte: bus.index, value: dword * 4, reached: true },
                Access::Read { byte: bus.index, value: Some(dword * 4) },
                Access::Write { byte: bus.data, value, reached: true },
                Access::Read { byte: bus.data, value: Some(value) },
            ]),
            What::Read { dword, value, .. } if dword * 4 >= bus.bar5_bytes => out.extend([
                Access::Write { byte: bus.index, value: dword * 4, reached: true },
                Access::Read { byte: bus.index, value: Some(dword * 4) },
                Access::Read { byte: bus.data, value: Some(value) },
            ]),
            What::Write { dword, value, .. } => out.push(Access::Write { byte: dword * 4, value, reached: true }),
            What::Read { dword, value, times, .. } => out.extend((0..times).map(|_| Access::Read { byte: dword * 4, value: Some(value) })),
            What::Wait { dword, cond, max, last, reads, us, .. } => out.push(Access::Wait { byte: dword * 4, cond, bound: Bound::Reads(max), reads, last: Some(last), us }),
            What::Sleep { us, .. } => out.push(Access::Sleep { us }),
            What::Hdp { flush, mem, value, .. } => out.extend([Access::Write { byte: flush * 4, value: 0, reached: true }, Access::Read { byte: mem * 4, value: Some(value) }]),
            What::Mem { .. } => panic!("no section of the kernel reads memory"),
        }
    }
    out
}

pub fn whole(s: &Section, rules: &[Rule], trail: &[Access]) -> Result<(), Diff> {
    let mut c = Compare::new(s, rules)?;
    c.feed_trail(trail, 0, &bus())?;
    c.finish()
}

/// The place in `trail` of the access that stands for init.txt line `init` of psp-ring (no folding there but HDP).
pub fn at(s: &Section, init: u32) -> usize {
    let b = bus();
    let before: Vec<_> = s.lines.iter().take_while(|l| l.init != init).cloned().collect();
    as_linux(&Section { name: s.name, head: s.head, lines: before }, &b).len()
}

pub fn ring() -> (Section<'static>, Vec<Access>) {
    let s = section("psp-ring").unwrap();
    let t = as_linux(&s, &bus());
    (s, t)
}

#[test]
fn linux_played_back_equals_every_section_without_a_rule() {
    let b = bus();
    for (n, _) in SECTIONS {
        let s = section(n).unwrap();
        let t = as_linux(&s, &b);
        whole(&s, &[], &t).unwrap_or_else(|d| panic!("{d}"));
        // The index pair and the flush pair fold to Linux's logical lines, one each (a line of N reads is N).
        let lines: u32 = s.lines.iter().map(|l| if let What::Read { times, .. } = l.what { times } else { 1 }).sum();
        assert_eq!(fold(&t, &b).unwrap().len(), lines as usize, "{n}");
    }
}

#[test]
fn a_swapped_pair_a_wrong_value_a_missing_and_an_extra_write() {
    let (s, t) = ring();
    let i = at(&s, 753);
    let mut swapped = t.clone();
    swapped.swap(i, i + 1);
    let d = whole(&s, &[], &swapped).unwrap_err();
    assert_eq!((d.kind, d.at), (Kind::Order, Some((5, 753))), "{d}");
    assert_eq!(d.to_string(), "psp-ring.txt line 5 (init.txt line 753): Linux writes 0xda7a2000 to MPASP_SMN_C2PMSG_69 and then writes 0x00000083 to MPASP_SMN_C2PMSG_70; the stage does the two the other way round");

    let mut wrong = t.clone();
    wrong[at(&s, 755)] = Access::Write { byte: 0x16087 * 4, value: 0x2000, reached: true };
    let d = whole(&s, &[], &wrong).unwrap_err();
    assert_eq!((d.kind, d.at), (Kind::Value, Some((7, 755))), "{d}");
    assert!(d.text.contains("Linux writes 0x00001000 to MPASP_SMN_C2PMSG_71; the stage writes 0x00002000"), "{d}");

    let mut missing = t.clone();
    missing.remove(at(&s, 755));
    let d = whole(&s, &[], &missing).unwrap_err();
    assert_eq!((d.kind, d.at), (Kind::Missing, Some((7, 755))), "{d}");
    assert!(d.text.contains("the stage leaves that out and writes 0x00020000 to MPASP_SMN_C2PMSG_64"), "{d}");

    let mut extra = t.clone();
    extra.insert(at(&s, 755), Access::Write { byte: 0x16063 * 4, value: 0x8_0000, reached: true });
    let d = whole(&s, &[], &extra).unwrap_err();
    assert_eq!((d.kind, d.at), (Kind::Extra, Some((7, 755))), "{d}");
    assert!(d.text.contains("the stage first writes 0x00080000 to MPASP_SMN_C2PMSG_35, which Linux does not do here"), "{d}");

    // The stage stops early, or goes on after Linux's last line.
    let d = whole(&s, &[], &t[..t.len() - 1]).unwrap_err();
    assert_eq!((d.kind, d.at.map(|a| a.1)), (Kind::Missing, Some(892)), "{d}");
    let mut on = t.clone();
    on.push(Access::Read { byte: 0x16083 * 4, value: Some(0x1f0) });
    let d = whole(&s, &[], &on).unwrap_err();
    assert_eq!((d.kind, d.at.map(|a| a.1)), (Kind::Extra, Some(892)), "{d}");
}

fn ring_rule(within: core::ops::Range<u64>) -> Rule {
    Rule::Address(Address { lo: "MPASP_SMN_C2PMSG_69", hi: Some("MPASP_SMN_C2PMSG_70"), shift: 0, keep: 0, align: 4096, within, bytes: 4096, why: "the ring's address is ours" })
}

fn with_ring_at(t: &[Access], s: &Section, mc: u64) -> Vec<Access> {
    let mut t = t.to_vec();
    let i = at(s, 753);
    t[i] = Access::Write { byte: 0x16085 * 4, value: mc as u32, reached: true };
    t[i + 1] = Access::Write { byte: 0x16086 * 4, value: (mc >> 32) as u32, reached: true };
    t
}

#[test]
fn an_address_differs_only_under_a_rule_that_checks_it() {
    let (s, t) = ring();
    let window = 0x80_0120_0000u64..0x80_0520_0000;
    let ours = with_ring_at(&t, &s, window.start);
    // No rule: an address of our own is just another value.
    let d = whole(&s, &[], &ours).unwrap_err();
    assert_eq!((d.kind, d.at), (Kind::Value, Some((5, 753))), "{d}");
    assert!(d.text.ends_with("no rule declares a difference for MPASP_SMN_C2PMSG_69"), "{d}");
    // The rule: equal.
    whole(&s, &[ring_rule(window.clone())], &ours).unwrap_or_else(|d| panic!("{d}"));
    // The rule's own check: unaligned, outside the window, the last page reaching past its end.
    for (mc, why) in [(window.start + 0x800, "is not aligned to 0x1000"), (0x83_da7a_2000, "is not inside"), (window.end - 0x800, "is not aligned"), (window.end, "is not inside")] {
        let d = whole(&s, &[ring_rule(window.clone())], &with_ring_at(&t, &s, mc)).unwrap_err();
        assert_eq!((d.kind, d.at), (Kind::Value, Some((6, 754))), "{d}");
        assert!(d.text.contains("the ring's address is ours") && d.text.contains(why), "{d}");
    }
    // Linux's own address is not ours either: the rule holds the stage to BitsOS's window.
    assert_eq!(whole(&s, &[ring_rule(window.clone())], &t).unwrap_err().kind, Kind::Value);
}

#[test]
fn a_rule_must_be_needed_and_must_name_what_linux_writes() {
    let (s, t) = ring();
    let window = 0x80_0120_0000u64..0x80_0520_0000;
    // Used by no compared line: the comparison stopped before the ring's address.
    let rules = [ring_rule(window.clone())];
    let mut c = Compare::new(&s, &rules).unwrap();
    c.feed_trail(&t[..1], 0, &bus()).unwrap();
    let d = c.stop().unwrap_err();
    assert_eq!(d.kind, Kind::Rule);
    assert_eq!(d.to_string(), "psp-ring: the rule for MPASP_SMN_C2PMSG_69 and MPASP_SMN_C2PMSG_70 was used by no compared line (the ring's address is ours): a rule nothing needs is an error");
    // Half an address.
    let mut c = Compare::new(&s, &rules).unwrap();
    c.feed_trail(&with_ring_at(&t, &s, window.start)[..2], 0, &bus()).unwrap();
    assert!(c.stop().unwrap_err().text.starts_with("only one half of the address"));
    // A register this section never writes, and two rules for one register.
    let other = [Rule::Address(Address { lo: "MPASP_SMN_C2PMSG_36", hi: None, shift: 20, keep: 0, align: 1 << 20, within: window.clone(), bytes: 1 << 20, why: "the firmware buffer is ours" })];
    let d = Compare::new(&s, &other).err().unwrap();
    assert!(d.kind == Kind::Rule && d.text.contains("MPASP_SMN_C2PMSG_36, which Linux never writes in this section"), "{d}");
    let d = Compare::new(&s, &[ring_rule(window.clone()), ring_rule(window.clone())]).err().unwrap();
    assert!(d.text.contains("two rules of one kind"), "{d}");
}

#[test]
fn the_firmware_buffers_address_and_its_declared_flush() {
    // psp-boot as BitsOS will do it: the buffer in VRAM (C2PMSG_36 holds its MiB number), flushed before each
    // command, where Linux's buffer is system memory behind the GART.
    let s = section("psp-boot").unwrap();
    let b = bus();
    let vram = 0x80_0000_0000u64..0x83_fb00_0000;
    let rules = [
        Rule::Address(Address { lo: "MPASP_SMN_C2PMSG_36", hi: None, shift: 20, keep: 0, align: 1 << 20, within: vram.clone(), bytes: 1 << 20, why: "the firmware buffer is ours, in VRAM" }),
        Rule::HdpBefore { reg: "MPASP_SMN_C2PMSG_36", why: "the buffer is written through BAR0" },
    ];
    let mut ours = Vec::new();
    for a in as_linux(&s, &b) {
        if let Access::Write { byte, .. } = a
            && byte == 0x16064 * 4
        {
            ours.extend([Access::Write { byte: 0x1fc00 * 4, value: 0, reached: true }, Access::Read { byte: b.memsize, value: Some(0x3fb0) }]);
            ours.push(Access::Write { byte, value: 0x80012, reached: true });
        } else {
            ours.push(a);
        }
    }
    whole(&s, &rules, &ours).unwrap_or_else(|d| panic!("{d}"));
    // Without the flush rule the flush is an access Linux does not make; with it, a stage that does not flush differs.
    let d = whole(&s, &rules[..1], &ours).unwrap_err();
    assert_eq!((d.kind, d.at.map(|a| a.1)), (Kind::Extra, Some(675)), "{d}");
    let unflushed: Vec<Access> = as_linux(&s, &b).into_iter().map(|a| match a {
        Access::Write { byte, .. } if byte == 0x16064 * 4 => Access::Write { byte, value: 0x80012, reached: true },
        a => a,
    }).collect();
    let d = whole(&s, &rules, &unflushed).unwrap_err();
    assert_eq!((d.kind, d.at.map(|a| a.1)), (Kind::Missing, Some(675)), "{d}");
    assert!(d.text.starts_with("BitsOS flushes the HDP as Linux does right before the write of MPASP_SMN_C2PMSG_36 (the buffer is written through BAR0)"), "{d}");
    // A buffer that is not on a MiB boundary cannot be said in C2PMSG_36 at all; one outside VRAM fails the check.
    let low: Vec<Access> = ours.iter().map(|a| match *a {
        Access::Write { byte, reached, .. } if byte == 0x16064 * 4 => Access::Write { byte, value: 3, reached },
        a => a,
    }).collect();
    assert!(whole(&s, &rules, &low).unwrap_err().text.contains("is not inside"));
}

#[test]
fn waits_are_held_to_linuxs_register_and_condition() {
    let (s, t) = ring();
    let ready = Cond::Masked { mask: 0x8000_ffff, value: 0x8000_0000 };
    assert_eq!(t[0], Access::Wait { byte: 0x16080 * 4, cond: ready, bound: Bound::Reads(LINUX_POLLS), reads: 1, last: Some(0x80c0_0000), us: 0 });
    // Bit 31 alone (what the 780M's mailbox code waits for): Linux also wants the status to be 0.
    let mut loose = t.clone();
    loose[0] = Access::Wait { byte: 0x16080 * 4, cond: Cond::Masked { mask: 0x8000_0000, value: 0x8000_0000 }, bound: Bound::Reads(LINUX_POLLS), reads: 1, last: Some(0x80c0_0000), us: 0 };
    let d = whole(&s, &[], &loose).unwrap_err();
    assert_eq!((d.kind, d.at), (Kind::Condition, Some((4, 752))), "{d}");
    assert!(d.text.contains("until (value & 0x8000ffff) == 0x80000000") && d.text.contains("until (value & 0x80000000) == 0x80000000") && d.text.ends_with("another condition"), "{d}");
    // Another register.
    let mut other = t.clone();
    other[0] = Access::Wait { byte: 0x16063 * 4, cond: ready, bound: Bound::Reads(LINUX_POLLS), reads: 1, last: Some(0x8000_0000), us: 0 };
    let d = whole(&s, &[], &other).unwrap_err();
    assert_eq!((d.kind, d.at), (Kind::Other, Some((4, 752))), "{d}");
    // A plain read where Linux waits, a wait where Linux reads once.
    let mut read = t.clone();
    read[0] = Access::Read { byte: 0x16080 * 4, value: Some(0x80c0_0000) };
    assert_eq!(whole(&s, &[], &read).unwrap_err().kind, Kind::Other);
    let mut waits = t.clone();
    let i = at(&s, 758);
    waits[i] = Access::Wait { byte: 0x16083 * 4, cond: Cond::Masked { mask: u32::MAX, value: 0 }, bound: Bound::Reads(LINUX_POLLS), reads: 1, last: Some(0), us: 0 };
    assert_eq!(whole(&s, &[], &waits).unwrap_err().at, Some((11, 758)));
    // How many reads and how long is the card's doing, not the stage's: not compared.
    let mut slow = t.clone();
    slow[0] = Access::Wait { byte: 0x16080 * 4, cond: ready, bound: Bound::Reads(LINUX_POLLS), reads: 700, last: Some(0x80c0_0000), us: 7000 };
    whole(&s, &[], &slow).unwrap_or_else(|d| panic!("{d}"));
    // A wait that ran out (the last answer does not meet the condition) or never read.
    let mut timed_out = t.clone();
    timed_out[0] = Access::Wait { byte: 0x16080 * 4, cond: ready, bound: Bound::Reads(LINUX_POLLS), reads: 100, last: Some(0), us: 100_000 };
    assert_eq!(whole(&s, &[], &timed_out).unwrap_err().kind, Kind::Answer);
    let mut unread = t.clone();
    unread[0] = Access::Wait { byte: 0x16080 * 4, cond: ready, bound: Bound::Reads(LINUX_POLLS), reads: 0, last: None, us: 0 };
    assert_eq!(whole(&s, &[], &unread).unwrap_err().kind, Kind::Answer);
    // The secure OS's start: the value no longer what it was (psp_v14_0.c:230-235), never a wait for a bit.
    let boot = section("psp-boot").unwrap();
    let mut b = as_linux(&boot, &bus());
    let last = b.len() - 1;
    assert_eq!(b[last], Access::Wait { byte: 0x16091 * 4, cond: Cond::Changed { from: 0 }, bound: Bound::Reads(LINUX_POLLS), reads: 5131, last: Some(0x0e5b_5a34), us: 8586 });
    assert_eq!(b[last - 2], Access::Sleep { us: 20_000 }, "the handshake sleep comes before the read that the wait compares with");
    b[last] = Access::Wait { byte: 0x16091 * 4, cond: Cond::Masked { mask: 0x8000_0000, value: 0x8000_0000 }, bound: Bound::Reads(LINUX_POLLS), reads: 1, last: Some(0x0e5b_5a34), us: 0 };
    assert_eq!(whole(&boot, &[], &b).unwrap_err().kind, Kind::Condition);
}

#[test]
fn reads_flushes_refusals_and_answers() {
    let (s, t) = ring();
    let i = at(&s, 759);
    // An extra read (the write pointer read twice).
    let mut twice = t.clone();
    twice.insert(i, Access::Read { byte: 0x16083 * 4, value: Some(0) });
    let d = whole(&s, &[], &twice).unwrap_err();
    assert_eq!((d.kind, d.at), (Kind::Extra, Some((12, 759))), "{d}");
    // The flush through the register itself instead of its remapped place (what the stages did before NBIF's remap).
    let mut direct = t.clone();
    direct[i] = Access::Write { byte: 0xe17 * 4, value: 0, reached: true };
    let d = whole(&s, &[], &direct).unwrap_err();
    assert_eq!((d.kind, d.at), (Kind::Other, Some((12, 759))), "{d}");
    assert!(d.text.contains("Linux flushes the HDP (0 to dword 0x1fc00, then a read of RCC_DEV0_EPF0_RCC_CONFIG_MEMSIZE); the stage flushes the HDP (0 to dword 0xe17"), "{d}");
    // The flush's write without its read is a write to a register Linux only flushes through.
    let mut half = t.clone();
    half.remove(i + 1);
    assert_eq!(whole(&s, &[], &half).unwrap_err().at, Some((12, 759)));
    // A write that did not reach the card.
    let mut refused = t.clone();
    refused[i + 2] = Access::Write { byte: 0x16083 * 4, value: 0x10, reached: false };
    let d = whole(&s, &[], &refused).unwrap_err();
    assert_eq!((d.kind, d.at), (Kind::Refused, Some((13, 761))), "{d}");
    // The memory size read of a flush answered differently, or not at all.
    for value in [Some(0x3fb1), None] {
        let mut size = t.clone();
        size[i + 1] = Access::Read { byte: 0xde3 * 4, value };
        let d = whole(&s, &[], &size).unwrap_err();
        assert_eq!((d.kind, d.at), (Kind::Answer, Some((12, 759))), "{d}");
        assert!(d.text.contains("Linux flushes the HDP") && d.text.ends_with("the card does not answer as it answered Linux"), "{d}");
    }
    // The fake card answers a read differently from the real card, or not at all.
    for value in [Some(0x20), None] {
        let mut answer = t.clone();
        answer[at(&s, 762)] = Access::Read { byte: 0x16083 * 4, value };
        let d = whole(&s, &[], &answer).unwrap_err();
        assert_eq!((d.kind, d.at), (Kind::Answer, Some((14, 762))), "{d}");
        assert!(d.text.ends_with("the card does not answer as it answered Linux"), "{d}");
    }
    // A stage's own answer on the card (`answers_kept`): such answers are the card's doing, kept in order, and
    // the accesses after them are compared on. What the stage does is held as before: a refused write still ends it.
    let mut answers = t.clone();
    answers[i + 1] = Access::Read { byte: 0xde3 * 4, value: Some(0x3fb1) };
    answers[at(&s, 762)] = Access::Read { byte: 0x16083 * 4, value: None };
    let mut c = Compare::new(&s, &[]).unwrap().answers_kept();
    c.feed_trail(&answers, 0, &bus()).unwrap_or_else(|d| panic!("{d}"));
    assert_eq!(c.answers().iter().map(|d| (d.kind, d.at)).collect::<Vec<_>>(), [(Kind::Answer, Some((12, 759))), (Kind::Answer, Some((14, 762)))]);
    c.finish().unwrap();
    let mut c = Compare::new(&s, &[]).unwrap().answers_kept();
    assert_eq!(c.feed_trail(&refused, 0, &bus()).unwrap_err().kind, Kind::Refused);
    assert!(c.answers().is_empty() && Compare::new(&s, &[]).unwrap().answers().is_empty());
    // A read-back after a write is an access like any other.
    let mut back = t.clone();
    back.insert(at(&s, 754), Access::Read { byte: 0x16085 * 4, value: Some(0xda7a_2000) });
    let d = whole(&s, &[], &back).unwrap_err();
    assert_eq!((d.kind, d.at), (Kind::Extra, Some((6, 754))), "{d}");
    // A trail that lost accesses, or holds an access that is no register, proves nothing.
    let mut c = Compare::new(&s, &[]).unwrap();
    let d = c.feed_trail(&t, 3, &bus()).unwrap_err();
    assert_eq!((d.kind, d.to_string().as_str()), (Kind::Lost, "psp-ring: the stage's trail lost 3 accesses: it proves no sequence"));
    assert_eq!(c.feed_trail(&[Access::Read { byte: 0x16083 * 4 + 1, value: Some(0) }], 0, &bus()).unwrap_err().kind, Kind::Lost);
}

#[test]
fn the_index_pair_folds_only_in_linuxs_form_and_only_past_bar5() {
    let b = bus();
    let s = section("nbif").unwrap();
    let t = as_linux(&s, &b);
    assert_eq!(t.len(), 3 + 4 + 2 + 3 + 4 + 2, "STRAP2 and DF go through the pair");
    assert_eq!(fold(&t, &b).unwrap()[0], Did::Read { dword: 0x404d102, value: Some(0x0313_2000) });
    // Without the read of the index after its write it is not Linux's access.
    let mut short = t.clone();
    short.remove(1);
    let d = whole(&s, &[], &short).unwrap_err();
    assert_eq!((d.kind, d.at), (Kind::Other, Some((2, 9))), "{d}");
    assert!(d.text.contains("a register Linux touches in none of these sections"), "{d}");
    // A register inside BAR5 through the pair stays a pair access: Linux reaches it directly.
    let inside = [Access::Write { byte: b.index, value: 0xde0 * 4, reached: true }, Access::Read { byte: b.index, value: Some(0xde0 * 4) }, Access::Read { byte: b.data, value: Some(0) }];
    assert_eq!(fold(&inside, &b).unwrap().len(), 3);
}

#[test]
fn a_cursor_runs_over_a_section_stage_by_stage() {
    let (s, t) = ring();
    let b = bus();
    let cut = at(&s, 771);
    let mut c = Compare::new(&s, &[]).unwrap();
    assert_eq!(c.next_line(), Some(752));
    c.feed_trail(&t[..cut], 0, &b).unwrap();
    assert_eq!(c.next_line(), Some(771));
    c.feed_trail(&t[cut..], 0, &b).unwrap();
    assert_eq!(c.next_line(), None);
    c.finish().unwrap();
    // A stop after a prefix names where Linux would go on.
    let mut c = Compare::new(&s, &[]).unwrap();
    c.feed_trail(&t[..cut], 0, &b).unwrap();
    assert_eq!(c.stop().unwrap(), Some(771));
}
