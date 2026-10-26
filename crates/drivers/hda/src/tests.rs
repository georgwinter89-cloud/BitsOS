//! Host tests: the controller logic against `fake.rs` with the SN-1 codec fixtures (ALC269, AMD AA01) and a
//! QEMU hda-duplex-like codec.

extern crate std;

use crate::ctrl::{Config, Controller, HdaError, Kind, OutputId, State};
use crate::fake::{Ev, FakeHda, FakeHw};
use crate::link::Mode;
use crate::regs::*;
use crate::signal;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};
use devmodel::audio::{Output as Port, PcmFormat};
use hdacodec::path::OutputKind;
use hdacodec::{NoResponse, VerbSource};
use std::string::{String, ToString};

const ALC269: &str = include_str!("../../../hdacodec/fixtures/alc269-synthetic.txt");
const AA01: &str = include_str!("../../../hdacodec/fixtures/aa01-synthetic.txt");
const QEMU_GCAP: u16 = 0x4401;
const F48: PcmFormat = PcmFormat { rate: 48000, channels: 2, bits: 16 };
const HP: OutputId = OutputId { cad: 0, pin: 0x15 };
const SPK: OutputId = OutputId { cad: 0, pin: 0x14 };

/// QEMU's hda-duplex (hw/audio/hda-codec-common.h): DAC 2, line-out pin 3, ADC 4, line-in pin 5; `mixer`
/// gives the DAC an output amp (QEMU_HDA_AMP_STEPS 0x4a).
struct Duplex {
    mixer: bool,
}

impl VerbSource for Duplex {
    fn get(&mut self, nid: u8, verb: u32) -> Result<u32, NoResponse> {
        let pcm = 0x0002_01FC;
        let amp = 0x0003_4A4A;
        Ok(match (nid, verb) {
            (0, 0xF0000) => 0x1AF4_0022,
            (0, 0xF0002) => 0x0010_0101,
            (0, 0xF0004) => 0x0001_0001,
            (1, 0xF0005) => 1,
            (1, 0xF0004) => 0x0002_0004,
            (1, 0xF000A) | (2, 0xF000A) | (4, 0xF000A) => pcm,
            (1, 0xF000B) | (2, 0xF000B) | (4, 0xF000B) => 1,
            (2, 0xF0009) => if self.mixer { 0x1D } else { 0x11 },
            (2, 0xF0012) => if self.mixer { amp } else { 0 },
            (3, 0xF0009) => 0x0040_0100,
            (3, 0xF000C) => 0x10,
            (3, 0xF1C00) => 0x0000_4010,
            (3, 0xF000E) | (4, 0xF000E) => 1,
            (3, 0xF0200) => 2,
            (4, 0xF0009) => 0x0010_011B,
            (4, 0xF000D) => amp,
            (4, 0xF0200) => 5,
            (5, 0xF0009) => 0x0040_0000,
            (5, 0xF000C) => 0x20,
            (5, 0xF1C00) => 0x0081_2020,
            (0..=5, _) => 0,
            _ => return Err(NoResponse),
        })
    }
}

pub(crate) fn duplex_text(mixer: bool) -> String {
    hdacodec::walk::dump_codec(Duplex { mixer }, 0, &[("format", "hdacodec-dump 1")]).to_text()
}

fn ctrl(kind: Kind, codecs: &[(u8, &str)]) -> (Arc<FakeHda>, Controller<FakeHw>) {
    ctrl_cfg(kind, codecs, Config::default())
}

fn ctrl_cfg(kind: Kind, codecs: &[(u8, &str)], cfg: Config) -> (Arc<FakeHda>, Controller<FakeHw>) {
    let f = FakeHda::new(QEMU_GCAP);
    for &(cad, t) in codecs {
        f.add_codec(cad, t);
    }
    let c = Controller::new("hda0".into(), kind, FakeHw(f.clone()), cfg);
    (f, c)
}

pub(crate) fn up(kind: Kind, codecs: &[(u8, &str)]) -> (Arc<FakeHda>, Controller<FakeHw>) {
    let (f, c) = ctrl(kind, codecs);
    c.bring_up().unwrap();
    assert!(f.bad_accesses().is_empty(), "{:?}", f.bad_accesses());
    (f, c)
}

/// The first output descriptor (QEMU: index 4, after four inputs).
const OUT0: usize = 4;

