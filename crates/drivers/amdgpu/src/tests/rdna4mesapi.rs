//! Wave 44, item ME: what MES gives the stages after it. The TLB flush after a GART change goes through MES once
//! it runs (Linux's form: three HDP flushes and one WR_WAIT_WR_REG per hub on the KIQ ring), and the card's walker
//! sees a page only after MES did the hubs' invalidations; a flush MES does not answer is logged and gone on, as
//! Linux's is. The API the rings' stage needs (a kernel queue mapped and unmapped, a register read, written and
//! written-and-waited through MES) is held against the model; the KIQ ring wraps; a Reload halts MES.

use super::rdna4fix::*;
use super::rdna4mes::{before_mes, up};
use crate::gpucore::psp::Hw;
use crate::gpucore::trail::Access;
use crate::rdna4::fakemodel::Model;
use crate::rdna4::guard::Mc;
use crate::rdna4::host::Host;
use crate::rdna4::ihoff::OffIo;
use crate::rdna4::meskq::{self, MesWrites};
use crate::rdna4::mesoff::{self, Halted, Written};
use crate::rdna4::stage::{Session, Stage};
use rdna4_core::hub41;
use rdna4_core::mes12::{self, Pipe};
use rdna4_core::mes12api::{self as api, Unmap, at, op};
use rdna4_core::nbreg::member;
use std::string::String;
use std::vec::Vec;

/// A page of system memory of the test's own, bound and flushed in this boot's form (the table written at MC
/// addresses, as a stage's `guard::Mc` hands them to the card); the bind's lines.
fn bind_page(s: &mut Session, m: &mut Model, n: &crate::rdna4::nbregs::NbRegs) -> (u64, u64, Vec<String>) {
    let sys = m.pages(4096).unwrap();
    let f = s.flush_regs(n).unwrap();
    let l = s.layout.clone().unwrap();
    let mut tlb = crate::rdna4::mesflush::tlb(f, &mut s.mes);
    let mut out = Vec::new();
    let mut card = Mc { hw: core::mem::replace(m, model()), base: l.mc_base, bytes: l.vram_bytes };
    let mc = s.nb.gart.as_mut().unwrap().map_with(&mut card, &mut tlb, "test page", &[sys], &mut out).unwrap();
    *m = card.hw;
    (sys, mc, out)
}

/// The same for the unbind.
fn unbind_page(s: &mut Session, m: &mut Model, n: &crate::rdna4::nbregs::NbRegs, mc: u64) {
    let f = s.flush_regs(n).unwrap();
    let l = s.layout.clone().unwrap();
    let mut tlb = crate::rdna4::mesflush::tlb(f, &mut s.mes);
    let mut card = Mc { hw: core::mem::replace(m, model()), base: l.mc_base, bytes: l.vram_bytes };
    s.nb.gart.as_mut().unwrap().unmap_with(&mut card, &mut tlb, mc, &mut Vec::new()).unwrap();
    *m = card.hw;
}

#[test]
fn a_bind_after_mes_flushes_through_mes_and_the_walker_sees_it_only_then() {
    let rig = Rig::new();
    let n = super::rdna4nbmodel::n(&rig);
    let (mut s, mut m, _) = up(&rig);
    let (gc_req, gc_ack) = (n.byte(member(hub41::GC.eng0_req, hub41::GART_ENGINE, 1)).unwrap(), n.byte(member(hub41::GC.eng0_ack, hub41::GART_ENGINE, 1)).unwrap());
    let (mm_req, mm_ack) = (n.byte(member(hub41::MM.eng0_req, hub41::GART_ENGINE, 1)).unwrap(), n.byte(member(hub41::MM.eng0_ack, hub41::GART_ENGINE, 1)).unwrap());
    let (frames, writes, flushes, gc_tlb, mm_tlb) = (m.mes.frames.len(), m.writes.len(), m.flushes, m.gc.tlb_flushes, m.nb.tlb_flushes);
    let (sys, mc, out) = bind_page(&mut s, &mut m, &n);
    assert!(out.is_empty(), "{out:?}");
    // On the bus: three HDP flushes, no request written by the CPU.
    assert_eq!((m.flushes - flushes, m.writes.len() - writes), (3, 3 + 2), "three flushes' writes, and MES's two requests through the model");
    assert!(m.writes[writes..].iter().filter(|(b, _)| *b != 0x7_F000).map(|(b, _)| *b).eq([gc_req, mm_req]), "{:x?}", &m.writes[writes..]);
    // The two commands: WR_WAIT_WR_REG on the KIQ ring, GFXHUB's engine 17 then MMHUB's, VMID 0's request, bit 0.
    let cmds: Vec<&Vec<u32>> = m.mes.frames[frames..].iter().filter(|(_, d)| api::header_fields(d[0]).1 == op::MISC).map(|(_, d)| d).collect();
    assert!(m.mes.frames[frames..].iter().all(|(p, _)| *p == Pipe::Kiq.index()));
    assert_eq!(cmds.len(), 2);
    for (d, (req, ack)) in cmds.iter().zip([(gc_req, gc_ack), (mm_req, mm_ack)]) {
        assert_eq!((d[at::MISC_OPCODE], &d[at::MISC_DATA..=at::MISC_WRM_REG2], api::get64(d, at::MISC_STATUS + 2)), (api::misc::WAIT_REG_MEM, &[api::misc::WRM_WR_WAIT_WR_REG, 0x00F8_0001, 1, req / 4, ack / 4][..], 1));
    }
    assert_eq!((m.gc.tlb_flushes - gc_tlb, m.nb.tlb_flushes - mm_tlb), (1, 1));
    // The walker sees the page on both hubs.
    assert_eq!((m.gc_walk(mc), m.gart_walk(mc)), (Some(sys), Some(sys)));
    // The unbind likewise: the walker loses it.
    unbind_page(&mut s, &mut m, &n, mc);
    assert_eq!(m.gc_walk(mc), None);
}

