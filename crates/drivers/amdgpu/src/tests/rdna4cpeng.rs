//! Wave 44, items CP and RG: the model's command processor runs the queues MES mapped (`rdna4::fakecpeng`): the
//! chain through `mesq` maps the kernel's five, and nothing places a queue by hand. Linux's ring test, IB test and
//! fences run; a CE packet or a packet Linux's GFX 12 code never emits halts the queue at the packet; an address
//! GFXHUB does not know is a fault in its status, never a silent success; a doorbell is taken only as Linux rings
//! it; MES's map is refused while the engines are halted.

use super::rdna4fix::*;
use super::rdna4gc::chain_before;
use crate::gpucore::psp::Hw;
use crate::rdna4::cpring;
use crate::rdna4::fakemes::Mapped;
use crate::rdna4::fakemodel::Model;
use crate::rdna4::host::Host;
use crate::rdna4::meskq::{self, KernelQueue};
use crate::rdna4::ringops;
use crate::rdna4::stage::{Session, Stage};
use amdgpu_fmt::{gfx12, pm4};
use rdna4_core::fault12::st;
use rdna4_core::mes12api::Unmap;
use rdna4_core::{doorbell12 as db, gc12, gc12cp as cp, mqd12};
use std::vec::Vec;

/// The chain through `mesq`: the five kernel queues mapped through MES on the unhalted CP.
pub fn mesq_up() -> (Rig, Session, Model) {
    let rig = Rig::new();
    let mut s = Session::default();
    let (_, mut m) = rig.pass(&mut s, model(), &mut Stick::full(), chain_before(Stage::GcRing));
    m.writes.clear();
    (rig, s, m)
}

/// Kernel queue `i` (`mes12::KERNEL_QUEUES`: 4 is the gfx queue, 0 the first compute queue).
pub fn kq(s: &Session, i: usize) -> KernelQueue {
    KernelQueue::of(s.mes.up.as_ref().unwrap(), i).unwrap()
}

/// The packets onto the queue's ring from `wptr`, padded and committed as Linux commits (`ringops`).
pub fn submit(s: &Session, m: &mut Model, i: usize, wptr: u64, words: &[u32]) -> (bool, u64) {
    let up = s.mes.up.as_ref().unwrap();
    match ringops::commit(m, &up.place, &kq(s, i), wptr, words) {
        Ok(w) => (true, w),
        Err(_) => (false, wptr),
    }
}

pub fn rptr(s: &Session, m: &mut Model, i: usize) -> u64 {
    let up = s.mes.up.as_ref().unwrap();
    cpring::get_rptr(m, up.place.slot_sys(kq(s, i).slots.rptr)).unwrap()
}

/// A free page of VRAM behind everything the manager handed out.
fn free_vram(s: &Session) -> u64 {
    s.bump.as_ref().unwrap().taken.iter().map(|t| t.1.end).max().unwrap() + 0x10_0000
}