#[test]
fn bring_up_alc269() {
    let (f, c) = up(Kind::Intel, &[(0, ALC269)]);
    assert_eq!(c.state(), State::Up);
    assert_eq!(c.link_mode(), Some(Mode::Rings));
    assert!(f.has(&Ev::Enable(true)));
    assert!(f.has(&Ev::PciModify(PCI_TCSEL, 7, 0)));
    let cs = c.codecs();
    assert_eq!(cs.len(), 1);
    assert_eq!(cs[0].model.vendor_id, 0x10EC_0269);
    assert!(hdacodec::walk::check(&cs[0].dump, 0).complete());
    let outs = c.outputs();
    let kinds: Vec<(u8, OutputKind)> = outs.iter().map(|o| (o.id.pin, o.kind)).collect();
    assert_eq!(kinds, [(0x14, OutputKind::Speaker), (0x15, OutputKind::Headphone)]);
    assert!(outs.iter().all(|o| o.playable && o.converter == 0x02 && o.port == Port::Jack));
    assert_eq!(outs[0].formats[0], F48);
    // The headphone sends plug events: unsolicited enable with tag 1 (708 | 0x80 | 1), its state was read.
    assert!(f.sets(0).contains(&(0x15, 0x70881)), "{:?}", f.sets(0));
    assert_eq!(c.jack_present(HP), Some(false));
    assert_eq!(c.jack_present(SPK), None);
    // Unsolicited responses on, interrupts not yet (polled until MSI).
    assert_ne!(f.reg32(GCTL) & GCTL_UNSOL, 0);
    assert_eq!(f.reg32(INTCTL), 0);
}

#[test]
fn amd_fch_quirks() {
    let (f, c) = up(Kind::AmdFch, &[(0, ALC269)]);
    // ATI snoop byte 0x42 = 2 (bits 18:16 of dword 0x40), no TCSEL.
    assert!(f.has(&Ev::PciModify(0x40, 7 << 16, 2 << 16)));
    assert!(!f.trace.lock().iter().any(|e| matches!(e, Ev::PciModify(PCI_TCSEL, ..))));
    assert!(c.snooped());
}

#[test]
fn gpu_controller_is_only_read() {
    let (f, c) = up(Kind::AtiHdmi, &[(0, AA01)]);
    assert!(f.has(&Ev::Enable(false)) && !f.has(&Ev::Enable(true)));
    assert!(!f.trace.lock().iter().any(|e| matches!(e, Ev::Alloc(..) | Ev::DmaWithoutBme)));
    assert_eq!(c.link_mode(), Some(Mode::Immediate));
    // Not a single set verb reached the codec (no ATI list, no 709 trigger, no unsolicited enable).
    assert!(f.sets(0).is_empty(), "{:?}", f.sets(0));
    let outs = c.outputs();
    assert_eq!(outs.len(), 5);
    assert!(outs.iter().all(|o| o.port == Port::Hdmi && !o.playable));
    // Back into reset afterwards.
    assert_eq!(f.reg32(GCTL) & GCTL_CRST, 0);
    assert_eq!(c.open(outs[0].id, F48, 4096, 1024), Err(HdaError::NotUp));
}

