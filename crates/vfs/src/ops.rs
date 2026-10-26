//! Name operations with Linux's semantics and error order (fs/namei.c `do_open`, `may_open`, `do_mkdirat`,
//! `filename_create`, `do_unlinkat`, `do_rmdir`, `may_delete`, `do_renameat2`/`vfs_rename`, `do_linkat`,
//! `do_symlinkat`; fs/open.c `do_faccessat`, `do_truncate`). Used by the syscalls (`sys`) and by kernel code
//! (`Ctx::kernel`). B3: opening or creating outside the program's own folders asks `policy::check`.

use crate::abi::{self, Attr};
use crate::dentry::{Dentry, Fixed};
use crate::fd::OpenFile;
use crate::inode::{Cred, Data, MAY_EXEC, MAY_READ, MAY_WRITE};
use crate::mount_ns::{MountNs, Path};
use crate::node::{self, File, PathFile};
use crate::synth;
use crate::walk::{Last, LastKind, Walker};
use crate::Errno;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use ksync::OnceRef;
use uapi::errno::*;

klog::counter!(OPENS, "vfs.open");
klog::counter!(POLICY_REFUSED, "vfs.policy_refused");

/// Which folders belong to a program (its package and data folder): accesses there skip `policy::check`.
/// PK-1/PM-1 install the real answer; until then no folder is a program's own and every access is asked.
pub trait OwnFolders: Sync {
    fn is_own(&self, program: policy::ProgramId, path: &[u8]) -> bool;
}

static OWN: OnceRef<dyn OwnFolders> = OnceRef::new();

pub fn set_own_folders(f: &'static dyn OwnFolders) {
    OWN.set(f);
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CreateKind {
    File,
    Dir,
    Link,
}

/// Who acts and from where.
pub struct Ctx<'a> {
    pub ns: &'a MountNs,
    pub root: Path,
    pub cwd: Path,
    pub cred: Cred,
    pub umask: u32,
    pub subject: policy::Subject,
}

fn kind_bits(want: u32) -> u32 {
    want & (MAY_READ | MAY_WRITE | MAY_EXEC)
}

impl<'a> Ctx<'a> {
    /// The kernel itself: root credentials, no umask, no policy questions.
    pub fn kernel(ns: &'a MountNs) -> Ctx<'a> {
        let root = ns.root_path();
        Ctx { ns, cwd: root.clone(), root, cred: Cred::ROOT, umask: 0, subject: policy::Subject::KERNEL }
    }
    pub fn walker(&self) -> Walker<'a> {
        Walker::new(self.ns, self.root.clone(), self.cred)
    }
    fn start(&self, base: Option<&Path>) -> Path {
        base.cloned().unwrap_or_else(|| self.cwd.clone())
    }

    /// B3: asks the policy for an access outside the program's own folders. Refusals are EACCES.
    pub(crate) fn policy(&self, at: &Path, name: Option<&[u8]>, write: bool, create: bool) -> Result<(), Errno> {
        // PM-1: no path is built while this program's path rules cannot depend on it (allow-all programs).
        if policy::allows_any(&self.subject, policy::AccessClass::Path) {
            return Ok(());
        }
        let mut p = self.ns.path_of(&self.root, at).unwrap_or_else(|| b"(unreachable)".to_vec());
        if let Some(n) = name {
            if p.last() != Some(&b'/') {
                p.push(b'/');
            }
            p.extend_from_slice(n);
        }
        if OWN.get().is_some_and(|o| o.is_own(self.subject.program, &p)) {
            return Ok(());
        }
        let text = String::from_utf8_lossy(&p);
        match policy::check(&self.subject, &policy::Access::Path { path: &text, write, create }) {
            Ok(()) => Ok(()),
            Err(d) => {
                POLICY_REFUSED.inc();
                klog::event!(Info, "vfs", "policy.refused", pid = self.subject.pid, program = self.subject.program.0, path = &*text, write = write, create = create, reason = d.reason.as_str());
                Err(EACCES)
            }
        }
    }

    fn may_create(&self, dir: &Path) -> Result<(), Errno> {
        if dir.dentry.is_dead_dir() {
            return Err(ENOENT);
        }
        if dir.dentry.inode.immutable.load(core::sync::atomic::Ordering::Relaxed) {
            return Err(EPERM);
        }
        dir.dentry.inode.meta.permission(self.cred, MAY_WRITE | MAY_EXEC)
    }

    /// `may_delete`: write+search on the directory, the sticky-bit rule, the type rule.
    fn may_delete(&self, dir: &Path, victim: &Arc<Dentry>, isdir: bool) -> Result<(), Errno> {
        let dm = &dir.dentry.inode.meta;
        // inode_permission: no write access at all to an immutable inode.
        if dir.dentry.inode.immutable.load(core::sync::atomic::Ordering::Relaxed) {
            return Err(EPERM);
        }
        dm.permission(self.cred, MAY_WRITE | MAY_EXEC)?;
        if dm.mode() & abi::S_ISVTX != 0 && self.cred.uid != 0 && self.cred.uid != dm.uid() && self.cred.uid != victim.inode.meta.uid() {
            return Err(EPERM);
        }
        if isdir {
            if !victim.is_dir() {
                return Err(ENOTDIR);
            }
            if victim.parent().is_none() {
                return Err(EBUSY);
            }
        } else if victim.is_dir() {
            return Err(EISDIR);
        }
        Ok(())
    }

