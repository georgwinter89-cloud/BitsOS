//! Where a victim goes and the move itself, under the caller's `write_begin` and names lock (the same step as
//! the call it replaces, so a crash leaves the file at its name or in the trash, never nowhere).
//!
//! Lock order: the file system's names lock, then `OPEN`, then the file system's own (the move and the trash's
//! directories are made under `OPEN`, so an entry's state and its directories agree). Entries that end are
//! dropped only after `OPEN` is let go: theirs may be the last references to the trash's dentries.

use super::batch::{self, Batch, OPEN};
use super::record::{self, What};
use crate::dentry::Dentry;
use crate::inode::{Cred, Data};
use crate::mount_ns::Path;
use crate::ops::Ctx;
use crate::Errno;
use alloc::format;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};
use uapi::errno::*;

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// What is left to do after the names lock: the record's new text, when the entry's top changed.
#[must_use]
pub(crate) struct Moved {
    record: Option<(Arc<Dentry>, Vec<u8>)>,
}

impl Moved {
    /// Writes the record (a failure is logged: the files are in the trash already, a restore needs no record).
    /// Over the old text in place, a shorter one padded with blanks (JSON's whitespace) to the old length: the
    /// file's length never shrinks, so a stop in between leaves the old record or the new one, never an empty one.
    pub(crate) fn finish(self) {
        let Some((d, text)) = self.record else { return };
        let r = d.sb.write_begin().and_then(|_w| {
            let text = padded(text, d.inode.size());
            d.inode.write_at(0, &text)
        });
        if let Err(e) = r {
            klog::event!(Warn, "vfs", "trash.record_failed", errno = e; "the entry stays restorable without its record");
        }
    }
}

/// `text` (ending in a newline) at least `len` bytes long: blanks before its newline.
fn padded(mut text: Vec<u8>, len: u64) -> Vec<u8> {
    let short = usize::try_from(len).unwrap_or(usize::MAX).saturating_sub(text.len());
    if short > 0 {
        let nl = text.pop();
        text.resize(text.len() + short, b' ');
        text.extend(nl);
    }
    text
}

/// The dentries from the file system's top down to `d` (`[top, .., d]`).
pub(super) fn chain(d: &Arc<Dentry>) -> Vec<Arc<Dentry>> {
    let mut v = alloc::vec![d.clone()];
    while let Some(p) = v[v.len() - 1].parent() {
        v.push(p);
    }
    v.reverse();
    v
}

/// The directory holding `.papierkorb` on the file system whose top is `top` (`FsOps::trash_home`).
pub(super) fn home(top: &Arc<Dentry>) -> Result<Arc<Dentry>, Errno> {
    let mut d = top.clone();
    for name in top.sb.ops.as_ref().map_or(&[][..], |o| o.trash_home()) {
        d = d.child(name).filter(|c| c.is_dir()).ok_or(EROFS)?;
    }
    Ok(d)
}

/// `name` in `dir` as a directory, made with `mode` and `owner` when missing. Caller holds the names lock.
fn dir_in(dir: &Arc<Dentry>, name: &[u8], mode: u32, owner: Cred) -> Result<Arc<Dentry>, Errno> {
    match dir.child(name) {
        Some(d) if d.is_dir() => Ok(d),
        Some(_) => Err(EEXIST),
        None => dir.create_child(name, uapi::S_IFDIR | (mode & 0o7777), owner, Data::Dir),
    }
}

/// A new entry for `c`'s removal below `dir` (top `top`).
fn start(c: &Ctx, dir: &Path, top: &Arc<Dentry>, what: What, now: u64) -> Result<Batch, Errno> {
    let trash = dir_in(&home(top)?, super::DIR, 0o700, c.cred)?;
    let secs = crate::inode::now().sec;
    let (entry, id) = loop {
        let id = format!("{secs}-{}", NEXT_ID.fetch_add(1, Ordering::Relaxed)).into_bytes();
        if trash.child(&id).is_none() {
            break (trash.create_child(&id, uapi::S_IFDIR | 0o700, c.cred, Data::Dir)?, id);
        }
    };
    let files = entry.create_child(super::FILES, uapi::S_IFDIR | 0o700, c.cred, Data::Dir)?;
    let record = entry.create_child(super::RECORD, uapi::S_IFREG | 0o600, c.cred, crate::kapi::reg_data(&top.sb))?;
    let (command, chat) = super::tell(c.subject.pid);
    let fs = fs_path(c, dir, top);
    klog::event!(Info, "vfs", "trash.entry", pid = c.subject.pid, tree = c.subject.tree, id = String::from_utf8_lossy(&id).as_ref(), fs = String::from_utf8_lossy(&fs).as_ref());
    Ok(Batch {
        tree: c.subject.tree,
        pid: c.subject.pid,
        sb: Arc::downgrade(&top.sb),
        files,
        record,
        id,
        fs,
        top: Vec::new(),
        written: None,
        what,
        secs,
        command,
        chat,
        last_ns: now,
    })
}

