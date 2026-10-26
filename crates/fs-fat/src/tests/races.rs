//! Work on other threads: the background write-back of SYS files written under the grant, a read racing a
//! truncate whose clusters another file takes over; and the smaller Linux-exactness points (utimensat's
//! times on the disk, names counted in UTF-16 units).

use super::util::*;
use crate::sysapi::Sys;
use std::string::String;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::vec::Vec;
use std::{format, vec};
use uapi::errno::*;
use vfs::abi::{O_CREAT, O_EXCL, O_WRONLY};
use vfs::ops::Ctx;
use vfs::pagecache::SyncKind;

#[test]
fn sys_pages_written_back_by_another_thread() {
    let dir = scratch("sys-wb");
    let dev = Arc::new(crate::MemDev::new(super::rules::stick_image(&dir, false), 512));
    let vol = mount_stick(&dev);
    let ns = view(&vol);
    Sys { vol: &vol, ns: &ns, mount: "/boot" }.create_generation(2).unwrap();
    let c = Ctx::kernel(&ns);
    let data = pattern(100_000, 9);
    let g = vol.grant_sys();
    let f = c.open(None, b"/boot/BITSOS2/SYS/2/KERNEL", O_WRONLY | O_CREAT | O_EXCL, 0o644).unwrap();
    f.pwrite(0, &data).unwrap();
    // The background flusher runs on its own thread, which never holds the grant: it must still write
    // what was admitted under it (it used to answer EPERM and log a refusal every 5 s).
    let inode = f.inode().unwrap().clone();
    let r = std::thread::spawn(move || inode.sync(SyncKind::Background)).join().unwrap();
    assert_eq!(r, Ok(()));
    drop(f);
    drop(g);
    vol.sync(true).unwrap();
    assert_clean(&dir, &dev.image());
    assert_eq!(get(&save(&dir, &dev), "/BITSOS2/SYS/2/KERNEL").unwrap(), data);
    // Without the grant nothing gets in, on any thread.
    assert_eq!(c.open(None, b"/boot/BITSOS2/SYS/2/KERNEL", O_WRONLY, 0).err(), Some(EPERM));
}

#[test]
fn a_read_never_sees_bytes_of_another_file() {
    let dir = scratch("race");
    let img = mkfs(&dir, 40, 1, "RACE");
    let dev = load(&img);
    let vol = mount_plain(&dev);
    let a = vol.create(crate::vol::ROOT, b"a", false).unwrap().ino;
    let b = vol.create(crate::vol::ROOT, b"b", false).unwrap().ino;
    let len = 64 * 1024;
    let (fill_a, fill_b) = (vec![0xAAu8; len], vec![0xBBu8; len]);
    vol.write_data(a, 0, &fill_a, len as u64).unwrap();
    vol.commit(a).unwrap();
    dev.slow_reads(30);
    let stop = Arc::new(AtomicBool::new(false));
    let (v2, s2) = (vol.clone(), stop.clone());
    // The writer frees a's clusters, lets b take them with other bytes, then gives them back to a.
    let w = std::thread::spawn(move || {
        while !s2.load(Ordering::Relaxed) {
            v2.set_size(a, 0).unwrap();
            v2.write_data(b, 0, &fill_b, len as u64).unwrap();
            v2.commit(b).unwrap();
            v2.set_size(b, 0).unwrap();
            v2.write_data(a, 0, &fill_a, len as u64).unwrap();
            v2.commit(a).unwrap();
        }
    });
    let mut buf = vec![0u8; len];
    for _ in 0..400 {
        vol.read_data(a, 0, &mut buf).unwrap();
        assert!(buf.iter().all(|&x| x == 0xAA || x == 0), "a read of a showed bytes of b");
    }
    stop.store(true, Ordering::Relaxed);
    w.join().unwrap();
    dev.slow_reads(0);
    vol.sync(true).unwrap();
    assert_clean(&dir, &dev.image());
}

#[test]
fn times_and_long_names() {
    let dir = scratch("names");
    let img = mkfs(&dir, 40, 1, "NAMES");
    let dev = load(&img);
    let vol = mount_plain(&dev);
    let ns = view(&vol);
    let c = Ctx::kernel(&ns);
    // 200 umlauts are 400 bytes in UTF-8 and 200 UTF-16 units: valid on Linux vfat.
    let long = "ä".repeat(200);
    let p = format!("/boot/{long}");
    ns.create_file_bytes(p.as_bytes(), 0o644, b"umlauts").unwrap().inode.sync(SyncKind::Fsync).unwrap();
    assert_eq!(ns.read_file(&p, 64).unwrap(), b"umlauts");
    let over = format!("/boot/{}", "ä".repeat(256));
    assert_eq!(c.open(None, over.as_bytes(), O_WRONLY | O_CREAT, 0o644).err(), Some(ENAMETOOLONG), "256 units");
    let ascii = format!("/boot/{}", "x".repeat(256));
    assert_eq!(c.open(None, ascii.as_bytes(), O_WRONLY | O_CREAT, 0o644).err(), Some(ENAMETOOLONG));
    assert_eq!(c.open(None, format!("/boot/{}", "x".repeat(300)).as_bytes(), 0, 0).err(), Some(ENOENT), "a lookup is not found");
    // utimensat's times reach the entry (FAT: 2-second mtime, access date only).
    let f = vol.create(crate::vol::ROOT, b"stamped", false).unwrap().ino;
    let t = 1_760_000_000i64; // 2025-10-09 08:53:20 UTC
    vol.set_times(f, Some(t), Some(t)).unwrap();
    vol.sync(true).unwrap();
    let e = vol.peek(&[b"stamped"]).unwrap().unwrap();
    assert_eq!(vol.clock.utc(e.wdate, e.wtime), t);
    assert_eq!(vol.clock.utc(e.adate, 0), t - t % 86_400);
    assert_clean(&dir, &dev.image());
    let after = save(&dir, &dev);
    assert!(ls(&after, "/").contains(&long), "{:?}", ls(&after, "/"));
    let _: Vec<String> = Vec::new();
}
