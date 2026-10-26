extern crate std;
use super::*;
use std::vec;

fn sample(buf: &mut [u8]) -> usize {
    let mut w = Writer::new(buf).unwrap();
    w.config(b"token=abc\nscreen=2560x1440\n").unwrap();
    w.framebuffer(&Framebuffer {
        phys: 0x7C_0000_0000,
        virt: layout::VMAP_BASE,
        size: 2560 * 1440 * 4,
        width: 2560,
        height: 1440,
        stride: 2560,
        format: PixelFormat::Bgrx,
        masks: [0; 4],
        mode_index: 3,
    })
    .unwrap();
    let modes = [
        Mode { index: 0, width: 1024, height: 768, format: PixelFormat::Bgrx, stride: 1024 },
        Mode { index: 3, width: 2560, height: 1440, format: PixelFormat::Bgrx, stride: 2560 },
    ];
    w.mode_list(3, modes.iter().copied()).unwrap();
    w.module(&Module { kind: module_kind::INITRAMFS, phys: 0x1000_0000, size: 12345, name: b"INITRD" }).unwrap();
    w.module(&Module { kind: module_kind::FIRMWARE, phys: 0x2000_0000, size: 99, name: b"rtw8852b_fw-2.bin" }).unwrap();
    w.stick(&Stick {
        flags: stick_flags::STICK_ID_VALID | stick_flags::PCI_VALID,
        pci_seg: 0,
        pci_bus: 0xC7,
        pci_dev: 0,
        pci_fn: 3,
        usb_vid: 0x090C,
        usb_pid: 0x1000,
        stick_id: [7; 16],
        block0: [1; 96],
        device_path: b"PciRoot(0x0)/Pci(0x8,0x1)/Pci(0x0,0x3)/USB(0x0,0x0)/HD(1,MBR)",
    })
    .unwrap();
    w.generation(Generation { number: 4, reason: gen_reason::NEWEST_OK, skipped: 0, flags: 0 }).unwrap();
    w.tsc(Tsc { hint_hz: 3_800_006_000, ..Default::default() }).unwrap();
    let descs = [
        MemDesc { ty: memtype::CONVENTIONAL, phys: 0x100000, virt: 0, pages: 100, attr: 0xF },
        MemDesc { ty: memtype::BITS_KEEP, phys: 0x1000, virt: 0, pages: 1, attr: 0xF },
        MemDesc { ty: memtype::CONVENTIONAL, phys: 0x1_0000_0000, virt: 0, pages: 0x10000, attr: 0xF },
    ];
    w.memory_map(descs.iter().copied()).unwrap();
    w.len()
}

#[test]
fn round_trip_all_records() {
    let mut buf = vec![0u8; 8192];
    let len = sample(&mut buf);
    let bi = BootInfo::parse(&buf).unwrap();
    assert_eq!(bi.total_size(), len);
    assert_eq!(bi.config().unwrap(), b"token=abc\nscreen=2560x1440\n");
    let fb = bi.framebuffer().unwrap();
    assert_eq!((fb.width, fb.height, fb.stride, fb.format), (2560, 1440, 2560, PixelFormat::Bgrx));
    let ml = bi.mode_list().unwrap();
    assert_eq!(ml.len(), 2);
    assert_eq!(ml.current, 3);
    assert_eq!(ml.iter().nth(1).unwrap().width, 2560);
    let mods: std::vec::Vec<_> = bi.modules().collect();
    assert_eq!(mods.len(), 2);
    assert_eq!(mods[1].name, b"rtw8852b_fw-2.bin");
    let s = bi.stick().unwrap();
    assert_eq!((s.pci_bus, s.pci_fn, s.usb_vid), (0xC7, 3, 0x090C));
    assert_eq!(s.stick_id, [7; 16]);
    assert!(s.device_path.starts_with(b"PciRoot"));
    assert_eq!(bi.generation().unwrap().number, 4);
    let mm = bi.memory_map().unwrap();
    let phys: std::vec::Vec<u64> = mm.iter().map(|d| d.phys).collect();
    assert_eq!(phys, [0x1000, 0x100000, 0x1_0000_0000], "sorted by phys");
    assert_eq!(mm.ram_top(), 0x1_0000_0000 + 0x10000 * 4096);
    assert!(bi.rsdp().is_none(), "absent records are None, not errors");
}

