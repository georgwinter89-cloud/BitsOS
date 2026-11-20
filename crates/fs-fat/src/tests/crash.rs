//! CRASHDISK: `\BITSOS2\CRASH\SLOTS.BIN` only through `crash_area`, its runs map the file's bytes on the image.

use super::rules::stick_image;
use super::util::*;
use crate::crash_area::{Area, runs_of};
use std::sync::Arc;
use std::vec;
use std::vec::Vec;
use uapi::errno::*;
use vfs::abi::{O_CREAT, O_WRONLY};
use vfs::ops::Ctx;

#[test]
fn runs_merge_neighbours() {
    let at = |c: u32| 100 + (c as u64 - 2) * 8;
    assert_eq!(runs_of(&[5, 6, 7, 20, 21, 9], at, 8), vec![(124, 24), (244, 16), (156, 8)]);
    assert!(runs_of(&[], at, 8).is_empty());
}

#[test]
fn area_made_mapped_and_guarded() {
    let dir = scratch("crash");
    let dev = Arc::new(crate::MemDev::new(stick_image(&dir, false), 512));
    let vol = mount_stick(&dev);
    let ns = view(&vol);
    let c = Ctx::kernel(&ns);
    let a = Area { vol: &vol, ns: &ns, mount: "/boot" };
    assert_eq!(a.layout().err(), Some(ENOENT));
    const BYTES: u64 = 4 * 16 * 1024;
    // Grown around another file: the area's chain takes several runs.
    assert!(a.ensure(BYTES / 4).unwrap());
    let f = c.open(None, b"/boot/BITSOS2/between", O_WRONLY | O_CREAT, 0o644).unwrap();
    f.pwrite(0, &pattern(3000, 1)).unwrap();
    f.inode().unwrap().sync(vfs::pagecache::SyncKind::Fsync).unwrap();
    drop(f);
    assert!(a.ensure(BYTES).unwrap());
    assert!(!a.ensure(BYTES).unwrap(), "the same size again changes nothing");
    let l = a.layout().unwrap();
    assert_eq!(l.size, BYTES);
    assert!(l.runs.len() >= 2, "the gaps split the chain: {:?}", l.runs);
    let sectors: u64 = l.runs.iter().map(|r| r.1).sum();
    assert!(sectors * 512 >= BYTES && l.runs.iter().all(|r| r.0 >= l.data_start && r.0 + r.1 <= l.total));
    // Each slot's first sector written through the area, then found at the runs' sectors in the image.
    for s in 0..4u64 {
        a.write_at(s * 16 * 1024, &[0xA0 + s as u8; 512]).unwrap();
    }
    let img = dev.image();
    let byte_at = |off: u64| {
        let mut left = off / 512;
        for (first, n) in &l.runs {
            if left < *n {
                return img[((first + left) * 512 + off % 512) as usize];
            }
            left -= n;
        }
        panic!("offset {off} past the runs");
    };
    for s in 0..4u64 {
        assert_eq!(byte_at(s * 16 * 1024), 0xA0 + s as u8);
        assert_eq!(byte_at(s * 16 * 1024 + 600), 0);
    }
    let mut back = vec![0u8; 512];
    assert_eq!(a.read_at(3 * 16 * 1024, &mut back).unwrap(), 512);
    assert!(back.iter().all(|&b| b == 0xA3));
    assert_eq!(a.write_at(BYTES, &[0; 512]).err(), Some(EINVAL), "past the end");
    assert_eq!(a.write_at(100, &[0; 512]).err(), Some(EINVAL), "not a whole sector");
    // Nobody else: no write, cut, removal, rename or new file there without the grant.
    let p = b"/boot/BITSOS2/CRASH/SLOTS.BIN";
    assert_eq!(c.open(None, p, O_WRONLY, 0).err(), Some(EPERM));
    assert_eq!(c.truncate(None, p, 0).err(), Some(EPERM));
    assert_eq!(c.unlink(None, p).err(), Some(EPERM));
    assert_eq!(c.rename(None, p, None, b"/boot/BITSOS2/moved", 0).err(), Some(EPERM));
    assert_eq!(c.open(None, b"/boot/BITSOS2/CRASH/other", O_WRONLY | O_CREAT, 0o644).err(), Some(EPERM));
    assert_eq!(c.rmdir(None, b"/boot/BITSOS2/CRASH").err(), Some(EPERM));
    assert_eq!(a.layout().unwrap(), l, "the place did not move");
    // Smaller again: the area itself may.
    assert!(a.ensure(BYTES / 2).unwrap());
    assert_eq!(a.layout().unwrap().size, BYTES / 2);
    vol.sync(true).unwrap();
    assert_clean(&dir, &dev.image());
    let names: Vec<_> = ls(&save(&dir, &dev), "/BITSOS2/CRASH");
    assert!(names.iter().any(|n| n.contains("SLOTS")), "{names:?}");
}

