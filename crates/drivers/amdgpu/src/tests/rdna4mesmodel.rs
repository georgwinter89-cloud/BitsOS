//! Wave 44, item ME: what the model's MES refuses on its own account, whatever a driver sends: frames that are
//! not the header's, commands Linux's code for this card never sends, commands out of Linux's order, queues
//! that are not the kernel's or are mapped twice, registers it does not know; and at the registers, a pipe or
//! a queue started otherwise than Linux starts it. Each refusal is named in the model's list; a refused command
//! gets the header's failed status, a frame the firmware cannot read gets none.

use super::rdna4fix::*;
use super::rdna4mes::{before_mes, up};
use crate::gpucore::psp::Hw;
use crate::rdna4::fakemodel::Model;
use crate::rdna4::host::Host;
use crate::rdna4::mesring::{Sent, lay};
use crate::rdna4::messtage::Up;
use crate::rdna4::stage::Session;
use rdna4_core::mes12::{self, Pipe};
use rdna4_core::mes12api::{self as api, Legacy, Misc, Unmap, at, op, qtype};
use std::string::String;
use std::vec::Vec;

/// A frame on a pipe's ring with its trailing QUERY, as `Port::submit` would put it, whatever the frame holds.
fn raw(up: &mut Up, m: &mut Model, pipe: Pipe, frame: &[u32]) {
    let p = up.place;
    let port = &mut up.ports[pipe.index()];
    port.seq += 1;
    let mut q = api::query_status();
    api::set_status(&mut q, at::QUERY_STATUS, p.slot_mc(port.slots.fence), u64::from(port.seq));
    for d in [frame, &q] {
        let at = (port.wptr % u64::from(mes12::RING_DW)) * 4;
        let bytes: Vec<u8> = d.iter().flat_map(|w| w.to_le_bytes()).collect();
        assert!(m.sys_write(p.sys + lay::ring(pipe.index()) + at, &bytes));
        port.wptr += d.len() as u64;
    }
    assert!(m.sys_write(p.slot_sys(port.slots.wptr), &port.wptr.to_le_bytes()));
    assert!(m.doorbell_write64(pipe.doorbell(), port.wptr), "{:?}", m.refused);
}

/// A command as the driver sends it (`Port::submit`): how it went, and what the model refused.
fn send(up: &mut Up, m: &mut Model, pipe: Pipe, mut frame: Vec<u32>) -> (Sent, Vec<String>) {
    m.refused.clear();
    let sent = up.ports[pipe.index()].submit(m, &up.place, &mut up.pool, &mut frame).unwrap();
    (sent, core::mem::take(&mut m.refused))
}

fn refused_with(r: (Sent, Vec<String>), want: &str) {
    assert!(matches!(r.0, Sent::Failed { status, .. } if status as u32 == 0 && status >> 63 == 1), "{:?}", r.0);
    assert!(r.1.len() == 1 && r.1[0].contains(want), "{want}: {:?}", r.1);
}

fn ready(rig: &Rig) -> (Session, Model) {
    let (s, m, _) = up(rig);
    (s, m)
}

#[test]
fn a_frame_that_is_not_the_headers_or_never_sent_by_linux_is_refused() {
    let rig = Rig::new();
    let (mut s, mut m) = ready(&rig);
    let up = s.mes.up.as_mut().unwrap();
    // Another size, another type: no status (the firmware cannot place it), the driver's wait finds 0.
    for header in [api::header(op::QUERY_SCHEDULER_STATUS) & !0xFF000 | (32 << 12), api::header(op::QUERY_SCHEDULER_STATUS) & !0xF | 2] {
        let mut f = api::query_status();
        f[0] = header;
        let (sent, refused) = send(up, &mut m, Pipe::Sched, f);
        assert!(matches!(sent, Sent::Failed { status: 0, .. }), "{sent:?}");
        assert!(refused.len() == 1 && refused[0].contains("Linux sends scheduler frames of 64 dwords only"), "{refused:?}");
    }
    // Commands this card's Linux never sends.
    for opcode in [op::SET_SCHEDULING_CONFIG, op::PERFORM_YIELD, op::SUSPEND, op::RESUME, op::RESET, op::SET_LOG_BUFFER, op::SET_DEBUG_VMID, op::UPDATE_ROOT_PAGE_TABLE, op::SET_GANG_SUBMIT, op::INV_TLBS, 0xFE] {
        m.refused.clear();
        raw(up, &mut m, Pipe::Sched, &api::frame(opcode));
        assert!(m.refused.len() == 1 && m.refused[0].contains("a command Linux's code for this card never sends"), "{opcode}: {:?}", m.refused);
    }
    // Misc ops it never sends, and MISC on the scheduler pipe.
    let mut inv = api::frame(op::MISC);
    inv[at::MISC_OPCODE] = api::misc::INV_GART;
    refused_with(send(up, &mut m, Pipe::Kiq, inv), "a misc command Linux's code for this card never sends");
    refused_with(send(up, &mut m, Pipe::Sched, api::misc_frame(Misc::ChangeConfig { limit_single_process: false })), "MISC on the scheduler pipe");
    refused_with(send(up, &mut m, Pipe::Kiq, api::misc_frame(Misc::ChangeConfig { limit_single_process: true })), "CHANGE_CONFIG");
    let mut sub = api::query_status();
    sub[at::QUERY_SUBOPCODE] = 1;
    refused_with(send(up, &mut m, Pipe::Sched, sub), "Linux sends 0");
    // After all of it MES still answers a command of Linux's.
    let (sent, refused) = send(up, &mut m, Pipe::Sched, api::query_status());
    assert!(sent.ok() && refused.is_empty());
}

