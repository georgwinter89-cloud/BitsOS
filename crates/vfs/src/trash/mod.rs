//! EINS-C (K6, F4): the trash of the assistant's process tree. Inside the tree (`Subject::tree`) an unlink, an
//! rmdir or a rename over an existing name on a persistent file system (ext4, vfat) does not destroy: the victim
//! moves, in the same step and on the same file system, to `<fs>/.papierkorb/<id>/files/<its path in the fs>`,
//! and the call answers as Linux would. Outside the tree nothing here runs (one load of `Subject::tree`).
//!
//! The rules (plan/spec/assistent-eins/eins-c.md says why):
//! - Not diverted: tmpfs, proc, sysfs, devpts (no `FsOps`), the trash itself (the tree may not change it at all,
//!   `guard`), a name of a file with more names (its data stays under them), and the tree's own files: made by a
//!   process of this tree and opened by nobody outside it since (`Inode::born_tree`), so a program's temp files
//!   never fill the trash.
//! - One id per command: what one process of the tree removes on one file system with less than `GAP_NS`
//!   between two removals (`rm -r`, `find -delete`) is one entry; its record names the common top.
//! - rmdir: an empty directory goes to the trash too (it may be part of a tree being removed); once its contents
//!   are in an open entry, it is only removed (the entry holds it already, made with its mode and owner).
//! - The trash grows until Georg empties it; with less than 1/`RESERVE_DIV` of the file system free a removal
//!   is refused with ENOSPC instead of destroying.
//! - Truncation and writing over a file in place are not covered (stage 2's snapshot is).

mod batch;
pub mod guard;
mod node;
mod place;
mod record;
mod restore;

pub use batch::end_batches_of;
pub use node::{command, set_caller, CONTROL};
pub use restore::{empty, restore, Caller, Restored};

use crate::dentry::{Dentry, SuperBlock};
use crate::mount_ns::Path;
use crate::ops::Ctx;
use crate::Errno;
use alloc::string::String;
use alloc::sync::Arc;

/// The trash directory at the top of each persistent file system.
pub const DIR: &[u8] = b".papierkorb";
/// Inside an entry: the victims at their paths below the file system's top.
pub const FILES: &[u8] = b"files";
/// Inside an entry: what it holds and who removed it (`record`).
pub const RECORD: &[u8] = b"record.json";
/// Removals of one process further apart than this start a new entry.
pub const GAP_NS: u64 = 2_000_000_000;
/// Below 1/50 of the file system free a removal is refused (ENOSPC).
pub const RESERVE_DIV: u64 = 50;

klog::counter!(DIVERTED, "vfs.trash.diverted");
klog::counter!(OWN_REMOVED, "vfs.trash.own_removed");
klog::counter!(FULL, "vfs.trash.full");

/// A file system whose removals the tree's trash keeps.
pub fn persistent(sb: &SuperBlock) -> bool {
    sb.ops.is_some() && matches!(sb.fstype, "ext4" | "vfat")
}

/// What a process of a tree is, for an entry's record: its command name and its chat label.
pub type Teller = fn(u32) -> Option<(String, Option<String>)>;
static TELLER: ksync::Once<Teller> = ksync::Once::new();

/// vfs-sys installs the process layer's answer.
pub fn set_teller(f: Teller) {
    TELLER.get_or_init(|| f);
}

pub(crate) fn tell(pid: u32) -> (String, Option<String>) {
    TELLER.get().and_then(|f| f(pid)).unwrap_or_else(|| (String::from("?"), None))
}

/// Whether a removal by `c` below `dir` falls under the trash.
#[inline]
pub(crate) fn applies(c: &Ctx, dir: &Path) -> bool {
    c.subject.tree != 0 && persistent(&dir.dentry.sb)
}

/// The tree's own file: removed for real (`Inode::born_tree`), or a name of a file that keeps another one.
pub(crate) fn removed_for_real(c: &Ctx, victim: &Dentry) -> bool {
    let own = victim.inode.born_tree.load(core::sync::atomic::Ordering::Acquire) == c.subject.tree;
    if own {
        OWN_REMOVED.inc();
    }
    own || (!victim.is_dir() && victim.inode.meta.nlink() > 1)
}

/// A process outside the tree that made it opened `d`: from now on it is Georg's (`Inode::born_tree`).
#[inline]
pub(crate) fn seen(c: &Ctx, d: &Dentry) {
    let b = &d.inode.born_tree;
    if b.load(core::sync::atomic::Ordering::Relaxed) != 0 && b.load(core::sync::atomic::Ordering::Acquire) != c.subject.tree && c.subject != policy::Subject::KERNEL {
        b.store(0, core::sync::atomic::Ordering::Release);
    }
}

/// A node `c` just made: the tree's own until someone outside opens it.
#[inline]
pub(crate) fn born(c: &Ctx, d: &Dentry) {
    if c.subject.tree != 0 {
        d.inode.born_tree.store(c.subject.tree, core::sync::atomic::Ordering::Release);
    }
}

