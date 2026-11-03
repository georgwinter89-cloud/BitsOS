//! DCN 3.1.4: the display side that exists (`crate::dcn` and the attach, flip, mode and cursor code around it).
//! It keeps its paths this wave (SN-7 edits `live.rs`); moving it under this module is a later step. Its DMCUB
//! firmware is the BIOS's, already running (loaded through the PSP); BitsOS does not reload it.

use super::{Firmware, Module, Role, State, v};
use crate::gpucore::fw::Need;

pub static MODULE: Module = Module {
    name: "dcn3_1_4",
    role: Role::Display,
    versions: &[v(3, 1, 4)],
    linux: "dm (dcn31/dcn314)",
    linux_also: &[v(3, 1, 2), v(3, 1, 3), v(3, 1, 5), v(3, 1, 6)],
    firmware: Firmware::Files { prefix: "dcn", needs: &[Need::one("_dmcub")] },
    state: State::Present("crate::dcn, attach, flip, mode, cursor (GX-2..GX-5a)"),
};
