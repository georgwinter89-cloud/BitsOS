use super::fixture::{self, MIC_RATES, SPEAKER_RATES, range_reply};
use crate::desc::*;
use crate::plan::*;
use crate::range::Ranges;

fn function() -> Function {
    Function::parse(&fixture::config()).unwrap()
}

fn plan_at(want: &Want, rates: &[u32]) -> Result<Plan, PlanError> {
    let reply = range_reply(rates);
    plan(&function(), want, &Ranges::parse(&reply).unwrap())
}

#[test]
fn the_speaker_gets_s16_stereo_at_48_khz() {
    let p = plan_at(&SPEAKER_S16_48K, &SPEAKER_RATES).unwrap();
    assert_eq!((p.control_iface, p.iface, p.alt), (0, 3, 1));
    assert_eq!(p.clock, 5);
    assert!(p.playback);
    assert_eq!((p.channels, p.subslot, p.bits, p.rate), (2, 2, 16, 48_000));
    assert_eq!(p.endpoint.address, 0x05);
    assert_eq!(p.endpoint.sync(), Sync::Adaptive);
    assert_eq!(p.feature, Some(29));
    // 48 kHz at 125 us is 6 frames, plus the one of margin, four bytes each.
    assert_eq!(p.max_packet_bytes, 28);
}

#[test]
fn each_format_finds_its_own_alternate_setting() {
    let alt = |bits, channels| {
        let want = Want { bits, channels, ..SPEAKER_S16_48K };
        plan_at(&want, &SPEAKER_RATES).map(|p| (p.alt, p.subslot))
    };
    assert_eq!(alt(16, 2), Ok((1, 2)));
    assert_eq!(alt(24, 2), Ok((2, 3)));
    assert_eq!(alt(32, 2), Ok((3, 4)));
    assert_eq!(alt(16, 4), Ok((4, 2)));
    assert_eq!(alt(32, 8), Ok((12, 4)));
}

#[test]
fn the_other_jacks_find_their_interfaces() {
    let at = |jack, rates: &[u32]| {
        let want = Want { jack, ..SPEAKER_S16_48K };
        plan_at(&want, rates).map(|p| (p.iface, p.alt, p.clock, p.playback, p.feature))
    };
    assert_eq!(at(terminal::HEADPHONES, &SPEAKER_RATES), Ok((4, 1, 6, true, Some(30))));
    assert_eq!(at(terminal::SPDIF, &SPEAKER_RATES), Ok((5, 1, 8, true, Some(32))));
    assert_eq!(at(terminal::MICROPHONE, &MIC_RATES), Ok((2, 1, 4, false, Some(28))));
    assert_eq!(at(terminal::DESKTOP_MICROPHONE, &MIC_RATES), Ok((1, 1, 3, false, Some(27))));
}

#[test]
fn a_capture_stream_may_be_asynchronous_a_playback_stream_may_not() {
    let mic = Want { jack: terminal::MICROPHONE, ..SPEAKER_S16_48K };
    assert_eq!(plan_at(&mic, &MIC_RATES).unwrap().endpoint.sync(), Sync::Async);

    let mut f = function();
    for a in f.alts.iter_mut().filter(|a| a.iface == 3) {
        a.endpoint.attributes = 5;
    }
    let reply = range_reply(&SPEAKER_RATES);
    let got = plan(&f, &SPEAKER_S16_48K, &Ranges::parse(&reply).unwrap());
    assert_eq!(got, Err(PlanError::NeedsFeedback(0x05)));
}

#[test]
fn a_rate_the_clock_does_not_offer_is_refused() {
    let want = Want { rate: 22_050, ..SPEAKER_S16_48K };
    assert_eq!(plan_at(&want, &SPEAKER_RATES), Err(PlanError::RateNotOffered(22_050)));
    assert_eq!(plan_at(&SPEAKER_S16_48K, &[]), Err(PlanError::RateNotOffered(48_000)));
    let high = Want { jack: terminal::MICROPHONE, rate: 384_000, ..SPEAKER_S16_48K };
    assert_eq!(plan_at(&high, &MIC_RATES), Err(PlanError::RateNotOffered(384_000)));
}

#[test]
fn a_format_nobody_carries_is_refused() {
    for (channels, bits) in [(3, 16), (2, 8), (2, 20), (0, 16)] {
        let want = Want { channels, bits, ..SPEAKER_S16_48K };
        assert_eq!(plan_at(&want, &SPEAKER_RATES), Err(PlanError::NoFormat { channels, bits }));
    }
    // The headphones are stereo only.
    let four = Want { jack: terminal::HEADPHONES, channels: 4, ..SPEAKER_S16_48K };
    assert_eq!(plan_at(&four, &SPEAKER_RATES), Err(PlanError::NoFormat { channels: 4, bits: 16 }));
}

