//! Where bitsboot came from (STICK record): device path text, the controller's PCI address (the xHCI for a
//! USB stick), the USB VID:PID when the firmware tells, the first 96 bytes of block 0 (FAT label and serial)
//! and STICK.ID. All best effort: a missing part leaves its flag clear and is named in the log.

use crate::files;
use crate::mem;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use bitsboot_pure::volume::{fat_id, parse_stick_id};
use bootinfo::{memtype, stick_flags};
use log::{info, warn};
use uefi::boot::{self, AllocateType, OpenProtocolAttributes, OpenProtocolParams, ScopedProtocol};
use uefi::proto::device_path::DevicePath;
use uefi::proto::device_path::text::{AllowShortcuts, DisplayOnly};
use uefi::proto::media::block::BlockIO;
use uefi::proto::media::file::Directory;
use uefi::proto::usb::io::UsbIo;
use uefi::proto::{ProtocolPointer, unsafe_protocol};
use uefi::{Handle, Status};

#[derive(Default)]
pub struct StickInfo {
    pub flags: u32,
    pub pci: (u16, u8, u8, u8),
    pub usb: (u16, u16),
    pub id: [u8; 16],
    pub block0: Vec<u8>,
    pub path: Vec<u8>,
}

/// Prefix of EFI_PCI_IO_PROTOCOL up to GetLocation (14 function pointers precede it, UEFI 2.10 §14.4).
#[repr(C)]
struct PciIoRaw {
    _fns: [usize; 14],
    get_location: unsafe extern "efiapi" fn(*mut PciIoRaw, *mut usize, *mut usize, *mut usize, *mut usize) -> Status,
}
#[repr(transparent)]
#[unsafe_protocol("4cf5b200-68b8-4ca5-9eec-b23e3f50029a")]
struct PciIo(PciIoRaw);

/// Shared open: an exclusive open would disconnect the drivers that keep the stick alive.
fn open<P: ProtocolPointer + ?Sized>(handle: Handle) -> Option<ScopedProtocol<P>> {
    let params = OpenProtocolParams { handle, agent: boot::image_handle(), controller: None };
    // SAFETY: GetProtocol does not take the device from its driver; the interface stays valid while the
    // ScopedProtocol lives, and bitsboot only reads through it.
    unsafe { boot::open_protocol::<P>(params, OpenProtocolAttributes::GetProtocol) }.ok()
}

pub fn probe(device: Handle, root: &mut Directory) -> StickInfo {
    let mut s = StickInfo::default();
    if let Some(dp) = open::<DevicePath>(device) {
        if let Ok(text) = dp.to_string16(DisplayOnly(false), AllowShortcuts(false)) {
            s.path = text.to_string().into_bytes();
        }
        let mut rest: &DevicePath = &dp;
        if let Some(mut pci) = boot::locate_device_path::<PciIo>(&mut rest).ok().and_then(open::<PciIo>) {
            let (mut seg, mut bus, mut dev, mut func) = (0, 0, 0, 0);
            let raw = &mut pci.0 as *mut PciIoRaw;
            // SAFETY: `raw` is the live EFI_PCI_IO_PROTOCOL of this handle (opened above); GetLocation only
            // writes the four out-parameters, which are valid locals.
            if unsafe { (pci.0.get_location)(raw, &mut seg, &mut bus, &mut dev, &mut func) }.is_success() {
                s.pci = (seg as u16, bus as u8, dev as u8, func as u8);
                s.flags |= stick_flags::PCI_VALID;
            }
        }
        let mut rest: &DevicePath = &dp;
        let usb = boot::locate_device_path::<UsbIo>(&mut rest).ok().and_then(open::<UsbIo>);
        if let Some(d) = usb.and_then(|mut u| u.device_descriptor().ok()) {
            s.usb = (d.id_vendor, d.id_product);
            s.flags |= stick_flags::USB_VALID;
        }
    }
    if let Some(b) = read_block0(device) {
        s.block0 = b;
        s.flags |= stick_flags::BLOCK0_VALID;
    }
    for (path, legacy) in [("\\BITSOS2\\STICK.ID", 0), ("\\BITSOS\\STICK.ID", stick_flags::STICK_ID_LEGACY)] {
        if let Some(id) = files::read(root, path).ok().flatten().and_then(|t| parse_stick_id(&t)) {
            s.id = id;
            s.flags |= stick_flags::STICK_ID_VALID | legacy;
            break;
        }
    }
    s
}

