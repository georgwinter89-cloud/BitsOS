//! BIGDIR: getdents64 over a directory's children (Steam on the Mini-PC, 12.11.2026: 15700 getdents64 a second
//! in folders of 50000 files at about 1 us per entry: per entry a reference taken, the child's
//! `vfs.dentry.link` lock and a heap block for its name). A listing now copies the records straight from the
//! directory's listing entries (`Listed`) into the caller's buffer under one hold of `vfs.dentry.dir` per
//! `HOLD_BYTES` of records: no lock and no heap block per entry.
//!
//! Beside create, remove and rename: a position (`d_off`) names one attachment of one name. A removed child's
//! position is gone; a renamed child has a new position at the end, and its old position never shows the new
//! name. So a child renamed during a listing shows under the name its position had when the listing came by:
//! the old name (the listing was there before the rename), the new one, both (it passed the old place before
//! and the new one after) or neither; a name that stays shows exactly once, at a position that never changes.
//! That is what the per-child check of the position did before (`Dentry::name_at`), and what Linux lists.

use super::Dentry;
use crate::abi;
use crate::Errno;
use alloc::sync::Arc;
use alloc::vec::Vec;
use uapi::errno::*;

/// Records copied per hold of the directory's lock (about 1000 entries): lookups in the same directory wait
/// that long at most.
const HOLD_BYTES: usize = 64 << 10;

impl Dentry {
    /// Appends `dirent64` records of the children from listing number `*from` on to `out`, while `out` stays
    /// within `max` bytes; `*from` moves behind the last child added. True when no child is left, false when the
    /// next record does not fit (EINVAL when `out` is empty: not even one record fits).
    ///
    /// The room in `out` is made with the directory's lock dropped (`children_from` says why), a batch at a
    /// time by the bytes a listing of the whole directory takes: a small directory asks for little.
    pub(crate) fn list_into(self: &Arc<Self>, from: &mut u64, out: &mut Vec<u8>, max: usize) -> Result<bool, Errno> {
        let mut need = 0;
        loop {
            let all = self.dir.lock().listed_bytes;
            let room = max.saturating_sub(out.len()).min(HOLD_BYTES).min(usize::try_from(all).unwrap_or(usize::MAX)).max(need);
            if out.capacity() - out.len() < room {
                out.try_reserve(room).map_err(|_| ENOMEM)?;
            }
            let m = self.dir.lock();
            let mut copied = 0;
            need = 0;
            for (seq, e) in m.by_seq.range(*from..) {
                let len = abi::dirent64_len(&e.name);
                if out.len() + len > max {
                    return if out.is_empty() { Err(EINVAL) } else { Ok(false) };
                }
                if out.len() + len > out.capacity() {
                    need = len;
                    break;
                }
                if copied >= HOLD_BYTES {
                    break;
                }
                // `abi::dirent64`'s record, written as one piece (its zero padding first).
                let at = out.len();
                out.resize(at + len, 0);
                let r = &mut out[at..];
                r[..8].copy_from_slice(&e.ino.to_le_bytes());
                r[8..16].copy_from_slice(&(seq + 2).to_le_bytes());
                r[16..18].copy_from_slice(&(len as u16).to_le_bytes());
                r[18] = e.dt;
                r[19..19 + e.name.len()].copy_from_slice(&e.name);
                *from = seq + 1;
                copied += len;
            }
            if need == 0 && copied < HOLD_BYTES {
                return Ok(true);
            }
        }
    }
}
