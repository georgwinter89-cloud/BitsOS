//! Piece 13, the command processor's resume: what Linux's `gfx_v12_0_hw_init` (gfx_v12_0.c:3767) does after
//! GFXHUB's start and before MES, as registers, fields and arithmetic. The trace's section gc-cp (init.txt lines
//! 9362-9501), in Linux's order:
//!
//! - `gfx_v12_0_constants_init` :1824-1857: GRBM_CNTL's read timeout, the shader arrays' and render backends'
//!   disable masks (`gfx_v12_0_setup_rb` :1749-1782, `gfx_v12_0_get_cu_info` :5597-5648 with one GRBM_GFX_INDEX
//!   selection per active shader array), TA_CNTL2, DB_RING_CONTROL, SH_MEM_CONFIG and SH_MEM_BASES per VMID
//!   through GRBM_GFX_CNTL, and the compute VMIDs' apertures and trap enable (`gfx_v12_0_init_compute_vmid`
//!   :1784-1812);
//! - NBIF's doorbell entries for GFX and MES (`doorbell12`);
//! - `gfx_v12_0_rlc_resume` :2104-2125 under PSP loading: the clear-state buffer's registers (`init_csb`
//!   :1938-1948, the buffer itself is `clearstate12`) and the RLC's save-restore machine (:1999-2006);
//! - `gfx_v12_0_cp_resume` :3524-3593 up to MES: the GUI idle interrupt off (:1909-1935), the doorbell ranges
//!   (:2988-3001), the unhalt of MEC (:2812-2842) and of PFP and ME (:2365-2383), then the KIQ's place in
//!   RLC_CP_SCHEDULERS, which MES's `mes_v12_0_kiq_setting` writes (mes_v12_0.c:1944-1953) for its KIQ pipe.
//!
//! Offsets and masks are gc_12_0_0_offset.h's and gc_12_0_0_sh_mask.h's (`rdna4-regs`' test holds each).

use crate::gc12;
use crate::hubseq::Op;
use crate::ipfacts::hw;
use crate::nbreg::Field;
use crate::regmap::Reg;

const fn gc(name: &'static str, base_index: u8, offset: u32) -> Reg {
    Reg { name, hw_id: hw::GC, base_index, offset }
}

pub const GRBM_CNTL: Reg = gc("GRBM_CNTL", 0, 0x0DA0);
pub const GRBM_GFX_INDEX: Reg = gc("GRBM_GFX_INDEX", 1, 0x2200);
pub const GRBM_CC_GC_SA_UNIT_DISABLE: Reg = gc("GRBM_CC_GC_SA_UNIT_DISABLE", 0, 0x0FE9);
pub const GRBM_GC_USER_SA_UNIT_DISABLE: Reg = gc("GRBM_GC_USER_SA_UNIT_DISABLE", 1, 0x5B92);
pub const CC_RB_BACKEND_DISABLE: Reg = gc("CC_RB_BACKEND_DISABLE", 0, 0x13DD);
pub const GC_USER_RB_BACKEND_DISABLE: Reg = gc("GC_USER_RB_BACKEND_DISABLE", 1, 0x5B94);
pub const CC_GC_SHADER_ARRAY_CONFIG: Reg = gc("CC_GC_SHADER_ARRAY_CONFIG", 0, 0x100F);
pub const GC_USER_SHADER_ARRAY_CONFIG: Reg = gc("GC_USER_SHADER_ARRAY_CONFIG", 1, 0x5B90);
pub const TA_CNTL2: Reg = gc("TA_CNTL2", 0, 0x12E5);
pub const DB_RING_CONTROL: Reg = gc("DB_RING_CONTROL", 0, 0x13BB);
pub const SH_MEM_CONFIG: Reg = gc("SH_MEM_CONFIG", 1, 0x09E4);
pub const SH_MEM_BASES: Reg = gc("SH_MEM_BASES", 1, 0x09E3);
pub const SPI_GDBG_PER_VMID_CNTL: Reg = gc("SPI_GDBG_PER_VMID_CNTL", 0, 0x1F72);
pub const RLC_CSIB_ADDR_LO: Reg = gc("RLC_CSIB_ADDR_LO", 1, 0x0987);
pub const RLC_CSIB_ADDR_HI: Reg = gc("RLC_CSIB_ADDR_HI", 1, 0x0988);
pub const RLC_CSIB_LENGTH: Reg = gc("RLC_CSIB_LENGTH", 1, 0x0989);
pub const RLC_SRM_CNTL: Reg = gc("RLC_SRM_CNTL", 1, 0x4C80);
pub const CP_INT_CNTL_RING0: Reg = gc("CP_INT_CNTL_RING0", 0, 0x1E0A);
pub const CP_RB_DOORBELL_RANGE_LOWER: Reg = gc("CP_RB_DOORBELL_RANGE_LOWER", 0, 0x1DFA);
pub const CP_RB_DOORBELL_RANGE_UPPER: Reg = gc("CP_RB_DOORBELL_RANGE_UPPER", 0, 0x1DFB);
pub const CP_MEC_DOORBELL_RANGE_LOWER: Reg = gc("CP_MEC_DOORBELL_RANGE_LOWER", 0, 0x1DFC);
pub const CP_MEC_DOORBELL_RANGE_UPPER: Reg = gc("CP_MEC_DOORBELL_RANGE_UPPER", 0, 0x1DFD);
pub const RLC_CP_SCHEDULERS: Reg = gc("RLC_CP_SCHEDULERS", 1, 0x098A);
/// The ring test's register (`gfx_v12_0_ring_test_ring` :452-492); the rings are the next item's.
pub const SCRATCH_REG0: Reg = gc("SCRATCH_REG0", 1, 0x2040);

