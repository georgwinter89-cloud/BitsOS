//! Wave 44, item ME: MES 12's arithmetic against the values Linux wrote on this card (init.txt lines 9502-9553,
//! the trace's section mes) and against the source's own numbers.

use crate::hubseq::Op;
use crate::mes12::{self, Pipe};
use crate::mes12api::{self as api, at, op};
use crate::mqd12::{self, Prop, c};
use std::vec::Vec;

#[test]
fn the_values_mes_v12_0_writes_are_the_traces() {
    // CP_MES_PRGRM_CNTR_START/_HI from the file's start 0xffffffff_f0005000 (init.txt 9505-9506).
    assert_eq!(mes12::pc_start(0xF000_5000, 0xFFFF_FFFF), (0xFC00_1400, 0x3FFF_FFFF));
    assert_eq!(mes12::active(Pipe::Sched), 0x0400_0000);
    assert_eq!(mes12::active(Pipe::Kiq), 0x0C00_0000);
    assert_eq!((mes12::mes_select(Pipe::Sched), mes12::mes_select(Pipe::Kiq)), (0xC, 0xD));
    let Op::Modify(_, reset0) = mes12::enable(Pipe::Sched, (0, 0))[0] else { panic!("a read-modify-write first") };
    let Op::Modify(_, reset1) = mes12::enable(Pipe::Kiq, (0, 0))[0] else { panic!("a read-modify-write first") };
    assert_eq!((reset0(0x4000_0000), reset1(0x0400_0000)), (0x4001_0000, 0x0402_0000));
    // The KIQ queue's registers (9528, 9531, 9532) and the oddity behind 0xd830800c.
    assert_eq!(mes12::mes_pq_control(mes12::RING_BYTES), 0xD830_800C);
    assert_eq!(mes12::doorbell_enabled(Pipe::Kiq.doorbell()), 0x4000_0060);
    assert_eq!(mes12::persistent_state(), 0x0BE0_5501);
    assert_eq!(mes12::unmapped_doorbell(0x0C30), 0x0D31);
    let agg: Vec<u32> = mes12::AGGREGATED_DOORBELLS.iter().map(|d| mes12::aggregated(0, *d)).collect();
    assert_eq!(agg, [0x4000_2000, 0x4000_2008, 0x4000_2010, 0x4000_2018, 0x4000_2020]);
    assert_eq!(mes12::HQD_GFX_CONTROL, 0x8000);
    // Linux's literal 0 for CP_MQD_CONTROL after reading 0x100 (9522-9523).
    let m = mqd12::HqdFromMqd::of(&mqd12::mes(&prop())).unwrap();
    let Op::Modify(_, mqd_control) = mes12::queue_init_register(&m)[4] else { panic!("CP_MQD_CONTROL is read first") };
    assert_eq!(mqd_control(0x100), 0);
    assert_eq!(mes12::halted(0x0C00_0000), 0x4003_0010);
}

fn prop() -> Prop {
    Prop { mqd: 0x2A_9000, ring: 0x2A_A000, ring_bytes: mes12::RING_BYTES, rptr: 0x1740, wptr: 0x1760, eop: 0x2B_2000, doorbell: Pipe::Kiq.doorbell(), high_priority: false }
}

#[test]
fn the_mes_mqd_names_what_the_trace_wrote_into_the_kiq_registers() {
    let m = mqd12::mes(&prop());
    assert_eq!(m.len(), mqd12::MQD_DW);
    let h = mqd12::HqdFromMqd::of(&m).unwrap();
    // Linux's addresses of 18.11. give Linux's register values (init.txt 9520-9533).
    assert_eq!((h.mqd_lo, h.mqd_hi, h.pq_base_lo, h.pq_base_hi), (0x002A_9000, 0, 0x2AA0, 0));
    assert_eq!((h.rptr_lo, h.rptr_hi, h.wptr_lo, h.wptr_hi), (0x1740, 0, 0x1760, 0));
    assert_eq!((h.pq_control, h.doorbell, h.persistent, h.active), (0xD830_800C, 0x4000_0060, 0x0BE0_5501, 1));
    assert_eq!((m[c::HEADER], m[c::RESERVED_184], m[c::HQD_EOP_CONTROL]), (mqd12::HEADER, 0x8000, 0x08));
}

