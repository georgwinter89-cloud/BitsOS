//! Wave 44, item ME (piece 14): MES 12, the scheduler firmware the card's queues go through, as registers, fields
//! and the arithmetic of Linux's `mes_v12_0_hw_init` (A:mes_v12_0.c:2036-2105, entered through
//! `mes_v12_0_kiq_hw_init` :1956-2016). One `uni_mes` file serves both pipes (`enable_uni_mes`, set for GC 12.0.1 by
//! A:amdgpu_discovery.c:2998-3007): pipe 0 runs the scheduler, pipe 1 the KIQ. The KIQ pipe's queue is loaded from its
//! MQD by registers (`mes_v12_0_queue_init_register` :1595-1655); the scheduler's own queue is mapped by an
//! ADD_QUEUE on the KIQ pipe's ring (`amdgpu_mes_map_legacy_queue`, A:amdgpu_mes.c:372-395).
//!
//! Offsets and masks are gc_12_0_0_offset.h's and gc_12_0_0_sh_mask.h's (`rdna4-regs`' test holds each); the
//! defaults Linux starts its values from are gc_11_0_0_default.h's, which mes_v12_0.c includes (:32).

use crate::hubseq::Op;
use crate::ipfacts::hw;
use crate::nbreg::Field;
use crate::regmap::Reg;

const fn gc(name: &'static str, base_index: u8, offset: u32) -> Reg {
    Reg { name, hw_id: hw::GC, base_index, offset }
}

pub const GRBM_GFX_CNTL: Reg = crate::gc12::GRBM_GFX_CNTL;
pub const CP_MES_CNTL: Reg = gc("CP_MES_CNTL", 1, 0x2807);
pub const CP_MES_PRGRM_CNTR_START: Reg = gc("CP_MES_PRGRM_CNTR_START", 1, 0x2800);
pub const CP_MES_PRGRM_CNTR_START_HI: Reg = gc("CP_MES_PRGRM_CNTR_START_HI", 1, 0x289D);
pub const CP_MES_GP3_LO: Reg = gc("CP_MES_GP3_LO", 1, 0x2849);
pub const CP_MES_DOORBELL_CONTROL1: Reg = gc("CP_MES_DOORBELL_CONTROL1", 1, 0x283C);
pub const CP_UNMAPPED_DOORBELL: Reg = gc("CP_UNMAPPED_DOORBELL", 1, 0x0880);
pub const CP_HQD_GFX_CONTROL: Reg = gc("CP_HQD_GFX_CONTROL", 0, 0x1E9F);
pub const CP_MQD_BASE_ADDR: Reg = gc("CP_MQD_BASE_ADDR", 0, 0x1FA9);
pub const CP_MQD_BASE_ADDR_HI: Reg = gc("CP_MQD_BASE_ADDR_HI", 0, 0x1FAA);
pub const CP_HQD_ACTIVE: Reg = gc("CP_HQD_ACTIVE", 0, 0x1FAB);
pub const CP_HQD_VMID: Reg = gc("CP_HQD_VMID", 0, 0x1FAC);
pub const CP_HQD_PERSISTENT_STATE: Reg = gc("CP_HQD_PERSISTENT_STATE", 0, 0x1FAD);
pub const CP_HQD_PQ_BASE: Reg = gc("CP_HQD_PQ_BASE", 0, 0x1FB1);
pub const CP_HQD_PQ_BASE_HI: Reg = gc("CP_HQD_PQ_BASE_HI", 0, 0x1FB2);
pub const CP_HQD_PQ_RPTR_REPORT_ADDR: Reg = gc("CP_HQD_PQ_RPTR_REPORT_ADDR", 0, 0x1FB4);
pub const CP_HQD_PQ_RPTR_REPORT_ADDR_HI: Reg = gc("CP_HQD_PQ_RPTR_REPORT_ADDR_HI", 0, 0x1FB5);
pub const CP_HQD_PQ_WPTR_POLL_ADDR: Reg = gc("CP_HQD_PQ_WPTR_POLL_ADDR", 0, 0x1FB6);
pub const CP_HQD_PQ_WPTR_POLL_ADDR_HI: Reg = gc("CP_HQD_PQ_WPTR_POLL_ADDR_HI", 0, 0x1FB7);
pub const CP_HQD_PQ_DOORBELL_CONTROL: Reg = gc("CP_HQD_PQ_DOORBELL_CONTROL", 0, 0x1FB8);
pub const CP_HQD_PQ_CONTROL: Reg = gc("CP_HQD_PQ_CONTROL", 0, 0x1FBA);
pub const CP_MQD_CONTROL: Reg = gc("CP_MQD_CONTROL", 0, 0x1FCB);