/// SN-5: once the display side sends audio the GPU controller streams like the others (bus master, CORB/RIRB,
/// snooped as Linux's generic ATI entry), only the DP audio pin (3) is playable, it plays sample-exact through the
/// ATI setup list; back to enumerate-only it is in reset with bus master off again.
#[test]
fn gpu_controller_streams_after_display_side() {
    let (f, c) = up(Kind::AtiHdmi, &[(0, AA01)]);
    assert!(c.read_only());
    c.set_streaming(true);
    c.bring_up().unwrap();
    assert!(!c.read_only() && f.has(&Ev::Enable(true)));
    assert_eq!(c.link_mode(), Some(Mode::Rings));
    assert!(c.snooped());
    assert_ne!(f.reg32(GCTL) & GCTL_CRST, 0, "out of reset");
    let outs = c.outputs();
    let playable: Vec<u8> = outs.iter().filter(|o| o.playable).map(|o| o.id.pin).collect();
    assert_eq!(playable, [crate::DP_AUDIO_PIN]);
    assert!(outs.iter().all(|o| o.port == Port::Hdmi));
    let pin = OutputId { cad: 0, pin: crate::DP_AUDIO_PIN };
    assert_eq!(c.open(OutputId { cad: 0, pin: 5 }, F48, 4096, 1024), Err(HdaError::Refused), "endpoint 1 gets no DP audio");
    f.clear_sets(0);
    let sid = c.open(pin, F48, 4096, 1024).unwrap();
    let codec = c.codecs()[0].clone();
    let out = codec.outputs.iter().find(|o| o.pin == pin.pin).unwrap();
    let want: Vec<(u8, u32)> = hdacodec::setup::output_setup(&codec.model, out, &hdacodec::setup::Stream { tag: 1, channel: 0, format: 0x11 })
        .unwrap()
        .into_iter()
        .filter_map(|a| match a {
            hdacodec::setup::Action::Set { nid, verb } => Some((nid, verb)),
            _ => None,
        })
        .collect();
    assert_eq!(f.sets(0), want, "the ATI list (patch_hdmi.c order)");
    let pcm = signal::bytes(0, 6144, false);
    assert_eq!(c.write(sid, &pcm).unwrap(), 16384);
    c.start(sid).unwrap();
    let mut sent = 16384usize;
    while c.position(sid).unwrap() < 6144 {
        f.advance(1000);
        c.service();
        if sent < pcm.len() {
            sent += c.write(sid, &pcm[sent..]).unwrap();
        }
    }
    let sd = f.sd(OUT0);
    assert_eq!(&sd.sink[..pcm.len()], &pcm[..]);
    c.close(sid).unwrap();
    c.set_streaming(false);
    c.bring_up().unwrap();
    assert!(c.read_only());
    assert_eq!(f.reg32(GCTL) & GCTL_CRST, 0);
    assert!(c.outputs().iter().all(|o| !o.playable));
    assert_eq!(c.open(pin, F48, 4096, 1024), Err(HdaError::NotUp));
}

/// Opens the headphone, checks verbs, descriptor and BDL, plays a ring and a half and compares the sink.
#[test]
fn play_headphone_sample_exact() {
    let (f, c) = up(Kind::Intel, &[(0, ALC269)]);
    f.clear_sets(0);
    let sid = c.open(HP, F48, 4096, 1024).unwrap();
    // The verbs are hdacodec's output_setup list for stream tag 1, 48 kHz/16 bit.
    let codec = c.codecs()[0].clone();
    let out = codec.outputs.iter().find(|o| o.pin == 0x15).unwrap();
    let want: Vec<(u8, u32)> = hdacodec::setup::output_setup(&codec.model, out, &hdacodec::setup::Stream { tag: 1, channel: 0, format: 0x11 })
        .unwrap()
        .into_iter()
        .filter_map(|a| match a {
            hdacodec::setup::Action::Set { nid, verb } => Some((nid, verb)),
            _ => None,
        })
        .collect();
    assert_eq!(f.sets(0), want);
    let sd = f.sd(OUT0);
    assert_eq!((sd.cbl, sd.lvi, sd.fmt, sd.tag()), (16384, 3, 0x0011, 1));
    // BDL: four 4096-byte periods, contiguous, IOC off (polled).
    for i in 0..4u64 {
        let e = f.mem_bytes(sd.bdl() + i * 16, 16).unwrap();
        assert_eq!(u32::from_le_bytes(e[8..12].try_into().unwrap()), 4096);
        assert_eq!(u32::from_le_bytes(e[12..16].try_into().unwrap()), 0);
    }
    let total = 6144u64;
    let pcm = signal::bytes(0, total as usize, false);
    assert_eq!(c.write(sid, &pcm).unwrap(), 16384, "a full ring");
    let periods = Arc::new(AtomicU64::new(0));
    let p = periods.clone();
    c.set_period_callback(sid, Some(Arc::new(move |_, _| { p.fetch_add(1, Ordering::AcqRel); }))).unwrap();
    c.start(sid).unwrap();
    let mut sent = 16384usize;
    while (c.position(sid).unwrap()) < total {
        f.advance(1000);
        c.service();
        if sent < pcm.len() {
            sent += c.write(sid, &pcm[sent..]).unwrap();
        }
    }
    let sink = f.sd(OUT0).sink;
    assert_eq!(&sink[..pcm.len()], &pcm[..]);
    assert!(periods.load(Ordering::Acquire) >= 5);
    // Delay = queued frames + FIFOS (0xFF + 1 bytes = 64 frames) on Intel.
    let d = c.delay(sid).unwrap() as u64;
    assert_eq!(d, (sent as u64 / 4).saturating_sub(c.position(sid).unwrap()) + 64);
    c.stop(sid).unwrap();
    assert!(!f.sd(OUT0).running());
    c.close(sid).unwrap();
    assert_eq!(f.sets(0).last(), Some(&(0x02, 0x70600)));
    assert_eq!(c.position(sid), Err(HdaError::NoSuchStream));
}

