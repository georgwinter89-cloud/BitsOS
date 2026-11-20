//! Ring reservation, padding, wrap, pointer forms; IB padding.

use std::vec;
use std::vec::Vec;

use crate::ring::{Kind, Ring, RingError, pad_ib};
use crate::{pm4, sdma};

const SDMA: Kind = Kind::Sdma { burst_nop: true };

#[test]
fn geometry_is_the_callers() {
    assert!(Ring::new(Kind::Gfx, 512, 0).is_ok());
    assert_eq!(Ring::new(Kind::Gfx, 256, 0).unwrap_err(), RingError::BadSize(256));
    assert_eq!(Ring::new(SDMA, 48, 0).unwrap_err(), RingError::BadSize(48));
    assert_eq!(Ring::new(SDMA, 64, 8).unwrap_err(), RingError::BadRptr(8));
    // Any power of two works, also a large one.
    assert!(Ring::new(SDMA, 1 << 24, 1 << 40).is_ok());
}

#[test]
fn pointer_forms() {
    assert_eq!(SDMA.hw_ptr(0x40), 0x100);
    assert_eq!(SDMA.ptr_from_hw(0x100), 0x40);
    assert_eq!(Kind::Gfx.hw_ptr(0x40), 0x40);
}

#[test]
fn commit_pads_to_alignment() {
    for (kind, a) in [(SDMA, 16usize), (Kind::Sdma { burst_nop: false }, 16), (Kind::Gfx, 256), (Kind::Vcn, 64)] {
        let mut r = Ring::new(kind, 4096, 0).unwrap();
        let mut mem = vec![0xEEEE_EEEEu32; 4096];
        r.begin(5, 0).unwrap();
        r.write(&[1, 2, 3]).unwrap();
        let w = r.commit(&mut mem).unwrap();
        assert_eq!(w, a as u64);
        assert_eq!(&mem[..3], &[1, 2, 3]);
        match kind {
            Kind::Gfx => assert!(mem[3..a].iter().all(|&d| d == pm4::PAD)),
            Kind::Sdma { burst_nop: true } => {
                assert_eq!(mem[3], ((a as u32 - 4) << 16));
                assert!(mem[4..a].iter().all(|&d| d == sdma::NOP));
                assert_eq!(sdma::packet_len(&mem[3..]), Ok(a - 3));
            }
            Kind::Sdma { .. } | Kind::Vcn => assert!(mem[3..a].iter().all(|&d| d == 0)),
        }
        assert_eq!(mem[a], 0xEEEE_EEEE);
    }
}

#[test]
fn reservation_rules() {
    let mut r = Ring::new(SDMA, 64, 0).unwrap();
    let mut mem = vec![0u32; 64];
    assert_eq!(r.begin(49, 0), Err(RingError::TooBig(49)));
    assert_eq!(r.begin(49, 0).unwrap_err().errno(), crate::errno::ENOMEM);
    r.begin(17, 0).unwrap();
    assert_eq!(r.begin(1, 0), Err(RingError::State));
    r.write(&[0; 32]).unwrap();
    assert_eq!(r.write(&[0]), Err(RingError::Overrun));
    r.commit(&mut mem).unwrap();
    assert_eq!(r.wptr(), 32);
    // 32 used of 64, one alignment unit always stays free: 16 left.
    assert_eq!(r.free_dw(0), Ok(16));
    assert_eq!(r.begin(17, 0), Err(RingError::Full { need: 32, free: 16 }));
    assert_eq!(r.begin(17, 0).unwrap_err().errno(), crate::errno::EBUSY);
    assert_eq!(r.free_dw(100), Err(RingError::BadRptr(100)));
    r.begin(16, 0).unwrap();
    r.undo();
    assert_eq!(r.commit(&mut mem), Err(RingError::State));
}

#[test]
fn commit_wraps() {
    let mut r = Ring::new(SDMA, 64, 48).unwrap();
    let mut mem = vec![0u32; 64];
    r.begin(32, 48).unwrap();
    let words: Vec<u32> = (1..=20).collect();
    r.write(&words).unwrap();
    assert_eq!(r.commit(&mut mem).unwrap(), 80);
    assert_eq!(&mem[48..64], &words[..16]);
    assert_eq!(&mem[..4], &words[16..]);
}

#[test]
fn short_sink_is_refused_and_nothing_moves() {
    let mut r = Ring::new(SDMA, 64, 0).unwrap();
    let mut short = vec![0u32; 63];
    r.begin(16, 0).unwrap();
    r.write(&[7; 16]).unwrap();
    let e = r.commit(&mut short).unwrap_err();
    assert_eq!(e, RingError::SinkTooSmall { have: 63, need: 64 });
    assert_eq!(e.errno(), crate::errno::EINVAL);
    assert_eq!(r.wptr(), 0);
    assert!(short.iter().all(|&d| d == 0));
    // The reservation stays open: the right sink takes it.
    let mut mem = vec![0u32; 64];
    assert_eq!(r.commit(&mut mem), Ok(16));
    assert_eq!(&mem[..16], &[7; 16]);
    let empty: &mut [u32] = &mut [];
    r.begin(1, 0).unwrap();
    assert_eq!(r.commit(empty), Err(RingError::SinkTooSmall { have: 0, need: 64 }));
}

