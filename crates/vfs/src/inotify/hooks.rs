//! The VFS's notification points, named after Linux's fsnotify hooks (linux-6.6/include/linux/fsnotify.h) and
//! called where Linux calls them, in Linux's order:
//! - `created` (`fsnotify_create`/`fsnotify_mkdir`): IN_CREATE on the directory; `linked` (`fsnotify_link`):
//!   IN_ATTRIB on the inode, then IN_CREATE;
//! - `unlinked` / `rmdir`: IN_ATTRIB on the inode (unlink only, `fsnotify_link_count`), its IN_DELETE_SELF and
//!   IN_IGNORED when this was its last name and nobody else holds the name (`d_delete` -> `dentry_unlink_inode`;
//!   otherwise at the last reference, `dentry_gone`), then IN_DELETE on the directory (`d_delete_notify`);
//! - `moved` (`fsnotify_move`): IN_MOVED_FROM and IN_MOVED_TO with one cookie, IN_ATTRIB on a replaced target,
//!   IN_MOVE_SELF on the moved inode;
//! - `changed` (`fsnotify_change`): ATTRIB / MODIFY / ACCESS from the attribute bits; `file` (`fsnotify_file`):
//!   ACCESS, MODIFY, OPEN, CLOSE_WRITE / CLOSE_NOWRITE of an open file (IN_EXCL_UNLINK watches skip unlinked
//!   names). Both go to the parent directory's watches with the name first, then to the inode's own.

use super::{active, inode_removed, send, Ev};
use super::{IN_ATTRIB, IN_CREATE, IN_DELETE, IN_ISDIR, IN_MODIFY, IN_MOVED_FROM, IN_MOVED_TO, IN_MOVE_SELF};
use crate::dentry::Dentry;
use crate::fd::OpenFile;
use crate::inode::Inode;
use crate::setattr::{ATTR_ATIME, ATTR_GID, ATTR_MODE, ATTR_MTIME, ATTR_UID};
use alloc::sync::Arc;
use core::sync::atomic::{AtomicU32, Ordering};

/// `fsnotify_sync_cookie`: one per rename, never 0 (wraps past it).
static COOKIE: AtomicU32 = AtomicU32::new(0);

fn isdir(i: &Inode) -> u32 {
    if i.is_dir() { IN_ISDIR } else { 0 }
}

fn cookie() -> u32 {
    loop {
        let c = COOKIE.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
        if c != 0 {
            return c;
        }
    }
}

/// An event on a name in `dir` (`fsnotify_name`).
fn name_event(dir: &Inode, mask: u32, name: &[u8], cookie: u32) {
    send(dir, Ev { mask, cookie, name: Some(name) }, false, false);
}

/// An event on an inode alone (`fsnotify_inode`).
pub(crate) fn inode_event(inode: &Inode, mask: u32) {
    if active() {
        send(inode, Ev { mask: mask | isdir(inode), cookie: 0, name: None }, false, false);
    }
}

/// `fsnotify_parent`: the parent's watches with the name, then the dentry's own. `path`: an open file's event
/// (IN_EXCL_UNLINK applies).
fn dentry_event(d: &Arc<Dentry>, mask: u32, path: bool) {
    if !active() {
        return;
    }
    let mask = mask | isdir(&d.inode);
    let unlinked = path && d.is_unlinked();
    if let Some(p) = d.parent() {
        let name = d.name();
        send(&p.inode, Ev { mask, cookie: 0, name: Some(&name) }, true, unlinked);
    }
    send(&d.inode, Ev { mask, cookie: 0, name: None }, false, unlinked);
}

/// A new name `child` in `dir` (a new node: `fsnotify_create`, `fsnotify_mkdir`).
pub(crate) fn created(dir: &Inode, child: &Dentry) {
    if active() {
        name_event(dir, IN_CREATE | isdir(&child.inode), &child.name(), 0);
    }
}

