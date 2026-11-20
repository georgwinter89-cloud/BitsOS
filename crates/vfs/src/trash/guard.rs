//! EINS-C (F3) and the trash's own rule, for processes of an assistant tree only: no change (write, create,
//! truncate, rename, link, unlink, mode, owner, times) of
//! - the guarded files: the boot stick's `CONFIG.TXT` and `STICK.ID`, the MCP service's keys and pairings
//!   (`/boot/BITSOS2/mcp`, bitsmcp `keys::DIR`), the installer's full-access grants (`/data/bitsos-access`,
//!   bitsinstall `access_store`) and settingsd's stores (`/boot/BITSOS2/settings`, the fallback
//!   `/var/lib/bitsos/settings`; settings go through settingsd and its own rules);
//! - the trash (`<fs>/.papierkorb` and everything in it): it is emptied by Georg, restored by the kernel.
//!
//! A rename of a folder that holds one of them is refused too (`check_source`); creating in such a folder, its
//! mode and times stay free. EPERM with a journal line. Reading stays as the files' modes and the other rules say. The places are found
//! in the boot view each time (a mount that came later counts) and compared as dentries, so another spelling
//! (FAT: any case, the 8.3 alias), a symlink or a bind mount in a view of the tree's own leads to the same
//! answer.

use super::place;
use crate::dentry::Dentry;
use crate::mount_ns::Path;
use crate::ops::Ctx;
use crate::Errno;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use uapi::errno::EPERM;

/// The guarded places as the boot view names them; true: everything below too.
pub const GUARDED: [(&str, bool); 6] = [
    ("/boot/BITSOS2/CONFIG.TXT", false),
    ("/boot/BITSOS2/STICK.ID", false),
    ("/boot/BITSOS2/mcp", true),
    ("/boot/BITSOS2/settings", true),
    ("/data/bitsos-access", true),
    ("/var/lib/bitsos/settings", true),
];

klog::counter!(REFUSED, "vfs.tree.guarded");

/// Whether changing `name` in `at` (or `at` itself) is refused to `c`.
pub(crate) fn check(c: &Ctx, at: &Path, name: Option<&[u8]>) -> Result<(), Errno> {
    if c.subject.tree == 0 {
        return Ok(());
    }
    let Some(what) = refusal(at, name) else { return Ok(()) };
    REFUSED.inc();
    let p = name_of(c, at, name);
    klog::event!(Warn, "vfs", "tree.guarded", pid = c.subject.pid, tree = c.subject.tree, path = String::from_utf8_lossy(&p).as_ref(), what = what; "the assistant may not change this");
    Err(EPERM)
}

/// A rename of `name` in `at` by `c`: refused too when that name is a directory above a guarded place or the
/// trash (moving it away would leave the place unguarded under another path).
pub(crate) fn check_source(c: &Ctx, at: &Path, name: &[u8]) -> Result<(), Errno> {
    if c.subject.tree == 0 {
        return Ok(());
    }
    let Some(src) = at.dentry.child(name).filter(|d| d.is_dir()) else { return Ok(()) };
    let Some(what) = above(&src) else { return Ok(()) };
    REFUSED.inc();
    klog::event!(Warn, "vfs", "tree.guarded", pid = c.subject.pid, tree = c.subject.tree, path = String::from_utf8_lossy(&name_of(c, at, Some(name))).as_ref(), what = what; "the assistant may not move a folder that holds this");
    Err(EPERM)
}

/// The guarded place or trash below the directory `src`, if any.
fn above(src: &Arc<Dentry>) -> Option<&'static str> {
    let under = |n: &Arc<Dentry>| Arc::ptr_eq(&n.sb, &src.sb) && n.is_descendant_of(src);
    if super::persistent(&src.sb) {
        let top = place::chain(src).swap_remove(0);
        if place::home(&top).ok().and_then(|h| h.child(super::DIR)).is_some_and(|t| under(&t)) {
            return Some("trash");
        }
    }
    let ns = crate::root_ns();
    GUARDED.iter().find(|(path, _)| ns.lookup_path(path.as_bytes(), false).is_ok_and(|p| under(&p.dentry))).map(|(p, _)| *p)
}

fn name_of(c: &Ctx, at: &Path, name: Option<&[u8]>) -> Vec<u8> {
    let mut p = c.ns.path_of(&c.root, at).unwrap_or_else(|| b"?".to_vec());
    if let Some(n) = name {
        if p.last() != Some(&b'/') {
            p.push(b'/');
        }
        p.extend_from_slice(n);
    }
    p
}

/// `name` in `dir` (or `dir` itself) is the place `node` (named `last` in `parent`), or below it when `whole`.
fn hits(dir: &Arc<Dentry>, name: Option<&[u8]>, parent: &Arc<Dentry>, last: &[u8], node: Option<&Arc<Dentry>>, whole: bool) -> bool {
    let below = |d: &Arc<Dentry>| node.is_some_and(|n| Arc::ptr_eq(d, n) || (whole && d.is_descendant_of(n)));
    match name {
        // The name as given, or another spelling of the same node (FAT's 8.3 alias).
        Some(n) => (Arc::ptr_eq(dir, parent) && (dir.sb.key(n) == dir.sb.key(last) || dir.child(n).is_some_and(|c| below(&c)))) || (whole && below(dir)),
        None => below(dir),
    }
}

fn refusal(at: &Path, name: Option<&[u8]>) -> Option<&'static str> {
    let d = &at.dentry;
    if super::persistent(&d.sb) {
        let top = place::chain(d).swap_remove(0);
        if let Ok(home) = place::home(&top) {
            let trash = home.child(super::DIR);
            if hits(d, name, &home, super::DIR, trash.as_ref(), true) {
                return Some("trash");
            }
        }
    }
    let ns = crate::root_ns();
    for (path, whole) in GUARDED {
        let (up, last) = path.rsplit_once('/')?;
        let Ok(parent) = ns.lookup_path(up.as_bytes(), true) else { continue };
        let node = parent.dentry.child(last.as_bytes());
        if !Arc::ptr_eq(&parent.dentry.sb, &d.sb) && node.as_ref().is_none_or(|n| !Arc::ptr_eq(&n.sb, &d.sb)) {
            continue;
        }
        if hits(d, name, &parent.dentry, last.as_bytes(), node.as_ref(), whole) {
            return Some(path);
        }
    }
    None
}

/// The guarded places, for the spec's list and the tests.
pub fn places() -> Vec<&'static str> {
    GUARDED.iter().map(|(p, _)| *p).collect()
}
