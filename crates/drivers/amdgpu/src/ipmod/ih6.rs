//! Interrupt handler ring on OSSSYS 6.0.x: Linux `ih_v6_0` (linux-6.6 amdgpu/ih_v6_0.c). GX-D6: the ring
//! belongs to the device object; GX-7n: GFX/SDMA fences and VM faults arrive on it. The display's sources stay masked
//! (GX-D9): nothing here or in `gpucore::ih` writes a DCN register, their entries are counted and dropped unless DS
//! gives the source a hook (`hw::ih::IrqFeed::set_hook`) after turning it on itself.

use super::{Firmware, Module, Role, State, v};

pub static MODULE: Module = Module {
    name: "ih6_0",
    role: Role::Ih,
    versions: &[v(6, 0, 1)],
    linux: "ih_v6_0",
    linux_also: &[v(6, 0, 0), v(6, 0, 2)],
    firmware: Firmware::None("no firmware"),
    state: State::Present("gpucore::ih, hw::ih (GX-7n: ring 0 with MSI, EOP, SDMA trap and GFXHUB faults; DCN sources masked)"),
};

/// GX-7b, read-only facts (osssys_6_0_0_offset.h, segment 0 of OSSSYS). IH_RB_CNTL bit 0 RB_ENABLE (the Mini-PC:
/// 0x40610000 cold, 0x40330121 after Linux's ih_v6_0 init).
pub const IH_RB_CNTL: (usize, u32) = (0, 0x80);
pub const IH_RB_BASE: (usize, u32) = (0, 0x81);
pub const IH_RB_BASE_HI: (usize, u32) = (0, 0x82);
pub const IH_RB_RPTR: (usize, u32) = (0, 0x83);
pub const IH_RB_WPTR: (usize, u32) = (0, 0x84);
pub const IH_RB_WPTR_ADDR_HI: (usize, u32) = (0, 0x85);
pub const IH_RB_WPTR_ADDR_LO: (usize, u32) = (0, 0x86);
/// Bit 28 ENABLE: the ring's read pointer comes by doorbell.
pub const IH_DOORBELL_RPTR: (usize, u32) = (0, 0x87);
pub const IH_INT_FLOOD_CNTL: (usize, u32) = (0, 0xd5);
pub const IH_STORM_CLIENT_LIST_CNTL: (usize, u32) = (0, 0xda);
pub const IH_MSI_STORM_CTRL: (usize, u32) = (0, 0xf1);
/// GX-7m: IH_VMID_0_LUT .. IH_VMID_15_LUT (osssys_6_0_0_offset.h:30, segment 0): the VMID-to-PASID table of
/// GFXHUB's VMIDs that names a fault's process; a kernel ring writes entry `vmid` (gmc_v11_0.c:429-445), MES does
/// it for its own queues. (The MMHUB table IH_VMID_0_LUT_MM is not used: no MMHUB VMID here.)
pub const IH_VMID_0_LUT: (usize, u32) = (0, 0x0);

/// NBIO's interrupt side, beyond BAR5: through the RSMU window as Linux's indirect access (nbio_7_7_0_offset.h).
/// Segment 3 / segment 5 of NBIF.
pub const IH_DOORBELL_RANGE: (usize, u32) = (3, 0x4f_0af2);
pub const BIF_INTERRUPT_CNTL: (usize, u32) = (5, 0x8e11);
pub const BIF_INTERRUPT_CNTL2: (usize, u32) = (5, 0x8e12);

/// The ring: 256 KiB, ring 0 alone (ih_v6_0.c:542-548), entries of 32 bytes.
pub const RING_BYTES: u32 = 256 * 1024;
pub const ENTRY_BYTES: u32 = 32;
/// `AMDGPU_NAVI10_DOORBELL_IH << 1` (amdgpu_doorbell.h:214, ih_v6_0.c:547): BAR2's dword.
pub const DOORBELL_DW: u32 = 0x178 << 1;

pub mod f {
    pub const RB_ENABLE: u32 = 1 << 0;
    pub const RB_SIZE_SHIFT: u32 = 1;
    pub const RB_SIZE_MASK: u32 = 0x3f << 1;
    pub const WPTR_WRITEBACK_ENABLE: u32 = 1 << 8;
    pub const WPTR_OVERFLOW_ENABLE: u32 = 1 << 16;
    pub const ENABLE_INTR: u32 = 1 << 17;
    pub const MC_SNOOP: u32 = 1 << 20;
    pub const RPTR_REARM: u32 = 1 << 21;
    pub const MC_RO: u32 = 1 << 22;
    pub const MC_VMID_MASK: u32 = 0xf << 24;
    pub const MC_SPACE_MASK: u32 = 0x7 << 28;
    pub const MC_SPACE_GPUVM: u32 = 4 << 28;
    pub const WPTR_OVERFLOW_CLEAR: u32 = 1 << 31;
    /// IH_RB_WPTR's RB_OVERFLOW.
    pub const RB_OVERFLOW: u32 = 1 << 0;
    pub const DOORBELL_ENABLE: u32 = 1 << 28;
    pub const FLOOD_CNTL_ENABLE: u32 = 1 << 3;
    pub const CLIENT18_IS_STORM_CLIENT: u32 = 1 << 18;
    pub const MSI_STORM_DELAY_MASK: u32 = 0xffff;
    pub const MSI_STORM_DELAY: u32 = 3;
    /// BIF_BX1_INTERRUPT_CNTL.
    pub const IH_DUMMY_RD_OVERRIDE: u32 = 1 << 0;
    pub const IH_REQ_NONSNOOP_EN: u32 = 1 << 3;
    /// GDC0_BIF_IH_DOORBELL_RANGE.
    pub const RANGE_OFFSET_MASK: u32 = 0xffc;
    pub const RANGE_SIZE_MASK: u32 = 0x1f << 16;
    pub const RANGE_SIZE_SHIFT: u32 = 16;
}