#[test]
fn commands_out_of_linuxs_order_or_with_other_fields_are_refused() {
    let rig = Rig::new();
    let (mut s, mut m) = ready(&rig);
    let up = s.mes.up.as_mut().unwrap();
    // SET_HW_RESOURCES a second time on either pipe.
    let res = api::HwRes { sched: true, sch_ctx: up.place.mc + lay::WB, query_fence: up.place.mc + lay::WB, gc_base: m.mes.bases[0], mmhub_base: m.mes.bases[1], osssys_base: m.mes.bases[2], oversubscription_timer: 50 };
    refused_with(send(up, &mut m, Pipe::Sched, api::set_hw_resources(&res)), "SET_HW_RESOURCES a second time");
    // SET_HW_RESOURCES only with Linux's fields: another flag word, another mask, another base, another timer.
    for (k, v) in [(at::HW_FLAGS, 0x0010_0447 | 1 << 18), (at::HW_COMPUTE_HQD, 0x0F), (at::HW_GC_BASE, 0x1261), (at::HW_OVERSUB_TIMER, 0)] {
        let mut f = api::set_hw_resources(&res);
        f[k] = v;
        refused_with(send(up, &mut m, Pipe::Sched, f), &std::format!("with dword {k} "));
    }
    // Before SET_HW_RESOURCES took on a pipe nothing else is taken there, its trailing QUERY neither: no fence.
    let saved = m.mes.hw[0].take();
    let (sent, refused) = send(up, &mut m, Pipe::Sched, api::query_status());
    assert!(matches!(sent, Sent::NoAnswer { polls: 1_050_000, .. }) && refused.len() == 2 && refused.iter().all(|r| r.contains("before SET_HW_RESOURCES took on the scheduler pipe")), "{sent:?} {refused:?}");
    m.mes.hw[0] = saved;
    // The ring's fence stands one behind: Linux's next command waits for the one 15 before it, which came.
    up.ports[0].seq -= 1;
    // SET_HW_RESOURCES_1 with another timeout or a fence outside VRAM.
    let mut one = api::set_hw_resources_1(0x1000);
    refused_with(send(up, &mut m, Pipe::Kiq, one.clone()), "Linux gives 0xa and a VRAM page");
    one = api::set_hw_resources_1(s.layout.as_ref().unwrap().mc(s.mes.resource1.clone().unwrap().start));
    one[at::HW1_KIQ_UNMAP_TIMEOUT] = 5;
    let up = s.mes.up.as_mut().unwrap();
    refused_with(send(up, &mut m, Pipe::Kiq, one), "Linux gives 0xa and a VRAM page");
}

