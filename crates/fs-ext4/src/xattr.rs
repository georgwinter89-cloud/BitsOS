//! Extended attributes, read (linux-6.6/fs/ext4/xattr.h, xattr.c): in the inode after `i_extra_isize` and in
//! one shared block (`i_file_acl`, with its reference count). V2's VFS has no xattr calls yet; the volume
//! answers them for the syscall glue to come (change request in the FS-8a notes) and frees blocks right.

use crate::crc::crc32c;
use crate::inode::DiskInode;
use crate::le::*;
use crate::state::State;
use crate::tx::Tx;
use alloc::vec::Vec;
use crate::errno::*;

const MAGIC: u32 = 0xEA02_0000;
/// linux-6.6/fs/ext4/xattr.h:82: the consistency bound on a value size (bigger is corruption).
const SIZE_MAX: usize = 1 << 24;

fn prefix(index: u8) -> &'static [u8] {
    match index {
        1 => b"user.",
        2 => b"system.posix_acl_access",
        3 => b"system.posix_acl_default",
        4 => b"trusted.",
        6 => b"security.",
        7 => b"system.",
        8 => b"system.richacl",
        _ => b"",
    }
}

/// Entries from `b[start..]` until the 4 zero bytes; values at `base + e_value_offs`.
fn parse(st: &State, b: &[u8], start: usize, base: usize, out: &mut Vec<(Vec<u8>, Vec<u8>)>) -> Result<(), i64> {
    let mut o = start;
    loop {
        if o + 4 > b.len() {
            return Err(EUCLEAN);
        }
        if g32(b, o) == 0 {
            return Ok(());
        }
        let (nl, idx, voff, vino, vsize) = (g8(b, o) as usize, g8(b, o + 1), g16(b, o + 2) as usize, g32(b, o + 4), g32(b, o + 8) as usize);
        let name = b.get(o + 16..o + 16 + nl).ok_or(EUCLEAN)?;
        let mut full = prefix(idx).to_vec();
        full.extend_from_slice(name);
        if vsize > SIZE_MAX {
            klog::event!(Warn, "ext4", "xattr.too_big", ino = vino, bytes = vsize);
            return Err(EUCLEAN);
        }
        let value = if vino != 0 {
            let di = st.inode(None, vino)?;
            if di.flags() & crate::inode::FL_EA_INODE == 0 || di.size() != vsize as u64 {
                return Err(EUCLEAN);
            }
            let mut v = Vec::new();
            v.try_reserve_exact(vsize).map_err(|_| ENOMEM)?;
            v.resize(vsize, 0);
            st.read_file(&di, 0, &mut v)?;
            v
        } else {
            b.get(base + voff..base + voff + vsize).ok_or(EUCLEAN)?.to_vec()
        };
        out.push((full, value));
        o += (16 + nl + 3) & !3;
    }
}

impl State {
    /// Every xattr of the inode: (full name, value).
    pub fn xattrs(&self, di: &DiskInode) -> Result<Vec<(Vec<u8>, Vec<u8>)>, i64> {
        let mut out = Vec::new();
        let ex = di.extra_isize();
        let at = 128 + ex;
        if di.raw.len() > at + 4 && g32(&di.raw, at) == MAGIC {
            parse(self, &di.raw, at + 4, at + 4, &mut out)?;
        }
        let blk = di.file_acl();
        if blk != 0 {
            let b = self.read(None, blk)?;
            if g32(&b, 0) != MAGIC || g32(&b, 8) != 1 {
                return Err(EUCLEAN);
            }
            if !self.xattr_csum_ok(blk, &b) {
                klog::event!(Warn, "ext4", "xattr.badcrc", ino = di.ino, block = blk);
                return Err(EBADMSG);
            }
            parse(self, &b, 32, 0, &mut out)?;
        }
        Ok(out)
    }

    fn xattr_csum(&self, blk: u64, b: &[u8]) -> Option<u32> {
        let seed = self.geo.csum?;
        let mut c = crc32c(seed, &blk.to_le_bytes());
        c = crc32c(c, &b[..0x10]);
        c = crc32c(c, &[0; 4]);
        Some(crc32c(c, &b[0x14..]))
    }
    fn xattr_csum_ok(&self, blk: u64, b: &[u8]) -> bool {
        self.xattr_csum(blk, b).is_none_or(|c| c == g32(b, 0x10))
    }

    /// The inode lets go of its xattr block (`ext4_xattr_release_block`): one reference fewer, the block
    /// freed with the last. Returns the block to free, if any.
    pub fn release_xattr_block(&mut self, tx: &mut Tx, di: &mut DiskInode) -> Result<Option<u64>, i64> {
        let blk = di.file_acl();
        if blk == 0 {
            return Ok(None);
        }
        di.set_file_acl(0);
        let mut b = self.read(Some(tx), blk)?;
        if g32(&b, 0) != MAGIC {
            klog::event!(Warn, "ext4", "xattr.bad_block", ino = di.ino, block = blk);
            return Ok(None);
        }
        let refs = g32(&b, 4);
        if refs <= 1 {
            return Ok(Some(blk));
        }
        p32(&mut b, 4, refs - 1);
        if let Some(c) = self.xattr_csum(blk, &b) {
            p32(&mut b, 0x10, c);
        }
        tx.put(blk, b);
        Ok(None)
    }
}
