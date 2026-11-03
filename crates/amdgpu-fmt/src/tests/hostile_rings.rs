//! Garbage rings and IBs: decoders and interpreters never panic, every run ends within its limits.

use std::vec::Vec;

use super::Rng;
use crate::bus::SimMem;
use crate::hang::{decode_ring, decode_stream};
use crate::sim::{Engine, Isa, Limits, Stop};
use crate::{pm4, sdma};

const RING: u64 = 0x10_0000;
const RING_DW: u64 = 256;

/// Dwords that look like packets often enough to reach deep paths: real headers with random fields,
/// addresses into the mapped window, small counts.
fn word(rng: &mut Rng, isa: Isa) -> u32 {
    match rng.below(6) {
        0 => rng.next() as u32,
        1 => rng.below(16) as u32,
        2 => 0x0080_0000 + (rng.below(0x4000) as u32 & !3),
        3 => match isa {
            Isa::Pm4 => {
                let ops = [0x10u8, 0x37, 0x49, 0x3F, 0x33, 0x22, 0x3C, 0x79, 0x58, 0x50];
                pm4::packet3(ops[rng.below(ops.len() as u64) as usize], rng.below(12) as u32)
            }
            Isa::Sdma => {
                let ops = [0u8, 1, 2, 4, 5, 6, 8, 9, 11, 12, 13, 14, 17, 3];
                sdma::header(ops[rng.below(ops.len() as u64) as usize], 0) | ((rng.below(4) as u32) << 16)
            }
        },
        4 => pm4::PAD,
        _ => rng.below(64) as u32,
    }
}

fn hostile_mem(rng: &mut Rng, isa: Isa) -> SimMem {
    let mut m = SimMem::new();
    m.map(0, RING, RING_DW as usize * 4).unwrap();
    m.map(0, 0x80_0000, 0x4000).unwrap();
    let ring: Vec<u32> = (0..RING_DW).map(|_| word(rng, isa)).collect();
    m.poke_dwords(0, RING, &ring).unwrap();
    let ib: Vec<u32> = (0..0x1000).map(|_| word(rng, isa)).collect();
    m.poke_dwords(0, 0x80_0000, &ib).unwrap();
    if rng.chance(50) {
        m.set_reg(rng.below(16) as u32, rng.next() as u32);
    }
    m
}

#[test]
fn interpreters_survive_garbage() {
    let mut rng = Rng::new(0x6A52);
    let lim = Limits { max_dw: 4096, max_bytes: 1 << 16, max_ib_depth: 2 };
    for round in 0..3000 {
        let isa = if round % 2 == 0 { Isa::Pm4 } else { Isa::Sdma };
        let mut m = hostile_mem(&mut rng, isa);
        let start = rng.next();
        let mut e = Engine::new(isa, RING, RING_DW, start, lim).unwrap();
        let wptr = start.wrapping_add(rng.below(RING_DW + 4));
        for _ in 0..4 {
            let rep = e.run(&mut m, wptr);
            assert!(rep.fetched_dw <= lim.max_dw);
            if matches!(rep.stop, Stop::Idle | Stop::Hang(_) | Stop::Incomplete { .. }) {
                break;
            }
        }
        // Whatever happened, the dump decodes.
        let mut snap = std::vec![0u32; RING_DW as usize];
        for (i, s) in snap.iter_mut().enumerate() {
            *s = m.peek_u32(0, RING + 4 * i as u64).unwrap();
        }
        let d = decode_ring(isa, &snap, start, e.rptr(), wptr);
        assert!(d.lines.iter().map(|l| l.len).sum::<u64>() <= RING_DW);
        let _ = std::format!("{d}");
    }
}

/// A random valid packet (addresses inside or just outside the mapped window).
fn packet(rng: &mut Rng, isa: Isa, out: &mut Vec<u32>) {
    let addr = 0x80_0000 + (rng.below(0x4400) & !7);
    let n = 1 + rng.below(6) as usize;
    let data: Vec<u32> = (0..n).map(|_| rng.next() as u32).collect();
    let _ = match (isa, rng.below(8)) {
        (Isa::Pm4, 0) => pm4::write_data(out, pm4::dst_sel::MEM, addr, 0, &data),
        (Isa::Pm4, 1) => pm4::fence(out, addr, rng.next(), rng.chance(50), rng.chance(50)),
        (Isa::Pm4, 2) => pm4::indirect_buffer(out, addr & !0xFF, rng.below(64) as u32, 0, false, 0),
        (Isa::Pm4, 3) => pm4::cond_exec(out, addr, rng.below(8) as u32),
        (Isa::Pm4, 4) => pm4::set_reg(out, pm4::RegSpace::Uconfig, 0xc000 + rng.below(8) as u32, &data),
        (Isa::Pm4, 5) => {
            let w = pm4::WaitRegMem {
                function: rng.below(7) as u8,
                mem: true,
                operation: 0,
                engine: 0,
                addr0: addr as u32,
                addr1: 0,
                reference: rng.below(2) as u32,
                mask: 1,
                interval: 4,
            };
            pm4::wait_reg_mem(out, &w)
        }
        (Isa::Pm4, _) => pm4::nop(out, &data),
        (Isa::Sdma, 0) => sdma::write_linear(out, addr, &data),
        (Isa::Sdma, 1) => sdma::emit_fence(out, addr, rng.next(), rng.chance(50), rng.chance(50)),
        (Isa::Sdma, 2) => sdma::indirect(out, 0, addr & !0xFF, rng.below(64) as u32, 0),
        (Isa::Sdma, 3) => sdma::copy_linear(out, addr, addr ^ 0x2000, 1 + rng.below(0x3000), false),
        (Isa::Sdma, 4) => sdma::const_fill(out, addr, data[0], 4 * (1 + rng.below(0x800)), sdma::FILL_DWORD),
        (Isa::Sdma, 5) => sdma::cond_exe(out, addr, rng.below(2) as u32, rng.below(8) as u32),
        (Isa::Sdma, 6) => sdma::srbm_write(out, rng.below(64) as u32, data[0]),
        (Isa::Sdma, _) => sdma::nops(out, n as u32, rng.chance(50)),
    };
}

