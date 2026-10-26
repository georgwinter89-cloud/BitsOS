//! Directory changes: free slots for an entry set, writing and clearing sets in the crash-safe sector order,
//! growing a directory by a zeroed cluster.
//!
//! - A new set goes into one sector whenever it fits (V1: "an entry set never crosses a sector"), so adding a
//!   name is one sector write; free slots after the end mark count, and the slots between the mark and such a
//!   set become deleted ones first. A set placed over the end mark keeps the slot after it an end mark (no
//!   garbage behind an old end mark comes back, V1 rule).
//! - A set that cannot fit one sector (a name over 195 characters with 512-byte sectors) is written first
//!   sector first and cleared last sector first: a cut between leaves its first LFN parts without the rest,
//!   which fsck.fat deletes by itself ("orphaned long file name part"); never a short entry with half a name.

use crate::dirent::{Entry, DELETED, ENTRY};
use crate::name::{self, Short};
use crate::vol::{State, Volume, MAX_DIR_BYTES};
use alloc::collections::BTreeSet;
use alloc::vec;
use alloc::vec::Vec;
use uapi::errno::*;

fn end_of(raw: &[u8]) -> usize {
    raw.chunks_exact(ENTRY).position(|e| e[0] == 0).unwrap_or(raw.len() / ENTRY)
}

/// The first run of `n` free slots (deleted ones, or any at or after the end mark), inside one sector when it
/// fits there. A run after the end mark turns the slots between the mark and the run into deleted ones
/// (`add_set`), because readers stop at the mark (Windows) or read on (Linux, fsck.fat).
pub fn find_run(raw: &[u8], n: usize, per_sector: usize) -> Option<usize> {
    let total = raw.len() / ENTRY;
    let end = end_of(raw);
    let free = |i: usize| i >= end || raw[i * ENTRY] == DELETED;
    if n == 0 || n > total {
        return None;
    }
    for whole in [true, false] {
        if whole && n > per_sector {
            continue;
        }
        let mut s = 0;
        while s + n <= total {
            if whole && s / per_sector != (s + n - 1) / per_sector {
                s += 1;
                continue;
            }
            match (s..s + n).find(|&i| !free(i)) {
                None => return Some(s),
                Some(bad) => s = bad + 1,
            }
        }
    }
    None
}

impl Volume {
    fn per_sector(&self) -> usize {
        self.geo.bps / ENTRY
    }

    /// Writes the sectors of `slots` of directory `dir` from its cached raw entries, in the given order.
    fn write_dir_sectors(&self, st: &State, dir: u64, slots_in_order: &[u32]) -> Result<(), i64> {
        let per = self.per_sector() as u32;
        let mut done: Vec<u32> = Vec::new();
        let raw = Self::node(st, dir)?.raw.as_ref().ok_or(EIO)?;
        for &s in slots_in_order {
            let sec = s / per;
            if done.contains(&sec) {
                continue;
            }
            done.push(sec);
            let off = sec as usize * self.geo.bps;
            let bytes = raw.get(off..off + self.geo.bps).ok_or(EIO)?;
            self.write(self.slot_sector(st, dir, s)?, bytes)?;
        }
        Ok(())
    }

    /// Grows directory `dir` by one zeroed cluster: zeros, then the new FAT entry, then the link.
    fn grow_dir(&self, st: &mut State, dir: u64) -> Result<(), i64> {
        let cb = self.geo.cluster_bytes();
        let len = self.dir_raw(st, dir)?.len();
        if len + cb > MAX_DIR_BYTES {
            return Err(ENOSPC);
        }
        self.begin(st)?;
        let c = self.allocate(st, None, 1)?[0];
        self.write(self.geo.cluster_sector(c), &vec![0u8; cb])?;
        st.fat.link(&*self.dev, &[c])?;
        st.fat.write_out(&*self.dev)?;
        let last = *self.chain(st, dir)?.last().ok_or(EIO)?;
        st.fat.set(&*self.dev, last, c)?;
        st.fat.write_out(&*self.dev)?;
        let n = Self::node_mut(st, dir)?;
        n.chain.as_mut().ok_or(EIO)?.push(c);
        n.raw.as_mut().ok_or(EIO)?.resize(len + cb, 0);
        klog::event!(Debug, "fat", "dir.grown", dir = dir, clusters = (len + cb) / cb);
        Ok(())
    }

