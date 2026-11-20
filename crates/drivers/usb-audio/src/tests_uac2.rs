//! Host tests of the UAC2 binding (GP-SOUND AU-6) on the board device's configuration (`uac2::fixture`, typed from
//! plan/hw/gaming-pc-265k-2026-11-05/lsusb-asrock-usb-audio-26ce-0a09.txt), against a fake device that refuses what
//! the UAC2 rules forbid: a rate written while its stream's alternate setting is selected, a rate outside the
//! clock's RANGE, and requests to units or clocks it does not have.

use crate::check::{self, Env, Kind};
use crate::dev::{self, OutputInfo};
use crate::plan::Sync;
use crate::req::{self, Ctl};
use crate::stream::{Layout, Playback};
use crate::tests_stream::{FakePipe, alloc, none};
use crate::uac;
use crate::uac2dev::{self, JackError, StartError};
use std::boxed::Box;
use std::collections::BTreeMap;
use std::string::String;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::vec;
use std::vec::Vec;
use uac2::desc::{Function, terminal};
use uac2::fixture;
use uac2::plan::PlanError;
use usb_core::{Configuration, Setup, UsbError};

#[derive(Clone, Debug, PartialEq, Eq)]
enum Req {
    Alt(u8, u8),
    Out(Setup, Vec<u8>),
    In(Setup),
}

/// The board device as far as control requests go. Clocks 3, 4, 5, 6, 8 start at 44.1 kHz; each clock serves the
/// streaming interface whose terminal names it (interface 1-5 <- clock 3, 4, 5, 6, 8).
pub(crate) struct Board {
    config: Vec<u8>,
    log: Mutex<Vec<Req>>,
    alt: Mutex<BTreeMap<u8, u8>>,
    rate: Mutex<BTreeMap<u8, u32>>,
    ranges: BTreeMap<u8, Vec<u32>>,
    valid: Mutex<BTreeMap<u8, bool>>,
    /// Order violations and unknown requests the fake saw.
    errors: Mutex<Vec<String>>,
}

const CLOCK_OF_IFACE: [(u8, u8); 5] = [(1, 3), (2, 4), (3, 5), (4, 6), (5, 8)];

impl Board {
    pub(crate) fn new() -> Board {
        let mut ranges = BTreeMap::new();
        for c in [3u8, 4] {
            ranges.insert(c, fixture::MIC_RATES.to_vec());
        }
        for c in [5u8, 6, 8] {
            ranges.insert(c, fixture::SPEAKER_RATES.to_vec());
        }
        Board {
            config: fixture::config(),
            log: Mutex::new(Vec::new()),
            alt: Mutex::new(BTreeMap::new()),
            rate: Mutex::new(ranges.keys().map(|&c| (c, 44_100)).collect()),
            ranges,
            valid: Mutex::new(BTreeMap::new()),
            errors: Mutex::new(Vec::new()),
        }
    }
    fn iface_of(clock: u8) -> u8 {
        CLOCK_OF_IFACE.iter().find(|x| x.1 == clock).map_or(0, |x| x.0)
    }
    fn log(&self) -> Vec<Req> {
        self.log.lock().unwrap().clone()
    }
    pub(crate) fn errors(&self) -> Vec<String> {
        self.errors.lock().unwrap().clone()
    }
}