    /// A name component's length (NAME_MAX bytes, or what the directory's file system allows: `FsOps::name_max`).
    fn check_name(dir: &Path, name: &[u8]) -> Result<(), Errno> {
        if name.len() > crate::walk::name_max(dir) { Err(ENAMETOOLONG) } else { Ok(()) }
    }

    fn is_kernel(&self) -> bool {
        self.subject == policy::Subject::KERNEL
    }

    /// FS-2: a program creating a name in proc or sysfs. proc: the lookup of a missing name already fails
    /// (`proc_lookup_de` returns ENOENT), so every create is ENOENT. sysfs (kernfs): the permission check, then
    /// EACCES for a file (`vfs_create` without `create`), EPERM for a directory or link.
    fn fixed_create(&self, dir: &Path, kind: CreateKind) -> Result<(), Errno> {
        if self.is_kernel() {
            return Ok(());
        }
        match dir.dentry.sb.fixed {
            None => Ok(()),
            Some(Fixed::Proc) => Err(ENOENT),
            Some(Fixed::Sys) => {
                self.may_create(dir)?;
                Err(if kind == CreateKind::File { EACCES } else { EPERM })
            }
        }
    }

    /// FS-2: a program removing a name in proc or sysfs: ENOENT when missing, the `may_delete` checks, then
    /// EPERM (neither has `unlink`/`rmdir`).
    fn fixed_delete(&self, dir: &Path, name: &[u8], isdir: bool) -> Result<(), Errno> {
        if self.is_kernel() || dir.dentry.sb.fixed.is_none() {
            return Ok(());
        }
        let victim = self.walker().child(dir, name)?.ok_or(ENOENT)?;
        self.may_delete(dir, &victim.dentry, isdir)?;
        Err(EPERM)
    }

