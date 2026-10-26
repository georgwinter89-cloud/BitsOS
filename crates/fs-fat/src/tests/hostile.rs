//! Hostile images: a mkfs.fat image patched the way a broken or crafted stick could be (boot sector fields,
//! FAT loops and links outside the volume, directory entries with wrong clusters or sizes, long-name parts with
//! a wrong checksum or order, a directory that contains itself). Every case must end in an errno or a refused
//! mount: never a panic, an endless loop or a read past the device.

use super::util::*;
use crate::vol::{Clock, Options, Volume};
use std::boxed::Box;
use std::string::String;
use std::sync::Arc;
use std::vec::Vec;
use std::{format, vec};
use uapi::errno::*;
use vfs::abi::{O_CREAT, O_WRONLY};
use vfs::ops::Ctx;
use vfs::pagecache::SyncKind;

fn base(dir: &std::path::Path) -> Vec<u8> {
    let img = mkfs(dir, 40, 1, "HOSTILE");
    mkdir(&img, "/D");
    put_bytes(&img, dir, "/D/FILE.BIN", &pattern(5000, 1));
    put_bytes(&img, dir, "/ONE.TXT", &pattern(100, 2));
    put_bytes(&img, dir, "/Long name file.txt", b"long");
    std::fs::read(&img).unwrap()
}

fn u16_at(b: &[u8], o: usize) -> u64 {
    u16::from_le_bytes([b[o], b[o + 1]]) as u64
}
fn u32_at(b: &[u8], o: usize) -> u64 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]) as u64
}
fn put32(b: &mut [u8], o: usize, v: u32) {
    b[o..o + 4].copy_from_slice(&v.to_le_bytes());
}

/// The byte offset of the short entry named `short` (11 bytes, as on disk).
fn entry(b: &[u8], short: &[u8; 11]) -> usize {
    b.windows(11).position(|w| w == short).unwrap_or_else(|| panic!("{}", String::from_utf8_lossy(short)))
}
fn first_cluster(b: &[u8], e: usize) -> u32 {
    ((u16_at(b, e + 20) << 16) | u16_at(b, e + 26)) as u32
}
fn set_first(b: &mut [u8], e: usize, c: u32) {
    b[e + 20..e + 22].copy_from_slice(&((c >> 16) as u16).to_le_bytes());
    b[e + 26..e + 28].copy_from_slice(&(c as u16).to_le_bytes());
}
/// Sets cluster `c`'s entry in every FAT copy.
fn set_fat(b: &mut [u8], c: u32, v: u32) {
    let (reserved, fats, size) = (u16_at(b, 14), b[16] as u64, u32_at(b, 36));
    for k in 0..fats {
        put32(b, ((reserved + k * size) * 512 + 4 * c as u64) as usize, v);
    }
}

fn mount(b: Vec<u8>) -> Result<Arc<Volume>, &'static str> {
    Volume::mount(Arc::new(crate::MemDev::new(b, 512)), Options { writable: true, stick: false }, Clock::UTC_EPOCH)
}

/// Walks the tree (at most `depth` levels: a directory may contain itself), reads every file, writes one
/// byte into each and makes a file in each directory. Errors are fine; panics and hangs are not.
fn exercise(ns: &vfs::MountNs, p: &str, depth: u32, errors: &mut Vec<i64>) {
    let c = Ctx::kernel(ns);
    let list = match ns.list_dir(p) {
        Ok(l) => l,
        Err(e) => return errors.push(e),
    };
    if let Err(e) = c.open(None, format!("{p}/new{depth}").as_bytes(), O_WRONLY | O_CREAT, 0o644) {
        errors.push(e);
    }
    for (n, a) in list {
        let path = format!("{p}/{}", String::from_utf8_lossy(&n));
        if a.mode & uapi::S_IFMT == uapi::S_IFDIR {
            if depth > 0 {
                exercise(ns, &path, depth - 1, errors);
            }
            continue;
        }
        if let Err(e) = ns.read_file(&path, 4 << 20) {
            errors.push(e);
        }
        match c.open(None, path.as_bytes(), O_WRONLY, 0) {
            Ok(f) => {
                if let Err(e) = f.pwrite(1, b"x").and_then(|_| f.inode().ok_or(EIO)?.sync(SyncKind::Fsync)) {
                    errors.push(e);
                }
            }
            Err(e) => errors.push(e),
        }
    }
}

