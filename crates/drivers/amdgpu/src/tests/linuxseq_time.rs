//! GX-10: what a register trace shows only as time, held to Linux's source all the same: the point where a wait
//! gives up (a count of reads, amdgpu_psp.c:621-630) and the handshake sleeps (psp_v14_0.c:229-230, :328-329). A
//! stage that waits for the right register and condition but would give up earlier than Linux, or that leaves a
//! sleep out or makes it at another place, differs.

use super::linuxseq::{as_linux, at, bus, ring, whole};
use crate::gpucore::psp::{Bound, Cond, LINUX_POLLS};
use crate::gpucore::trail::Access;
use crate::rdna4::linuxseq::compare::Kind;
use crate::rdna4::linuxseq::{What, section};
use std::string::ToString;
use std::vec::Vec;

#[test]
fn the_fixtures_carry_linuxs_bounds_and_sleeps() {
    let bounds = |n: &str| -> Vec<(&'static str, u32)> {
        let mut out: Vec<(&str, u32)> = section(n).unwrap().lines.iter().filter_map(|l| if let What::Wait { name, max, .. } = l.what { Some((name, max)) } else { None }).collect();
        out.dedup();
        out
    };
    // The bootloader's wait is ten runs of psp_wait_for (psp_v14_0.c:113-121), every other wait one.
    assert_eq!(bounds("psp-boot"), [("MPASP_SMN_C2PMSG_35", 10 * LINUX_POLLS), ("MPASP_SMN_C2PMSG_81", LINUX_POLLS)]);
    assert_eq!(bounds("psp-ring"), [("MPASP_SMN_C2PMSG_64", LINUX_POLLS)]);
    assert_eq!(bounds("mmhub"), [("MMVM_INVALIDATE_ENG17_SEM", LINUX_POLLS), ("MMVM_INVALIDATE_ENG17_ACK", LINUX_POLLS)]);
    // One sleep each: after the secure OS command and after ring create, 20 ms, right behind the write.
    for (n, reg, value) in [("psp-boot", "MPASP_SMN_C2PMSG_35", 0x2_0000), ("psp-ring", "MPASP_SMN_C2PMSG_64", 0x2_0000)] {
        let s = section(n).unwrap();
        let sleeps: Vec<usize> = s.lines.iter().enumerate().filter(|(_, l)| matches!(l.what, What::Sleep { .. })).map(|(i, _)| i).collect();
        assert_eq!(sleeps.len(), 1, "{n}");
        assert!(matches!(s.lines[sleeps[0]].what, What::Sleep { us: 20_000, gap: 20_000..21_000 }), "{n}: {:?}", s.lines[sleeps[0]]);
        assert!(matches!(s.lines[sleeps[0] - 1].what, What::Write { name, value: v, .. } if name == reg && v == value), "{n}");
        assert!(s.head.starts_with("# linuxseq "));
    }
    for n in ["nbif", "mmhub", "ih", "sw-init"] {
        assert!(section(n).unwrap().lines.iter().all(|l| !matches!(l.what, What::Sleep { .. })), "{n}");
    }
}