    /// open/openat/creat. `flags` as the program passed them (bits outside VALID_OPEN_FLAGS ignored).
    pub fn open(&self, base: Option<&Path>, path: &[u8], flags: u64, mode: u32) -> Result<Arc<OpenFile>, Errno> {
        let mut flags = flags & abi::VALID_OPEN_FLAGS;
        if flags & (abi::O_DIRECTORY | abi::O_CREAT) == abi::O_DIRECTORY | abi::O_CREAT {
            return Err(EINVAL);
        }
        if flags & abi::O_TMPFILE_BIT != 0 {
            if flags & abi::O_DIRECTORY == 0 || flags & abi::O_ACCMODE == abi::O_RDONLY {
                return Err(EINVAL);
            }
            return self.open_tmpfile(base, path, flags, mode);
        }
        let path_only = flags & abi::O_PATH != 0;
        if path_only {
            flags &= abi::O_DIRECTORY | abi::O_NOFOLLOW | abi::O_PATH | abi::O_CLOEXEC;
        }
        let mut want = match flags & abi::O_ACCMODE {
            abi::O_RDONLY => MAY_READ,
            abi::O_WRONLY => MAY_WRITE,
            _ => MAY_READ | MAY_WRITE,
        };
        if path_only {
            want = 0;
        } else if flags & abi::O_TRUNC != 0 {
            want |= MAY_WRITE;
        }
        let creat = flags & abi::O_CREAT != 0;
        let excl = creat && flags & abi::O_EXCL != 0;
        let follow = flags & abi::O_NOFOLLOW == 0 && !excl;
        let mut w = self.walker();
        let (mut dir, mut last) = w.parent(self.start(base), path)?;
        let mut created = false;
        let found = if !creat {
            // The last name without following first: a proc magic link may open its object itself (AB-7a CR8:
            // a pipe end opened again through /proc/<pid>/fd/<n>, Linux's jump into pipefs and `fifo_open`).
            let (d0, l0, f0) = w.last(dir, last, false)?;
            let f0 = f0.ok_or(ENOENT)?;
            if follow && f0.dentry.inode.is_symlink() {
                if !path_only
                    && let Some(m) = &f0.dentry.inode.magic
                    && let Some(r) = m.open(flags)
                {
                    w.follow_count()?;
                    let of = r?;
                    if flags & abi::O_DIRECTORY != 0 {
                        return Err(ENOTDIR);
                    }
                    OPENS.inc();
                    return Ok(of);
                }
                let (_, _, f) = w.last(d0, l0, true)?;
                f.ok_or(ENOENT)?
            } else {
                f0
            }
        } else {
            loop {
                if last.kind != LastKind::Normal {
                    let (_, _, f) = w.last(dir, last, false)?;
                    let _ = f.ok_or(ENOENT)?;
                    return Err(if excl { EEXIST } else { EISDIR });
                }
                if last.trailing {
                    return Err(EISDIR);
                }
                match w.child(&dir, &last.name)? {
                    Some(p) => {
                        if p.dentry.inode.is_symlink() && follow {
                            w.follow_count()?;
                            // FS-10: O_CREAT through a proc magic link opens the object it leads to.
                            if let Some(m) = &p.dentry.inode.magic {
                                break m.target()?;
                            }
                            let body = p.dentry.inode.readlink()?;
                            if body.is_empty() {
                                return Err(ENOENT);
                            }
                            let (d2, l2) = w.parent(dir, &body)?;
                            dir = d2;
                            last = l2;
                            continue;
                        }
                        if excl {
                            return Err(EEXIST);
                        }
                        break p;
                    }
                    None => {
                        self.fixed_create(&dir, CreateKind::File)?;
                        self.may_create(&dir)?;
                        if dir.mnt.readonly {
                            return Err(EROFS);
                        }
                        // Asked before the names lock: an `ask` rule may wait for an answer (B4).
                        self.policy(&dir, Some(&last.name), true, true)?;
                        let sb = dir.dentry.sb.clone();
                        // B1: a frozen file system takes no new names (waits before the names lock).
                        let _w = sb.write_begin()?;
                        let guard = sb.lock_names();
                        if dir.dentry.child(&last.name).is_some() {
                            // Created meanwhile by someone else: look again.
                            drop(guard);
                            continue;
                        }
                        self.may_create(&dir)?;
                        let perm = mode & 0o7777 & !self.umask;
                        let d = dir.dentry.create_child(&last.name, uapi::S_IFREG | perm, self.cred, crate::kapi::reg_data(&dir.dentry.sb))?;
                        drop(guard);
                        created = true;
                        break Path { mnt: dir.mnt.clone(), dentry: d };
                    }
                }
            }
        };
        let d = found.dentry.clone();
        if creat && !created && d.is_dir() {
            return Err(EISDIR);
        }
        if flags & abi::O_DIRECTORY != 0 && !d.is_dir() {
            return Err(ENOTDIR);
        }
        let ftype = d.inode.meta.file_type();
        if !created {
            match ftype {
                uapi::S_IFLNK if !path_only => return Err(ELOOP),
                uapi::S_IFDIR if want & MAY_WRITE != 0 => return Err(EISDIR),
                // FS-13: `may_open_dev` (linux-6.6/fs/namei.c:3250); O_PATH skips `may_open` (do_o_path).
                uapi::S_IFCHR | abi::S_IFBLK if !path_only && found.nodev() => {
                    klog::event!(Info, "vfs", "open.nodev", mount = found.mnt.id);
                    return Err(EACCES);
                }
                _ => {}
            }
            if want & MAY_WRITE != 0 && found.mnt.readonly && matches!(ftype, uapi::S_IFREG | uapi::S_IFDIR | uapi::S_IFLNK) {
                return Err(EROFS);
            }
            if want & MAY_WRITE != 0
                && let Some(o) = &d.sb.ops
            {
                o.may_write(&d)?;
            }
            if want != 0 {
                d.inode.meta.permission(self.cred, kind_bits(want))?;
            }
            if !path_only {
                self.policy(&found, None, want & MAY_WRITE != 0, false)?;
            }
        }
        // `do_dentry_open`'s get_write_access before `handle_truncate`: ETXTBSY for a running program's file.
        let write = if want & MAY_WRITE != 0 && ftype == uapi::S_IFREG && !path_only { Some(d.inode.get_write_access()?) } else { None };
        let truncated = !created && flags & abi::O_TRUNC != 0 && ftype == uapi::S_IFREG && !path_only;
        if truncated {
            let _g = d.sb.write_begin()?;
            d.inode.truncate(0)?;
        }
        OPENS.inc();
        // FS-3: a FIFO makes its pipe end here and may wait for the other side (no lock is held). The kernel
        // never waits for a program there (B4): its opens are non-blocking (O_WRONLY without a reader: ENXIO).
        let node_flags = if self.is_kernel() { flags | abi::O_NONBLOCK } else { flags };
        let file: Arc<dyn File> =
            if path_only { Arc::new(PathFile { mode: d.inode.meta.mode() }) } else { node::open_file_flags(&d, self.cred, node_flags, want & MAY_WRITE != 0)? };
        // FS-13: a recording program's opened file (its fills count for the program's preload record).
        if ftype == uapi::S_IFREG && !path_only {
            crate::preload::opened(self, &found);
        }
        let of = OpenFile::with_path_writing(file, flags, found, write);
        // FS-14: `fsnotify_open` at the end of `do_dentry_open` (an O_PATH open returns before it), then
        // `handle_truncate`'s notify_change(ATTR_SIZE | ATTR_MTIME | ATTR_CTIME): IN_OPEN before IN_MODIFY.
        crate::inotify::hooks::file(&of, crate::inotify::IN_OPEN);
        if truncated {
            crate::inotify::hooks::changed(&d, crate::setattr::ATTR_MTIME, true);
        }
        Ok(of)
    }

