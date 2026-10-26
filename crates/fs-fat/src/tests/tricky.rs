//! V1's tricky stick (`BitsOS-ASM/fat-tricky.mjs`, ported): folders as Windows, Linux and other tools leave
//! them, and what must be read past. 512-byte clusters here (V1 used 16 KiB to stay above 65525 clusters at
//! 1.1 GB; 512 bytes reach FAT32 at 34 MiB), so the folders span more clusters, out of order.
//!
//! Differences to V1's expectations, on purpose: names are shown in full UTF-8 ("Länge.txt", not "L?nge.txt";
//! the 255-character name whole, not cut at 100), because V2 has no 100-byte name limit.

use super::util::*;
use crate::dirent::{lfn_entries, Entry, ATTR_ARCHIVE, ATTR_DIR, ATTR_VOLUME};
use crate::name::checksum;
use std::collections::BTreeMap;
use std::string::{String, ToString};
use std::vec::Vec;
use std::{format, vec};
use vfs::ops::Ctx;

const SS: usize = 512;

fn s83(base: &[u8], ext: &[u8]) -> [u8; 11] {
    let mut s = [b' '; 11];
    s[..base.len()].copy_from_slice(base);
    s[8..8 + ext.len()].copy_from_slice(ext);
    s
}

const STAMP_T: u16 = 0x6000;
const STAMP_D: u16 = ((2025 - 1980) << 9) | (7 << 5) | 14;

fn dirent(s: [u8; 11], attr: u8, cluster: u32, size: u32, ntres: u8) -> [u8; 32] {
    let mut e = Entry { name: s, attr, ntres, ctime_tenth: 50, ctime: STAMP_T, cdate: STAMP_D, adate: STAMP_D, first: cluster, wtime: STAMP_T, wdate: STAMP_D, size };
    e.ntres = ntres;
    e.bytes()
}

fn lfn(name: &str, s: [u8; 11], sum: Option<u8>) -> Vec<[u8; 32]> {
    let u: Vec<u16> = name.encode_utf16().collect();
    lfn_entries(&u, sum.unwrap_or_else(|| checksum(&s)))
}

fn deleted(mut e: [u8; 32]) -> [u8; 32] {
    e[0] = 0xE5;
    e
}

struct Builder {
    fat: Vec<u32>,
    body: BTreeMap<u32, Vec<u8>>,
    next: u32,
    seed: u32,
    files: BTreeMap<String, Vec<u8>>,
}

impl Builder {
    fn take(&mut self, n: usize) -> Vec<u32> {
        let v: Vec<u32> = (0..n as u32).map(|i| self.next + i).collect();
        self.next += n as u32;
        v
    }
    fn link(&mut self, chain: &[u32]) {
        for (i, &c) in chain.iter().enumerate() {
            self.fat[c as usize] = chain.get(i + 1).copied().unwrap_or(0x0FFF_FFFF);
        }
    }
    fn rnd(&mut self, n: usize) -> Vec<u8> {
        let mut b = vec![0u8; n];
        for x in b.iter_mut() {
            self.seed = (self.seed.wrapping_mul(1_103_515_245).wrapping_add(12345)) & 0x7FFF_FFFF;
            *x = (self.seed >> 16) as u8;
        }
        b
    }
    fn put_file(&mut self, path: &str, data: &[u8], chain: Option<Vec<u32>>) -> u32 {
        self.files.insert(path.to_string(), data.to_vec());
        if data.is_empty() {
            return 0;
        }
        let chain = chain.unwrap_or_else(|| self.take(data.len().div_ceil(SS)));
        self.link(&chain);
        for (i, &c) in chain.iter().enumerate() {
            let mut b = vec![0u8; SS];
            let part = &data[(i * SS).min(data.len())..((i + 1) * SS).min(data.len())];
            b[..part.len()].copy_from_slice(part);
            self.body.insert(c, b);
        }
        chain[0]
    }
    fn put_dir(&mut self, chain: &[u32], entries: &[[u8; 32]]) {
        self.link(chain);
        let raw: Vec<u8> = entries.iter().flatten().copied().collect();
        assert!(raw.len() <= chain.len() * SS, "tricky: folder too small");
        for (i, &c) in chain.iter().enumerate() {
            let mut b = vec![0u8; SS];
            let part = &raw[(i * SS).min(raw.len())..((i + 1) * SS).min(raw.len())];
            b[..part.len()].copy_from_slice(part);
            self.body.insert(c, b);
        }
    }
}

