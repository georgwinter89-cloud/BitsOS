//! CRASHDISK: proof that the crash area's clusters belong to its file alone. The panic writes them without the
//! file system, so a cross-linked FAT (another chain or another directory entry reaching into them, e.g. after a
//! damaged or foreign-edited ESP) would let the panic overwrite another file: the boot check refuses then. Linux
//! 7.3.1 has no such check to copy (pstore/blk owns a whole partition or region); this is fsck.fat's cross-link
//! rule limited to one chain.

use crate::dirent::{ATTR_LFN, ATTR_VOLUME, DELETED, ENTRY, Entry};
use crate::vol::{MAX_DIR_BYTES, State, Volume};
use alloc::collections::BTreeSet;
use alloc::vec;
use alloc::vec::Vec;
use uapi::errno::*;

/// Directories walked at most (an ESP holds a few dozen; more is a damaged or looping tree).
const MAX_DIRS: usize = 65_536;

impl Volume {
    /// Links into the chain of file `ino` from anywhere but the chain itself and the file's own directory entry:
    /// FAT entries of other clusters pointing into it, other directory entries (also folders, also the root)
    /// starting in it. 0: the clusters are the file's alone. The whole FAT and every directory are read once.
    pub fn foreign_links(&self, ino: u64) -> Result<u64, i64> {
        let mut st = self.lock()?;
        let chain: BTreeSet<u32> = self.chain(&mut st, ino)?.iter().copied().collect();
        let first = Self::node(&st, ino)?.e.first;
        let mut foreign = 0u64;
        st.fat.scan(&*self.dev, |c, v| foreign += (!chain.contains(&c) && chain.contains(&v)) as u64)?;
        let starts = self.dir_starts(&mut st, &chain)?;
        // The file's own entry is one of the starts at its first cluster; every other start is foreign.
        let own = starts.iter().filter(|&&c| c == first).count();
        if own == 0 {
            return Err(EIO);
        }
        Ok(foreign + starts.len() as u64 - 1)
    }

    /// The first clusters of every live directory entry (and the root) that lie in `chain`, walking the tree on
    /// the disk from the root.
    fn dir_starts(&self, st: &mut State, chain: &BTreeSet<u32>) -> Result<Vec<u32>, i64> {
        let g = &self.geo;
        let cb = g.cluster_bytes();
        let mut hits = Vec::new();
        if chain.contains(&g.root) {
            hits.push(g.root);
        }
        let mut seen = BTreeSet::from([g.root]);
        let mut todo = vec![g.root];
        let mut buf = vec![0u8; cb];
        while let Some(dir) = todo.pop() {
            let clusters = st.fat.chain(&*self.dev, dir)?;
            if clusters.len() * cb > MAX_DIR_BYTES {
                return Err(EIO);
            }
            'dir: for c in clusters {
                self.dev.read(g.cluster_sector(c), &mut buf)?;
                for e in buf.chunks_exact(ENTRY) {
                    match e[0] {
                        0 => break 'dir,
                        DELETED | b'.' => continue,
                        _ => {}
                    }
                    if e[11] == ATTR_LFN || (e[11] & ATTR_VOLUME != 0 && e[11] & crate::dirent::ATTR_DIR == 0) {
                        continue;
                    }
                    let d = Entry::parse(e);
                    if d.first == 0 {
                        continue;
                    }
                    if chain.contains(&d.first) {
                        hits.push(d.first);
                    }
                    if d.is_dir() && g.valid_cluster(d.first) && seen.insert(d.first) {
                        if seen.len() > MAX_DIRS {
                            return Err(EIO);
                        }
                        todo.push(d.first);
                    }
                }
            }
        }
        Ok(hits)
    }
}
