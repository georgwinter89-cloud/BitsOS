//! Host tests: Georg's JBL dump through the real `usb_core` configuration parser, the plan, the class requests
//! against a recording fake, and hostile descriptors (every prefix, random mutations, loops, bad lengths).

use crate::jbl;
use crate::plan::{self, FunctionPlan, Sync};
use crate::req::{self, Ctl, RateResult};
use crate::uac::{self, Control, Rates, Unit};
use std::sync::Mutex;
use std::vec;
use std::vec::Vec;
use usb_core::{Configuration, Setup, UsbError};

fn cfg() -> Configuration {
    Configuration::parse(&jbl::config()).unwrap()
}

#[test]
fn the_dump_parses_whole() {
    let b = jbl::config();
    assert_eq!(b.len(), 0x147, "transcription has the dump's wTotalLength");
    let c = cfg();
    assert!(!c.truncated);
    assert_eq!(c.interfaces.len(), 10);
    let ac = Control::parse(&c.interface(0, 0).unwrap().extra);
    assert_eq!((ac.adc, ac.streams.clone(), ac.bad), (Some(0x0100), vec![1], 0));
    assert_eq!(ac.units.len(), 3);
    let chat = Control::parse(&c.interface(2, 0).unwrap().extra);
    assert_eq!((chat.streams.clone(), chat.units.len(), chat.bad), (vec![3, 4], 6, 0));
    match chat.unit(3) {
        Some(Unit::Fu(f)) => assert_eq!((f.source, f.controls.clone()), (6, vec![3, 0])),
        u => panic!("FU3: {u:?}"),
    }
}

#[test]
fn game_function_plays_stereo_on_alt_2() {
    let p = plan::plan(&cfg(), 0);
    assert_eq!(p.playback.len(), 1);
    assert!(p.capture.is_empty());
    let s = &p.playback[0];
    assert_eq!((s.iface, s.alt, s.ep.address, s.ep.max_packet()), (1, 2, 0x02, 288), "the streaming setting is alt 2, not 1");
    assert_eq!((s.channels, s.subframe, s.bits, s.sync), (2, 2, 16, Sync::Sync));
    assert!(s.rate_control && s.rate_only);
    assert_eq!(s.terminal_type, 0x0301);
    assert_eq!(s.features.len(), 1);
    assert_eq!((s.features[0].id, s.features[0].has(uac::FU_MUTE), s.features[0].has(uac::FU_VOLUME)), (2, true, false));
    assert!(p.skipped.iter().any(|k| (k.iface, k.alt, k.why) == (1, 1, "no isochronous data endpoint")));
    assert_eq!(s.frame_bytes(), 4);
}

#[test]
fn chat_function_has_mono_mic_and_mono_playback() {
    let p = plan::plan(&cfg(), 2);
    assert_eq!(p.capture.len(), 1);
    let m = &p.capture[0];
    assert_eq!((m.iface, m.alt, m.ep.address, m.ep.max_packet(), m.channels, m.terminal_type), (3, 1, 0x81, 144, 1, 0x0402));
    assert_eq!(m.features.iter().map(|f| f.id).collect::<Vec<_>>(), vec![3]);
    assert!(m.features[0].has(uac::FU_VOLUME) && m.features[0].has(uac::FU_MUTE));
    assert_eq!(p.playback.len(), 1);
    let c = &p.playback[0];
    assert_eq!((c.iface, c.ep.address, c.channels, c.terminal_type, c.features[0].id), (4, 0x06, 1, 0x0402, 4));
}

#[test]
fn best_prefers_stereo_16_bit_low_alt() {
    let p = plan::plan(&cfg(), 0);
    let mut a = p.playback[0].clone();
    let mut mono = a.clone();
    mono.alt = 1;
    mono.channels = 1;
    let mut s24 = a.clone();
    s24.alt = 3;
    s24.subframe = 3;
    a.alt = 4;
    let b = FunctionPlan::best(&[mono, s24, a.clone()]);
    assert_eq!(b.len(), 1);
    assert_eq!(b[0].alt, 4);
}