    /// O_TMPFILE (fs/namei.c `do_tmpfile`, `vfs_tmpfile`; mm/shmem.c `shmem_tmpfile`): the directory, EROFS,
    /// write+search permission on it, then a nameless regular file in it. B3: asked as a create in that folder.
    fn open_tmpfile(&self, base: Option<&Path>, path: &[u8], flags: u64, mode: u32) -> Result<Arc<OpenFile>, Errno> {
        let dir = self.walker().lookup(self.start(base), path, flags & abi::O_NOFOLLOW == 0)?;
        if !dir.dentry.is_dir() {
            return Err(ENOTDIR);
        }
        if dir.mnt.readonly {
            return Err(EROFS);
        }
        self.may_create(&dir)?;
        if dir.dentry.sb.fixed.is_some() && !self.is_kernel() {
            // proc and sysfs have no tmpfile method (`vfs_tmpfile`).
            return Err(EOPNOTSUPP);
        }
        self.policy(&dir, None, true, true)?;
        let perm = mode & 0o7777 & !self.umask;
        let linkable = flags & abi::O_EXCL == 0;
        // FS-14: a program's tmpfile is charged per page (shmem_tmpfile: VM_NORESERVE); the kernel's own nameless
        // objects (shared anonymous memory) by size, so their faults never meet an empty pool.
        let data = if self.is_kernel() { crate::kapi::reg_data_accounted(&dir.dentry.sb) } else { crate::kapi::reg_data(&dir.dentry.sb) };
        let d = dir.dentry.tmpfile_child(uapi::S_IFREG | perm, self.cred, data, linkable)?;
        OPENS.inc();
        let write = Some(d.inode.get_write_access()?);
        let found = Path { mnt: dir.mnt.clone(), dentry: d };
        Ok(OpenFile::with_path_writing(node::file_of(&found.dentry), flags, found, write))
    }

    /// The parent and a free last name for mkdir/symlink/link (`filename_create`): EEXIST for dots and taken
    /// names, ENOENT for a trailing slash on a non-directory create. Returns with the names lock taken.
    fn create_slot(&self, base: Option<&Path>, path: &[u8]) -> Result<(Path, Last), Errno> {
        let (dir, last) = self.walker().parent(self.start(base), path)?;
        if last.kind != LastKind::Normal {
            return Err(EEXIST);
        }
        Self::check_name(&dir, &last.name)?;
        Ok((dir, last))
    }

    /// `filename_create` + `may_create`; run once before asking the policy and again under the names lock.
    fn create_checks(&self, dir: &Path, last: &Last, want_dir: bool) -> Result<(), Errno> {
        self.create_checks_as(dir, last, if want_dir { CreateKind::Dir } else { CreateKind::Link })
    }

    fn create_checks_as(&self, dir: &Path, last: &Last, kind: CreateKind) -> Result<(), Errno> {
        let want_dir = kind == CreateKind::Dir;
        if self.walker().child(dir, &last.name)?.is_some() {
            return Err(EEXIST);
        }
        self.fixed_create(dir, kind)?;
        if last.trailing && !want_dir {
            return Err(ENOENT);
        }
        if dir.mnt.readonly {
            return Err(EROFS);
        }
        self.may_create(dir)
    }

    /// mknod/mknodat after `may_mknod` (fs/namei.c `do_mknodat`, `vfs_create`, `vfs_mknod`): the name as
    /// `filename_create` finds it, `may_create`, CAP_MKNOD (uid 0) for character and block devices other than
    /// the whiteout 0:0, then the node: a regular file (type 0 or S_IFREG), a FIFO (FS-3, `pipe::Fifo`), a
    /// socket name or a device number (`special`). `mode` carries the type; `dev` is the 32-bit
    /// `new_encode_dev` value. B3: asked as a create in that folder.
    pub fn mknod(&self, base: Option<&Path>, path: &[u8], mode: u32, dev: u32) -> Result<Path, Errno> {
        let ftype = match mode & uapi::S_IFMT {
            0 => uapi::S_IFREG,
            t => t,
        };
        let kind = if ftype == uapi::S_IFREG { CreateKind::File } else { CreateKind::Link };
        let (dir, last) = self.create_slot(base, path)?;
        let device = matches!(ftype, uapi::S_IFCHR | abi::S_IFBLK);
        let whiteout = ftype == uapi::S_IFCHR && dev == 0;
        let checks = |c: &Self| -> Result<(), Errno> {
            c.create_checks_as(&dir, &last, kind)?;
            if device && !whiteout && c.cred.uid != 0 {
                return Err(EPERM);
            }
            Ok(())
        };
        checks(self)?;
        self.policy(&dir, Some(&last.name), true, true)?;
        let sb = dir.dentry.sb.clone();
        let _w = sb.write_begin()?;
        let _g = sb.lock_names();
        checks(self)?;
        let perm = mode & 0o7777 & !self.umask;
        let (data, fifo) = match ftype {
            uapi::S_IFREG => (crate::kapi::reg_data(&dir.dentry.sb), None),
            uapi::S_IFIFO => {
                let f = Arc::new(crate::pipe::Fifo::new());
                (Data::Device(f.clone()), Some(f))
            }
            _ => {
                let rdev = if device { crate::special::decode_new_dev(dev) } else { 0 };
                (Data::Device(Arc::new(crate::special::Special::new(ftype, rdev))), None)
            }
        };
        let d = dir.dentry.create_child(&last.name, ftype | perm, self.cred, data)?;
        if let Some(f) = fifo {
            f.attach(&d.inode);
        }
        Ok(Path { mnt: dir.mnt.clone(), dentry: d })
    }

    pub fn mkdir(&self, base: Option<&Path>, path: &[u8], mode: u32) -> Result<Path, Errno> {
        let (dir, last) = self.create_slot(base, path)?;
        self.create_checks(&dir, &last, true)?;
        self.policy(&dir, Some(&last.name), true, true)?;
        let sb = dir.dentry.sb.clone();
        let _w = sb.write_begin()?;
        let _g = sb.lock_names();
        self.create_checks(&dir, &last, true)?;
        let perm = mode & (0o777 | abi::S_ISVTX) & !self.umask;
        // The kernel builds the static directories of proc and sysfs: they can carry computed names.
        let data = if dir.dentry.sb.fixed.is_some() { Data::SynthDir(Arc::new(crate::synth::SynthDir::new())) } else { Data::Dir };
        let d = dir.dentry.create_child(&last.name, uapi::S_IFDIR | perm, self.cred, data)?;
        Ok(Path { mnt: dir.mnt.clone(), dentry: d })
    }