/// The free share check before a removal goes to the trash (under the caller's names lock, before `OPEN`).
pub(crate) fn room(c: &Ctx, dir: &Path) -> Result<(), Errno> {
    let Some(o) = &dir.dentry.sb.ops else { return Ok(()) };
    let st = o.statfs()?;
    if st.blocks > 0 && st.bavail.saturating_mul(RESERVE_DIV) < st.blocks {
        FULL.inc();
        klog::event!(Warn, "vfs", "trash.full", pid = c.subject.pid, tree = c.subject.tree, fs = dir.dentry.sb.fstype, free = st.bavail, blocks = st.blocks; "the file system is nearly full: the removal is refused, nothing was deleted (empty the trash)");
        return Err(uapi::errno::ENOSPC);
    }
    Ok(())
}

/// Moves `victim` (a child of `dir`) into the trash. The caller holds `write_begin` and the names lock and
/// made every check of the call; afterwards it sends the notifications and writes the record (`Moved::finish`,
/// outside the lock).
pub(crate) fn divert(c: &Ctx, dir: &Path, victim: &Arc<Dentry>, what: record::What) -> Result<Moved, Errno> {
    // A failed shift moved nothing, so nothing is left in the trash.
    let m = place::move_in(c, dir, victim, what, &mut |to, name| place::shift(&dir.dentry, victim, to, name), &mut None)?;
    DIVERTED.inc();
    Ok(m)
}

/// A rename of `old` (in `odir`) over `victim` (in `ndir`) that the tree's trash keeps: the victim goes to the
/// trash and `old` takes its name, as `divert` is called. Where the file system swaps two names (ext4) they are
/// swapped first and the victim then leaves from the old name, so the target name is never missing; elsewhere
/// (vfat) the victim moves first and comes back when the rename fails. On an error both names are as before, or,
/// when the way back is refused too, the victim is in the trash with its record (the error's Moved to finish).
pub(crate) fn replace(c: &Ctx, odir: &Path, old: &Arc<Dentry>, ndir: &Path, victim: &Arc<Dentry>) -> Result<Moved, (Errno, Option<Moved>)> {
    let o = ndir.dentry.sb.ops.clone().ok_or((uapi::errno::EINVAL, None))?;
    let old_name = old.name();
    let mut left = None;
    let r = place::move_in(c, ndir, victim, record::What::Rename, &mut |to, name| match o.exchange(old, victim) {
        Ok(()) => {
            Dentry::exchange(&odir.dentry, &old_name, &ndir.dentry, name)?;
            place::shift(&odir.dentry, victim, to, name).inspect_err(|_| {
                let undo = o.exchange(victim, old).and_then(|_| Dentry::exchange(&odir.dentry, &old_name, &ndir.dentry, name));
                undone(undo);
            })
        }
        Err(uapi::errno::EINVAL) => {
            place::shift(&ndir.dentry, victim, to, name)?;
            if let Err(e) = o.rename(&odir.dentry, old, &ndir.dentry, name, None) {
                undone(place::shift(to, victim, &ndir.dentry, name));
                return Err(e);
            }
            let moved = odir.dentry.detach(&old_name).ok_or(uapi::errno::ENOENT)?;
            ndir.dentry.reattach(&moved, name)?;
            if moved.is_dir() && !Arc::ptr_eq(&odir.dentry, &ndir.dentry) {
                odir.dentry.inode.meta.dec_nlink();
                ndir.dentry.inode.meta.inc_nlink();
            }
            Ok(())
        }
        Err(e) => Err(e),
    }, &mut left);
    let m = r.map_err(|e| (e, left))?;
    DIVERTED.inc();
    Ok(m)
}

fn undone(r: Result<(), Errno>) {
    if let Err(e) = r {
        klog::event!(Error, "vfs", "trash.undo_failed", errno = e; "undoing a failed rename failed: the replaced file stays in the trash");
    }
}

/// An rmdir of the empty `victim` in the tree: whether the open entry of this process holds its contents
/// already (then it is only removed, and `took` follows under the same names lock: the entry has the directory).
pub(crate) fn holds(c: &Ctx, dir: &Path, victim: &Arc<Dentry>) -> bool {
    place::holds(c, dir, victim)
}

pub(crate) fn took(c: &Ctx, dir: &Path, name: &[u8]) -> Option<Moved> {
    place::took(c, dir, name)
}

pub(crate) use place::Moved;
pub(crate) use record::What;

#[cfg(test)]
mod tests {
    use crate::inode::{Data, Inode, Meta};
    use alloc::sync::Arc;
    use core::sync::atomic::{AtomicBool, AtomicI64};

    /// The inode's fields as they were before `born_tree`.
    type Before = (Meta, Data, AtomicBool, AtomicBool, Option<crate::pool::MetaCharge>, Option<Arc<dyn crate::synth::MagicLink>>, AtomicI64, crate::inotify::Marks);

    /// The tree's mark sits in the padding after the two flags: an inode costs no byte more (memory rule).
    #[test]
    fn the_mark_costs_no_memory() {
        assert_eq!(core::mem::size_of::<Inode>(), core::mem::size_of::<Before>());
    }
}