#[test]
fn only_the_kernels_queues_are_mapped_once_on_their_pipe_and_on_linuxs_mqd() {
    let rig = Rig::new();
    let (mut s, mut m) = ready(&rig);
    let up = s.mes.up.as_mut().unwrap();
    let q0 = crate::rdna4::meskq::KernelQueue::of(up, 0).unwrap();
    let legacy = q0.legacy(&up.place);
    // Mapped already.
    refused_with(send(up, &mut m, Pipe::Sched, api::add_legacy(&legacy)), "mapped already");
    // Unmapped as Linux unmaps it, it can be mapped again, once.
    let (sent, _) = crate::rdna4::meskq::unmap_legacy(up, &mut m, &legacy, Unmap::ResetQueues, (0, 0)).unwrap();
    assert!(sent.ok() && m.cp.queues.len() == 4 && !m.cp.queues.iter().any(|q| q.at.doorbell_dw == 6), "{:?}", m.refused);
    refused_with(send(up, &mut m, Pipe::Sched, api::remove_legacy(&legacy, Unmap::ResetQueues, 0x97, (0, 0))), "which MES did not map");
    // A kernel queue on the KIQ pipe, without map_legacy_kq, with a process field, at a doorbell or place that is
    // no kernel queue's, inside the queues SET_HW_RESOURCES handed MES, on an MQD that is not bound.
    refused_with(send(up, &mut m, Pipe::Kiq, api::add_legacy(&legacy)), "mapped on the KIQ pipe");
    let mut f = api::add_legacy(&legacy);
    f[at::ADD_FLAGS] = 0;
    refused_with(send(up, &mut m, Pipe::Sched, f), "map_legacy_kq alone");
    let mut f = api::add_legacy(&legacy);
    f[at::ADD_PROCESS_ID] = 7;
    refused_with(send(up, &mut m, Pipe::Sched, f), "every process and gang field 0");
    refused_with(send(up, &mut m, Pipe::Sched, api::add_legacy(&Legacy { doorbell: 0x20, ..legacy })), "none of the kernel's queues");
    refused_with(send(up, &mut m, Pipe::Sched, api::add_legacy(&Legacy { queue: 2, ..legacy })), "which SET_HW_RESOURCES handed MES itself (mask 0xc)");
    let unbound = up.place.mc + lay::PIECE;
    refused_with(send(up, &mut m, Pipe::Sched, api::add_legacy(&Legacy { mqd: unbound, ..legacy })), "that GFXHUB's walker does not find");
    refused_with(send(up, &mut m, Pipe::Sched, api::add_legacy(&Legacy { queue_type: qtype::SCHQ, queue: 1, ..legacy })), "Linux maps it on the KIQ pipe");
    refused_with(send(up, &mut m, Pipe::Sched, api::add_legacy(&Legacy { queue_type: qtype::SDMA, ..legacy })), "Linux maps gfx, compute and the scheduler's queue here");
    // An MQD that does not name the queue: its doorbell control spoiled in memory.
    let at_bell = q0.mqd_sys + 4 * rdna4_core::mqd12::c::HQD_PQ_DOORBELL_CONTROL as u64;
    assert!(m.sys_write(at_bell, &0u32.to_le_bytes()));
    refused_with(send(up, &mut m, Pipe::Sched, api::add_legacy(&legacy)), "not the queue ADD_QUEUE names");
    let wrong_wptr = Legacy { wptr: legacy.wptr + 8, ..legacy };
    let good = rdna4_core::mqd12::compute(&q0.prop(&up.place, 0));
    let bytes: Vec<u8> = good.iter().flat_map(|w| w.to_le_bytes()).collect();
    assert!(m.sys_write(q0.mqd_sys, &bytes));
    refused_with(send(up, &mut m, Pipe::Sched, api::add_legacy(&wrong_wptr)), "the MQD polls");
    // With Linux's MQD and fields it is taken again and handed to the command processor's model.
    let (sent, refused) = send(up, &mut m, Pipe::Sched, api::add_legacy(&legacy));
    assert!(sent.ok() && refused.is_empty() && m.cp.queues.len() == 5, "{refused:?}");
}