#[test]
fn broken_boot_sectors_are_refused() {
    let dir = scratch("hostile-bpb");
    let good = base(&dir);
    assert!(mount(good.clone()).is_ok());
    type Patch = fn(&mut Vec<u8>);
    let cases: Vec<(&str, Patch)> = vec![
        ("sectors per cluster 0", |b| b[13] = 0),
        ("sectors per cluster 3", |b| b[13] = 3),
        ("bytes per sector 0", |b| b[11..13].copy_from_slice(&[0, 0])),
        ("no reserved sectors", |b| b[14..16].copy_from_slice(&[0, 0])),
        ("no FAT", |b| b[16] = 0),
        ("FAT size 0", |b| put32(b, 36, 0)),
        ("FAT size 1", |b| put32(b, 36, 1)),
        ("root cluster beyond the volume", |b| put32(b, 44, 0x0FFF_FFF0)),
        ("root cluster 1", |b| put32(b, 44, 1)),
        ("more sectors than the device", |b| put32(b, 32, 0xFFFF_FFFF)),
        ("device cut in half", |b| {
            let n = b.len() / 2;
            b.truncate(n)
        }),
        ("root chain free", |b| {
            let r = u32_at(b, 44) as u32;
            set_fat(b, r, 0)
        }),
        ("root chain loops", |b| {
            let r = u32_at(b, 44) as u32;
            set_fat(b, r, r)
        }),
    ];
    for (what, patch) in cases {
        let mut b = good.clone();
        patch(&mut b);
        assert!(mount(b).is_err(), "{what}: mounted");
    }
    // FSInfo pointing outside the reserved area, or garbage in it: ignored, the volume still works.
    for fsinfo in [0xFFFFu16, 0, 31] {
        let mut b = good.clone();
        b[48..50].copy_from_slice(&fsinfo.to_le_bytes());
        let vol = mount(b).unwrap_or_else(|e| panic!("FSInfo {fsinfo}: {e}"));
        let ns = view(&vol);
        let mut errors = Vec::new();
        exercise(&ns, "/boot", 4, &mut errors);
        assert!(errors.is_empty(), "FSInfo {fsinfo}: {errors:?}");
    }
}

#[test]
fn broken_chains_and_entries_are_errnos() {
    let dir = scratch("hostile-chain");
    let good = base(&dir);
    let file = entry(&good, b"FILE    BIN");
    let one = entry(&good, b"ONE     TXT");
    let d = entry(&good, b"D          ");
    let clusters = {
        let g = crate::bpb::parse(&good[..512], 512, (good.len() / 512) as u64).unwrap();
        g.clusters
    };
    type Patch = Box<dyn Fn(&mut Vec<u8>)>;
    let cases: Vec<(&str, Patch, Option<i64>)> = vec![
        ("a file's chain loops", Box::new(move |b| {
            let c = first_cluster(b, file);
            set_fat(b, c + 1, c)
        }), Some(EIO)),
        ("a chain leaves the volume", Box::new(move |b| {
            let c = first_cluster(b, one);
            set_fat(b, c, clusters + 5)
        }), None),
        ("a chain reaches a free cluster", Box::new(move |b| {
            let c = first_cluster(b, file);
            set_fat(b, c + 2, 0)
        }), Some(EIO)),
        ("an entry's first cluster beyond the volume", Box::new(move |b| set_first(b, one, 0x0FFF_FFF0)), None),
        ("an entry's first cluster 1", Box::new(move |b| set_first(b, one, 1)), None),
        ("a size beyond the chain", Box::new(move |b| put32(b, one + 28, 0xFFFF_FFFF)), None),
        ("a directory's chain loops", Box::new(move |b| {
            let c = first_cluster(b, d);
            set_fat(b, c, c)
        }), Some(EIO)),
        ("a directory contains itself", Box::new(move |b| {
            let c = first_cluster(b, d);
            let at = entry(b, b"FILE    BIN");
            set_first(b, at, c);
            b[at + 11] = crate::dirent::ATTR_DIR;
        }), None),
        ("a file marked as directory", Box::new(move |b| b[one + 11] = crate::dirent::ATTR_DIR), None),
        ("a directory marked as file", Box::new(move |b| b[d + 11] = 0x20), None),
        ("a directory without a cluster", Box::new(move |b| set_first(b, d, 0)), None),
    ];
    for (what, patch, want) in cases {
        let mut b = good.clone();
        patch(&mut b);
        let vol = mount(b).unwrap_or_else(|e| panic!("{what}: {e}"));
        let ns = view(&vol);
        let mut errors = Vec::new();
        exercise(&ns, "/boot", 6, &mut errors);
        assert!(errors.iter().all(|&e| e > 0), "{what}: {errors:?}");
        if let Some(w) = want {
            assert!(errors.contains(&w), "{what}: no {w} in {errors:?}");
        }
    }
}

#[test]
fn broken_long_names_show_the_short_name() {
    let dir = scratch("hostile-lfn");
    let good = base(&dir);
    let short = entry(&good, b"LONGNA~1TXT");
    let check = |b: Vec<u8>, what: &str| {
        let vol = mount(b).unwrap();
        let ns = view(&vol);
        let names: Vec<String> = ns.list_dir("/boot").unwrap().into_iter().map(|(n, _)| String::from_utf8(n).unwrap()).collect();
        assert!(names.iter().any(|n| n == "LONGNA~1.TXT"), "{what}: {names:?}");
        assert_eq!(ns.read_file("/boot/LONGNA~1.TXT", 64).unwrap(), b"long", "{what}");
    };
    // A wrong checksum in the last part (the one right before the short entry).
    let mut b = good.clone();
    b[short - 32 + 13] ^= 0xFF;
    check(b, "checksum");
    // Parts out of order: the sequence numbers of the two parts swapped.
    let mut b = good.clone();
    let (p1, p2) = (short - 32, short - 64);
    let (s1, s2) = (b[p1], b[p2]);
    b[p1] = s2;
    b[p2] = s1;
    check(b, "order");
    // A part without the 0x40 "last" flag first.
    let mut b = good.clone();
    b[p2] &= !0x40;
    check(b, "no last flag");
}