fn read_block0(device: Handle) -> Option<Vec<u8>> {
    let mut bio = open::<BlockIO>(device)?;
    let (media_id, size) = (bio.media().media_id(), bio.media().block_size() as u64);
    if !(96..=mem::PAGE).contains(&size) {
        return None;
    }
    // A page is aligned for any IoAlign a USB disk asks for.
    let page = mem::alloc(memtype::LOADER_DATA, AllocateType::AnyPages, 1, "block 0").ok()?;
    let ok = bio.read_blocks(media_id, 0, mem::bytes(page, size)).is_ok();
    let out = ok.then(|| mem::bytes(page, 96).to_vec());
    mem::free(page, 1);
    out
}

fn hex(id: &[u8; 16]) -> String {
    id.iter().map(|b| alloc::format!("{b:02x}")).collect()
}

/// The log lines a Mini-PC check reads: where bitsboot came from and which identity the stick has.
pub fn log(s: &StickInfo, volume: Option<(String, u64)>) {
    info!("stick: device path {}", core::str::from_utf8(&s.path).unwrap_or("?"));
    if s.flags & stick_flags::PCI_VALID != 0 {
        info!("stick: controller PCI {:04x}:{:02x}:{:02x}.{}", s.pci.0, s.pci.1, s.pci.2, s.pci.3);
    } else {
        warn!("stick: the firmware names no PCI controller for the boot volume");
    }
    if s.flags & stick_flags::USB_VALID != 0 {
        info!("stick: USB {:04x}:{:04x}", s.usb.0, s.usb.1);
    } else {
        info!("stick: no USB VID:PID (not a USB device, or the firmware does not tell)");
    }
    match s.block0.get(..96).and_then(fat_id) {
        Some(f) => info!("stick: {} volume label \"{}\" serial {}", if f.fat32 { "FAT32" } else { "FAT" }, f.label, f.serial_text()),
        None if s.flags & stick_flags::BLOCK0_VALID != 0 => warn!("stick: block 0 holds no FAT boot sector"),
        None => warn!("stick: block 0 could not be read"),
    }
    if let Some((label, size)) = volume {
        info!("stick: file system label \"{label}\", {} MiB", size >> 20);
    }
    if s.flags & stick_flags::STICK_ID_VALID != 0 {
        let from = if s.flags & stick_flags::STICK_ID_LEGACY != 0 { "\\BITSOS\\STICK.ID (V1)" } else { "\\BITSOS2\\STICK.ID" };
        info!("stick: ID {} from {from}", hex(&s.id));
    } else {
        warn!("stick: no valid STICK.ID: the kernel will not write to this stick");
    }
}

/// Safe mode (BT-5): the kernel gets no STICK.ID, so FS-7 mounts the stick read-only ("bitsboot handed over no
/// \BITSOS2\STICK.ID") and nothing on the Gaming-PC can write to it; block 0 and the USB IDs stay, so the
/// kernel still finds the stick and shows it at /boot.
pub fn withhold_id(s: &mut StickInfo) {
    if s.flags & stick_flags::STICK_ID_VALID != 0 {
        s.flags &= !(stick_flags::STICK_ID_VALID | stick_flags::STICK_ID_LEGACY);
        s.id = [0; 16];
        warn!("bitsboot stick.id_withheld: safe mode, the kernel keeps the stick read-only");
    }
}
