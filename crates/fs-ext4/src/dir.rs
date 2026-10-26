//! Directory blocks (linux-6.6/fs/ext4/ext4.h:2309 `struct ext4_dir_entry_2`, namei.c): entries, the
//! metadata_csum tail (`struct ext4_dir_entry_tail`), recognising htree index blocks, inserting into free
//! space and removing as `ext4_generic_delete_entry` does (the space goes to the entry before, or the inode
//! field is cleared when it is the first of its block).

use crate::crc::crc32c;
use crate::le::*;
use alloc::vec;
use alloc::vec::Vec;
use crate::errno::*;

pub const TAIL: usize = 12;
pub const FT_UNKNOWN: u8 = 0;
pub const FT_REG: u8 = 1;
pub const FT_DIR: u8 = 2;
pub const FT_CHR: u8 = 3;
pub const FT_BLK: u8 = 4;
pub const FT_FIFO: u8 = 5;
pub const FT_SOCK: u8 = 6;
pub const FT_SYMLINK: u8 = 7;

#[derive(Clone, Debug)]
pub struct DirEnt {
    pub off: usize,
    pub ino: u32,
    pub rec_len: usize,
    pub ftype: u8,
    pub name: Vec<u8>,
}

/// `EXT4_DIR_REC_LEN`: the bytes an entry with a name of `n` bytes needs.
pub fn rec_len(n: usize) -> usize {
    (8 + n + 3) & !3
}

pub fn ftype_of_mode(mode: u16) -> u8 {
    use crate::inode::*;
    match mode & S_IFMT {
        S_IFREG => FT_REG,
        S_IFDIR => FT_DIR,
        S_IFCHR => FT_CHR,
        S_IFBLK => FT_BLK,
        S_IFIFO => FT_FIFO,
        S_IFSOCK => FT_SOCK,
        S_IFLNK => FT_SYMLINK,
        _ => FT_UNKNOWN,
    }
}

/// The end of the entry area (where the checksum tail starts when there is one).
pub fn area(bs: usize, csum: bool) -> usize {
    if csum { bs - TAIL } else { bs }
}

/// Every entry of a leaf block's area (empty ones included), checked as `ext4_check_dir_entry` does.
pub fn entries(b: &[u8], end: usize) -> Result<Vec<DirEnt>, i64> {
    let end = end.min(b.len());
    let mut out = Vec::new();
    let mut off = 0;
    while off < end {
        let ino = g32(b, off);
        let rl = g16(b, off + 4) as usize;
        let nl = g8(b, off + 6) as usize;
        if rl < 12 || rl % 4 != 0 || off + rl > end || rec_len(nl) > rl {
            klog::event!(Warn, "ext4", "dir.bad_entry", off = off, rec_len = rl, name_len = nl);
            return Err(EUCLEAN);
        }
        out.push(DirEnt { off, ino, rec_len: rl, ftype: g8(b, off + 7), name: b[off + 8..off + 8 + nl].to_vec() });
        off += rl;
    }
    Ok(out)
}

/// An htree interior block: one fake entry over the whole block (`dx_node`).
pub fn is_dx_node(b: &[u8], bs: usize) -> bool {
    g32(b, 0) == 0 && g16(b, 4) as usize == bs && g8(b, 6) == 0
}

pub fn tail_ok(b: &[u8], bs: usize) -> bool {
    let t = bs - TAIL;
    g32(b, t) == 0 && g16(b, t + 4) == 12 && g8(b, t + 6) == 0 && g8(b, t + 7) == 0xDE
}

/// `ext4_dirblock_csum_verify`: a leaf with a tail whose checksum is right.
pub fn csum_ok(b: &[u8], bs: usize, seed: u32) -> bool {
    tail_ok(b, bs) && crc32c(seed, &b[..bs - TAIL]) == g32(b, bs - 4)
}

/// Writes the tail and its checksum (`ext4_dirblock_csum_set` after `ext4_initialize_dirent_tail`).
pub fn seal(b: &mut [u8], bs: usize, seed: Option<u32>) {
    let Some(seed) = seed else { return };
    let t = bs - TAIL;
    p32(b, t, 0);
    p16(b, t + 4, 12);
    p8(b, t + 6, 0);
    p8(b, t + 7, 0xDE);
    let c = crc32c(seed, &b[..t]);
    p32(b, t + 8, c);
}

fn put_entry(b: &mut [u8], off: usize, ino: u32, rl: usize, ftype: u8, name: &[u8]) {
    p32(b, off, ino);
    p16(b, off + 4, rl as u16);
    p8(b, off + 6, name.len() as u8);
    p8(b, off + 7, ftype);
    if let Some(s) = b.get_mut(off + 8..off + 8 + name.len()) {
        s.copy_from_slice(name);
    }
}

/// A block with no names (a new block of a growing directory).
pub fn empty_block(bs: usize, seed: Option<u32>) -> Vec<u8> {
    let mut b = vec![0u8; bs];
    let end = area(bs, seed.is_some());
    put_entry(&mut b, 0, 0, end, 0, &[]);
    seal(&mut b, bs, seed);
    b
}

/// The first block of a new directory: `.` and `..` (`ext4_init_dot_dotdot`).
pub fn first_block(bs: usize, seed: Option<u32>, me: u32, parent: u32) -> Vec<u8> {
    let mut b = vec![0u8; bs];
    let end = area(bs, seed.is_some());
    put_entry(&mut b, 0, me, 12, FT_DIR, b".");
    put_entry(&mut b, 12, parent, end - 12, FT_DIR, b"..");
    seal(&mut b, bs, seed);
    b
}

/// Puts a name into free space of this block; false when it does not fit.
pub fn insert(b: &mut [u8], end: usize, name: &[u8], ino: u32, ftype: u8) -> Result<bool, i64> {
    let need = rec_len(name.len());
    for e in entries(b, end)? {
        if e.ino == 0 && e.rec_len >= need {
            put_entry(b, e.off, ino, e.rec_len, ftype, name);
            return Ok(true);
        }
        let used = rec_len(e.name.len());
        if e.ino != 0 && e.rec_len - used >= need {
            p16(b, e.off + 4, used as u16);
            put_entry(b, e.off + used, ino, e.rec_len - used, ftype, name);
            return Ok(true);
        }
    }
    Ok(false)
}

/// Removes the entry at `off` (it must be one of this block's).
pub fn remove(b: &mut [u8], end: usize, off: usize) -> Result<(), i64> {
    let es = entries(b, end)?;
    let i = es.iter().position(|e| e.off == off).ok_or(EUCLEAN)?;
    if i == 0 {
        p32(b, off, 0);
    } else {
        let p = &es[i - 1];
        p16(b, p.off + 4, (p.rec_len + es[i].rec_len) as u16);
    }
    Ok(())
}

/// Points the entry at `off` at another inode (rename over an existing name, `..` of a moved directory).
pub fn retarget(b: &mut [u8], off: usize, ino: u32, ftype: u8) {
    p32(b, off, ino);
    p8(b, off + 7, ftype);
}

/// Validates a name for a new entry.
pub fn check_name(name: &[u8]) -> Result<(), i64> {
    if name.is_empty() || name.contains(&b'/') || name.contains(&0) {
        return Err(EINVAL);
    }
    if name.len() > 255 {
        return Err(ENAMETOOLONG);
    }
    Ok(())
}
