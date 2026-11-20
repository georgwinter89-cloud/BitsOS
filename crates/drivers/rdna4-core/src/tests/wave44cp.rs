//! Wave 44, item CP: the command processor's resume against the trace of Linux's own start (init.txt lines
//! 9362-9501, the section gc-cp) and the source it was taken from: registers, the constants' sequence, the
//! doorbells, the clear-state buffer and the queue descriptors.

use crate::gc12;
use crate::gc12cp::{self as cp, Bus, Config};
use crate::hubseq::Op;
use crate::ipfacts::TABLE;
use crate::regmap::{Reg, dword};
use crate::{clearstate12 as cs, doorbell12 as db, mqd12};
use std::vec::Vec;

/// The writes of a fixture's section in order: (dword, value).
fn fixture_writes(text: &str) -> Vec<(u32, u32)> {
    text.lines()
        .filter_map(|l| {
            let t: Vec<&str> = l.split(' ').collect();
            let hex = |s: &str| u32::from_str_radix(s.trim_start_matches("0x"), 16).unwrap();
            (t.get(1) == Some(&"W")).then(|| (hex(t[3]), hex(t[4])))
        })
        .collect()
}

const GC_CP: &str = include_str!("../../../amdgpu/src/rdna4/linuxseq/gc-cp.txt");
const MES: &str = include_str!("../../../amdgpu/src/rdna4/linuxseq/mes.txt");

#[test]
fn the_cp_registers_lie_where_the_trace_names_them() {
    for (r, dw) in [
        (cp::GRBM_CNTL, 0x2000),
        (cp::GRBM_GFX_INDEX, 0xC200),
        (cp::GRBM_CC_GC_SA_UNIT_DISABLE, 0x2249),
        (cp::GRBM_GC_USER_SA_UNIT_DISABLE, 0xFB92),
        (cp::CC_RB_BACKEND_DISABLE, 0x263D),
        (cp::GC_USER_RB_BACKEND_DISABLE, 0xFB94),
        (cp::CC_GC_SHADER_ARRAY_CONFIG, 0x226F),
        (cp::GC_USER_SHADER_ARRAY_CONFIG, 0xFB90),
        (cp::TA_CNTL2, 0x2545),
        (cp::DB_RING_CONTROL, 0x261B),
        (cp::SH_MEM_CONFIG, 0xA9E4),
        (cp::SH_MEM_BASES, 0xA9E3),
        (cp::SPI_GDBG_PER_VMID_CNTL, 0x31D2),
        (cp::RLC_CSIB_ADDR_HI, 0xA988),
        (cp::RLC_CSIB_ADDR_LO, 0xA987),
        (cp::RLC_CSIB_LENGTH, 0xA989),
        (cp::RLC_SRM_CNTL, 0xEC80),
        (cp::CP_INT_CNTL_RING0, 0x306A),
        (cp::CP_RB_DOORBELL_RANGE_LOWER, 0x305A),
        (cp::CP_RB_DOORBELL_RANGE_UPPER, 0x305B),
        (cp::CP_MEC_DOORBELL_RANGE_LOWER, 0x305C),
        (cp::CP_MEC_DOORBELL_RANGE_UPPER, 0x305D),
        (cp::RLC_CP_SCHEDULERS, 0xA98A),
        (cp::SCRATCH_REG0, 0xC040),
        (db::ENTRY_0, 0xEEB),
        (db::ENTRY_3, 0xEEE),
        (db::STRAP0, 0xD3C),
    ] {
        assert_eq!(dword(&TABLE, r), Some(dw), "{}", r.name);
    }
}