    pub fn symlink(&self, target: &[u8], base: Option<&Path>, path: &[u8]) -> Result<Path, Errno> {
        if target.is_empty() {
            return Err(ENOENT);
        }
        let (dir, last) = self.create_slot(base, path)?;
        self.create_checks(&dir, &last, false)?;
        self.policy(&dir, Some(&last.name), true, true)?;
        let sb = dir.dentry.sb.clone();
        let _w = sb.write_begin()?;
        let _g = sb.lock_names();
        self.create_checks(&dir, &last, false)?;
        let d = dir.dentry.create_child(&last.name, uapi::S_IFLNK | 0o777, self.cred, Data::Symlink(target.to_vec()))?;
        Ok(Path { mnt: dir.mnt.clone(), dentry: d })
    }

    /// link/linkat (`follow` = AT_SYMLINK_FOLLOW).
    pub fn link(&self, obase: Option<&Path>, opath: &[u8], nbase: Option<&Path>, npath: &[u8], follow: bool) -> Result<(), Errno> {
        let old = self.walker().lookup(self.start(obase), opath, follow)?;
        self.link_to(&old, nbase, npath)
    }

    /// A new name `npath` for the object at `old`.
    pub fn link_to(&self, old: &Path, nbase: Option<&Path>, npath: &[u8]) -> Result<(), Errno> {
        let (dir, last) = self.create_slot(nbase, npath)?;
        if dir.dentry.sb.fixed.is_some() && !self.is_kernel() {
            if self.walker().child(&dir, &last.name)?.is_some() {
                return Err(EEXIST);
            }
            self.fixed_create(&dir, CreateKind::Link)?;
        }
        // B3: a new name for an outside object must not turn it into an own-folder file the policy never sees
        // again (a later open of the link is not asked). The source is asked as a write, since the link
        // could be opened for writing.
        if !old.dentry.is_dir() {
            self.policy(old, None, true, false)?;
        }
        self.policy(&dir, Some(&last.name), true, true)?;
        let sb = dir.dentry.sb.clone();
        let _w = sb.write_begin()?;
        let _g = sb.lock_names();
        if dir.dentry.child(&last.name).is_some() {
            return Err(EEXIST);
        }
        if last.trailing {
            return Err(ENOENT);
        }
        if !Arc::ptr_eq(&old.mnt, &dir.mnt) {
            return Err(EXDEV);
        }
        if dir.mnt.readonly {
            return Err(EROFS);
        }
        self.may_create(&dir)?;
        let m = &old.dentry.inode.meta;
        // fs.protected_hardlinks=1: only the owner (or root) links a file it could not open read-write.
        if self.cred.uid != 0 && self.cred.uid != m.uid() && (m.file_type() != uapi::S_IFREG || m.permission(self.cred, MAY_READ | MAY_WRITE).is_err()) {
            return Err(EPERM);
        }
        if old.dentry.is_dir() {
            return Err(EPERM);
        }
        // An O_TMPFILE inode gets its first name once (`I_LINKABLE`); any other inode without names is gone.
        let first_name = m.nlink() == 0;
        if first_name && !old.dentry.inode.linkable.load(core::sync::atomic::Ordering::Acquire) {
            return Err(ENOENT);
        }
        dir.dentry.link_child(&last.name, old.dentry.inode.clone())?;
        if first_name {
            old.dentry.inode.linkable.store(false, core::sync::atomic::Ordering::Release);
        }
        Ok(())
    }

    pub fn unlink(&self, base: Option<&Path>, path: &[u8]) -> Result<(), Errno> {
        let (dir, last) = self.walker().parent(self.start(base), path)?;
        if last.kind != LastKind::Normal {
            return Err(EISDIR);
        }
        if dir.mnt.readonly {
            return Err(EROFS);
        }
        Self::check_name(&dir, &last.name)?;
        self.fixed_delete(&dir, &last.name, false)?;
        self.policy(&dir, Some(&last.name), true, false)?;
        let sb = dir.dentry.sb.clone();
        let _w = sb.write_begin()?;
        let _g = sb.lock_names();
        let victim = dir.dentry.child(&last.name);
        if last.trailing {
            return Err(match victim {
                None => ENOENT,
                Some(v) if v.is_dir() => EISDIR,
                Some(_) => ENOTDIR,
            });
        }
        let victim = victim.ok_or(ENOENT)?;
        self.may_delete(&dir, &victim, false)?;
        if let Some(o) = &sb.ops {
            o.may_remove(&victim)?;
        }
        if self.ns.is_covered(&dir.mnt, &victim) {
            return Err(EBUSY);
        }
        if let Some(o) = &sb.ops {
            o.remove(&dir.dentry, &victim)?;
        }
        let name = victim.name();
        if let Some(v) = dir.dentry.detach(&name) {
            v.inode.drop_link();
            v.inode.meta.touch_ctime();
            dir.dentry.inode.meta.touch_mtime();
            // FS-14: `victim` and `v` are the remover's own references.
            crate::inotify::hooks::unlinked(&dir.dentry.inode, &v, &name, 2);
        }
        Ok(())
    }

