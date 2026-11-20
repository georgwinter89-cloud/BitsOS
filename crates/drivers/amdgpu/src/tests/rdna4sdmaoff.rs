//! Wave 43: the copy engines at a Reload (`rdna4::sdmaoff`, the hook of `livesdma`): nothing of the block is read
//! or written before the stage `sdma` set out to write it; from then on the engines are stopped as Linux's
//! `sdma_v7_0_hw_fini` stops them, whatever came of the stage.

use super::rdna4fix::*;
use super::rdna4sdma::{loaded, sr};
use crate::gpucore::psp::Hw;
use crate::rdna4::fakemodel::Model;
use crate::rdna4::ihoff::OffIo;
use crate::rdna4::sdmaoff::{self, OffRegs, Stopped, Written};
use crate::rdna4::stage::Stage;
use rdna4_core::sdma7;
use std::vec::Vec;

/// The Reload hook's side of the card: the writes it made, and how many reads.
struct Regs<'a>(&'a mut Model, Vec<(u32, u32)>, u32);

impl OffIo for Regs<'_> {
    fn read(&mut self, byte: u32) -> Option<u32> {
        self.2 += 1;
        self.0.reg_read(byte)
    }
    fn write(&mut self, byte: u32, v: u32) -> bool {
        self.1.push((byte, v));
        self.0.reg_write(byte, v)
    }
    fn delay_us(&mut self, _us: u64) {}
}

/// A Reload: the engines stopped as Linux's `sdma_v7_0_hw_fini` stops them, from the moment the stage sets out
/// to write the block and not before: until then the hook neither reads nor writes it.
#[test]
fn a_reload_stops_the_engines_as_linux_does_once_the_stage_set_out_to_write_them() {
    let rig = Rig::new();
    let r = sr(&rig);
    let off = OffRegs::of(&r).unwrap();
    let b = |reg, i| r.byte(reg, i).unwrap();
    // gfx_stop per engine (the ring, the IBs), then both halts.
    let linux = |rb0: u32, rb1: u32| [(b(sdma7::RB_CNTL, 0), rb0), (b(sdma7::IB_CNTL, 0), 0x100), (b(sdma7::RB_CNTL, 1), rb1), (b(sdma7::IB_CNTL, 1), 0x100), (b(sdma7::MCU_CNTL, 0), 1), (b(sdma7::MCU_CNTL, 1), 1)];
    // The hook is armed before the stage runs. A stage that stops before its first write (no pages) leaves the
    // word unset: the block, which may be without power, is not even read.
    let (mut s, mut m) = loaded(&rig);
    let hook = Written::default();
    s.sdma.reload = Some(hook.clone());
    m.nb.pages_refused = true;
    let (d, mut m) = rig.run(&mut s, Stage::Sdma, m, &mut Stick::full());
    assert!(!d.pass && !s.sdma.tried && !hook.get(), "{}", d.line);
    let mut io = Regs(&mut m, Vec::new(), 0);
    assert_eq!((sdmaoff::stop(&mut io, &off, &hook), io.1.len(), io.2), (Stopped::Untouched, 0, 0));
    // The stage passes: the word is set, and the stop is Linux's.
    m.nb.pages_refused = false;
    let (d, mut m) = rig.run(&mut s, Stage::Sdma, m, &mut Stick::full());
    assert!(d.pass && s.sdma.tried && hook.get(), "{}", d.line);
    let mut io = Regs(&mut m, Vec::new(), 0);
    assert_eq!((sdmaoff::stop(&mut io, &off, &hook), io.2), (Stopped::Halted, 6));
    assert_eq!(io.1, linux(0x0084_1816, 0x0084_1816));
    assert!(m.refused.is_empty() && !m.sdma.any_on(), "{:?}", m.refused);
    // A stopped engine takes no doorbell. Linux's hw_fini asks nothing: a second stop writes the same again.
    assert!(!m.doorbell_write64(0x200, 0x80) && m.refused.pop().unwrap().contains("ring off"));
    let mut io = Regs(&mut m, Vec::new(), 0);
    assert_eq!(sdmaoff::stop(&mut io, &off, &hook), Stopped::Halted);
    assert_eq!(io.1, linux(0x0084_1816, 0x0084_1816));
    // A stage that stopped between the unhalts and its first ring (the card may not send, so the model refuses
    // the ring's enable): no ring is on, and the MCUs are halted all the same.
    let (mut s, mut m) = loaded(&rig);
    let hook = Written::default();
    s.sdma.reload = Some(hook.clone());
    m.nb.bus_master = false;
    let (d, mut m) = rig.run(&mut s, Stage::Sdma, m, &mut Stick::full());
    assert!(!d.pass && s.sdma.tried && hook.get() && !m.sdma.any_on() && m.refused.len() == 1, "{} {:?}", d.line, m.refused);
    m.refused.clear();
    let mut io = Regs(&mut m, Vec::new(), 0);
    assert_eq!(sdmaoff::stop(&mut io, &off, &hook), Stopped::Halted);
    // Engine 0's control holds its size from the stage's first write of it; engine 1's is still the cold card's.
    assert_eq!(io.1, linux(0x0084_0816, 0x0004_0800));
    assert!(m.refused.is_empty() && m.sdma.e.iter().all(|e| sdma7::f::HALT.get(e.val(sdma7::MCU_CNTL)) == 1));
    // A card that fell off the bus reads all ones: the stop ends at its first read and writes nothing.
    let mut gone = Gone(0);
    assert_eq!((sdmaoff::stop(&mut gone, &off, &hook), gone.0), (Stopped::Failed, 0));
}

struct Gone(u32);

impl OffIo for Gone {
    fn read(&mut self, _byte: u32) -> Option<u32> {
        Some(u32::MAX)
    }
    fn write(&mut self, _byte: u32, _v: u32) -> bool {
        self.0 += 1;
        true
    }
    fn delay_us(&mut self, _us: u64) {}
}
