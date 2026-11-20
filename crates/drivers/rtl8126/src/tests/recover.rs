//! Recovery and the edges of a card's life: the MAC-only restart Linux makes after a transmit timeout, a start that
//! meets a Quiesce, a PHY window slower than Linux's numbers, a card that fails before it exists.

use super::device::{card, card_on, clean, firmware, frame, pass, running};
use crate::device::*;
use crate::fakecard::{Fake, Knobs, Op, AN_DELAY_NS};
use crate::machine::Machine;
use crate::mmio_hw::{byte_of, usable_dword, with_byte, MapFail};
use crate::regs::*;
use crate::stage::{numbered, Early, Stage};
use std::string::String;
use std::vec;
use std::vec::Vec;

fn is_phy_write(op: &Op) -> bool {
    matches!(op, Op::W32(GPHY_OCP, v) if v & 0x8000_0000 != 0)
}

fn count(ops: &[Op], op: Op) -> usize {
    ops.iter().filter(|o| **o == op).count()
}

/// A running card whose one frame the chip keeps: the next health look after 5 s is a transmit timeout.
fn stuck_frame(c: &Card<Fake>, seq: u32) -> usize {
    c.hw().knobs(|k| k.tx_stuck = true);
    assert_eq!(c.transmit(&frame(seq, 60)), Ok(()));
    c.hw().ops().len()
}

#[test]
fn the_first_transmit_timeout_restarts_the_mac_alone_as_linux_does() {
    let c = running();
    let f = c.hw();
    let mark = stuck_frame(&c, 1);
    pass(&c, TX_TIMEOUT_NS + 1_200_000_000);
    assert_eq!(c.state(), State::Running);
    assert!(c.link().up, "the link never went away: no new negotiation");
    let ops = f.ops()[mark..].to_vec();
    assert!(ops.contains(&Op::AspmOff), "M:4899-4904");
    assert!(!ops.contains(&Op::PciOff) && !ops.iter().any(|o| matches!(o, Op::Enable(_))), "bus master stays as it is");
    assert!(!ops.iter().any(is_phy_write), "M:4306-4319 writes nothing to the PHY");
    assert_eq!((count(&ops, Op::Reset), count(&ops, Op::Started)), (1, 1), "rtl8169_cleanup's reset, then rtl_hw_start");
    assert!(f.started() && f.bus_master());
    assert_eq!(c.tx_info().0, 0, "the ring is empty after the restart (rtl8169_tx_clear)");
    // Frames move at once, both ways.
    f.knobs(|k| k.tx_stuck = false);
    assert_eq!(c.transmit(&frame(2, 60)), Ok(()));
    assert_eq!(f.sent().len(), 1);
    assert!(f.rx(&frame(3, 60)));
    assert_eq!(c.receive(&mut [0u8; 2048]), Some(60));
    clean(&c);
}

#[test]
fn a_second_transmit_timeout_in_a_row_restarts_the_whole_card() {
    let c = running();
    let f = c.hw();
    stuck_frame(&c, 1);
    pass(&c, TX_TIMEOUT_NS + 1_200_000_000);
    assert!(c.link().up);
    let mark = stuck_frame(&c, 2);
    pass(&c, TX_TIMEOUT_NS + 1_200_000_000);
    assert_eq!(c.state(), State::Running);
    let ops = f.ops()[mark..].to_vec();
    assert!(ops.contains(&Op::PciOff) && ops.iter().any(is_phy_write), "probe and PHY again");
    assert!(!c.link().up, "which costs a negotiation");
    pass(&c, AN_DELAY_NS + 200_000_000);
    assert!(c.link().up);
    clean(&c);
}

#[test]
fn a_mac_restart_that_does_not_come_through_is_followed_by_the_whole_one() {
    let c = running();
    let f = c.hw();
    let mark = stuck_frame(&c, 1);
    // The chip's reset never ends: the one wait that is an error in the start sequence too.
    f.knobs(|k| k.stuck_reset = true);
    pass(&c, TX_TIMEOUT_NS + 1_200_000_000);
    assert_eq!(c.state(), State::Failed, "the whole bring-up was tried next and met the same reset");
    assert!(!f.bus_master() && !f.started() && f.reg32(INTR_MASK) == 0, "nothing is left on meanwhile");
    assert!(f.ops()[mark..].contains(&Op::PciOff));
    f.knobs(|k| {
        k.stuck_reset = false;
        k.tx_stuck = false;
    });
    f.poke8(CHIP_CMD, 0);
    pass(&c, RETRY_SOON_NS + 1_000_000_000);
    assert_eq!(c.state(), State::Running, "the usual retry brings it back");
    // No `clean` here: a reset bit that reads as set makes every later ChipCmd write look like a reset to the fake.
    assert!(f.started() && f.bus_master());
}

#[test]
fn a_start_does_not_undo_a_quiesce_that_came_first() {
    // Probe registers the card for Quiesce; its first start runs later, on the worker.
    let c = card();
    c.quiesce();
    for sleepy in [true, false] {
        assert_eq!(c.start(sleepy), Err(BringUpError::Stopped(State::Quiesced)));
    }
    assert_eq!(c.state(), State::Quiesced);
    let ops = c.hw().ops();
    assert!(!ops.iter().any(|o| matches!(o, Op::Enable(_))), "no decoding, no bus master: {ops:?}");
    assert!(!ops.iter().any(|o| matches!(o, Op::W8(..) | Op::W16(..) | Op::W32(..))), "no register written: {ops:?}");
    assert!(!c.hw().bus_master());
    // Nor does the poller bring it back.
    pass(&c, 120_000_000_000);
    assert_eq!(c.state(), State::Quiesced);
}

