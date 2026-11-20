//! SDMA 7.0.1 (the RX 9070 XT's two copy engines): Linux `sdma_v7_0` (linux-7.3.1 amdgpu/sdma_v7_0.c, 1830 lines).
//! Its registers sit in the GC 12 register map on GC's bases (`sdma_v7_0_get_reg_offset` :125-141).
//!
//! The version after the 780M's `sdma6`. Shared with it, as one piece of code for both: the ring with its
//! alignment and NOP padding, the write-back slots and the packets (`amdgpu_fmt::{ring, wb, sdma}`; SDMA 7
//! includes 6.0's packet header, sdma_v7_0.c:40), the walk of a commit's ring memory (`gpucore::sdma::ring_spans`)
//! and the fake's interpreter (`amdgpu_fmt::sim::Engine`). New in this version: the register offsets, an MCU
//! control where 6 has F32_CNTL, RB_CNTL as Linux's plain read-modify-write, and the two packet forms of
//! `amdgpu_fmt::sdma7` (a copy of eight dwords with CPV, a fill with COMPRESS). The registers, fields and values
//! are `rdna4_core::sdma7`; the run is `rdna4::{sdmastage, sdmarungs}` over `rdna4::sdmaq`, with the memory in
//! system pages behind the GART where the 780M's lies in its carve-out.

use super::{Firmware, Module, Role, State, v};
use crate::gpucore::fw::Need;

pub use rdna4_core::sdma7::*;

pub static MODULE: Module = Module {
    name: "sdma7_0_1",
    role: Role::Sdma,
    versions: &[v(7, 0, 1)],
    linux: "sdma_v7_0",
    linux_also: &[v(7, 0, 0)],
    firmware: Firmware::Files { prefix: "sdma", needs: &[Need::one("")] },
    state: State::Present("rdna4::sdmastage and rdna4::sdmarungs (wave 43: both engines' queue 0 as Linux's hw_init starts them with their ring tests, then Linux's IB test and the fill, copy and trap rungs; bring-up stages on the fake and against Linux's trace, the card itself at the hardware round)"),
};
