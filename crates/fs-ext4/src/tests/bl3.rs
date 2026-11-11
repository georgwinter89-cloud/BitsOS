//! BL-3 on the host: a fragmented file (data extents between another file's, holes, an unwritten extent, a block
//! alone in a hole, a tail not on a page) built through the VFS, read back cold through `Volume::read_pages` byte by
//! byte, on 4 KiB and 1 KiB blocks, through a device without `read_pages` (MemDev) and through the disk path
//! (`CachedDiskDev` on a RAM disk); e2fsprogs' `debugfs dump` of the same image agrees and e2fsck is clean.

use super::util::*;
use crate::bl3_ktests::{build_fragmented, cold_check, extents, unflushed_steps, ROUNDS};
use crate::dev::{CachedDiskDev, Dev, MemDev};
use crate::mount;
use block::ramdisk::RamDisk;
use block::{Dispatch, Disk, DiskKind};
use std::process::Command;
use std::sync::Arc;
use std::format;
use std::string::String;
use std::vec;
use std::vec::Vec;

fn host_dump(s: &Scratch, img: &[u8], name: &str) -> Vec<u8> {
    let p = s.path("dump.img");
    let out = s.path("dump.out");
    std::fs::write(&p, img).unwrap();
    let o = Command::new("debugfs").env("LC_ALL", "C").args(["-R", &format!("dump /{name} {}", out.display())]).arg(&p).output().unwrap();
    assert!(o.status.success(), "debugfs: {}", String::from_utf8_lossy(&o.stderr));
    std::fs::read(&out).unwrap()
}

/// Builds the file on `ns` at /data and checks it cold; the volume `v`'s mapping must be fragmented.
fn build_and_check(ns: &vfs::MountNs, v: &crate::Volume) -> Vec<u8> {
    let (path, want) = build_fragmented(ns, "/data").unwrap();
    let ino = vfs::ops::Ctx::kernel(ns).stat(None, path.as_bytes(), true).unwrap().ino as u32;
    let (data, unwritten, holes) = extents(v, ino, want.len() as u64).unwrap();
    assert!(data >= ROUNDS && unwritten >= 1 && holes >= ROUNDS - 2, "extents: {data} data, {unwritten} unwritten, {holes} holes");
    cold_check(ns, &path, &want).unwrap();
    // Twice: the second cold read meets the pages the first one's read-ahead may have left behind.
    cold_check(ns, &path, &want).unwrap();
    want
}

fn memdev_round(bs: &str) {
    let s = Scratch::new();
    let img = mkfs(&s, "16M", &["-b", bs], None);
    let ns = vfs::MountNs::new_empty();
    let dev = Arc::new(MemDev::new(img));
    let (v, _m) = mount::mount_dev(dev.clone(), None, &ns, "/data", opts()).unwrap();
    let want = build_and_check(&ns, &v);
    v.sync_fs().unwrap();
    let img = dev.image();
    assert_clean(&s, &img, "the fragmented file");
    assert!(host_dump(&s, &img, "bl3-frag") == want, "e2fsprogs reads other bytes than written");
    // A fresh mount of the same image: nothing of the file in any cache.
    let ns2 = vfs::MountNs::new_empty();
    let (_v2, _m2) = mount::mount_dev(Arc::new(MemDev::new(img)), None, &ns2, "/data", opts()).unwrap();
    cold_check(&ns2, "/data/bl3-frag", &want).unwrap();
}

#[test]
fn a_fragmented_file_reads_back_through_a_device_without_frames_4k() {
    memdev_round("4096");
}

#[test]
fn a_fragmented_file_reads_back_through_a_device_without_frames_1k() {
    memdev_round("1024");
}