impl Ctl for Board {
    fn out(&self, s: Setup, d: &[u8]) -> Result<usize, UsbError> {
        self.log.lock().unwrap().push(Req::Out(s, d.to_vec()));
        let (cs, entity, iface) = ((s.value >> 8) as u8, (s.index >> 8) as u8, s.index as u8);
        if s.request_type != 0x21 || s.request != 1 || iface != 0 {
            self.errors.lock().unwrap().push(std::format!("unknown OUT {s:?}"));
            return Err(UsbError::Stall);
        }
        if let Some(allowed) = self.ranges.get(&entity) {
            let rate = u32::from_le_bytes(d.try_into().map_err(|_| UsbError::Stall)?);
            let streaming = Board::iface_of(entity);
            if cs != 1 || s.length != 4 {
                return Err(UsbError::Stall);
            }
            if self.alt.lock().unwrap().get(&streaming).copied().unwrap_or(0) != 0 {
                self.errors.lock().unwrap().push(std::format!("rate of clock {entity} written while interface {streaming} streams"));
                return Err(UsbError::Stall);
            }
            if !allowed.contains(&rate) {
                return Err(UsbError::Stall);
            }
            self.rate.lock().unwrap().insert(entity, rate);
            return Ok(4);
        }
        // Feature units 29 (speaker), 30, 32: CUR of mute (1 byte) or volume (2 bytes).
        match (entity, cs, s.length) {
            (27..=32, 1, 1) | (27..=29, 2, 2) => Ok(d.len()),
            _ => {
                self.errors.lock().unwrap().push(std::format!("OUT to unit {entity} control {cs}"));
                Err(UsbError::Stall)
            }
        }
    }
    fn input(&self, s: Setup, b: &mut [u8]) -> Result<usize, UsbError> {
        self.log.lock().unwrap().push(Req::In(s));
        if (s.request_type, s.request, s.value) == (0x80, 6, 0x0200) {
            let n = b.len().min(self.config.len());
            b[..n].copy_from_slice(&self.config[..n]);
            return Ok(n);
        }
        let (cs, entity) = ((s.value >> 8) as u8, (s.index >> 8) as u8);
        let reply: Vec<u8> = match (s.request_type, s.request, cs) {
            (0xA1, 2, 1) if self.ranges.contains_key(&entity) => fixture::range_reply(&self.ranges[&entity]),
            (0xA1, 1, 1) if self.ranges.contains_key(&entity) => self.rate.lock().unwrap()[&entity].to_le_bytes().to_vec(),
            (0xA1, 1, 2) if self.ranges.contains_key(&entity) => vec![*self.valid.lock().unwrap().get(&entity).unwrap_or(&true) as u8],
            (0xA1, 2, 2) if (27..=29).contains(&entity) => {
                let mut r = 1u16.to_le_bytes().to_vec();
                for v in [-0x7F00i16, 0, 0x0100] {
                    r.extend_from_slice(&v.to_le_bytes());
                }
                r
            }
            _ => {
                self.errors.lock().unwrap().push(std::format!("unknown IN {s:?}"));
                return Err(UsbError::Stall);
            }
        };
        let n = b.len().min(reply.len());
        b[..n].copy_from_slice(&reply[..n]);
        Ok(n)
    }
    fn set_interface(&self, iface: u8, alt: u8) -> Result<(), UsbError> {
        self.log.lock().unwrap().push(Req::Alt(iface, alt));
        self.alt.lock().unwrap().insert(iface, alt);
        Ok(())
    }
}

pub(crate) fn function() -> Function {
    Function::parse(&fixture::config()).unwrap()
}

pub(crate) fn cfg() -> Configuration {
    Configuration::parse(&fixture::config()).unwrap()
}

fn rate_reqs(clock: u8) -> [Req; 4] {
    let get = Setup { request_type: 0xA1, request: 1, value: 0x0100, index: (clock as u16) << 8, length: 4 };
    [
        Req::In(get),
        Req::Out(Setup { request_type: 0x21, request: 1, value: 0x0100, index: (clock as u16) << 8, length: 4 }, 48_000u32.to_le_bytes().to_vec()),
        Req::In(get),
        Req::In(Setup { request_type: 0xA1, request: 1, value: 0x0200, index: (clock as u16) << 8, length: 1 }),
    ]
}

#[test]
fn usb_core_and_uac2_read_the_same_board_configuration() {
    let c = cfg();
    assert!(!c.truncated);
    assert_eq!(c.total_length, 0x0C73);
    let ac = c.interface(0, 0).unwrap();
    assert_eq!((ac.class, ac.subclass, ac.protocol), (1, 1, 0x20), "only the UAC2 match takes it");
    let sp = c.interface(3, 1).unwrap();
    assert_eq!((sp.endpoints[0].address, sp.endpoints[0].max_packet(), sp.endpoints[0].interval), (0x05, 248, 1));
    let f = function();
    assert_eq!(uac2dev::output_jacks(&f), vec![terminal::SPEAKER, terminal::HEADPHONES, terminal::SPDIF]);
}

