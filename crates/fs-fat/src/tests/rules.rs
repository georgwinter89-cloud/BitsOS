//! The boot stick's rules on a stick-shaped image: every refusal before anything is written, the narrow SYS
//! API, CONFIG.TXT's guarded keys, a crafted alias, a read-only mount.

use super::util::*;
use crate::sysapi::{Marker, Sys};
use crate::vol::{Clock, Options, Volume};
use std::string::String;
use std::sync::Arc;
use std::vec::Vec;
use std::{format, vec};
use uapi::errno::*;
use vfs::abi::{O_APPEND, O_CREAT, O_RDWR, O_TRUNC, O_WRONLY};
use vfs::ops::Ctx;
use vfs::pagecache::SyncKind;

pub(super) const ID: &[u8] = b"00112233445566778899aabbccddeeff";

/// A stick image: EFI loader, STICK.ID, CONFIG.TXT, generation 1, a program, a file in the root, and
/// "Innocent file.txt" whose 8.3 alias was rewritten to STICK.ID (a crafted entry).
pub(super) fn stick_image(dir: &std::path::Path, craft: bool) -> Vec<u8> {
    let img = mkfs(dir, 40, 1, "BITSOS");
    for d in ["/EFI", "/EFI/BOOT", "/BITSOS2", "/BITSOS2/SYS", "/BITSOS2/SYS/1", "/BITSOS2/bin"] {
        mkdir(&img, d);
    }
    for (p, d) in [
        ("/EFI/BOOT/BOOTX64.EFI", pattern(9000, 1)),
        ("/BITSOS2/STICK.ID", ID.to_vec()),
        ("/BITSOS2/CONFIG.TXT", b"screen=1024x768\nnvme_write=off\n".to_vec()),
        ("/BITSOS2/SYS/1/KERNEL", pattern(20_000, 2)),
        ("/BITSOS2/SYS/1/INITRD", pattern(7000, 3)),
        ("/BITSOS2/SYS/1/OK", Vec::new()),
        ("/BITSOS2/bin/hello", pattern(3000, 4)),
        ("/BITSOS2/Innocent file.txt", b"innocent".to_vec()),
        ("/ROOT.TXT", b"root".to_vec()),
    ] {
        put_bytes(&img, dir, p, &d);
    }
    let mut b = std::fs::read(&img).unwrap();
    if !craft {
        return b;
    }
    // Rewrite innocent.txt's short entry to STICK.ID and fix its LFN checksums.
    let at = b.windows(11).position(|w| w == b"INNOCE~1TXT").expect("the crafted file's alias");
    let mut s = [0u8; 11];
    s.copy_from_slice(b"STICK   ID ");
    b[at..at + 11].copy_from_slice(&s);
    let sum = crate::name::checksum(&s);
    let mut e = at - 32;
    while b[e + 11] == 0x0F {
        b[e + 13] = sum;
        e -= 32;
    }
    b
}

fn rw(dev: &Arc<crate::MemDev>) -> Arc<Volume> {
    mount_stick(dev)
}