/// Records every control request; answers GET_CUR/MIN/MAX from its table; `fail` stalls requests with that bRequest.
#[derive(Default)]
struct Fake {
    log: Mutex<Vec<(Setup, Vec<u8>)>>,
    rate: Mutex<Option<[u8; 3]>>,
    min: i16,
    max: i16,
    fail_out: Option<u8>,
    fail_in: bool,
}

impl Ctl for Fake {
    fn out(&self, s: Setup, d: &[u8]) -> Result<usize, UsbError> {
        self.log.lock().unwrap().push((s, d.to_vec()));
        if self.fail_out == Some(s.request) {
            return Err(UsbError::Stall);
        }
        if s.request_type == 0x22 {
            *self.rate.lock().unwrap() = Some([d[0], d[1], d[2]]);
        }
        Ok(d.len())
    }
    fn input(&self, s: Setup, b: &mut [u8]) -> Result<usize, UsbError> {
        self.log.lock().unwrap().push((s, Vec::new()));
        if self.fail_in {
            return Err(UsbError::Stall);
        }
        let v = match (s.request_type, s.request) {
            (0xA2, req::GET_CUR) => self.rate.lock().unwrap().map(|r| r.to_vec()),
            (0xA1, req::GET_MIN) => Some(self.min.to_le_bytes().to_vec()),
            (0xA1, req::GET_MAX) => Some(self.max.to_le_bytes().to_vec()),
            (0xA1, req::GET_CUR) => Some(vec![1]),
            _ => None,
        };
        let v = v.ok_or(UsbError::Stall)?;
        let n = v.len().min(b.len());
        b[..n].copy_from_slice(&v[..n]);
        Ok(n)
    }
    fn set_interface(&self, _: u8, _: u8) -> Result<(), UsbError> {
        Ok(())
    }
}

#[test]
fn sampling_frequency_set_cur_on_the_endpoint_and_verified() {
    let s = plan::plan(&cfg(), 0).playback[0].clone();
    let f = Fake::default();
    assert_eq!(req::set_rate(&f, &s), Ok(RateResult::Set(48000)));
    let log = f.log.lock().unwrap();
    // linux-6.6/sound/usb/clock.c:414: USB_TYPE_CLASS | USB_RECIP_ENDPOINT | USB_DIR_OUT, UAC_EP_CS_ATTR_SAMPLE_RATE << 8, ep.
    assert_eq!(log[0].0, Setup { request_type: 0x22, request: 1, value: 0x0100, index: 0x02, length: 3 });
    assert_eq!(log[0].1, vec![0x80, 0xBB, 0x00]);
    assert_eq!(log[1].0, Setup { request_type: 0xA2, request: 0x81, value: 0x0100, index: 0x02, length: 3 });
    let g = Fake { fail_in: true, ..Default::default() };
    assert_eq!(req::set_rate(&g, &s), Ok(RateResult::Unverified), "a device that cannot read back still plays");
    let h = Fake { fail_out: Some(req::SET_CUR), ..Default::default() };
    assert_eq!(req::set_rate(&h, &s), Err(UsbError::Stall));
    let mut n = s.clone();
    n.rate_control = false;
    assert_eq!(req::set_rate(&Fake::default(), &n), Ok(RateResult::NoControl));
}

