//! BIGDIR: what the tests (host and kernel) ask of the directory indexes and switch in them: the index against a
//! fresh pass over the blocks, the costs per change, the index switched off or refused its memory.

use crate::dirx::DirIndex;
use crate::state::State;
use crate::vol::Volume;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

impl State {
    /// The index of directory `ino` against a fresh pass over its blocks. Ok(None): it has no index.
    pub fn dirx_check(&self, ino: u32) -> Result<Option<usize>, String> {
        let Some(ix) = self.dirx.map.get(&ino) else { return Ok(None) };
        let di = self.inode(None, ino).map_err(|e| format!("inode {ino}: {e}"))?;
        let fresh = self.dirx_build(None, &di, false).map_err(|e| format!("pass over {ino}: {e}"))?.ok_or("no memory")?;
        if ix.rooms() != fresh.rooms() {
            let at = ix.rooms().iter().zip(fresh.rooms()).position(|(a, b)| a != b);
            return Err(format!("directory {ino}: room differs ({} blocks in the index, {} on the disk, first at {at:?})", ix.blocks(), fresh.blocks()));
        }
        let (a, b): (Vec<_>, Vec<_>) = (ix.places().collect(), fresh.places().collect());
        if a != b {
            let first = a.iter().zip(&b).find(|(x, y)| x != y);
            return Err(format!("directory {ino}: {} names in the index, {} on the disk, first difference {first:?}", a.len(), b.len()));
        }
        if let Some(have) = self.dirx.stats.get(ino) {
            let want = (di.size(), di.blocks512(&self.geo));
            if have != want {
                return Err(format!("directory {ino}: fstat would say {have:?}, the inode says {want:?}"));
            }
        }
        Ok(Some(a.len()))
    }
}

impl Volume {
    /// Only the bits of `mask` count in the names' hashes from now on (the indexes are built anew).
    pub fn dirx_hash_mask(&self, mask: u32) {
        let mut st = self.lock();
        st.dirx.hash_mask = mask;
        st.dirx.map.clear();
    }

    /// The pool refuses the indexes' charges from now on (or no longer).
    pub fn dirx_refuse(&self, on: bool) {
        self.lock().dirx.refuse_above = on.then_some(0);
    }

    /// Switches the index off (every change walks the directory as before BIGDIR) or on again.
    pub fn dirx_off(&self, off: bool) {
        let mut st = self.lock();
        st.dirx.off = off;
        if off {
            st.dirx.map.clear();
        }
    }

    /// (directory blocks read, written, full walks) by name changes so far.
    pub fn dirx_counts(&self) -> (u64, u64, u64) {
        let st = self.lock();
        (st.dirx.block_reads.get(), st.dirx.block_writes.get(), st.dirx.scans.get())
    }

    /// (indexes of this volume found stale so far, whether directory `ino` is a suspect now: found stale and
    /// not built again).
    pub fn dirx_stales(&self, ino: u32) -> (u64, bool) {
        let st = self.lock();
        (st.dirx.stales, st.dirx.suspect(ino))
    }

    /// `State::dirx_check`.
    pub fn dirx_check(&self, ino: u32) -> Result<Option<usize>, String> {
        self.lock().dirx_check(ino)
    }

    /// Bytes the indexes hold now, and how many there are.
    pub fn dirx_bytes(&self) -> (u64, usize) {
        let st = self.lock();
        (st.dirx.map.values().map(DirIndex::bytes).sum(), st.dirx.map.len())
    }

    /// Host tests: makes the index of directory `ino` wrong (`dirx::Corrupt`), as no code path does. False: the
    /// directory has no index, or nothing in it could be made wrong that way.
    #[cfg(test)]
    pub fn dirx_corrupt(&self, ino: u32, how: impl FnOnce(&crate::vol_dirx::Dirs) -> crate::dirx::Corrupt) -> bool {
        let mut st = self.lock();
        let how = how(&st.dirx);
        st.dirx.map.get_mut(&ino).is_some_and(|ix| ix.corrupt(how))
    }
}