#[test]
fn the_two_drivers_split_audio_control_by_protocol() {
    let jbl = Configuration::parse(&crate::jbl::config()).unwrap();
    let uac1: Vec<u8> = jbl.interfaces.iter().filter(|f| (f.class, f.subclass) == (1, 1)).map(|f| f.protocol).collect();
    assert_eq!(uac1, vec![0, 0], "the JBL's two functions are UAC1 and stay with the old driver");
    let protocols: Vec<Option<u8>> = crate::DRIVER.matches.iter().map(|m| match m {
        usb_core::UsbMatch::Interface { class: 1, subclass: Some(1), protocol } => *protocol,
        other => panic!("{other:?}"),
    }).collect();
    assert_eq!(protocols, vec![Some(0), Some(0x20)], "UAC1 to dev::probe, UAC2 to uac2dev::probe, nothing else");
}

#[test]
fn each_jack_starts_in_the_uac2_order() {
    let b = Board::new();
    let (f, c) = (function(), cfg());
    let mut got = Vec::new();
    for jack in uac2dev::output_jacks(&f) {
        let (j, sp, st, fd) = uac2dev::bring_up_jack(&b, &c, &f, jack, false, false).unwrap();
        got.push((jack, j.plan.iface, j.plan.alt, j.plan.clock, sp.ep.max_packet(), st, fd.mutes, fd.volumes, fd.failed));
    }
    let st = uac2dev::Started { before: Some(44_100), wrote: true, after: Some(48_000) };
    assert_eq!(got, vec![
        (terminal::SPEAKER, 3, 1, 5, 248, st, 1, 8, 0),
        (terminal::HEADPHONES, 4, 1, 6, 248, st, 1, 0, 0),
        (terminal::SPDIF, 5, 1, 8, 124, st, 1, 0, 0),
    ]);
    // The speaker's requests: its clock's RANGE (count, then the table), then format.c's and endpoint.c's order.
    let log = b.log();
    let speaker: Vec<Req> = log.iter().take_while(|r| !matches!(r, Req::In(s) if s.request == 2 && s.index >> 8 == 6)).cloned().collect();
    let head = Setup { request_type: 0xA1, request: 2, value: 0x0100, index: 5 << 8, length: 2 };
    let full = Setup { length: 2 + 7 * 12, ..head };
    let mut want = vec![Req::In(head), Req::In(full), Req::Alt(3, 0)];
    want.extend(rate_reqs(5));
    want.push(Req::Alt(3, 1));
    want.push(Req::Out(Setup { request_type: 0x21, request: 1, value: 0x0100, index: 29 << 8, length: 1 }, vec![0]));
    assert_eq!(speaker[..want.len()], want[..]);
    assert_eq!(speaker.len(), want.len() + 16, "then RANGE and CUR of the volume on channels 1-8");
    assert_eq!(speaker[want.len()], Req::In(Setup { request_type: 0xA1, request: 2, value: 0x0201, index: 29 << 8, length: 8 }));
    assert_eq!(speaker[want.len() + 1], Req::Out(Setup { request_type: 0x21, request: 1, value: 0x0201, index: 29 << 8, length: 2 }, vec![0, 0]));
    assert!(b.errors().is_empty(), "{:?}", b.errors());
}

#[test]
fn a_clock_already_at_48_khz_is_not_written() {
    let b = Board::new();
    b.rate.lock().unwrap().insert(5, 48_000);
    let (f, c) = (function(), cfg());
    let (_, _, st, _) = uac2dev::bring_up_jack(&b, &c, &f, terminal::SPEAKER, false, false).unwrap();
    assert_eq!(st, uac2dev::Started { before: Some(48_000), wrote: false, after: Some(48_000) });
    assert!(!b.log().iter().any(|r| matches!(r, Req::Out(s, _) if s.index == 5 << 8)), "clock.c:629-633");
}