/// CONTROL1..5 lie one after the other (gc_12_0_0_offset.h).
pub const fn doorbell_control(level: u32) -> Reg {
    crate::nbreg::member(CP_MES_DOORBELL_CONTROL1, level, 1)
}

/// The HQD registers `queue_init_register` writes, in its order, then the GFX queue's control of
/// `init_aggregated_doorbell`: what the stage lists and the model knows.
pub const HQD: [Reg; 15] = [
    CP_HQD_VMID, CP_HQD_PQ_DOORBELL_CONTROL, CP_MQD_BASE_ADDR, CP_MQD_BASE_ADDR_HI, CP_MQD_CONTROL, CP_HQD_PQ_BASE, CP_HQD_PQ_BASE_HI, CP_HQD_PQ_RPTR_REPORT_ADDR,
    CP_HQD_PQ_RPTR_REPORT_ADDR_HI, CP_HQD_PQ_CONTROL, CP_HQD_PQ_WPTR_POLL_ADDR, CP_HQD_PQ_WPTR_POLL_ADDR_HI, CP_HQD_PERSISTENT_STATE, CP_HQD_ACTIVE, CP_HQD_GFX_CONTROL,
];

pub mod f {
    use super::Field;
    // CP_MES_CNTL.
    pub const MES_INVALIDATE_ICACHE: Field = Field::new(4, 0x0000_0010);
    pub const MES_PIPE0_RESET: Field = Field::new(16, 0x0001_0000);
    pub const MES_PIPE1_RESET: Field = Field::new(17, 0x0002_0000);
    pub const MES_PIPE0_ACTIVE: Field = Field::new(26, 0x0400_0000);
    pub const MES_PIPE1_ACTIVE: Field = Field::new(27, 0x0800_0000);
    pub const MES_HALT: Field = Field::new(30, 0x4000_0000);
    // CP_HQD_PQ_CONTROL.
    pub const QUEUE_SIZE: Field = Field::new(0, 0x0000_003F);
    pub const RPTR_BLOCK_SIZE: Field = Field::new(8, 0x0000_3F00);
    pub const NO_UPDATE_RPTR: Field = Field::new(27, 0x0800_0000);
    pub const UNORD_DISPATCH: Field = Field::new(28, 0x1000_0000);
    pub const TUNNEL_DISPATCH: Field = Field::new(29, 0x2000_0000);
    pub const PRIV_STATE: Field = Field::new(30, 0x4000_0000);
    pub const KMD_QUEUE: Field = Field::new(31, 0x8000_0000);
    // CP_HQD_PQ_DOORBELL_CONTROL (and the MES's aggregated doorbells, whose fields lie at the same bits).
    pub const DOORBELL_OFFSET: Field = Field::new(2, 0x0FFF_FFFC);
    pub const DOORBELL_SOURCE: Field = Field::new(28, 0x1000_0000);
    pub const DOORBELL_EN: Field = Field::new(30, 0x4000_0000);
    pub const DOORBELL_HIT: Field = Field::new(31, 0x8000_0000);
    pub const PRELOAD_SIZE: Field = Field::new(8, 0x0003_FF00);
    pub const EOP_SIZE: Field = Field::new(0, 0x0000_003F);
    pub const MQD_VMID: Field = Field::new(0, 0x0000_000F);
    pub const HQD_VMID: Field = Field::new(0, 0x0000_000F);
    // CP_UNMAPPED_DOORBELL.
    pub const UNMAPPED_ENABLE: Field = Field::new(0, 0x0000_0001);
    pub const PROC_LSB: Field = Field::new(8, 0x0000_1F00);
    pub const DB_UPDATED_MSG_EN: Field = Field::new(15, 0x0000_8000);
}