fn disk_round(bs: &str, sector: u32) {
    let s = Scratch::new();
    let img = mkfs(&s, "16M", &["-b", bs], None);
    let blocks = img.len() as u64 / u64::from(sector);
    let r = Arc::new(RamDisk::from_image("bl3host", sector, img, false));
    let disk = Disk::open(r.clone(), DiskKind::Ram, Dispatch::Inline).unwrap();
    let dev = Arc::new(CachedDiskDev::new(disk.clone(), 0, blocks));
    let ns = vfs::MountNs::new_empty();
    let (v, _m) = mount::mount_dev(dev.clone(), None, &ns, "/data", opts()).unwrap();
    let frames0 = r.frame_calls();
    let want = build_and_check(&ns, &v);
    assert!(r.frame_calls() > frames0, "no file read went into frames");
    v.sync_fs().unwrap();
    let img = r.snapshot();
    assert_clean(&s, &img, "the fragmented file on a disk");
    assert!(host_dump(&s, &img, "bl3-frag") == want, "e2fsprogs reads other bytes than written");
    if bs == "4096" {
        // One ext4 block per disk-cache page: no file page shares a page with metadata, so after the fsync no read
        // needed the cache.
        assert_eq!(dev.file_reads_via_cache(), 0, "file reads through the disk's cache");
    }
}

#[test]
fn a_fragmented_file_reads_back_from_a_disk_into_its_pages_4k() {
    disk_round("4096", 512);
}

#[test]
fn a_fragmented_file_reads_back_from_a_disk_into_its_pages_1k() {
    disk_round("1024", 512);
}

#[test]
fn a_fragmented_file_reads_back_from_a_4k_sector_disk() {
    disk_round("4096", 4096);
}

/// 1 KiB blocks on a 4 KiB-sector disk: a piece of one or two blocks is not in whole sectors and reads through the
/// disk's cache; the bytes are right either way.
#[test]
fn a_fragmented_file_of_1k_blocks_reads_back_from_a_4k_sector_disk() {
    disk_round("1024", 4096);
}

#[test]
fn bytes_not_yet_flushed_are_read_through_the_disk_cache() {
    let img: Vec<u8> = (0..256 * 4096usize).map(|i| (i as u8).wrapping_mul(31).wrapping_add(5) ^ (i >> 12) as u8).collect();
    let r = Arc::new(RamDisk::from_image("bl3dirty", 512, img.clone(), false));
    let disk = Disk::open(r.clone(), DiskKind::Ram, Dispatch::Inline).unwrap();
    let dev = CachedDiskDev::new(disk, 0, 256 * 4096 / 512);
    unflushed_steps(&dev, &img, &|| r.frame_calls()).unwrap();
}

/// The window (`read_window`, 32 pages) is what a sequential miss and a mapping's fault read; a random read(2)
/// reads only its own pages (Linux `page_cache_sync_ra`: "Standalone, small random read. Read as is"). On the
/// host no read-ahead thread runs, so the cache holds exactly what the fills read.
#[test]
fn a_random_read_reads_its_page_and_a_sequential_miss_or_a_fault_the_window() {
    let s = Scratch::new();
    let tree = s.path("bl3win");
    std::fs::create_dir_all(&tree).unwrap();
    let data: Vec<u8> = (0..200 * 4096usize).map(|i| (i % 251) as u8 ^ (i >> 12) as u8).collect();
    std::fs::write(tree.join("f"), &data).unwrap();
    let img = mkfs(&s, "16M", &["-b", "4096"], Some(&tree));
    let ns = vfs::MountNs::new_empty();
    mount::mount_dev(Arc::new(MemDev::new(img)), None, &ns, "/data", opts()).unwrap();
    let f = vfs::ops::Ctx::kernel(&ns).open(None, b"/data/f", vfs::abi::O_RDONLY, 0).unwrap();
    let d = ns.lookup("/data/f").unwrap();
    let c = d.inode.page_cache().unwrap();
    let mut b = [0u8; 4096];
    let p = 4096u64;
    assert_eq!(f.pread(100 * p, &mut b).unwrap(), 4096);
    assert!(b[..] == data[100 * 4096..101 * 4096]);
    assert_eq!(c.pages(), 1, "a random read reads its page alone");
    assert_eq!(f.pread(101 * p, &mut b).unwrap(), 4096);
    assert!(b[..] == data[101 * 4096..102 * 4096]);
    assert_eq!(c.pages(), 1 + 32, "a miss after a cached page reads the window");
    assert_eq!(f.pread(0, &mut b).unwrap(), 4096);
    assert_eq!(c.pages(), 1 + 32 + 32, "a miss at the start reads the window");
    let pg = c.page_for_map(150, false).unwrap();
    let mut q = [0u8; 4096];
    pg.read(0, &mut q);
    assert!(q[..] == data[150 * 4096..151 * 4096]);
    assert_eq!(c.pages(), 1 + 32 + 32 + 32, "a fault reads the window around its page");
    // A stream whose history pages went (fadvise DONTNEED after each piece): the read where the last one ended
    // still reads the window (Linux `ra->prev_pos`, `index - prev_index <= 1`). The fault's page is let go first.
    drop(pg);
    c.dont_need(0, 0);
    assert_eq!(c.pages(), 0);
    assert_eq!(f.pread(120 * p, &mut b).unwrap(), 4096);
    assert_eq!(c.pages(), 1, "a random read reads its page alone");
    c.dont_need(0, 0);
    assert_eq!(f.pread(121 * p, &mut b).unwrap(), 4096);
    assert!(b[..] == data[121 * 4096..122 * 4096]);
    assert_eq!(c.pages(), 32, "the read where the last one ended reads the window, its page before gone");
}

