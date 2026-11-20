//! GX-10: the rules' own refusals (`linuxseq::rules`): bits of an address register that are no address, an address
//! that cannot be one, half an address written twice, and the TLB flush after a GART bind that a stage declares
//! before a write (`Rule::BindBefore`), compared line by line with Linux's own (`linuxseq::bind_flush`).

use super::linuxseq::{as_linux, at, bus, ring, whole};
use crate::gpucore::psp::{Bound, Cond, LINUX_POLLS};
use crate::gpucore::trail::Access;
use crate::rdna4::linuxseq::compare::{Compare, Kind};
use crate::rdna4::linuxseq::rules::{Address, Rule};
use crate::rdna4::linuxseq::{self, Section, What, section};
use std::vec::Vec;

const WINDOW: core::ops::Range<u64> = 0x80_0120_0000..0x80_0520_0000;

fn rule(keep: u32, shift: u8, align: u64) -> Rule {
    Rule::Address(Address { lo: "MPASP_SMN_C2PMSG_69", hi: Some("MPASP_SMN_C2PMSG_70"), shift, keep, align, within: WINDOW, bytes: 4096, why: "the ring's address is ours" })
}

/// Linux's ring section with the ring's address registers written as `lo` and `hi`.
fn with(lo: u32, hi: u32) -> (Section<'static>, Vec<Access>) {
    let (s, mut t) = ring();
    let i = at(&s, 753);
    t[i] = Access::Write { byte: 0x16085 * 4, value: lo, reached: true };
    t[i + 1] = Access::Write { byte: 0x16086 * 4, value: hi, reached: true };
    (s, t)
}

#[test]
fn bits_that_are_no_address_must_be_linuxs() {
    // Linux wrote 0xda7a2000 into the low register: under `keep` 1 its bit 0 is 0 and must stay 0.
    let (s, ours) = with(0x0120_0000, 0x80);
    whole(&s, &[rule(1, 0, 4096)], &ours).unwrap_or_else(|d| panic!("{d}"));
    let (s, flipped) = with(0x0120_0001, 0x80);
    let d = whole(&s, &[rule(1, 0, 4096)], &flipped).unwrap_err();
    assert_eq!((d.kind, d.at), (Kind::Value, Some((6, 754))), "{d}");
    assert!(d.text.ends_with("the bits 0x1 of MPASP_SMN_C2PMSG_69 are 0x1, Linux's are 0x0"), "{d}");
    // Without `keep` the same bit is part of the address, which is then not aligned.
    assert!(whole(&s, &[rule(0, 0, 4096)], &flipped).unwrap_err().text.contains("is not aligned to 0x1000"));
}

#[test]
fn an_address_that_cannot_be_one() {
    // Shifted out of 64 bits.
    let (s, ours) = with(0x0120_0000, 0x80);
    let d = whole(&s, &[rule(0, 32, 4096)], &ours).unwrap_err();
    assert_eq!((d.kind, d.at), (Kind::Value, Some((6, 754))), "{d}");
    assert!(d.text.ends_with("0x8001200000 shifted by 32 is no 64-bit address"), "{d}");
    // A rule with no alignment says nothing about the address: refused, whatever the stage wrote, also address 0
    // (which every alignment but 0 divides).
    let d = whole(&s, &[rule(0, 0, 0)], &ours).unwrap_err();
    assert_eq!(d.kind, Kind::Value);
    assert!(d.text.ends_with("the address 0x8001200000 is not aligned to 0x0"), "{d}");
    let low = |align| Rule::Address(Address { lo: "MPASP_SMN_C2PMSG_69", hi: Some("MPASP_SMN_C2PMSG_70"), shift: 0, keep: 0, align, within: 0..0x1_0000, bytes: 4096, why: "t" });
    let (s, zero) = with(0, 0);
    whole(&s, &[low(4096)], &zero).unwrap_or_else(|d| panic!("{d}"));
    let d = whole(&s, &[low(0)], &zero).unwrap_err();
    assert!(d.kind == Kind::Value && d.text.ends_with("the address 0x0 is not aligned to 0x0"), "{d}");
}