#[test]
fn sdma_ib_packet_ends_on_8() {
    for start_fill in 0..8u64 {
        let mut r = Ring::new(SDMA, 256, 0).unwrap();
        r.begin(64, 0).unwrap();
        r.write(&vec![0; start_fill as usize]).unwrap();
        r.emit_ib(1, 0x1000, 8, 0, false).unwrap();
        assert_eq!(r.pos() % 8, 0, "start {start_fill}");
    }
}

#[test]
fn emit_errors_leave_the_frame() {
    let mut r = Ring::new(Kind::Gfx, 512, 0).unwrap();
    r.begin(16, 0).unwrap();
    assert!(matches!(r.emit_fence(0x1004, 1, true, false), Err(RingError::Encode(_))));
    assert_eq!(r.pos(), 0);
    let mut s = Ring::new(SDMA, 64, 0).unwrap();
    s.begin(16, 0).unwrap();
    assert!(s.emit_ib(0, 0x1010, 8, 0, false).is_err());
    assert_eq!(s.pos(), 0);
}

#[test]
fn ib_padding() {
    let mut ib = vec![1u32; 3];
    pad_ib(SDMA, &mut ib, 8).unwrap();
    assert_eq!(ib, [1, 1, 1, 4 << 16, 0, 0, 0, 0]);
    pad_ib(SDMA, &mut ib, 8).unwrap();
    assert_eq!(ib.len(), 8);
    let mut g = vec![1u32; 17];
    pad_ib(Kind::Gfx, &mut g, 256).unwrap();
    assert_eq!(g.len(), 256);
    assert!(pad_ib(SDMA, &mut g, 4).is_err());
    assert!(pad_ib(Kind::Gfx, &mut g, 12).is_err());
    // Long SDMA padding splits the burst NOP.
    let mut big = vec![1u32];
    pad_ib(SDMA, &mut big, 1 << 16).unwrap();
    assert_eq!(big.len(), 1 << 16);
    let mut i = 1;
    while i < big.len() {
        i += sdma::packet_len(&big[i..]).unwrap();
    }
    assert_eq!(i, big.len());
}

/// L7 row 47: GFX and compute rings pad as `gfx_v11_ring_insert_nop` (linux-7.3.1 gfx_v11_0.c:566): one counted NOP
/// over the whole pad (its body the filler), a pad of one is the filler alone; the decoder sees one packet.
#[test]
fn gfx_commit_pads_with_one_counted_nop() {
    let mut out = Vec::new();
    crate::ring::gfx_insert_nop(&mut out, 1);
    assert_eq!(out, [pm4::PAD]);
    for n in [2u64, 3, 255, 0x4000, 0x4001, 0x5000] {
        out.clear();
        crate::ring::gfx_insert_nop(&mut out, n);
        assert_eq!(out.len() as u64, n);
        let count = (n - 2).min(0x3ffe) as u32;
        assert_eq!(out[0], pm4::packet3(pm4::op::NOP, count), "{n}");
        assert!(out[1..].iter().all(|&d| d == pm4::PAD), "{n}");
    }
    let mut r = Ring::new(Kind::Gfx, 512, 0).unwrap().with_counted_nops();
    let mut sink = vec![0u32; 512];
    r.begin(4, 0).unwrap();
    r.write(&[1, 2, 3, 4]).unwrap();
    assert_eq!(r.commit(&mut sink[..]).unwrap(), 256);
    assert_eq!(&sink[..5], &[1, 2, 3, 4, pm4::packet3(pm4::op::NOP, 250)]);
    assert!(sink[5..256].iter().all(|&d| d == pm4::PAD));
    // Without it (KIQ, MES): single NOPs, as before; SDMA ignores the switch.
    let mut k = Ring::new(Kind::Gfx, 512, 0).unwrap();
    k.begin(1, 0).unwrap();
    k.write(&[9]).unwrap();
    k.commit(&mut sink[..]).unwrap();
    assert!(sink[1..256].iter().all(|&d| d == pm4::PAD));
    let s = Ring::new(SDMA, 64, 0).unwrap().with_counted_nops();
    let mut s2 = Ring::new(SDMA, 64, 0).unwrap();
    let (mut a, mut b) = (vec![0u32; 64], vec![0u32; 64]);
    for (ring, sink) in [(s, &mut a), (core::mem::replace(&mut s2, Ring::new(SDMA, 64, 0).unwrap()), &mut b)] {
        let mut ring = ring;
        ring.begin(1, 0).unwrap();
        ring.write(&[7]).unwrap();
        ring.commit(&mut sink[..]).unwrap();
    }
    assert_eq!(a, b);
}
