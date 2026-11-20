//! GFX 12's packets (wave 44, GX-10's change request for item CP): what Linux's gfx_v12_0.c emits, hand
//! transcribed from its emitters, and the engine that runs them and halts at what Linux never sends there.

use std::vec;
use std::vec::Vec;

use super::limits;
use crate::bus::SimMem;
use crate::gfx12;
use crate::pm4;
use crate::sim::{Engine, Event, Hang, Isa, Stop};

const RING: u64 = 0x10_0000;
const BUF: u64 = 0x80_0000;
const SCRATCH: u32 = 0xC040;

#[test]
fn the_fence_ib_and_tests_are_linuxs_dwords() {
    // gfx_v12_0_ring_emit_fence :4567-4596, 32 bits with an interrupt, 64 bits without.
    let mut f = Vec::new();
    gfx12::fence(&mut f, 0x1_2345_6780, 0x1_0000_0007, false, true).unwrap();
    assert_eq!(f, [0xC006_4900, 0x0660_0514, (1 << 29) | (2 << 24), 0x2345_6780, 1, 7, 1, 0]);
    assert_eq!(gfx12::FENCE, 0x0660_0514);
    assert_ne!(gfx12::FENCE, pm4::rm::GFX11_FENCE);
    let mut w = Vec::new();
    gfx12::fence(&mut w, 0x1000, 9, true, false).unwrap();
    assert_eq!(w[2], 2 << 29);
    assert!(gfx12::fence(&mut w, 0x1004, 9, true, false).is_err(), "64 bits want 8-byte alignment (Linux's WARN_ON)");
    // emit_ib_gfx :4525-4545 and emit_ib_compute :4547-4565.
    let mut ib = Vec::new();
    gfx12::ib_gfx(&mut ib, 0x8_0000_1000, 16, 3).unwrap();
    gfx12::ib_compute(&mut ib, 0x8_0000_1000, 16, 0).unwrap();
    assert_eq!(ib, [0xC002_3F00, 0x1000, 8, 16 | (3 << 24), 0xC002_3F00, 0x1000, 8, 16 | (1 << 23)]);
    // ring test :468-473: SET_UCONFIG_REG count 1, the register less 0xc000, the value.
    let mut r = Vec::new();
    gfx12::ring_test(&mut r, SCRATCH).unwrap();
    assert_eq!(r, [0xC001_7900, 0x40, 0xDEAD_BEEF]);
    // IB test :525-530.
    let mut t = Vec::new();
    gfx12::ib_test(&mut t, 0x1_0000_2000).unwrap();
    assert_eq!(t, [0xC003_3700, (5 << 8) | (1 << 20), 0x2000, 1, 0xDEAD_BEEF]);
    // mem_sync :5131-5152, cleaner shader :5169-5174, cntxcntl :4657-4675, pfp_sync_me :4627-4630.
    let mut m = Vec::new();
    gfx12::mem_sync(&mut m);
    gfx12::cleaner_shader(&mut m);
    gfx12::cntxcntl(&mut m, true);
    gfx12::pfp_sync_me(&mut m);
    assert_eq!(m, [0xC006_5800, 0, u32::MAX, 0xFF_FFFF, 0, 0, 0xA, 0xC3B1, 0xC000_D200, 0, 0xC001_2800, 0x8101_8003, 0, 0xC000_4200, 0]);
    // insert_nop :5154-5167: one NOP packet over n dwords.
    let mut n = Vec::new();
    gfx12::insert_nop(&mut n, 4);
    assert_eq!(n, [0xC002_1000, pm4::PAD, pm4::PAD, pm4::PAD]);
    assert_eq!(pm4::packet_len(&n), Ok(4));
}

#[test]
fn every_opcode_of_gfx_v12_0_is_listed_and_no_ce_packet() {
    // 17 opcodes in gfx_v12_0.c: 11 on the gfx and compute rings, 6 only on the KIQ (MES's on this card).
    assert_eq!((gfx12::OPCODES.len(), gfx12::KIQ_OPCODES.len()), (11, 6));
    for op in gfx12::KIQ_OPCODES {
        assert!(!gfx12::emitted(pm4::packet3(op, 0)) && !gfx12::OPCODES.contains(&op), "{op:#x}");
    }
    for op in gfx12::CE_OPCODES {
        assert!(!gfx12::emitted(pm4::packet3(op, 0)), "{op:#x}");
    }
    assert!(!gfx12::emitted(pm4::packet3(pm4::op::INDIRECT_BUFFER_CNST, 2)));
    assert!(!gfx12::emitted(pm4::TYPE2) && !gfx12::emitted(0x0000_1234));
    assert!(gfx12::emitted(pm4::PAD) && gfx12::emitted(pm4::packet3(gfx12::RUN_CLEANER_SHADER, 0)));
}

