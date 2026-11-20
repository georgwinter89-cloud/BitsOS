use crate::bootnote::*;
use crate::bootseq::{Gate, Mailbox, Part, SEQUENCE};
use std::string::ToString;

#[test]
fn a_note_comes_back_as_it_was_written() {
    let mut all = std::vec![BootNote::NOTHING, BootNote::IN_FLIGHT];
    for sent in 1..=9u8 {
        all.push(BootNote { sent, failed: None });
        for (part, _) in SEQUENCE {
            all.push(BootNote { sent, failed: Some((part, 0)) });
            all.push(BootNote { sent, failed: Some((part, 0x000B_0000)) });
        }
    }
    all.push(BootNote { sent: BootNote::UNKNOWN, failed: Some((Part::Kdb, 0x8000_0000)) });
    for n in all {
        assert_eq!(BootNote::parse(&n.bytes()), Some(n), "{n:?}");
        assert_eq!(n.spent(), n != BootNote::NOTHING, "{n:?}");
    }
}

#[test]
fn bytes_that_are_no_note_of_this_version_are_refused() {
    let good = BootNote { sent: 4, failed: Some((Part::SocDrv, 0)) }.bytes();
    assert!(BootNote::parse(&good).is_some());
    assert_eq!(BootNote::parse(&good[..LEN - 1]), None, "short");
    assert_eq!(BootNote::parse(&[good.as_slice(), &[0]].concat()), None, "long");
    let with = |at: usize, v: u8| {
        let mut b = good;
        b[at] = v;
        BootNote::parse(&b)
    };
    assert_eq!(with(0, b'X'), None, "magic");
    assert_eq!(with(7, b'2'), None, "version");
    // More than nine, a part type the bootloader has none of, the reserved bytes.
    for sent in [10, 11, 0x80, 0xFE] {
        assert_eq!(with(8, sent), None, "{sent}");
    }
    for kind in [4, 6, 12, 0xFF] {
        assert_eq!(with(9, kind), None, "{kind}");
    }
    assert_eq!(with(10, 1), None);
    assert_eq!(with(11, 1), None);
    // A failed part without a command, a C2PMSG_35 without a failed part.
    assert_eq!(with(8, 0), None);
    let mut b = BootNote { sent: 2, failed: None }.bytes();
    b[12] = 1;
    assert_eq!(BootNote::parse(&b), None);
}

/// What is handed on under the tag and cannot be read is no reason to send: it counts as a sequence in flight.
#[test]
fn unreadable_bytes_under_the_tag_count_as_spent() {
    assert_eq!(BootNote::handed(None), None);
    assert_eq!(BootNote::handed(Some(&BootNote::NOTHING.bytes())), Some(BootNote::NOTHING));
    let n = BootNote { sent: 9, failed: Some((Part::Sos, 0x0002_0000)) };
    assert_eq!(BootNote::handed(Some(&n.bytes())), Some(n));
    for junk in [&b""[..], b"GXBOOT02........", &[0u8; 48]] {
        let got = BootNote::handed(Some(junk)).unwrap();
        assert!(got == BootNote::IN_FLIGHT && got.spent());
    }
}

#[test]
fn the_note_says_in_words_what_went_out() {
    assert_eq!(BootNote::NOTHING.to_string(), "sent nothing to the bootloader");
    assert!(BootNote::IN_FLIGHT.to_string().contains("was sending"));
    assert_eq!(BootNote { sent: 4, failed: Some((Part::SocDrv, 0)) }.to_string(), "sent 4 of the nine bootloader commands, and the SOC driver was not taken (C2PMSG_35 0x00000000)");
}

/// The gate of a kernel that inherited a spent note: nothing passes, whatever was read, at any step.
#[test]
fn a_spent_gate_lets_nothing_through() {
    let window = 0x80_0120_0000..0x80_0520_0000u64;
    let mut g = Gate::spent(window);
    let why = "the kernel before this Reload sent to this bootloader: nothing more until the card was off";
    assert_eq!(g.refused(Mailbox::Address, 0x8_0013), Some(why));
    g.saw_sign_of_life(0);
    g.saw_bootloader(0x8000_0000);
    assert_eq!(g.refused(Mailbox::Address, 0x8_0013), Some(why));
    for (_, command) in SEQUENCE {
        assert_eq!(g.refused(Mailbox::Command, command), Some(why), "{command:#x}");
    }
    assert_eq!((g.sent(), g.alive()), (0, false));
}