#[test]
fn the_registers_it_writes_or_waits_on_are_only_the_hubs_and_known_ones() {
    let rig = Rig::new();
    let (mut s, mut m) = ready(&rig);
    let n = super::rdna4nbmodel::n(&rig);
    let up = s.mes.up.as_mut().unwrap();
    refused_with(send(up, &mut m, Pipe::Kiq, api::misc_frame(Misc::WriteWait { reg0: 0x1000, reg1: 0x1001, reference: 1, mask: 1 })), "a hub's engine 17 request and acknowledge only");
    refused_with(send(up, &mut m, Pipe::Kiq, api::misc_frame(Misc::ReadReg { reg: 0x7FFF_F000, buffer: up.place.mc + lay::WB })), "a register the model does not know");
    refused_with(send(up, &mut m, Pipe::Kiq, api::misc_frame(Misc::ReadReg { reg: n.byte(mes12::CP_UNMAPPED_DOORBELL).unwrap() / 4, buffer: up.place.mc + lay::PIECE })), "where GFXHUB's walker finds nothing");
    // An acknowledge that never comes: MES gives up, and says so in its status.
    m.gc.ack_never = true;
    let req = n.byte(rdna4_core::nbreg::member(rdna4_core::hub41::GC.eng0_req, rdna4_core::hub41::GART_ENGINE, 1)).unwrap();
    let ack = n.byte(rdna4_core::nbreg::member(rdna4_core::hub41::GC.eng0_ack, rdna4_core::hub41::GART_ENGINE, 1)).unwrap();
    refused_with(send(up, &mut m, Pipe::Kiq, api::misc_frame(Misc::WriteWait { reg0: req / 4, reg1: ack / 4, reference: rdna4_core::hub41::invalidate_req(0), mask: 1 })), "never showed 0x1");
}

#[test]
fn the_pipes_and_the_kiq_queue_start_only_as_linux_starts_them() {
    let rig = Rig::new();
    let n = super::rdna4nbmodel::n(&rig);
    let b = |r| n.byte(r).unwrap();
    let (grbm, cntl, pc) = (b(mes12::GRBM_GFX_CNTL), b(mes12::CP_MES_CNTL), b(mes12::CP_MES_PRGRM_CNTR_START));
    let refused = |m: &mut Model, byte: u32, v: u32, want: &str| {
        m.refused.clear();
        assert!(!m.reg_write(byte, v), "{want}: taken");
        assert!(m.refused.len() == 1 && m.refused[0].contains(want), "{want}: {:?}", m.refused);
    };
    let (_, mut m) = before_mes(&rig);
    // A program counter under selection 0, a pipe started without its reset or its program counter, a step of
    // CP_MES_CNTL that is no step of Linux's, the KIQ pipe before the scheduler pipe.
    refused(&mut m, pc, 0xFC00_1400, "not a MES pipe");
    assert!(m.reg_write(grbm, mes12::mes_select(Pipe::Sched)));
    refused(&mut m, cntl, mes12::active(Pipe::Sched), "before its reset bit was set and its program counter written");
    refused(&mut m, cntl, 0x1234, "not one of Linux's steps");
    assert!(m.reg_write(grbm, mes12::mes_select(Pipe::Kiq)));
    assert!(m.reg_write(cntl, 0x4002_0000) && m.reg_write(pc, 1) && m.reg_write(b(mes12::CP_MES_PRGRM_CNTR_START_HI), 1));
    refused(&mut m, cntl, mes12::active(Pipe::Kiq), "before the scheduler pipe");
    // The KIQ queue's registers out of Linux's order, with another value, or under another selection.
    refused(&mut m, b(mes12::CP_MQD_BASE_ADDR), 0x1000, "where Linux writes CP_HQD_VMID");
    refused(&mut m, b(mes12::CP_HQD_VMID), 3, "where Linux writes 0x0");
    assert!(m.reg_write(grbm, 0));
    refused(&mut m, b(mes12::CP_HQD_VMID), 0, "Linux writes the HQD registers of the KIQ pipe only");
    // Doorbells: of a pipe whose queue is not on.
    m.refused.clear();
    assert!(!m.doorbell_write64(Pipe::Kiq.doorbell(), 0x80) && m.refused[0].contains("queue not on"), "{:?}", m.refused);
    // The doorbells once MES runs: a value the slot does not hold, a part of a frame.
    let (mut s, mut m) = ready(&rig);
    let up = s.mes.up.as_mut().unwrap();
    let wptr = up.ports[1].wptr;
    m.refused.clear();
    assert!(!m.doorbell_write64(Pipe::Kiq.doorbell(), wptr + 64) && m.refused[0].contains("Linux writes the slot first, whole frames"), "{:?}", m.refused);
    // The unmapped and aggregated doorbells only as Linux sets them.
    refused(&mut m, b(mes12::CP_UNMAPPED_DOORBELL), 0xFFFF, "Linux sets PROC_LSB 0xd and ENABLE");
    refused(&mut m, b(mes12::doorbell_control(2)), 0x4000_2000, "level 2's aggregated doorbell 0x804");
    refused(&mut m, b(mes12::CP_MES_GP3_LO), 1, "Linux only reads it");
}

