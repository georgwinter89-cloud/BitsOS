//! Items 15: the card's firmware set and what of it lies on the stick. Names, sizes and the kicker rule come from
//! `rdna4_core::fwset` (measured against linux-firmware-amdgpu 20261019); here they meet the stick's files, whose
//! documented common header is parsed (`gpucore::ucode::header`) and nothing else: the licence forbids looking
//! inside. This file sends nothing: it says what is on the stick.

use crate::gpucore::fwload::{self, FwSource, LoadError};
use crate::gpucore::ucode::{self, Header};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use rdna4_core::fwset::{self, Use};

/// One file the card's driver wants on the stick.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Wanted {
    /// `amdgpu/<name>.bin`.
    pub file: &'static str,
    /// Size and sha256 as measured; None for a kicker board's name, which nobody has checked.
    pub expected: Option<(u32, &'static str)>,
}

/// The set for a board, by PCI device id and revision (the kicker rule, amdgpu_ucode.c:33 and :1432), with one MES
/// file for both pipes (`amdgpu_uni_mes=1`, amdgpu_drv.c:227).
pub fn wanted(device: u16, revision: u8) -> Vec<Wanted> {
    let kicker = fwset::is_kicker(device, revision);
    fwset::FILES
        .iter()
        .filter(|f| matches!(f.usage, Use::Load | Use::MesUnified))
        .map(|f| match f.kicker_file {
            Some(name) if kicker => Wanted { file: name, expected: None },
            _ => Wanted { file: f.file, expected: Some((f.bytes, f.sha256)) },
        })
        .collect()
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Found {
    Missing,
    /// Only `<name>.zst` is there.
    Compressed,
    Unreadable(i64),
    /// The file is there and its header does not hold.
    Bad { bytes: usize, why: String },
    Present { bytes: usize, header: Header },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StickFile {
    pub wanted: Wanted,
    pub found: Found,
}

impl StickFile {
    /// The file is there, parses, and has the measured size (a kicker name has no measured size).
    pub fn usable(&self) -> bool {
        match &self.found {
            Found::Present { bytes, .. } => self.wanted.expected.is_none_or(|(want, _)| want as usize == *bytes),
            _ => false,
        }
    }
}

/// Looks for every wanted file on the stick.
pub fn on_stick(wanted: &[Wanted], src: &mut dyn FwSource) -> Vec<StickFile> {
    wanted
        .iter()
        .map(|w| {
            let path = fwload::stick_path(w.file);
            let found = match src.read(&path) {
                Ok(Some(b)) => match ucode::header(&b) {
                    Ok(header) => Found::Present { bytes: b.len(), header },
                    Err(why) => Found::Bad { bytes: b.len(), why: format!("{why:?}") },
                },
                Ok(None) => match src.read(&format!("{path}.zst")) {
                    Ok(Some(_)) => Found::Compressed,
                    _ => Found::Missing,
                },
                Err(LoadError::Read { errno, .. }) => Found::Unreadable(errno),
                Err(_) => Found::Missing,
            };
            StickFile { wanted: *w, found }
        })
        .collect()
}

/// One line per file: `firmware <name>: present, 435632 bytes (as measured), header 2.0, ip 14.0, version 0x...`.
pub fn lines(files: &[StickFile]) -> Vec<String> {
    files
        .iter()
        .map(|f| {
            let what = match &f.found {
                Found::Missing => String::from("MISSING on the stick"),
                Found::Compressed => String::from("only the .zst is on the stick (not unpacked)"),
                Found::Unreadable(e) => format!("could not be read (errno {e})"),
                Found::Bad { bytes, why } => format!("{bytes} bytes, header not accepted: {why}"),
                Found::Present { bytes, header: h } => {
                    let size = match f.wanted.expected {
                        Some((want, _)) if want as usize == *bytes => "as measured",
                        Some(_) => "NOT the measured size",
                        None => "a kicker board's file, size not measured",
                    };
                    format!("present, {bytes} bytes ({size}), header {}.{}, ip {}.{}, version {:#010x}", h.header_major, h.header_minor, h.ip_major, h.ip_minor, h.ucode_version)
                }
            };
            format!("firmware {}: {what}", f.wanted.file)
        })
        .collect()
}