    pub fn rmdir(&self, base: Option<&Path>, path: &[u8]) -> Result<(), Errno> {
        let (dir, last) = self.walker().parent(self.start(base), path)?;
        match last.kind {
            LastKind::DotDot => return Err(ENOTEMPTY),
            LastKind::Dot => return Err(EINVAL),
            LastKind::Root => return Err(EBUSY),
            LastKind::Normal => {}
        }
        if dir.mnt.readonly {
            return Err(EROFS);
        }
        Self::check_name(&dir, &last.name)?;
        self.fixed_delete(&dir, &last.name, true)?;
        self.policy(&dir, Some(&last.name), true, false)?;
        let sb = dir.dentry.sb.clone();
        let _w = sb.write_begin()?;
        let _g = sb.lock_names();
        let victim = dir.dentry.child(&last.name).ok_or(ENOENT)?;
        self.may_delete(&dir, &victim, true)?;
        if let Some(o) = &sb.ops {
            o.may_remove(&victim)?;
        }
        if self.ns.is_covered(&dir.mnt, &victim) {
            return Err(EBUSY);
        }
        if victim.child_count() != 0 {
            return Err(ENOTEMPTY);
        }
        if let Some(o) = &sb.ops {
            o.remove(&dir.dentry, &victim)?;
        }
        let name = victim.name();
        let gone = dir.dentry.detach(&name);
        if let Some(v) = &gone {
            v.mark_dead();
            v.inode.meta.clear_nlink();
            v.inode.meta.touch_ctime();
        }
        dir.dentry.inode.meta.dec_nlink();
        dir.dentry.inode.meta.touch_mtime();
        if let Some(v) = &gone {
            crate::inotify::hooks::rmdir(&dir.dentry.inode, v, &name, 2);
        }
        Ok(())
    }