#[test]
fn feature_units_unmuted_and_at_0_db() {
    let p = plan::plan(&cfg(), 0);
    let f = Fake::default();
    let r = req::apply_features(&f, &p.playback[0], false);
    assert_eq!((r.mutes, r.volumes, r.failed), (1, 0, 0));
    let log = f.log.lock().unwrap();
    // FU2, master channel, interface 0: wValue MUTE_CONTROL << 8 | 0, wIndex unit << 8 | interface.
    assert_eq!(log[0].0, Setup { request_type: 0x21, request: 1, value: 0x0100, index: 0x0200, length: 1 });
    assert_eq!(log[0].1, vec![0]);
    drop(log);
    let mic = &plan::plan(&cfg(), 2).capture[0];
    let f = Fake { min: -0x1000, max: 0x0600, ..Default::default() };
    let r = req::apply_features(&f, mic, false);
    assert_eq!((r.mutes, r.volumes, r.failed, r.level), (1, 1, 0, 0));
    let log = f.log.lock().unwrap();
    let set_vol = log.iter().find(|x| x.0.request == 1 && x.0.value == 0x0200).unwrap();
    assert_eq!((set_vol.0.index, set_vol.1.clone()), (0x0302, vec![0, 0]));
    assert!(log.iter().any(|x| x.0 == Setup { request_type: 0xA1, request: 0x82, value: 0x0200, index: 0x0302, length: 2 }));
    drop(log);
    assert_eq!(req::get_mute(&f, mic), Some(true));
    let bad = Fake { fail_out: Some(1), ..Default::default() };
    assert_eq!(req::apply_features(&bad, mic, true).failed, 2);
    assert_eq!(req::set_mute(&bad, mic, true), Err(UsbError::Stall));
}

#[test]
fn unity_clamps_into_the_device_range() {
    assert_eq!(req::unity(Some(-0x2000), Some(0x0100)), 0);
    assert_eq!(req::unity(Some(-0x4000), Some(-0x0800)), -0x0800, "only attenuation: the loudest it has");
    assert_eq!(req::unity(Some(0x0100), Some(0x0400)), 0x0100, "only gain: the quietest step");
    assert_eq!(req::unity(Some(0x0400), Some(0x0100)), 0, "min above max: ignored");
    assert_eq!(req::unity(None, Some(5)), 0);
    assert_eq!(req::unity(Some(i16::MIN), Some(i16::MIN)), 0, "silence only: ignored, no panic");
    assert_eq!(req::unity(Some(i16::MIN), Some(-0x100)), -0x100);
}

fn ac(units: &[&[u8]]) -> Vec<u8> {
    units.concat()
}

#[test]
fn hostile_control_descriptors_never_panic() {
    // Lengths 0 and 1, an overrun, a foreign type, FU with bControlSize 0 and 5, FU without channels.
    for b in [
        vec![0u8, 0x24, 1],
        vec![1u8, 0x24],
        vec![200u8, 0x24, 2, 1],
        ac(&[&[3, 0x05, 1], &[9, 0x24, 1, 0, 1, 9, 0, 1, 1]]),
        ac(&[&[10, 0x24, 6, 2, 1, 0, 1, 0, 0, 0]]),
        ac(&[&[10, 0x24, 6, 2, 1, 5, 1, 0, 0, 0]]),
        ac(&[&[7, 0x24, 6, 2, 1, 1, 0]]),
        ac(&[&[9, 0x24, 1, 0, 1, 9, 0, 200, 1]]),
        ac(&[&[5, 0x24, 4, 9, 250]]),
        ac(&[&[7, 0x24, 7, 9, 0, 0, 255]]),
        ac(&[&[12, 0x24, 2, 0, 1, 1, 0, 2, 3, 0, 0, 0]]),
    ] {
        let c = Control::parse(&b);
        assert!(c.bad > 0 || c.units.is_empty(), "{b:?}: {c:?}");
        let _ = c.path(1, 2);
    }
}

