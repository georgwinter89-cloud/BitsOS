//! Directory blocks (linux-7.3.1/fs/ext4/ext4.h:2528 `struct ext4_dir_entry_2`, namei.c): entries, the
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

/// One entry of a leaf block, its name left in the block (`name`): walking 620 blocks of a big directory must
/// not make a heap block per name.
#[derive(Clone, Copy, Debug)]
pub struct Raw {
    pub off: usize,
    pub ino: u32,
    pub rec_len: usize,
    pub name_len: usize,
    pub ftype: u8,
}

impl Raw {
    pub fn name<'a>(&self, b: &'a [u8]) -> &'a [u8] {
        &b[self.off + 8..self.off + 8 + self.name_len]
    }
    /// The largest record that fits here: all of an empty entry, the space behind the name of a live one.
    pub fn slack(&self) -> usize {
        if self.ino == 0 { self.rec_len } else { self.rec_len - rec_len(self.name_len) }
    }
    pub fn owned(&self, b: &[u8]) -> DirEnt {
        DirEnt { off: self.off, ino: self.ino, rec_len: self.rec_len, ftype: self.ftype, name: self.name(b).to_vec() }
    }
}

/// The entries of a leaf block's area in order (empty ones included), each checked as `ext4_check_dir_entry`
/// does; ends after the first bad one.
pub struct Walk<'a> {
    b: &'a [u8],
    end: usize,
    off: usize,
}

pub fn walk(b: &[u8], end: usize) -> Walk<'_> {
    Walk { b, end: end.min(b.len()), off: 0 }
}

impl Iterator for Walk<'_> {
    type Item = Result<Raw, i64>;
    fn next(&mut self) -> Option<Result<Raw, i64>> {
        let (b, off, end) = (self.b, self.off, self.end);
        if off >= end {
            return None;
        }
        if off + 8 > end {
            self.off = end;
            return Some(Err(EUCLEAN));
        }
        let rl = g16(b, off + 4) as usize;
        let nl = g8(b, off + 6) as usize;
        if rl < 12 || rl % 4 != 0 || off + rl > end || rec_len(nl) > rl {
            klog::event!(Warn, "ext4", "dir.bad_entry", off = off, rec_len = rl, name_len = nl);
            self.off = end;
            return Some(Err(EUCLEAN));
        }
        self.off = off + rl;
        Some(Ok(Raw { off, ino: g32(b, off), rec_len: rl, name_len: nl, ftype: g8(b, off + 7) }))
    }
}

/// Every entry of a leaf block's area (empty ones included), checked as `ext4_check_dir_entry` does.
pub fn entries(b: &[u8], end: usize) -> Result<Vec<DirEnt>, i64> {
    walk(b, end).map(|e| e.map(|e| e.owned(b))).collect()
}

/// The largest record that fits into this block (0: it is full).
pub fn room(b: &[u8], end: usize) -> Result<usize, i64> {
    let mut most = 0;
    for e in walk(b, end) {
        most = most.max(e?.slack());
    }
    Ok(most)
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

/// Puts a name into free space of this block (the first entry with room); returns where, None when it does
/// not fit.
pub fn insert_at(b: &mut [u8], end: usize, name: &[u8], ino: u32, ftype: u8) -> Result<Option<usize>, i64> {
    let need = rec_len(name.len());
    let mut fit = None;
    for e in walk(b, end) {
        let e = e?;
        if e.slack() >= need {
            fit = Some(e);
            break;
        }
    }
    let Some(e) = fit else { return Ok(None) };
    if e.ino == 0 {
        put_entry(b, e.off, ino, e.rec_len, ftype, name);
        return Ok(Some(e.off));
    }
    let used = rec_len(e.name_len);
    p16(b, e.off + 4, used as u16);
    put_entry(b, e.off + used, ino, e.rec_len - used, ftype, name);
    Ok(Some(e.off + used))
}

pub fn insert(b: &mut [u8], end: usize, name: &[u8], ino: u32, ftype: u8) -> Result<bool, i64> {
    Ok(insert_at(b, end, name, ino, ftype)?.is_some())
}

/// Removes the entry at `off` (it must be one of this block's).
pub fn remove(b: &mut [u8], end: usize, off: usize) -> Result<(), i64> {
    let mut prev: Option<Raw> = None;
    let mut found = None;
    for e in walk(b, end) {
        let e = e?;
        if e.off == off {
            found = Some(e);
            break;
        }
        prev = Some(e);
    }
    let e = found.ok_or(EUCLEAN)?;
    match prev {
        None => p32(b, off, 0),
        Some(p) => p16(b, p.off + 4, (p.rec_len + e.rec_len) as u16),
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