/// Valid streams with a few dwords flipped: reaches the deep paths (IBs, polls, copies) far more often.
#[test]
fn mutated_streams_stay_bounded() {
    let mut rng = Rng::new(0x3A7);
    let lim = Limits { max_dw: 2048, max_bytes: 1 << 15, max_ib_depth: 2 };
    let mut events = 0usize;
    for round in 0..2000 {
        let isa = if round % 2 == 0 { Isa::Pm4 } else { Isa::Sdma };
        let mut m = hostile_mem(&mut rng, isa);
        let mut ring = Vec::new();
        while ring.len() < RING_DW as usize - 16 {
            packet(&mut rng, isa, &mut ring);
        }
        ring.truncate(RING_DW as usize);
        let mut ib = Vec::new();
        while ib.len() < 0x400 {
            packet(&mut rng, isa, &mut ib);
        }
        for _ in 0..rng.below(4) {
            let i = rng.below(ring.len() as u64) as usize;
            ring[i] ^= 1 << rng.below(32);
        }
        m.poke_dwords(0, RING, &ring).unwrap();
        m.poke_dwords(0, 0x80_0000, &ib[..0x400]).unwrap();
        let mut e = Engine::new(isa, RING, RING_DW, 0, lim).unwrap();
        for _ in 0..4 {
            let rep = e.run(&mut m, ring.len() as u64);
            assert!(rep.fetched_dw <= lim.max_dw);
            events += rep.events.len();
            if !matches!(rep.stop, Stop::Budget) {
                break;
            }
        }
    }
    assert!(events > 4_000, "the stream reaches real work ({events} events)");
}

#[test]
fn decoders_survive_random_words() {
    let mut rng = Rng::new(0xDEC0);
    for _ in 0..3000 {
        let n = rng.below(40) as usize;
        let w: Vec<u32> = (0..n).map(|_| word(&mut rng, Isa::Pm4)).collect();
        let _ = pm4::decode(&w);
        let lines = decode_stream(Isa::Pm4, &w, 0);
        assert_eq!(lines.iter().map(|l| l.len).sum::<u64>(), n as u64);
        let w: Vec<u32> = (0..n).map(|_| word(&mut rng, Isa::Sdma)).collect();
        let _ = sdma::decode(&w);
        let lines = decode_stream(Isa::Sdma, &w, 0);
        assert_eq!(lines.iter().map(|l| l.len).sum::<u64>(), n as u64);
    }
}

/// A count field claiming the largest packets: never read past the end, never allocate by the claim.
#[test]
fn huge_claims_are_bounded() {
    let lim = Limits { max_dw: 1 << 12, max_bytes: 1 << 12, max_ib_depth: 2 };
    for (isa, words) in [
        (Isa::Pm4, std::vec![pm4::packet3(pm4::op::NOP, 0x3FFE)]),
        (Isa::Sdma, std::vec![sdma::header(sdma::op::WRITE, 0), 0x80_0000, 0, 0xF_FFFF]),
        (Isa::Sdma, std::vec![sdma::header(sdma::op::COPY, 0), 0x3FFF_FFFF, 0, 0x80_0000, 0, 0x80_1000, 0]),
        (Isa::Pm4, std::vec![pm4::packet3(pm4::op::INDIRECT_BUFFER, 2), 0x80_0000, 0, 0xF_FFFF]),
    ] {
        let mut m = SimMem::new();
        m.map(0, RING, RING_DW as usize * 4).unwrap();
        m.map(0, 0x80_0000, 0x4000).unwrap();
        m.poke_dwords(0, RING, &words).unwrap();
        let mut e = Engine::new(isa, RING, RING_DW, 0, lim).unwrap();
        let mut stops = Vec::new();
        for _ in 0..8 {
            let rep = e.run(&mut m, words.len() as u64);
            assert!(rep.fetched_dw <= lim.max_dw);
            stops.push(rep.stop);
            if !matches!(rep.stop, Stop::Budget) {
                break;
            }
        }
        assert!(!matches!(stops.last(), Some(Stop::Budget)), "{isa:?} {stops:?}");
    }
}
