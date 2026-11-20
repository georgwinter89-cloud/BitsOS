//! Wave 44, item ME: MES's two stages on the model (`rdna4::fakemes`). They equal the trace's section `mes` with
//! five declared rules and nothing else; every command is the header's struct with Linux's fields; the KIQ
//! queue's MQD holds what its registers say; the kernel's five queues reach the command processor's model; a command
//! that gets no answer stops the stage where Linux's does and only there. The model's refusals are
//! `rdna4mesmodel`, the flush through MES and the API for the rings `rdna4mesapi`.

use super::rdna4fix::*;
use super::rdna4nb::regs;
use super::rdna4sdma::chain_before;
use crate::gpucore::trail::Access;
use crate::rdna4::fakemodel::Model;
use crate::rdna4::host::Host;
use crate::rdna4::linuxseq::compare::Compare;
use crate::rdna4::linuxseq::fold::Bus;
use crate::rdna4::linuxseq::rules::Rule;
use crate::rdna4::linuxseq::section;
use crate::rdna4::mesring::lay;
use crate::rdna4::messeq;
use crate::rdna4::stage::{Done, Session, Stage};
use rdna4_core::mes12::{self, Pipe};
use rdna4_core::mes12api::{self as api, at, op};
use rdna4_core::mqd12;
use std::vec::Vec;

/// The chain on a cold model up to the stage before `mes`: GFXHUB on, the graphics block powered.
pub fn before_mes(rig: &Rig) -> (Session, Model) {
    let mut s = Session::default();
    let (_, mut m) = rig.pass(&mut s, model(), &mut Stick::full(), chain_before(Stage::Mes));
    m.writes.clear();
    (s, m)
}

/// `before_mes`, then both stages.
pub fn up(rig: &Rig) -> (Session, Model, [Done; 2]) {
    let (mut s, m) = before_mes(rig);
    let (d, m) = rig.pass(&mut s, m, &mut Stick::full(), &[Stage::Mes, Stage::MesQ]);
    let [a, b]: [Done; 2] = d.try_into().unwrap();
    (s, m, [a, b])
}

fn opcodes(m: &Model) -> Vec<(usize, u32)> {
    m.mes.frames.iter().map(|(p, d)| (*p, api::header_fields(d[0]).1)).collect()
}

