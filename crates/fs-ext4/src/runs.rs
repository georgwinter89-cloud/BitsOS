//! Which blocks of a write the disk maps already (`State::reserve_pages`): one extent lookup per run of mapped
//! blocks or of a hole, not one per block. Every lookup copies the tree's leaf block out of the disk's cache; per
//! block that was about a megabyte of metadata copied per megabyte written once a file's extents left its inode.

use crate::extent::{self, Found};
use crate::inode::DiskInode;
use crate::state::State;

klog::counter!(pub LOOKUPS, "ext4.write.map_lookups");

/// The last answer of the tree: the blocks [lo, hi) are all mapped (`mapped`) or all a hole. Valid only while the
/// tree does not change: one call under the volume lock.
#[derive(Default)]
pub struct Runs {
    lo: u64,
    hi: u64,
    mapped: bool,
}

impl Runs {
    /// Whether file block `l` is mapped (an unwritten extent counts: its blocks are allocated).
    pub fn mapped(&mut self, st: &State, di: &DiskInode, l: u32) -> Result<bool, i64> {
        let at = l as u64;
        if at >= self.lo && at < self.hi {
            return Ok(self.mapped);
        }
        LOOKUPS.inc();
        st.map_lookups.set(st.map_lookups.get() + 1);
        let (lo, hi, mapped) = match extent::lookup(st, None, di, l)? {
            Found::Ext(x) => (x.lblk as u64, x.lblk as u64 + x.len as u64, true),
            // A hole reaches up to the next mapped block the tree names (an index entry's start is a lower bound).
            Found::Hole(next) => (at, next.map_or(1 << 32, u64::from), false),
        };
        // A damaged tree may answer a run that does not hold `l`: answer it, remember nothing.
        (self.lo, self.hi) = if lo <= at && at < hi { (lo, hi) } else { (0, 0) };
        self.mapped = mapped;
        Ok(mapped)
    }
}