/// gc_11_0_0_default.h, as mes_v12_0.c and gfx_v12_0.c:55-70 use them.
pub mod default {
    pub const CP_HQD_EOP_CONTROL: u32 = 0x0000_0006;
    pub const CP_MQD_CONTROL: u32 = 0x0000_0100;
    pub const CP_HQD_PQ_CONTROL: u32 = 0x0030_8509;
    pub const CP_HQD_PERSISTENT_STATE: u32 = 0x0BE0_5501;
    pub const CP_HQD_IB_CONTROL: u32 = 0x0030_0000;
    pub const CP_HQD_IQ_TIMER: u32 = 0;
    pub const CP_HQD_QUANTUM: u32 = 0;
}

/// `enum amdgpu_mes_pipe` (A:amdgpu_mes.h): the scheduler pipe, then the KIQ pipe.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Pipe {
    Sched = 0,
    Kiq = 1,
}

impl Pipe {
    pub const BOTH: [Pipe; 2] = [Pipe::Sched, Pipe::Kiq];

    pub const fn index(self) -> usize {
        self as usize
    }

    pub const fn name(self) -> &'static str {
        match self {
            Pipe::Sched => "scheduler pipe",
            Pipe::Kiq => "KIQ pipe",
        }
    }

    /// `doorbell_index.mes_ring0/1 << 1` (A:mes_v12_0.c:1752-1755, A:amdgpu_doorbell.h:200-201): BAR2 dwords.
    pub const fn doorbell(self) -> u32 {
        match self {
            Pipe::Sched => 0x0B << 1,
            Pipe::Kiq => 0x0C << 1,
        }
    }
}

/// `soc24_grbm_select` (A:soc24.c:102-112): GRBM_GFX_CNTL's value for (me, pipe, queue), VMID 0.
pub const fn select(me: u32, pipe: u32, queue: u32) -> u32 {
    crate::gc12::f::MEID.put(crate::gc12::f::PIPEID.put(crate::gc12::f::QUEUEID.put(0, queue), pipe), me)
}

/// MES is ME 3 (A:mes_v12_0.c:1309, :1599).
pub const MES_ME: u32 = 3;

pub const fn mes_select(p: Pipe) -> u32 {
    select(MES_ME, p as u32, 0)
}

/// `MES_EOP_SIZE` (A:mes_v12_0.c:48), allocated on a page of its own (:1461-1484).
pub const EOP_BYTES: u32 = 2048;
/// The MES rings: `amdgpu_ring_init(.., 1024, ..)` with `sched_hw_submission` 8 for ring type MES
/// (A:amdgpu_ring.c:244-245), `ring_size = roundup_pow_of_two(max_dw * 4 * sched_hw_submission)`.
pub const RING_BYTES: u32 = 1024 * 4 * 8;
pub const RING_DW: u32 = RING_BYTES / 4;
/// `num_fences_mask` of a MES ring: `num_hw_submission * 2 - 1` (A:amdgpu_fence.c).
pub const FENCES_MASK: u32 = 8 * 2 - 1;
/// `mes_v12_0_submit_pkt_and_poll_completion`'s bound, 2100 ms, polled with `udelay(2)`
/// (A:mes_v12_0.c:157, A:amdgpu_fence.c:310-320).
pub const TIMEOUT_US: u64 = 2_100_000;
pub const POLL_US: u64 = 2;
/// `mes_v12_0_enable`'s settle after the pipes start, uni_mes (A:mes_v12_0.c:1353-1356).
pub const ENABLE_SETTLE_US: u64 = 500;

