//! Golden test: the raw IBs the Linux capture of the real 780M submitted (run gx-6d9cae1d, bundle/ibtest.out,
//! 27.10.2026; family 148, external_rev 0xd) encode byte for byte, and the interpreters produce what Linux
//! saw on the chip: SDMA wrote deadbeef 0badf00d 12345678 9abcdef0, GFX's RELEASE_MEM wrote the fence
//! 0x1122334455667788, both submissions signalled sequence number 1.

use std::vec::Vec;

use super::limits;
use crate::bus::SimMem;
use crate::ring::{Kind, Ring, SDMA_IB_ALIGN_DW, pad_ib};
use crate::sim::{Engine, Event, IntSrc, Isa, Stop};
use crate::{pm4, sdma};

const SDMA_IB_VA: u64 = 0x1_0000_0000;
const SDMA_TARGET: u64 = 0x1_0000_2000;
const GFX_IB_VA: u64 = 0x1_0000_1000;
const GFX_TARGET: u64 = 0x1_0000_3000;

/// The SDMA IB as captured (8 dwords).
const SDMA_IB: [u32; 8] = [0x00000002, 0x00002000, 0x00000001, 0x00000003, 0xdeadbeef, 0x0badf00d, 0x12345678, 0x9abcdef0];
/// The GFX IB as captured (16 dwords).
const GFX_IB: [u32; 16] = [
    0xc0011000, 0x1b175000, 0x1b175001, 0xc0064900, 0x0671f514, 0x40000000, 0x00003000, 0x00000001, 0x55667788,
    0x11223344, 0x00000000, 0xffff1000, 0xffff1000, 0xffff1000, 0xffff1000, 0xffff1000,
];
const PATTERN: [u32; 4] = [0xdeadbeef, 0x0badf00d, 0x12345678, 0x9abcdef0];
const FENCE: u64 = 0x1122334455667788;
/// The user VM the harness ran in (any non-zero VMID).
const VMID: u8 = 3;

fn sdma_ib() -> Vec<u32> {
    let mut ib = Vec::new();
    sdma::write_linear(&mut ib, SDMA_TARGET, &PATTERN).unwrap();
    pad_ib(Kind::Sdma { burst_nop: true }, &mut ib, SDMA_IB_ALIGN_DW).unwrap();
    ib
}

fn gfx_ib() -> Vec<u32> {
    let mut ib = Vec::new();
    pm4::nop(&mut ib, &[0x1b175000, 0x1b175001]).unwrap();
    pm4::fence(&mut ib, GFX_TARGET, FENCE, true, false).unwrap();
    // The harness padded its IB to 16 dwords with the GFX ring NOP.
    pad_ib(Kind::Gfx, &mut ib, 16).unwrap();
    ib
}

#[test]
fn sdma_ib_encodes_byte_for_byte() {
    assert_eq!(sdma_ib(), SDMA_IB);
}

#[test]
fn gfx_ib_encodes_byte_for_byte() {
    assert_eq!(gfx_ib(), GFX_IB);
    // The fence's flag dword is Linux's gfx_v11 emit_fence set, field by field (nvd.h:166-185).
    assert_eq!(pm4::rm::GFX11_FENCE, 0x0671f514);
    assert_eq!(GFX_IB[5] >> 29, 2, "DATA_SEL 2: 64-bit");
    assert_eq!((GFX_IB[5] >> 24) & 7, 0, "INT_SEL 0");
}

fn mem() -> SimMem {
    let mut m = SimMem::new();
    m.map(VMID, SDMA_IB_VA, 0x4000).unwrap();
    m.poke_dwords(VMID, SDMA_IB_VA, &SDMA_IB).unwrap();
    m.poke_dwords(VMID, GFX_IB_VA, &GFX_IB).unwrap();
    m
}

