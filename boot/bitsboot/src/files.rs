//! Files, only on the volume bitsboot itself was loaded from (LoadedImage -> DeviceHandle -> SimpleFileSystem):
//! no other disk is ever searched (V1 rule, V2-PLAN §7). Everything is opened read-only.

use crate::mem::{self, PAGE};
use crate::{Ctx, R, fail};
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use bitsboot_pure::readall::{self, ReadError};
use uefi::boot::{self, AllocateType, OpenProtocolAttributes, OpenProtocolParams};
use uefi::proto::media::file::{Directory, File, FileAttribute, FileInfo, FileMode, FileSystemInfo, FileType, RegularFile};
use uefi::proto::media::fs::SimpleFileSystem;
use uefi::{CString16, Handle, Status};

/// The root directory of the boot volume. `device` is the loaded image's DeviceHandle.
pub fn open_volume(device: Handle) -> R<Directory> {
    let params = OpenProtocolParams { handle: device, agent: boot::image_handle(), controller: None };
    // SAFETY: GetProtocol (non-exclusive) leaves the firmware's file system driver attached; the protocol
    // stays valid while the volume exists, and bitsboot never disconnects it.
    let mut sfs = unsafe { boot::open_protocol::<SimpleFileSystem>(params, OpenProtocolAttributes::GetProtocol) }
        .ctx("the boot volume has no file system")?;
    sfs.open_volume().ctx("cannot open the boot volume")
}

/// Label and size of the boot volume as the file system driver reports them (for the log).
pub fn volume_info(root: &mut Directory) -> Option<(String, u64)> {
    let i = root.get_boxed_info::<FileSystemInfo>().ok()?;
    Some((i.volume_label().to_string(), i.volume_size()))
}

enum Node {
    File(RegularFile),
    Dir(Directory),
}

/// Opens `path` read-only; `None` if it does not exist.
fn open(root: &mut Directory, path: &str) -> R<Option<Node>> {
    let name = CString16::try_from(path).map_err(|_| fail!("bad path {path}"))?;
    let h = match root.open(&name, FileMode::Read, FileAttribute::empty()) {
        Ok(h) => h,
        Err(e) if e.status() == Status::NOT_FOUND => return Ok(None),
        Err(e) => return Err(fail!("cannot open {path} (UEFI status {:?})", e.status())),
    };
    Ok(Some(match h.into_type().ctx(path)? {
        FileType::Regular(f) => Node::File(f),
        FileType::Dir(d) => Node::Dir(d),
    }))
}

pub fn is_file(root: &mut Directory, path: &str) -> R<bool> {
    Ok(matches!(open(root, path)?, Some(Node::File(_))))
}

pub fn is_dir(root: &mut Directory, path: &str) -> R<bool> {
    Ok(matches!(open(root, path)?, Some(Node::Dir(_))))
}

fn size_of(f: &mut RegularFile, path: &str) -> R<u64> {
    Ok(f.get_boxed_info::<FileInfo>().ctx(path)?.file_size())
}

fn read_open(f: &mut RegularFile, hint: u64, path: &str) -> R<Vec<u8>> {
    readall::read_to_end(hint, |buf| f.read(buf).map_err(|e| e.status())).map_err(|e| match e {
        ReadError::NoMemory { wanted } => fail!("not enough memory to read {path} ({wanted} bytes)"),
        ReadError::Io { done, err } => fail!("cannot read {path} after {done} bytes (UEFI status {err:?})"),
    })
}

/// A whole file in loader memory, however large (the size the file system reports is only the first
/// guess); `None` if it does not exist.
pub fn read(root: &mut Directory, path: &str) -> R<Option<Vec<u8>>> {
    let Some(Node::File(mut f)) = open(root, path)? else { return Ok(None) };
    let hint = size_of(&mut f, path)?;
    read_open(&mut f, hint, path).map(Some)
}

/// A whole file straight into fresh pages of memory type `ty`: `(phys, size)`, or `None` if it does not
/// exist. The pages hold the reported size plus one byte; a file that turns out longer is read again through
/// a growing buffer and copied, so no size is trusted.
pub fn read_to_pages(root: &mut Directory, path: &str, ty: u32) -> R<Option<(u64, u64)>> {
    let Some(Node::File(mut f)) = open(root, path)? else { return Ok(None) };
    let hint = size_of(&mut f, path)?;
    let cap = hint.checked_add(1).ok_or_else(|| fail!("{path} reports an impossible size"))?;
    let pages = mem::pages_for(cap);
    let phys = mem::alloc(ty, AllocateType::AnyPages, pages, path)?;
    let buf = mem::bytes(phys, pages * PAGE);
    let mut done = 0usize;
    while done < buf.len() {
        match f.read(&mut buf[done..]) {
            Ok(0) => break,
            Ok(n) => done += n.min(buf.len() - done),
            Err(e) => {
                mem::free(phys, pages);
                return Err(fail!("cannot read {path} after {done} bytes (UEFI status {:?})", e.status()));
            }
        }
    }
    if done < buf.len() {
        buf[done..].fill(0);
        return Ok(Some((phys, done as u64)));
    }
    // Longer than reported: start over with a growing buffer.
    mem::free(phys, pages);
    ::log::warn!("{path}: longer than the reported {hint} bytes, reading it again");
    f.set_position(0).ctx(path)?;
    let data = read_open(&mut f, hint, path)?;
    let pages = mem::pages_for(data.len() as u64);
    let phys = mem::alloc(ty, AllocateType::AnyPages, pages, path)?;
    let dst = mem::bytes(phys, pages * PAGE);
    dst[..data.len()].copy_from_slice(&data);
    dst[data.len()..].fill(0);
    Ok(Some((phys, data.len() as u64)))
}

pub struct Entry {
    pub name: String,
    pub is_dir: bool,
}

/// Entries of a directory without "." and ".."; `None` if it does not exist.
pub fn list(root: &mut Directory, path: &str) -> R<Option<Vec<Entry>>> {
    let Some(Node::Dir(mut d)) = open(root, path)? else { return Ok(None) };
    let mut out = Vec::new();
    while let Some(info) = d.read_entry_boxed().ctx(path)? {
        let name = info.file_name().to_string();
        if name != "." && name != ".." {
            out.push(Entry { name, is_dir: info.is_directory() });
        }
    }
    Ok(Some(out))
}