#[test]
fn rejects_garbage() {
    assert_eq!(BootInfo::parse(&[0u8; 8]).err(), Some(ParseError::TooShort));
    let mut buf = vec![0u8; 64];
    assert_eq!(BootInfo::parse(&buf).err(), Some(ParseError::BadMagic));
    Writer::new(&mut buf).unwrap();
    buf[8] = 9;
    assert_eq!(BootInfo::parse(&buf).err(), Some(ParseError::UnsupportedVersion(9)));
}

#[test]
fn truncated_record_is_an_error() {
    let mut buf = vec![0u8; 8192];
    let len = sample(&mut buf);
    // Claim one more record than written: the parser must notice before any getter runs.
    let count = rd_u32(&buf, 24) + 1;
    buf[24..28].copy_from_slice(&count.to_le_bytes());
    // total_size points to the end, so the extra record header would read past it.
    assert!(matches!(BootInfo::parse(&buf[..len]), Err(ParseError::RecordOverrun { .. })));
}

#[test]
fn unknown_tags_are_skipped() {
    let mut buf = vec![0u8; 4096];
    let mut w = Writer::new(&mut buf).unwrap();
    w.record(0xDEAD, &[1, 2, 3]).unwrap();
    w.config(b"x=1").unwrap();
    let bi = BootInfo::parse(&buf).unwrap();
    assert_eq!(bi.config().unwrap(), b"x=1");
}

#[test]
fn full_buffer_is_reported() {
    let mut buf = vec![0u8; 48];
    let mut w = Writer::new(&mut buf).unwrap();
    assert!(matches!(w.config(&[0u8; 64]), Err(WriteError::Full { .. })));
}

#[test]
fn resume_appends() {
    let mut buf = vec![0u8; 8192];
    let len = sample(&mut buf);
    let mut w = Writer::resume(&mut buf).unwrap();
    assert_eq!(w.len(), len);
    w.reload(Reload { counter: 3 }).unwrap();
    let bi = BootInfo::parse(&buf).unwrap();
    assert_eq!(bi.reload().unwrap().counter, 3);
    assert_eq!(bi.generation().unwrap().number, 4);
}

#[test]
fn layout_math() {
    assert_eq!(layout::virt_to_phys(layout::phys_to_virt(0x1234)), Some(0x1234));
    assert_eq!(layout::virt_to_phys(layout::KERNEL_BASE), None);
    assert!(memtype::is_ram(memtype::BITS_KEEP));
    assert!(!memtype::is_free_after_handover(memtype::BITS_KEEP));
    assert!(memtype::is_free_after_handover(memtype::BOOT_SERVICES_DATA));
}

#[test]
fn edid_round_trip_and_absent() {
    let mut buf = vec![0u8; 4096];
    let n = {
        let mut w = Writer::new(&mut buf).unwrap();
        w.config(b"x=1\n").unwrap();
        w.len()
    };
    let bi = BootInfo::parse(&buf).unwrap();
    let _ = n;
    assert_eq!(bi.edid(), None);
    let mut buf = vec![0u8; 4096];
    let edid: std::vec::Vec<u8> = (0..128u32).map(|i| i as u8).collect();
    let n = {
        let mut w = Writer::new(&mut buf).unwrap();
        w.edid(&edid).unwrap();
        w.len()
    };
    let bi = BootInfo::parse(&buf).unwrap();
    let _ = n;
    assert_eq!(bi.edid(), Some(&edid[..]));
}
