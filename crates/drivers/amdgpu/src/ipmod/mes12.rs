//! MES 12 (the RX 9070 XT's scheduler firmware, both pipes from one `uni_mes` file): Linux `mes_v12_0`
//! (linux-7.3.1 amdgpu/mes_v12_0.c, 2157 lines) over `amdgpu_mes.c`. Its registers sit in the GC 12 register map.
//!
//! The version after the 780M's `mes11`. Shared with it: nothing but the idea. MES 12's frames are
//! `include/mes_v12_api_def.h`'s, packed to 8 bytes where MES 11's are packed to 4, so every field after the
//! first 64-bit one lies elsewhere (`rdna4_core::mes12api`, held against the header by the C compiler); the MQD is
//! `v12_compute_mqd` (`rdna4_core::mqd12`); with one file both pipes start, the KIQ pipe's queue by registers and
//! the scheduler's mapped through it; MISC goes to the KIQ pipe. The run is `rdna4::{messtage, meskq}` over
//! `rdna4::mesring`, with every memory in system pages behind the GART; from the stage `mes` on the TLB flush
//! after a GART change goes through MES (`rdna4::mesflush`).

use super::{Firmware, Module, Role, State, v};
use crate::gpucore::fw::Need;

pub use rdna4_core::mes12::*;

pub static MODULE: Module = Module {
    name: "mes12",
    role: Role::Mes,
    versions: &[v(12, 0, 1)],
    linux: "mes_v12_0",
    linux_also: &[v(12, 0, 0)],
    firmware: Firmware::Files { prefix: "gc", needs: &[Need::one("_uni_mes")] },
    state: State::Present("rdna4::messtage and rdna4::meskq (wave 44: both pipes started as Linux's mes_v12_0_hw_init starts them, its commands on the rings, the kernel's five queues mapped through it, the TLB flush through MES; bring-up stages on the fake and against Linux's trace, the card itself at the hardware round)"),
};