/// The image (MBR + FAT32 partition at 2048) and what a reader must see: files by path, names per folder.
fn tricky() -> (Vec<u8>, BTreeMap<String, Vec<u8>>, BTreeMap<String, Vec<String>>) {
    let (start, resv, total) = (2048u64, 32u64, 36 * 2048u64);
    let sectors = total - start;
    let mut fatsz = 1u64;
    loop {
        let n = (sectors - resv - 2 * fatsz) / 1;
        let need = ((n + 2) * 4).div_ceil(SS as u64);
        if need <= fatsz {
            break;
        }
        fatsz = need;
    }
    let clusters = sectors - resv - 2 * fatsz;
    assert!(clusters >= 65525);
    let mut b = Builder { fat: vec![0u32; clusters as usize + 2], body: BTreeMap::new(), next: 3, seed: 12345, files: BTreeMap::new() };
    b.fat[0] = 0x0FFF_FFF8;
    b.fat[1] = 0x0FFF_FFFF;
    // BITSOS's folder: 12 clusters in a scrambled order.
    let base = b.next;
    let bits: Vec<u32> = [0u32, 40, 20, 7, 33, 2, 25, 11, 38, 15, 29, 4].iter().map(|k| base + k).collect();
    b.next += 41;
    let efi = b.take(1);
    let junk = b.take(1);
    let games = b.take(1);
    let holes = b.take(1);
    let mut e: Vec<[u8; 32]> = vec![dirent(s83(b".", b""), ATTR_DIR, bits[0], 0, 0), dirent(s83(b"..", b""), ATTR_DIR, 0, 0, 0)];
    let mut shown: Vec<String> = Vec::new();
    let file = |b: &mut Builder, e: &mut Vec<[u8; 32]>, shown: &mut Vec<String>, name: &str, s: [u8; 11], attr: u8, data: Vec<u8>, long: bool, ntres: u8, chain: Option<Vec<u32>>, show: &str| {
        if long {
            e.extend(lfn(name, s, None));
        }
        let c = b.put_file(&format!("BITSOS/{show}"), &data, chain);
        e.push(dirent(s, attr, c, data.len() as u32, ntres));
        shown.push(show.to_string());
    };
    let k = b.rnd(70_000);
    file(&mut b, &mut e, &mut shown, "KERNEL.BIN", s83(b"KERNEL", b"BIN"), ATTR_ARCHIVE, k, false, 0, None, "KERNEL.BIN");
    file(&mut b, &mut e, &mut shown, "STICK.ID", s83(b"STICK", b"ID"), ATTR_ARCHIVE, b"0123456789abcdef0123456789abcdef\r\n".to_vec(), false, 0, None, "STICK.ID");
    file(&mut b, &mut e, &mut shown, "CONFIG.TXT", s83(b"CONFIG", b"TXT"), ATTR_ARCHIVE, b"screen=1024x768\r\n".to_vec(), false, 0, None, "CONFIG.TXT");
    file(&mut b, &mut e, &mut shown, "demo.asm", s83(b"DEMO", b"ASM"), ATTR_ARCHIVE, b"main:\n    ret\n".to_vec(), false, 0x18, None, "demo.asm");
    let r = b.rnd(5000);
    file(&mut b, &mut e, &mut shown, "Readme.TXT", s83(b"README", b"TXT"), ATTR_ARCHIVE, r, true, 0, None, "Readme.TXT");
    e.extend(lfn("Ghost of a long name.txt", s83(b"ORPHAN", b"TXT"), Some(checksum(&s83(b"ORPHAN", b"TXT")) ^ 0x5A)));
    file(&mut b, &mut e, &mut shown, "ORPHAN.TXT", s83(b"ORPHAN", b"TXT"), ATTR_ARCHIVE, b"orphan\r\n".to_vec(), false, 0, None, "ORPHAN.TXT");
    for x in lfn("Deleted long name.txt", s83(b"DELETE~1", b"TXT"), None) {
        e.push(deleted(x));
    }
    e.push(deleted(dirent(s83(b"DELETE~1", b"TXT"), ATTR_ARCHIVE, 0, 0, 0)));
    e.extend(lfn("Half deleted.txt", s83(b"HALFDE~1", b"TXT"), None));
    e.push(deleted(dirent(s83(b"HALFDE~1", b"TXT"), ATTR_ARCHIVE, 0, 0, 0)));
    for (n, s, len) in [("This is file one.txt", s83(b"THISIS~1", b"TXT"), 100), ("This is file two.txt", s83(b"THISIS~2", b"TXT"), 101), ("This is file five.txt", s83(b"THC0A3~1", b"TXT"), 33)] {
        let d = b.rnd(len);
        file(&mut b, &mut e, &mut shown, n, s, ATTR_ARCHIVE, d, true, 0, None, n);
    }
    let umlaut = s83(b"L\x8eNGE", b"TXT");
    e.extend(lfn("Länge.txt", umlaut, None));
    let c = b.put_file("BITSOS/Länge.txt", b"umlaut\r\n", None);
    e.push(dirent(umlaut, ATTR_ARCHIVE, c, 8, 0));
    shown.push("Länge.txt".to_string());
    file(&mut b, &mut e, &mut shown, "empty file.dat", s83(b"EMPTYF~1", b"DAT"), ATTR_ARCHIVE, Vec::new(), true, 0, None, "empty file.dat");
    let frag = b.rnd(3 * SS + 100);
    let mut fc = b.take(4);
    fc.reverse();
    file(&mut b, &mut e, &mut shown, "FRAG.BIN", s83(b"FRAG", b"BIN"), ATTR_ARCHIVE, frag, false, 0, Some(fc), "FRAG.BIN");
    let d13 = b.rnd(13);
    file(&mut b, &mut e, &mut shown, "abcdefghijklm", s83(b"ABCDEF~1", b""), ATTR_ARCHIVE, d13, true, 0, None, "abcdefghijklm");
    let d26 = b.rnd(26);
    file(&mut b, &mut e, &mut shown, "abcdefghijklmnopqrstuvwxyz", s83(b"ABCDEF~2", b""), ATTR_ARCHIVE, d26, true, 0, None, "abcdefghijklmnopqrstuvwxyz");
    file(&mut b, &mut e, &mut shown, "RO.TXT", s83(b"RO", b"TXT"), 0x21, b"read only\r\n".to_vec(), false, 0, None, "RO.TXT");
    file(&mut b, &mut e, &mut shown, "HIDSYS.TXT", s83(b"HIDSYS", b"TXT"), 0x26, b"hidden\r\n".to_vec(), false, 0, None, "HIDSYS.TXT");
    let gap = lfn("Gap set with three parts in it.txt", s83(b"GAPSET~1", b"TXT"), None);
    e.push(gap[0]);
    e.push(gap[2]);
    file(&mut b, &mut e, &mut shown, "GAPSET~1.TXT", s83(b"GAPSET~1", b"TXT"), ATTR_ARCHIVE, b"gap\r\n".to_vec(), false, 0, None, "GAPSET~1.TXT");
    e.push(lfn("Incomplete long name.txt", s83(b"INCOMP~1", b"TXT"), None)[0]);
    file(&mut b, &mut e, &mut shown, "INCOMP~1.TXT", s83(b"INCOMP~1", b"TXT"), ATTR_ARCHIVE, b"incomplete\r\n".to_vec(), false, 0, None, "INCOMP~1.TXT");
    let mut mixed = lfn("Mixed checksum parts.txt", s83(b"MIXSUM~1", b"TXT"), None);
    mixed[1][13] ^= 0x5A;
    e.extend(mixed);
    file(&mut b, &mut e, &mut shown, "MIXSUM~1.TXT", s83(b"MIXSUM~1", b"TXT"), ATTR_ARCHIVE, b"mixed\r\n".to_vec(), false, 0, None, "MIXSUM~1.TXT");
    e.extend(lfn("Games", s83(b"GAMES", b""), None));
    e.push(dirent(s83(b"GAMES", b""), ATTR_DIR, games[0], 0, 0));
    shown.push("Games".to_string());
    e.push(dirent(s83(b"JUNK", b""), ATTR_DIR, junk[0], 0, 0));
    e.push(dirent(s83(b"HOLES", b""), ATTR_DIR, holes[0], 0, 0));
    shown.push("JUNK".to_string());
    shown.push("HOLES".to_string());
    // Fillers up to just before a cluster's end, then a set across the boundary (16 entries per cluster).
    let per = SS / 32;
    let mut k = 0;
    while e.len() % per != per - 2 {
        let sn = format!("FILLER~{}", k + 1);
        let n = format!("Filler number {k:03}.bin");
        let d = b.rnd(10 + k);
        file(&mut b, &mut e, &mut shown, &n, s83(sn.as_bytes(), b"BIN"), ATTR_ARCHIVE, d, true, 0, None, &n);
        k += 1;
    }
    let a = b.rnd(1234);
    file(&mut b, &mut e, &mut shown, "Across the cluster boundary file.txt", s83(b"ACROSS~1", b"TXT"), ATTR_ARCHIVE, a, true, 0, None, "Across the cluster boundary file.txt");
    let longest: String = ("L".to_string() + &"o".repeat(250) + "ng.x").chars().take(255).collect();
    e.extend(lfn(&longest, s83(b"LOOOOO~1", b"X"), None));
    let d = b.rnd(55);
    let c = b.put_file(&format!("BITSOS/{longest}"), &d, None);
    e.push(dirent(s83(b"LOOOOO~1", b"X"), ATTR_ARCHIVE, c, 55, 0));
    shown.push(longest.clone());
    for _ in 0..5 {
        e.push(deleted([0u8; 32]));
    }
    b.put_dir(&bits, &e);
    let mut list: BTreeMap<String, Vec<String>> = BTreeMap::new();
    list.insert("BITSOS".into(), shown);
    let a_c = b.put_file("BITSOS/JUNK/A.TXT", b"a\r\n", None);
    b.put_dir(&junk, &[dirent(s83(b".", b""), ATTR_DIR, junk[0], 0, 0), dirent(s83(b"..", b""), ATTR_DIR, bits[0], 0, 0), dirent(s83(b"A", b"TXT"), ATTR_ARCHIVE, a_c, 3, 0), deleted([0u8; 32]), [0u8; 32], dirent(s83(b"GHOST", b"TXT"), ATTR_ARCHIVE, 0, 0, 0)]);
    list.insert("BITSOS/JUNK".into(), vec!["A.TXT".into()]);
    b.put_dir(&games, &[dirent(s83(b".", b""), ATTR_DIR, games[0], 0, 0), dirent(s83(b"..", b""), ATTR_DIR, bits[0], 0, 0)]);
    list.insert("BITSOS/Games".into(), Vec::new());
    let mut h = vec![dirent(s83(b".", b""), ATTR_DIR, holes[0], 0, 0), dirent(s83(b"..", b""), ATTR_DIR, bits[0], 0, 0)];
    h.extend(lfn("gone gone gone.txt", s83(b"GONEGO~1", b"TXT"), None).into_iter().map(deleted));
    h.extend(lfn("orphan only", s83(b"NOTHERE", b""), None));
    b.put_dir(&holes, &h);
    list.insert("BITSOS/HOLES".into(), Vec::new());
    b.put_dir(&efi, &[dirent(s83(b".", b""), ATTR_DIR, efi[0], 0, 0), dirent(s83(b"..", b""), ATTR_DIR, 0, 0, 0)]);
    let mut root = vec![dirent(s83(b"TRICKY", b""), ATTR_VOLUME, 0, 0, 0), dirent(s83(b"EFI", b""), ATTR_DIR, efi[0], 0, 0)];
    root.extend(lfn("BITSOS", s83(b"BITSOS", b""), None));
    root.push(dirent(s83(b"BITSOS", b""), ATTR_DIR, bits[0], 0, 0));
    let rc = b.put_file("ROOT.TXT", b"root file\r\n", None);
    root.push(dirent(s83(b"ROOT", b"TXT"), ATTR_ARCHIVE, rc, 11, 0));
    b.put_dir(&[2], &root);
    list.insert(String::new(), vec!["EFI".into(), "BITSOS".into(), "ROOT.TXT".into()]);

    let mut img = vec![0u8; (total as usize) * SS];
    let data_at = (start + resv + 2 * fatsz) as usize * SS;
    for (c, body) in &b.body {
        let at = data_at + (*c as usize - 2) * SS;
        img[at..at + SS].copy_from_slice(body);
    }
    let mut v = crate::bpb::tests::sector(1, sectors as u32, fatsz as u32);
    v[28..32].copy_from_slice(&(start as u32).to_le_bytes());
    v[71..82].copy_from_slice(b"TRICKY     ");
    let mut fsi = vec![0u8; SS];
    fsi[0..4].copy_from_slice(&0x4161_5252u32.to_le_bytes());
    fsi[484..488].copy_from_slice(&0x6141_7272u32.to_le_bytes());
    fsi[488..496].fill(0xFF);
    fsi[508..512].copy_from_slice(&0xAA55_0000u32.to_le_bytes());
    for base in [0u64, 6] {
        let at = ((start + base) as usize) * SS;
        img[at..at + SS].copy_from_slice(&v);
        img[at + SS..at + 2 * SS].copy_from_slice(&fsi);
    }
    let fb: Vec<u8> = b.fat.iter().flat_map(|x| x.to_le_bytes()).collect();
    for k in 0..2u64 {
        let at = ((start + resv + k * fatsz) as usize) * SS;
        img[at..at + fb.len()].copy_from_slice(&fb);
    }
    img[446] = 0x80;
    img[446 + 4] = 0x0C;
    img[446 + 8..446 + 12].copy_from_slice(&(start as u32).to_le_bytes());
    img[446 + 12..446 + 16].copy_from_slice(&(sectors as u32).to_le_bytes());
    img[510] = 0x55;
    img[511] = 0xAA;
    // The volume alone (what `stick::DiskDev` hands the FAT code: the partition).
    let vol = img[start as usize * SS..].to_vec();
    (vol, b.files, list)
}