#[test]
fn the_kernel_queues_mqds_are_gfx_v12_0s() {
    let p = Prop { ring_bytes: mes12::KQ_RING_BYTES, doorbell: 6, high_priority: true, ..prop() };
    let m = mqd12::compute(&p);
    // QUEUE_SIZE 10 for 8 KiB, RPTR_BLOCK_SIZE 9 here (no double shift), UNORD, PRIV, KMD.
    assert_eq!(m[c::HQD_PQ_CONTROL], 0xD030_890A);
    assert_eq!((m[c::HQD_PQ_DOORBELL_CONTROL], m[c::HQD_ACTIVE], m[c::HQD_EOP_CONTROL]), (0x4000_0018, 0, 0x08));
    assert_eq!((m[c::HQD_PIPE_PRIORITY], m[c::HQD_QUEUE_PRIORITY], m[c::HQD_QUANTUM], m[c::HQD_IB_CONTROL]), (2, 15, 0x111, 0x0030_0000));
    let low = mqd12::compute(&Prop { high_priority: false, ..p });
    assert_eq!((low[c::HQD_PIPE_PRIORITY], low[c::HQD_QUEUE_PRIORITY]), (0, 0));
    let g = mqd12::gfx(&Prop { doorbell: 0x116, ..p });
    use mqd12::g as gf;
    assert_eq!((g[gf::HQD_CNTL], g[gf::RB_DOORBELL_CONTROL], g[gf::HQD_QUANTUM], g[gf::MQD_CONTROL], g[gf::HQD_ACTIVE]), (0x20F0_080A, 0x4000_0458, 0x0A01, 0x100, 1));
    assert_eq!((g[gf::MQD_BASE_ADDR], g[gf::HQD_BASE], g[gf::HQD_RPTR_ADDR], g[gf::RB_WPTR_POLL_ADDR_LO]), (0x2A_9000, 0x2AA0, 0x1740, 0x1760));
}