#[test]
fn sdma_ib_runs_like_the_chip() {
    let mut m = mem();
    let mut e = Engine::new(Isa::Sdma, 0x10_0000, 64, 0, limits()).unwrap();
    let r = e.exec_ib(&mut m, VMID, SDMA_IB_VA, SDMA_IB.len() as u64);
    assert_eq!(r.stop, Stop::Idle);
    for (i, &want) in PATTERN.iter().enumerate() {
        assert_eq!(m.peek_u32(VMID, SDMA_TARGET + 4 * i as u64), Some(want));
    }
    assert_eq!(r.events, [Event::MemWrite { vmid: VMID, addr: SDMA_TARGET, bytes: 16 }]);
}

#[test]
fn gfx_ib_runs_like_the_chip() {
    let mut m = mem();
    let mut e = Engine::new(Isa::Pm4, 0x10_0000, 256, 0, limits()).unwrap();
    let r = e.exec_ib(&mut m, VMID, GFX_IB_VA, GFX_IB.len() as u64);
    assert_eq!(r.stop, Stop::Idle);
    assert_eq!(m.peek_u64(VMID, GFX_TARGET), Some(FENCE));
    assert_eq!(r.events, [Event::Fence { vmid: VMID, addr: GFX_TARGET, value: FENCE, wide: true }]);
    // 0xffff1000 padding is one dword per slot, never a 16384-dword body.
    assert_eq!(r.fetched_dw, 16);
}

/// The whole submission as Linux's CS path does it: IB packet and a fence with seq 1 on the kernel ring.
fn submit(kind: Kind, isa: Isa, ib_va: u64, ib_len: u32) -> (SimMem, Engine, u64, crate::sim::Report) {
    const RING: u64 = 0x20_0000;
    const WB: u64 = 0x30_0000;
    let size = 1024;
    let mut m = mem();
    m.map(0, RING, size as usize * 4).unwrap();
    m.map(0, WB, 4096).unwrap();
    let mut ring = Ring::new(kind, size, 0).unwrap();
    let mut e = Engine::new(isa, RING, size, 0, limits()).unwrap();
    ring.begin(64, e.rptr()).unwrap();
    ring.emit_ib(VMID, ib_va, ib_len, 0, false).unwrap();
    ring.emit_fence(WB, 1, true, true).unwrap();
    let mut shadow = std::vec![0u32; size as usize];
    let wptr = ring.commit(&mut shadow).unwrap();
    m.poke_dwords(0, RING, &shadow).unwrap();
    let r = e.run(&mut m, wptr);
    (m, e, WB, r)
}

#[test]
fn sdma_submission_signals_seq_1() {
    let (m, e, wb, r) = submit(Kind::Sdma { burst_nop: true }, Isa::Sdma, SDMA_IB_VA, 8);
    assert_eq!(r.stop, Stop::Idle);
    assert_eq!(m.peek_u64(0, wb), Some(1));
    assert_eq!(m.peek_u32(VMID, SDMA_TARGET), Some(0xdeadbeef));
    // Order: the IB's write, then the two fence halves, then the trap.
    let kinds: Vec<_> = r.events.iter().filter(|e| !matches!(e, Event::Ib { .. })).collect();
    assert!(matches!(kinds[0], Event::MemWrite { .. }));
    assert_eq!(kinds[1], &Event::Fence { vmid: 0, addr: wb, value: 1, wide: false });
    assert_eq!(kinds[2], &Event::Fence { vmid: 0, addr: wb + 4, value: 0, wide: false });
    assert_eq!(kinds[3], &Event::Interrupt { src: IntSrc::Trap, ctx: 0 });
    assert_eq!(e.rptr() % 16, 0);
}

#[test]
fn gfx_submission_signals_seq_1() {
    let (m, e, wb, r) = submit(Kind::Gfx, Isa::Pm4, GFX_IB_VA, 16);
    assert_eq!(r.stop, Stop::Idle);
    assert_eq!(m.peek_u64(0, wb), Some(1));
    assert_eq!(m.peek_u64(VMID, GFX_TARGET), Some(FENCE));
    let n = r.events.len();
    assert_eq!(r.events[n - 2], Event::Fence { vmid: 0, addr: wb, value: 1, wide: true });
    assert_eq!(r.events[n - 1], Event::Interrupt { src: IntSrc::EndOfPipe, ctx: 0 });
    assert_eq!(e.rptr(), 256, "GFX ring frames pad to 256 dwords");
}