/// `adev->mes.vmid_mask_mmhub` and `vmid_mask_gfxhub` (A:amdgpu_mes.c:123-124): VMIDs 8..15, the first KFD VMID
/// being 8 (A:gmc_v12_0.c:953); dmesg 18.11.: "vmid_mask_mmhub 0x0000ff00, vmid_mask_gfxhub 0x0000ff00".
pub const VMID_MASK_MMHUB: u32 = 0xFF00;
pub const VMID_MASK_GFXHUB: u32 = 0xFF00;
/// `amdgpu_mes_get_hqd_mask` (A:amdgpu_mes.c:89-101) for this card: one gfx pipe of 8 queues with 1 kernel ring
/// (0xfe), two MEC pipes of 4 queues with 4 kernel rings (0x0c each), two SDMA engines (0xfc); dmesg 18.11.
pub const GFX_HQD_MASK: [u32; 2] = [0xFE, 0];
pub const COMPUTE_HQD_MASK: [u32; 8] = [0x0C, 0x0C, 0, 0, 0, 0, 0, 0];
pub const SDMA_HQD_MASK: [u32; 2] = [0xFC, 0xFC];
/// `db_start_dw_offset`: the kernel's doorbells (`max_assignment` 0x8C << 1 = 0x118, + 1, + 0x400 for the
/// unmapped-queue page) end within 8 KiB, so MES's start at dword 0x800 (A:amdgpu_doorbell_mgr.c:157-162, :213-227).
pub const DB_START_DW: u32 = 0x800;
/// `aggregated_doorbells[i] = db_start_dw_offset + i * 2` (A:amdgpu_mes.c:54-57), five priority levels.
pub const AGGREGATED_DOORBELLS: [u32; 5] = [DB_START_DW, DB_START_DW + 2, DB_START_DW + 4, DB_START_DW + 6, DB_START_DW + 8];

/// What CP_MES_GP3_LO holds: the firmware's version in bits 0-11 (`AMDGPU_MES_VERSION_MASK`), its API version in
/// bits 12-23 (`AMDGPU_MES_API_VERSION_MASK`), the feature version above (A:amdgpu_mes.h:40-46).
pub const fn fw_version(gp3: u32) -> u32 {
    gp3 & 0xFFF
}
pub const fn api_version(gp3: u32) -> u32 {
    (gp3 >> 12) & 0xFFF
}
pub const fn feature_version(gp3: u32) -> u32 {
    gp3 >> 24
}
/// From these firmware versions on Linux sends SET_HW_RESOURCES_1 on the scheduler pipe (A:mes_v12_0.c:2076),
/// gives the oversubscription timer 50 (:1004), and sets `remove_queue_after_reset` (:770-772).
pub const HW_RSRC_1_FROM: u32 = 0x4B;
pub const OVERSUBSCRIPTION_FROM: u32 = 0x8B;
pub const REMOVE_AFTER_RESET_FROM: u32 = 0x5A;
/// What the card's GP3 read for Linux on both pipes (init.txt 9536, 9541): version 0x97, API 0x29, feature 1.
pub const GP3_OF_THIS_CARD: u32 = 0x0102_9097;

/// `mes_v12_0_set_hw_resources`'s oversubscription timer for the pipe's firmware version.
pub const fn oversubscription_timer(version: u32) -> u32 {
    if version < OVERSUBSCRIPTION_FROM { 0 } else { 50 }
}

/// `gfx.enable_cleaner_shader` for GC 12.0.1 (A:gfx_v12_0.c:1450-1458): ME, PFP and MEC from their files'
/// `ucode_version`, MES from `mes.fw_version[0]`. Linux sends CHANGE_CONFIG in hw_init only then
/// (`amdgpu_mes_update_enforce_isolation`, A:amdgpu_mes.c:911-926).
pub const fn cleaner_shader(me: u32, pfp: u32, mec: u32, mes: u32) -> bool {
    me >= 2480 && pfp >= 2530 && mec >= 2680 && mes >= 100
}

/// The program counter's start from the file's `mes_uc_start_addr_lo/hi` (`mes_firmware_header_v1_0` @56 and @60,
/// A:amdgpu_ucode.h:201-213): `uc_start_addr >> 2`, written low then high (A:mes_v12_0.c:1336-1340).
pub const fn pc_start(lo: u32, hi: u32) -> (u32, u32) {
    let at = ((hi as u64) << 32 | lo as u64) >> 2;
    (at as u32, (at >> 32) as u32)
}
pub const UC_START_LO_AT: usize = 56;
pub const UC_START_HI_AT: usize = 60;

/// `mes_v12_0_enable(adev, true)` for one pipe (A:mes_v12_0.c:1308-1347), under that pipe's selection: its reset
/// bit into what CP_MES_CNTL holds, the program counter, then CP_MES_CNTL with the active bits of this pipe and
/// every pipe before it (the loop's "activate one pipe each loop": HALT and the resets clear).
pub fn enable(p: Pipe, start: (u32, u32)) -> [Op; 4] {
    let reset: fn(u32) -> u32 = match p {
        Pipe::Sched => |v| f::MES_PIPE0_RESET.put(v, 1),
        Pipe::Kiq => |v| f::MES_PIPE1_RESET.put(v, 1),
    };
    [Op::Modify(CP_MES_CNTL, reset), Op::Write(CP_MES_PRGRM_CNTR_START, start.0), Op::Write(CP_MES_PRGRM_CNTR_START_HI, start.1), Op::Write(CP_MES_CNTL, active(p))]
}