#[test]
fn an_alternate_setting_with_more_than_pcm_is_not_chosen() {
    let mut f = function();
    f.alts.retain(|a| a.iface != 5 || a.alt == 11);
    assert_eq!(f.alts.iter().find(|a| a.iface == 5).unwrap().formats, 0x1381);
    let want = Want { jack: terminal::SPDIF, ..SPEAKER_S16_48K };
    let reply = range_reply(&SPEAKER_RATES);
    let got = plan(&f, &want, &Ranges::parse(&reply).unwrap());
    assert_eq!(got, Err(PlanError::NoFormat { channels: 2, bits: 16 }));

    // Type I with the same bits is not plain PCM either, and Type III with only the PCM bit is not Type I.
    for (format_type, formats) in [(FORMAT_TYPE_I, 0x1381), (3, FORMAT_PCM)] {
        let mut f = function();
        f.alts.retain(|a| a.iface != 5 || a.alt == 1);
        let a = f.alts.iter_mut().find(|a| a.iface == 5).unwrap();
        (a.format_type, a.formats) = (format_type, formats);
        let got = plan(&f, &want, &Ranges::parse(&reply).unwrap());
        assert_eq!(got, Err(PlanError::NoFormat { channels: 2, bits: 16 }), "{format_type} {formats:#x}");
    }
}

#[test]
fn a_jack_the_device_lacks_is_refused() {
    let want = Want { jack: 0x0303, ..SPEAKER_S16_48K };
    assert_eq!(plan_at(&want, &SPEAKER_RATES), Err(PlanError::NoSuchJack(0x0303)));
    let want = Want { jack: 0x0204, ..SPEAKER_S16_48K };
    assert_eq!(plan_at(&want, &SPEAKER_RATES), Err(PlanError::NoSuchJack(0x0204)));
}

fn plan_on(f: &Function, want: &Want) -> Result<Plan, PlanError> {
    let reply = range_reply(&SPEAKER_RATES);
    plan(f, want, &Ranges::parse(&reply).unwrap())
}

#[test]
fn a_path_through_a_unit_it_cannot_read_is_refused() {
    let mut f = function();
    f.outputs.iter_mut().find(|o| o.id == 21).unwrap().source = 33;
    assert_eq!(plan_on(&f, &SPEAKER_S16_48K), Err(PlanError::PathUnsupported { unit: 33 }));
}

#[test]
fn a_loop_of_feature_units_ends() {
    let mut f = function();
    f.features.iter_mut().find(|u| u.id == 29).unwrap().source = 29;
    assert!(matches!(plan_on(&f, &SPEAKER_S16_48K), Err(PlanError::PathUnsupported { .. })));
}

#[test]
fn a_jack_with_no_usb_side_is_refused() {
    let mut f = function();
    // The speaker's path now starts at a microphone: no stream can feed it.
    f.features.iter_mut().find(|u| u.id == 29).unwrap().source = 12;
    assert_eq!(plan_on(&f, &SPEAKER_S16_48K), Err(PlanError::NoUsbSide(terminal::SPEAKER)));

    let mut f = function();
    f.outputs.retain(|o| o.id != 20);
    let mic = Want { jack: terminal::MICROPHONE, ..SPEAKER_S16_48K };
    assert_eq!(plan_on(&f, &mic), Err(PlanError::NoUsbSide(terminal::MICROPHONE)));
}

#[test]
fn the_clock_must_exist_and_be_settable() {
    let mut f = function();
    f.clocks.retain(|c| c.id != 5);
    assert_eq!(plan_on(&f, &SPEAKER_S16_48K), Err(PlanError::ClockNotASource(5)));

    let mut f = function();
    f.clocks.iter_mut().find(|c| c.id == 5).unwrap().controls = 0b0101;
    assert_eq!(plan_on(&f, &SPEAKER_S16_48K), Err(PlanError::ClockFixed(5)));
}

#[test]
fn a_packet_too_small_for_the_stream_is_refused() {
    let mut f = function();
    f.alts.iter_mut().find(|a| a.iface == 3 && a.alt == 1).unwrap().endpoint.max_packet = 27;
    assert_eq!(plan_on(&f, &SPEAKER_S16_48K), Err(PlanError::PacketTooSmall { need: 28, have: 27 }));
    f.alts.iter_mut().find(|a| a.iface == 3 && a.alt == 1).unwrap().endpoint.max_packet = 28;
    assert!(plan_on(&f, &SPEAKER_S16_48K).is_ok());
}

/// Every format the speaker has at its highest rate fits, in the packet sizes the device states.
#[test]
fn the_device_has_room_for_everything_it_offers() {
    for channels in [2, 4, 6, 8] {
        for bits in [16, 24, 32] {
            let want = Want { rate: 384_000, channels, bits, ..SPEAKER_S16_48K };
            assert!(plan_at(&want, &SPEAKER_RATES).is_ok(), "{channels} channels {bits} bits");
        }
    }
}