#[test]
fn amd_lpib_shows_cbl_at_the_wrap() {
    let (f, c) = up(Kind::AmdFch, &[(0, ALC269)]);
    f.amd_wrap.store(true, Ordering::Release);
    let sid = c.open(HP, F48, 1024, 256).unwrap();
    c.write(sid, &signal::bytes(0, 1024, false)).unwrap();
    c.start(sid).unwrap();
    f.advance(2048);
    assert_eq!(c.position(sid).unwrap(), 512);
    f.advance(2048); // right at the wrap: LPIB reads CBL, which is 0
    assert_eq!(f.reg32(sd(OUT0) + SD_LPIB), 4096);
    assert_eq!(c.position(sid).unwrap(), 1024);
    f.advance(400);
    assert_eq!(c.position(sid).unwrap(), 1124);
    // AMD FIFO delay: 32 frames once the FIFO filled.
    assert_eq!(c.delay(sid).unwrap(), 32);
}

#[test]
fn underrun_silences_the_ring() {
    let (f, c) = up(Kind::Intel, &[(0, ALC269)]);
    let sid = c.open(HP, F48, 1024, 256).unwrap();
    c.write(sid, &signal::bytes(0, 512, false)).unwrap();
    c.start(sid).unwrap();
    f.advance(3000);
    c.service();
    f.advance(4096);
    let sink = f.sd(OUT0).sink;
    // After the underrun (played 750 frames of 512 written) the old samples are not played again.
    assert!(sink[3000..].iter().all(|&b| b == 0), "stale samples replayed");
    assert_eq!(c.write(sid, &[0u8; 4]).unwrap(), 4);
}

#[test]
fn irq_ack_quiets_the_controller() {
    let (f, c) = up(Kind::Intel, &[(0, ALC269)]);
    c.set_irq_driven(true);
    // Outputs 4-7 and (SN-9) inputs 0-3.
    assert_eq!(f.reg32(INTCTL), INT_GLOBAL_EN | INT_CTRL_EN | 0xFF);
    let sid = c.open(HP, F48, 1024, 256).unwrap();
    c.write(sid, &signal::bytes(0, 1024, false)).unwrap();
    c.start(sid).unwrap();
    f.advance(1024);
    assert_ne!(f.reg32(INTSTS) & 1 << OUT0, 0);
    let seen = c.irq_ack();
    assert_ne!(seen & 1 << OUT0, 0);
    assert_eq!(f.reg32(INTSTS), 0);
    assert_eq!(c.irq_count(), 1);
    assert_eq!(c.irq_ack(), 0);
}

#[test]
fn unsolicited_plug_event() {
    let (f, c) = up(Kind::Intel, &[(0, ALC269)]);
    let got = Arc::new(ksync::SpinLock::new(Vec::new()));
    let g = got.clone();
    c.on_jack(Arc::new(move |id, p| g.lock().push((id, p))));
    f.set_sense(0, 0x15, 0x8000_0000);
    f.unsol(0, 1 << 26); // tag 1 = the headphone
    c.service();
    assert_eq!(c.jack_present(HP), Some(true));
    assert_eq!(*got.lock(), [(HP, true)]);
    // An unknown tag changes nothing.
    f.set_sense(0, 0x15, 0);
    f.unsol(0, 9 << 26);
    c.service();
    assert_eq!(c.jack_present(HP), Some(true));
}

#[test]
fn dead_corb_falls_back_to_immediate() {
    let (f, c) = ctrl(Kind::Intel, &[(0, ALC269)]);
    f.corb_dead.store(true, Ordering::Release);
    c.bring_up().unwrap();
    assert_eq!(c.link_mode(), Some(Mode::Immediate));
    assert_eq!(c.codecs()[0].model.vendor_id, 0x10EC_0269);
    assert_eq!(c.outputs().len(), 2);
}

