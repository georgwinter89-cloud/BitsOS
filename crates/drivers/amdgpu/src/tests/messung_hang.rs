//! The measurement run's protocol where the run cannot end well: a hang in a stage or in a look keeps every line
//! up to it and names where it hung; a protocol write that fails stops the run before the next stage.

use super::messung::{Paper, Room, fake};
use crate::rdna4::fakefiles::model;
use crate::rdna4::messung::{self, RUN};
use crate::rdna4::stage::Stage;
use std::format;
use std::vec::Vec;

#[test]
fn after_a_hang_the_protocol_holds_every_line_up_to_the_hung_stage() {
    let mut card = fake(model());
    card.hang_at = Some(Stage::Fw);
    let disk = Paper::default();
    let paper = disk.text.clone();
    let mut room = Room::default();
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut disk = disk;
        messung::go(&mut card, &mut disk, &mut room)
    }));
    assert!(r.is_err(), "the fake hang did not hang");
    assert_eq!(room.offs, 0, "a hung stage reaches no power-off: the power button does");
    let text = paper.lock().unwrap().clone();
    let last = text.lines().last().unwrap();
    assert!(last.starts_with("START fw nr=8 "), "{last}");
    for st in &RUN[..7] {
        assert!(text.contains(&format!("WEITER {} ", st.name())), "{st:?} is not whole in the protocol");
    }
}

/// A hang in the look between two stages is told apart from one in the stage before: the look's START is the
/// last line, after the stage's WEITER.
#[test]
fn after_a_hang_in_a_look_the_protocol_names_the_look_not_the_stage_before() {
    let mut card = fake(model());
    // Calls: alive, bootloader, state (the preflight), nbif, state, gmc, then the state look after gmc.
    card.hang_call = Some(7);
    let disk = Paper::default();
    let paper = disk.text.clone();
    let mut room = Room::default();
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut disk = disk;
        messung::go(&mut card, &mut disk, &mut room)
    }));
    assert!(r.is_err(), "the fake hang did not hang");
    let text = paper.lock().unwrap().clone();
    let mut tail = text.lines().rev();
    assert_eq!(tail.next(), Some("BLICK state nach=gmc START"), "{text}");
    assert!(text.contains("WEITER gmc "), "gmc is not whole in the protocol");
}

/// A protocol write that fails in the middle stops the run before the next stage: no stage runs whose record
/// cannot be kept, and the power-off still comes.
#[test]
fn a_protocol_write_that_fails_stops_the_run_before_the_next_stage() {
    // Writes: the preflight, alive's START and lines, the cold line, bootloader's and state's START and lines (8),
    // nbif's START (9), nbif's record with the look's START (10), the look's lines (11), gmc's START (12).
    for (from, last) in [(9, None), (10, Some(Stage::Nbif)), (11, Some(Stage::Nbif)), (12, Some(Stage::Nbif))] {
        let mut card = fake(model());
        let mut disk = Paper { fail_from: Some(from), ..Paper::default() };
        let mut room = Room::default();
        let out = messung::go(&mut card, &mut disk, &mut room);
        assert_eq!(out.ran.last().copied(), last, "a write failing from {from}: {:?}", out.ran);
        let ran: Vec<Stage> = card.calls.iter().map(|(s, _)| *s).filter(|s| RUN.contains(s)).collect();
        assert_eq!(ran.last().copied(), last, "from {from}: a stage touched the card after the write failed: {ran:?}");
        let (at, why) = out.stopped.clone().unwrap();
        assert!(why.contains("nicht weiterschreiben"), "from {from}: {at} {why}");
        assert_eq!(room.offs, 1, "from {from}: the power-off");
        assert!(room.screen.iter().filter(|l| l.contains("Protokoll ließ sich nicht weiterschreiben")).count() == 1, "{:?}", room.screen);
    }
    // In the preflight: no look and no stage runs after the failed write.
    let mut card = fake(model());
    let mut disk = Paper { fail_from: Some(1), ..Paper::default() };
    let mut room = Room::default();
    let out = messung::go(&mut card, &mut disk, &mut room);
    assert!(card.calls.is_empty() && out.ran.is_empty() && out.stopped.is_some(), "{:?}", card.calls);
    assert_eq!(room.offs, 1);
}