    /// The names used in a directory, read once per change: every short name of an entry, and every shown
    /// name (folded). An 8.3 name is taken when either holds it (sets: an alias search may try many names in a
    /// directory of 65536 entries).
    fn names_used(raw: &[u8]) -> (BTreeSet<Short>, BTreeSet<Vec<u8>>) {
        let l = crate::listing::parse(raw);
        let shorts = raw
            .chunks_exact(ENTRY)
            .take(l.end as usize)
            .filter(|e| e[0] != DELETED && e[11] & 0x3F != 0x0F)
            .map(|e| {
                let mut s = [0u8; 11];
                s.copy_from_slice(&e[..11]);
                s
            })
            .collect();
        let longs = l.items.iter().map(|i| name::fold(&i.name)).collect();
        (shorts, longs)
    }

    /// The entry set for `name` with the fields of `tmpl` (its name replaced): LFN entries (if the name needs
    /// them) and the short entry last. EEXIST when the name is taken (case-insensitive) in `dir`.
    pub(crate) fn entry_set(&self, st: &mut State, dir: u64, nm: &[u8], tmpl: &Entry) -> Result<(Vec<[u8; ENTRY]>, Short), i64> {
        let u = name::to_utf16(nm)?;
        let raw = self.dir_raw(st, dir)?;
        let (shorts, longs) = Self::names_used(raw);
        if longs.contains(&name::fold(nm)) {
            return Err(EEXIST);
        }
        let taken = |s: &Short| shorts.contains(s) || longs.contains(&name::short_key(s));
        let plain = name::plain_short(name::strip_dots(nm)).filter(|s| !taken(s));
        let (short, lfn) = match plain {
            Some(s) => (s, Vec::new()),
            None => {
                let s = name::make_alias(&u, &taken)?;
                (s, crate::dirent::lfn_entries(&u, name::checksum(&s)))
            }
        };
        let mut e = *tmpl;
        e.name = short;
        e.ntres = 0;
        let mut set = lfn;
        set.push(e.bytes());
        Ok((set, short))
    }

    /// Grows directory `dir` until a set of `n` entries fits: called before a change that cannot be undone
    /// (a rename's old entry cleared, a new directory's cluster taken), so a full volume answers ENOSPC
    /// before it, never in the middle.
    pub(crate) fn room_for(&self, st: &mut State, dir: u64, n: usize) -> Result<usize, i64> {
        let per = self.per_sector();
        loop {
            let raw = self.dir_raw(st, dir)?;
            if let Some(s) = find_run(raw, n, per) {
                return Ok(s);
            }
            self.grow_dir(st, dir)?;
        }
    }

    /// Puts an entry set into directory `dir` (growing it when needed). Returns the slot of the short entry.
    pub(crate) fn add_set(&self, st: &mut State, dir: u64, set: &[[u8; ENTRY]]) -> Result<u32, i64> {
        let s = self.room_for(st, dir, set.len())?;
        self.begin(st)?;
        let raw = self.dir_raw(st, dir)?;
        let end = end_of(raw);
        let total = raw.len() / ENTRY;
        let after = s + set.len();
        let mut order: Vec<u32> = Vec::new();
        // Slots between the end mark and a run placed after it become deleted ones (written first), so the
        // set is never hidden behind an end mark.
        for i in end..s {
            raw[i * ENTRY] = DELETED;
            order.push(i as u32);
        }
        // The slot after a set placed over the end mark must be an end mark itself (written before the set).
        if after > end && after < total && raw[after * ENTRY] != 0 {
            raw[after * ENTRY..(after + 1) * ENTRY].fill(0);
            order.push(after as u32);
        }
        for (i, e) in set.iter().enumerate() {
            raw[(s + i) * ENTRY..(s + i + 1) * ENTRY].copy_from_slice(e);
        }
        // The set's sectors in order: a set inside one sector is one write; a longer one (a name over 195
        // characters) has its first part written first, the short entry last.
        order.extend((s..after).map(|i| i as u32));
        self.write_dir_sectors(st, dir, &order)?;
        Ok((after - 1) as u32)
    }

