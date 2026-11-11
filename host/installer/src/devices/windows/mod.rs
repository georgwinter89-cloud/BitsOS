//! Disks on Windows. The list asks every `\\.\PhysicalDriveN` for its storage descriptor (no administrator rights
//! needed) and keeps the disks whose bus is USB, as on Linux; only those with a removable medium are offered. A disk is the system's
//! when a volume on it holds the Windows folder, the partition Windows booted from (`HKLM\SYSTEM\Setup`
//! SystemPartition) or a page file; if the Windows folder's disk cannot be found, nothing is offered. Writing
//! (`disk`) needs administrator rights, locks and dismounts every volume of the stick and writes the physical drive
//! in whole sectors.

mod disk;
mod ffi;
pub mod user;
pub mod vm;

use super::winparse::{self, Volume};
use super::{BlockDev, Devices, Disk};
use windows_sys::Win32::Storage::FileSystem::IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS;
use windows_sys::Win32::System::Ioctl::{IOCTL_DISK_GET_DRIVE_GEOMETRY_EX, IOCTL_STORAGE_QUERY_PROPERTY};

pub struct Windows;

/// PhysicalDrive numbers tried. Windows numbers disks densely from 0; a gap only follows a removed disk.
const MAX_DRIVES: u32 = 128;

fn same_volume(a: &str, b: &str) -> bool {
    a.trim_end_matches('\\').eq_ignore_ascii_case(b.trim_end_matches('\\'))
}

/// Every volume with its disks, mount points and system role.
fn scan() -> Result<Vec<Volume>, String> {
    let refuse = |what: &str, e: std::io::Error| format!("cannot find {what} ({e}); no disk is offered");
    let (windir, root) = ffi::windows_root().map_err(|e| refuse("the running Windows' volume", e))?;
    let win = ffi::volume_at(&root).map_err(|e| refuse("the running Windows' volume", e))?;
    let boot = ffi::registry("SYSTEM\\Setup", "SystemPartition", false).map(|w| ffi::from_wide(&w));
    let pages: Vec<(String, String)> = ffi::registry("SYSTEM\\CurrentControlSet\\Control\\Session Manager\\Memory Management", "ExistingPageFiles", true)
        .map(|w| winparse::multi_sz(&w))
        .unwrap_or_default()
        .iter()
        .filter_map(|p| winparse::page_file_drive(p))
        .filter_map(|d| ffi::volume_at(&d).ok().map(|v| (v, d)))
        .collect();
    let mut out = Vec::new();
    for name in ffi::volumes().map_err(|e| refuse("the volumes", e))? {
        let paths = ffi::mount_paths(&name).map(|w| winparse::multi_sz(&w)).unwrap_or_default();
        let role = if same_volume(&name, &win) {
            Some(format!("it holds the running Windows ({windir})"))
        } else if boot.as_deref().is_some_and(|b| ffi::nt_device(&name).is_some_and(|d| d.eq_ignore_ascii_case(b))) {
            Some("it holds the partition Windows started from".to_string())
        } else {
            pages.iter().find(|(v, _)| same_volume(v, &name)).map(|(_, d)| format!("it holds the page file on {d}"))
        };
        let disks = ffi::open(name.trim_end_matches('\\'), 0, 0).and_then(|h| ffi::ioctl(&h, IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS, &[], 4096)).ok().and_then(|b| winparse::extents(&b));
        match (disks, &role) {
            (Some(disks), _) => out.push(Volume { name, disks, paths, role }),
            // A system volume whose disk is unknown could be on any of them.
            (None, Some(r)) => return Err(format!("cannot tell which disk {r} lies on; no disk is offered")),
            // Optical drives, RAM disks, a stick's volume we locked ourselves: on no disk we would offer.
            (None, None) => {}
        }
    }
    if !out.iter().any(|v| same_volume(&v.name, &win)) {
        return Err("the running Windows' volume is not among the volumes; no disk is offered".into());
    }
    Ok(out)
}

/// Disk `n` if it is a USB disk (`None` for anything else or no such drive); `offered` judges the rest.
fn look(n: u32, vols: &[Volume]) -> Option<Disk> {
    let h = ffi::open(&format!("\\\\.\\PhysicalDrive{n}"), 0, 0).ok()?;
    // STORAGE_PROPERTY_QUERY { StorageDeviceProperty, PropertyStandardQuery }, padded to its 12 bytes.
    let d = winparse::descriptor(&ffi::ioctl(&h, IOCTL_STORAGE_QUERY_PROPERTY, &[0u8; 12], 4096).ok()?)?;
    if d.bus != winparse::BUS_USB {
        return None;
    }
    let geom = ffi::ioctl(&h, IOCTL_DISK_GET_DRIVE_GEOMETRY_EX, &[], 256).ok().and_then(|b| winparse::geometry(&b));
    Some(winparse::disk(n, &d, geom, vols))
}

/// The drive number in `\\.\PhysicalDriveN`.
fn number(d: &Disk) -> Option<u32> {
    d.path.to_str()?.strip_prefix("\\\\.\\PhysicalDrive")?.parse().ok()
}

impl Devices for Windows {
    fn list(&self) -> Result<Vec<Disk>, String> {
        let vols = scan()?;
        Ok((0..MAX_DRIVES).filter_map(|n| look(n, &vols)).collect())
    }

    fn open(&self, d: &Disk) -> Result<Box<dyn BlockDev>, String> {
        if let Some(why) = d.why_not() {
            return Err(format!("refused: {} is not offered ({why})", d.id));
        }
        let n = number(d).ok_or_else(|| format!("refused: {} is not a physical drive", d.path.display()))?;
        if !user::elevated() {
            return Err(disk::NEED_ADMIN.into());
        }
        // A fresh look just before: the same stick, still not the system's.
        let vols = scan()?;
        match look(n, &vols) {
            Some(now) if now.same_as(d) && now.offered() => {}
            _ => return Err(format!("refused: {} changed since the list (unplugged, swapped or now in use). Nothing was written.", d.id)),
        }
        Ok(Box::new(disk::open(d, &winparse::volumes_of(n, &vols))?))
    }
}