#[test]
fn the_fake_refuses_a_rate_written_while_the_stream_is_selected() {
    let b = Board::new();
    b.set_interface(3, 1).unwrap();
    let (s, d) = uac2::clock::set_rate(0, 5, 48_000);
    assert_eq!(b.out(uac2dev::conv(s), &d), Err(UsbError::Stall), "UAC1's order (interface first) on a UAC2 clock");
    assert_eq!(b.errors().len(), 1);
    // The driver's order never trips it, even from a selected interface.
    let (f, c) = (function(), cfg());
    let b = Board::new();
    b.set_interface(3, 1).unwrap();
    uac2dev::bring_up_jack(&b, &c, &f, terminal::SPEAKER, false, false).unwrap();
    assert!(b.errors().is_empty(), "{:?}", b.errors());
}

#[test]
fn a_rate_outside_the_clock_range_is_refused_before_anything_changes() {
    let mut b = Board::new();
    b.ranges.insert(5, vec![44_100, 96_000]);
    let (f, c) = (function(), cfg());
    assert_eq!(uac2dev::bring_up_jack(&b, &c, &f, terminal::SPEAKER, false, false).err(), Some(JackError::Plan(PlanError::RateNotOffered(48_000))));
    assert!(b.log().iter().all(|r| matches!(r, Req::In(_))), "only the RANGE was asked: {:?}", b.log());
    // The other jacks still come up.
    assert!(uac2dev::bring_up_jack(&b, &c, &f, terminal::HEADPHONES, false, false).is_ok());
}

#[test]
fn a_clock_that_is_not_valid_leaves_the_stream_deselected() {
    let b = Board::new();
    b.valid.lock().unwrap().insert(6, false);
    let (f, c) = (function(), cfg());
    let e = uac2dev::bring_up_jack(&b, &c, &f, terminal::HEADPHONES, false, false).err();
    assert_eq!(e, Some(JackError::Start { error: StartError::Invalid { rate: Some(48_000) }, clock: 6, iface: 4, alt: 1 }));
    assert_eq!(crate::words::jack(&e.unwrap()), "Takt 6 meldet ungültig (bei 48000 Hz)");
    assert_eq!(b.alt.lock().unwrap().get(&4), Some(&0), "clock.c:665-667: no alternate setting on an invalid clock");
}

#[test]
fn a_rate_the_device_refuses_is_named() {
    let b = Board::new();
    let (f, c) = (function(), cfg());
    let j = uac2dev::bring_up_jack(&b, &c, &f, terminal::SPDIF, false, false).unwrap().0;
    let mut p = j.plan;
    p.rate = 32_000;
    assert_eq!(uac2dev::start(&b, &j.clock, &p), Err(StartError::SetRate { err: UsbError::Stall, before: Some(48_000) }));
}

#[test]
fn the_mixer_side_is_s16_stereo_adaptive_with_the_uac2_mute() {
    let b = Board::new();
    let (f, c) = (function(), cfg());
    let (_, sp, _, _) = uac2dev::bring_up_jack(&b, &c, &f, terminal::SPEAKER, false, false).unwrap();
    assert_eq!((sp.iface, sp.alt, sp.channels, sp.subframe, sp.bits, sp.sync, sp.rate_control), (3, 1, 2, 2, 16, Sync::Adaptive, false));
    assert_eq!(sp.terminal_type, terminal::SPEAKER);
    assert_eq!(sp.features.len(), 1);
    assert_eq!((sp.features[0].id, sp.features[0].controls[0]), (29, uac::FU_MUTE));
    assert!(!sp.features[0].has(uac::FU_VOLUME), "UAC1's GET_MIN/GET_MAX never go to a UAC2 device");
    // The master switch's request is UAC2's CUR of mute, byte for byte.
    let b = Board::new();
    assert_eq!(req::set_mute(&b, &sp, true), Ok(1));
    assert_eq!(b.log(), vec![Req::Out(Setup { request_type: 0x21, request: 1, value: 0x0100, index: 29 << 8, length: 1 }, vec![1])]);
    assert!(b.errors().is_empty());
}

fn info(key: &str, id: u64, iface: u8, ty: u16, board: bool) -> OutputInfo {
    OutputInfo { key: key.into(), id, control: 0, iface, channels: 2, terminal_type: ty, board }
}

