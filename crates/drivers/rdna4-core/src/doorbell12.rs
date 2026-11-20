//! Piece 13: the command processor's doorbells on this card. Linux numbers them in `soc24_init_doorbell_index`
//! (soc24.c:267-288) with `AMDGPU_NAVI10_DOORBELL_ASSIGNMENT` (amdgpu_doorbell.h:186-236), in 64-bit doorbells; a
//! ring's `doorbell_index` is the BAR2 dword `index << 1` (gfx_v12_0.c:993, :1022; mes_v12_0.c:1751-1754), and
//! the CP's two ranges are byte offsets `(index * 2) << 2` (`gfx_v12_0_cp_set_doorbell_range` :2988-3001).
//! NBIF routes the GC block's doorbells through its entries 0 (GFX) and 3 (MES), written by
//! `nbif_v6_3_1_gc_doorbell_init` (nbif_v6_3_1.c:300-309: NBIO 6.3.1 is below 7.11.4, so the plain registers).
//! STRAP0's revision is the one Linux's `soc24_common_early_init` reads (soc24.c:345, nbif_v6_3_1.c:112-122).

use crate::ipfacts::hw;
use crate::nbreg::Field;
use crate::regmap::Reg;

pub const KIQ: u32 = 0x000;
pub const MEC_RING0: u32 = 0x003;
pub const MEC_RING7: u32 = 0x00A;
pub const MES_RING0: u32 = 0x00B;
pub const MES_RING1: u32 = 0x00C;
pub const USERQUEUE_START: u32 = 0x00D;
pub const USERQUEUE_END: u32 = 0x08A;
pub const GFX_RING0: u32 = 0x08B;
pub const GFX_RING1: u32 = 0x08C;
pub const GFX_USERQUEUE_START: u32 = 0x08D;
pub const GFX_USERQUEUE_END: u32 = 0x0FF;
/// `AMDGPU_NAVI10_DOORBELL64_FIRST_NON_CP`: SDMA's first engine; every doorbell below belongs to the CP or MES.
pub const FIRST_NON_CP: u32 = 0x100;

/// The BAR2 dword a ring with this index rings (`index << 1`).
pub const fn dword(index: u32) -> u32 {
    index << 1
}

/// A range register's value for this index (`(index * 2) << 2`, a byte offset into BAR2).
pub const fn range(index: u32) -> u32 {
    (index * 2) << 2
}

/// The four range registers' values as Linux writes them: graphics from GFX ring 0 to the last graphics user
/// queue, compute from the KIQ to the last compute user queue.
pub const RB_LOWER: u32 = range(GFX_RING0);
pub const RB_UPPER: u32 = range(GFX_USERQUEUE_END);
pub const MEC_LOWER: u32 = range(KIQ);
pub const MEC_UPPER: u32 = range(USERQUEUE_END);

/// The doorbells of the kernel's own rings, as BAR2 dwords: the graphics ring, compute ring `i` (MEC ring 0 + i),
/// MES's scheduler and its KIQ.
pub const GFX_RING0_DW: u32 = dword(GFX_RING0);
pub const fn mec_ring_dw(i: u32) -> u32 {
    dword(MEC_RING0 + i)
}
pub const MES_SCHED_DW: u32 = dword(MES_RING0);
pub const MES_KIQ_DW: u32 = dword(MES_RING1);

const fn nbif(name: &'static str, offset: u32) -> Reg {
    Reg { name, hw_id: hw::NBIF, base_index: 2, offset }
}

pub const ENTRY_0: Reg = nbif("GDC_S2A0_S2A_DOORBELL_ENTRY_0_CTRL", 0x1CB);
pub const ENTRY_3: Reg = nbif("GDC_S2A0_S2A_DOORBELL_ENTRY_3_CTRL", 0x1CE);
/// Linux's two values: enable, AWID 3 (GFX) or 6 (MES), AWADDR bits 31:28 = 3, offset and size 0.
pub const ENTRY_0_GFX: u32 = 0x3000_0007;
pub const ENTRY_3_MES: u32 = 0x3000_000D;
pub const PORT_ENABLE: Field = Field::new(0, 0x0000_0001);
pub const PORT_AWID: Field = Field::new(1, 0x0000_003E);
pub const PORT_AWADDR_31_28: Field = Field::new(28, 0xF000_0000);

pub const STRAP0: Reg = nbif("RCC_STRAP0_RCC_DEV0_EPF0_STRAP0", 0x1C);
pub const ATI_REV_ID: Field = Field::new(24, 0x0F00_0000);

/// `nbif_v6_3_1_get_rev_id`: the revision Linux's `adev->rev_id` holds.
pub const fn rev_id(strap0: u32) -> u32 {
    ATI_REV_ID.get(strap0)
}