    /// Marks the set ending at `slot` (with `lfn` LFN entries before it) deleted: LFN sectors first.
    pub(crate) fn clear_set(&self, st: &mut State, dir: u64, slot: u32, lfn: u32) -> Result<(), i64> {
        self.begin(st)?;
        let first = slot.checked_sub(lfn).ok_or(EIO)?;
        let raw = self.dir_raw(st, dir)?;
        for i in first..=slot {
            *raw.get_mut(i as usize * ENTRY).ok_or(EIO)? = DELETED;
        }
        // Last sector first: a cut between the sectors of a long set leaves its first parts without the rest,
        // as a cut while adding it would.
        let order: Vec<u32> = (first..=slot).rev().collect();
        self.write_dir_sectors(st, dir, &order)
    }

    /// Writes node `ino`'s short entry (size, first cluster, times, attributes) at its slot: one sector.
    pub(crate) fn write_entry(&self, st: &mut State, ino: u64) -> Result<(), i64> {
        let (parent, slot, bytes) = {
            let n = Self::node(st, ino)?;
            if !n.linked {
                return Ok(());
            }
            (n.parent, n.slot, n.e.bytes())
        };
        self.begin(st)?;
        let raw = self.dir_raw(st, parent)?;
        raw.get_mut(slot as usize * ENTRY..(slot as usize + 1) * ENTRY).ok_or(EIO)?.copy_from_slice(&bytes);
        self.write_dir_sectors(st, parent, &[slot])?;
        Self::node_mut(st, ino)?.entry_dirty = false;
        Ok(())
    }

    /// Whether directory `ino` holds nothing but "." and ".." on disk.
    pub(crate) fn dir_empty(&self, st: &mut State, ino: u64) -> Result<bool, i64> {
        let raw = self.dir_raw(st, ino)?;
        Ok(crate::listing::parse(raw).items.is_empty())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(marks: &[u8]) -> Vec<u8> {
        let mut r = vec![0u8; marks.len() * ENTRY];
        for (i, &m) in marks.iter().enumerate() {
            r[i * ENTRY] = m;
        }
        r
    }

    #[test]
    fn runs() {
        // 4 slots per "sector" here.
        let r = raw(&[b'A', 0xE5, 0xE5, b'B', 0xE5, 0xE5, 0xE5, b'C', 0, 0, 0, 0]);
        assert_eq!(find_run(&r, 2, 4), Some(1));
        assert_eq!(find_run(&r, 3, 4), Some(4));
        assert_eq!(find_run(&r, 4, 4), Some(8));
        assert_eq!(find_run(&r, 5, 4), None, "no run of 5 before the end");
        let r2 = raw(&[b'A', b'B', 0xE5, 0xE5, 0xE5, 0, 0, 0]);
        assert_eq!(find_run(&r2, 3, 4), Some(4), "inside one sector first");
        assert_eq!(find_run(&r2, 6, 4), Some(2), "across sectors, over the end mark");
        assert_eq!(find_run(&r2, 7, 4), None);
        let r5 = raw(&[b'A', b'B', 0, 0, 0, 0, 0, 0]);
        assert_eq!(find_run(&r5, 3, 4), Some(4), "after the end mark, inside one sector");
        let r3 = raw(&[b'A', 0xE5, 0xE5, 0xE5, 0xE5, 0xE5, b'B', 0]);
        assert_eq!(find_run(&r3, 5, 4), Some(1), "a long set may cross a sector");
        // Garbage behind an end mark counts as free.
        let r4 = raw(&[b'A', 0, b'G', b'G']);
        assert_eq!(find_run(&r4, 2, 4), Some(1));
        assert_eq!(find_run(&r4, 3, 4), Some(1));
        let full = raw(&[b'A', b'B', b'C', b'D']);
        assert_eq!(find_run(&full, 1, 4), None);
    }
}
