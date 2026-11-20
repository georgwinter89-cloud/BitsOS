//! Putting an entry back, and emptying, done by the kernel for the caller (`node`): the trash's directories copy
//! the removed ones' owners, so neither works with the caller's own rights.
//!
//! - restore: every file and directory of `files/` goes back to its path below the file system's top; a
//!   directory that is there still is gone into. A name that is taken refuses the whole restore before anything
//!   moves (EEXIST): nothing is overwritten, the caller asks for another name and restores the entry's top
//!   there (`to`). The emptied entry is then removed.
//! - empty: removes an entry (or all) for real. Never from the tree (EPERM, journal line `trash.empty_refused`).
//!
//! Both: the caller is root or the entry's owner (whose removal made it); the file system is named by its top as
//! the boot view shows it. A restore moves with the kernel's rights, but only to where the caller could make the
//! name itself (`Ctx::may_put` with the caller's credentials and subject: write and search on the directory, the
//! policy, and for the tree the write guard), and never into the trash: a whole restore is refused before
//! anything moves.

use super::place;
use crate::dentry::Dentry;
use crate::inode::Cred;
use crate::mount_ns::{MountNs, Path};
use crate::ops::Ctx;
use crate::walk::LastKind;
use crate::Errno;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use uapi::errno::*;

/// Who asks.
#[derive(Clone, Copy, Debug)]
pub struct Caller {
    pub pid: u32,
    pub tree: u32,
    pub cred: Cred,
    /// Its program (the policy's path rules apply to where a restore puts things).
    pub program: policy::ProgramId,
}

impl Caller {
    /// The caller as a context of its own: its credentials and subject, from the view's root.
    fn ctx<'a>(&self, ns: &'a MountNs) -> Ctx<'a> {
        let mut c = Ctx::kernel(ns);
        c.cred = self.cred;
        c.subject = policy::Subject { program: self.program, pid: self.pid, uid: self.cred.uid, tree: self.tree };
        c
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct Restored {
    /// Names put back (a directory that went back whole counts once).
    pub moved: usize,
}

struct Trash {
    top: Path,
    dir: Arc<Dentry>,
}

impl Trash {
    fn at(&self, d: &Arc<Dentry>) -> Path {
        Path { mnt: self.top.mnt.clone(), dentry: d.clone() }
    }
    /// `d` is the trash or inside it.
    fn holds(&self, d: &Arc<Dentry>) -> bool {
        Arc::ptr_eq(d, &self.dir) || d.is_descendant_of(&self.dir)
    }
}

fn open(ns: &MountNs, fs: &[u8]) -> Result<Trash, Errno> {
    let top = ns.lookup_path(fs, true)?;
    if top.dentry.parent().is_some() || !super::persistent(&top.dentry.sb) {
        return Err(EINVAL);
    }
    let dir = place::home(&top.dentry)?.child(super::DIR).filter(|d| d.is_dir()).ok_or(ENOENT)?;
    Ok(Trash { top, dir })
}

fn valid_id(id: &[u8]) -> bool {
    !id.is_empty() && id != b"." && id != b".." && !id.contains(&b'/') && !id.contains(&0)
}

/// Root, or the one whose removal made the entry (its owner).
fn may(who: &Caller, d: &Dentry) -> bool {
    who.cred.uid == 0 || who.cred.uid == d.inode.meta.uid()
}

fn entry(t: &Trash, who: &Caller, id: &[u8]) -> Result<Arc<Dentry>, Errno> {
    if !valid_id(id) {
        return Err(EINVAL);
    }
    let e = t.dir.child(id).filter(|d| d.is_dir()).ok_or(ENOENT)?;
    if !may(who, &e) {
        return Err(EACCES);
    }
    Ok(e)
}

/// Removes `name` in `parent` and everything below it, for real (the kernel's own removal: no trash).
fn remove_all(c: &Ctx, parent: &Path, name: &[u8]) -> Result<(), Errno> {
    let top = c.lookup(Some(parent), name, false)?;
    if !top.dentry.is_dir() {
        return c.unlink(Some(parent), name);
    }
    let mut stack = alloc::vec![top];
    while let Some(dir) = stack.last().cloned() {
        let Some((_, child)) = dir.dentry.children_from(0, 1).into_iter().next() else {
            stack.pop();
            let up = stack.last().cloned().unwrap_or_else(|| parent.clone());
            c.rmdir(Some(&up), &dir.dentry.name())?;
            continue;
        };
        let p = Path { mnt: dir.mnt.clone(), dentry: child.clone() };
        if child.is_dir() && child.child_count() > 0 {
            stack.push(p);
        } else if child.is_dir() {
            c.rmdir(Some(&dir), &child.name())?;
        } else {
            c.unlink(Some(&dir), &child.name())?;
        }
    }
    Ok(())
}

/// The entry's top as its record names it (names below the file system's top); None without a readable record.
fn recorded_top(e: &Arc<Dentry>, fs: &[u8]) -> Option<Vec<Vec<u8>>> {
    let r = e.child(super::RECORD)?;
    let mut buf = alloc::vec![0u8; 8192];
    let n = r.inode.read_at(0, &mut buf).ok()?;
    let text = core::str::from_utf8(&buf[..n]).ok()?;
    let at = text.find("\"path\":\"")? + 8;
    let end = at + text[at..].find('"')?;
    let path = text[at..end].as_bytes();
    let rest = if fs == b"/" { path } else { path.strip_prefix(fs)? };
    Some(rest.split(|&b| b == b'/').filter(|c| !c.is_empty()).map(<[u8]>::to_vec).collect())
}

