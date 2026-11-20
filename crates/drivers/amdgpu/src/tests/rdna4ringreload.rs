//! Wave 44, item RG: a Reload after the whole chain, the five hooks in the order their registration gives them.
//! Each is registered when its stage first runs (`livestage::run_one`: GFXHUB's at `gchub`, the command
//! processor's at `gccp`, MES's at `mes`, the rings' at `gcring`, the copy engines' at `sdma`) and hooks run newest
//! first, so the chain's order makes the Reload's: the engines stopped, the rings' interrupt enables cleared, MES
//! halted, then the command processor, then GFXHUB off. That is Linux's: the blocks' fini in reverse order (sdma
//! before gfx), and inside `gfx_v12_0_hw_fini` the fault sources put (gfx_v12_0.c:3888-3890) before MES's
//! `amdgpu_mes_kiq_hw_fini` (:3902) before `gfx_v12_0_cp_enable(adev, false)` (:3914) before `gart_disable`
//! (:3917); only the end-of-pipe pair, which Linux's fence driver clears before every block's suspend
//! (amdgpu_device.c:4577), is cleared after the copy engines' stop.

use super::rdna4fix::*;
use crate::gpucore::psp::Hw;
use crate::rdna4::fakemodel::Model;
use crate::rdna4::ihoff::OffIo;
use crate::rdna4::sdmaoff::{Stopped, Written};
use crate::rdna4::stage::{Session, Stage};
use crate::rdna4::{cpoff, mesoff, ringoff, sdmaoff};
use rdna4_core::{gc12, gc12ring as rg, mes12};

struct Rec<'a>(&'a mut Model);

impl OffIo for Rec<'_> {
    fn read(&mut self, byte: u32) -> Option<u32> {
        self.0.reg_read(byte)
    }
    fn write(&mut self, byte: u32, v: u32) -> bool {
        self.0.reg_write(byte, v)
    }
    fn delay_us(&mut self, us: u64) {
        self.0.delay_us(us)
    }
}

#[test]
fn a_reload_clears_the_rings_interrupts_halts_mes_before_the_command_processor_before_gfxhub() {
    // The hooks' registering stages stand in this order in the chain.
    let at = |st: Stage| Stage::CHAIN.iter().position(|x| *x == st).unwrap();
    assert!(at(Stage::GcHub) < at(Stage::GcCp) && at(Stage::GcCp) < at(Stage::Mes) && at(Stage::Mes) < at(Stage::GcRing) && at(Stage::GcRing) < at(Stage::Sdma));
    let rig = Rig::new();
    let mut s = Session::default();
    let (_, mut m) = rig.pass(&mut s, model(), &mut Stick::full(), &Stage::CHAIN);
    let (_, back) = rig.pass(&mut s, m, &mut Stick::full(), &[Stage::RingTest]);
    m = back;
    let n = super::rdna4cp::n_of(&rig);
    let w = Written::default();
    w.set();
    m.writes.clear();
    // Newest first: sdma's, the rings', MES's, the command processor's. The rings' hook before its stage set out to
    // write touches nothing.
    let sr = crate::rdna4::sdmaregs::SdmaRegs::from(&rig.att.discovery).unwrap();
    assert_eq!(sdmaoff::stop(&mut Rec(&mut m), &sdmaoff::OffRegs::of(&sr).unwrap(), &w), Stopped::Halted);
    let rr = ringoff::OffRegs::of(&n).unwrap();
    let before = m.writes.len();
    assert_eq!(ringoff::off(&mut Rec(&mut m), &rr, &Written::default()), Stopped::Untouched);
    assert_eq!(m.writes.len(), before);
    assert_eq!(ringoff::off(&mut Rec(&mut m), &rr, &w), Stopped::Halted);
    // Linux's ten changes on the three pipes (the end-of-pipe pair, then bad opcode, privileged instruction,
    // privileged register), none of the enables left; a second run finds nothing to clear.
    let ints = [rr.ring0, rr.pipe0, rr.pipe1];
    let cleared: std::vec::Vec<u32> = m.writes[before..].iter().filter(|(b, _)| ints.contains(b)).map(|(b, _)| *b).collect();
    assert_eq!(cleared, [rr.ring0, rr.pipe0, rr.pipe1, rr.ring0, rr.pipe0, rr.pipe1, rr.ring0, rr.ring0, rr.pipe0, rr.pipe1], "{:x?}", &m.writes[before..]);
    let all = rg::EOP_ENABLES | rg::f::PRIV_REG_INT_ENABLE.mask | rg::f::PRIV_INSTR_INT_ENABLE.mask | rg::f::OPCODE_ERROR_INT_ENABLE.mask;
    assert!(ints.iter().all(|b| m.reg_read(*b).unwrap() & all == 0), "{:x?}", ints.map(|b| m.reg_read(b)));
    let again = m.writes.len();
    assert_eq!(ringoff::off(&mut Rec(&mut m), &rr, &w), Stopped::Halted);
    assert_eq!(m.writes.len(), again);
    assert!(matches!(mesoff::halt(&mut Rec(&mut m), &n, &w), mesoff::Halted::Halted { .. }));
    assert_eq!(cpoff::halt(&mut Rec(&mut m), &cpoff::OffRegs::of(&n).unwrap(), &w), Stopped::Halted);
    assert!(m.refused.is_empty(), "{:?}", m.refused);
    let first = |r| m.writes.iter().position(|(b, _)| Some(*b) == n.byte(r)).unwrap();
    assert!(first(rg::CP_ME1_PIPE1_INT_CNTL) < first(mes12::CP_MES_CNTL) && first(mes12::CP_MES_CNTL) < first(gc12::CP_ME_CNTL) && first(gc12::CP_ME_CNTL) < first(gc12::CP_MEC_RS64_CNTL), "{:x?}", m.writes);
    assert!(!m.mes.running.iter().any(|r| *r) && !m.cp.gfx_on && !m.cp.mec_on && !m.sdma.any_on());
    // GFXHUB's switch-off comes last and the model takes it with every engine on it halted.
    let off = crate::rdna4::gchuboff::gart_disable(&mut Rec(&mut m), &n, &rdna4_core::hub41::GC);
    assert!(m.refused.is_empty() && !m.gc.context0, "{off:?} {:?}", m.refused);
}