#[test]
fn quiesce_then_bring_up_again() {
    let (f, c) = up(Kind::Intel, &[(0, ALC269)]);
    let sid = c.open(HP, F48, 1024, 256).unwrap();
    c.start(sid).unwrap();
    c.quiesce();
    assert_eq!(c.state(), State::Quiesced);
    assert!(!f.sd(OUT0).running());
    assert_eq!(f.reg32(INTCTL), 0);
    assert_eq!(f.reg32(GCTL) & GCTL_CRST, 0);
    assert!(f.has(&Ev::PciOff) && !f.bus_master());
    // The slot stays until the next bring-up, but only for winding down: no start, no write.
    assert_eq!(c.write(sid, &[0; 4]), Err(HdaError::NotUp));
    assert_eq!(c.start(sid), Err(HdaError::NotUp));
    assert!(c.position(sid).is_ok());
    assert_eq!(c.open(SPK, F48, 1024, 256), Err(HdaError::NotUp));
    // A stream left running (as a Reload leaves one) is stopped by the bring-up.
    c.bring_up().unwrap();
    f.force_run(OUT0);
    c.bring_up().unwrap();
    assert!(!f.sd(OUT0).running());
    assert_eq!(c.position(sid), Err(HdaError::NoSuchStream));
    assert!(c.open(HP, F48, 1024, 256).is_ok());
}

#[test]
fn shared_converter_is_busy() {
    let (_f, c) = up(Kind::Intel, &[(0, ALC269)]);
    let a = c.open(HP, F48, 1024, 256).unwrap();
    assert_eq!(c.open(SPK, F48, 1024, 256), Err(HdaError::Busy));
    c.close(a).unwrap();
    assert!(c.open(SPK, F48, 1024, 256).is_ok());
}

#[test]
fn bad_requests() {
    let (_f, c) = up(Kind::Intel, &[(0, ALC269)]);
    assert_eq!(c.open(OutputId { cad: 0, pin: 0x77 }, F48, 1024, 256), Err(HdaError::NoSuchOutput));
    assert_eq!(c.open(HP, PcmFormat { rate: 12345, channels: 2, bits: 16 }, 1024, 256), Err(HdaError::BadFormat));
    assert_eq!(c.open(HP, PcmFormat { rate: 88200, channels: 2, bits: 16 }, 1024, 256), Err(HdaError::BadFormat));
    assert_eq!(c.open(HP, F48, 0, 0), Err(HdaError::BadFormat));
    assert_eq!(c.write(0x1234, &[0; 4]), Err(HdaError::NoSuchStream));
    let sid = c.open(HP, F48, 1024, 256).unwrap();
    // Partial frames are not taken.
    assert_eq!(c.write(sid, &[0; 7]).unwrap(), 4);
    // A stale id (closed and reopened slot) is refused.
    c.close(sid).unwrap();
    let again = c.open(HP, F48, 1024, 256).unwrap();
    assert_ne!(again, sid);
    assert_eq!(c.write(sid, &[0; 4]), Err(HdaError::NoSuchStream));
}

#[test]
fn qemu_duplex_codec() {
    for mixer in [false, true] {
        let t = duplex_text(mixer);
        let (f, c) = up(Kind::Intel, &[(0, &t)]);
        let outs = c.outputs();
        assert_eq!(outs.len(), 1, "{outs:?}");
        assert_eq!((outs[0].id.pin, outs[0].kind, outs[0].converter), (3, OutputKind::LineOut, 2));
        assert!(outs[0].formats.contains(&PcmFormat { rate: 44100, channels: 2, bits: 16 }));
        f.clear_sets(0);
        let sid = c.open(outs[0].id, PcmFormat { rate: 44100, channels: 2, bits: 16 }, 2048, 512).unwrap();
        let sets = f.sets(0);
        assert!(sets.contains(&(2, 0x24011)), "{sets:?}");
        assert!(sets.contains(&(2, 0x70610)));
        assert_eq!(sets.contains(&(2, 0x3B04A)), mixer, "0 dB unmuted on the DAC's amp with the mixer");
        assert_eq!(f.sd(OUT0).fmt, 0x4011);
        c.write(sid, &signal::bytes(0, 2048, false)).unwrap();
        c.start(sid).unwrap();
        f.advance(8192);
        assert_eq!(f.sd(OUT0).sink, signal::bytes(0, 2048, false));
    }
}

#[test]
fn no_snoop_falls_back_without_uc() {
    let (f, c) = ctrl_cfg(Kind::Intel, &[(0, ALC269)], Config { force_no_snoop: true });
    assert!(!c.snooped());
    c.bring_up().unwrap();
    assert!(f.has(&Ev::Alloc(crate::link::RINGS_BYTES, false)));
    f.no_uc.store(true, Ordering::Release);
    let sid = c.open(HP, F48, 1024, 256).unwrap();
    assert!(c.snooped(), "fell back to snooping");
    // No-snoop descriptors carry the traffic-priority bit (Linux); after the fallback this one does not.
    assert_eq!(f.sd(OUT0).ctl & SD_CTL_TP, 0);
    c.close(sid).unwrap();
}