pub const fn active(p: Pipe) -> u32 {
    let v = f::MES_PIPE0_ACTIVE.put(0, 1);
    match p {
        Pipe::Sched => v,
        Pipe::Kiq => f::MES_PIPE1_ACTIVE.put(v, 1),
    }
}

/// `mes_v12_0_enable(adev, false)` (:1356-1368): both pipes inactive, the instruction cache invalidated, both
/// resets and HALT set, over what CP_MES_CNTL holds.
pub const fn halted(v: u32) -> u32 {
    let v = f::MES_PIPE0_ACTIVE.put(f::MES_PIPE1_ACTIVE.put(v, 0), 0);
    f::MES_HALT.put(f::MES_PIPE1_RESET.put(f::MES_PIPE0_RESET.put(f::MES_INVALIDATE_ICACHE.put(v, 1), 1), 1), 1)
}

/// `order_base_2(n) - 1` of Linux's size fields (n a power of two).
pub const fn log2_less_one(n: u32) -> u32 {
    n.trailing_zeros() - 1
}

/// `mes_v12_0_mqd_init`'s CP_HQD_PQ_CONTROL (:1544-1556). RPTR_BLOCK_SIZE gets `(order_base_2(4096 / 4) - 1) << 8`
/// through REG_SET_FIELD, which shifts it by 8 again past the field: the field ends up 0, as the trace's 0xd830800c
/// shows (init.txt 9528). Built as Linux builds it, not "fixed".
pub const fn mes_pq_control(ring_bytes: u32) -> u32 {
    let mut t = f::QUEUE_SIZE.put(default::CP_HQD_PQ_CONTROL, log2_less_one(ring_bytes / 4));
    t = f::RPTR_BLOCK_SIZE.put(t, log2_less_one(4096 / 4) << 8);
    t = f::UNORD_DISPATCH.put(t, 1);
    t = f::TUNNEL_DISPATCH.put(t, 0);
    t = f::PRIV_STATE.put(t, 1);
    t = f::KMD_QUEUE.put(t, 1);
    f::NO_UPDATE_RPTR.put(t, 1)
}

/// A queue's CP_HQD_PQ_DOORBELL_CONTROL with its doorbell (dword index) enabled (:1559-1569).
pub const fn doorbell_enabled(index: u32) -> u32 {
    let t = f::DOORBELL_OFFSET.put(0, index);
    f::DOORBELL_HIT.put(f::DOORBELL_SOURCE.put(f::DOORBELL_EN.put(t, 1), 0), 0)
}

/// CP_HQD_PERSISTENT_STATE with PRELOAD_SIZE 0x55 (:1579-1582).
pub const fn persistent_state() -> u32 {
    f::PRELOAD_SIZE.put(default::CP_HQD_PERSISTENT_STATE, 0x55)
}

/// CP_HQD_EOP_CONTROL for an EOP buffer of `bytes` ("2^(EOP_SIZE+1) dwords", :1502-1505).
pub const fn eop_control(bytes: u32) -> u32 {
    f::EOP_SIZE.put(default::CP_HQD_EOP_CONTROL, log2_less_one(bytes / 4))
}

/// `mes_v12_0_enable_unmapped_doorbell_handling(mes, true)` (:1076-1094): PROC_LSB 0xd, ENABLE.
pub const fn unmapped_doorbell(v: u32) -> u32 {
    f::UNMAPPED_ENABLE.put(f::PROC_LSB.put(v, 0xD), 1)
}

/// One of `mes_v12_0_init_aggregated_doorbell`'s five (:1021-1068): offset, enable and hit cleared, the level's
/// doorbell as the offset, enabled.
pub const fn aggregated(v: u32, doorbell_dw: u32) -> u32 {
    let v = v & !(f::DOORBELL_OFFSET.mask | f::DOORBELL_EN.mask | f::DOORBELL_HIT.mask);
    v | (doorbell_dw << f::DOORBELL_OFFSET.shift) | (1 << f::DOORBELL_EN.shift)
}