pub mod f {
    use super::Field;
    pub const READ_TIMEOUT: Field = Field::new(0, 0x0000_0FFF);
    // GRBM_GFX_INDEX.
    pub const INSTANCE_INDEX: Field = Field::new(0, 0x0000_007F);
    pub const SA_INDEX: Field = Field::new(8, 0x0000_0300);
    pub const SE_INDEX: Field = Field::new(16, 0x000F_0000);
    pub const SA_BROADCAST_WRITES: Field = Field::new(29, 0x2000_0000);
    pub const INSTANCE_BROADCAST_WRITES: Field = Field::new(30, 0x4000_0000);
    pub const SE_BROADCAST_WRITES: Field = Field::new(31, 0x8000_0000);
    /// GRBM_CC_GC_SA_UNIT_DISABLE and GRBM_GC_USER_SA_UNIT_DISABLE.
    pub const SA_DISABLE: Field = Field::new(8, 0x00FF_FF00);
    /// CC_RB_BACKEND_DISABLE and GC_USER_RB_BACKEND_DISABLE.
    pub const BACKEND_DISABLE: Field = Field::new(4, 0x0000_00F0);
    /// CC_GC_SHADER_ARRAY_CONFIG and GC_USER_SHADER_ARRAY_CONFIG.
    pub const INACTIVE_WGPS: Field = Field::new(16, 0xFFFF_0000);
    pub const TRUNCATE_COORD_MODE: Field = Field::new(18, 0x0004_0000);
    pub const COUNTER_CONTROL: Field = Field::new(0, 0x0000_0003);
    pub const ADDRESS_MODE: Field = Field::new(0, 0x0000_0001);
    pub const ALIGNMENT_MODE: Field = Field::new(2, 0x0000_000C);
    pub const INITIAL_INST_PREFETCH: Field = Field::new(14, 0x0000_C000);
    pub const PRIVATE_BASE: Field = Field::new(0, 0x0000_FFFF);
    pub const SHARED_BASE: Field = Field::new(16, 0xFFFF_0000);
    pub const TRAP_EN: Field = Field::new(3, 0x0000_0008);
    pub const SRM_ENABLE: Field = Field::new(0, 0x0000_0001);
    pub const AUTO_INCR_ADDR: Field = Field::new(1, 0x0000_0002);
    // CP_INT_CNTL_RING0.
    pub const CMP_BUSY_INT_ENABLE: Field = Field::new(18, 0x0004_0000);
    pub const CNTX_BUSY_INT_ENABLE: Field = Field::new(19, 0x0008_0000);
    pub const CNTX_EMPTY_INT_ENABLE: Field = Field::new(20, 0x0010_0000);
    pub const GFX_IDLE_INT_ENABLE: Field = Field::new(21, 0x0020_0000);
    // CP_MEC_RS64_CNTL.
    pub const MEC_INVALIDATE_ICACHE: Field = Field::new(4, 0x0000_0010);
    pub const MEC_PIPE0_ACTIVE: Field = Field::new(26, 0x0400_0000);
    pub const MEC_PIPE1_ACTIVE: Field = Field::new(27, 0x0800_0000);
    pub const MEC_PIPE2_ACTIVE: Field = Field::new(28, 0x1000_0000);
    pub const MEC_PIPE3_ACTIVE: Field = Field::new(29, 0x2000_0000);
}