#[test]
fn a_flush_mes_does_not_answer_is_logged_and_gone_on_and_the_page_stays_unseen() {
    let rig = Rig::new();
    let n = super::rdna4nbmodel::n(&rig);
    let (mut s, mut m, _) = up(&rig);
    m.mes.dead[Pipe::Kiq.index()] = true;
    let used = s.mes.up.as_ref().unwrap().pool.used();
    let (sys, mc, out) = bind_page(&mut s, &mut m, &n);
    // Each unanswered command's status slot stays taken: a MES that answers late writes into no later command's.
    assert_eq!(s.mes.up.as_ref().unwrap().pool.used(), used + 2);
    assert_eq!(out.len(), 2, "one line per hub: {out:?}");
    assert!(out.iter().all(|l| l.starts_with("tlb flush: MES(1) failed to respond to msg=MISC (WAIT_REG_MEM): no fence after 1050000 polls") && l.ends_with("Linux logs it and goes on (gmc_v12_0.c:322-326), so does this")), "{out:?}");
    // GFXHUB never got its invalidation: its walker does not find the page; the bind itself stands.
    assert_eq!((m.gc_walk(mc), s.nb.gart.as_ref().unwrap().system_of(mc)), (None, Some(sys)));
}

#[test]
fn the_flush_before_mes_is_by_registers_and_after_it_the_write_lists_hold_no_hub_request() {
    let rig = Rig::new();
    let n = super::rdna4nbmodel::n(&rig);
    let (s, _) = before_mes(&rig);
    let regs = s.flush_regs(&n).unwrap();
    assert_eq!(s.flush_written(&n), regs.written(), "before MES: the register form of both hubs");
    assert!(matches!(crate::rdna4::mesflush::tlb(regs, &mut crate::rdna4::messtage::State::new()), crate::rdna4::mesflush::Tlb::Regs(_)));
    let (s, _, [mes, _]) = up(&rig);
    assert_eq!(s.flush_written(&n), [regs.hdp], "after MES: the HDP flush register alone");
    // MES's own bind was the register form: the GFXHUB and MMHUB requests are in its trail, written by the CPU.
    let req = |h: &hub41::Hub| n.byte(member(h.eng0_req, hub41::GART_ENGINE, 1)).unwrap();
    assert!([req(&hub41::GC), req(&hub41::MM)].iter().all(|r| mes.trail.iter().any(|a| matches!(a, Access::Write { byte, .. } if byte == r))));
}

