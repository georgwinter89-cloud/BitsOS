//! Inline data, read (linux-6.6/fs/ext4/inline.c): the first 60 bytes in `i_block`, the rest in the
//! `system.data` xattr inside the inode; a directory keeps its parent in the first 4 bytes and its entries
//! after, without `.` and `..`. Volumes with inline_data mount read-only in FS-8a.

use crate::dir;
use crate::inode::DiskInode;
use crate::state::State;
use alloc::vec::Vec;
use crate::errno::*;

fn content(st: &State, di: &DiskInode) -> Result<(Vec<u8>, Vec<u8>), i64> {
    let head = di.i_block().to_vec();
    let rest = st.xattrs(di)?.into_iter().find(|(n, _)| n == b"system.data").map(|(_, v)| v).unwrap_or_default();
    Ok((head, rest))
}

pub fn read(st: &State, di: &DiskInode, off: u64, buf: &mut [u8]) -> Result<usize, i64> {
    let (mut all, rest) = content(st, di)?;
    all.extend_from_slice(&rest);
    let size = di.size().min(all.len() as u64);
    if off >= size {
        return Ok(0);
    }
    let n = buf.len().min((size - off) as usize);
    buf[..n].copy_from_slice(&all[off as usize..off as usize + n]);
    Ok(n)
}

pub fn list(st: &State, di: &DiskInode) -> Result<Vec<(Vec<u8>, u32, u8)>, i64> {
    let (head, rest) = content(st, di)?;
    let mut out = Vec::new();
    for (b, from) in [(&head, 4usize), (&rest, 0)] {
        let area = b.get(from..).ok_or(EUCLEAN)?;
        if area.is_empty() {
            continue;
        }
        for e in dir::entries(area, area.len())? {
            if e.ino != 0 {
                out.push((e.name, e.ino, e.ftype));
            }
        }
    }
    Ok(out)
}