/// `SH_MEM_ADDRESS_MODE_64` and `SH_MEM_ALIGNMENT_MODE_UNALIGNED` (navi10_enum.h:13701).
const ADDRESS_MODE_64: u32 = 0;
const ALIGNMENT_MODE_UNALIGNED: u32 = 3;
/// `DEFAULT_SH_MEM_CONFIG` (gfx_v12_0.c:263-266).
pub const DEFAULT_SH_MEM_CONFIG: u32 = f::INITIAL_INST_PREFETCH.put(f::ALIGNMENT_MODE.put(f::ADDRESS_MODE.put(0, ADDRESS_MODE_64), ALIGNMENT_MODE_UNALIGNED), 3);
/// `gmc_v12_0_init_apertures`-era constants: the shared and private apertures (gmc_v12_0.c:659, :663).
pub const SHARED_APERTURE_START: u64 = 0x2000_0000_0000_0000;
pub const PRIVATE_APERTURE_START: u64 = 0x1000_0000_0000_0000;
/// SH_MEM_BASES of VMIDs 1 to 7: the apertures' upper 16 bits (gfx_v12_0.c:1846-1852).
pub const SH_MEM_BASES_GFX: u32 = f::SHARED_BASE.put(f::PRIVATE_BASE.put(0, (PRIVATE_APERTURE_START >> 48) as u32), (SHARED_APERTURE_START >> 48) as u32);
/// `LDS_APP_BASE` and `SCRATCH_APP_BASE` (gfx_v12_0.c:1784-1785).
const LDS_APP_BASE: u32 = 1;
const SCRATCH_APP_BASE: u32 = 2;
/// SH_MEM_BASES of the compute VMIDs (:1796-1797).
pub const SH_MEM_BASES_KFD: u32 = (LDS_APP_BASE << 16) | SCRATCH_APP_BASE;
/// `first_kfd_vmid` for GC 12.0.x (gmc_v12_0.c:953-955): the graphics VMIDs are 0 to 7
/// (`id_mgr[GFXHUB].num_ids`, amdgpu_ids.c:661), the compute ones 8 to 15 (`AMDGPU_NUM_VMID`).
pub const FIRST_KFD_VMID: u32 = 8;
pub const NUM_VMID: u32 = 16;
/// `adev->gfx.me.num_pipe_per_me` for 12.0.x (gfx_v12_0.c:1418-1427): DB_RING_CONTROL's counter goes to pipe 0.
pub const ME_PIPES: u32 = 1;
/// GRBM_GFX_INDEX with every field broadcast (`gfx_v12_0_select_se_sh(adev, 0xffffffff, ...)`).
pub const GFX_INDEX_ALL: u32 = f::SE_BROADCAST_WRITES.mask | f::SA_BROADCAST_WRITES.mask | f::INSTANCE_BROADCAST_WRITES.mask;
/// The KIQ's pipe as MES's `mes_v12_0_ring_init` gives it for `AMDGPU_MES_KIQ_PIPE` (mes_v12_0.c:1741-1743 with
/// amdgpu_mes.h:68): ME 3, pipe 1, queue 0.
pub const KIQ_ME: u32 = 3;
pub const KIQ_PIPE: u32 = 1;
pub const KIQ_QUEUE: u32 = 0;