#[test]
fn two_controllers_play_side_by_side() {
    let (f0, c0) = up(Kind::Intel, &[(0, ALC269)]);
    let t = duplex_text(false);
    let (f1, c1) = up(Kind::Intel, &[(0, &t)]);
    let s0 = c0.open(HP, F48, 1024, 256).unwrap();
    let out1 = c1.outputs()[0].id;
    let s1 = c1.open(out1, F48, 1024, 256).unwrap();
    c0.write(s0, &signal::bytes(0, 1024, false)).unwrap();
    c1.write(s1, &signal::bytes(0, 1024, true)).unwrap();
    c0.start(s0).unwrap();
    c1.start(s1).unwrap();
    f0.advance(4096);
    f1.advance(4096);
    assert_eq!(f0.sd(OUT0).sink, signal::bytes(0, 1024, false));
    assert_eq!(f1.sd(OUT0).sink, signal::bytes(0, 1024, true));
}

#[test]
fn missing_codec_and_gone_controller() {
    let (_f, c) = ctrl(Kind::Intel, &[]);
    c.bring_up().unwrap();
    assert!(c.outputs().is_empty());
    assert_eq!(c.open(HP, F48, 1024, 256), Err(HdaError::NoSuchOutput));
    let _ = "x".to_string();
}

#[test]
fn bring_up_turns_bus_master_off_first() {
    let (f, c) = up(Kind::Intel, &[(0, ALC269)]);
    let sid = c.open(HP, F48, 1024, 256).unwrap();
    c.start(sid).unwrap();
    f.trace.lock().clear();
    c.bring_up().unwrap();
    let t = f.trace.lock().clone();
    let off = t.iter().position(|e| *e == Ev::PciOff).expect("pci_off on a restart");
    let on = t.iter().position(|e| matches!(e, Ev::Enable(true))).expect("enable");
    assert!(off < on);
}

#[test]
fn stuck_close_keeps_buffers_until_link_reset() {
    let (f, c) = up(Kind::Intel, &[(0, ALC269)]);
    let sid = c.open(HP, F48, 1024, 256).unwrap();
    c.start(sid).unwrap();
    let live = f.live_buffers();
    f.stuck.store(true, Ordering::Release);
    c.close(sid).unwrap();
    assert_eq!(c.kept_buffers(), 2, "BDL and ring of a descriptor that did not stop");
    assert_eq!(f.live_buffers(), live, "nothing freed under a running engine");
    // The converter is free again (its stream id was cleared under the slot lock).
    assert!(f.sets(0).iter().any(|&(nid, v)| nid == 0x02 && v == hdacodec::verb::set(hdacodec::verb::SET_CHANNEL_STREAMID, 0)));
    // A restart over stuck engines: they are only stopped by the link reset, after which everything goes back.
    c.bring_up().unwrap();
    assert_eq!(c.kept_buffers(), 0);
    assert!(!f.sd(OUT0).running());
    assert_eq!(f.live_buffers(), 1, "only the new command rings");
}

#[test]
fn stuck_rings_on_fallback_are_kept() {
    let (f, c) = ctrl(Kind::Intel, &[(0, ALC269)]);
    f.corb_dead.store(true, Ordering::Release);
    f.stuck.store(true, Ordering::Release);
    c.bring_up().unwrap();
    assert_eq!(c.link_mode(), Some(Mode::Immediate));
    assert_eq!(c.kept_buffers(), 1, "rings whose DMA did not stop");
    assert_eq!(f.live_buffers(), 1);
    f.corb_dead.store(false, Ordering::Release);
    f.stuck.store(false, Ordering::Release);
    c.bring_up().unwrap();
    assert_eq!(c.link_mode(), Some(Mode::Rings));
    assert_eq!(c.kept_buffers(), 0);
    assert_eq!(f.live_buffers(), 1);
}

#[test]
fn huge_ring_is_refused() {
    let (f, c) = up(Kind::Intel, &[(0, ALC269)]);
    let live = f.live_buffers();
    let frames = crate::stream::MAX_RING_BYTES / 4 + 1;
    assert_eq!(c.open(HP, F48, frames, 1024), Err(HdaError::NoMemory));
    assert_eq!(f.live_buffers(), live);
    assert!(c.open(HP, F48, crate::stream::MAX_RING_BYTES / 8, 4096).is_ok());
}