#[test]
fn half_an_address_written_twice() {
    let (s, mut t) = with(0x0120_0000, 0x80);
    let i = at(&s, 754);
    t[i] = Access::Write { byte: 0x16085 * 4, value: 0x0120_1000, reached: true };
    // Without a rule that is one register written where Linux writes another.
    assert_eq!(whole(&s, &[], &t).unwrap_err().kind, Kind::Value);
    // Under the rule the comparer must not take the second low half for the pair's high half. Linux's line there
    // is the high register, so the place differs first; fed as Linux's two lines of the low register it is the rule.
    let twice = Section { name: s.name, head: s.head, lines: [s.lines[1], s.lines[1], s.lines[2]].to_vec() };
    let rules = [rule(0, 0, 4096)];
    let mut c = Compare::new(&twice, &rules).unwrap();
    let d = c.feed_trail(&t[at(&s, 753)..=i], 0, &bus()).unwrap_err();
    assert_eq!(d.kind, Kind::Rule, "{d}");
    assert!(d.text.starts_with("MPASP_SMN_C2PMSG_69 is written a second time before the other half of its address"), "{d}");
}

fn flush() -> Vec<Access> {
    let unit = linuxseq::bind_flush().unwrap();
    as_linux(&Section { name: "sw-init", head: "", lines: unit }, &bus())
}

#[test]
fn linuxs_flush_after_a_bind_is_the_first_of_28_alike() {
    let unit = linuxseq::bind_flush().unwrap();
    assert_eq!(unit.len(), 9);
    assert_eq!((unit[0].init, unit[8].init), (208, 218));
    // Stripped of what the card did (reads, times, answers' counts), every one of the 28 is this one.
    let plain = |w: &What<'static>| match *w {
        What::Wait { name, dword, cond, max, last, .. } => What::Wait { name, dword, cond, max, last, reads: 0, us: 0, seen: "" },
        w => w,
    };
    let sw = section("sw-init").unwrap();
    let all: Vec<What> = sw.lines.iter().map(|l| plain(&l.what)).collect();
    let one: Vec<What> = unit.iter().map(|l| plain(&l.what)).collect();
    assert_eq!(all.windows(9).filter(|w| *w == &one[..]).count(), 28);
    assert!(all[..9] == one[..], "sw-init begins with one");
}