#[test]
fn refusals_write_nothing() {
    let dir = scratch("rules");
    let dev = Arc::new(crate::MemDev::new(stick_image(&dir, true), 512));
    let vol = rw(&dev);
    let ns = view(&vol);
    let c = Ctx::kernel(&ns);
    let open = |p: &str, f: u64| c.open(None, p.as_bytes(), f, 0o644).map(|_| ()).err();
    // Read everything first (directories load), then record: a refusal must not write a single sector.
    for p in ["/boot", "/boot/EFI/BOOT", "/boot/BITSOS2", "/boot/BITSOS2/SYS/1", "/boot/BITSOS2/bin"] {
        ns.list_dir(p).unwrap();
    }
    assert_eq!(ns.read_file("/boot/BITSOS2/Innocent file.txt", 100).unwrap(), b"innocent");
    dev.record();
    // Outside \BITSOS2\: read-only.
    assert_eq!(open("/boot/new.txt", O_WRONLY | O_CREAT), Some(EROFS));
    assert_eq!(open("/boot/ROOT.TXT", O_WRONLY), Some(EROFS));
    assert_eq!(open("/boot/EFI/BOOT/BOOTX64.EFI", O_RDWR), Some(EROFS));
    assert_eq!(open("/boot/efi/boot/new.efi", O_WRONLY | O_CREAT), Some(EROFS));
    assert_eq!(c.truncate(None, b"/boot/EFI/BOOT/BOOTX64.EFI", 0).err(), Some(EROFS));
    assert_eq!(c.unlink(None, b"/boot/EFI/BOOT/BOOTX64.EFI").err(), Some(EROFS));
    assert_eq!(c.mkdir(None, b"/boot/NEWDIR", 0o755).err(), Some(EROFS));
    assert_eq!(c.rename(None, b"/boot/BITSOS2", None, b"/boot/B2", 0).err(), Some(EROFS));
    assert_eq!(c.rmdir(None, b"/boot/EFI/BOOT").err(), Some(EROFS));
    assert_eq!(c.rename(None, b"/boot/BITSOS2/bin/hello", None, b"/boot/hello", 0).err(), Some(EROFS));
    assert_eq!(c.access(None, b"/boot/ROOT.TXT", vfs::abi::W_OK, true).err(), Some(EROFS));
    // STICK.ID: never; also by its alias, by another case, and the crafted entry whose alias is STICK.ID.
    assert_eq!(open("/boot/BITSOS2/STICK.ID", O_WRONLY), Some(EPERM));
    assert_eq!(open("/boot/BITSOS2/stick.id.", O_WRONLY | O_TRUNC), Some(EPERM));
    assert_eq!(open("/boot/BITSOS2/Innocent file.txt", O_WRONLY), Some(EPERM));
    assert_eq!(c.unlink(None, b"/boot/BITSOS2/STICK.ID").err(), Some(EPERM));
    assert_eq!(c.unlink(None, b"/boot/BITSOS2/Innocent file.txt").err(), Some(EPERM));
    assert_eq!(c.rename(None, b"/boot/BITSOS2/STICK.ID", None, b"/boot/BITSOS2/old.id", 0).err(), Some(EPERM));
    assert_eq!(c.rename(None, b"/boot/BITSOS2/bin/hello", None, b"/boot/BITSOS2/Stick.Id", 0).err(), Some(EPERM));
    // SYS: only through the narrow API.
    assert_eq!(c.mkdir(None, b"/boot/BITSOS2/SYS/2", 0o755).err(), Some(EPERM));
    assert_eq!(open("/boot/BITSOS2/SYS/1/NEW", O_WRONLY | O_CREAT), Some(EPERM));
    assert_eq!(open("/boot/BITSOS2/sys/1/kernel", O_WRONLY), Some(EPERM));
    assert_eq!(c.unlink(None, b"/boot/BITSOS2/SYS/1/OK").err(), Some(EPERM));
    assert_eq!(c.rename(None, b"/boot/BITSOS2/SYS", None, b"/boot/BITSOS2/OLDSYS", 0).err(), Some(EPERM));
    assert_eq!(c.rename(None, b"/boot/BITSOS2/bin", None, b"/boot/BITSOS2/SYS/bin", 0).err(), Some(EPERM));
    assert_eq!(c.mkdir(None, b"/boot/BITSOS2/sys.", 0o755).err(), Some(EEXIST));
    // CONFIG.TXT: guarded keys may not be added or changed.
    let cfg = c.open(None, b"/boot/BITSOS2/CONFIG.TXT", O_WRONLY | O_APPEND, 0).unwrap();
    assert_eq!(cfg.write(b"nvme_write=on\n").err(), Some(EPERM));
    assert_eq!(cfg.pwrite(16, b"nvme_write=on \n").err(), Some(EPERM), "changing nvme_write=off to on in place");
    drop(cfg);
    assert_eq!(dev.take_log().len(), 0, "a refusal wrote to the stick");

    // Allowed below \BITSOS2\.
    let cfg = c.open(None, b"/boot/BITSOS2/CONFIG.TXT", O_WRONLY | O_APPEND, 0).unwrap();
    cfg.write(b"console_log=info\n").unwrap();
    cfg.write(b"wlan=on\n").unwrap();
    drop(cfg);
    c.truncate(None, b"/boot/BITSOS2/CONFIG.TXT", 0).unwrap();
    ns.create_file_bytes(b"/boot/BITSOS2/new.cfg", 0o644, b"stick_write=on\n").unwrap();
    assert_eq!(c.rename(None, b"/boot/BITSOS2/new.cfg", None, b"/boot/BITSOS2/CONFIG.TXT", 0).err(), Some(EPERM));
    c.unlink(None, b"/boot/BITSOS2/CONFIG.TXT").unwrap();
    let f = c.open(None, b"/boot/BITSOS2/config.txt", O_WRONLY | O_CREAT, 0o644).unwrap();
    assert_eq!(f.write(b"nvme_write=on\n").err(), Some(EPERM), "a new CONFIG.TXT starts without guarded lines");
    f.write(b"screen=2560x1440\n").unwrap();
    f.inode().unwrap().sync(SyncKind::Fsync).unwrap();
    drop(f);
    c.rename(None, b"/boot/BITSOS2/new.cfg", None, b"/boot/BITSOS2/bin/new.cfg", 0).unwrap();
    c.mkdir(None, b"/boot/BITSOS2/data", 0o755).unwrap();
    ns.create_file_bytes(b"/boot/BITSOS2/data/x", 0o644, b"x").unwrap().inode.sync(SyncKind::Fsync).unwrap();
    super::basic::sync_tree(&ns, "/boot/BITSOS2");
    // fsck.fat finds only the crafted duplicate (two entries with the 8.3 name STICK.ID).
    let (_, out) = fsck(&dir, &dev.image());
    for l in out.lines().map(str::trim) {
        let ok = l.is_empty()
            || l.starts_with("fsck.fat ")
            || l.contains(" files, ")
            || l.starts_with("Leaving filesystem unchanged")
            || l == "/BITSOS2/STICK.ID"
            || l.starts_with("Duplicate directory entry")
            || l.starts_with("First ")
            || l.starts_with("Second ")
            || l.starts_with("Auto-renaming second")
            || l.starts_with("Renamed to ");
        assert!(ok, "fsck.fat:\n{out}");
    }
    let after = save(&dir, &dev);
    assert_eq!(get(&after, "/BITSOS2/CONFIG.TXT").unwrap(), b"screen=2560x1440\n");
    assert!(get(&after, "/BITSOS2/STICK.ID").unwrap().starts_with(ID), "mtools shows both crafted entries");
    assert_eq!(ns.read_file("/boot/BITSOS2/STICK.ID", 100).unwrap(), ID);
    assert_eq!(get(&after, "/BITSOS2/SYS/1/KERNEL").unwrap(), pattern(20_000, 2));
}

