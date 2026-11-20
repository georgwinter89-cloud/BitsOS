//! GP-GPU1: the modules of the RX 9070 XT (Navi 48: GC 12.0.1, SDMA 7.0.1, MP0/MP1 14.0.3, NBIF 6.3.1, OSSSYS 7.0.0,
//! MMHUB 4.1.0, DMU 4.1.0, VCN 5.0.0; the table measured on the Gaming-PC, `rdna4_core::ipfacts`). None has code yet:
//! each names its versions, Linux's block (linux-7.3.1 drivers/gpu/drm/amd/amdgpu unless a path says otherwise), its
//! firmware and the piece of plan/spec/gpu-rdna4.md section 4 that brings its code, so `plan()` says exactly what a card
//! cannot do yet. A module moves to a file of its own with its first code (the 780M's layout). Only GC 12.0.1 and its
//! exact companions are listed as versions: Navi 44 (GC 12.0.0) is a candidate, never a choice.
//!
//! Registers come from `crates/rdna4-regs` (generated from the same Linux tree), placed by the card's own table.

use super::{Firmware, Module, Role, State, v};
use crate::gpucore::fw::Need;

/// `psp_v14_0.c` (705 lines) over `amdgpu_psp.c`. A cold card has no secure OS (C2PMSG_81 reads 0, the trace of
/// 18.11.): the driver loads it through the bootloader with the eight parts before it, all from `_sos`, which also
/// holds the TOC and the register list (`rdna4::stage_sos`, plan/spec/gpu-rdna4.md pieces 7 and 8).
pub static PSP: Module = Module {
    name: "psp14_0_3",
    role: Role::Psp,
    versions: &[v(14, 0, 3)],
    linux: "psp_v14_0",
    linux_also: &[v(14, 0, 2), v(14, 0, 5)],
    firmware: Firmware::Files { prefix: "psp", needs: &[Need::one("_sos"), Need::one("_ta")] },
    state: State::Planned("GP-gpu-7"),
};

/// `pm/swsmu/smu14/smu_v14_0.c` (1835) and `smu_v14_0_2_ppt.c` (2995): mailbox MP1 C2PMSG_66/82/90, driver interface
/// 0x2E (:74). The firmware itself is loaded by the PSP.
pub static SMU: Module = Module {
    name: "smu14_0_3",
    role: Role::Smu,
    versions: &[v(14, 0, 3)],
    linux: "smu_v14_0",
    linux_also: &[v(14, 0, 2)],
    firmware: Firmware::Files { prefix: "smu", needs: &[Need::one("")] },
    state: State::Planned("GP-gpu-9"),
};

/// `nbif_v6_3_1.c` (736): memory size (`RCC_DEV0_EPF0_RCC_CONFIG_MEMSIZE`, :148; read by `crate::rdna4::attach`),
/// doorbell aperture, index/data pair, HDP flush and remap.
pub static NBIF: Module = Module {
    name: "nbif6_3_1",
    role: Role::Nbio,
    versions: &[v(6, 3, 1)],
    linux: "nbif_v6_3_1",
    linux_also: &[],
    firmware: Firmware::None("no firmware"),
    state: State::Present("rdna4::nbifstage (wave 42: the common hw_init as a bring-up stage on the fake and against Linux's trace; the card itself at the hardware round)"),
};

/// The memory size in MiB (`nbif_v6_3_1_get_memsize`, :148: `RCC_DEV0_EPF0_RCC_CONFIG_MEMSIZE`, nbif_6_3_1_offset.h:
/// 1809), as (base index, dword) in NBIF's bases: the one register of this block the read-only attach uses.
pub const RCC_CONFIG_MEMSIZE: (usize, u32) = (2, 0xc3);

/// Where the card's own table places that register (dword). Linux reads it at a fixed dword before there is a table
/// (0xDE3, amdgpu_discovery.c:143); the attach checks the two agree.
pub fn memsize_dword(d: &crate::gpucore::disc::Discovery) -> Option<u32> {
    d.reg(crate::gpucore::hwid::NBIF, 0, RCC_CONFIG_MEMSIZE.0, RCC_CONFIG_MEMSIZE.1)
}