/// Restores entry `id` of the trash on the file system whose top is `fs` (a path in `ns`); with `to`, the entry's
/// top goes to that path (on the same file system, not taken) instead of its own.
pub fn restore(ns: &MountNs, who: &Caller, fs: &[u8], id: &[u8], to: Option<&[u8]>) -> Result<Restored, Errno> {
    let t = open(ns, fs)?;
    let e = entry(&t, who, id)?;
    let files = e.child(super::FILES).filter(|d| d.is_dir()).ok_or(ENOENT)?;
    let k = Ctx::kernel(ns);
    let me = who.ctx(ns);
    let moved = match to {
        None => back(&k, &me, &t, &files)?,
        Some(to) => elsewhere(&k, &me, &t, &e, &files, fs, to)?,
    };
    super::batch::end_on(&t.top.dentry.sb);
    remove_all(&k, &t.at(&t.dir), id)?;
    klog::event!(Info, "vfs", "trash.restored", pid = who.pid, tree = who.tree, id = String::from_utf8_lossy(id).as_ref(), moved = moved);
    Ok(Restored { moved })
}

/// Every name of `files` back to its place; refused as a whole when one is taken or the caller could not make it.
fn back(k: &Ctx, me: &Ctx, t: &Trash, files: &Arc<Dentry>) -> Result<usize, Errno> {
    let mut plan: Vec<(Arc<Dentry>, Vec<u8>, Arc<Dentry>)> = Vec::new();
    let mut work = alloc::vec![(files.clone(), t.top.dentry.clone())];
    while let Some((src, dst)) = work.pop() {
        for (_, child) in src.children_from(0, usize::MAX) {
            let name = child.name();
            match dst.child(&name) {
                None => plan.push((src.clone(), name, dst.clone())),
                Some(there) if child.is_dir() && there.is_dir() => work.push((child, there)),
                Some(_) => {
                    let mut p = dst.path_in_fs();
                    p.push(name);
                    klog::event!(Info, "vfs", "trash.restore_taken", path = String::from_utf8_lossy(&p.join(&b'/')).as_ref(); "nothing was moved: the name is taken");
                    return Err(EEXIST);
                }
            }
        }
    }
    for (_, name, dst) in &plan {
        if t.holds(dst) {
            return Err(EINVAL);
        }
        me.may_put(&t.at(dst), name)?;
    }
    for (src, name, dst) in &plan {
        k.rename(Some(&t.at(src)), name, Some(&t.at(dst)), name, crate::abi::RENAME_NOREPLACE)?;
    }
    Ok(plan.len())
}

/// The entry's top to `to` instead.
fn elsewhere(k: &Ctx, me: &Ctx, t: &Trash, e: &Arc<Dentry>, files: &Arc<Dentry>, fs: &[u8], to: &[u8]) -> Result<usize, Errno> {
    let top = recorded_top(e, fs).ok_or(ENOENT)?;
    let (last, up) = top.split_last().ok_or(EINVAL)?;
    let mut src = files.clone();
    for c in up {
        src = src.child(c).filter(|d| d.is_dir()).ok_or(ENOENT)?;
    }
    src.child(last).ok_or(ENOENT)?;
    let (dir, l) = me.walker().parent(me.root.clone(), to)?;
    if l.kind != LastKind::Normal {
        return Err(EINVAL);
    }
    if !Arc::ptr_eq(&dir.mnt, &t.top.mnt) {
        return Err(EXDEV);
    }
    if t.holds(&dir.dentry) {
        return Err(EINVAL);
    }
    me.may_put(&dir, &l.name)?;
    k.rename(Some(&t.at(&src)), last, Some(&dir), &l.name, crate::abi::RENAME_NOREPLACE)?;
    Ok(1)
}

/// Empties entry `id` (None: every entry) for real. Refused for a process of a tree.
pub fn empty(ns: &MountNs, who: &Caller, fs: &[u8], id: Option<&[u8]>) -> Result<usize, Errno> {
    if who.tree != 0 {
        klog::event!(Warn, "vfs", "trash.empty_refused", pid = who.pid, tree = who.tree; "only Georg empties the trash, never the assistant");
        return Err(EPERM);
    }
    let t = open(ns, fs)?;
    let k = Ctx::kernel(ns);
    let ids: Vec<Vec<u8>> = match id {
        Some(i) => alloc::vec![entry(&t, who, i)?.name()],
        None => {
            let all: Vec<_> = t.dir.children_from(0, usize::MAX).into_iter().map(|(_, d)| d).collect();
            if !all.is_empty() && !all.iter().any(|d| may(who, d)) {
                return Err(EACCES);
            }
            all.iter().filter(|d| may(who, d)).map(|d| d.name()).collect()
        }
    };
    super::batch::end_on(&t.top.dentry.sb);
    for i in &ids {
        remove_all(&k, &t.at(&t.dir), i)?;
    }
    klog::event!(Info, "vfs", "trash.emptied", pid = who.pid, entries = ids.len());
    Ok(ids.len())
}