/// `WREG32_FIELD15_PREREG(GC, 0, GRBM_CNTL, READ_TIMEOUT, 0xff)` (:1829).
pub fn grbm_read_timeout(v: u32) -> u32 {
    f::READ_TIMEOUT.put(v, 0xFF)
}

/// :1850-1851: all occlusion counters to pipe 0 when ME has one pipe.
pub fn db_ring_counter(v: u32) -> u32 {
    f::COUNTER_CONTROL.put(v, u32::from(ME_PIPES <= 1))
}

/// :1806-1808.
pub fn trap_en(v: u32) -> u32 {
    f::TRAP_EN.put(v, 1)
}

/// `gfx_v12_0_rlc_enable_srm` (:1999-2006).
pub fn srm_enable(v: u32) -> u32 {
    v | f::AUTO_INCR_ADDR.mask | f::SRM_ENABLE.mask
}

/// The four enables `gfx_v12_0_enable_gui_idle_interrupt(adev, false)` clears (:1921-1930).
pub const GUI_IDLE_ENABLES: u32 = f::CNTX_BUSY_INT_ENABLE.mask | f::CNTX_EMPTY_INT_ENABLE.mask | f::CMP_BUSY_INT_ENABLE.mask | f::GFX_IDLE_INT_ENABLE.mask;

pub fn gui_idle_off(v: u32) -> u32 {
    v & !GUI_IDLE_ENABLES
}

/// The four pipes' active bits: Linux sets all four although MEC has two pipes here (:2829-2836).
pub const MEC_PIPES_ACTIVE: u32 = f::MEC_PIPE0_ACTIVE.mask | f::MEC_PIPE1_ACTIVE.mask | f::MEC_PIPE2_ACTIVE.mask | f::MEC_PIPE3_ACTIVE.mask;

/// `gfx_v12_0_cp_compute_enable(adev, true)` (:2812-2842): the instruction cache's invalidate, the pipes'
/// resets and the halt cleared, the four pipes active.
pub fn unhalt_mec(v: u32) -> u32 {
    (v & !(f::MEC_INVALIDATE_ICACHE.mask | gc12::MEC_PIPE_RESETS | gc12::f::MEC_HALT.mask)) | MEC_PIPES_ACTIVE
}

/// `gfx_v12_0_cp_gfx_enable(adev, true)` (:2365-2370): ME_HALT and PFP_HALT cleared. CE_HALT is not touched:
/// GFX 12 has no constant engine.
pub fn unhalt_gfx(v: u32) -> u32 {
    v & !(gc12::f::ME_HALT.mask | gc12::f::PFP_HALT.mask)
}

/// `gfx_v12_0_cp_gfx_enable(adev, false)` (:2369-2370), the first step of `gfx_v12_0_hw_fini` (:3914).
pub fn halt_gfx(v: u32) -> u32 {
    v | gc12::f::ME_HALT.mask | gc12::f::PFP_HALT.mask
}

/// `gfx_v12_0_cp_compute_enable(adev, false)` (:2812-2838): the instruction cache invalidated, the pipes in
/// reset and inactive, MEC halted.
pub fn halt_mec(v: u32) -> u32 {
    (v | f::MEC_INVALIDATE_ICACHE.mask | gc12::MEC_PIPE_RESETS | gc12::f::MEC_HALT.mask) & !MEC_PIPES_ACTIVE
}

/// `mes_v12_0_kiq_setting` (mes_v12_0.c:1944-1953): the KIQ's ME, pipe and queue in the low byte with bit 7.
pub fn kiq_setting(v: u32) -> u32 {
    (v & 0xFFFF_FF00) | (KIQ_ME << 5) | (KIQ_PIPE << 3) | KIQ_QUEUE | 0x80
}

/// `gfx_v12_0_select_se_sh(adev, se, sa, 0xffffffff, 0)` (:1685-1710): `None` broadcasts the field.
pub const fn select_se_sa(se: Option<u32>, sa: Option<u32>) -> u32 {
    let v = f::INSTANCE_BROADCAST_WRITES.put(0, 1);
    let v = match se {
        Some(i) => f::SE_INDEX.put(v, i),
        None => f::SE_BROADCAST_WRITES.put(v, 1),
    };
    match sa {
        Some(j) => f::SA_INDEX.put(v, j),
        None => f::SA_BROADCAST_WRITES.put(v, 1),
    }
}