/// `gmc_v12_0.c` (1175): VRAM at the MC address the card reports, a 512 MiB GART with its table in VRAM, 44 DMA bits.
pub static GMC: Module = Module {
    name: "gmc12",
    role: Role::Gmc,
    versions: &[v(12, 0, 1)],
    linux: "gmc_v12_0",
    linux_also: &[v(12, 0, 0)],
    firmware: Firmware::None("no firmware"),
    state: State::Present("rdna4::gmcstage and rdna4::gart12 (wave 42: layout, the GART table in VRAM, map and unmap with Linux's TLB flush; a bring-up stage on the fake and against Linux's trace)"),
};

/// `gfxhub_v12_0.c` (519).
pub static GFXHUB: Module = Module {
    name: "gfxhub12_0",
    role: Role::Gfxhub,
    versions: &[v(12, 0, 1)],
    linux: "gfxhub_v12_0",
    linux_also: &[v(12, 0, 0)],
    firmware: Firmware::None("no firmware"),
    state: State::Planned("GP-gpu-10"),
};

/// `mmhub_v4_1_0.c` (644).
pub static MMHUB: Module = Module {
    name: "mmhub4_1_0",
    role: Role::Mmhub,
    versions: &[v(4, 1, 0)],
    linux: "mmhub_v4_1_0",
    linux_also: &[],
    firmware: Firmware::None("no firmware"),
    state: State::Present("rdna4_core::hub41 through rdna4::gmcstage (wave 42: gart_enable as Linux's register sequence)"),
};

/// `ih_v7_0.c` (875): one MSI vector (the card has no MSI-X).
pub static IH: Module = Module {
    name: "ih7_0",
    role: Role::Ih,
    versions: &[v(7, 0, 0)],
    linux: "ih_v7_0",
    linux_also: &[],
    firmware: Firmware::None("no firmware"),
    state: State::Present("rdna4::ihstage and rdna4::ihring (wave 42: both rings through the GART, the reader with overflow and the self-interrupt; a bring-up stage on the fake and against Linux's trace)"),
};

/// `gfx_v12_0.c` (5664) with `imu_v12_0.c` (407): RS64 PFP/ME/MEC, RLC and IMU, all loaded by the PSP (`init_microcode`
/// :560-660).
pub static GFX: Module = Module {
    name: "gfx12_0_1",
    role: Role::Gfx,
    versions: &[v(12, 0, 1)],
    linux: "gfx_v12_0",
    linux_also: &[v(12, 0, 0)],
    firmware: Firmware::Files { prefix: "gc", needs: &[Need::one("_pfp"), Need::one("_me"), Need::one("_mec"), Need::one("_rlc"), Need::one("_imu")] },
    state: State::Present("rdna4::gcstage, rdna4::cpstage and rdna4::ringstage (waves 43-44: the block's start, the command processor's resume, the kernel rings with Linux's ring and IB tests; bring-up stages on the fake and against Linux's trace, the card itself at the hardware round)"),
};

/// DMU 4.1.0, which Linux runs as DCN 4.0.1 (amdgpu_discovery.c:2688) and names its DMCUB file after. The dcn401
/// sources are not in the reference tree (plan/spec/gpu-rdna4.md section 2); until piece 16 the picture is the firmware's framebuffer.
pub static DISPLAY: Module = Module {
    name: "dcn4_0_1",
    role: Role::Display,
    versions: &[v(4, 1, 0)],
    linux: "dm (dcn401)",
    linux_also: &[],
    firmware: Firmware::Named(&[("dcn_4_0_1_dmcub", "_dmcub")]),
    state: State::Planned("GP-gpu-16"),
};

/// `vcn_v5_0_0.c` with `jpeg_v5_0_0.c` (JPEG has no file of its own).
pub static VCN: Module = Module {
    name: "vcn5_0_0",
    role: Role::Vcn,
    versions: &[v(5, 0, 0)],
    linux: "vcn_v5_0_0",
    linux_also: &[],
    firmware: Firmware::Files { prefix: "vcn", needs: &[Need::one("")] },
    state: State::Planned("GP-gpu-20"),
};