#[test]
fn both_stages_equal_the_section_mes_with_five_declared_rules() {
    let rig = Rig::new();
    let (s, m, [mes, mesq]) = up(&rig);
    let sec = section("mes").unwrap();
    let rules = messeq::rules(s.mes.kiq).unwrap();
    assert_eq!((sec.lines.len(), sec.lines[0].init, sec.lines.last().unwrap().init), (64, 9502, 9566));
    let mut c = Compare::new(&sec, &rules).unwrap();
    let bus = Bus::of(&regs(&rig));
    c.feed_trail(&mes.trail, mes.trail_lost, &bus).unwrap_or_else(|e| panic!("{e}\n{}", mes.lines.join("\n")));
    assert_eq!(c.next_line(), Some(9554), "mes ends where the kernel queues' selections begin");
    c.feed_trail(&mesq.trail, mesq.trail_lost, &bus).unwrap_or_else(|e| panic!("{e}\n{}", mesq.lines.join("\n")));
    c.finish().unwrap_or_else(|e| panic!("{e}"));
    // One bind (by registers, both hubs: MES does not run yet) and the KIQ queue's four addresses, each exactly
    // its own object: the MQD page, the ring, the two slots of the KIQ ring.
    assert_eq!(rules.len(), 5);
    let up = s.mes.up.as_ref().unwrap();
    let (p, kiq) = (up.place, &up.ports[1]);
    let objects = [(p.mc + lay::mqd(1), u64::from(mqd12::MQD_BYTES)), (p.mc + lay::ring(1), u64::from(mes12::RING_BYTES)), (p.slot_mc(kiq.slots.rptr), 8), (p.slot_mc(kiq.slots.wptr), 8)];
    assert!(matches!(rules[0], Rule::BindBothBefore { reg: "GRBM_GFX_CNTL", .. }));
    for (r, (at, bytes)) in rules[1..].iter().zip(objects) {
        assert!(matches!(r, Rule::Address(a) if a.hi.is_some() && a.within == (at..at + bytes) && a.bytes == bytes), "{r:?}");
    }
    assert_eq!(messeq::rules(None).unwrap_err(), "the stage mes has not bound MES's piece in this boot, so the KIQ queue's addresses have no rule");
    assert_eq!(mes.lines.last().unwrap(), "linuxseq mes: equal so far (Linux goes on at init.txt line 9554)");
    assert_eq!(mesq.lines.last().unwrap(), "linuxseq mes: equal");
    assert!(mes.line.ends_with("Planer der Karte starten: fertig.") && mesq.line.ends_with("Warteschlangen beim Planer anmelden: fertig."), "{} {}", mes.line, mesq.line);
    // Linux's 52 register lines of hw_init (the HQD registers' and the doorbells' reads included) and the bind's
    // twelve; the kernel queues' ten selections and two HDP flushes. No sleep of a millisecond: the 500 us after
    // the pipes' start are a delay the trace shows as silence.
    assert!(mes.trail.iter().all(|a| !matches!(a, Access::Sleep { .. })));
    assert_eq!(mesq.trail.iter().filter(|a| matches!(a, Access::Write { .. })).count(), 10 + 2);
    // The commands, in Linux's order, each followed by its QUERY_SCHEDULER_STATUS.
    let pairs: Vec<(usize, u32)> = opcodes(&m).chunks(2).map(|c| c[0]).collect();
    assert!(opcodes(&m).chunks(2).all(|c| c[1].1 == op::QUERY_SCHEDULER_STATUS && c[1].0 == c[0].0));
    let (k, sch) = (Pipe::Kiq.index(), Pipe::Sched.index());
    assert_eq!(pairs, [(k, op::SET_HW_RSRC), (k, op::SET_HW_RSRC_1), (k, op::ADD_QUEUE), (sch, op::SET_HW_RSRC), (sch, op::SET_HW_RSRC_1), (sch, op::QUERY_SCHEDULER_STATUS), (k, op::MISC), (sch, op::ADD_QUEUE), (sch, op::ADD_QUEUE), (sch, op::ADD_QUEUE), (sch, op::ADD_QUEUE), (sch, op::ADD_QUEUE)]);
    // Every command's line says it was answered; the session holds MES running and the queues mapped.
    let answered: Vec<&str> = mes.lines.iter().chain(&mesq.lines).filter(|l| l.starts_with("MES(")).map(|l| l.as_str()).collect();
    assert!(answered.len() == 12 && answered.iter().all(|l| l.contains(": answered after 0 polls of its fence, status 0x1")), "{answered:?}");
    assert!(s.mes.ready() && s.mes.kq && s.passed(Stage::Mes) && s.passed(Stage::MesQ));
    let up = s.mes.up.as_ref().unwrap();
    assert_eq!((up.sched_version, up.kiq_version, up.cleaner_shader, up.ports[0].seq, up.ports[1].seq), (mes12::GP3_OF_THIS_CARD, mes12::GP3_OF_THIS_CARD, true, 3 + 5, 4));
    assert!(mes.lines.iter().any(|l| l == "mes: kiq version 0x1029097, scheduler version 0x1029097 (firmware 0x97, API 0x29, the header's API 0x14); cleaner shader on: CHANGE_CONFIG sent (ME 0xc26, PFP 0xc94, MEC 0xda2, MES 0x97); the file's own version dword (amdgpu_mes_validate_fw_version) is in its body and not read"), "{:?}", mes.lines);
    // Both pipes run; the fence of each ring stands at its last sequence number.
    assert!(m.mes.runs(Pipe::Sched) && m.mes.runs(Pipe::Kiq) && m.refused.is_empty());
    let mut m = m;
    for (i, port) in up.ports.iter().enumerate() {
        let mut b = [0u8; 4];
        assert!(m.sys_read(up.place.slot_sys(port.slots.fence), &mut b));
        assert_eq!(u32::from_le_bytes(b), port.seq, "pipe {i}");
    }
}