/// A damaged extent (its start past the end of the volume) of a file of three: the pages it maps answer
/// EUCLEAN or EIO and never reach the device, the pages of the good extents around it read right, also when
/// their window (a sequential miss, a fault's read-around) reaches into the bad one, and nothing panics.
#[test]
fn a_damaged_extent_fails_its_pages_and_not_their_neighbours() {
    let s = Scratch::new();
    let tree = s.path("bl3bad");
    std::fs::create_dir_all(&tree).unwrap();
    let data: Vec<u8> = (0..96 * 4096usize).map(|i| (i % 253) as u8 ^ (i >> 12) as u8).collect();
    std::fs::write(tree.join("f"), &data).unwrap();
    let img = mkfs(&s, "16M", &["-b", "4096"], Some(&tree));
    let out = debugfs(&s, &img, "bmap /f 0");
    let start: u64 = out.trim().parse().unwrap_or_else(|_| panic!("bmap: {out}"));
    // i_block as three extents in the inode (header, then 3 words each: logical block, len | start_hi << 16,
    // start_lo): [0, 32) and [64, 96) where mkfs put them, [32, 64) at block 0x7fff0000 of a 4096-block volume.
    let ext = [(0u64, start), (32, 0x7fff_0000), (64, start + 64)];
    let mut cmds = vec![format!("sif /f block[0] {:#x}", 0xF30Au32 | (3 << 16)), format!("sif /f block[1] {:#x}", 4u32)];
    for (k, (l, p)) in ext.iter().enumerate() {
        cmds.push(format!("sif /f block[{}] {l}", 3 + 3 * k));
        cmds.push(format!("sif /f block[{}] 32", 4 + 3 * k));
        cmds.push(format!("sif /f block[{}] {p}", 5 + 3 * k));
    }
    let pth = s.path("bad.img");
    std::fs::write(&pth, &img).unwrap();
    let cf = s.path("bad.cmds");
    std::fs::write(&cf, cmds.join("\n") + "\n").unwrap();
    let o = Command::new("debugfs").args(["-w", "-f"]).arg(&cf).arg(&pth).output().unwrap();
    assert!(o.status.success(), "debugfs: {}", String::from_utf8_lossy(&o.stderr));
    let img = std::fs::read(&pth).unwrap();
    let r = Arc::new(RamDisk::from_image("bl3bad", 512, img, false));
    let disk = Disk::open(r.clone(), DiskKind::Ram, Dispatch::Inline).unwrap();
    let blocks = 16 * 1024 * 1024 / 512;
    let dev = Arc::new(CachedDiskDev::new(disk, 0, blocks));
    let ns = vfs::MountNs::new_empty();
    mount::mount_dev(dev, None, &ns, "/data", opts()).unwrap();
    let f = vfs::ops::Ctx::kernel(&ns).open(None, b"/data/f", vfs::abi::O_RDONLY, 0).unwrap();
    let dent = ns.lookup("/data/f").unwrap();
    let c = dent.inode.page_cache().unwrap();
    let p = 4096u64;
    let mut b = [0u8; 4096];
    r.record();
    let good = |i: usize, b: &[u8]| b[..] == data[i * 4096..(i + 1) * 4096];
    assert_eq!(f.pread(15 * p, &mut b).unwrap(), 4096);
    assert!(good(15, &b));
    // Sequential after page 15: its window [16, 48) reaches into the bad extent; the window fails, the page is
    // read alone.
    assert_eq!(f.pread(16 * p, &mut b).unwrap(), 4096);
    assert!(good(16, &b), "a page whose window reaches a bad extent");
    for i in [32u64, 40, 63] {
        let e = f.pread(i * p, &mut b).unwrap_err();
        assert!(e == crate::errno::EUCLEAN || e == crate::errno::EIO, "page {i} of the bad extent: errno {e}");
    }
    assert_eq!(f.pread(70 * p, &mut b).unwrap(), 4096);
    assert!(good(70, &b), "a page after the bad extent");
    // A fault's read-around [50, 82) reaches into it too.
    c.dont_need(0, 0);
    let pg = c.page_for_map(66, false).unwrap();
    pg.read(0, &mut b);
    assert!(good(66, &b), "a fault next to the bad extent");
    assert!(c.page_for_map(33, false).is_err(), "a fault in the bad extent");
    let calls = r.calls();
    assert!(!calls.is_empty() && calls.iter().all(|k| k.lba + k.blocks <= blocks), "a read reached past the volume: {calls:?}");
}

