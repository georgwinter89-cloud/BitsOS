//! NBIO 7.7.0 (NBIF): Linux `nbio_v7_7` (linux-6.6 amdgpu/nbio_v7_7.c): doorbells, HDP flush, the RCC registers
//! (`RCC_CONFIG_MEMSIZE`) and the index/data windows. Today's BAR5 constants of `dcn314_regs::bar5` are this
//! block's; GX-7 moves their users here.

use super::{Firmware, Module, Role, State, v};

pub static MODULE: Module = Module {
    name: "nbio7_7",
    role: Role::Nbio,
    versions: &[v(7, 7, 0)],
    linux: "nbio_v7_7",
    linux_also: &[],
    firmware: Firmware::None("no firmware"),
    state: State::Planned("GX-7"),
};

/// GX-7b, read-only facts (nbio_7_7_0_offset.h, segment 2 of NBIF). Bit 0 BIF_DOORBELL_APER_EN: the doorbell BAR
/// decodes (0 cold on the Mini-PC, 1 after Linux's `nbio_v7_7_enable_doorbell_aperture`).
pub const RCC_DOORBELL_APER_EN: (usize, u32) = (2, 0xc0);
/// Graphics memory in MB (`nbio_v7_7_get_memsize`, nbio_v7_7.c:60); the same dword as `layout::MEMSIZE`.
pub const RCC_CONFIG_MEMSIZE: (usize, u32) = (2, 0xc3);
/// The doorbell ranges per engine (GDC0_BIF_*_DOORBELL_RANGE) lie in segment 3 at 0x4f0af0..: far past BAR5's 512
/// KiB, reachable only through the PCIE index window (a register write), so the read-only probe names them only.
pub const GDC0_BIF_IH_DOORBELL_RANGE: (usize, u32) = (3, 0x4f0af2);