#[test]
fn a_start_on_a_card_that_already_runs_changes_nothing() {
    let c = running();
    let started = count(&c.hw().ops(), Op::Started);
    assert_eq!(c.start(false), Err(BringUpError::Stopped(State::Running)));
    assert_eq!(c.state(), State::Running);
    assert!(c.link().up);
    assert_eq!(count(&c.hw().ops(), Op::Started), started);
    clean(&c);
}

#[test]
fn a_phy_window_slower_than_linuxs_numbers_still_plays_the_script_and_keeps_its_link() {
    // 30 looks of 25 us per access: more than Linux's ten exact steps, well inside its sleeping ones.
    let c = card_on(Fake::with(Knobs { slow_phy: 30, firmware: Some(firmware()), ..Knobs::default() }), Config::default());
    c.start(false).unwrap();
    assert_eq!(c.state(), State::Running);
    assert_eq!(c.hw().phy(0xa430), 0x0123, "the script's write arrived (0x0127), then the ALDPS setting cleared bit 2");
    let (max_us, over) = c.phy_waits();
    assert_eq!(max_us, 750, "the longest wait is kept for the log");
    assert!(over > 0);
    // The poller's shorter wait covers it too: the link is seen and stays.
    pass(&c, AN_DELAY_NS + 200_000_000);
    assert!(c.link().up);
    pass(&c, 5_000_000_000);
    assert_eq!(c.state(), State::Running);
    assert_eq!(count(&c.hw().ops(), Op::Started), 1, "no restart");
    clean(&c);
}

#[test]
fn a_card_without_ring_memory_leaves_a_stage_one_line_for_the_report_page() {
    const LINE: &str = "Netzwerk 1/8: Die Karte lässt sich nicht vorbereiten: kein passender Speicher für die Datenpakete (Steckplatz 83:00.7)";
    for _ in 0..2 {
        let fake = Fake::with(Knobs { no_dma: true, ..Knobs::default() });
        let r = crate::new_card("rtl-nomem".into(), "83:00.7".into(), fake, Config::default());
        assert!(r.err().is_some_and(|e| e.contains("RX buffers")));
    }
    let lines: Vec<String> = crate::report_lines().into_iter().filter(|l| l.contains("83:00.7")).collect();
    assert_eq!(lines, vec![String::from(LINE)], "one line per location, however often it failed");
    // A card that gets its memory says nothing here.
    assert!(crate::new_card("rtl-mem".into(), "83:00.6".into(), Fake::new(), Config::default()).is_ok());
    assert!(!crate::report_lines().iter().any(|l| l.contains("83:00.6")));
}

#[test]
fn early_lines_are_one_per_location_and_bounded() {
    let early = Early::new();
    assert!(early.lines().is_empty());
    early.set("83:00.0", numbered(Stage::Probe, "a"));
    early.set("84:00.0", numbered(Stage::Probe, "b"));
    early.set("83:00.0", numbered(Stage::Probe, "c"));
    assert_eq!(early.lines(), ["Netzwerk 1/8: c", "Netzwerk 1/8: b"]);
    for i in 0..20 {
        early.set(&std::format!("90:{i:02x}.0"), numbered(Stage::Probe, "x"));
    }
    assert_eq!(early.lines().len(), 8);
    assert_eq!(early.lines()[0], "Netzwerk 1/8: c");
}

#[test]
fn every_way_the_register_window_can_be_missing_has_its_own_plain_reason() {
    let all = [MapFail::NoBar, MapFail::Small(0x1000), MapFail::Refused("map BAR2 0x1000+0x10000: NoMemory".into())];
    let reasons: Vec<&str> = all.iter().map(MapFail::reason).collect();
    assert_eq!(reasons, ["ihr Adressbereich wurde nicht gefunden", "ihr Adressbereich ist zu klein", "ihr Adressbereich lässt sich nicht einblenden"]);
    assert_eq!(all[1].text(), "BAR2 is 0x1000 bytes, want at least 0x604a");
    assert_eq!(
        crate::stage::text::probe_early(reasons[0], "83:00.0"),
        "Die Karte lässt sich nicht vorbereiten: ihr Adressbereich wurde nicht gefunden (Steckplatz 83:00.0)"
    );
}

#[test]
fn a_config_dword_that_reads_all_ones_is_neither_believed_nor_written_back() {
    // What a function that does not answer reads as: no byte of it, and no dword to put a byte into.
    assert_eq!(usable_dword(Some(u32::MAX)), None);
    assert_eq!(usable_dword(None), None);
    assert_eq!(usable_dword(Some(0xffff_fffe)), Some(0xffff_fffe));
    // The two bytes Linux writes, each inside its dword with the three neighbours as they were.
    assert_eq!(with_byte(0x1122_3344, 0x070f, 0x27), 0x2722_3344);
    assert_eq!(with_byte(0x1122_33ff, 0x0890, 0xfe), 0x1122_33fe);
    assert_eq!((byte_of(0x1122_3344, 0x070f), byte_of(0x1122_3344, 0x0890), byte_of(0x1122_3344, 0x0891)), (0x11, 0x44, 0x33));
}