#[test]
fn generations_only_through_the_api() {
    let dir = scratch("sys");
    let dev = Arc::new(crate::MemDev::new(stick_image(&dir, false), 512));
    let vol = rw(&dev);
    let ns = view(&vol);
    let s = Sys { vol: &vol, ns: &ns, mount: "/boot" };
    let g1 = s.generations().unwrap();
    assert_eq!(g1.len(), 1);
    assert!(g1[0].ok && !g1[0].new && g1[0].files.len() == 2);
    let kernel = pattern(150_000, 7);
    s.create_generation(2).unwrap();
    assert_eq!(s.create_generation(2).err(), Some(EEXIST));
    s.write_file(2, "KERNEL", &kernel).unwrap();
    assert_eq!(s.write_file(2, "KERNEL", b"again").err(), Some(EEXIST), "written once, never changed");
    s.write_file(2, "INITRD", &pattern(30_000, 8)).unwrap();
    assert_eq!(s.write_file(2, "STICK.ID", b"x").err(), Some(EINVAL));
    assert_eq!(s.write_file(0, "KERNEL", b"x").err(), Some(EINVAL));
    s.set_marker(2, Marker::New, true).unwrap();
    s.set_marker(2, Marker::Try, true).unwrap();
    s.set_marker(2, Marker::New, false).unwrap();
    assert_eq!(s.set_marker(3, Marker::Ok, true).err(), Some(ENOENT));
    s.set_pin(Some(1)).unwrap();
    assert_eq!(s.pin().unwrap(), Some(1));
    assert_eq!(s.read_file(2, "KERNEL", 1 << 20).unwrap(), kernel, "read back, compare");
    // The same piece by piece (install_kernel's read-back of a large kernel).
    let (mut pieces, mut buf) = (Vec::new(), vec![0u8; 4096]);
    loop {
        let n = s.read_at(2, "KERNEL", pieces.len() as u64, &mut buf).unwrap();
        if n == 0 {
            break;
        }
        pieces.extend_from_slice(&buf[..n]);
    }
    assert_eq!(pieces, kernel, "read back in pieces");
    assert_eq!(s.read_at(2, "KERNEL", 100, &mut buf).err(), Some(EINVAL), "an offset off the sector");
    let g = s.generations().unwrap();
    assert_eq!(g.iter().map(|g| (g.n, g.new, g.try_, g.ok)).collect::<Vec<_>>(), vec![(1, false, false, true), (2, false, true, false)]);
    // The grant opened SYS for those calls only.
    let c = Ctx::kernel(&ns);
    assert_eq!(c.unlink(None, b"/boot/BITSOS2/SYS/2/TRY").err(), Some(EPERM));
    assert!(dev.flushes() >= 6, "every API change ends with the stick's cache flush");
    assert_clean(&dir, &dev.image());
    let after = save(&dir, &dev);
    assert_eq!(get(&after, "/BITSOS2/SYS/2/KERNEL").unwrap(), kernel);
    assert_eq!(get(&after, "/BITSOS2/SYS/PIN").unwrap(), b"1\r\n");
    assert!(get(&after, "/BITSOS2/SYS/2/TRY").is_some() && get(&after, "/BITSOS2/SYS/2/NEW").is_none());
    s.set_pin(None).unwrap();
    s.remove_generation(2).unwrap();
    assert_eq!(s.pin().unwrap(), None);
    assert_eq!(s.generations().unwrap().len(), 1);
    assert_clean(&dir, &dev.image());
    // bitsboot reads the markers by their 8.3 names: no long names were written.
    let after = save(&dir, &dev);
    assert!(ls(&after, "/BITSOS2/SYS").iter().all(|n| n == "1"), "{:?}", ls(&after, "/BITSOS2/SYS"));
}