#[test]
fn reads_v1_tricky_stick() {
    let (img, files, list) = tricky();
    let dev = std::sync::Arc::new(crate::dev::MemDev::new(img.clone(), SS));
    let vol = mount_plain(&dev);
    assert_eq!(vol.label_text(), b"TRICKY");
    let ns = view(&vol);
    for (dir, want) in &list {
        let p = if dir.is_empty() { String::from("/boot") } else { format!("/boot/{dir}") };
        let got: Vec<String> = ns.list_dir(&p).unwrap().into_iter().map(|(n, _)| String::from_utf8(n).unwrap()).collect();
        assert_eq!(&got, want, "{p}");
    }
    for (path, data) in &files {
        assert!(ns.read_file(&format!("/boot/{path}"), 1 << 20).unwrap() == *data, "{path}");
    }
    let ro = ns.lookup("/boot/BITSOS/RO.TXT").unwrap();
    assert_eq!(ro.inode.meta.mode() & 0o777, 0o555, "the read-only attribute");
    assert_eq!(dev.image(), img, "reading wrote nothing");

    // Writing into the folder whose end mark hides garbage: the garbage stays hidden.
    let c = Ctx::kernel(&ns);
    ns.create_file_bytes(b"/boot/BITSOS/JUNK/B.TXT", 0o644, b"b").unwrap();
    ns.lookup("/boot/BITSOS/JUNK/B.TXT").unwrap().inode.sync(vfs::pagecache::SyncKind::Fsync).unwrap();
    // A new name next to the ones with ~n aliases gets a free one.
    ns.create_file_bytes(b"/boot/BITSOS/This is file three.txt", 0o644, b"3").unwrap().inode.sync(vfs::pagecache::SyncKind::Fsync).unwrap();
    c.unlink(None, b"/boot/BITSOS/Readme.TXT").unwrap();
    vol.sync(true).unwrap();
    let vol2 = mount_plain(&dev);
    let ns2 = view(&vol2);
    let junk: Vec<String> = ns2.list_dir("/boot/BITSOS/JUNK").unwrap().into_iter().map(|(n, _)| String::from_utf8(n).unwrap()).collect();
    assert_eq!(junk, ["A.TXT", "B.TXT"]);
    assert_eq!(ns2.read_file("/boot/BITSOS/this is file three.txt", 10).unwrap(), b"3");
    assert!(ns2.lookup("/boot/BITSOS/Readme.TXT").is_err());
    let st = vol2.st.lock();
    let n = st.nodes.values().find(|n| n.name == b"This is file three.txt").unwrap();
    assert_eq!(&n.e.name, b"THISIS~3TXT");
}