#[test]
fn each_command_is_the_headers_struct_with_linuxs_fields() {
    let rig = Rig::new();
    let (s, m, _) = up(&rig);
    let up = s.mes.up.as_ref().unwrap();
    let p = up.place;
    let frame = |i: usize| &m.mes.frames[i].1;
    for (_, d) in &m.mes.frames {
        assert_eq!(api::header_fields(d[0]).0, 1);
        assert_eq!(api::header_fields(d[0]).2 as usize, api::FRAME_DW);
    }
    // KIQ's SET_HW_RESOURCES: no masks, no doorbells, its own two slots, the measured table's bases, Linux's flags.
    let kiq = frame(0);
    assert!(kiq[1..at::HW_SCH_CTX].iter().all(|w| *w == 0));
    assert_eq!((kiq[at::HW_FLAGS], kiq[at::HW_OVERSUB_TIMER]), (0x0010_0447, 50));
    assert_eq!(&kiq[at::HW_GC_BASE..at::HW_GC_BASE + 5], &[0x1260, 0xA000, 0x1C000, 0x0240_2C00, 0]);
    assert_eq!(&kiq[at::HW_MMHUB_BASE..at::HW_MMHUB_BASE + 2], &[0x1A000, 0x0240_8800]);
    assert_eq!(&kiq[at::HW_OSSSYS_BASE..at::HW_OSSSYS_BASE + 2], &[0x10A0, 0x0240_A000]);
    let in_wb = |a: u64| a >= p.mc + lay::WB && a < p.mc + lay::WB + 4096 && a % 32 == 0;
    assert!(in_wb(api::get64(kiq, at::HW_SCH_CTX)) && in_wb(api::get64(kiq, at::HW_QUERY_FENCE)) && api::get64(kiq, at::HW_SCH_CTX) != api::get64(kiq, at::HW_QUERY_FENCE));
    // Its completion fence: a slot of the write-back page with the value 1; the QUERY behind it: the ring's fence
    // with the sequence number.
    assert!(in_wb(api::get64(kiq, at::HW_STATUS)) && api::get64(kiq, at::HW_STATUS + 2) == 1);
    assert_eq!((api::get64(frame(1), at::QUERY_STATUS), api::get64(frame(1), at::QUERY_STATUS + 2)), (p.slot_mc(up.ports[1].slots.fence), 1));
    // SET_HW_RESOURCES_1: the unmap timeout and pipe 1's resource_1 page in VRAM.
    let r1 = s.mes.resource1.clone().unwrap();
    let l = s.layout.as_ref().unwrap();
    assert_eq!((frame(2)[at::HW1_KIQ_UNMAP_TIMEOUT], api::get64(frame(2), at::HW1_CLEANER_FENCE)), (0xA, l.mc(r1.start + 4096)));
    // The scheduler's queue mapped on the KIQ ring.
    let add = frame(4);
    assert_eq!((add[at::ADD_FLAGS], add[at::ADD_QUEUE_TYPE], add[at::ADD_DOORBELL], add[at::ADD_PIPE], add[at::ADD_QUEUE]), (0x2000, 3, 0x16, 0, 0));
    assert_eq!((api::get64(add, at::ADD_MQD), api::get64(add, at::ADD_WPTR)), (p.mc + lay::mqd(0), p.slot_mc(up.ports[0].slots.wptr)));
    // The scheduler's SET_HW_RESOURCES: the masks of dmesg 18.11. and the aggregated doorbells.
    let sch = frame(6);
    assert_eq!((sch[at::HW_VMID_MMHUB], sch[at::HW_VMID_GFXHUB], sch[at::HW_COMPUTE_HQD], sch[at::HW_COMPUTE_HQD + 1], sch[at::HW_COMPUTE_HQD + 2], sch[at::HW_GFX_HQD], sch[at::HW_SDMA_HQD]), (0xFF00, 0xFF00, 0x0C, 0x0C, 0, 0xFE, 0xFC));
    assert_eq!(&sch[at::HW_AGG_DOORBELLS..at::HW_AGG_DOORBELLS + 5], &[0x800, 0x802, 0x804, 0x806, 0x808]);
    // _1 on the scheduler pipe names pipe 0's page; CHANGE_CONFIG limits nothing.
    assert_eq!(api::get64(frame(8), at::HW1_CLEANER_FENCE), l.mc(r1.start));
    let cc = frame(12);
    assert_eq!((cc[at::MISC_OPCODE], cc[at::MISC_DATA], cc[at::MISC_CHANGE_CONFIG_OPTION]), (9, 0, 0));
    // The kernel queues: compute pipe/queue (0,0), (1,0), (0,1), (1,1) with doorbells 6..12, then gfx 0x116.
    let kq: Vec<(u32, u32, u32, u32)> = m.mes.frames[14..].chunks(2).map(|c| (c[0].1[at::ADD_QUEUE_TYPE], c[0].1[at::ADD_PIPE], c[0].1[at::ADD_QUEUE], c[0].1[at::ADD_DOORBELL])).collect();
    assert_eq!(kq, [(1, 0, 0, 6), (1, 1, 0, 8), (1, 0, 1, 10), (1, 1, 1, 12), (0, 0, 0, 0x116)]);
}