#[test]
fn unit_loops_and_dangling_sources_end_the_search() {
    let it = [12u8, 0x24, 2, 1, 1, 1, 0, 2, 3, 0, 0, 0];
    // FU 2 <- FU 3 <- FU 2 (loop), OT 5 <- FU 2; IT 1 unreachable.
    let c = Control::parse(&ac(&[&it, &[10, 0x24, 6, 2, 3, 1, 1, 0, 0, 0], &[10, 0x24, 6, 3, 2, 1, 1, 0, 0, 0], &[9, 0x24, 3, 5, 1, 3, 0, 2, 0]]));
    assert_eq!(c.path(1, 5), None);
    assert!(c.outputs_from(1).is_empty());
    // OT 5 <- 9 (no such unit).
    let c = Control::parse(&ac(&[&it, &[9, 0x24, 3, 5, 1, 3, 0, 9, 0]]));
    assert_eq!(c.path(1, 5), None);
    // A self loop on a selector beside the real source.
    let c = Control::parse(&ac(&[&it, &[7, 0x24, 5, 4, 2, 4, 1], &[9, 0x24, 3, 5, 1, 3, 0, 4, 0]]));
    assert_eq!(c.path(1, 5), Some(vec![5, 4, 1]));
    // Duplicate unit IDs: the first one counts, the second is bad.
    let c = Control::parse(&ac(&[&it, &it]));
    assert_eq!((c.units.len(), c.bad), (1, 1));
}

#[test]
fn hostile_stream_formats() {
    assert_eq!(uac::stream_format(&[7, 0x24, 1, 1, 0, 1, 0]), None, "no FORMAT_TYPE");
    assert_eq!(uac::stream_format(&[7, 0x24, 1, 1, 0, 1, 0, 11, 0x24, 2, 1, 2, 2, 16, 255, 0, 0, 0]), None, "255 rates in 3 bytes");
    let f = uac::stream_format(&[7, 0x24, 1, 1, 0, 1, 0, 14, 0x24, 2, 1, 2, 2, 16, 0, 0x44, 0xAC, 0, 0x80, 0xBB, 0]).unwrap();
    assert_eq!(f.rates, Rates::Continuous { min: 44100, max: 48000 });
    assert!(f.rates.has(48000) && !f.rates.only(48000));
    assert_eq!(uac::endpoint_general(&[7, 0x25, 1, 1]), None);
    assert_eq!(uac::endpoint_general(&[7, 0x25, 1, 1, 0, 0, 0]), Some((1, 0, 0)));
}

#[test]
fn every_prefix_and_random_mutations_of_the_dump_plan_without_panic() {
    let b = jbl::config();
    let run = |x: &[u8]| {
        if let Ok(c) = Configuration::parse(x) {
            for f in c.interfaces.iter().filter(|f| f.class == 1 && f.subclass == 1) {
                let p = plan::plan(&c, f.number);
                for s in p.playback.iter().chain(p.capture.iter()) {
                    assert!(s.channels >= 1 && s.channels <= 2 && s.ep.max_packet() > 0);
                }
            }
        }
    };
    for n in 0..=b.len() {
        let mut x = b[..n].to_vec();
        // The prefix claims its own length, so the walk sees the cut, not the old total.
        if x.len() >= 4 {
            x[2] = n as u8;
            x[3] = (n >> 8) as u8;
        }
        run(&x);
    }
    let mut seed = 0x2545_F491_4F6C_DD1Du64;
    let mut rnd = || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    for _ in 0..4000 {
        let mut x = b.clone();
        for _ in 0..1 + rnd() % 6 {
            let i = 9 + (rnd() as usize % (x.len() - 9));
            x[i] = rnd() as u8;
        }
        run(&x);
    }
}

#[test]
fn a_function_without_48k_is_skipped_with_a_reason() {
    let mut b = jbl::config();
    // Game format: 48000 -> 44100.
    let at = b.windows(11).position(|w| w == [11, 0x24, 2, 1, 2, 2, 16, 1, 0x80, 0xBB, 0x00]).unwrap();
    b[at + 8] = 0x44;
    b[at + 9] = 0xAC;
    let p = plan::plan(&Configuration::parse(&b).unwrap(), 0);
    assert!(p.playback.is_empty());
    assert!(p.skipped.iter().any(|k| k.why == "no 48 kHz"));
}