#[test]
fn a_mapped_queue_runs_linuxs_ring_test_ib_test_and_fences() {
    let (rig, s, mut m) = mesq_up();
    let n = super::rdna4cp::n_of(&rig);
    let scratch = n.byte(cp::SCRATCH_REG0).unwrap();
    // The ring test (gfx_v12_0_ring_test_ring): the poison by MMIO, then SET_UCONFIG_REG from the ring.
    assert!(m.reg_write(scratch, gfx12::RING_TEST_POISON));
    let mut v = Vec::new();
    gfx12::ring_test(&mut v, scratch / 4).unwrap();
    let (rung, wptr) = submit(&s, &mut m, 4, 0, &v);
    assert!(rung && m.reg_read(scratch) == Some(0xDEAD_BEEF), "{:?}", m.refused);
    assert_eq!((rptr(&s, &mut m, 4), wptr), (256, 256), "the read pointer back in its slot, 32 bits, after the frame's padding");
    // The IB test (gfx_v12_0_ring_test_ib): an IB in VRAM writing a slot, then a 32- and a 64-bit fence.
    let (l, off) = (s.layout.clone().unwrap(), free_vram(&s));
    let mut w = Vec::new();
    gfx12::ib_test(&mut w, l.mc(off + 0x3000)).unwrap();
    let bytes: Vec<u8> = w.iter().flat_map(|x| x.to_le_bytes()).collect();
    assert!(m.mem_write(off + 0x2000, &bytes));
    let mut r = Vec::new();
    gfx12::ib_gfx(&mut r, l.mc(off + 0x2000), w.len() as u32, 0).unwrap();
    gfx12::fence(&mut r, l.mc(off + 0x3008), 5, false, true).unwrap();
    gfx12::fence(&mut r, l.mc(off + 0x3010), 0x1_0000_0006, true, false).unwrap();
    let (rung, wptr) = submit(&s, &mut m, 4, wptr, &r);
    assert!(rung && m.refused.is_empty(), "{:?}", m.refused);
    let mut got = [0u8; 24];
    assert!(m.mem_read(off + 0x3000, &mut got));
    assert_eq!((u32::from_le_bytes(got[..4].try_into().unwrap()), u32::from_le_bytes(got[8..12].try_into().unwrap()), u64::from_le_bytes(got[16..].try_into().unwrap())), (0xDEAD_BEEF, 5, 0x1_0000_0006));
    assert_eq!(rptr(&s, &mut m, 4), wptr);
    // The fence asked for its interrupt, but the end-of-pipe enables are not on yet (`ringtest` sets them): none.
    assert!(m.cp.raised.is_empty());
    // A compute queue on MEC: the same IB with INDIRECT_BUFFER_VALID.
    let mut r = Vec::new();
    gfx12::ib_compute(&mut r, l.mc(off + 0x2000), w.len() as u32, 0).unwrap();
    let (rung, wptr) = submit(&s, &mut m, 0, 0, &r);
    assert!(rung && m.refused.is_empty() && rptr(&s, &mut m, 0) == wptr, "{:?}", m.refused);
    assert!(m.cp.queues.iter().all(|q| q.halted.is_none()) && !m.faults_open()[0]);
}

#[test]
fn a_ce_packet_or_one_linux_never_emits_halts_the_queue_at_the_packet() {
    for bad in [pm4::packet3(0x84, 0), pm4::packet3(pm4::op::INDIRECT_BUFFER_CNST, 2), pm4::packet3(pm4::op::DMA_DATA, 5), pm4::packet3(pm4::op::EVENT_WRITE, 0)] {
        let (_, s, mut m) = mesq_up();
        let n = pm4::packet_len(&[bad]).unwrap();
        let mut v = std::vec![0u32; n];
        v[0] = bad;
        let (rung, _) = submit(&s, &mut m, 4, 0, &v);
        assert!(rung, "{bad:#x}: the doorbell is taken");
        assert_eq!(rptr(&s, &mut m, 4), 0, "{bad:#x}: the read pointer stays at the packet");
        let why = m.cp.queues.iter().find_map(|q| q.halted.clone()).unwrap_or_default();
        assert!(why.contains(&std::format!("Unsupported {{ header: {bad} }}")), "{bad:#x}: {why}");
        // A halted queue runs no more.
        let mut ok = Vec::new();
        gfx12::cleaner_shader(&mut ok);
        submit(&s, &mut m, 4, 256, &ok);
        assert_eq!(rptr(&s, &mut m, 4), 0);
    }
}

#[test]
fn an_address_gfxhub_does_not_know_is_a_fault_with_the_cps_client_id() {
    // An IB inside the GART that nothing bound: the fetch faults (CPF), the queue stops there.
    let (_, s, mut m) = mesq_up();
    let mut r = Vec::new();
    gfx12::ib_gfx(&mut r, 0x1000_0000, 5, 0).unwrap();
    let (rung, _) = submit(&s, &mut m, 4, 0, &r);
    assert!(rung && m.faults_open()[0] && m.cp.queues.iter().any(|q| q.halted.is_some()), "{:?}", m.refused);
    assert_eq!(st::CID.get(m.gc.fault_status[0]), 4, "CPF fetched");
    // A fence into an unbound page of the GART: a write fault of CPG.
    let (_, s, mut m) = mesq_up();
    let mut r = Vec::new();
    gfx12::fence(&mut r, 0x1000_0000, 1, false, false).unwrap();
    submit(&s, &mut m, 4, 0, &r);
    let status = m.gc.fault_status[0];
    assert!(m.faults_open()[0] && st::CID.get(status) == 6 && st::RW.get(status) == 1, "{status:#x}");
}