/// The area's layout on a fresh mount of `img`.
fn layout_of(img: Vec<u8>) -> crate::crash_area::Layout {
    let dev = Arc::new(crate::MemDev::new(img, 512));
    let vol = mount_stick(&dev);
    let ns = view(&vol);
    Area { vol: &vol, ns: &ns, mount: "/boot" }.layout().unwrap()
}

#[test]
fn a_cross_linked_area_is_seen() {
    let dir = scratch("crash-xlink");
    let dev = Arc::new(crate::MemDev::new(stick_image(&dir, false), 512));
    let vol = mount_stick(&dev);
    let ns = view(&vol);
    let c = Ctx::kernel(&ns);
    let a = Area { vol: &vol, ns: &ns, mount: "/boot" };
    a.ensure(4 * 16 * 1024).unwrap();
    let f = c.open(None, b"/boot/BITSOS2/other", O_WRONLY | O_CREAT, 0o644).unwrap();
    f.pwrite(0, &pattern(3000, 2)).unwrap();
    f.inode().unwrap().sync(vfs::pagecache::SyncKind::Fsync).unwrap();
    drop(f);
    vol.sync(true).unwrap();
    let l = a.layout().unwrap();
    assert_eq!(l.foreign, 0, "a clean volume: the clusters are the area's alone");
    let g = vol.geo.clone();
    let other = vol.peek(&[b"BITSOS2", b"other"]).unwrap().unwrap().first;
    drop(a);
    drop(vol);
    let img = dev.image();
    assert_eq!(layout_of(img.clone()).foreign, 0, "the same after a fresh mount");
    let spc = g.spc as u64;
    let area: Vec<u32> = l.runs.iter().flat_map(|&(s, n)| (0..n / spc).map(move |k| ((s + k * spc - g.data_start) / spc + 2) as u32)).collect();
    assert!(area.len() >= 4 && !area.contains(&other));
    // A. Another chain runs into the area: the other file's first cluster links to the area's fourth.
    let mut x = img.clone();
    for copy in 0..g.fats as u64 {
        let at = ((g.reserved + copy * g.fat_size) * 512 + other as u64 * 4) as usize;
        x[at..at + 4].copy_from_slice(&area[3].to_le_bytes());
    }
    assert!(layout_of(x).foreign >= 1, "a FAT entry of another chain pointing into the area");
    // B. Another directory entry starts inside the area.
    let mut x = img.clone();
    let name = *b"OTHER      ";
    let e = (0..x.len() / 32).map(|i| i * 32).find(|&o| x[o..o + 11] == name && x[o + 11] & 0x0F != 0x0F).expect("the other file's entry");
    x[e + 20..e + 22].copy_from_slice(&((area[2] >> 16) as u16).to_le_bytes());
    x[e + 26..e + 28].copy_from_slice(&(area[2] as u16).to_le_bytes());
    assert!(layout_of(x).foreign >= 1, "a directory entry starting inside the area");
}