#[test]
fn a_headset_comes_before_the_board_jacks() {
    // The board was found first (lower device id); the JBL still leads, then the speaker jack.
    let mut v = vec![
        info("usb:xhci1-12.1", 3, 5, terminal::SPDIF, true),
        info("usb:xhci1-12.1", 3, 3, terminal::SPEAKER, true),
        info("usb:xhci1-10.3", 7, 1, 0x0301, false),
        info("usb:xhci1-12.1", 3, 4, terminal::HEADPHONES, true),
    ];
    dev::order(&mut v);
    assert_eq!(v.iter().map(|o| (o.id, o.iface)).collect::<Vec<_>>(), vec![(7, 1), (3, 3), (3, 4), (3, 5)]);
    // UAC1 only (the Mini-PC): the old order, stereo first, then by device and interface.
    let mut mono = info("usb:a", 2, 4, 0x0402, false);
    mono.channels = 1;
    let mut w = vec![mono.clone(), info("usb:b", 9, 1, 0x0301, false), info("usb:a", 2, 1, 0x0301, false)];
    dev::order(&mut w);
    assert_eq!(w.iter().map(|o| (o.id, o.iface)).collect::<Vec<_>>(), vec![(2, 1), (9, 1), (2, 4)]);
}

#[test]
fn names_pick_outputs() {
    let mut v = vec![info("usb:xhci1-10.3", 7, 1, 0x0301, false), info("usb:xhci1-12.1", 3, 3, terminal::SPEAKER, true),
        info("usb:xhci1-12.1", 3, 4, terminal::HEADPHONES, true), info("usb:xhci1-12.1", 3, 5, terminal::SPDIF, true)];
    dev::order(&mut v);
    let at = |n: Option<&str>| check::pick(&v, n).map(|o| o.iface);
    assert_eq!((at(None), at(Some("headset")), at(Some("lautsprecher")), at(Some("kopfhoerer")), at(Some("spdif"))), (Ok(1), Ok(1), Ok(3), Ok(4), Ok(5)));
    assert_eq!((at(Some("2")), at(Some(" 4 ")), at(Some("usb:xhci1-12.1/usb/0:0x4"))), (Ok(3), Ok(5), Ok(4)));
    let e = at(Some("9")).unwrap_err();
    assert!(e.starts_with("Tontest: kein Ausgang „9“. Da sind: 1 = USB-Headset (usb:xhci1-10.3/usb/0:0x1), 2 = Lautsprecher-Buchse"), "{e}");
    assert_eq!(check::pick(&[], None).unwrap_err(), "Tontest: kein USB-Ausgang da (nichts gebunden).");
}

#[test]
fn the_tone_is_quiet_and_fades() {
    let peak = (0..check::TONE_FRAMES).map(|i| check::tone_frame(i)[0].unsigned_abs()).max().unwrap();
    assert_eq!(peak, 1000, "about -30 dB below full scale");
    assert_eq!(check::tone_frame(0), [0, 0]);
    assert_eq!(check::tone_frame(check::TONE_FRAMES - 1), [0, 0]);
    assert_eq!(check::tone_frame(528), [1000, 1000]);
    assert_eq!(check::tone_frame(560), [-1000, -1000]);
}

/// The check's clock. The fake pipe's packets are 1 ms of 48 frames, two to a request: a sleep of `ms` completes
/// `ms / 2` requests when `takes` says so, so the device takes the tone at 48 kHz.
struct Day {
    now: AtomicU64,
}

fn day() -> Arc<Day> {
    Arc::new(Day { now: AtomicU64::new(0) })
}

fn tone_on(kind: &Kind, b: &Board, pipe: &FakePipe, takes: bool) -> String {
    tone_with(kind, b, pipe, takes, false)
}