/// `amdgpu_gfx_create_bitmask` (amdgpu_gfx.h): `n` low bits set.
pub const fn bitmask(n: u32) -> u32 {
    if n >= 32 { u32::MAX } else { (1 << n) - 1 }
}

/// The shader-engine layout Linux takes from the discovery table's GC info (amdgpu_discovery.c:2016-2022 for
/// version 1, :2063-2066 for version 2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Config {
    pub shader_engines: u32,
    pub sa_per_se: u32,
    pub rb_per_se: u32,
    pub cu_per_sa: u32,
}

/// What the constants learned (`backend_enable_mask`, `num_rbs`, `cu_info.number`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Learned {
    pub active_sa: u32,
    pub active_rb: u32,
    pub rbs: u32,
    pub cus: u32,
}

/// The card's registers as the constants reach them: a read, and one operation of a sequence.
pub trait Bus {
    type E;
    fn read(&mut self, r: Reg) -> Result<u32, Self::E>;
    fn op(&mut self, op: Op) -> Result<(), Self::E>;
}

/// `gfx_v12_0_get_sa_active_bitmap` (:1712-1728).
fn sa_active<B: Bus>(b: &mut B, c: &Config) -> Result<u32, B::E> {
    let cc = f::SA_DISABLE.get(b.read(GRBM_CC_GC_SA_UNIT_DISABLE)?);
    let user = f::SA_DISABLE.get(b.read(GRBM_GC_USER_SA_UNIT_DISABLE)?);
    Ok(bitmask(c.sa_per_se.saturating_mul(c.shader_engines)) & !(cc | user))
}

/// `gfx_v12_0_get_rb_active_bitmap` (:1730-1747).
fn rb_active<B: Bus>(b: &mut B, c: &Config) -> Result<u32, B::E> {
    let cc = f::BACKEND_DISABLE.get(b.read(CC_RB_BACKEND_DISABLE)?);
    let user = f::BACKEND_DISABLE.get(b.read(GC_USER_RB_BACKEND_DISABLE)?);
    Ok(bitmask(c.rb_per_se.saturating_mul(c.shader_engines)) & !(cc | user))
}

/// `gfx_v12_0_get_wgp_active_bitmap_per_sh` and `..._cu_active_bitmap_per_sh` (:5564-5595): two CUs per WGP.
fn cus_active<B: Bus>(b: &mut B, c: &Config) -> Result<u32, B::E> {
    let inactive = f::INACTIVE_WGPS.get(b.read(CC_GC_SHADER_ARRAY_CONFIG)? | b.read(GC_USER_SHADER_ARRAY_CONFIG)?);
    let wgps = !inactive & bitmask(c.cu_per_sa >> 1);
    Ok((0..16).filter(|w| wgps & (1 << w) != 0).fold(0, |m, w| m | (3 << (2 * w))))
}