/// A read while the flush of its bytes runs (taken off the dirty list, on their way to the disk) and after a
/// flush that failed (noted again) still reads the bytes written, through the disk's cache.
#[test]
fn bytes_of_a_running_or_failed_flush_are_read_through_the_disk_cache() {
    use block::ramdisk::Fault;
    let img: Vec<u8> = (0..256 * 4096usize).map(|i| (i as u8).wrapping_mul(7) ^ (i >> 12) as u8).collect();
    let r = Arc::new(RamDisk::from_image("bl3flush", 512, img.clone(), false));
    let disk = Disk::open(r.clone(), DiskKind::Ram, Dispatch::Inline).unwrap();
    let dev = Arc::new(CachedDiskDev::new(disk, 0, 256 * 4096 / 512));
    let (p, at) = (4096u64, 64 * 4096u64);
    let new: Vec<u8> = (0..4096usize).map(|i| (i as u8) ^ 0x5a).collect();
    dev.write(at, &new).unwrap();
    // Running: the device holds the flush's write.
    r.pause();
    let d2 = dev.clone();
    let flush = std::thread::spawn(move || d2.flush());
    let t0 = std::time::Instant::now();
    while !r.holding() {
        assert!(t0.elapsed() < std::time::Duration::from_secs(5), "the flush never reached the device");
        std::thread::yield_now();
    }
    let via0 = dev.file_reads_via_cache();
    let got = crate::bl3_ktests::read_into(&dev, at, 1).unwrap();
    assert!(got == new, "a read while its flush runs");
    assert_eq!(dev.file_reads_via_cache(), via0 + 1, "it went through the disk's cache");
    r.resume();
    flush.join().unwrap().unwrap();
    // Failed: the write errors, the range is noted again.
    dev.write(at, &new[..2048]).unwrap();
    r.inject(Fault { write: true, lba: at / 512, blocks: 8, error: devmodel::DevError::Io, times: None });
    assert!(dev.flush().is_err(), "the injected write error");
    let got = crate::bl3_ktests::read_into(&dev, at, 2).unwrap();
    assert!(got[..4096] == new[..], "after a failed flush");
    assert!(got[4096..] == img[(at + p) as usize..(at + 2 * p) as usize], "the page after it, from the disk");
    assert_eq!(dev.file_reads_via_cache(), via0 + 2, "the failed range through the cache");
    r.clear_faults();
    dev.flush().unwrap();
    let got = crate::bl3_ktests::read_into(&dev, at, 1).unwrap();
    assert!(got == new && dev.file_reads_via_cache() == via0 + 2, "after the flush, from the disk");
}