    /// rename/renameat/renameat2 (`flags`: RENAME_NOREPLACE, RENAME_EXCHANGE; RENAME_WHITEOUT is refused).
    pub fn rename(&self, obase: Option<&Path>, opath: &[u8], nbase: Option<&Path>, npath: &[u8], flags: u64) -> Result<(), Errno> {
        if flags & !(abi::RENAME_NOREPLACE | abi::RENAME_EXCHANGE | abi::RENAME_WHITEOUT) != 0 {
            return Err(EINVAL);
        }
        if flags & (abi::RENAME_NOREPLACE | abi::RENAME_WHITEOUT) != 0 && flags & abi::RENAME_EXCHANGE != 0 {
            return Err(EINVAL);
        }
        if flags & abi::RENAME_WHITEOUT != 0 {
            // Whiteouts need CAP_MKNOD on Linux; overlay file systems (their only user) do not exist here.
            return Err(if self.cred.uid == 0 { EINVAL } else { EPERM });
        }
        let exchange = flags & abi::RENAME_EXCHANGE != 0;
        let (odir, olast) = self.walker().parent(self.start(obase), opath)?;
        let (ndir, nlast) = self.walker().parent(self.start(nbase), npath)?;
        if !Arc::ptr_eq(&odir.mnt, &ndir.mnt) {
            return Err(EXDEV);
        }
        if olast.kind != LastKind::Normal {
            return Err(EBUSY);
        }
        if nlast.kind != LastKind::Normal {
            return Err(if flags & abi::RENAME_NOREPLACE != 0 { EEXIST } else { EBUSY });
        }
        if odir.mnt.readonly {
            return Err(EROFS);
        }
        Self::check_name(&odir, &olast.name)?;
        Self::check_name(&ndir, &nlast.name)?;
        if odir.dentry.sb.fixed.is_some() && !self.is_kernel() {
            // do_renameat2: both names are looked up first (a missing new name fails proc's lookup), then
            // vfs_rename's may_delete, then EPERM (no `rename` method).
            let old = self.walker().child(&odir, &olast.name)?.ok_or(ENOENT)?;
            if odir.dentry.sb.fixed == Some(Fixed::Proc) && self.walker().child(&ndir, &nlast.name)?.is_none() {
                return Err(ENOENT);
            }
            self.may_delete(&odir, &old.dentry, old.dentry.is_dir())?;
            return Err(EPERM);
        }
        self.policy(&odir, Some(&olast.name), true, false)?;
        self.policy(&ndir, Some(&nlast.name), true, true)?;
        let sb = odir.dentry.sb.clone();
        let _w = sb.write_begin()?;
        let _g = sb.lock_names();
        let old = odir.dentry.child(&olast.name).ok_or(ENOENT)?;
        let new = ndir.dentry.child(&nlast.name);
        if flags & abi::RENAME_NOREPLACE != 0 && new.is_some() {
            return Err(EEXIST);
        }
        if exchange {
            match &new {
                None => return Err(ENOENT),
                Some(n) if !n.is_dir() && nlast.trailing => return Err(ENOTDIR),
                _ => {}
            }
        }
        if !old.is_dir() && (olast.trailing || (!exchange && nlast.trailing)) {
            return Err(ENOTDIR);
        }
        let same_dir = Arc::ptr_eq(&odir.dentry, &ndir.dentry);
        // The source must not be an ancestor of the target's directory, nor the target one of the source's.
        if !same_dir && ndir.dentry.is_descendant_of(&old) {
            return Err(EINVAL);
        }
        if let Some(n) = &new
            && !same_dir
            && odir.dentry.is_descendant_of(n)
        {
            return Err(if exchange { EINVAL } else { ENOTEMPTY });
        }
        if let Some(n) = &new
            && Arc::ptr_eq(&n.inode, &old.inode)
        {
            return Ok(());
        }
        self.may_delete(&odir, &old, old.is_dir())?;
        if let Some(o) = &sb.ops {
            o.may_remove(&old)?;
            if let Some(n) = &new {
                o.may_remove(n)?;
            }
        }
        match &new {
            None => self.may_create(&ndir)?,
            Some(n) => self.may_delete(&ndir, n, if exchange { n.is_dir() } else { old.is_dir() })?,
        }
        if !same_dir {
            if old.is_dir() {
                old.inode.meta.permission(self.cred, MAY_WRITE)?;
            }
            if let Some(n) = &new
                && exchange
                && n.is_dir()
            {
                n.inode.meta.permission(self.cred, MAY_WRITE)?;
            }
        }
        if self.ns.is_covered(&odir.mnt, &old) || new.as_ref().is_some_and(|n| self.ns.is_covered(&ndir.mnt, n)) {
            return Err(EBUSY);
        }
        if let Some(n) = &new
            && !exchange
            && n.is_dir()
            && n.child_count() != 0
        {
            return Err(ENOTEMPTY);
        }
        if let Some(o) = &sb.ops {
            match (&new, exchange) {
                (Some(n), true) => o.exchange(&old, n)?,
                _ => o.rename(&odir.dentry, &old, &ndir.dentry, &nlast.name, new.as_ref())?,
            }
        }
        // The new name: a replaced entry's own (a case-insensitive file system may have found it by another
        // spelling or by its alias), else the one given.
        let new_name = new.as_ref().map(|n| n.name()).unwrap_or_else(|| nlast.name.clone());
        let old_name = old.name();
        if exchange {
            Dentry::exchange(&odir.dentry, &old_name, &ndir.dentry, &new_name)?;
            if let Some(n) = &new {
                use crate::inotify::hooks::moved;
                moved(&odir.dentry.inode, &old_name, &ndir.dentry.inode, &new_name, &old.inode, None);
                moved(&ndir.dentry.inode, &new_name, &odir.dentry.inode, &old_name, &n.inode, None);
            }
        } else {
            let replaced = new.as_ref().and_then(|n| ndir.dentry.detach(&n.name()));
            if let Some(gone) = &replaced {
                if gone.is_dir() {
                    gone.mark_dead();
                    gone.inode.meta.clear_nlink();
                    ndir.dentry.inode.meta.dec_nlink();
                } else {
                    gone.inode.drop_link();
                }
                gone.inode.meta.touch_ctime();
            }
            let moved = odir.dentry.detach(&old_name).ok_or(ENOENT)?;
            ndir.dentry.reattach(&moved, &new_name)?;
            if moved.is_dir() && !same_dir {
                odir.dentry.inode.meta.dec_nlink();
                ndir.dentry.inode.meta.inc_nlink();
            }
            moved.inode.meta.touch_ctime();
            odir.dentry.inode.meta.touch_mtime();
            ndir.dentry.inode.meta.touch_mtime();
            // FS-14: a replaced target's references here: `new` and `replaced`.
            crate::inotify::hooks::moved(&odir.dentry.inode, &old_name, &ndir.dentry.inode, &new_name, &moved.inode, replaced.as_ref().map(|g| (g, 2)));
            return Ok(());
        }
        odir.dentry.inode.meta.touch_mtime();
        ndir.dentry.inode.meta.touch_mtime();
        Ok(())
    }

    pub fn lookup(&self, base: Option<&Path>, path: &[u8], follow: bool) -> Result<Path, Errno> {
        self.walker().lookup(self.start(base), path, follow)
    }

    pub fn stat(&self, base: Option<&Path>, path: &[u8], follow: bool) -> Result<Attr, Errno> {
        Ok(self.lookup(base, path, follow)?.attr())
    }

    pub fn readlink(&self, base: Option<&Path>, path: &[u8]) -> Result<Vec<u8>, Errno> {
        let p = self.lookup(base, path, false)?;
        p.dentry.inode.readlink()
    }

    /// access/faccessat (`mode`: R_OK|W_OK|X_OK bits, 0 = existence).
    pub fn access(&self, base: Option<&Path>, path: &[u8], mode: u32, follow: bool) -> Result<(), Errno> {
        let p = self.lookup(base, path, follow)?;
        if mode == 0 {
            return Ok(());
        }
        let m = &p.dentry.inode.meta;
        // FS-13: X_OK of a regular file on a noexec mount (linux-6.6/fs/open.c:489), before the permission bits.
        if mode & abi::X_OK != 0 && m.file_type() == uapi::S_IFREG && p.noexec() {
            return Err(EACCES);
        }
        if mode & abi::W_OK != 0 && p.mnt.readonly && matches!(m.file_type(), uapi::S_IFREG | uapi::S_IFDIR | uapi::S_IFLNK) {
            return Err(EROFS);
        }
        if mode & abi::W_OK != 0
            && let Some(o) = &p.dentry.sb.ops
        {
            o.may_write(&p.dentry)?;
        }
        m.permission(self.cred, mode & 7)
    }

