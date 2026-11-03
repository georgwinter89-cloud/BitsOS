//! Firmware file names from IP versions: `amdgpu/<ip>_<major>_<minor>_<rev><suffix>.bin`, the rule of
//! linux-6.6 amdgpu_ucode.c `amdgpu_ucode_ip_version_decode` for GPUs without legacy names (every GC 11 and later;
//! the legacy codenames of GC 9/10 have no BitsOS module). On the stick the files come from the host's
//! linux-firmware package (xtask copies them with their licence; never into git).

use crate::ip::IpVersion;
use alloc::format;
use alloc::string::String;

/// The directory Linux and the stick use.
pub const DIR: &str = "amdgpu";

/// `amdgpu/<prefix>_<maj>_<min>_<rev><suffix>.bin`.
pub fn file(prefix: &str, v: IpVersion, suffix: &str) -> String {
    format!("{DIR}/{prefix}_{}_{}_{}{suffix}.bin", v.major, v.minor, v.rev)
}

/// One file a module needs; `fallback` is tried when `suffix` is missing (Linux: MES `_mes_2` falls back to
/// `_mes`, amdgpu_mes.c:1401-1418).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Need {
    pub suffix: &'static str,
    pub fallback: Option<&'static str>,
}

impl Need {
    pub const fn one(suffix: &'static str) -> Need {
        Need { suffix, fallback: None }
    }
}