#[test]
fn an_endpoint_that_is_not_isochronous_is_refused() {
    let mut f = function();
    f.alts.iter_mut().find(|a| a.iface == 3 && a.alt == 1).unwrap().endpoint.attributes = 2;
    assert_eq!(plan_on(&f, &SPEAKER_S16_48K), Err(PlanError::BadEndpoint(0x05)));
    let mut f = function();
    f.alts.iter_mut().find(|a| a.iface == 3 && a.alt == 1).unwrap().endpoint.interval = 0;
    assert_eq!(plan_on(&f, &SPEAKER_S16_48K), Err(PlanError::BadEndpoint(0x05)));
}

#[test]
fn a_longer_interval_needs_a_bigger_packet() {
    let mut f = function();
    let ep = &mut f.alts.iter_mut().find(|a| a.iface == 3 && a.alt == 1).unwrap().endpoint;
    ep.interval = 4;
    ep.max_packet = 196;
    // 1 ms at 48 kHz is 48 frames, plus one, four bytes each.
    assert_eq!(plan_on(&f, &SPEAKER_S16_48K).unwrap().max_packet_bytes, 196);
    f.alts.iter_mut().find(|a| a.iface == 3 && a.alt == 1).unwrap().endpoint.max_packet = 195;
    assert_eq!(plan_on(&f, &SPEAKER_S16_48K), Err(PlanError::PacketTooSmall { need: 196, have: 195 }));
}

/// A full-speed device's interval counts 1 ms frames: bInterval 1 needs room for 49 frames, not 7 (the board is high speed).
#[test]
fn a_full_speed_device_needs_a_frame_of_room() {
    let mut f = function();
    f.alts.iter_mut().find(|a| a.iface == 3 && a.alt == 1).unwrap().endpoint.max_packet = 100;
    assert_eq!(plan_on(&f, &SPEAKER_S16_48K).unwrap().max_packet_bytes, 28);
    let fs = Want { full_speed: true, ..SPEAKER_S16_48K };
    assert_eq!(plan_on(&f, &fs), Err(PlanError::PacketTooSmall { need: 196, have: 100 }));
    // Bits 12:11 are a high-speed multiplier only; at full speed they do not add room.
    f.alts.iter_mut().find(|a| a.iface == 3 && a.alt == 1).unwrap().endpoint.max_packet = 0x0800 | 100;
    assert_eq!(plan_on(&f, &fs), Err(PlanError::PacketTooSmall { need: 196, have: 100 }));
    f.alts.iter_mut().find(|a| a.iface == 3 && a.alt == 1).unwrap().endpoint.max_packet = 196;
    assert_eq!(plan_on(&f, &fs).unwrap().max_packet_bytes, 196);
}

#[test]
fn each_jack_names_its_own_clock() {
    let f = function();
    let c = |jack| crate::plan::clock_for(&f, jack);
    assert_eq!((c(terminal::SPEAKER), c(terminal::HEADPHONES), c(terminal::SPDIF)), (Ok(5), Ok(6), Ok(8)));
    assert_eq!(c(terminal::MICROPHONE), Ok(4));
    assert_eq!(c(0x0402), Err(PlanError::NoSuchJack(0x0402)));
}

#[test]
fn a_feedback_endpoint_after_the_data_endpoint_is_not_taken_for_it() {
    // Interface 3 alternate setting 1, made asynchronous.
    let a = fixture::Alt { iface: 3, alt: 1, link: 13, formats: 1, channels: 2, config: 3, subslot: 2, bits: 16, ep: 0x05, attributes: 5, mps: 0x00f8 };
    let mut body = fixture::control_interface(&fixture::control_tree(), (9 + fixture::control_tree().len()) as u16);
    body.extend_from_slice(&[9, 4, 3, 0, 0, 1, 2, 0x20, 0]);
    fixture::streaming_alt(&mut body, &a);
    // The feedback endpoint: IN 0x85, isochronous, usage type 1 (bits 5:4), 4 bytes.
    body.extend_from_slice(&[7, 5, 0x85, 0x11, 4, 0, 4]);
    let f = Function::parse(&fixture::wrap(&body, 2)).unwrap();
    let alt = f.alts.iter().find(|x| x.iface == 3).unwrap();
    assert_eq!((alt.endpoint.address, alt.feedback), (0x05, Some(0x85)));
    let rates = range_reply(&SPEAKER_RATES);
    let r = crate::range::Ranges::parse(&rates).unwrap();
    assert_eq!(crate::plan::plan(&f, &crate::plan::SPEAKER_S16_48K, &r), Err(PlanError::NeedsFeedback(0x05)), "still refused by name");
}