#[test]
fn the_kiq_queues_mqd_holds_what_its_registers_say() {
    let rig = Rig::new();
    let (s, mut m, _) = up(&rig);
    let up = s.mes.up.as_ref().unwrap();
    let n = super::rdna4nbmodel::n(&rig);
    let reg = |m: &Model, r| m.mes.hqd.get(&n.byte(r).unwrap()).copied().unwrap();
    let mut b = std::vec![0u8; mqd12::MQD_BYTES as usize];
    assert!(m.sys_read(up.place.sys + lay::mqd(1), &mut b));
    let mqd: Vec<u32> = b.chunks(4).map(|w| u32::from_le_bytes(w.try_into().unwrap())).collect();
    use mqd12::c;
    for (field, r) in [
        (c::MQD_BASE_ADDR_LO, mes12::CP_MQD_BASE_ADDR),
        (c::MQD_BASE_ADDR_HI, mes12::CP_MQD_BASE_ADDR_HI),
        (c::HQD_PQ_BASE_LO, mes12::CP_HQD_PQ_BASE),
        (c::HQD_PQ_BASE_HI, mes12::CP_HQD_PQ_BASE_HI),
        (c::HQD_PQ_RPTR_REPORT_ADDR_LO, mes12::CP_HQD_PQ_RPTR_REPORT_ADDR),
        (c::HQD_PQ_RPTR_REPORT_ADDR_HI, mes12::CP_HQD_PQ_RPTR_REPORT_ADDR_HI),
        (c::HQD_PQ_CONTROL, mes12::CP_HQD_PQ_CONTROL),
        (c::HQD_PQ_WPTR_POLL_ADDR_LO, mes12::CP_HQD_PQ_WPTR_POLL_ADDR),
        (c::HQD_PQ_WPTR_POLL_ADDR_HI, mes12::CP_HQD_PQ_WPTR_POLL_ADDR_HI),
        (c::HQD_PQ_DOORBELL_CONTROL, mes12::CP_HQD_PQ_DOORBELL_CONTROL),
        (c::HQD_PERSISTENT_STATE, mes12::CP_HQD_PERSISTENT_STATE),
        (c::HQD_ACTIVE, mes12::CP_HQD_ACTIVE),
    ] {
        assert_eq!(mqd[field], reg(&m, r), "{}", r.name);
    }
    // ... and points at the KIQ pipe's ring, MQD and slots in MES's piece; CP_MQD_CONTROL got Linux's literal 0.
    assert_eq!((reg(&m, mes12::CP_HQD_PQ_BASE), reg(&m, mes12::CP_MQD_CONTROL)), (((up.place.mc + lay::ring(1)) >> 8) as u32, 0));
    assert_eq!((mqd[c::HEADER], mqd[c::RESERVED_184], mqd[c::MQD_CONTROL]), (mqd12::HEADER, 0x8000, 0x100));
    // The scheduler's MQD names its own ring and the doorbell 0x16; MES loaded it, the CPU wrote no register of it.
    assert!(m.sys_read(up.place.sys + lay::mqd(0), &mut b));
    let sch: Vec<u32> = b.chunks(4).map(|w| u32::from_le_bytes(w.try_into().unwrap())).collect();
    assert_eq!((sch[c::HQD_PQ_DOORBELL_CONTROL], sch[c::HQD_PQ_BASE_LO]), (0x4000_0058, ((up.place.mc + lay::ring(0)) >> 8) as u32));
    assert_eq!(m.mes.queue[0].unwrap().ring_mc, up.place.mc + lay::ring(0));
}

#[test]
fn both_pipes_start_at_the_files_program_counter() {
    let rig = Rig::new();
    let (_, m, _) = up(&rig);
    let stick = Stick::full();
    let file = &stick.0[&crate::gpucore::fwload::stick_path("amdgpu/gc_12_0_1_uni_mes.bin")];
    let (lo, hi) = crate::rdna4::fwlist::mes_start(file).unwrap();
    let (pc_lo, pc_hi) = mes12::pc_start(lo, hi);
    // `uc_start_addr >> 2` of the file's header (0xffffffff_f0005000 on this card), under each pipe's selection.
    assert_eq!((pc_lo, pc_hi), (0xFC00_1400, 0x3FFF_FFFF));
    assert_eq!(m.mes.pc, [[Some(pc_lo), Some(pc_hi)]; 2]);
}

