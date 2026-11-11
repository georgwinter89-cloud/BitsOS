//! fatfs 0.3.6 writes a long-name entry in front of every short one, `.` and `..` of a new folder included, so a
//! folder's first two slots are not `.` and `..` as the FAT specification wants (fsck.fat, chkdsk and some firmware
//! check that), and its `..` of a folder in the root names the root's cluster where the specification wants 0. This
//! pass, run after the file system is unmounted, moves `.` and `..` into slots 0 and 1 (the freed slots marked
//! deleted), points `..` of the root's folders at 0, and writes the free-cluster count into FSInfo, which fatfs
//! leaves "unknown".

use std::io::{self, Read, Seek, SeekFrom, Write};

struct Geo {
    fsinfo: u64,
    bps: u64,
    spc: u64,
    fat_start: u64,
    data_start: u64,
    root: u32,
    clusters: u32,
}

fn geo<T: Read + Seek>(v: &mut T) -> io::Result<Geo> {
    let mut b = [0u8; 512];
    v.seek(SeekFrom::Start(0))?;
    v.read_exact(&mut b)?;
    let u16at = |o: usize| u16::from_le_bytes([b[o], b[o + 1]]) as u64;
    let u32at = |o: usize| u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]) as u64;
    let (bps, spc, reserved, fats, fat_size, total) = (u16at(11), b[13] as u64, u16at(14), b[16] as u64, u32at(36), u32at(32));
    if bps < 512 || spc == 0 || fat_size == 0 || &b[82..87] != b"FAT32" {
        return Err(io::Error::other("not a FAT32 volume"));
    }
    let data_start = (reserved + fats * fat_size) * bps;
    Ok(Geo { fsinfo: u16at(48) * bps, bps, spc, fat_start: reserved * bps, data_start, root: u32at(44) as u32, clusters: ((total * bps - data_start) / (spc * bps)) as u32 })
}

impl Geo {
    fn cluster_bytes(&self) -> u64 {
        self.bps * self.spc
    }
    fn at(&self, c: u32) -> u64 {
        self.data_start + (c as u64 - 2) * self.cluster_bytes()
    }
}

fn chain<T: Read + Seek>(v: &mut T, g: &Geo, first: u32) -> io::Result<Vec<u32>> {
    let mut out = Vec::new();
    let mut c = first;
    while (2..g.clusters + 2).contains(&c) {
        if out.len() > 65_536 {
            return Err(io::Error::other("a folder's cluster chain loops"));
        }
        out.push(c);
        let mut e = [0u8; 4];
        v.seek(SeekFrom::Start(g.fat_start + c as u64 * 4))?;
        v.read_exact(&mut e)?;
        c = u32::from_le_bytes(e) & 0x0FFF_FFFF;
    }
    Ok(out)
}

const DOT: &[u8; 11] = b".          ";
const DOTDOT: &[u8; 11] = b"..         ";

/// Fixes every folder below the root; returns how many it changed.
pub fn fix<T: Read + Write + Seek>(v: &mut T) -> io::Result<usize> {
    let g = geo(v)?;
    let cb = g.cluster_bytes() as usize;
    // (first cluster, the folder's parent's first cluster; None for the root itself)
    let mut todo: Vec<(u32, Option<u32>)> = vec![(g.root, None)];
    let mut fixed = 0;
    let mut seen = std::collections::BTreeSet::new();
    while let Some((first, parent)) = todo.pop() {
        let is_root = parent.is_none();
        if !seen.insert(first) {
            continue;
        }
        let clusters = chain(v, &g, first)?;
        let mut raw = vec![0u8; clusters.len() * cb];
        for (i, &c) in clusters.iter().enumerate() {
            v.seek(SeekFrom::Start(g.at(c)))?;
            v.read_exact(&mut raw[i * cb..(i + 1) * cb])?;
        }
        if !is_root && raw.len() >= 128 && raw[11] == 0x0F && &raw[32..43] == DOT && raw[64 + 11] == 0x0F && &raw[96..107] == DOTDOT {
            let (dot, dotdot) = (raw[32..64].to_vec(), raw[96..128].to_vec());
            raw[0..32].copy_from_slice(&dot);
            raw[32..64].copy_from_slice(&dotdot);
            raw[64] = 0xE5;
            raw[96] = 0xE5;
            v.seek(SeekFrom::Start(g.at(clusters[0])))?;
            v.write_all(&raw[..cb])?;
            fixed += 1;
        }
        if let Some(p) = parent
            && raw.len() >= 64
            && &raw[32..43] == DOTDOT
        {
            let want = if p == g.root { 0 } else { p };
            let have = (u16::from_le_bytes([raw[52], raw[53]]) as u32) << 16 | u16::from_le_bytes([raw[58], raw[59]]) as u32;
            if have != want {
                raw[52..54].copy_from_slice(&((want >> 16) as u16).to_le_bytes());
                raw[58..60].copy_from_slice(&(want as u16).to_le_bytes());
                v.seek(SeekFrom::Start(g.at(clusters[0]) + 32))?;
                v.write_all(&raw[32..64])?;
                fixed += 1;
            }
        }
        for e in raw.chunks_exact(32) {
            match e[0] {
                0 => break,
                0xE5 => continue,
                _ => {}
            }
            let (attr, name) = (e[11], &e[..11]);
            if attr == 0x0F || attr & 0x08 != 0 || attr & 0x10 == 0 || name == DOT || name == DOTDOT {
                continue;
            }
            let c = (u16::from_le_bytes([e[20], e[21]]) as u32) << 16 | u16::from_le_bytes([e[26], e[27]]) as u32;
            todo.push((c, Some(first)));
        }
    }
    free_count(v, &g)?;
    v.flush()?;
    Ok(fixed)
}

/// FSInfo's free count and next-free hint, from the first FAT.
fn free_count<T: Read + Write + Seek>(v: &mut T, g: &Geo) -> io::Result<()> {
    let mut fat = vec![0u8; (g.clusters as usize + 2) * 4];
    v.seek(SeekFrom::Start(g.fat_start))?;
    v.read_exact(&mut fat)?;
    let free: Vec<u32> = (2..g.clusters + 2).filter(|&c| u32::from_le_bytes(fat[c as usize * 4..c as usize * 4 + 4].try_into().expect("4 bytes")) & 0x0FFF_FFFF == 0).collect();
    let mut s = vec![0u8; g.bps as usize];
    v.seek(SeekFrom::Start(g.fsinfo))?;
    v.read_exact(&mut s)?;
    if g.fsinfo == 0 || s[0..4] != 0x4161_5252u32.to_le_bytes() || s[484..488] != 0x6141_7272u32.to_le_bytes() {
        return Ok(());
    }
    s[488..492].copy_from_slice(&(free.len() as u32).to_le_bytes());
    s[492..496].copy_from_slice(&free.first().copied().unwrap_or(0xFFFF_FFFF).to_le_bytes());
    v.seek(SeekFrom::Start(g.fsinfo))?;
    v.write_all(&s)
}