#[test]
fn a_bind_of_our_own_is_flushed_as_linux_does_before_the_first_write() {
    // psp-boot with the firmware buffer bound into the GART by the stage itself, before the first C2PMSG_36.
    let s = section("psp-boot").unwrap();
    let b = bus();
    let linux = as_linux(&s, &b);
    let first = linux.iter().position(|a| matches!(a, Access::Write { byte, .. } if *byte == 0x16064 * 4)).unwrap();
    let mut ours = linux.clone();
    ours.splice(first..first, flush());
    let rules = [Rule::BindBefore { reg: "MPASP_SMN_C2PMSG_36", why: "the firmware buffer is bound by this stage" }];
    whole(&s, &rules, &ours).unwrap_or_else(|d| panic!("{d}"));
    // Without the rule the flush is nothing Linux does here.
    let d = whole(&s, &[], &ours).unwrap_err();
    assert_eq!((d.kind, d.at.map(|a| a.1)), (Kind::Other, Some(675)), "{d}");
    // With the rule and no flush: missing. With the rule and nothing bound later: the rule was needed once only.
    let d = whole(&s, &rules, &linux).unwrap_err();
    assert_eq!((d.kind, d.at.map(|a| a.1)), (Kind::Missing, Some(675)), "{d}");
    assert!(
        d.text.starts_with("BitsOS binds GART pages before the first write of MPASP_SMN_C2PMSG_36 and flushes the TLB as Linux does after a bind (the firmware buffer is bound by this stage; sw-init.txt line 4): Linux flushes the HDP"),
        "{d}"
    );
    // A second flush before the second command is not declared.
    let second = ours.iter().enumerate().filter(|(_, a)| matches!(a, Access::Write { byte, .. } if *byte == 0x16064 * 4)).nth(1).unwrap().0;
    let mut again = ours.clone();
    again.splice(second..second, flush());
    let d = whole(&s, &rules, &again).unwrap_err();
    assert_eq!((d.kind, d.at.map(|a| a.1)), (Kind::Other, Some(684)), "{d}");
    // Every line of the flush is held: one HDP flush instead of two, the request for another VMID, the acknowledge
    // awaited with another bound or another condition, the semaphore not released, the read-back left out.
    let unit = flush();
    assert_eq!(unit.len(), 2 + 2 + 1 + 1 + 1 + 1 + 3);
    let try_with = |change: &dyn Fn(&mut Vec<Access>)| {
        let mut f = flush();
        change(&mut f);
        let mut t = linux.clone();
        t.splice(first..first, f);
        whole(&s, &rules, &t).unwrap_err()
    };
    let d = try_with(&|f| drop(f.drain(2..4)));
    assert_eq!((d.kind, d.at.map(|a| a.1)), (Kind::Missing, Some(675)), "{d}");
    assert!(d.text.contains("sw-init.txt line 5"), "{d}");
    let d = try_with(&|f| f[5] = Access::Write { byte: 0x1a598 * 4, value: 0x00f8_0002, reached: true });
    assert_eq!(d.kind, Kind::Value, "{d}");
    assert!(d.text.contains("sw-init.txt line 7): Linux writes 0x00f80001 to MMVM_INVALIDATE_ENG17_REQ; the stage writes 0x00f80002"), "{d}");
    let ack = Cond::Masked { mask: 1, value: 1 };
    let d = try_with(&|f| f[6] = Access::Wait { byte: 0x1a5aa * 4, cond: ack, bound: Bound::Time { us: 100_000, step_us: 10 }, reads: 1, last: Some(0x0001_0001), us: 0 });
    assert_eq!(d.kind, Kind::Bound, "{d}");
    let d = try_with(&|f| f[6] = Access::Wait { byte: 0x1a5aa * 4, cond: Cond::Masked { mask: 0xffff, value: 1 }, bound: Bound::Reads(LINUX_POLLS), reads: 1, last: Some(0x0001_0001), us: 0 });
    assert_eq!(d.kind, Kind::Condition, "{d}");
    let d = try_with(&|f| {
        f.remove(7);
    });
    assert_eq!(d.kind, Kind::Missing, "{d}");
    assert!(d.text.contains("sw-init.txt line 9): Linux writes 0x00000000 to MMVM_INVALIDATE_ENG17_SEM"), "{d}");
    let d = try_with(&|f| {
        f.pop();
    });
    assert_eq!(d.kind, Kind::Missing, "{d}");
    assert!(d.text.contains("sw-init.txt line 12): Linux reads MMVM_L2_BANK_SELECT_RESERVED_CID2"), "{d}");
    // A stage that ends inside the flush has not done it, and a rule for a register this section never writes, or
    // one nothing used, is an error as for every rule.
    let mut c = Compare::new(&s, &rules).unwrap();
    c.feed_trail(&ours[..first + 4], 0, &b).unwrap();
    let d = c.stop().unwrap_err();
    assert_eq!((d.kind, d.at.map(|a| a.1)), (Kind::Missing, Some(675)), "{d}");
    let mut c = Compare::new(&s, &rules).unwrap();
    c.feed_trail(&ours[..first], 0, &b).unwrap();
    assert_eq!(c.stop().unwrap_err().kind, Kind::Rule);
    let ih = section("ih").unwrap();
    assert_eq!(Compare::new(&ih, &rules).err().unwrap().kind, Kind::Rule);
}

#[test]
fn a_bind_flush_and_an_hdp_flush_before_the_same_write() {
    // Both declared for C2PMSG_36: the bind's flush once, before the first write; the HDP flush before each, in
    // the rules' order.
    let s = section("psp-boot").unwrap();
    let b = bus();
    let hdp = [Access::Write { byte: 0x1fc00 * 4, value: 0, reached: true }, Access::Read { byte: b.memsize, value: Some(0x3fb0) }];
    let mut ours = Vec::new();
    let mut bound = false;
    for a in as_linux(&s, &b) {
        if matches!(a, Access::Write { byte, .. } if byte == 0x16064 * 4) {
            if !bound {
                ours.extend(flush());
                bound = true;
            }
            ours.extend(hdp);
        }
        ours.push(a);
    }
    let rules = [
        Rule::BindBefore { reg: "MPASP_SMN_C2PMSG_36", why: "the firmware buffer is bound by this stage" },
        Rule::HdpBefore { reg: "MPASP_SMN_C2PMSG_36", why: "the buffer is written through BAR0" },
    ];
    whole(&s, &rules, &ours).unwrap_or_else(|d| panic!("{d}"));
    // The other way round in the trail is not what the rules say.
    let swapped = [rules[1].clone(), rules[0].clone()];
    assert_eq!(whole(&s, &swapped, &ours).unwrap_err().kind, Kind::Missing);
}