#[test]
fn a_frame_on_a_page_gfxhub_does_not_find_faults_and_the_pipe_stops() {
    let rig = Rig::new();
    let (mut s, mut m) = ready(&rig);
    let up = s.mes.up.as_mut().unwrap();
    // The scheduler ring's queue moved onto a page outside every bind (the model's own record, as a ring that
    // ran past what was bound would read).
    let q = m.mes.queue[0].as_mut().unwrap();
    q.ring_mc = up.place.mc + lay::PIECE;
    raw(up, &mut m, Pipe::Sched, &api::query_status());
    assert!(m.refused.iter().any(|r| r.contains("that GFXHUB's walker does not find: the pipe stops")), "{:?}", m.refused);
    assert!(m.faults_open()[0] && !m.mes.running[0], "the fault is latched in GFXHUB's status and the pipe stopped");
}

#[test]
fn the_kiq_queue_is_taken_only_500_us_after_both_pipes_started() {
    let rig = Rig::new();
    let n = super::rdna4nbmodel::n(&rig);
    let b = |r| n.byte(r).unwrap();
    let (_, mut m) = before_mes(&rig);
    for (pipe, reset) in [(Pipe::Sched, mes12::f::MES_PIPE0_RESET), (Pipe::Kiq, mes12::f::MES_PIPE1_RESET)] {
        let v = reset.put(m.mes.cntl, 1);
        assert!(m.reg_write(b(mes12::GRBM_GFX_CNTL), mes12::mes_select(pipe)) && m.reg_write(b(mes12::CP_MES_CNTL), v));
        assert!(m.reg_write(b(mes12::CP_MES_PRGRM_CNTR_START), 0xFC00_1400) && m.reg_write(b(mes12::CP_MES_PRGRM_CNTR_START_HI), 0x3FFF_FFFF));
        assert!(m.reg_write(b(mes12::CP_MES_CNTL), mes12::active(pipe)), "{:?}", m.refused);
    }
    // The KIQ pipe's first HQD register right after the start, and 1 us too early.
    for wait in [0, 499] {
        m.delay_us(wait);
        m.refused.clear();
        assert!(!m.reg_write(b(mes12::CP_HQD_VMID), 0));
        assert!(m.refused.len() == 1 && m.refused[0].contains("before 500 us passed since both MES pipes started") && m.refused[0].contains("mes_v12_0.c:1353-1356"), "{:?}", m.refused);
    }
    m.delay_us(1);
    assert!(m.reg_write(b(mes12::CP_HQD_VMID), 0), "{:?}", m.refused);
}

/// The scheduler's own queue as the stage maps it on the KIQ ring.
fn schq(up: &Up) -> Legacy {
    Legacy { queue_type: qtype::SCHQ, pipe: 0, queue: 0, doorbell: Pipe::Sched.doorbell(), mqd: up.place.mc + lay::mqd(0), wptr: up.place.slot_mc(up.ports[0].slots.wptr) }
}

/// One dword of an MQD in system memory changed by `xor`; the old value.
fn spoil(m: &mut Model, mqd_sys: u64, dword: usize, xor: u32) -> u32 {
    let at = mqd_sys + 4 * dword as u64;
    let mut b = [0u8; 4];
    assert!(m.sys_read(at, &mut b));
    let old = u32::from_le_bytes(b);
    assert!(m.sys_write(at, &(old ^ xor).to_le_bytes()));
    old
}