/// ih_v6_0_rb_cntl + RPTR_REARM of ih_v6_0_enable_ring: the ring still off.
pub fn rb_cntl(cur: u32, ring_bytes: u32, msi: bool) -> u32 {
    let size = (ring_bytes / 4).trailing_zeros();
    let mut v = cur & !(f::RB_SIZE_MASK | f::MC_SPACE_MASK | f::MC_RO | f::MC_VMID_MASK | f::RPTR_REARM);
    v |= f::MC_SPACE_GPUVM | f::WPTR_OVERFLOW_CLEAR | f::WPTR_OVERFLOW_ENABLE | size << f::RB_SIZE_SHIFT | f::WPTR_WRITEBACK_ENABLE | f::MC_SNOOP;
    if msi {
        v |= f::RPTR_REARM;
    }
    v
}

/// ih_v6_0_toggle_ring_interrupts of ring 0.
pub fn toggled(cur: u32, on: bool) -> u32 {
    if on { cur | f::RB_ENABLE | f::ENABLE_INTR } else { cur & !(f::RB_ENABLE | f::ENABLE_INTR) }
}

/// ih_v6_0_doorbell_rptr: 0x100002f0 on the Mini-PC under Linux.
pub fn doorbell_rptr() -> u32 {
    DOORBELL_DW | f::DOORBELL_ENABLE
}

/// nbio_v7_7_ih_doorbell_range: OFFSET in dwords at bit 2, SIZE 2.
pub fn doorbell_range(cur: u32) -> u32 {
    (cur & !(f::RANGE_OFFSET_MASK | f::RANGE_SIZE_MASK)) | (DOORBELL_DW << 2 & f::RANGE_OFFSET_MASK) | 2 << f::RANGE_SIZE_SHIFT
}

/// nbio_v7_7_ih_control, with the ring in the carve-out: the dummy read off under MSI; IH_REQ_NONSNOOP_EN "if ring is in
/// non-cacheable memory, e.g., vram" (Linux's own comment; its ring is in GTT, this one is not).
pub fn bif_interrupt_cntl(cur: u32) -> u32 {
    (cur & !f::IH_DUMMY_RD_OVERRIDE) | f::IH_REQ_NONSNOOP_EN
}

/// One interrupt vector (amdgpu_ih_decode_iv_helper).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Iv {
    pub client: u8,
    pub src: u8,
    pub ring_id: u8,
    pub vmid: u8,
    /// 0: GFXHUB, 1: MMHUB.
    pub vmid_src: u8,
    pub timestamp: u64,
    pub timestamp_src: u8,
    pub pasid: u16,
    pub node_id: u8,
    pub data: [u32; 4],
}

impl Iv {
    pub fn decode(d: &[u32; 8]) -> Iv {
        Iv {
            client: d[0] as u8,
            src: (d[0] >> 8) as u8,
            ring_id: (d[0] >> 16) as u8,
            vmid: (d[0] >> 24) as u8 & 0xf,
            vmid_src: (d[0] >> 31) as u8,
            timestamp: u64::from(d[1]) | u64::from(d[2] & 0xffff) << 32,
            timestamp_src: (d[2] >> 31) as u8,
            pasid: d[3] as u16,
            node_id: (d[3] >> 16) as u8,
            data: [d[4], d[5], d[6], d[7]],
        }
    }
    /// The reverse (a fake chip's write).
    pub fn encode(&self) -> [u32; 8] {
        [
            u32::from(self.client) | u32::from(self.src) << 8 | u32::from(self.ring_id) << 16 | u32::from(self.vmid & 0xf) << 24 | u32::from(self.vmid_src & 1) << 31,
            self.timestamp as u32,
            ((self.timestamp >> 32) as u32 & 0xffff) | u32::from(self.timestamp_src & 1) << 31,
            u32::from(self.pasid) | u32::from(self.node_id) << 16,
            self.data[0],
            self.data[1],
            self.data[2],
            self.data[3],
        ]
    }
    /// A fault's page (gmc_v11_0_process_interrupt).
    pub fn fault_addr(&self) -> u64 {
        u64::from(self.data[0]) << 12 | u64::from(self.data[1] & 0xf) << 44
    }
}

