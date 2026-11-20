//! The checkers' findings on GP-SOUND: the master switch in the tone line, where the check may run, who may open a
//! playback interface while the tone plays, a probe's verdict replacing the last one, a descriptor read twice.

use crate::check::{self, Kind};
use crate::plays::Plays;
use crate::tests_stream::FakePipe;
use crate::tests_uac2::{Board, cfg, function, speaker_jack, tone_with};
use crate::uac2dev::{self, JackError};
use std::string::String;
use std::vec;
use std::vec::Vec;
use uac2::desc::terminal;
use usb_core::UsbError;

#[test]
fn a_muted_master_switch_is_named_in_the_line() {
    let b = Board::new();
    let j = speaker_jack(&b);
    let line = tone_with(&Kind::Uac2(j.clone()), &b, &FakePipe::new(40, 192), true, true);
    assert!(line.starts_with(&std::format!("Tontest Lautsprecher-Buchse: gefunden; {}; Takt 5 ", check::MUTED)), "{line}");
    assert!(line.contains("192000 Bytes in 1000 ms gesendet"), "the device still takes it: {line}");
    let line = tone_with(&Kind::Uac2(j), &b, &FakePipe::new(40, 192), true, false);
    assert!(!line.contains("Hauptschalter"), "{line}");
}

#[test]
fn the_check_runs_only_with_georg_there() {
    assert!(check::allowed(true).is_ok());
    let e = check::allowed(false).unwrap_err();
    assert_eq!(e, "Tontest: gesperrt; er läuft nur, wenn in CONFIG.TXT ktest_georg=on steht (Georg hört zu).");
}

#[test]
fn the_tone_waits_for_every_open_stream_and_keeps_others_off() {
    let mut p = Plays::new();
    // The mixer's stream, and one it retires that has not played out yet: both count.
    p.open(3, false).unwrap();
    p.open(3, false).unwrap();
    assert_eq!(p.claim_tone(3), Err(UsbError::Busy));
    p.close(3);
    assert_eq!(p.claim_tone(3), Err(UsbError::Busy), "the retiring stream is still open");
    p.close(3);
    p.claim_tone(3).unwrap();
    assert_eq!(p.claim_tone(3), Err(UsbError::Busy), "one tone at a time");
    p.open(3, true).unwrap();
    assert_eq!(p.open(3, false), Err(UsbError::Busy), "the mixer's open during the tone");
    p.open(1, false).unwrap();
    p.close(3);
    assert!(p.release_tone(), "an open was refused: outputs count as changed");
    p.open(3, false).unwrap();
    p.close(3);
    p.claim_tone(3).unwrap();
    assert!(!p.release_tone(), "nothing refused this time");
}

#[test]
fn a_probe_replaces_what_the_last_probe_of_that_function_said() {
    let mut list: Vec<(u64, u8, String)> = Vec::new();
    uac2dev::note(&mut list, 7, 0, vec![String::from("USB-Tongerät: Beschreibung nicht gelesen: belegt")]);
    uac2dev::note(&mut list, 8, 0, vec![String::from("other device")]);
    uac2dev::note(&mut list, 7, 0, vec![String::from("S/PDIF: x"), String::from("Kopfhörer-Buchse: y")]);
    assert_eq!(list.iter().filter(|r| r.0 == 7).map(|r| r.2.as_str()).collect::<Vec<_>>(), ["S/PDIF: x", "Kopfhörer-Buchse: y"]);
    uac2dev::note(&mut list, 7, 0, Vec::new());
    assert_eq!(list, [(8, 0, String::from("other device"))], "a probe that worked leaves no line");
}

#[test]
fn an_endpoint_described_otherwise_the_second_time_is_not_used() {
    let b = Board::new();
    let mut c = cfg();
    let ep = c.interfaces.iter_mut().filter(|i| i.number == 3 && i.alt == 1).flat_map(|i| i.endpoints.iter_mut()).find(|e| e.address == 0x05).unwrap();
    ep.attributes = 0x05;
    let e = uac2dev::bring_up_jack(&b, &c, &function(), terminal::SPEAKER, false, false).err();
    assert_eq!(e, Some(JackError::DescriptorChanged(0x05)));
    assert_eq!(crate::words::jack(&e.unwrap()), "Gerät beschreibt Endpunkt 0x05 beim zweiten Lesen anders, nicht benutzt");
    let mut c = cfg();
    let ep = c.interfaces.iter_mut().filter(|i| i.number == 3 && i.alt == 1).flat_map(|i| i.endpoints.iter_mut()).find(|e| e.address == 0x05).unwrap();
    ep.max_packet_raw = 0x00c0;
    assert_eq!(uac2dev::bring_up_jack(&b, &c, &function(), terminal::SPEAKER, false, false).err(), Some(JackError::DescriptorChanged(0x05)));
    assert!(b.errors().is_empty(), "nothing was sent that changes the device: {:?}", b.errors());
}

#[test]
fn a_full_speed_board_is_planned_in_frames() {
    let b = Board::new();
    // The fixture's 248-byte packets hold a 1 ms frame of 48 kHz stereo S16 (196 bytes) too.
    let (j, ..) = uac2dev::bring_up_jack(&b, &cfg(), &function(), terminal::SPEAKER, false, true).unwrap();
    assert_eq!(j.plan.max_packet_bytes, 196);
}