/// A hard link `name` in `dir` to `inode` (`fsnotify_link`).
pub(crate) fn linked(dir: &Inode, inode: &Inode, name: &[u8]) {
    if active() {
        inode_event(inode, IN_ATTRIB);
        name_event(dir, IN_CREATE, name, 0);
    }
}

/// Whether the removed dentry `victim` is used only by the remover (`holders` references it holds itself):
/// then the inode goes now, else when the last user lets it go (`dentry_gone`).
fn only_remover(victim: &Arc<Dentry>, holders: usize) -> bool {
    Arc::strong_count(victim) <= holders
}

/// `name` was unlinked from `dir` (`vfs_unlink`): `victim` is its detached dentry, the remover holding `holders`
/// references to it.
pub(crate) fn unlinked(dir: &Inode, victim: &Arc<Dentry>, name: &[u8], holders: usize) {
    if !active() {
        return;
    }
    inode_event(&victim.inode, IN_ATTRIB);
    if victim.inode.meta.nlink() == 0 && only_remover(victim, holders) {
        inode_removed(&victim.inode);
    }
    name_event(dir, IN_DELETE | isdir(&victim.inode), name, 0);
}

/// The directory `name` was removed from `dir` (`vfs_rmdir`).
pub(crate) fn rmdir(dir: &Inode, victim: &Arc<Dentry>, name: &[u8], holders: usize) {
    if !active() {
        return;
    }
    if only_remover(victim, holders) {
        inode_removed(&victim.inode);
    }
    name_event(dir, IN_DELETE | IN_ISDIR, name, 0);
}

/// `fsnotify_move`: `moved` went from `oname` in `odir` to `nname` in `ndir`; `target` is the dentry it replaced
/// (its remover holds `holders` references).
pub(crate) fn moved(odir: &Inode, oname: &[u8], ndir: &Inode, nname: &[u8], moved: &Inode, target: Option<(&Arc<Dentry>, usize)>) {
    if !active() {
        return;
    }
    let c = cookie();
    let d = isdir(moved);
    name_event(odir, IN_MOVED_FROM | d, oname, c);
    name_event(ndir, IN_MOVED_TO | d, nname, c);
    if let Some((t, _)) = target {
        inode_event(&t.inode, IN_ATTRIB);
    }
    inode_event(moved, IN_MOVE_SELF);
    if let Some((t, holders)) = target
        && t.inode.meta.nlink() == 0
        && only_remover(t, holders)
    {
        inode_removed(&t.inode);
    }
}

/// The last reference to a removed dentry went (`dentry_unlink_inode` at the final dput).
pub(crate) fn dentry_gone(inode: &Inode) {
    if active() && inode.meta.nlink() == 0 {
        inode_removed(inode);
    }
}

/// `fsnotify_change` for the attribute bits `valid` (`ATTR_SIZE`: `size`).
pub(crate) fn changed(d: &Arc<Dentry>, valid: u32, size: bool) {
    let mut mask = 0;
    if valid & (ATTR_UID | ATTR_GID | ATTR_MODE) != 0 {
        mask |= IN_ATTRIB;
    }
    if size {
        mask |= IN_MODIFY;
    }
    if valid & (ATTR_ATIME | ATTR_MTIME) == ATTR_ATIME | ATTR_MTIME {
        mask |= IN_ATTRIB;
    } else if valid & ATTR_ATIME != 0 {
        mask |= super::IN_ACCESS;
    } else if valid & ATTR_MTIME != 0 {
        mask |= IN_MODIFY;
    }
    if mask != 0 {
        dentry_event(d, mask, false);
    }
}

/// An open file's event (`fsnotify_file`): nothing for O_PATH files and files without a name.
pub(crate) fn file(f: &OpenFile, mask: u32) {
    if active()
        && !f.is_path_only()
        && let Some(p) = &f.path
    {
        dentry_event(&p.dentry, mask, true);
    }
}
