//! VCN 4.0.2 (video decode, GX-8v): Linux `vcn_v4_0` with `jpeg_v4_0` (linux-6.6 amdgpu/vcn_v4_0.c). Its
//! discovery revision carries the encode/decode-off bits 7:6 (`IpEntry::vcn_config`); the VCN table's fuse bits
//! say which codecs are off.

use super::{Firmware, Module, Role, State, v};
use crate::gpucore::fw::Need;

pub static MODULE: Module = Module {
    name: "vcn4_0_2",
    role: Role::Vcn,
    versions: &[v(4, 0, 2)],
    linux: "vcn_v4_0 + jpeg_v4_0",
    linux_also: &[v(4, 0, 0), v(4, 0, 4)],
    firmware: Firmware::Files { prefix: "vcn", needs: &[Need::one("")] },
    state: State::Planned("GX-8v"),
};