#[test]
fn the_kiq_queues_address_rules_hold_each_register_to_its_own_object() {
    let rig = Rig::new();
    let (s, _, [mes, _]) = up(&rig);
    let n = super::rdna4nbmodel::n(&rig);
    let (sec, bus) = (section("mes").unwrap(), Bus::of(&regs(&rig)));
    let rules = messeq::rules(s.mes.kiq).unwrap();
    // Each address register moved by one write-back slot (32 bytes): still in the GART and aligned, but not its
    // object (the read pointer's slot would name the write pointer's).
    for r in [mes12::CP_HQD_PQ_RPTR_REPORT_ADDR, mes12::CP_HQD_PQ_WPTR_POLL_ADDR, mes12::CP_MQD_BASE_ADDR] {
        let byte = n.byte(r).unwrap();
        let mut t = mes.trail.clone();
        let w = t.iter_mut().find_map(|a| match a {
            Access::Write { byte: b, value, .. } if *b == byte => Some(value),
            _ => None,
        });
        *w.unwrap() += if r.name == "CP_MQD_BASE_ADDR" { 4096 } else { 32 };
        let mut c = Compare::new(&sec, &rules).unwrap();
        let e = c.feed_trail(&t, 0, &bus).unwrap_err();
        assert!(std::format!("{e}").contains("is not inside"), "{}: {e}", r.name);
    }
}

#[test]
fn the_kernel_queues_reach_the_command_processors_model_with_linuxs_mqds() {
    let rig = Rig::new();
    let (s, mut m, [_, mesq]) = up(&rig);
    let up = s.mes.up.as_ref().unwrap();
    assert_eq!((m.mes.mapped.len(), m.cp.queues.len()), (6, 5));
    for i in 0..5 {
        let k = crate::rdna4::meskq::KernelQueue::of(up, i).unwrap();
        let q = m.cp.queues.iter().find(|q| q.at.doorbell_dw == k.kq.doorbell).map(|q| q.at).unwrap();
        // The CP's model took the queue from its MQD: the ring, its size and the read pointer's report address.
        assert_eq!((q.gfx, q.me, q.pipe, q.queue, q.ring_mc, q.ring_dw), (k.kq.gfx, k.kq.me, k.kq.pipe, k.kq.queue, k.ring_mc, 2048));
        assert_eq!((q.rptr_mc, q.wptr_mc), (up.place.slot_mc(k.slots.rptr), up.place.slot_mc(k.slots.wptr)));
        let mut b = std::vec![0u8; mqd12::MQD_BYTES as usize];
        assert!(m.sys_read(k.mqd_sys, &mut b));
        let mqd: Vec<u32> = b.chunks(4).map(|w| u32::from_le_bytes(w.try_into().unwrap())).collect();
        let want = if k.kq.gfx { mqd12::gfx(&k.prop(&up.place, i)) } else { mqd12::compute(&k.prop(&up.place, i)) };
        assert_eq!(mqd, want, "queue {i}");
    }
    // The first compute queue is the high-priority one; the gfx queue's ring is 8 KiB.
    let mut b = [0u8; 4];
    let k0 = crate::rdna4::meskq::KernelQueue::of(up, 0).unwrap();
    assert!(m.sys_read(k0.mqd_sys + 4 * mqd12::c::HQD_PIPE_PRIORITY as u64, &mut b) && u32::from_le_bytes(b) == 2);
    assert_eq!(mesq.lines.iter().filter(|l| l.starts_with("mesq: ")).count(), 5);
    assert!(mesq.lines.iter().any(|l| l.starts_with("mesq: gfx queue me 0 pipe 0 queue 0") && l.contains("doorbell dword 0x116")), "{:?}", mesq.lines);
}