#[test]
fn a_wait_that_gives_up_at_another_point_than_linuxs() {
    let (s, t) = ring();
    let ready = Cond::Masked { mask: 0x8000_ffff, value: 0x8000_0000 };
    // 100 ms by the clock (the 780M driver's form): on this card Linux's 100000 reads take about 167 ms.
    let mut timed = t.clone();
    timed[0] = Access::Wait { byte: 0x16080 * 4, cond: ready, bound: Bound::Time { us: 100_000, step_us: 10 }, reads: 1, last: Some(0x80c0_0000), us: 0 };
    let d = whole(&s, &[], &timed).unwrap_err();
    assert_eq!((d.kind, d.at), (Kind::Bound, Some((4, 752))), "{d}");
    assert_eq!(
        d.to_string(),
        "psp-ring.txt line 4 (init.txt line 752): Linux waits on MPASP_SMN_C2PMSG_64 until (value & 0x8000ffff) == 0x80000000 (the card answered 0x80c00000 at read 1) \
         and gives up after 100000 reads 1 us apart; the stage gives up after 100000 us, reading every 10 us"
    );
    // Fewer reads, or more.
    for reads in [LINUX_POLLS - 1, 10 * LINUX_POLLS] {
        let mut other = t.clone();
        other[0] = Access::Wait { byte: 0x16080 * 4, cond: ready, bound: Bound::Reads(reads), reads: 1, last: Some(0x80c0_0000), us: 0 };
        assert_eq!(whole(&s, &[], &other).unwrap_err().kind, Kind::Bound);
    }
    // The bootloader's wait is the ten-fold one: a stage that waits one run of 100000 reads gives up ten times early.
    let boot = section("psp-boot").unwrap();
    let mut b = as_linux(&boot, &bus());
    let i = b.iter().position(|a| matches!(a, Access::Wait { .. })).unwrap();
    assert!(matches!(b[i], Access::Wait { byte, bound: Bound::Reads(1_000_000), .. } if byte == 0x16063 * 4), "{:?}", b[i]);
    b[i] = Access::Wait { byte: 0x16063 * 4, cond: Cond::Masked { mask: 0x8000_0000, value: 0x8000_0000 }, bound: Bound::Reads(LINUX_POLLS), reads: 1, last: Some(0x8000_0000), us: 0 };
    let d = whole(&boot, &[], &b).unwrap_err();
    assert_eq!((d.kind, d.at.map(|a| a.1)), (Kind::Bound, Some(674)), "{d}");
    assert!(d.text.ends_with("and gives up after 1000000 reads 1 us apart; the stage gives up after 100000 reads 1 us apart"), "{d}");
    // Another condition is named before another bound.
    b[i] = Access::Wait { byte: 0x16063 * 4, cond: Cond::Masked { mask: 0x8000_ffff, value: 0x8000_0000 }, bound: Bound::Reads(1), reads: 1, last: Some(0x8000_0000), us: 0 };
    assert_eq!(whole(&boot, &[], &b).unwrap_err().kind, Kind::Condition);
}

#[test]
fn a_sleep_left_out_moved_shortened_or_added() {
    let (s, t) = ring();
    let i = at(&s, 757) - 1;
    assert_eq!(t[i], Access::Sleep { us: 20_000 });
    // Left out: the wait comes right after the command.
    let mut none = t.clone();
    none.remove(i);
    let d = whole(&s, &[], &none).unwrap_err();
    assert_eq!((d.kind, d.at), (Kind::Missing, Some((9, 756))), "{d}");
    assert_eq!(
        d.to_string(),
        "psp-ring.txt line 9 (init.txt line 756): Linux sleeps 20000 us (the trace is silent for 20011 us); the stage leaves that out and waits on MPASP_SMN_C2PMSG_64 until \
         (value & 0x8000ffff) == 0x80000000 (its card answered 0x80c20000 at read 1)"
    );
    // After the wait instead of before it: the same accesses, another order.
    let mut late = t.clone();
    late.swap(i, i + 1);
    let d = whole(&s, &[], &late).unwrap_err();
    assert_eq!((d.kind, d.at), (Kind::Order, Some((9, 756))), "{d}");
    // Before the command instead of after it.
    let mut early = t.clone();
    early.swap(i - 1, i);
    assert_eq!(whole(&s, &[], &early).unwrap_err().at, Some((8, 756)));
    // Shorter, or longer.
    for us in [5000, 19_999, 20_001] {
        let mut other = t.clone();
        other[i] = Access::Sleep { us };
        let d = whole(&s, &[], &other).unwrap_err();
        assert_eq!((d.kind, d.at), (Kind::Value, Some((9, 756))), "{d}");
        assert!(d.text.starts_with("Linux sleeps 20000 us") && d.text.ends_with(&std::format!("the stage sleeps {us} us")), "{d}");
    }
    // A sleep where Linux makes none.
    let mut more = t.clone();
    more.insert(at(&s, 755), Access::Sleep { us: 1000 });
    let d = whole(&s, &[], &more).unwrap_err();
    assert_eq!((d.kind, d.at), (Kind::Extra, Some((7, 755))), "{d}");
    assert!(d.text.contains("the stage first sleeps 1000 us, which Linux does not do here"), "{d}");
    // The secure OS: the sleep comes before the read that the wait compares with, not after it.
    let boot = section("psp-boot").unwrap();
    let mut b = as_linux(&boot, &bus());
    let n = b.len();
    b.swap(n - 3, n - 2);
    let d = whole(&boot, &[], &b).unwrap_err();
    assert_eq!((d.kind, d.at.map(|a| a.1)), (Kind::Order, Some(748)), "{d}");
}
