//! SDMA 7.0's two packet forms (wave 43, GX-10's change request): what Linux emits, the strict decoder, and the
//! engine that runs them and halts on what Linux would not send.

use std::vec;
use std::vec::Vec;

use super::limits;
use crate::bus::SimMem;
use crate::ring::{Kind, Ring};
use crate::sdma::{self, DecodeError, Packet};
use crate::sdma7;
use crate::sim::{Engine, Hang, Isa, Stop};

const RING: u64 = 0x10_0000;
const BUF: u64 = 0x80_0000;

#[test]
fn the_copy_and_the_fill_are_linuxs_dwords() {
    // sdma_v7_0_emit_copy_buffer (sdma_v7_0.c:1753-1785) with no copy flags.
    let mut v = Vec::new();
    sdma7::copy_linear(&mut v, 0x80_1234_5000, 0x2_0000, 8192).unwrap();
    assert_eq!(v, [0x0008_0001, 8191, 0, 0x1234_5000, 0x80, 0x2_0000, 0, 0]);
    // sdma_v7_0_emit_fill_buffer (:1797-1808).
    let mut f = Vec::new();
    sdma7::const_fill(&mut f, 0x80_0000_1000, 0xA5, 4096).unwrap();
    assert_eq!(f, [0x0001_000B, 0x1000, 0x80, 0xA5, 4095]);
    assert!(sdma7::copy_linear(&mut v, 0, 0, 0).is_err() && sdma7::const_fill(&mut f, 0, 0, (1 << 30) + 1).is_err());
    assert_eq!((v.len(), f.len()), (8, 5), "a refused packet appends nothing");
}

#[test]
fn the_decoder_takes_a_copy_only_as_linux_sends_it() {
    let mut v = Vec::new();
    sdma7::copy_linear(&mut v, 0x1000, 0x2000, 64).unwrap();
    v.push(sdma::NOP);
    assert_eq!(sdma7::packet_len(&v), Ok(8));
    let (p, n, dcc) = sdma7::decode(&v).unwrap();
    assert!(matches!(p, Packet::CopyLinear { src: 0x1000, dst: 0x2000, bytes: 64, .. }) && n == 8 && dcc == Some(0));
    // 6.0's decoder would take the DCC dword for a NOP: seven dwords, then one more packet.
    assert_eq!(sdma::packet_len(&v), Ok(7));
    // Without CPV the length is not known: it does not decode.
    let mut old = Vec::new();
    sdma::copy_linear(&mut old, 0x1000, 0x2000, 64, false).unwrap();
    assert_eq!(sdma7::packet_len(&old), Err(DecodeError::Unknown { header: old[0] }));
    assert!(matches!(sdma7::decode(&v[..7]), Err(DecodeError::Truncated { need: 8, have: 7 })));
    // Every other packet is 6.0's.
    let mut w = Vec::new();
    sdma::write_linear(&mut w, 0x4000, &[1, 2]).unwrap();
    assert_eq!(sdma7::decode(&w).map(|(p, n, d)| (p, n, d)), sdma::decode(&w).map(|(p, n)| (p, n, None)));
}

struct Rig {
    mem: SimMem,
    ring: Ring,
    eng: Engine,
    shadow: Vec<u32>,
}

fn rig() -> Rig {
    let mut mem = SimMem::new();
    mem.map(0, RING, 256 * 4).unwrap();
    mem.map(0, BUF, 0x1_0000).unwrap();
    let mut eng = Engine::new(Isa::Sdma, RING, 256, 0, limits()).unwrap();
    eng.speak_sdma7();
    Rig { mem, ring: Ring::new(Kind::Sdma { burst_nop: true }, 256, 0).unwrap(), eng, shadow: vec![0; 256] }
}

impl Rig {
    fn run(&mut self, words: &[u32]) -> crate::sim::Report {
        self.ring.begin(words.len() as u64, self.eng.rptr()).unwrap();
        self.ring.write(words).unwrap();
        let w = self.ring.commit(&mut self.shadow).unwrap();
        self.mem.poke_dwords(0, RING, &self.shadow).unwrap();
        self.eng.run(&mut self.mem, w)
    }
}

#[test]
fn the_engine_runs_linuxs_fill_and_copy() {
    let mut r = rig();
    let mut v = Vec::new();
    sdma7::const_fill(&mut v, BUF, 0xA5, 4096).unwrap();
    sdma7::copy_linear(&mut v, BUF, BUF + 0x2000, 4096).unwrap();
    sdma::emit_fence(&mut v, BUF + 0x4000, 7, false, true).unwrap();
    assert_eq!(r.run(&v).stop, Stop::Idle);
    let mut b = [0u8; 4096];
    r.mem.peek(0, BUF + 0x2000, &mut b).unwrap();
    assert!(b.iter().all(|x| *x == 0xA5), "the fill's bytes arrived through the copy");
    assert_eq!((r.mem.peek_u32(0, BUF + 0x3000), r.mem.peek_u32(0, BUF + 0x4000)), (Some(0), Some(7)));
}

#[test]
fn the_engine_halts_on_what_linux_does_not_send() {
    // A 6.0 copy (no CPV).
    let mut r = rig();
    let mut v = Vec::new();
    sdma::copy_linear(&mut v, BUF, BUF + 0x1000, 64, false).unwrap();
    let rep = r.run(&v);
    assert!(matches!(rep.stop, Stop::Hang(Hang::Sdma(DecodeError::Unknown { .. }))), "{:?}", rep.stop);
    assert_eq!((r.eng.rptr(), rep.at.rptr), (0, 0), "the read pointer stays at the packet");
    // A fill without COMPRESS.
    let mut r = rig();
    let mut v = Vec::new();
    sdma::const_fill(&mut v, BUF, 1, 64, sdma::FILL_BYTE).unwrap();
    assert!(matches!(r.run(&v).stop, Stop::Hang(Hang::BadField { what: "CONST_FILL without COMPRESS", .. })));
    assert_eq!(r.mem.peek_u32(0, BUF), Some(0), "nothing was filled");
    // A compressed copy (a DCC dword).
    let mut r = rig();
    let mut v = Vec::new();
    sdma7::copy_linear(&mut v, BUF, BUF + 0x1000, 64).unwrap();
    v[7] = 1;
    assert!(matches!(r.run(&v).stop, Stop::Hang(Hang::Unsupported { .. })));
    // 6.0's engine is unchanged: it still takes its own seven-dword copy.
    let mut mem = SimMem::new();
    mem.map(0, RING, 64 * 4).unwrap();
    mem.map(0, BUF, 0x2000).unwrap();
    let mut six = Engine::new(Isa::Sdma, RING, 64, 0, limits()).unwrap();
    let mut v = Vec::new();
    sdma::copy_linear(&mut v, BUF, BUF + 0x1000, 64, false).unwrap();
    v.resize(16, sdma::NOP);
    mem.poke_dwords(0, RING, &v).unwrap();
    assert_eq!(six.run(&mut mem, 16).stop, Stop::Idle);
}
