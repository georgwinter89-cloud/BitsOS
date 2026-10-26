//! CONFIG.TXT's guarded keys through the file system: the evasions a hostile writer tries (case, spaces, CRLF,
//! a line split over several writes, reordering lines whose last one wins), two writers whose fragments only
//! together form a guarded line, and offsets that would make the kernel copy gigabytes.

use super::util::*;
use crate::protect::CONFIG_MAX;
use std::string::String;
use std::sync::{Arc, Barrier};
use std::vec::Vec;
use uapi::errno::*;
use vfs::abi::{O_APPEND, O_WRONLY};
use vfs::ops::Ctx;
use vfs::pagecache::SyncKind;

const CFG: &[u8] = b"/boot/BITSOS2/CONFIG.TXT";
const CFG_S: &str = "/boot/BITSOS2/CONFIG.TXT";

fn stick(tag: &str, config: &[u8]) -> (std::path::PathBuf, Arc<crate::MemDev>) {
    let dir = scratch(tag);
    let mut img = super::rules::stick_image(&dir, false);
    if config != b"screen=1024x768\nnvme_write=off\n" {
        let p = dir.join("cfg.img");
        std::fs::write(&p, &img).unwrap();
        put_bytes(&p, &dir, "/BITSOS2/CONFIG.TXT", config);
        img = std::fs::read(&p).unwrap();
    }
    (dir, Arc::new(crate::MemDev::new(img, 512)))
}

#[test]
fn evasions_are_refused() {
    let (dir, dev) = stick("cfg-ev", b"nvme_write=on\nnvme_write=off\nscreen=1\n");
    let vol = mount_stick(&dev);
    let ns = view(&vol);
    let c = Ctx::kernel(&ns);
    let old = ns.read_file(CFG_S, 1 << 16).unwrap();
    let f = c.open(None, CFG, O_WRONLY | O_APPEND, 0).unwrap();
    for line in [&b"NVME_WRITE=yes\n"[..], b"  nvme_write  =  1  \r\n", b"Nvme_Write=yes\n", b"usb_msd_write = on\n", b"stick_write=on # x\n"] {
        assert_eq!(f.write(line).err(), Some(EPERM), "{}", String::from_utf8_lossy(line));
    }
    // A line split over two writes: the first half is harmless, the second completes a guarded line.
    f.write(b"nv").unwrap();
    assert_eq!(f.write(b"me_write=on\n").err(), Some(EPERM));
    drop(f);
    let f = c.open(None, CFG, O_WRONLY, 0).unwrap();
    // Last line wins (kernel, bitsboot): swapping the two nvme_write lines would turn it on.
    assert_eq!(f.pwrite(0, b"nvme_write=off\nnvme_write=on\n").err(), Some(EPERM));
    drop(f);
    // So would dropping the last of them.
    assert_eq!(c.truncate(None, CFG, 14).err(), Some(EPERM));
    assert_eq!(ns.read_file(CFG_S, 1 << 16).unwrap(), [&old[..], b"nv"].concat(), "only the harmless half went in");
    // An unrelated key may change.
    let f = c.open(None, CFG, O_WRONLY, 0).unwrap();
    f.pwrite(36, b"9").unwrap();
    f.inode().unwrap().sync(SyncKind::Fsync).unwrap();
    drop(f);
    assert_eq!(get(&save(&dir, &dev), "/BITSOS2/CONFIG.TXT").unwrap(), b"nvme_write=on\nnvme_write=off\nscreen=9\nnv");
    assert_clean(&dir, &dev.image());
}

#[test]
fn huge_offsets_are_efbig() {
    let (dir, dev) = stick("cfg-big", b"screen=1024x768\nnvme_write=off\n");
    let vol = mount_stick(&dev);
    let ns = view(&vol);
    let c = Ctx::kernel(&ns);
    let f = c.open(None, CFG, O_WRONLY, 0).unwrap();
    // Without the limit each of these made the kernel build a copy of up to 4 GiB.
    assert_eq!(f.pwrite(0xFFFF_FF00, b"x").err(), Some(EFBIG));
    assert_eq!(f.pwrite(CONFIG_MAX, b"x").err(), Some(EFBIG));
    assert_eq!(c.truncate(None, CFG, 0xFFFF_FFFF).err(), Some(EFBIG));
    assert_eq!(c.truncate(None, CFG, CONFIG_MAX + 1).err(), Some(EFBIG));
    f.pwrite(CONFIG_MAX - 1, b"\n").unwrap();
    c.truncate(None, CFG, 31).unwrap();
    f.inode().unwrap().sync(SyncKind::Fsync).unwrap();
    drop(f);
    assert_clean(&dir, &dev.image());
}

#[test]
fn fragments_of_two_writers_never_meet() {
    let (_dir, dev) = stick("cfg-race", b"screen=1024x768\nnvme_write=off\n");
    let vol = mount_stick(&dev);
    let ns = view(&vol);
    let c = Ctx::kernel(&ns);
    let mut met = 0;
    for _ in 0..300 {
        c.truncate(None, CFG, 0).unwrap();
        c.open(None, CFG, O_WRONLY, 0).unwrap().pwrite(0, b"xxxxxzzzzz=1\n").unwrap();
        let (a, b) = (c.open(None, CFG, O_WRONLY, 0).unwrap(), c.open(None, CFG, O_WRONLY, 0).unwrap());
        let go = Arc::new(Barrier::new(2));
        let g2 = go.clone();
        let t = std::thread::spawn(move || {
            g2.wait();
            b.pwrite(5, b"write").map(|_| ())
        });
        go.wait();
        let ra = a.pwrite(0, b"nvme_").map(|_| ());
        let rb = t.join().unwrap();
        let now: Vec<u8> = ns.read_file(CFG_S, 64).unwrap();
        assert_ne!(now, b"nvme_write=1\n", "two fragments formed a guarded line in the cache");
        // Exactly one of them may have been refused (the second one, which would have completed the line).
        assert!(ra.is_ok() || rb.is_ok());
        met += (ra.is_err() || rb.is_err()) as u32;
    }
    assert!(met == 300, "every round one writer must be refused ({met} of 300)");
}