#[test]
fn a_read_only_mount_refuses_everything() {
    let dir = scratch("ro");
    let dev = Arc::new(crate::MemDev::new(stick_image(&dir, false), 512));
    let vol = Volume::mount(dev.clone(), Options { writable: false, stick: true }, Clock::UTC_EPOCH).unwrap();
    let ns = view(&vol);
    let c = Ctx::kernel(&ns);
    dev.record();
    assert_eq!(c.open(None, b"/boot/BITSOS2/bin/new", O_WRONLY | O_CREAT, 0o644).err(), Some(EROFS));
    assert_eq!(c.open(None, b"/boot/BITSOS2/bin/hello", O_WRONLY, 0).err(), Some(EROFS));
    assert_eq!(c.unlink(None, b"/boot/BITSOS2/bin/hello").err(), Some(EROFS));
    let s = Sys { vol: &vol, ns: &ns, mount: "/boot" };
    assert_eq!(s.create_generation(5).err(), Some(EROFS));
    assert_eq!(ns.read_file("/boot/BITSOS2/bin/hello", 1 << 20).unwrap(), pattern(3000, 4));
    assert!(dev.take_log().is_empty());
    // A volume with one FAT copy (or mirroring off) is never written either.
    let mut one = dev.image();
    one[40] = 0x80;
    let d1 = Arc::new(crate::MemDev::new(one, 512));
    let v1 = Volume::mount(d1, Options { writable: true, stick: true }, Clock::UTC_EPOCH).unwrap();
    assert!(!v1.writable());
    let _ = String::new();
    let _ = format!("");
}