#[test]
fn the_api_maps_unmaps_and_reaches_registers_through_mes_as_linux_does() {
    let rig = Rig::new();
    let n = super::rdna4nbmodel::n(&rig);
    let (mut s, mut m, _) = up(&rig);
    let up = s.mes.up.as_mut().unwrap();
    // The gfx queue preempted without its unmap: the trailing fence's value lands where it was told.
    let g = meskq::KernelQueue::of(up, 4).unwrap();
    let gfx = g.legacy(&up.place);
    let tf = up.place.slot_mc(g.slots.trail);
    let (sent, line) = meskq::unmap_legacy(up, &mut m, &gfx, Unmap::PreemptQueuesNoUnmap, (tf, 0x1_0000_0042)).unwrap();
    assert!(sent.ok(), "{line} {:?}", m.refused);
    let mut b = [0u8; 4];
    assert!(m.sys_read(up.place.slot_sys(g.slots.trail), &mut b) && u32::from_le_bytes(b) == 0x42);
    assert!(m.cp.queues.iter().any(|q| q.at.doorbell_dw == 0x116), "a preemption keeps the queue mapped");
    let rm = &m.mes.frames.last().unwrap().1;
    assert_eq!(api::header_fields(m.mes.frames[m.mes.frames.len() - 2].1[0]).1, op::REMOVE_QUEUE);
    let rmf = &m.mes.frames[m.mes.frames.len() - 2].1;
    assert_eq!((rmf[at::REMOVE_FLAGS], rmf[at::REMOVE_TF_DATA], rmf[at::REMOVE_QUEUE_TYPE], rmf[at::REMOVE_DOORBELL]), (api::REMOVE_PREEMPT_LEGACY_GFX, 0x42, 0, 0x116));
    assert_eq!(api::header_fields(rm[0]).1, op::QUERY_SCHEDULER_STATUS);
    // Unmapped, then mapped again: the command processor's model lets it go and takes it again.
    let (sent, _) = meskq::unmap_legacy(up, &mut m, &gfx, Unmap::ResetQueues, (0, 0)).unwrap();
    assert!(sent.ok() && m.cp.queues.len() == 4 && !m.cp.queues.iter().any(|q| q.at.doorbell_dw == 0x116));
    let (sent, _) = meskq::map_legacy(up, &mut m, &gfx).unwrap();
    assert!(sent.ok() && m.cp.queues.len() == 5 && m.refused.is_empty(), "{:?}", m.refused);
    // Registers through MES: a read, a write, a write and wait on a hub's engine 17, each write on the list of
    // what the caller lets MES write.
    let unmapped = n.byte(mes12::CP_UNMAPPED_DOORBELL).unwrap() / 4;
    assert_eq!(meskq::rreg(up, &mut m, unmapped).unwrap(), Ok(0x0D31));
    let gfx_control = n.byte(mes12::CP_HQD_GFX_CONTROL).unwrap() / 4;
    let (req, ack) = (n.byte(member(hub41::GC.eng0_req, hub41::GART_ENGINE, 1)).unwrap() / 4, n.byte(member(hub41::GC.eng0_ack, hub41::GART_ENGINE, 1)).unwrap() / 4);
    let regs = super::rdna4nb::regs(&rig);
    let list = [gfx_control * 4, req * 4, 0x1000 * 4];
    let may = MesWrites { regs: &regs, list: &list };
    assert_eq!(meskq::wreg(up, &mut m, may, gfx_control, mes12::HQD_GFX_CONTROL).unwrap(), Ok(()));
    assert_eq!(meskq::reg_write_reg_wait(up, &mut m, may, req, ack, hub41::invalidate_req(0), 1).unwrap(), Ok(()));
    assert!(m.mes.misc_writes.ends_with(&[(gfx_control, 0x8000), (req, 0x00F8_0001)]), "{:x?}", m.mes.misc_writes);
    // One the model refuses comes back as Linux's "failed to respond" line.
    let said = meskq::reg_write_reg_wait(up, &mut m, may, 0x1000, 0x1001, 1, 1).unwrap().unwrap_err();
    assert!(said.starts_with("MES(1) failed to respond to msg=MISC (WAIT_REG_MEM): the fence came after 0 polls"), "{said}");
}

