//! The image file beside the start script: readable by its owner alone, room for it on the disk, and errors that
//! name a full disk.

#[cfg(windows)]
use crate::devices;
use std::path::Path;

/// The image is its owner's alone: CONFIG.TXT in it may hold the remote-control token. Also an image an earlier
/// run made in a folder others may read.
pub fn private(p: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Err(e) = std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o600)) {
            println!("note: {} keeps its access ({e})", p.display());
        }
    }
    #[cfg(windows)]
    if let Err(e) = devices::windows::user::restrict_to_user(p) {
        println!("note: {} keeps the access of the folder it is in ({e})", p.display());
    }
}

/// The image writer names the stick mode's flag; here the person gave no --image.
pub fn plain(e: String) -> String {
    e.replacen("--image ", "", 1)
}

/// A write that failed for want of space says so, and what to do.
pub fn full_disk(e: String, dir: &Path) -> String {
    // ENOSPC on Linux; ERROR_HANDLE_DISK_FULL and ERROR_DISK_FULL on Windows.
    if ["(os error 28)", "(os error 39)", "(os error 112)"].iter().any(|c| e.contains(c)) {
        format!("{e}\nThe disk that holds {} is full: free some space there, or give another --image-dir.", dir.display())
    } else {
        e
    }
}

/// Enough free space for what the new image gets written with (Windows; Linux reports a full disk when writing).
pub fn room(dir: &Path, need: u64, size: u64) -> Result<(), String> {
    let Some(free) = free_bytes(dir) else { return Ok(()) };
    if free < need {
        return Err(format!(
            "refused: {} has {} MB free, the image needs about {} MB now. Free some space or give another --image-dir. Nothing was written.",
            dir.display(),
            free >> 20,
            need >> 20
        ));
    }
    if free < size {
        println!("note: {} has {} MB free; the image grows up to {} MB as BitsOS writes into it", dir.display(), free >> 20, size >> 20);
    }
    Ok(())
}

#[cfg(windows)]
fn free_bytes(dir: &Path) -> Option<u64> {
    // The folder may not exist yet: ask about the nearest one that does.
    dir.ancestors().find(|d| d.is_dir()).and_then(devices::windows::vm::free_bytes)
}
#[cfg(not(windows))]
fn free_bytes(_: &Path) -> Option<u64> {
    None
}