/// `gfx_v12_0_constants_init` (:1824-1857) without its two VMID loops: the read timeout, the render backends
/// (`setup_rb`), the CUs per active shader array (`get_cu_info`, every shader array's bitmap read again before
/// it is selected, as Linux's loop does), TA_CNTL2 and DB_RING_CONTROL. Linux writes GC_USER_SHADER_ARRAY_CONFIG
/// only for a CU disable mask of the module parameter `disable_cu`, which is not set: no such write.
pub fn constants<B: Bus>(b: &mut B, c: &Config) -> Result<Learned, B::E> {
    b.op(Op::Modify(GRBM_CNTL, grbm_read_timeout))?;
    let active_sa = sa_active(b, c)?;
    let global_rb = rb_active(b, c)?;
    let width = c.rb_per_se.checked_div(c.sa_per_se).unwrap_or(0);
    let per_sa = bitmask(width);
    let max_sa = c.shader_engines.saturating_mul(c.sa_per_se);
    let active_rb = (0..max_sa.min(32)).filter(|i| active_sa & (1 << i) != 0).fold(0u32, |m, i| m | per_sa.checked_shl(i * width).unwrap_or(0)) & global_rb;
    let mut cus = 0;
    for se in 0..c.shader_engines {
        for sa in 0..c.sa_per_se {
            if (sa_active(b, c)? >> (se * c.sa_per_se + sa)) & 1 == 0 {
                continue;
            }
            b.op(Op::Write(GRBM_GFX_INDEX, select_se_sa(Some(se), Some(sa))))?;
            cus += cus_active(b, c)?.count_ones().min(c.cu_per_sa);
        }
    }
    b.op(Op::Write(GRBM_GFX_INDEX, select_se_sa(None, None)))?;
    b.read(TA_CNTL2)?;
    b.op(Op::Modify(DB_RING_CONTROL, db_ring_counter))?;
    Ok(Learned { active_sa, active_rb, rbs: active_rb.count_ones(), cus })
}

/// The constants' two VMID loops (:1840-1856 and `gfx_v12_0_init_compute_vmid` :1784-1812): per graphics VMID
/// SH_MEM_CONFIG and, but for VMID 0, SH_MEM_BASES; per compute VMID both and the trap enable; GRBM_GFX_CNTL
/// back on VMID 0 after each loop.
pub fn vmids<E>(mut op: impl FnMut(Op) -> Result<(), E>) -> Result<(), E> {
    for vmid in 0..FIRST_KFD_VMID {
        op(Op::Write(gc12::GRBM_GFX_CNTL, gc12::grbm_select(0, 0, 0, vmid)))?;
        op(Op::Write(SH_MEM_CONFIG, DEFAULT_SH_MEM_CONFIG))?;
        if vmid != 0 {
            op(Op::Write(SH_MEM_BASES, SH_MEM_BASES_GFX))?;
        }
    }
    op(Op::Write(gc12::GRBM_GFX_CNTL, gc12::grbm_select(0, 0, 0, 0)))?;
    for vmid in FIRST_KFD_VMID..NUM_VMID {
        op(Op::Write(gc12::GRBM_GFX_CNTL, gc12::grbm_select(0, 0, 0, vmid)))?;
        op(Op::Write(SH_MEM_CONFIG, DEFAULT_SH_MEM_CONFIG))?;
        op(Op::Write(SH_MEM_BASES, SH_MEM_BASES_KFD))?;
        op(Op::Modify(SPI_GDBG_PER_VMID_CNTL, trap_en))?;
    }
    op(Op::Write(gc12::GRBM_GFX_CNTL, gc12::grbm_select(0, 0, 0, 0)))
}

/// What SH_MEM_BASES must hold under a VMID (`None`: Linux writes none there, VMID 0).
pub const fn sh_mem_bases_of(vmid: u32) -> Option<u32> {
    match vmid {
        0 => None,
        v if v < FIRST_KFD_VMID => Some(SH_MEM_BASES_GFX),
        v if v < NUM_VMID => Some(SH_MEM_BASES_KFD),
        _ => None,
    }
}

/// Every register the stage writes (its write list), each once.
pub const WRITTEN: [Reg; 19] = [
    GRBM_CNTL,
    GRBM_GFX_INDEX,
    DB_RING_CONTROL,
    gc12::GRBM_GFX_CNTL,
    SH_MEM_CONFIG,
    SH_MEM_BASES,
    SPI_GDBG_PER_VMID_CNTL,
    RLC_CSIB_ADDR_HI,
    RLC_CSIB_ADDR_LO,
    RLC_CSIB_LENGTH,
    RLC_SRM_CNTL,
    CP_INT_CNTL_RING0,
    CP_RB_DOORBELL_RANGE_LOWER,
    CP_RB_DOORBELL_RANGE_UPPER,
    CP_MEC_DOORBELL_RANGE_LOWER,
    CP_MEC_DOORBELL_RANGE_UPPER,
    gc12::CP_MEC_RS64_CNTL,
    gc12::CP_ME_CNTL,
    RLC_CP_SCHEDULERS,
];