#[test]
fn a_write_through_mes_reaches_the_firmware_only_from_the_callers_list_and_never_a_refused_class() {
    let rig = Rig::new();
    let n = super::rdna4nbmodel::n(&rig);
    let regs = super::rdna4nb::regs(&rig);
    let (mut s, mut m, _) = up(&rig);
    let up = s.mes.up.as_mut().unwrap();
    let gfx_control = n.byte(mes12::CP_HQD_GFX_CONTROL).unwrap() / 4;
    let (req, ack) = (n.byte(member(hub41::GC.eng0_req, hub41::GART_ENGINE, 1)).unwrap() / 4, n.byte(member(hub41::GC.eng0_ack, hub41::GART_ENGINE, 1)).unwrap() / 4);
    // On every list, a class refused by construction stays refused: an SMU message nobody reviewed, the
    // bootloader's mailbox, the index pair.
    let all = [gfx_control * 4, req * 4, regs.smu_message, regs.psp.c2pmsg_35, regs.rsmu_index, regs.rsmu_data];
    let wide = MesWrites { regs: &regs, list: &all };
    let none = MesWrites { regs: &regs, list: &[] };
    let frames = m.mes.frames.len();
    for (may, reg, value, why) in [
        (none, gfx_control, mes12::HQD_GFX_CONTROL, "not on the list of what this stage lets MES write"),
        (wide, regs.smu_message / 4, 0xFFFF, "an SMU message nobody reviewed"),
        (wide, regs.psp.c2pmsg_35 / 4, 1, "the bootloader's mailbox"),
        (wide, regs.rsmu_index / 4, 0x1234, "the index pair"),
        (wide, regs.rsmu_data / 4, 0x1234, "the index pair"),
    ] {
        let said = meskq::wreg(up, &mut m, may, reg, value).unwrap().unwrap_err();
        assert!(said.starts_with("refused before MES: ") && said.contains(why), "{why}: {said}");
    }
    let said = meskq::reg_write_reg_wait(up, &mut m, none, req, ack, hub41::invalidate_req(0), 1).unwrap().unwrap_err();
    assert!(said.contains("not on the list of what this stage lets MES write"), "{said}");
    // Nothing of it reached the firmware.
    assert_eq!(m.mes.frames.len(), frames);
    assert!(m.refused.is_empty());
}

#[test]
fn the_kiq_ring_wraps_and_every_command_after_the_wrap_is_answered() {
    let rig = Rig::new();
    let n = super::rdna4nbmodel::n(&rig);
    let (mut s, mut m, _) = up(&rig);
    let up = s.mes.up.as_mut().unwrap();
    let reg = n.byte(mes12::CP_UNMAPPED_DOORBELL).unwrap() / 4;
    // 64 commands of 128 dwords fill the 8192 dwords once; past that each lands at the ring's start again.
    for i in 0..70 {
        assert_eq!(meskq::rreg(up, &mut m, reg).unwrap(), Ok(0x0D31), "command {i}");
    }
    assert!(up.ports[1].wptr > u64::from(mes12::RING_DW) && m.mes.queue[1].unwrap().rptr == up.ports[1].wptr && m.refused.is_empty());
    assert_eq!(up.pool.used(), 4 + 5 * 7, "every command's status slot came back: MES's four, the two rings' and the five kernel queues' five each");
}

/// The Reload hook's side of the card.
struct Regs<'a>(&'a mut Model, Vec<(u32, u32)>);

impl OffIo for Regs<'_> {
    fn read(&mut self, byte: u32) -> Option<u32> {
        self.0.reg_read(byte)
    }
    fn write(&mut self, byte: u32, v: u32) -> bool {
        self.1.push((byte, v));
        self.0.reg_write(byte, v)
    }
    fn delay_us(&mut self, _us: u64) {}
}

#[test]
fn a_reload_halts_mes_as_linux_does_once_the_stage_set_out_to_write_it() {
    let rig = Rig::new();
    let n = super::rdna4nbmodel::n(&rig);
    let cntl = n.byte(mes12::CP_MES_CNTL).unwrap();
    // Before the stage set out: nothing read, nothing written.
    let (_, mut m) = before_mes(&rig);
    let w = Written::default();
    let mut io = Regs(&mut m, Vec::new());
    assert_eq!(mesoff::halt(&mut io, &n, &w), Halted::Untouched);
    assert!(io.1.is_empty());
    // After both stages: both pipes inactive, the cache invalidated, both resets and HALT (0x40030010).
    let (_, mut m, _) = up(&rig);
    w.set();
    let mut io = Regs(&mut m, Vec::new());
    assert_eq!(mesoff::halt(&mut io, &n, &w), Halted::Halted { was: 0x0C00_0000 });
    assert_eq!(io.1, [(cntl, 0x4003_0010)]);
    assert!(m.refused.is_empty() && !m.mes.running[0] && !m.mes.running[1] && m.mes.queue == [None, None]);
    // The session armed its word at the stage's first write: the stage sets it.
    let (mut s, m) = before_mes(&rig);
    let word = Written::default();
    s.mes.reload = Some(word.clone());
    let (d, _) = rig.run(&mut s, Stage::Mes, m, &mut Stick::full());
    assert!(d.pass && word.get());
}
