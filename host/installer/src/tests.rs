//! Whole runs against image files: the layout of a fresh stick (MBR, FAT32, files) as BitsOS's own FAT code and
//! partition-table check see it, and updates that keep CONFIG.TXT, data.img and the older generations.

use crate::devices::{self, BlockDev};
use crate::stick::{self, Plan, TreeInput, VendorFile};
use crate::{fat, gens, sha, tar};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub struct Scratch(pub PathBuf);
impl Scratch {
    pub fn new(tag: &str) -> Scratch {
        let base = std::env::var_os("CARGO_TARGET_DIR").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
        let d = base.join("bitsos-install-tests").join(format!("{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        Scratch(d)
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A `.tar.zst` of (path, bytes); returns (file, unpacked bytes, files).
pub fn asset(dir: &Path, name: &str, files: &[(&str, Vec<u8>)]) -> (PathBuf, u64, u64) {
    let mut w = tar::Writer::new(Vec::new());
    for (p, b) in files {
        w.file(p, 0o644, b.len() as u64, &b[..]).unwrap();
    }
    let t = w.finish().unwrap();
    let z = ruzstd::encoding::compress_to_vec(&t[..], ruzstd::encoding::CompressionLevel::Fastest);
    let p = dir.join(name);
    std::fs::write(&p, z).unwrap();
    (p, files.iter().map(|(_, b)| b.len() as u64).sum(), files.len() as u64)
}

pub fn system(dir: &Path, rel: &str) -> (PathBuf, u64, u64) {
    let big: Vec<u8> = (0..3_000_000u32).map(|i| (i * 31 + i / 7) as u8).collect();
    asset(dir, &format!("bitsos-system-{rel}.tar.zst"), &[
        ("EFI/BOOT/BOOTX64.EFI", format!("MZ bitsboot {rel}").into_bytes()),
        ("SYS/KERNEL", format!("\x7fELF kernel {rel}").into_bytes()),
        ("SYS/INITRD", big),
        ("BITSOS2/firmware/amdgpu/gc_11_0_1_me.bin", b"fw".to_vec()),
        ("BITSOS2/bin/mesa-run", format!("#!/bin/sh {rel}").into_bytes()),
        ("NOTICE.txt", b"BitsOS, MIT OR Apache-2.0".to_vec()),
    ])
}

fn plan(dir: &Path, rel: &str, mesa_files: &[(&str, Vec<u8>)], vendor: Vec<VendorFile>) -> Plan {
    let (sys, sb, sf) = system(dir, rel);
    let (mesa, mb, mf) = asset(dir, &format!("bitsos-mesa-{rel}.tar.zst"), mesa_files);
    let vb: u64 = vendor.iter().map(|v| v.size).sum();
    let mut trees = vec![TreeInput { name: "mesa".into(), archive: mesa, asset: format!("bitsos-mesa-{rel}.tar.zst"), vendor: Vec::new() }];
    let mut extra = 0;
    if !vendor.is_empty() {
        let (rt, rb, rf) = asset(dir, &format!("bitsos-brave-runtime-{rel}.tar.zst"), &[("BITSOS2/brave/lib/libgtk-3.so.0", b"gtk".to_vec()), ("VENDOR.txt", b"# list".to_vec())]);
        extra = rb + rf;
        trees.push(TreeInput { name: "brave".into(), archive: rt, asset: "bitsos-brave-runtime.tar.zst".into(), vendor });
    }
    Plan { system: sys, system_asset: format!("bitsos-system-{rel}.tar.zst"), trees, bytes: sb + mb + vb + extra, files: sf + mf }
}

fn image(dir: &Path, mib: u64) -> devices::ImageFile {
    devices::open_image(&dir.join("stick.img"), Some(mib << 20)).unwrap()
}

fn partition(img: &mut devices::ImageFile) -> crate::mbr::Part {
    let mut s0 = [0u8; 512];
    img.seek(SeekFrom::Start(0)).unwrap();
    img.read_exact(&mut s0).unwrap();
    crate::mbr::find_fat(&s0, img.size() / 512).unwrap()
}

/// The FAT32 partition of the image as fs-fat (the kernel's FAT code) mounts it on the boot stick.
pub fn kernel_view(img: &mut devices::ImageFile) -> Arc<fs_fat::Volume> {
    let p = partition(img);
    let mut bytes = vec![0u8; (p.sectors * 512) as usize];
    img.seek(SeekFrom::Start(p.start * 512)).unwrap();
    img.read_exact(&mut bytes).unwrap();
    let dev = Arc::new(fs_fat::MemDev::new(bytes, 512));
    fs_fat::Volume::mount(dev, fs_fat::Options { writable: true, stick: true }, fs_fat::Clock::UTC_EPOCH).unwrap()
}

pub fn kernel_read(v: &fs_fat::Volume, path: &[&[u8]]) -> Option<Vec<u8>> {
    let e = v.peek(path).unwrap()?;
    Some(v.peek_read(&e, 16 << 20).unwrap())
}

fn fsck(dir: &Path, img: &mut devices::ImageFile) {
    let p = partition(img);
    let mut bytes = vec![0u8; (p.sectors * 512) as usize];
    img.seek(SeekFrom::Start(p.start * 512)).unwrap();
    img.read_exact(&mut bytes).unwrap();
    std::fs::write(dir.join("part.img"), bytes).unwrap();
    let o = std::process::Command::new("fsck.fat").arg("-n").arg(dir.join("part.img")).output();
    let o = o.expect("fsck.fat (dosfstools) checks the written volume");
    assert!(o.status.success(), "fsck.fat: {}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr));
}

fn with_fs<R>(img: &mut devices::ImageFile, f: impl FnOnce(&fatfs::Dir<&mut crate::blockio::Window>) -> R) -> R {
    let p = partition(img);
    let mut w = crate::blockio::Window::new(img, p.start * 512, p.sectors * 512);
    let fs = fat::mount(&mut w).unwrap();
    let r = f(&fs.root_dir());
    fs.unmount().unwrap();
    crate::dots::fix(&mut w).unwrap();
    w.sync().unwrap();
    r
}

const ID: &str = "0123456789abcdef0123456789abcdef";

#[test]
fn a_fresh_stick_is_mbr_fat32_bitsos_and_the_kernel_takes_it() {
    let s = Scratch::new("fresh");
    let mut img = image(&s.0, 256);
    let mesa = vec![("BITSOS2/mesa/lib/libvulkan.so.1", b"vk".to_vec()), ("BITSOS2/mesa/PKGLIST.TXT", b"list".to_vec())];
    let p = plan(&s.0, "1", &mesa, Vec::new());
    let done = stick::fresh(&mut img, &p, ID, "# config\ntoken=x\n", &mut |_| {}).unwrap();
    assert_eq!(done.generation, 1);
    assert_eq!(done.sys, "1 OK");

    let mut s0 = [0u8; 512];
    img.seek(SeekFrom::Start(0)).unwrap();
    img.read_exact(&mut s0).unwrap();
    let part = partition(&mut img);
    assert_eq!((part.start, part.kind, s0[446]), (2048, 0x0C, 0x80), "one active FAT32 LBA partition from 1 MiB");
    // The block layer's R8a look: a plain MBR, nothing Windows or EFI, so BitsOS may write it.
    let mut read = |lba: u64, n: u64| {
        let mut b = vec![0u8; (n * 512) as usize];
        img.seek(SeekFrom::Start(lba * 512)).ok()?;
        img.read_exact(&mut b).ok()?;
        Some(b)
    };
    let scan = block::table::scan(&mut read, 512, (256 << 20) / 512);
    assert_eq!(scan.scheme_str(), "mbr");
    assert_eq!(scan.verdict(), block::table::Verdict::Clean);

    let v = kernel_view(&mut img);
    assert!(v.writable(), "two mirrored FATs: the kernel mounts it writable");
    assert_eq!(v.label_text(), b"BITSOS");
    assert_eq!(kernel_read(&v, &[b"BITSOS2", b"STICK.ID"]).unwrap(), ID.as_bytes());
    assert_eq!(kernel_read(&v, &[b"BITSOS2", b"CONFIG.TXT"]).unwrap(), b"# config\ntoken=x\n");
    assert_eq!(kernel_read(&v, &[b"EFI", b"BOOT", b"BOOTX64.EFI"]).unwrap(), b"MZ bitsboot 1");
    assert_eq!(kernel_read(&v, &[b"BITSOS2", b"SYS", b"1", b"KERNEL"]).unwrap(), b"\x7fELF kernel 1");
    assert_eq!(kernel_read(&v, &[b"BITSOS2", b"SYS", b"1", b"INITRD"]).unwrap().len(), 3_000_000);
    assert!(kernel_read(&v, &[b"BITSOS2", b"SYS", b"1", b"OK"]).is_some());
    assert!(kernel_read(&v, &[b"BITSOS2", b"SYS", b"STAGING"]).is_none());
    assert_eq!(kernel_read(&v, &[b"BITSOS2", b"mesa", b"lib", b"libvulkan.so.1"]).unwrap(), b"vk");
    assert_eq!(kernel_read(&v, &[b"BITSOS2", b"firmware", b"amdgpu", b"gc_11_0_1_me.bin"]).unwrap(), b"fw");
    assert!(kernel_read(&v, &[b"BITSOS2", b"LICENSES", b"bitsos-system-1", b"NOTICE.txt"]).is_some());
    assert!(kernel_read(&v, &[b"EFI", b"BOOT", b"BOOTX64.NEW"]).is_none());
    fsck(&s.0, &mut img);
}

#[test]
fn an_update_keeps_settings_data_and_older_generations() {
    let s = Scratch::new("update");
    let mut img = image(&s.0, 256);
    let old = vec![("BITSOS2/mesa/lib/old.so", b"old".to_vec()), ("BITSOS2/mesa/PKGLIST.TXT", b"1".to_vec())];
    stick::fresh(&mut img, &plan(&s.0, "1", &old, Vec::new()), ID, "token=t\nsys_keep=1\n", &mut |_| {}).unwrap();
    // What BitsOS adds while it runs: the data volume, a home folder, the MCP service's keys.
    let data: Vec<u8> = (0..2_000_000u32).map(|i| (i % 251) as u8).collect();
    with_fs(&mut img, |r| {
        fat::write_bytes(r, "BITSOS2/data.img", &data).unwrap();
        fat::write_bytes(r, "BITSOS2/home/.config/x", b"mine").unwrap();
        fat::write_bytes(r, "BITSOS2/mcp/key.pem", b"key").unwrap();
    });
    assert!(stick::inspect(&mut img).unwrap().unwrap().is_bitsos());

    // Release 2: a new generation NEW next to 1 OK; the tree is replaced as a whole.
    let new = vec![("BITSOS2/mesa/lib/new.so", b"new".to_vec()), ("BITSOS2/mesa/PKGLIST.TXT", b"2".to_vec())];
    let vendor_file = s.0.join("brave-bin");
    std::fs::write(&vendor_file, b"brave binary").unwrap();
    let vf = VendorFile { rel: "brave".into(), file: vendor_file, size: 12, sha256: sha::hex(b"brave binary") };
    let d = stick::update(&mut img, &plan(&s.0, "2", &new, vec![vf]), &mut |_| {}).unwrap();
    assert_eq!((d.generation, d.sys.as_str()), (2, "1 OK, 2 NEW"));
    let v = kernel_view(&mut img);
    assert_eq!(kernel_read(&v, &[b"BITSOS2", b"data.img"]).unwrap(), data);
    assert_eq!(kernel_read(&v, &[b"BITSOS2", b"CONFIG.TXT"]).unwrap(), b"token=t\nsys_keep=1\n");
    assert_eq!(kernel_read(&v, &[b"BITSOS2", b"STICK.ID"]).unwrap(), ID.as_bytes());
    assert_eq!(kernel_read(&v, &[b"BITSOS2", b"home", b".config", b"x"]).unwrap(), b"mine");
    assert_eq!(kernel_read(&v, &[b"BITSOS2", b"mcp", b"key.pem"]).unwrap(), b"key");
    assert_eq!(kernel_read(&v, &[b"BITSOS2", b"SYS", b"1", b"KERNEL"]).unwrap(), b"\x7fELF kernel 1");
    assert_eq!(kernel_read(&v, &[b"BITSOS2", b"SYS", b"2", b"KERNEL"]).unwrap(), b"\x7fELF kernel 2");
    assert!(kernel_read(&v, &[b"BITSOS2", b"SYS", b"2", b"NEW"]).is_some());
    assert_eq!(kernel_read(&v, &[b"EFI", b"BOOT", b"BOOTX64.EFI"]).unwrap(), b"MZ bitsboot 2");
    assert!(kernel_read(&v, &[b"BITSOS2", b"mesa", b"lib", b"old.so"]).is_none(), "the old tree went");
    assert_eq!(kernel_read(&v, &[b"BITSOS2", b"mesa", b"lib", b"new.so"]).unwrap(), b"new");
    assert_eq!(kernel_read(&v, &[b"BITSOS2", b"brave", b"brave"]).unwrap(), b"brave binary");
    assert_eq!(kernel_read(&v, &[b"BITSOS2", b"brave", b"lib", b"libgtk-3.so.0"]).unwrap(), b"gtk");
    drop(v);
    fsck(&s.0, &mut img);

    // Release 3 while 2 was never confirmed: it takes 2's place, 1 stays the fallback.
    let d = stick::update(&mut img, &plan(&s.0, "3", &new, Vec::new()), &mut |_| {}).unwrap();
    assert_eq!(d.sys, "1 OK, 2 NEW");
    assert!(d.notes.iter().any(|n| n.contains("never confirmed")));
    assert_eq!(kernel_read(&kernel_view(&mut img), &[b"BITSOS2", b"SYS", b"2", b"KERNEL"]).unwrap(), b"\x7fELF kernel 3");

    // 2 confirmed (as the remote-control service does), then release 4 with sys_keep=1: 1 is pruned.
    with_fs(&mut img, |r| {
        r.remove("BITSOS2/SYS/2/NEW").unwrap();
        fat::write_bytes(r, "BITSOS2/SYS/2/OK", b"").unwrap();
    });
    let d = stick::update(&mut img, &plan(&s.0, "4", &new, Vec::new()), &mut |_| {}).unwrap();
    assert_eq!(d.sys, "2 OK, 3 NEW");
    assert!(d.notes.iter().any(|n| n.contains("generation 1") && n.contains("pruned")), "{:?}", d.notes);
    assert_eq!(with_fs(&mut img, |r| gens::scan(r).unwrap()).gens.len(), 2);
    assert_eq!(kernel_read(&kernel_view(&mut img), &[b"BITSOS2", b"data.img"]).unwrap(), data);
    fsck(&s.0, &mut img);
}

#[test]
fn a_stick_without_bitsos_is_never_updated_and_a_full_one_never_started() {
    let s = Scratch::new("refuse");
    let mut img = image(&s.0, 128);
    let p = plan(&s.0, "1", &[("BITSOS2/mesa/a", vec![1u8; 10])], Vec::new());
    assert!(stick::update(&mut img, &p, &mut |_| {}).unwrap_err().contains("refused"));
    let mut huge = p.clone();
    huge.bytes = 1 << 30;
    assert!(stick::fresh(&mut img, &huge, ID, "", &mut |_| {}).unwrap_err().contains("Nothing was written"));
    let mut z = [0u8; 4096];
    img.seek(SeekFrom::Start(0)).unwrap();
    img.read_exact(&mut z).unwrap();
    assert!(z.iter().all(|&b| b == 0), "refused before the first write");
}

/// A disk that takes whole, aligned 4 KiB blocks only: stricter than a raw Windows disk opened without buffering,
/// which wants whole aligned 512-byte sectors.
struct Sectors4k(Vec<u8>);

impl Sectors4k {
    fn check(&self, off: u64, p: *const u8, n: usize) -> std::io::Result<()> {
        let ok = off % 4096 == 0 && n % 4096 == 0 && p as usize % 4096 == 0 && off as usize + n <= self.0.len();
        if ok { Ok(()) } else { Err(std::io::Error::other(format!("unaligned I/O at {off}, {n} bytes"))) }
    }
}

impl devices::aligned::Raw for Sectors4k {
    fn read_at(&mut self, off: u64, buf: &mut [u8]) -> std::io::Result<()> {
        self.check(off, buf.as_ptr(), buf.len())?;
        buf.copy_from_slice(&self.0[off as usize..off as usize + buf.len()]);
        Ok(())
    }
    fn write_at(&mut self, off: u64, buf: &[u8]) -> std::io::Result<()> {
        self.check(off, buf.as_ptr(), buf.len())?;
        self.0[off as usize..off as usize + buf.len()].copy_from_slice(buf);
        Ok(())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// The Windows write path: a fresh stick and an update through whole aligned blocks give the stick the kernel takes.
#[test]
fn a_stick_written_in_whole_sectors_is_the_same_stick() {
    let s = Scratch::new("sectors");
    let size = 256u64 << 20;
    let mut dev = devices::aligned::Aligned::new(Sectors4k(vec![0u8; size as usize]), size, 4096).unwrap();
    let mesa = vec![("BITSOS2/mesa/lib/libvulkan.so.1", b"vk".to_vec())];
    stick::fresh(&mut dev, &plan(&s.0, "1", &mesa, Vec::new()), ID, "token=x\n", &mut |_| {}).unwrap();
    let d = stick::update(&mut dev, &plan(&s.0, "2", &mesa, Vec::new()), &mut |_| {}).unwrap();
    assert_eq!(d.sys, "1 OK, 2 NEW");
    let mut bytes = vec![0u8; size as usize];
    dev.seek(SeekFrom::Start(0)).unwrap();
    dev.read_exact(&mut bytes).unwrap();
    std::fs::write(s.0.join("stick.img"), bytes).unwrap();
    let mut img = devices::open_image(&s.0.join("stick.img"), None).unwrap();
    let v = kernel_view(&mut img);
    assert_eq!(kernel_read(&v, &[b"BITSOS2", b"STICK.ID"]).unwrap(), ID.as_bytes());
    assert_eq!(kernel_read(&v, &[b"BITSOS2", b"SYS", b"2", b"KERNEL"]).unwrap(), b"\x7fELF kernel 2");
    assert_eq!(kernel_read(&v, &[b"BITSOS2", b"SYS", b"1", b"INITRD"]).unwrap().len(), 3_000_000);
    drop(v);
    fsck(&s.0, &mut img);
}