/// The card as it answered Linux in gc-cp (the read lines), by register name.
struct Trace {
    reads: Vec<(&'static str, u32)>,
    writes: Vec<(u32, u32)>,
}

impl Bus for Trace {
    type E = ();
    fn read(&mut self, r: Reg) -> Result<u32, ()> {
        self.reads.iter().find(|(n, _)| *n == r.name).map(|(_, v)| *v).ok_or(())
    }
    fn op(&mut self, op: Op) -> Result<(), ()> {
        let dw = dword(&TABLE, op.reg()).ok_or(())?;
        let v = match op {
            Op::Write(_, v) => v,
            Op::Modify(r, f) => f(self.read(r)?),
            Op::Pulse(..) => return Err(()),
        };
        self.writes.push((dw, v));
        Ok(())
    }
}

const CARD: Config = Config { shader_engines: 4, sa_per_se: 2, rb_per_se: 4, cu_per_sa: 8 };

fn trace() -> Trace {
    Trace {
        reads: std::vec![
            ("GRBM_CNTL", 0x0030_0018),
            ("GRBM_CC_GC_SA_UNIT_DISABLE", 0x00FF_0000),
            ("GRBM_GC_USER_SA_UNIT_DISABLE", 0x00FF_0000),
            ("CC_RB_BACKEND_DISABLE", 0),
            ("GC_USER_RB_BACKEND_DISABLE", 0),
            ("CC_GC_SHADER_ARRAY_CONFIG", 0xFFF0_0000),
            ("GC_USER_SHADER_ARRAY_CONFIG", 0xFFF0_0000),
            ("TA_CNTL2", 0x0004_0000),
            ("DB_RING_CONTROL", 1),
            ("SPI_GDBG_PER_VMID_CNTL", 0),
        ],
        writes: Vec::new(),
    }
}

#[test]
fn the_constants_and_the_vmids_write_what_linux_wrote() {
    let mut t = trace();
    let l = cp::constants(&mut t, &CARD).unwrap();
    cp::vmids(|op| t.op(op)).unwrap();
    let want = fixture_writes(GC_CP);
    // Up to GRBM_GFX_CNTL back on VMID 0 after the compute VMIDs (init.txt 9476).
    assert_eq!(t.writes, want[..t.writes.len()], "constants and VMIDs");
    assert_eq!(t.writes.len(), 68);
    assert_eq!(want[t.writes.len()], (0xEEB, db::ENTRY_0_GFX), "NBIF's entry 0 follows (nbif_v6_3_1.c:307)");
    // Eight shader arrays active (SA_DISABLE 0xff00 names arrays 8 to 15), 16 render backends, 64 CUs (the
    // CC config's INACTIVE_WGPS 0xfff0 leaves WGPs 0 to 3 of each array).
    assert_eq!((l.active_sa, l.rbs, l.cus), (0xFF, 16, 64));
}

#[test]
fn a_disabled_shader_array_is_not_selected_but_its_mask_is_read_again() {
    let mut t = trace();
    t.reads[1].1 = 0x0000_0200; // SA 1 (SE 0, array 1) fused off.
    cp::constants(&mut t, &CARD).unwrap();
    let selects: Vec<u32> = t.writes.iter().filter(|(d, _)| *d == 0xC200).map(|(_, v)| *v).collect();
    assert_eq!(selects.len(), 8, "seven arrays and the broadcast at the end");
    assert!(!selects.contains(&0x4000_0100));
}

#[test]
fn the_values_of_the_section_are_linuxs_arithmetic() {
    assert_eq!(cp::DEFAULT_SH_MEM_CONFIG, 0xC00C);
    assert_eq!((cp::SH_MEM_BASES_GFX, cp::SH_MEM_BASES_KFD), (0x2000_1000, 0x0001_0002));
    assert_eq!((cp::sh_mem_bases_of(0), cp::sh_mem_bases_of(7), cp::sh_mem_bases_of(8), cp::sh_mem_bases_of(16)), (None, Some(0x2000_1000), Some(0x0001_0002), None));
    assert_eq!(cp::grbm_read_timeout(0x0030_0018), 0x0030_00FF);
    assert_eq!(cp::db_ring_counter(1), 1);
    assert_eq!(cp::trap_en(0), 8);
    assert_eq!(cp::srm_enable(2), 3);
    assert_eq!(cp::gui_idle_off(0x003C_0000), 0);
    assert_eq!(cp::unhalt_mec(0x4000_0000), 0x3C00_0000, "init.txt 9490-9491");
    assert_eq!(cp::unhalt_mec(0x4003_0010), 0x3C00_0000);
    assert_eq!(cp::unhalt_gfx(0x1500_A000), 0x0100_A000, "init.txt 9492-9493: CE_HALT stays");
    assert_eq!(cp::kiq_setting(0x3038), 0x30E8, "init.txt 9500-9501");
    assert_eq!(cp::select_se_sa(Some(3), Some(1)), 0x4003_0100);
    assert_eq!(cp::select_se_sa(None, None), cp::GFX_INDEX_ALL);
    assert_eq!(cp::GFX_INDEX_ALL, 0xE000_0000);
    for vmid in 0..16 {
        assert_eq!(gc12::grbm_select(0, 0, 0, vmid), vmid << 4);
    }
    // The section's last three writes, from the source's functions over the card's answers.
    let w = fixture_writes(GC_CP);
    assert_eq!(w[w.len() - 3..], [(0xC904, cp::unhalt_mec(0x4000_0000)), (0xA803, cp::unhalt_gfx(0x1500_A000)), (0xA98A, cp::kiq_setting(0x3038))]);
}

#[test]
fn the_doorbells_are_soc24s() {
    assert_eq!((db::RB_LOWER, db::RB_UPPER, db::MEC_LOWER, db::MEC_UPPER), (0x458, 0x7F8, 0, 0x450), "init.txt 9486-9489");
    assert_eq!((db::GFX_RING0_DW, db::MES_SCHED_DW, db::MES_KIQ_DW), (0x116, 0x16, 0x18));
    assert_eq!((db::mec_ring_dw(0), db::mec_ring_dw(7)), (6, db::dword(db::MEC_RING7)));
    assert!(db::dword(db::GFX_USERQUEUE_END) < db::dword(db::FIRST_NON_CP), "the CP's doorbells lie below SDMA's");
    assert_eq!((db::dword(db::FIRST_NON_CP), db::USERQUEUE_START, db::GFX_RING1, db::GFX_USERQUEUE_START), (0x200, 0xD, 0x8C, 0x8D));
    // The CP's range registers hold the kernel rings' doorbells as byte offsets.
    assert!(db::RB_LOWER / 4 == db::GFX_RING0_DW && db::MEC_UPPER / 4 > db::MES_KIQ_DW);
    let w = fixture_writes(GC_CP);
    assert!(w.contains(&(0xEEB, db::ENTRY_0_GFX)) && w.contains(&(0xEEE, db::ENTRY_3_MES)));
    assert_eq!((db::PORT_AWID.get(db::ENTRY_0_GFX), db::PORT_AWID.get(db::ENTRY_3_MES), db::PORT_AWADDR_31_28.get(db::ENTRY_0_GFX)), (3, 6, 3));
    assert_eq!(db::rev_id(0x310C_7550), 1, "discovery.txt line 6");
}

#[test]
fn the_clear_state_buffer_is_linuxs_75_dwords() {
    assert_eq!(cs::size_dw(), 0x4B, "init.txt 9481");
    let b = cs::buffer();
    assert_eq!(b.len(), 75);
    assert_eq!(b[..4], [6, 34, 0xA03E, 0]);
    // Extent 2 starts after 1 + 2 + 34 dwords.
    assert_eq!(b[37..39], [2, 0xA0CC]);
    assert_eq!(b.iter().filter(|v| **v != 0).count(), 1 + 6 * 2);
    assert_eq!(cs::bytes().len(), 300);
    assert_eq!(cs::address_regs(0x83_DAAF_C003), (0xDAAF_C000, 0x83), "init.txt 9479-9480");
}

/// The kernel queues' descriptors (`mqd12`, item ME's, which `mesq` writes) as gfx_v12_0.c fills them, from GFX
/// 12's own defaults (gfx_v12_0.c:55-70): the gfx queue's CP_GFX_HQD_CNTL starts from 0x00f00000, not GFX 11's.
#[test]
fn the_queue_descriptors_are_filled_as_linux_fills_them() {
    let p = mqd12::Prop { mqd: 0x1234_5003, ring: 0x0040_0000, ring_bytes: 8192, rptr: 0x0001_0000_1004, wptr: 0x0001_0000_1008, eop: 0x0050_0000, doorbell: db::GFX_RING0_DW, high_priority: false };
    let g = mqd12::gfx(&p);
    assert_eq!(g.len(), 512);
    use mqd12::g as G;
    assert_eq!((g[G::MQD_BASE_ADDR], g[G::MQD_BASE_ADDR_HI], g[G::MQD_CONTROL]), (0x1234_5000, 0, 0x100));
    assert_eq!((g[G::HQD_QUANTUM], g[G::HQD_BASE], g[G::HQD_RPTR_ADDR_HI]), (0xA01, 0x4000, 1));
    // RB_BUFSZ order_base_2(2048) - 1 = 10, RB_BLKSZ 8, KMD_QUEUE, over GFX 12's default.
    assert_eq!(g[G::HQD_CNTL], 0x20F0_080A);
    assert_eq!(g[G::RB_DOORBELL_CONTROL], 0x4000_0000 | (0x116 << 2));
    // A kernel queue has no user fence (amdgpu_ring.c:823-854).
    assert_eq!((g[G::HQD_ACTIVE], g[G::FENCE_ADDRESS_LO], g[G::FENCE_ADDRESS_HI]), (1, 0, 0));
    let c = mqd12::compute(&mqd12::Prop { doorbell: db::mec_ring_dw(0), ..p });
    use mqd12::c as C;
    assert_eq!((c[C::HEADER], c[C::COMPUTE_PIPELINESTAT_ENABLE], c[C::STATIC_THREAD_MGMT_SE3], c[C::COMPUTE_MISC_RESERVED]), (0xC031_0800, 1, u32::MAX, 7));
    assert_eq!((c[C::HQD_EOP_BASE_ADDR_LO], c[C::HQD_EOP_CONTROL]), (0x5000, 8), "2^(8+1) dwords = GFX12_MEC_HPD_SIZE");
    assert_eq!(c[C::HQD_PQ_CONTROL], 0xD030_890A);
    assert_eq!(c[C::HQD_PQ_DOORBELL_CONTROL], 0x4000_0000 | (6 << 2));
    assert_eq!((c[C::HQD_PERSISTENT_STATE], c[C::HQD_IB_CONTROL], c[C::HQD_QUANTUM], c[C::MQD_CONTROL]), (0x0BE0_5501, 0x0030_0000, 0x111, 0x100));
    assert_eq!((c[C::FENCE_ADDRESS_LO], c[C::HQD_ACTIVE]), (0, 0));
}

/// The defaults the trace pins: MES fills its KIQ's descriptor with the same defaults (mes_v12_0.c:1543-1553,
/// :1576-1578) and writes the result into the card (init.txt 9522, 9528, 9532).
#[test]
fn the_defaults_the_trace_shows_agree_with_mess_writes() {
    use crate::mes12::{default, f};
    let w = fixture_writes(MES);
    // QUEUE_SIZE 0xc, RPTR_BLOCK_SIZE (9 << 8) masked to 0 by REG_SET_FIELD, UNORD, PRIV, KMD, NO_UPDATE_RPTR.
    let mut c = f::QUEUE_SIZE.put(default::CP_HQD_PQ_CONTROL, 0xC);
    c = f::RPTR_BLOCK_SIZE.put(c, 9 << 8);
    c = f::KMD_QUEUE.put(f::PRIV_STATE.put(f::UNORD_DISPATCH.put(c, 1), 1), 1) | (1 << 27);
    assert!(w.contains(&(0x321A, c)), "CP_HQD_PQ_CONTROL {c:#x}");
    // gfx_v12_0.c:65 defines the default 0x00308509 itself: bit 27 (NO_UPDATE_RPTR) is 0 there.
    assert_eq!(default::CP_HQD_PQ_CONTROL & (1 << 27), 0);
    assert!(w.contains(&(0x320D, f::PRELOAD_SIZE.put(default::CP_HQD_PERSISTENT_STATE, 0x55))));
    assert!(MES.contains("R CP_MQD_CONTROL 0x322b 0x00000100") && default::CP_MQD_CONTROL == 0x100);
}

#[test]
fn the_stages_write_list_names_every_register_it_writes() {
    let w = fixture_writes(GC_CP);
    for (dw, _) in w {
        let named = cp::WRITTEN.iter().chain(&[db::ENTRY_0, db::ENTRY_3]).any(|r| dword(&TABLE, *r) == Some(dw));
        assert!(named, "dword {dw:#x}");
    }
}
