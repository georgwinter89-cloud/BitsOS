//! ext2/ext3 block maps (`i_block[0..12]` direct, then single, double and triple indirect blocks;
//! linux-6.6/fs/ext4/indirect.c), read; a change of such an inode first turns it into an extent tree
//! (`State::migrate`, as `EXT4_IOC_MIGRATE` does: linux-6.6/fs/ext4/migrate.c), so V2 writes only extents.

use crate::inode::DiskInode;
use crate::le::*;
use crate::state::State;
use crate::tx::Tx;
use alloc::vec::Vec;
use crate::errno::*;

/// Every mapping of a block-mapped inode: runs (file block, disk block, length) in file order, and the
/// indirect blocks (freed after the migration). EUCLEAN for a pointer outside the volume.
pub fn collect(st: &State, tx: Option<&Tx>, di: &DiskInode) -> Result<(Vec<(u64, u64, u64)>, Vec<u64>), i64> {
    let per = (st.geo.bs / 4) as u64;
    let ib = di.i_block();
    let mut runs: Vec<(u64, u64, u64)> = Vec::new();
    let mut ind: Vec<u64> = Vec::new();
    let ok = |b: u64| b >= st.geo.first_data_block && b < st.geo.blocks;
    // A right map holds no more blocks than i_blocks says: a hostile one (an indirect block named many
    // times) ends here instead of walking 2^30 pointers.
    let budget = di.blocks512(&st.geo) / (st.geo.bs / 512);
    let mut seen = 0u64;
    let mut add = |runs: &mut Vec<(u64, u64, u64)>, l: u64, p: u64| -> Result<(), i64> {
        seen += 1;
        if !ok(p) || seen > budget {
            return Err(EUCLEAN);
        }
        match runs.last_mut() {
            Some(r) if r.0 + r.2 == l && r.1 + r.2 == p && r.2 < 32768 => r.2 += 1,
            _ => runs.push((l, p, 1)),
        }
        Ok(())
    };
    for i in 0..12u64 {
        let p = g32(ib, i as usize * 4) as u64;
        if p != 0 {
            add(&mut runs, i, p)?;
        }
    }
    // (block, level: 0 = its pointers are data blocks, first file block it covers)
    let mut first = 12u64;
    let mut span = 1u64;
    for level in 0..3usize {
        span *= per;
        let top = g32(ib, (12 + level) * 4) as u64;
        if top != 0 {
            let mut stack = alloc::vec![(top, level, first)];
            while let Some((blk, lv, base)) = stack.pop() {
                if !ok(blk) || (ind.len() as u64) >= budget {
                    return Err(EUCLEAN);
                }
                ind.push(blk);
                let b = st.read(tx, blk)?;
                let sub = per.pow(lv as u32);
                // Pushed in reverse so they come off the stack in file order.
                for k in (0..per).rev() {
                    let p = g32(&b, k as usize * 4) as u64;
                    if p == 0 {
                        continue;
                    }
                    if lv == 0 {
                        stack.push((p, usize::MAX, base + k));
                    } else {
                        stack.push((p, lv - 1, base + k * sub));
                    }
                }
                // Data blocks of this level-0 block: taken off the stack in order now.
                while let Some(&(p, usize::MAX, l)) = stack.last() {
                    stack.pop();
                    add(&mut runs, l, p)?;
                }
            }
        }
        first += span;
    }
    Ok((runs, ind))
}

/// The disk block of file block `l`, None for a hole.
pub fn map(st: &State, tx: Option<&Tx>, di: &DiskInode, l: u32) -> Result<Option<u64>, i64> {
    let per = (st.geo.bs / 4) as u64;
    let ib = di.i_block();
    let mut l = l as u64;
    if l < 12 {
        return Ok(nz(g32(ib, l as usize * 4)));
    }
    l -= 12;
    let mut span = 1u64;
    for level in 0..3usize {
        span *= per;
        if l < span {
            let mut blk = g32(ib, (12 + level) * 4) as u64;
            let mut s = span;
            for _ in 0..=level {
                if blk == 0 {
                    return Ok(None);
                }
                s /= per;
                let b = st.read(tx, blk)?;
                blk = g32(&b, ((l / s) % per) as usize * 4) as u64;
            }
            return Ok(nz(blk as u32));
        }
        l -= span;
    }
    Ok(None)
}

fn nz(b: u32) -> Option<u64> {
    if b == 0 { None } else { Some(b as u64) }
}