    /// truncate(path, len) (`len` already checked >= 0).
    pub fn truncate(&self, base: Option<&Path>, path: &[u8], len: u64) -> Result<(), Errno> {
        let p = self.lookup(base, path, true)?;
        let i = &p.dentry.inode;
        if i.is_dir() {
            return Err(EISDIR);
        }
        if !i.is_reg() {
            return Err(EINVAL);
        }
        if p.mnt.readonly {
            return Err(EROFS);
        }
        if let Some(o) = &p.dentry.sb.ops {
            o.may_write(&p.dentry)?;
        }
        i.meta.permission(self.cred, MAY_WRITE)?;
        // `vfs_truncate`: get_write_access (ETXTBSY for a running program's file).
        let _w = i.get_write_access()?;
        self.policy(&p, None, true, false)?;
        let _g = p.dentry.sb.write_begin()?;
        i.truncate(len)?;
        crate::inotify::hooks::changed(&p.dentry, crate::setattr::ATTR_MTIME, true);
        Ok(())
    }

    /// chdir: a searchable directory.
    pub fn chdir_target(&self, base: Option<&Path>, path: &[u8]) -> Result<Path, Errno> {
        let p = self.lookup(base, path, true)?;
        Self::dir_usable(&p, self.cred)?;
        Ok(p)
    }

    pub fn dir_usable(p: &Path, cred: Cred) -> Result<(), Errno> {
        if !p.dentry.is_dir() {
            return Err(ENOTDIR);
        }
        p.dentry.inode.meta.permission(cred, MAY_EXEC)
    }
}

/// getdents64 on an open directory: fills at most `max` bytes of `linux_dirent64` records and advances the
/// position (positions: 0 ".", 1 "..", 1 + n the child with listing number n). EINVAL when not even the first
/// record fits, ENOENT for a removed directory, ENOTDIR for anything else.
pub fn read_dir(f: &OpenFile, max: usize) -> Result<Vec<u8>, Errno> {
    let mut got = Vec::new();
    read_dir_into(f, max, |b| {
        got = b.to_vec();
        Ok(())
    })?;
    Ok(got)
}

/// `read_dir` that hands the records to `sink` (the copy to the program) under the position lock and moves
/// the position only when `sink` succeeds: a fault does not lose entries.
pub fn read_dir_into(f: &OpenFile, max: usize, sink: impl FnOnce(&[u8]) -> Result<(), Errno>) -> Result<usize, Errno> {
    let p = f.path.as_ref().filter(|p| p.dentry.is_dir()).ok_or(ENOTDIR)?;
    let d = &p.dentry;
    if d.is_dead_dir() {
        return Err(ENOENT);
    }
    d.load()?;
    f.with_pos(|pos_ref| {
        let mut pos = *pos_ref;
        let mut out = Vec::new();
        let push = |out: &mut Vec<u8>, ino: u64, next: u64, dt: u8, name: &[u8]| -> Result<bool, Errno> {
            let len = abi::dirent64_len(name);
            if out.len() + len > max {
                return if out.is_empty() { Err(EINVAL) } else { Ok(false) };
            }
            out.try_reserve(len).map_err(|_| ENOMEM)?;
            abi::dirent64(out, ino, next as i64, dt, name);
            Ok(true)
        };
        'fill: {
            if pos == 0 {
                if !push(&mut out, d.inode.meta.ino, 1, abi::DT_DIR, b".")? {
                    break 'fill;
                }
                pos = 1;
            }
            if pos == 1 {
                let pino = d.parent().map(|p| p.inode.meta.ino).unwrap_or(d.inode.meta.ino);
                if !push(&mut out, pino, 2, abi::DT_DIR, b"..")? {
                    break 'fill;
                }
                pos = 2;
            }
            while pos < synth::DYN_BASE {
                let batch = d.children_from(pos - 1, 64);
                if batch.is_empty() {
                    // Static names done: computed ones (proc, sysfs) follow at their own positions.
                    pos = synth::DYN_BASE;
                    break;
                }
                for (seq, c) in batch {
                    // Renamed or removed since the batch was taken: it is listed at its new place (if any).
                    let Some(name) = c.name_at(seq) else {
                        pos = seq + 2;
                        continue;
                    };
                    if !push(&mut out, c.inode.meta.ino, seq + 2, abi::dtype(c.inode.meta.mode()), &name)? {
                        break 'fill;
                    }
                    pos = seq + 2;
                }
            }
            if let Data::SynthDir(sd) = &d.inode.data {
                loop {
                    // As many names as the room left can hold (a record is at least 24 bytes): each batch asks
                    // the sources for their whole list, so small batches made a big /proc quadratic.
                    let room = (max.saturating_sub(out.len()) / 24).max(1);
                    let batch = synth::list_from(d, sd, pos, room)?;
                    if batch.is_empty() {
                        break 'fill;
                    }
                    for e in batch {
                        if !push(&mut out, e.ino, e.next, e.dtype, &e.name)? {
                            break 'fill;
                        }
                        pos = e.next;
                    }
                }
            }
        }
        sink(&out)?;
        *pos_ref = pos;
        Ok(out.len())
    })
}
