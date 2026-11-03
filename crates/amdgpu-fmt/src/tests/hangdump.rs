//! The hang-dump decoder.

use std::string::ToString;
use std::vec;
use std::vec::Vec;

use crate::hang::{LineKind, decode_ring, decode_stream};
use crate::ring::{Kind, Ring};
use crate::sim::Isa;
use crate::{pm4, sdma};

#[test]
fn gfx_ring_dump_marks_rptr() {
    let mut ring = Ring::new(Kind::Gfx, 512, 0).unwrap();
    let mut mem = vec![0u32; 512];
    ring.begin(32, 0).unwrap();
    ring.emit_ib(1, 0x1_0000_1000, 16, 0, false).unwrap();
    ring.emit_fence(0x3000, 1, true, true).unwrap();
    let w = ring.commit(&mut mem).unwrap();
    let d = decode_ring(Isa::Pm4, &mem, 0, 4, w);
    assert_eq!(d.lines[0].text, "INDIRECT_BUFFER addr=0x100001000 len=16 vmid=1 control=0x01000010");
    assert!(d.lines[1].text.starts_with("RELEASE_MEM event=0x14 index=5 cntl=0x0671f514 data_sel=2 int_sel=2"));
    assert!(d.lines[1].at_rptr && !d.lines[0].at_rptr && !d.rptr_outside);
    // Padding: one line per 0xffff1000.
    assert_eq!(d.lines.len(), 2 + (256 - 12));
    let text = d.to_string();
    assert!(text.contains("=> 0x000004 [   8] c0064900  RELEASE_MEM"));
}

#[test]
fn sdma_dump_resyncs_after_garbage_and_wraps() {
    let mut words = Vec::new();
    sdma::fence(&mut words, 0x40, 3).unwrap();
    words.push(0x0000_0077); // unknown op
    sdma::trap(&mut words, 5).unwrap();
    sdma::write_linear(&mut words, 0x100, &[1, 2, 3, 4, 5]).unwrap();
    // Place at the end of a 16-dword ring so the stream wraps.
    let mut ring = vec![0u32; 16];
    for (i, &w) in words.iter().enumerate() {
        ring[(10 + i) % 16] = w;
    }
    let d = decode_ring(Isa::Sdma, &ring, 10, 15, 10 + words.len() as u64);
    let kinds: Vec<_> = d.lines.iter().map(|l| l.kind).collect();
    assert_eq!(kinds, [LineKind::Packet, LineKind::Raw, LineKind::Packet, LineKind::Packet]);
    assert_eq!(d.lines[1].text, "?? op=119 sub=0");
    assert_eq!(d.lines[2].text, "TRAP ctx=0x5");
    assert!(d.lines[3].text.starts_with("WRITE_LINEAR addr=0x100 n=5 00000001"));
    assert!(d.lines[2].at_rptr);
}

#[test]
fn truncated_and_window_limits() {
    let l = decode_stream(Isa::Pm4, &[pm4::packet3(pm4::op::WRITE_DATA, 10), 0, 0], 0);
    assert_eq!(l[0].kind, LineKind::Truncated);
    assert_eq!(l[0].text, "WRITE_DATA TRUNCATED need=12 have=3");
    let l = decode_stream(Isa::Pm4, &[0x4000_0000, pm4::PAD], 0);
    assert_eq!((l[0].kind, l[1].kind), (LineKind::Raw, LineKind::Packet));
    // A window longer than the ring is cut; a ring that is not a power of two gives nothing.
    let d = decode_ring(Isa::Sdma, &[0; 16], 0, 100, 1000);
    assert_eq!(d.from, 1000 - 16);
    assert_eq!(d.lines.len(), 16);
    assert!(d.rptr_outside);
    assert!(decode_ring(Isa::Sdma, &[0; 12], 0, 0, 4).lines.is_empty());
}