#[test]
fn the_stages_refuse_before_any_access_what_is_theirs_to_refuse() {
    let rig = Rig::new();
    let untouched = |d: &Done, m: &Model, what: &str| {
        assert!(!d.pass && d.trail.is_empty() && d.totals.writes == 0 && d.refused.is_empty() && m.writes.is_empty() && m.refused.is_empty(), "{what}: {} {:?}", d.line, d.lines);
        assert!(d.lines.iter().all(|l| !l.starts_with("linuxseq ")), "{what}");
    };
    // A second run of each.
    let (mut s, mut m, _) = up(&rig);
    m.writes.clear();
    let (d, m) = rig.run(&mut s, Stage::Mes, m, &mut Stick::full());
    untouched(&d, &m, "a second mes");
    assert!(d.line.contains("der Planer der Karte läuft schon"), "{}", d.line);
    let (d, m) = rig.run(&mut s, Stage::MesQ, m, &mut Stick::full());
    untouched(&d, &m, "a second mesq");
    // MES's four images or the autoload missing in this boot's record.
    for cut in 0..3 {
        let (mut s, m) = before_mes(&rig);
        match cut {
            0 => s.loaded.retain(|l| l.0 != crate::rdna4::fwlist::Id::CpMes1Data),
            1 => s.loaded.retain(|l| l.0 != crate::rdna4::fwlist::Id::CpMes),
            _ => s.autoload = false,
        }
        let (d, m) = rig.run(&mut s, Stage::Mes, m, &mut Stick::full());
        untouched(&d, &m, "no firmware");
        assert!(d.line.contains("zuerst die Steuerprogramme laden") && d.lines.last().unwrap().contains("types 33, 34, 81, 82"), "{}", d.line);
    }
    // No GART, no power on record, no GFXHUB on record, the survey's no, no pages: nothing taken, nothing bound.
    for case in 0..5 {
        let (mut s, mut m) = before_mes(&rig);
        match case {
            0 => s.nb.gart = None,
            1 => s.pmfw.facts.enabled = false,
            2 => s.gc.hub = false,
            3 => m.nb.survey = Err("TranslationOn"),
            _ => m.nb.pages_refused = true,
        }
        let (d, m) = rig.run(&mut s, Stage::Mes, m, &mut Stick::full());
        untouched(&d, &m, "case");
        assert!(!s.mes.tried && s.mes.piece.is_none() && s.nb.gart.as_ref().is_none_or(|g| g.bound.len() == 1), "case {case}");
        let want = ["zuerst den Speicher der Karte einrichten", "zuerst die Energieverwaltung der Karte starten", "zuerst den Speicher des Grafikteils einrichten", "lässt die Karte nicht an den Arbeitsspeicher", "kein Arbeitsspeicher für die Karte frei"][case];
        assert!(d.line.contains(want), "case {case}: {}", d.line);
    }
    // A stick whose MES file is too short for its header: the stage stops before any access.
    let (mut s, m) = before_mes(&rig);
    let mut stick = Stick::full();
    stick.0.get_mut(&crate::gpucore::fwload::stick_path("amdgpu/gc_12_0_1_uni_mes.bin")).unwrap().truncate(40);
    let (d, m) = rig.run(&mut s, Stage::Mes, m, &mut stick);
    untouched(&d, &m, "a short MES file");
    // A second fw once MES wrote its registers: refused before any access (after the engines' own refusal is
    // gone: they did not run here).
    let (mut s, m, _) = up(&rig);
    let (_, mut m) = rig.pass(&mut s, m, &mut Stick::full(), &[Stage::Destroy, Stage::Ring, Stage::Toc, Stage::Tmr]);
    m.writes.clear();
    let (d, m) = rig.run(&mut s, Stage::Fw, m, &mut Stick::full());
    untouched(&d, &m, "a second fw");
    assert!(d.line.contains("der Planer der Karte wurde in diesem Start schon gestartet") && d.lines.last().unwrap().contains("mes_v12_0_kiq_hw_fini"), "{}", d.line);
}

#[test]
fn the_destroy_keeps_mes_and_the_next_ring_finds_it_running() {
    let rig = Rig::new();
    let (mut s, m, _) = up(&rig);
    let (_, mut m) = rig.pass(&mut s, m, &mut Stick::full(), &[Stage::Destroy]);
    assert!(s.mes.ready() && s.mes.kq && s.mes.piece.is_some() && m.mes.runs(Pipe::Kiq) && m.mes.runs(Pipe::Sched));
    // MES still answers: a register read through it (CP_UNMAPPED_DOORBELL as the stage left it).
    let n = super::rdna4nbmodel::n(&rig);
    let up = s.mes.up.as_mut().unwrap();
    let v = crate::rdna4::meskq::rreg(up, &mut m, n.byte(mes12::CP_UNMAPPED_DOORBELL).unwrap() / 4).unwrap();
    assert_eq!(v, Ok(0x0D31));
    assert!(m.refused.is_empty(), "{:?}", m.refused);
}