#[test]
fn a_doorbell_is_taken_only_as_linux_rings_it() {
    let (rig, s, mut m) = mesq_up();
    let up = s.mes.up.as_ref().unwrap();
    // A doorbell with no queue behind it (the queues' dwords are 6, 8, 10, 12 and 0x116): a posted write the card
    // takes and nobody answers, named by the model.
    assert!(m.doorbell_write64(db::mec_ring_dw(7), 4) && m.refused.pop().unwrap().contains("no queue was placed"));
    // One rung before the shadow holds the value.
    let q = kq(&s, 4);
    assert!(m.sys_write(up.place.slot_sys(q.slots.wptr), &8u64.to_le_bytes()));
    assert!(!m.doorbell_write64(q.kq.doorbell, 4) && m.refused.pop().unwrap().contains("Linux writes the shadow first"));
    // The CP's doorbells and SDMA's do not steal each other's dwords: 0x200 is still SDMA's.
    assert!(!m.doorbell_write64(0x200, 0x40) && m.refused.pop().unwrap().contains("NBIF's doorbell entry for the engines"));
    // NBIF's entry and the ranges as Linux wrote them.
    let n = super::rdna4cp::n_of(&rig);
    m.cp.entries[0] = None;
    let (rung, _) = submit(&s, &mut m, 4, 0, &[pm4::PAD]);
    assert!(!rung && m.refused.pop().unwrap().contains("NBIF's doorbell entry 0"));
    m.cp.entries[0] = Some(db::ENTRY_0_GFX);
    m.cp.vals.insert(n.byte(cp::CP_RB_DOORBELL_RANGE_UPPER).unwrap(), 0);
    let (rung, _) = submit(&s, &mut m, 4, 0, &[pm4::PAD]);
    assert!(!rung && m.refused.pop().unwrap().contains("doorbell ranges hold Linux's values"));
}

/// The cross-item rule between the two models: MES maps a kernel queue only onto the unhalted command processor;
/// a doorbell of a queue whose engine was halted runs nothing.
#[test]
fn mes_maps_a_queue_only_onto_the_unhalted_command_processor() {
    let (rig, mut s, mut m) = mesq_up();
    let n = super::rdna4cp::n_of(&rig);
    let mec = n.byte(gc12::CP_MEC_RS64_CNTL).unwrap();
    // Unmapped, MEC halted as Linux's fini halts it: the map again is refused by the CP's model.
    let legacy = kq(&s, 0).legacy(&s.mes.up.as_ref().unwrap().place);
    let up = s.mes.up.as_mut().unwrap();
    let (sent, _) = meskq::unmap_legacy(up, &mut m, &legacy, Unmap::ResetQueues, (0, 0)).unwrap();
    assert!(sent.ok() && !m.cp.queues.iter().any(|q| q.at.doorbell_dw == legacy.doorbell));
    let v = m.reg_read(mec).unwrap();
    assert!(m.reg_write(mec, cp::halt_mec(v)), "{:?}", m.refused);
    let (sent, _) = meskq::map_legacy(up, &mut m, &legacy).unwrap();
    assert!(!sent.ok() && m.refused.iter().any(|r| r.contains("the command processor's model refused the queue: no queue on a halted MEC")), "{:?}", m.refused);
    m.refused.clear();
    // The compute queues still mapped run nothing now: their doorbell is a posted write nobody answers.
    let (rung, _) = submit(&s, &mut m, 1, 0, &[pm4::PAD]);
    assert!(rung && m.refused.pop().unwrap().contains("a queue on a halted engine"));
    // The CP's own checks of a map, which MES never reaches with the kernel's five doorbells, are held past MES
    // below (`the_cp_takes_a_queue_only_on_a_free_doorbell_in_its_range_from_an_mqd_at_0`).
}