/// CP_HQD_GFX_CONTROL with DB_UPDATED_MSG_EN, written whole (:1070-1071).
pub const HQD_GFX_CONTROL: u32 = 1 << 15;

/// What `mes_v12_0_queue_init_register` (:1595-1655) does once its pipe is selected, as steps over the queue's
/// MQD (`m`: the MQD's values for the registers it names). CP_MQD_CONTROL is read and then written with the
/// literal 0 Linux writes (:1641-1643), not the value it computed.
pub fn queue_init_register(m: &crate::mqd12::HqdFromMqd) -> [Op; 13] {
    [
        Op::Modify(CP_HQD_VMID, |v| f::HQD_VMID.put(v, 0)),
        Op::Modify(CP_HQD_PQ_DOORBELL_CONTROL, |v| f::DOORBELL_EN.put(v, 0)),
        Op::Write(CP_MQD_BASE_ADDR, m.mqd_lo),
        Op::Write(CP_MQD_BASE_ADDR_HI, m.mqd_hi),
        Op::Modify(CP_MQD_CONTROL, |_| 0),
        Op::Write(CP_HQD_PQ_BASE, m.pq_base_lo),
        Op::Write(CP_HQD_PQ_BASE_HI, m.pq_base_hi),
        Op::Write(CP_HQD_PQ_RPTR_REPORT_ADDR, m.rptr_lo),
        Op::Write(CP_HQD_PQ_RPTR_REPORT_ADDR_HI, m.rptr_hi),
        Op::Write(CP_HQD_PQ_CONTROL, m.pq_control),
        Op::Write(CP_HQD_PQ_WPTR_POLL_ADDR, m.wptr_lo),
        Op::Write(CP_HQD_PQ_WPTR_POLL_ADDR_HI, m.wptr_hi),
        Op::Write(CP_HQD_PQ_DOORBELL_CONTROL, m.doorbell),
    ]
}

/// The two writes that end `queue_init_register`: the persistent state, then the queue's ACTIVE.
pub fn queue_init_tail(m: &crate::mqd12::HqdFromMqd) -> [Op; 2] {
    [Op::Write(CP_HQD_PERSISTENT_STATE, m.persistent), Op::Write(CP_HQD_ACTIVE, m.active)]
}

/// The kernel queues Linux maps through MES on this card, in its order (`gfx_v12_0_kcq_resume` :3547-3560, then
/// `gfx_v12_0_cp_async_gfx_ring_resume` :3130-3146). Compute rings `amdgpu_gfx_compute_queue_acquire`'s
/// multipipe policy places on MEC 1 pipe 0 and 1, queue 0 and 1 in turn (dmesg: comp_1.0.0, comp_1.1.0,
/// comp_1.0.1, comp_1.1.1); their doorbells `(mec_ring0 + i) << 1` (A:gfx_v12_0.c:1017), the gfx ring's
/// `gfx_ring0 << 1` (:977); A:amdgpu_doorbell.h:192-205.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Kq {
    pub gfx: bool,
    pub me: u32,
    pub pipe: u32,
    pub queue: u32,
    pub doorbell: u32,
}

pub const KERNEL_QUEUES: [Kq; 5] = [
    Kq { gfx: false, me: 1, pipe: 0, queue: 0, doorbell: 0x03 << 1 },
    Kq { gfx: false, me: 1, pipe: 1, queue: 0, doorbell: 0x04 << 1 },
    Kq { gfx: false, me: 1, pipe: 0, queue: 1, doorbell: 0x05 << 1 },
    Kq { gfx: false, me: 1, pipe: 1, queue: 1, doorbell: 0x06 << 1 },
    Kq { gfx: true, me: 0, pipe: 0, queue: 0, doorbell: 0x8B << 1 },
];

/// GFX 12's compute and gfx rings: `amdgpu_ring_init(.., 1024, ..)` with `amdgpu_sched_hw_submission` 2
/// (A:amdgpu_drv.c), 8 KiB; the compute queues' EOP buffers `GFX12_MEC_HPD_SIZE` each (A:gfx_v12_0.c:51).
pub const KQ_RING_BYTES: u32 = 1024 * 4 * 2;
pub const MEC_HPD_BYTES: u32 = 2048;