#[test]
fn the_frames_are_the_headers_structs_filled_as_linux_fills_them() {
    assert_eq!(api::header(op::SET_HW_RSRC), 0x0004_0001);
    assert_eq!(api::header_fields(api::header(op::MISC)), (1, 14, 64));
    assert_eq!(api::hwflag::LINUX, 0x0010_0447);
    let r = api::HwRes { sched: true, sch_ctx: 0x1000, query_fence: 0x1020, gc_base: [1, 2, 3, 4, 5], mmhub_base: [6; 5], osssys_base: [7; 5], oversubscription_timer: 50 };
    let d = api::set_hw_resources(&r);
    assert_eq!((d.len(), d[at::HW_VMID_MMHUB], d[at::HW_VMID_GFXHUB], d[at::HW_COMPUTE_HQD], d[at::HW_GFX_HQD], d[at::HW_SDMA_HQD + 1]), (64, 0xFF00, 0xFF00, 0x0C, 0xFE, 0xFC));
    assert_eq!((&d[at::HW_AGG_DOORBELLS..at::HW_AGG_DOORBELLS + 5], d[at::HW_FLAGS], d[at::HW_OVERSUB_TIMER], api::get64(&d, at::HW_SCH_CTX)), (&[0x800, 0x802, 0x804, 0x806, 0x808][..], 0x0010_0447, 50, 0x1000));
    assert_eq!((d[at::HW_GC_BASE + 4], d[at::HW_GC_BASE + 5], d[at::HW_MMHUB_BASE], d[at::HW_OSSSYS_BASE]), (5, 0, 6, 7));
    let kiq = api::set_hw_resources(&api::HwRes { sched: false, ..r });
    assert!(kiq[1..at::HW_SCH_CTX].iter().all(|w| *w == 0), "the KIQ pipe gets no masks and no doorbells");
    let one = api::set_hw_resources_1(0x80_0000_1000);
    assert_eq!((one[at::HW1_KIQ_UNMAP_TIMEOUT], api::get64(&one, at::HW1_CLEANER_FENCE)), (0xA, 0x80_0000_1000));
    let q = api::Legacy { queue_type: api::qtype::SCHQ, pipe: 0, queue: 0, doorbell: 0x16, mqd: 0x2B_0000, wptr: 0x1780 };
    let a = api::add_legacy(&q);
    assert_eq!((a[at::ADD_FLAGS], a[at::ADD_QUEUE_TYPE], a[at::ADD_DOORBELL], api::get64(&a, at::ADD_MQD), api::get64(&a, at::ADD_WPTR)), (0x2000, 3, 0x16, 0x2B_0000, 0x1780));
    let rm = api::remove_legacy(&q, api::Unmap::ResetQueues, 0x97, (0, 0));
    assert_eq!((rm[at::REMOVE_FLAGS], rm[at::REMOVE_QUEUE_TYPE]), (0x18, 3));
    assert_eq!(api::remove_legacy(&q, api::Unmap::ResetQueues, 0x59, (0, 0))[at::REMOVE_FLAGS], 0x8);
    let pre = api::remove_legacy(&q, api::Unmap::PreemptQueuesNoUnmap, 0x97, (0x2000, 0x1_0000_0007));
    assert_eq!((pre[at::REMOVE_FLAGS], api::get64(&pre, at::REMOVE_TF_ADDR), pre[at::REMOVE_TF_DATA], pre[at::REMOVE_QUEUE_TYPE]), (0x4, 0x2000, 7, 0));
    let w = api::misc_frame(api::Misc::WriteWait { reg0: 0x1, reg1: 0x2, reference: 0x00F8_0001, mask: 1 });
    assert_eq!(&w[at::MISC_OPCODE..=at::MISC_WRM_REG2], &[4, 0, 0, 0, 0, 1, 0x00F8_0001, 1, 1, 2]);
    assert_eq!(api::misc_frame(api::Misc::ChangeConfig { limit_single_process: false })[at::MISC_OPCODE], 9);
    assert_eq!(api::error_status(1, 2, 0, api::ERROR_API) >> 32, 0x8100_0201);
    assert_eq!(api::status_at(op::ADD_QUEUE), Some(38));
}

#[test]
fn the_firmware_rules_of_this_card() {
    let v = mes12::fw_version(mes12::GP3_OF_THIS_CARD);
    assert_eq!((v, mes12::api_version(mes12::GP3_OF_THIS_CARD), mes12::feature_version(mes12::GP3_OF_THIS_CARD)), (0x97, 0x29, 1));
    assert!(v >= mes12::HW_RSRC_1_FROM && v >= mes12::OVERSUBSCRIPTION_FROM && v >= mes12::REMOVE_AFTER_RESET_FROM);
    assert_eq!((mes12::oversubscription_timer(v), mes12::oversubscription_timer(0x8A)), (50, 0));
    // amdgpu_firmware_info of 18.11.: ME 0xc26, PFP 0xc94, MEC 0xda2, MES 0x97.
    assert!(mes12::cleaner_shader(0xC26, 0xC94, 0xDA2, 0x97));
    assert!(!mes12::cleaner_shader(0xC26, 0xC94, 0xDA2, 99));
    let doorbells: Vec<u32> = mes12::KERNEL_QUEUES.iter().map(|q| q.doorbell).collect();
    assert_eq!(doorbells, [6, 8, 10, 12, 0x116]);
    let sel: Vec<u32> = mes12::KERNEL_QUEUES.iter().map(|q| mes12::select(q.me, q.pipe, q.queue)).collect();
    assert_eq!(sel, [0x4, 0x5, 0x104, 0x105, 0]);
}