/// What MES hands the command processor's model when it maps queue `i`, with another doorbell.
fn mapped(s: &Session, m: &mut Model, i: usize, doorbell: u32) -> (Mapped, Vec<u32>) {
    let up = s.mes.up.as_ref().unwrap();
    let q = kq(s, i);
    let l = q.legacy(&up.place);
    let mut b = std::vec![0u8; mqd12::MQD_BYTES as usize];
    assert!(m.sys_read(q.mqd_sys, &mut b));
    let mqd = b.chunks(4).map(|c| u32::from_le_bytes(c.try_into().unwrap())).collect();
    (Mapped { queue_type: l.queue_type, pipe: l.pipe, queue: l.queue, doorbell, mqd: l.mqd, wptr: l.wptr }, mqd)
}

/// The command processor's own checks of a queue MES hands it, reached here past MES (which takes only the
/// kernel's five doorbells): a doorbell outside the CP's range for the queue's kind, one that is another queue's,
/// an MQD whose pointers are not Linux's 0; and a doorbell rung while the GC block has no power.
#[test]
fn the_cp_takes_a_queue_only_on_a_free_doorbell_in_its_range_from_an_mqd_at_0() {
    let (rig, mut s, mut m) = mesq_up();
    let n = super::rdna4cp::n_of(&rig);
    let rd = |m: &mut Model, r| m.reg_read(n.byte(r).unwrap()).unwrap() / 4;
    let (rb_lo, rb_hi) = (rd(&mut m, cp::CP_RB_DOORBELL_RANGE_LOWER), rd(&mut m, cp::CP_RB_DOORBELL_RANGE_UPPER));
    let mec_hi = rd(&mut m, cp::CP_MEC_DOORBELL_RANGE_UPPER);
    let legacy = kq(&s, 0).legacy(&s.mes.up.as_ref().unwrap().place);
    let (sent, _) = meskq::unmap_legacy(s.mes.up.as_mut().unwrap(), &mut m, &legacy, Unmap::ResetQueues, (0, 0)).unwrap();
    assert!(sent.ok() && m.refused.is_empty(), "{:?}", m.refused);
    // MEC's range starts at the KIQ's doorbell 0: a compute doorbell can only lie above it; the gfx ring's on both
    // sides of the RB range.
    for (i, doorbell, why) in [(0, mec_hi + 1, "outside the CP's range"), (4, rb_lo - 1, "outside the CP's range"), (4, rb_hi + 1, "outside the CP's range"), (0, kq(&s, 1).kq.doorbell, "is another queue's")] {
        let (q, mqd) = mapped(&s, &mut m, i, doorbell);
        let e = m.mes_queue_mapped(q, &mqd).unwrap_err();
        assert!(e.contains(why), "{doorbell:#x}: {e}");
    }
    // The MQD with a read or a write pointer that is not 0 (a queue saved by an unmap and mapped again as it was).
    for (at, v) in [(mqd12::c::HQD_PQ_RPTR, 0x40), (mqd12::c::HQD_PQ_WPTR_LO, 0x40), (mqd12::c::HQD_PQ_WPTR_HI, 1)] {
        let (q, mut mqd) = mapped(&s, &mut m, 0, legacy.doorbell);
        mqd[at] = v;
        let e = m.mes_queue_mapped(q, &mqd).unwrap_err();
        assert!(e.contains("Linux maps a kernel queue with both 0"), "dword {at}: {e}");
    }
    let (q, mqd) = mapped(&s, &mut m, 0, legacy.doorbell);
    assert_eq!(m.mes_queue_mapped(q, &mqd), Ok(()));
    // A doorbell of the command processor on a block without power.
    let rig = Rig::new();
    let mut cold = Session::default();
    let (_, mut m) = rig.pass(&mut cold, model(), &mut Stick::full(), chain_before(Stage::Autoload));
    assert!(!m.doorbell_write64(db::mec_ring_dw(3), 4), "{:?}", m.refused);
    assert!(m.refused.pop().unwrap().contains("rung while the GC block has no power or its autoload is not complete"), "{:?}", m.refused);
}