use alloc::string::String;

/// The file system's top as Georg names it: its mount in the boot view, else in the caller's view ("?" when neither
/// shows the top itself, e.g. a sandbox that has only a folder of it).
fn fs_path(c: &Ctx, dir: &Path, top: &Arc<Dentry>) -> Vec<u8> {
    let boot = crate::root_ns();
    let shown = boot.mounts().into_iter().find(|m| Arc::ptr_eq(&m.root, top)).and_then(|m| boot.path_of(&boot.root_path(), &Path { mnt: m.clone(), dentry: m.root.clone() }));
    shown.or_else(|| Arc::ptr_eq(&dir.mnt.root, top).then(|| c.ns.path_of(&c.root, &Path { mnt: dir.mnt.clone(), dentry: top.clone() })).flatten()).unwrap_or_else(|| b"?".to_vec())
}

/// The directory in `files` that stands for `orig` (`[top, .., dir]`), made with the originals' modes and owners.
fn counterpart(files: &Arc<Dentry>, orig: &[Arc<Dentry>]) -> Result<Arc<Dentry>, Errno> {
    let mut cur = files.clone();
    for o in &orig[1..] {
        let m = &o.inode.meta;
        cur = dir_in(&cur, &o.name(), m.mode(), Cred { uid: m.uid(), gid: m.gid() })?;
    }
    Ok(cur)
}

/// The common start of `a` and `b`.
fn common(a: &[Vec<u8>], b: &[Vec<u8>]) -> Vec<Vec<u8>> {
    a.iter().zip(b).take_while(|(x, y)| x == y).map(|(x, _)| x.clone()).collect()
}

/// `victim` was added to (or is held by) entry `b` at `path`: the new top, and the record's text if it changed.
fn note(b: &mut Batch, path: Vec<Vec<u8>>, now: u64) -> Moved {
    b.top = if b.written.is_none() && b.top.is_empty() { path } else { common(&b.top, &path) };
    b.last_ns = now;
    if b.written.as_ref() == Some(&b.top) {
        return Moved { record: None };
    }
    b.written = Some(b.top.clone());
    Moved { record: Some((b.record.clone(), record::text(b))) }
}

/// The move of `victim` (a child of `from`) to `name` in `to`, with the bookkeeping `Ctx::rename` does; the
/// caller sends the notifications (the victim's name is gone, or replaced, as its call says).
pub(super) fn shift(from: &Arc<Dentry>, victim: &Arc<Dentry>, to: &Arc<Dentry>, name: &[u8]) -> Result<(), Errno> {
    let cur = victim.name();
    if let Some(o) = &from.sb.ops {
        o.rename(from, victim, to, name, None)?;
    }
    let moved = from.detach(&cur).ok_or(ENOENT)?;
    to.reattach(&moved, name)?;
    if moved.is_dir() {
        from.inode.meta.dec_nlink();
        to.inode.meta.inc_nlink();
    }
    moved.inode.meta.touch_ctime();
    from.inode.meta.touch_mtime();
    to.inode.meta.touch_mtime();
    Ok(())
}

/// Moves a victim into its place in the trash: `mv` gets that directory and the victim's name and does the
/// move itself (`shift`, or a rename's steps).
pub(super) type Mover<'m> = &'m mut dyn FnMut(&Arc<Dentry>, &[u8]) -> Result<(), Errno>;

/// `victim`'s path below the file system's top (it is in `dir`).
fn path_in_fs(dir: &Path, name: &[u8]) -> Vec<Vec<u8>> {
    let mut path: Vec<Vec<u8>> = chain(&dir.dentry)[1..].iter().map(|d| d.name()).collect();
    path.push(name.to_vec());
    path
}

/// On an error the victim is where it was, or (`left`) in the trash with its record to write.
pub(super) fn move_in(c: &Ctx, dir: &Path, victim: &Arc<Dentry>, what: What, mv: Mover, left: &mut Option<Moved>) -> Result<Moved, Errno> {
    // statfs before `OPEN`: it may read the disk.
    super::room(c, dir)?;
    let mut gone = Vec::new();
    let r = move_locked(c, dir, victim, what, mv, left, &mut gone);
    drop(gone);
    r
}