#[test]
fn a_queue_is_unmapped_only_on_its_pipe_with_linuxs_flags_type_and_firmware() {
    let rig = Rig::new();
    let (mut s, mut m) = ready(&rig);
    let up = s.mes.up.as_mut().unwrap();
    let q0 = crate::rdna4::meskq::KernelQueue::of(up, 0).unwrap();
    let legacy = q0.legacy(&up.place);
    let reset = || api::remove_legacy(&legacy, Unmap::ResetQueues, 0x97, (0, 0));
    refused_with(send(up, &mut m, Pipe::Kiq, reset()), "REMOVE_QUEUE on the KIQ pipe: Linux unmaps this queue on the scheduler pipe");
    let mut f = reset();
    f[at::REMOVE_FLAGS] = 0;
    refused_with(send(up, &mut m, Pipe::Sched, f), "not Linux's unmap of a kernel queue");
    let mut f = reset();
    f[at::REMOVE_QUEUE_TYPE] = qtype::GFX;
    refused_with(send(up, &mut m, Pipe::Sched, f), "not Linux's unmap of a kernel queue");
    // RESET_QUEUES after a reset is taken only from firmware 0x5a on (mes_v12_0.c:770-772).
    m.mes.gp3 = (mes12::GP3_OF_THIS_CARD & !0xFFF) | (mes12::REMOVE_AFTER_RESET_FROM - 1);
    refused_with(send(up, &mut m, Pipe::Sched, reset()), "not Linux's unmap of a kernel queue");
    m.mes.gp3 = mes12::GP3_OF_THIS_CARD;
    let (sent, refused) = send(up, &mut m, Pipe::Sched, reset());
    assert!(sent.ok() && refused.is_empty() && m.cp.queues.len() == 4, "{refused:?}");
    // Mapped again only on Linux's compute MQD: its header spoiled, then put back.
    let old = spoil(&mut m, q0.mqd_sys, rdna4_core::mqd12::c::HEADER, 1);
    refused_with(send(up, &mut m, Pipe::Sched, api::add_legacy(&legacy)), "a compute MQD with header");
    spoil(&mut m, q0.mqd_sys, rdna4_core::mqd12::c::HEADER, 1);
    assert_eq!(spoil(&mut m, q0.mqd_sys, rdna4_core::mqd12::c::HEADER, 0), old);
    let (sent, refused) = send(up, &mut m, Pipe::Sched, api::add_legacy(&legacy));
    assert!(sent.ok() && refused.is_empty(), "{refused:?}");
}

#[test]
fn the_schedulers_queue_is_mapped_only_on_linuxs_mqd() {
    let rig = Rig::new();
    let (mut s, mut m) = ready(&rig);
    let up = s.mes.up.as_mut().unwrap();
    let q = schq(up);
    let (sent, line) = crate::rdna4::meskq::unmap_legacy(up, &mut m, &q, Unmap::ResetQueues, (0, 0)).unwrap();
    assert!(sent.ok() && m.mes.queue[Pipe::Sched.index()].is_none(), "{line} {:?}", m.refused);
    // Each dword `mes_v12_0_mqd_init` sets that the model holds it to, one wrong at a time.
    let sys = up.place.sys + lay::mqd(0);
    use rdna4_core::mqd12::c;
    for dword in [c::HEADER, c::MQD_BASE_ADDR_LO, c::HQD_PQ_DOORBELL_CONTROL, c::HQD_PQ_CONTROL, c::HQD_ACTIVE, c::RESERVED_184] {
        let wrong = spoil(&mut m, sys, dword, 0x10) ^ 0x10;
        refused_with(send(up, &mut m, Pipe::Kiq, api::add_legacy(&q)), &std::format!("the scheduler's MQD holds {wrong:#x} at dword {dword},"));
        spoil(&mut m, sys, dword, 0x10);
    }
    refused_with(send(up, &mut m, Pipe::Kiq, api::add_legacy(&Legacy { wptr: q.wptr + 8, ..q })), "the MQD polls");
    let (sent, refused) = send(up, &mut m, Pipe::Kiq, api::add_legacy(&q));
    assert!(sent.ok() && refused.is_empty() && m.mes.queue[Pipe::Sched.index()].is_some(), "{refused:?}");
}

#[test]
fn set_hw_resources_naming_a_slot_gfxhub_does_not_find_is_refused() {
    let rig = Rig::new();
    let (mut s, mut m) = ready(&rig);
    let up = s.mes.up.as_mut().unwrap();
    let (good, unbound) = (up.place.mc + lay::WB, up.place.mc + lay::PIECE);
    let saved = m.mes.hw[0].take();
    for (sch_ctx, query_fence) in [(unbound, good), (good, unbound)] {
        let res = api::HwRes { sched: true, sch_ctx, query_fence, gc_base: m.mes.bases[0], mmhub_base: m.mes.bases[1], osssys_base: m.mes.bases[2], oversubscription_timer: 50 };
        // Refused, and with SET_HW_RESOURCES not taken its trailing QUERY neither: no fence.
        let (sent, refused) = send(up, &mut m, Pipe::Sched, api::set_hw_resources(&res));
        assert!(matches!(sent, Sent::NoAnswer { .. }) && m.mes.hw[0].is_none(), "{sent:?}");
        assert!(refused[0].contains(&std::format!("SET_HW_RESOURCES names a slot at {unbound:#x} that GFXHUB's walker does not find")), "{refused:?}");
        up.ports[0].seq -= 1;
    }
    m.mes.hw[0] = saved;
}
