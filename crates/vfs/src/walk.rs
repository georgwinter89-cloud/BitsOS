//! Path walk with Linux's rules (fs/namei.c: `link_path_walk`, `walk_component`, `handle_dots`,
//! `pick_link`, `lookup_last`, `filename_parentat`):
//! - every component is looked up in a directory the caller may search (exec bit, EACCES), a non-directory in
//!   the middle is ENOTDIR, a component longer than NAME_MAX that is actually looked up is ENAMETOOLONG;
//! - `.` stays, `..` stays at the view's root and steps out of mounts (`follow_dotdot`);
//! - symlinks in the middle are always followed; the last one only when asked or when a trailing slash
//!   follows it; at most `MAXSYMLINKS` (40) per walk, then ELOOP (the bound against hostile loops); an empty
//!   target is ENOENT;
//! - a trailing slash makes the last component a directory lookup (ENOTDIR for anything else);
//! - a proc magic link (FS-10: `/proc/<pid>/fd/<n>`) leads to the open file's own place (`nd_jump_link`).
//! The walk is iterative (a stack of pending components), never recursive.

use crate::abi::{MAXSYMLINKS, NAME_MAX};
use crate::inode::{Cred, MAY_EXEC};
use crate::mount_ns::{MountNs, Path};
use crate::Errno;
use alloc::vec::Vec;
use uapi::errno::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LastKind {
    Normal,
    Dot,
    DotDot,
    /// The path was only slashes.
    Root,
}

#[derive(Clone, Debug)]
pub struct Last {
    pub name: Vec<u8>,
    pub kind: LastKind,
    /// The path ended in `/` after this component.
    pub trailing: bool,
}

/// Splits a path into components (empty ones dropped) and tells whether a slash follows the last.
pub fn split(path: &[u8]) -> (Vec<&[u8]>, bool) {
    let comps: Vec<&[u8]> = path.split(|&b| b == b'/').filter(|c| !c.is_empty()).collect();
    let trailing = path.last() == Some(&b'/') && !comps.is_empty();
    (comps, trailing)
}

fn kind_of(c: &[u8]) -> LastKind {
    match c {
        b"." => LastKind::Dot,
        b".." => LastKind::DotDot,
        _ => LastKind::Normal,
    }
}

/// The longest component in bytes below `dir` (NAME_MAX, or its file system's `FsOps::name_max`).
pub(crate) fn name_max(dir: &Path) -> usize {
    dir.dentry.sb.ops.as_ref().map_or(NAME_MAX, |o| o.name_max())
}

pub struct Walker<'a> {
    pub ns: &'a MountNs,
    pub root: Path,
    pub cred: Cred,
    links: u32,
}

impl<'a> Walker<'a> {
    pub fn new(ns: &'a MountNs, root: Path, cred: Cred) -> Self {
        Walker { ns, root, cred, links: 0 }
    }

    /// The directory may be searched: a directory with the exec bit for the caller.
    fn may_lookup(&self, dir: &Path) -> Result<(), Errno> {
        if !dir.dentry.is_dir() {
            return Err(ENOTDIR);
        }
        dir.dentry.inode.meta.permission(self.cred, MAY_EXEC)
    }

    /// Looks `name` up in `dir` (searchable, checked by the caller): `Ok(None)` for a free name.
    pub fn child(&self, dir: &Path, name: &[u8]) -> Result<Option<Path>, Errno> {
        if name.len() > name_max(dir) {
            return Err(ENAMETOOLONG);
        }
        dir.dentry.load()?;
        if let Some(d) = dir.dentry.child(name) {
            return Ok(Some(self.ns.cross_down(Path { mnt: dir.mnt.clone(), dentry: d })));
        }
        // proc and sysfs: names computed by their owners (nothing is mounted on them).
        if let crate::inode::Data::SynthDir(sd) = &dir.dentry.inode.data {
            return Ok(crate::synth::lookup(&dir.dentry, sd, name)?.map(|d| Path { mnt: dir.mnt.clone(), dentry: d }));
        }
        Ok(None)
    }