fn move_locked(c: &Ctx, dir: &Path, victim: &Arc<Dentry>, what: What, mv: Mover, left: &mut Option<Moved>, gone: &mut Vec<Batch>) -> Result<Moved, Errno> {
    let orig = chain(&dir.dentry);
    let name = victim.name();
    let now = batch::now_ns();
    let mut open = OPEN.lock();
    batch::prune(&mut open, now, gone);
    let sb = &dir.dentry.sb;
    // A name the open entry holds already (made again and removed again): a new entry for it.
    for fresh in [false, true] {
        let mut started = false;
        let at = match batch::find(&open, c.subject.tree, c.subject.pid, sb) {
            Some(i) if !fresh => i,
            found => {
                if let Some(i) = found {
                    gone.try_reserve(1).map_err(|_| ENOMEM)?;
                    gone.push(open.swap_remove(i));
                }
                open.try_reserve(1).map_err(|_| ENOMEM)?;
                open.push(start(c, dir, &orig[0], what, now)?);
                started = true;
                open.len() - 1
            }
        };
        let to = match counterpart(&open[at].files, &orig) {
            Ok(d) if d.child(&name).is_none() => Ok(d),
            Ok(_) | Err(EEXIST) if !fresh => continue,
            Ok(_) => Err(EEXIST),
            Err(e) => Err(e),
        };
        let r = match &to {
            Ok(to) => mv(to, &name),
            Err(e) => Err(*e),
        };
        return match r {
            Ok(()) => Ok(note(&mut open[at], path_in_fs(dir, &name), now)),
            // The victim stayed in the trash (a failed move's undo failed too): the entry keeps it, with its record.
            Err(e) if to.as_ref().is_ok_and(|t| t.child(&name).is_some_and(|d| Arc::ptr_eq(&d, victim))) => {
                *left = Some(note(&mut open[at], path_in_fs(dir, &name), now));
                Err(e)
            }
            Err(e) => {
                // An entry made for this move alone goes again: the trash shows no empty entry.
                if started {
                    let b = open.swap_remove(at);
                    unmake(&b);
                    match gone.try_reserve(1) {
                        Ok(()) => gone.push(b),
                        Err(_) => drop(b),
                    }
                }
                Err(e)
            }
        };
    }
    Err(EEXIST)
}

/// Removes entry `b` (only what `start` and `counterpart` made: directories, the empty record) with the file
/// system's own calls, under the caller's names lock; a failure, or any other file in it, is logged and the rest
/// of the entry stays.
fn unmake(b: &Batch) {
    let Some(entry) = b.files.parent() else { return };
    let Some(trash) = entry.parent() else { return };
    let mut stack = alloc::vec![(trash, entry)];
    while let Some((up, d)) = stack.last().cloned() {
        if let Some((_, child)) = d.children_from(0, 1).into_iter().next() {
            stack.push((d, child));
            continue;
        }
        stack.pop();
        // A file in it is a victim a failed undo left there: the entry stays as it is.
        let kept = !d.is_dir() && !Arc::ptr_eq(&d, &b.record);
        if let Err(e) = if kept { Err(EEXIST) } else { raw_remove(&up, &d) } {
            klog::event!(Warn, "vfs", "trash.unmake_failed", errno = e, id = String::from_utf8_lossy(&b.id).as_ref(); "the entry stays in the trash");
            return;
        }
    }
}

fn raw_remove(dir: &Arc<Dentry>, victim: &Arc<Dentry>) -> Result<(), Errno> {
    if let Some(o) = &dir.sb.ops {
        o.remove(dir, victim)?;
    }
    let v = dir.detach(&victim.name()).ok_or(ENOENT)?;
    if v.is_dir() {
        v.mark_dead();
        v.inode.meta.clear_nlink();
        dir.inode.meta.dec_nlink();
    } else {
        v.inode.drop_link();
    }
    dir.inode.meta.touch_mtime();
    Ok(())
}

/// rmdir of the empty `victim`: whether the open entry of this process has its directory already (it was made
/// for the contents moved before); then it is only removed, and `took` widens the entry's top afterwards.
pub(super) fn holds(c: &Ctx, dir: &Path, victim: &Arc<Dentry>) -> bool {
    let mut gone = Vec::new();
    let found = {
        let mut open = OPEN.lock();
        batch::prune(&mut open, batch::now_ns(), &mut gone);
        batch::find(&open, c.subject.tree, c.subject.pid, &dir.dentry.sb).is_some_and(|at| {
            let mut cur = Some(open[at].files.clone());
            for o in chain(&dir.dentry)[1..].iter().map(|d| d.name()).chain(core::iter::once(victim.name())) {
                cur = cur.and_then(|d| d.child(&o)).filter(|d| d.is_dir());
            }
            cur.is_some()
        })
    };
    drop(gone);
    found
}

/// The directory `name` in `dir` that `holds` found was removed: the entry's top takes it in (None when the
/// entry ended meanwhile or its top stays).
pub(super) fn took(c: &Ctx, dir: &Path, name: &[u8]) -> Option<Moved> {
    let now = batch::now_ns();
    let mut open = OPEN.lock();
    let at = batch::find(&open, c.subject.tree, c.subject.pid, &dir.dentry.sb)?;
    Some(note(&mut open[at], path_in_fs(dir, name), now))
}