pub(crate) fn tone_with(kind: &Kind, b: &Board, pipe: &FakePipe, takes: bool, muted: bool) -> String {
    let d = day();
    let p2 = pipe.clone();
    let open = move || Playback::open(Box::new(p2.clone()), &alloc, Layout { channels: 2, subframe: 2 }, 4800, 2, Arc::new(|| {}));
    let now = || d.now.load(Ordering::Acquire);
    let sleep = |ms: u64| {
        d.now.fetch_add(ms * 1_000_000, Ordering::AcqRel);
        if takes {
            pipe.complete(ms as usize / 2, None, &none);
        }
    };
    check::run("Lautsprecher-Buchse", kind, 4, b, &open, &Env { now_ns: &now, sleep_ms: &sleep, muted })
}

pub(crate) fn speaker_jack(b: &Board) -> uac2dev::Jack {
    uac2dev::bring_up_jack(b, &cfg(), &function(), terminal::SPEAKER, false, false).unwrap().0
}

#[test]
fn the_check_says_what_the_device_answered_at_each_step() {
    let b = Board::new();
    let j = speaker_jack(&b);
    b.rate.lock().unwrap().insert(5, 44_100);
    let pipe = FakePipe::new(40, 192);
    let line = tone_on(&Kind::Uac2(j), &b, &pipe, true);
    assert_eq!(line, "Tontest Lautsprecher-Buchse: gefunden; Takt 5 war 44100 Hz, auf 48000 Hz gesetzt, Gerät meldet 48000 Hz, gültig, \
        Schnittstelle 3 Einstellung 1; Strom gestartet; 192000 Bytes in 1000 ms gesendet; 0 Pakete verpasst, 0 Fehler; gestoppt, fertig.");
    assert!(b.errors().is_empty(), "{:?}", b.errors());
}

#[test]
fn the_check_stops_by_itself_when_nothing_is_taken() {
    let b = Board::new();
    let j = speaker_jack(&b);
    let pipe = FakePipe::new(40, 192);
    let line = tone_on(&Kind::Uac2(j), &b, &pipe, false);
    assert!(line.contains("Strom gestartet; das Gerät hat in 3000 ms nichts abgenommen (0 Bytes)"), "{line}");
    assert!(line.ends_with("gestoppt, 40 Pakete noch unterwegs."), "{line}");
}

#[test]
fn the_check_names_the_refusing_step_and_plays_nothing() {
    let b = Board::new();
    let j = speaker_jack(&b);
    b.valid.lock().unwrap().insert(5, false);
    let pipe = FakePipe::new(40, 192);
    let line = tone_on(&Kind::Uac2(j), &b, &pipe, true);
    assert_eq!(line, "Tontest Lautsprecher-Buchse: gefunden; Takt 5 meldet ungültig (bei 48000 Hz); abgebrochen, kein Ton.");
    assert_eq!(pipe.pending(), 0, "no stream was opened");
    // A UAC1 headset that cannot say its rate still gets its tone.
    let b = Board::new();
    let line = tone_on(&Kind::Uac1 { ep: 0x02, rate_control: true }, &b, &FakePipe::new(40, 192), true);
    assert!(line.contains("gefunden; das Gerät nennt seine Rate nicht (bei Headsets üblich); Strom gestartet; 192000 Bytes"), "{line}");
}

#[test]
fn a_device_unplugged_during_the_check_is_named() {
    let b = Board::new();
    let j = speaker_jack(&b);
    let pipe = FakePipe::new(40, 192);
    let d = day();
    let p2 = pipe.clone();
    let open = move || Playback::open(Box::new(p2.clone()), &alloc, Layout { channels: 2, subframe: 2 }, 4800, 2, Arc::new(|| {}));
    let now = || d.now.load(Ordering::Acquire);
    let sleep = |ms: u64| {
        let t = d.now.fetch_add(ms * 1_000_000, Ordering::AcqRel);
        let err = (t >= 200_000_000).then_some(UsbError::Gone);
        pipe.complete(ms as usize / 2, err, &none);
    };
    let line = check::run("Lautsprecher-Buchse", &Kind::Uac2(j), 4, &b, &open, &Env { now_ns: &now, sleep_ms: &sleep, muted: false });
    assert!(line.contains("Strom gestartet; abgebrochen nach 38400 Bytes: Gerät weg"), "{line}");
}
