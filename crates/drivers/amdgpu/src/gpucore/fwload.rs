//! GX-7a: the firmware loader. Reads the files the chosen IP modules need (`ipmod::Plan::files`) from the stick,
//! `/boot/BITSOS2/firmware/amdgpu/<name>.bin`, tries a need's fallback name when the first is missing (MES
//! `_mes_2` → `_mes`, amdgpu_mes.c:1410-1418), and checks each file's common header (`ucode::header`). The stick
//! holds plain `.bin` files: BitsOS has no zstd decoder, so `cargo xtask image` decompresses linux-firmware's
//! `.bin.zst` on the host (desk change request in plan/notes/gx-GX-7a.md); a `.zst` alone is named as such.
//! The files are never in git (public repository).

use crate::gpucore::ucode::{FwFile, FwSet, Kind, ListOpts, Why};
use crate::ipmod::{FileNeed, Role};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

/// The stick's firmware directory on V2 (the ESP is mounted at /boot; `fw::DIR` = `amdgpu` below it).
pub const STICK_DIR: &str = "/boot/BITSOS2/firmware";
/// Largest firmware file read (the 780M's largest, vcn_4_0_2, is 395 KiB; Linux's request_firmware has no
/// cap, this one only keeps a damaged stick from filling memory).
pub const MAX_FILE: usize = 16 << 20;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LoadError {
    /// Neither the name nor its fallback is on the stick.
    Missing(String),
    /// Only `<name>.zst` is there (xtask did not decompress it).
    Compressed(String),
    /// Reading failed with this errno (not "no such file").
    Read { path: String, errno: i64 },
    Bad { name: String, why: Why },
}

/// Where firmware files come from: the stick in the kernel, a map in host tests.
pub trait FwSource {
    /// `Ok(None)`: no such file.
    fn read(&mut self, path: &str) -> Result<Option<Vec<u8>>, LoadError>;
}

/// The file kind for a module's need (role and suffix of the `ipmod` firmware lists).
pub fn kind_of(role: Role, suffix: &str) -> Option<Kind> {
    Some(match (role, suffix) {
        (Role::Gfx, "_pfp") => Kind::Pfp,
        (Role::Gfx, "_me") => Kind::Me,
        (Role::Gfx, "_mec") => Kind::Mec,
        (Role::Gfx, "_rlc") => Kind::Rlc,
        (Role::Gfx, "_imu") => Kind::Imu,
        (Role::Mes, "_mes_2") | (Role::Mes, "_mes") => Kind::MesSched,
        (Role::Mes, "_mes1") => Kind::MesKiq,
        (Role::Sdma, "") => Kind::Sdma,
        (Role::Vcn, "") => Kind::Vcn,
        (Role::Display, "_dmcub") => Kind::Dmcub,
        (Role::Psp, "_toc") => Kind::Toc,
        (Role::Psp, "_ta") => Kind::Ta,
        _ => return None,
    })
}

/// `/boot/BITSOS2/firmware/<name>` (`name` is `amdgpu/...bin`).
pub fn stick_path(name: &str) -> String {
    format!("{STICK_DIR}/{name}")
}

/// What `load` did besides the files: needs it left out and why.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Notes {
    pub skipped: Vec<(String, &'static str)>,
    /// (need, file used) where the fallback was taken.
    pub fallbacks: Vec<(String, String)>,
}

/// Reads and checks every file `needs` lists. MES's KIQ file is read only with `opts.mes_kiq` (Linux does not
/// request it otherwise); a need BitsOS has no kind for is noted, not read.
pub fn load(needs: &[FileNeed], src: &mut dyn FwSource, opts: &ListOpts) -> Result<(FwSet, Notes), LoadError> {
    let mut set = FwSet::default();
    let mut notes = Notes::default();
    for n in needs {
        let Some(kind) = kind_of(n.role, n.suffix) else {
            notes.skipped.push((n.name.clone(), "no BitsOS file kind"));
            continue;
        };
        if kind == Kind::MesKiq && !opts.mes_kiq {
            notes.skipped.push((n.name.clone(), "MES KIQ pipe off (Linux amdgpu_mes_kiq=0)"));
            continue;
        }
        if set.get(kind).is_some() {
            notes.skipped.push((n.name.clone(), "kind already loaded"));
            continue;
        }
        let mut got = None;
        for name in core::iter::once(&n.name).chain(n.fallback.as_ref()) {
            if let Some(b) = src.read(&stick_path(name))? {
                got = Some((name.clone(), b));
                break;
            }
        }
        let Some((name, bytes)) = got else {
            let zst = format!("{}.zst", stick_path(&n.name));
            let err = if src.read(&zst)?.is_some() { LoadError::Compressed(zst) } else { LoadError::Missing(n.name.clone()) };
            klog::event!(Error, "gx", "fw.missing", name = n.name.as_str(), why = format!("{err:?}").as_str());
            return Err(err);
        };
        if name != n.name {
            klog::event!(Info, "gx", "fw.fallback", need = n.name.as_str(), used = name.as_str());
            notes.fallbacks.push((n.name.clone(), name.clone()));
        }
        let size = bytes.len();
        let f = FwFile::new(kind, name.clone(), bytes).map_err(|why| {
            klog::event!(Error, "gx", "fw.bad", name = name.as_str(), why = format!("{why:?}").as_str());
            LoadError::Bad { name: name.clone(), why }
        })?;
        klog::event!(Info, "gx", "fw.loaded", name = name.as_str(), bytes = size, version = f.header.ucode_version);
        set.files.push(f);
    }
    Ok((set, notes))
}

/// The stick through the VFS (kernel).
pub struct StickSource;

/// `ENOENT` as the VFS returns it.
const ENOENT: i64 = 2;

impl FwSource for StickSource {
    fn read(&mut self, path: &str) -> Result<Option<Vec<u8>>, LoadError> {
        match vfs::root_ns().read_file(path, MAX_FILE) {
            Ok(b) => Ok(Some(b)),
            Err(ENOENT) => Ok(None),
            Err(errno) => Err(LoadError::Read { path: String::from(path), errno }),
        }
    }
}