fn rig() -> (SimMem, Engine) {
    let mut mem = SimMem::new();
    mem.map(0, RING, 256 * 4).unwrap();
    mem.map(0, BUF, 0x1_0000).unwrap();
    let mut e = Engine::new(Isa::Pm4, RING, 256, 0, limits()).unwrap();
    e.speak_gfx12();
    (mem, e)
}

fn run(mem: &mut SimMem, e: &mut Engine, words: &[u32]) -> crate::sim::Report {
    let at = e.rptr();
    mem.poke_dwords(0, RING + at * 4, words).unwrap();
    e.run(mem, at + words.len() as u64)
}

#[test]
fn the_engine_runs_linuxs_ring_test_ib_test_and_fences() {
    let (mut mem, mut e) = rig();
    mem.set_reg(SCRATCH, gfx12::RING_TEST_POISON);
    let mut v = Vec::new();
    gfx12::ring_test(&mut v, SCRATCH).unwrap();
    gfx12::insert_nop(&mut v, 3);
    gfx12::mem_sync(&mut v);
    gfx12::cleaner_shader(&mut v);
    let mut ib = Vec::new();
    gfx12::ib_test(&mut ib, BUF + 0x100).unwrap();
    mem.poke_dwords(0, BUF + 0x1000, &ib).unwrap();
    gfx12::ib_gfx(&mut v, BUF + 0x1000, ib.len() as u32, 0).unwrap();
    gfx12::fence(&mut v, BUF + 0x200, 5, false, true).unwrap();
    gfx12::fence(&mut v, BUF + 0x208, 0x1_0000_0006, true, false).unwrap();
    let rep = run(&mut mem, &mut e, &v);
    assert_eq!(rep.stop, Stop::Idle);
    assert_eq!(mem.reg(SCRATCH), Some(0xDEAD_BEEF));
    assert_eq!((mem.peek_u32(0, BUF + 0x100), mem.peek_u32(0, BUF + 0x200), mem.peek_u64(0, BUF + 0x208)), (Some(0xDEAD_BEEF), Some(5), Some(0x1_0000_0006)));
    assert!(rep.events.iter().any(|x| matches!(x, Event::Interrupt { .. })));
    assert_eq!(rep.events.iter().filter(|x| matches!(x, Event::NoEffect { .. })).count(), 2, "ACQUIRE_MEM and RUN_CLEANER_SHADER");
}

#[test]
fn a_ce_packet_and_any_unlisted_packet_halt_it_where_it_stands() {
    // The KIQ's packets too: on this card the KIQ is MES's pipe, never a gfx or compute queue.
    let kiq = gfx12::KIQ_OPCODES.map(|op| pm4::packet3(op, 4));
    for bad in [pm4::packet3(0x84, 0), pm4::packet3(pm4::op::INDIRECT_BUFFER_CNST, 2), pm4::packet3(pm4::op::DMA_DATA, 5), pm4::packet3(pm4::op::EVENT_WRITE, 0), pm4::TYPE2].into_iter().chain(kiq) {
        let (mut mem, mut e) = rig();
        let n = pm4::packet_len(&[bad]).unwrap();
        let mut v = vec![0u32; n];
        v[0] = bad;
        let rep = run(&mut mem, &mut e, &v);
        assert_eq!(rep.stop, Stop::Hang(Hang::Unsupported { header: bad }), "{bad:#x}");
        assert_eq!(e.rptr(), 0, "the read pointer stays at the packet");
    }
    // The same packets on a GFX 11 engine: EVENT_WRITE runs there, so the refusal is GFX 12's own.
    let mut mem = SimMem::new();
    mem.map(0, RING, 1024).unwrap();
    let mut old = Engine::new(Isa::Pm4, RING, 256, 0, limits()).unwrap();
    mem.poke_dwords(0, RING, &[pm4::packet3(pm4::op::EVENT_WRITE, 0), 0]).unwrap();
    assert_eq!(old.run(&mut mem, 2).stop, Stop::Idle);
}