/// soc15_ih_clientid.h.
pub const CLIENT_IH: u8 = 0x00;
pub const CLIENT_DCN: u8 = 0x04;
pub const CLIENT_GFX: u8 = 0x0a;
pub const CLIENT_VMC: u8 = 0x12;
pub const CLIENT_GRBM_CP: u8 = 0x14;
/// irqsrcs_gfx_11_0_0.h.
pub const SRC_UTCL2_FAULT: u8 = 0;
pub const SRC_SDMA_TRAP: u8 = 49;
pub const SRC_CP_EOP: u8 = 181;
pub const SRC_PRIV_REG: u8 = 184;
pub const SRC_PRIV_INSTR: u8 = 185;
/// VMC_1_0__SRCID__VM_FAULT.
pub const SRC_VMC_FAULT: u8 = 0;

/// Where an entry goes. The display's sources are named for DS (ds-roadmap.md section 6): irqsrcs_dcn_1_0.h,
/// irq_service_dcn314.c:96-112.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    Eop,
    SdmaTrap,
    GfxFault,
    MmhubFault,
    PrivReg,
    PrivInstr,
    IhSelf,
    /// client 4 src 9: every HPD and HPD RX; the context tells them apart.
    DcnHpd,
    /// src 0x3C..0x41: OTG VSTARTUP, Linux's vblank. Enabling it is OTG_GLOBAL_SYNC_STATUS's VSTARTUP_INT_EN
    /// (irq_service_dcn314.c:274), a DCN register: milestone 5's, not GX-7n's (GX-D9).
    DcnVblank,
    /// src 0x1E..0x23: OTG vertical interrupt 0.
    DcnVline0,
    /// src 0x4F..0x54: HUBP flip.
    DcnFlip,
    /// src 0x57..0x5C: OTG VUPDATE, no lock.
    DcnVupdate,
    /// src 0x68: DMCUB outbox.
    DcnOutbox,
    DcnOther,
    Unknown,
}

impl Source {
    pub const ALL: [Source; 15] = [
        Source::Eop,
        Source::SdmaTrap,
        Source::GfxFault,
        Source::MmhubFault,
        Source::PrivReg,
        Source::PrivInstr,
        Source::IhSelf,
        Source::DcnHpd,
        Source::DcnVblank,
        Source::DcnVline0,
        Source::DcnFlip,
        Source::DcnVupdate,
        Source::DcnOutbox,
        Source::DcnOther,
        Source::Unknown,
    ];
    pub const COUNT: usize = Source::ALL.len();

    pub fn index(self) -> usize {
        self as usize
    }
    pub fn name(self) -> &'static str {
        match self {
            Source::Eop => "EOP",
            Source::SdmaTrap => "SDMA trap",
            Source::GfxFault => "GFXHUB fault",
            Source::MmhubFault => "MMHUB fault",
            Source::PrivReg => "privileged register",
            Source::PrivInstr => "privileged instruction",
            Source::IhSelf => "IH self",
            Source::DcnHpd => "DCN HPD",
            Source::DcnVblank => "DCN vblank",
            Source::DcnVline0 => "DCN vline0",
            Source::DcnFlip => "DCN flip",
            Source::DcnVupdate => "DCN VUPDATE",
            Source::DcnOutbox => "DCN outbox",
            Source::DcnOther => "DCN other",
            Source::Unknown => "unknown",
        }
    }
    pub fn is_display(self) -> bool {
        matches!(self, Source::DcnHpd | Source::DcnVblank | Source::DcnVline0 | Source::DcnFlip | Source::DcnVupdate | Source::DcnOutbox | Source::DcnOther)
    }
}

/// The routing table.
pub fn route(client: u8, src: u8) -> Source {
    match (client, src) {
        (CLIENT_GRBM_CP, SRC_CP_EOP) => Source::Eop,
        (CLIENT_GRBM_CP, SRC_PRIV_REG) => Source::PrivReg,
        (CLIENT_GRBM_CP, SRC_PRIV_INSTR) => Source::PrivInstr,
        (CLIENT_GFX, SRC_SDMA_TRAP) => Source::SdmaTrap,
        (CLIENT_GFX, SRC_UTCL2_FAULT) => Source::GfxFault,
        (CLIENT_VMC, SRC_VMC_FAULT) => Source::MmhubFault,
        (CLIENT_IH, 0) => Source::IhSelf,
        (CLIENT_DCN, 9) => Source::DcnHpd,
        (CLIENT_DCN, 0x1e..=0x23) => Source::DcnVline0,
        (CLIENT_DCN, 0x3c..=0x41) => Source::DcnVblank,
        (CLIENT_DCN, 0x4f..=0x54) => Source::DcnFlip,
        (CLIENT_DCN, 0x57..=0x5c) => Source::DcnVupdate,
        (CLIENT_DCN, 0x68) => Source::DcnOutbox,
        (CLIENT_DCN, _) => Source::DcnOther,
        _ => Source::Unknown,
    }
}
