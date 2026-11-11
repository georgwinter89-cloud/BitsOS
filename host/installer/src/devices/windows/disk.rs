//! A USB stick opened for writing on Windows. Its volumes are locked and dismounted first and stay locked until
//! the installer is done (Windows refuses raw writes into a mounted volume, and a lock keeps Explorer and other
//! programs out). The physical drive is opened unbuffered, so every read, the read-back included, comes from the
//! stick; `Aligned` turns the installer's byte-wise I/O into whole aligned sectors. When it is dropped, the drive
//! is flushed, Windows is told to read the new partition table, and the volumes are unlocked.

use super::super::aligned::{Aligned, Raw};
use super::super::Disk;
use super::super::winparse::{self, Volume};
use super::ffi::{self, Handle};
use std::io;
use std::time::Duration;
use windows_sys::Win32::Foundation::{ERROR_ACCESS_DENIED, GENERIC_READ, GENERIC_WRITE};
use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_NO_BUFFERING;
use windows_sys::Win32::System::Ioctl::{FSCTL_DISMOUNT_VOLUME, FSCTL_LOCK_VOLUME, FSCTL_UNLOCK_VOLUME, IOCTL_DISK_GET_DRIVE_GEOMETRY_EX, IOCTL_DISK_UPDATE_PROPERTIES};

pub const NEED_ADMIN: &str = "writing a whole disk needs administrator rights: open the terminal with \"Run as administrator\" \
    (right-click Windows Terminal or the Command Prompt) and start the installer there again. Nothing was written.";

/// Explorer, an antivirus scan or the indexer may hold a volume for a moment after a stick is plugged in.
const LOCK_TRIES: u32 = 20;
const LOCK_PAUSE: Duration = Duration::from_millis(250);

/// One of the stick's volumes, locked and dismounted while this lives.
struct Locked(Handle);

impl Drop for Locked {
    fn drop(&mut self) {
        let _ = ffi::ioctl(&self.0, FSCTL_UNLOCK_VOLUME, &[], 0);
    }
}

fn lock(volume: &str, mounted: &[String]) -> Result<Locked, String> {
    let at = if mounted.is_empty() { String::new() } else { format!(" (mounted at {})", mounted.join(" ")) };
    let h = ffi::open(volume.trim_end_matches('\\'), GENERIC_READ | GENERIC_WRITE, 0).map_err(|e| format!("cannot open the stick's volume {volume}{at}: {e}. Nothing was written."))?;
    let mut last = None;
    for _ in 0..LOCK_TRIES {
        match ffi::ioctl(&h, FSCTL_LOCK_VOLUME, &[], 0) {
            Ok(_) => {
                last = None;
                break;
            }
            Err(e) => last = Some(e),
        }
        std::thread::sleep(LOCK_PAUSE);
    }
    if let Some(e) = last {
        return Err(format!(
            "the stick's volume{at} is in use ({e}): close Explorer windows, editors and anything else that shows or uses it \
             (the installer must not run from the stick itself), then try again. Nothing was written."
        ));
    }
    ffi::ioctl(&h, FSCTL_DISMOUNT_VOLUME, &[], 0).map_err(|e| format!("cannot dismount the stick's volume{at}: {e}. Nothing was written."))?;
    Ok(Locked(h))
}

pub struct RawDisk {
    // Held for their drop only, which comes after `drop` below: the volumes are unlocked, then the drive is closed.
    _locks: Vec<Locked>,
    h: Handle,
}

impl Raw for RawDisk {
    fn read_at(&mut self, off: u64, buf: &mut [u8]) -> io::Result<()> {
        ffi::read_at(&self.h, off, buf)
    }
    fn write_at(&mut self, off: u64, buf: &[u8]) -> io::Result<()> {
        ffi::write_at(&self.h, off, buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        ffi::flush(&self.h)
    }
}

impl Drop for RawDisk {
    fn drop(&mut self) {
        let _ = ffi::flush(&self.h);
        // Still locked: Windows reads the new table and mounts the new BITSOS volume once the locks go.
        let _ = ffi::ioctl(&self.h, IOCTL_DISK_UPDATE_PROPERTIES, &[], 0);
    }
}

/// Opens disk `d` (already checked: offered, unchanged, elevated) with its `volumes` locked.
pub fn open(d: &Disk, volumes: &[Volume]) -> Result<Aligned<RawDisk>, String> {
    let mut locks = Vec::new();
    for v in volumes {
        locks.push(lock(&v.name, &v.paths)?);
    }
    let path = d.path.to_string_lossy();
    let h = ffi::open(&path, GENERIC_READ | GENERIC_WRITE, FILE_FLAG_NO_BUFFERING).map_err(|e| {
        if e.raw_os_error() == Some(ERROR_ACCESS_DENIED as i32) { NEED_ADMIN.to_string() } else { format!("{path}: {e} (in use?). Nothing was written.") }
    })?;
    let (sector, size) = ffi::ioctl(&h, IOCTL_DISK_GET_DRIVE_GEOMETRY_EX, &[], 256)
        .ok()
        .and_then(|b| winparse::geometry(&b))
        .ok_or_else(|| format!("{path}: cannot read its size. Nothing was written."))?;
    if size != d.size {
        return Err(format!("refused: {path} now has {size} bytes, the list said {}. Nothing was written.", d.size));
    }
    // The partition table counts in 512-byte sectors, as firmware and BitsOS read a stick.
    if sector != 512 {
        return Err(format!("refused: {path} has {sector}-byte sectors; a BitsOS stick needs 512. Nothing was written."));
    }
    Aligned::new(RawDisk { _locks: locks, h }, size, sector).map_err(|e| format!("{path}: {e}. Nothing was written."))
}