    pub(crate) fn follow_count(&mut self) -> Result<(), Errno> {
        self.links += 1;
        if self.links > MAXSYMLINKS { Err(ELOOP) } else { Ok(()) }
    }

    /// Walks all but the last component (Linux `filename_parentat`): returns the directory that holds the
    /// last component and the last component itself. The directory was checked to be searchable.
    pub fn parent(&mut self, start: Path, path: &[u8]) -> Result<(Path, Last), Errno> {
        if path.is_empty() {
            return Err(ENOENT);
        }
        let mut cur = if path[0] == b'/' { self.root.clone() } else { start };
        let (comps, trailing) = split(path);
        if comps.is_empty() {
            return Ok((cur, Last { name: Vec::new(), kind: LastKind::Root, trailing: false }));
        }
        let mut stack: Vec<Vec<u8>> = comps.iter().rev().map(|c| c.to_vec()).collect();
        while let Some(comp) = stack.pop() {
            self.may_lookup(&cur)?;
            if stack.is_empty() {
                return Ok((cur, Last { kind: kind_of(&comp), name: comp, trailing }));
            }
            match kind_of(&comp) {
                LastKind::Dot | LastKind::Root => {}
                LastKind::DotDot => cur = self.ns.parent_of(&self.root, &cur),
                LastKind::Normal => {
                    let next = self.child(&cur, &comp)?.ok_or(ENOENT)?;
                    if next.dentry.inode.is_symlink() {
                        self.follow_count()?;
                        // FS-10: a proc magic link continues at the object itself (`nd_jump_link`).
                        if let Some(m) = &next.dentry.inode.magic {
                            cur = m.target()?;
                            continue;
                        }
                        let body = next.dentry.inode.readlink()?;
                        if body.is_empty() {
                            return Err(ENOENT);
                        }
                        if body[0] == b'/' {
                            cur = self.root.clone();
                        }
                        let (bc, _) = split(&body);
                        stack.extend(bc.iter().rev().map(|c| c.to_vec()));
                    } else {
                        cur = next;
                    }
                }
            }
        }
        Err(ENOENT)
    }

    /// Resolves the last component found by `parent`. Follows a final symlink when `follow` (or a trailing
    /// slash) says so. `Ok((dir, last, None))` is a free name in `dir` (for creating); a found object comes
    /// back as `Some(path)` together with the directory and name it was found under.
    pub fn last(&mut self, mut dir: Path, mut last: Last, follow: bool) -> Result<(Path, Last, Option<Path>), Errno> {
        loop {
            match last.kind {
                LastKind::Root | LastKind::Dot => return Ok((dir.clone(), last, Some(dir))),
                LastKind::DotDot => {
                    let p = self.ns.parent_of(&self.root, &dir);
                    return Ok((dir, last, Some(p)));
                }
                LastKind::Normal => {
                    let Some(found) = self.child(&dir, &last.name)? else { return Ok((dir, last, None)) };
                    if found.dentry.inode.is_symlink() && (follow || last.trailing) {
                        self.follow_count()?;
                        if let Some(m) = &found.dentry.inode.magic {
                            let to = m.target()?;
                            if last.trailing && !to.dentry.is_dir() {
                                return Err(ENOTDIR);
                            }
                            return Ok((dir, last, Some(to)));
                        }
                        let body = found.dentry.inode.readlink()?;
                        if body.is_empty() {
                            return Err(ENOENT);
                        }
                        let (d2, mut l2) = self.parent(dir, &body)?;
                        l2.trailing |= last.trailing;
                        dir = d2;
                        last = l2;
                        continue;
                    }
                    if last.trailing && !found.dentry.is_dir() {
                        return Err(ENOTDIR);
                    }
                    return Ok((dir, last, Some(found)));
                }
            }
        }
    }

    /// A whole lookup (`filename_lookup`): ENOENT for a free name.
    pub fn lookup(&mut self, start: Path, path: &[u8], follow: bool) -> Result<Path, Errno> {
        let (dir, last) = self.parent(start, path)?;
        let (_, _, found) = self.last(dir, last, follow)?;
        found.ok_or(ENOENT)
    }
}
