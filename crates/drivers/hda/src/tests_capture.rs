//! SN-9 host tests: input streams on the register-level fake (QEMU hda-duplex line in, ALC269 mic jack).

extern crate std;

use crate::ctrl::{HdaError, Kind, OutputId};
use crate::tests::{duplex_text, up};
use alloc::vec::Vec;
use devmodel::audio::PcmFormat;
use hdacodec::input::InputKind;

const F48: PcmFormat = PcmFormat { rate: 48000, channels: 2, bits: 16 };
const ALC269: &str = include_str!("../../../hdacodec/fixtures/alc269-synthetic.txt");
/// QEMU's intel-hda: input descriptor 0 is the first.
const IN0: usize = 0;
const LINE_IN: OutputId = OutputId { cad: 0, pin: 5 };

fn pattern(n: usize) -> Vec<u8> {
    (0..n).map(|i| (i * 7 + 3) as u8).collect()
}

#[test]
fn duplex_line_in_records_what_the_codec_sends() {
    let t = duplex_text(false);
    let (f, c) = up(Kind::Intel, &[(0, &t)]);
    let ins = c.inputs();
    assert_eq!(ins.len(), 1, "{ins:?}");
    assert_eq!((ins[0].id, ins[0].kind, ins[0].converter, ins[0].sense), (LINE_IN, InputKind::LineIn, 4, false));
    f.clear_sets(0);
    let sid = c.open_input(LINE_IN, F48, 1024, 256).unwrap();
    let sets = f.sets(0);
    // ADC 4: format, stream tag 1 (inputs have their own tags); the line-in pin on for input without VREF.
    assert!(sets.contains(&(4, 0x20011)) && sets.contains(&(4, 0x70610)), "{sets:?}");
    assert!(sets.contains(&(5, 0x70720)), "{sets:?}");
    assert_eq!(f.sd(IN0).tag(), 1);
    let src = pattern(4096 * 2);
    f.set_source(IN0, src.clone());
    assert_eq!(c.read(sid, &mut [0u8; 64]).unwrap(), 0, "nothing before start");
    c.start(sid).unwrap();
    f.advance(1000);
    assert_eq!(c.readable(sid).unwrap(), 250);
    let mut got = Vec::new();
    let mut buf = [0u8; 999];
    let n = c.read(sid, &mut buf).unwrap();
    assert_eq!(n, 996, "whole frames only");
    got.extend_from_slice(&buf[..n]);
    // Across the ring's wrap (4096 bytes).
    for _ in 0..6 {
        f.advance(1000);
        loop {
            let n = c.read(sid, &mut buf).unwrap();
            if n == 0 {
                break;
            }
            got.extend_from_slice(&buf[..n]);
        }
    }
    assert_eq!(got, src[..7000]);
    assert_eq!(c.overruns(sid).unwrap(), 0);
    c.close(sid).unwrap();
    assert!(!f.sd(IN0).running());
}

#[test]
fn a_reader_a_ring_behind_overruns() {
    let t = duplex_text(false);
    let (f, c) = up(Kind::Intel, &[(0, &t)]);
    let sid = c.open_input(LINE_IN, F48, 1024, 256).unwrap();
    f.set_source(IN0, pattern(20000));
    c.start(sid).unwrap();
    f.advance(3000);
    assert_eq!(c.readable(sid).unwrap(), 750);
    f.advance(1000);
    // 4000 bytes behind in a 4096-byte ring whose last period the DMA may be writing: lost, counted, reading
    // goes on from the DMA position.
    assert_eq!(c.readable(sid).unwrap(), 0);
    assert_eq!(c.overruns(sid).unwrap(), 1);
    f.advance(400);
    let mut buf = [0u8; 400];
    assert_eq!(c.read(sid, &mut buf).unwrap(), 400);
    assert_eq!(&buf[..], &pattern(20000)[4000..4400]);
}

#[test]
fn outputs_and_inputs_keep_their_own_slots_and_tags() {
    let (f, c) = up(Kind::Intel, &[(0, ALC269)]);
    let mic = c.inputs().into_iter().find(|i| i.id.pin == 0x18).expect("mic jack");
    assert_eq!((mic.kind, mic.converter, mic.sense), (InputKind::Mic, 0x08, true));
    let out = c.open(OutputId { cad: 0, pin: 0x15 }, F48, 1024, 256).unwrap();
    let inp = c.open_input(mic.id, F48, 1024, 256).unwrap();
    assert_eq!(f.sd(4).tag(), 1);
    assert_eq!(f.sd(IN0).tag(), 1);
    assert_eq!(c.open_input(mic.id, F48, 1024, 256), Err(HdaError::Busy), "one stream per ADC");
    assert_eq!(c.read(out, &mut [0u8; 4]), Err(HdaError::NoSuchStream), "an output is not read");
    assert_eq!(c.input_plugged(mic.id), Some(false));
    c.close(inp).unwrap();
    c.close(out).unwrap();
    // The ADC is taken off its stream at close.
    assert!(f.sets(0).contains(&(0x08, 0x70600)));
}
