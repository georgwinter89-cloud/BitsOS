//! Kernel-side file API on a view (no permission checks, no policy: the kernel acts as itself). Used at boot
//! (initramfs, device and proc nodes), by tests and by kernel services (MC-4a's file tools).

use crate::dentry::Dentry;
use crate::fd::OpenFile;
use crate::dentry::SuperBlock;
use crate::inode::Data;
use crate::pagecache::PageCache;
use crate::mount_ns::{MountNs, Path};
use crate::node::{self, File};
use crate::ops::Ctx;
use crate::walk::LastKind;
use crate::{abi, Errno};
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use uapi::errno::*;

/// The content of a new regular file on the file system of `sb` (its pages are charged to that tmpfs's pool
/// when allocated, FS-14).
pub(crate) fn reg_data(sb: &Arc<SuperBlock>) -> Data {
    Data::Reg(PageCache::tmpfs(sb.pool.clone()))
}

/// The content of a kernel-made nameless file (O_TMPFILE by the kernel: MM's shared anonymous memory), charged
/// by size like Linux's `shmem_kernel_file_setup` without VM_NORESERVE under overcommit mode 2.
pub(crate) fn reg_data_accounted(sb: &Arc<SuperBlock>) -> Data {
    Data::Reg(PageCache::tmpfs_accounted(sb.pool.clone()))
}

impl MountNs {
    fn kctx(&self) -> Ctx<'_> {
        Ctx::kernel(self)
    }

    /// The root directory of this view.
    pub fn root(&self) -> Arc<Dentry> {
        self.root_path().dentry
    }

    /// Resolves an absolute path (symlinks followed, at most 40 per walk).
    pub fn lookup(&self, path: &str) -> Result<Arc<Dentry>, Errno> {
        Ok(self.lookup_path(path.as_bytes(), true)?.dentry)
    }

    pub fn lookup_path(&self, path: &[u8], follow: bool) -> Result<Path, Errno> {
        self.kctx().lookup(None, path, follow)
    }

    /// Opens a node as a `File` (devices give their own object).
    pub fn open(&self, path: &str) -> Result<Arc<dyn File>, Errno> {
        Ok(node::file_of(&self.lookup(path)?))
    }

    /// Opens like a program would, but as the kernel (flags as open(2)).
    pub fn open_flags(&self, path: &[u8], flags: u64, mode: u32) -> Result<Arc<OpenFile>, Errno> {
        self.kctx().open(None, path, flags, mode)
    }

    /// Makes every missing directory of `path` (mode 0755); EEXIST when a non-directory is in the way.
    pub fn mkdir_p(&self, path: &str) -> Result<Arc<Dentry>, Errno> {
        self.mkdir_p_bytes(path.as_bytes(), 0o755)
    }

    pub fn mkdir_p_bytes(&self, path: &[u8], perm: u32) -> Result<Arc<Dentry>, Errno> {
        let c = self.kctx();
        let (comps, _) = crate::walk::split(path);
        let mut cur = self.root_path();
        for comp in comps {
            cur = match c.lookup(Some(&cur), comp, true) {
                Ok(p) if p.dentry.is_dir() => p,
                Ok(_) => return Err(EEXIST),
                Err(ENOENT) => match c.mkdir(Some(&cur), comp, perm) {
                    Ok(p) => p,
                    Err(EEXIST) => c.lookup(Some(&cur), comp, true)?,
                    Err(e) => return Err(e),
                },
                Err(e) => return Err(e),
            };
        }
        Ok(cur.dentry)
    }

    /// Where a kernel-created node goes: the parent directory (made if missing) and the last name. A node
    /// already there is removed first (a directory only when empty), so callers may run again (rerunnable
    /// ktests, a second initramfs entry with the same name).
    fn kernel_slot(&self, path: &[u8]) -> Result<(Path, Vec<u8>), Errno> {
        let c = self.kctx();
        let (dir, last) = c.walker().parent(self.root_path(), path)?;
        if last.kind != LastKind::Normal {
            return Err(EEXIST);
        }
        let dir = match dir.dentry.is_dir() {
            true => dir,
            false => return Err(ENOTDIR),
        };
        if let Some(old) = dir.dentry.child(&last.name) {
            if old.is_dir() {
                c.rmdir(Some(&dir), &last.name)?;
            } else {
                c.unlink(Some(&dir), &last.name)?;
            }
        }
        Ok((dir, last.name))
    }

    fn kernel_create(&self, path: &[u8], mode: u32, data: impl FnOnce(&Arc<SuperBlock>) -> Data) -> Result<Arc<Dentry>, Errno> {
        if let Some(i) = path.iter().rposition(|&b| b == b'/')
            && i > 0
        {
            self.mkdir_p_bytes(&path[..i], 0o755)?;
        }
        let (dir, name) = self.kernel_slot(path)?;
        let sb = dir.dentry.sb.clone();
        let _w = sb.write_begin()?;
        let _g = sb.lock_names();
        dir.dentry.create_child(&name, mode, crate::inode::Cred::ROOT, data(&sb))
    }

    /// Creates (or replaces) a regular file with `data`; missing parent directories are made.
    pub fn create_file(&self, path: &str, mode: u32, data: Vec<u8>) -> Result<Arc<Dentry>, Errno> {
        self.create_file_bytes(path.as_bytes(), mode, &data)
    }

    pub fn create_file_bytes(&self, path: &[u8], mode: u32, data: &[u8]) -> Result<Arc<Dentry>, Errno> {
        let d = self.kernel_create(path, uapi::S_IFREG | (mode & 0o7777), reg_data)?;
        if !data.is_empty() {
            // A kernel writer waits for a thaw like a program's (kernel_write takes file_start_write,
            // linux-6.6/fs/read_write.c:557; VS-2 CR3).
            let _w = d.sb.write_begin()?;
            let n = d.inode.write_at(0, data)?;
            if n != data.len() {
                return Err(ENOMEM);
            }
        }
        Ok(d)
    }

    pub fn create_symlink(&self, path: &str, target: &str) -> Result<Arc<Dentry>, Errno> {
        self.kernel_create(path.as_bytes(), uapi::S_IFLNK | 0o777, |_| Data::Symlink(target.as_bytes().to_vec()))
    }

    pub fn create_device(&self, path: &str, f: Arc<dyn File>) -> Result<Arc<Dentry>, Errno> {
        let mode = f.mode();
        // FS-3: a node made by number with mknod opens this object (`special`).
        crate::special::register(&f);
        self.kernel_create(path.as_bytes(), mode, |_| Data::Device(f))
    }

    /// FS-3: a FIFO (initramfs entries, kernel users); replaces what is there, like `create_file`.
    pub fn create_fifo(&self, path: &[u8], perm: u32) -> Result<Arc<Dentry>, Errno> {
        let fifo = Arc::new(crate::pipe::Fifo::new());
        let f2 = fifo.clone();
        let d = self.kernel_create(path, uapi::S_IFIFO | (perm & 0o7777), |_| Data::Device(f2))?;
        fifo.attach(&d.inode);
        Ok(d)
    }

    /// FS-3: a socket name or a device number (initramfs entries); `rdev` as `st_rdev` reports it.
    pub fn create_special(&self, path: &[u8], mode: u32, rdev: u64) -> Result<Arc<Dentry>, Errno> {
        let ftype = mode & uapi::S_IFMT;
        if !matches!(ftype, uapi::S_IFCHR | crate::abi::S_IFBLK | crate::abi::S_IFSOCK) {
            return Err(EINVAL);
        }
        self.kernel_create(path, mode, |_| Data::Device(Arc::new(crate::special::Special::new(ftype, rdev))))
    }

    pub fn create_proc(&self, path: &str, render: fn() -> String) -> Result<Arc<Dentry>, Errno> {
        self.kernel_create(path.as_bytes(), uapi::S_IFREG | 0o444, |_| Data::Proc(render))
    }

    /// A whole file (at most `max` bytes; EFBIG if it is longer). Files whose size says nothing (proc, sysfs,
    /// devices) are read until end of file.
    pub fn read_file(&self, path: &str, max: usize) -> Result<Vec<u8>, Errno> {
        let p = self.lookup_path(path.as_bytes(), true)?;
        if p.dentry.is_dir() {
            return Err(EISDIR);
        }
        let f = node::file_of(&p.dentry);
        let size = f.size() as usize;
        if size > max && matches!(p.dentry.inode.data, Data::Reg(_) | Data::External(_)) {
            return Err(EFBIG);
        }
        let mut out: Vec<u8> = Vec::new();
        let mut chunk = 4096usize;
        loop {
            let room = (max + 1).saturating_sub(out.len()).min(chunk);
            if room == 0 {
                return Err(EFBIG);
            }
            let at = out.len();
            out.try_reserve_exact(room).map_err(|_| ENOMEM)?;
            out.resize(at + room, 0);
            let n = f.read_at(at as u64, &mut out[at..])?;
            out.truncate(at + n);
            if out.len() > max {
                return Err(EFBIG);
            }
            if n == 0 {
                break;
            }
            chunk = (chunk * 2).min(1 << 20);
        }
        Ok(out)
    }

    /// Writes `data` at `offset` (or appends) to an existing or new regular file (mode 0644 when new).
    pub fn write_file(&self, path: &str, data: &[u8], offset: Option<u64>) -> Result<usize, Errno> {
        let flags = abi::O_WRONLY | abi::O_CREAT | if offset.is_none() { abi::O_APPEND } else { 0 };
        let f = self.open_flags(path.as_bytes(), flags, 0o644)?;
        match offset {
            Some(o) => f.pwrite(o, data),
            None => f.write(data),
        }
    }

    /// The target of the symbolic link at `path` (the last component is not followed); EINVAL for another type.
    pub fn read_link(&self, path: &str) -> Result<Vec<u8>, Errno> {
        let p = self.lookup_path(path.as_bytes(), false)?;
        match &p.dentry.inode.data {
            Data::Symlink(t) => Ok(t.clone()),
            _ => Err(EINVAL),
        }
    }

    /// Names in a directory with their stat (no "." and "..").
    pub fn list_dir(&self, path: &str) -> Result<Vec<(Vec<u8>, abi::Attr)>, Errno> {
        let p = self.lookup_path(path.as_bytes(), true)?;
        if !p.dentry.is_dir() {
            return Err(ENOTDIR);
        }
        let mut out: Vec<(Vec<u8>, abi::Attr)> = p.dentry.children_from(0, usize::MAX).into_iter().map(|(_, d)| (d.name(), d.attr())).collect();
        if let Data::SynthDir(sd) = &p.dentry.inode.data {
            let w = self.kctx().walker();
            for e in crate::synth::list_from(&p.dentry, sd, crate::synth::DYN_BASE, usize::MAX)? {
                if let Some(c) = w.child(&p, &e.name)? {
                    out.push((e.name, c.attr()));
                }
            }
        }
        Ok(out)
    }

    /// Removes `path` and everything below it (kernel clean-up, e.g. a test's scratch folder). Iterative, so
    /// any depth works; a mount inside the tree stops it with EBUSY.
    pub fn remove_tree(&self, path: &str) -> Result<(), Errno> {
        let c = self.kctx();
        let top = c.lookup(None, path.as_bytes(), false)?;
        if top.dentry.is_dir() {
            let mut stack = alloc::vec![top];
            while let Some(dir) = stack.last().cloned() {
                let Some((_, child)) = dir.dentry.children_from(0, 1).into_iter().next() else {
                    stack.pop();
                    if let Some(parent) = stack.last() {
                        c.rmdir(Some(parent), &dir.dentry.name())?;
                    }
                    continue;
                };
                let name = child.name();
                if child.is_dir() && child.child_count() > 0 {
                    if self.is_covered(&dir.mnt, &child) {
                        return Err(EBUSY);
                    }
                    stack.push(Path { mnt: dir.mnt.clone(), dentry: child });
                } else if child.is_dir() {
                    c.rmdir(Some(&dir), &name)?;
                } else {
                    c.unlink(Some(&dir), &name)?;
                }
            }
        }
        self.remove(path)
    }

    /// Removes a file, symlink or empty directory.
    pub fn remove(&self, path: &str) -> Result<(), Errno> {
        let c = self.kctx();
        let p = c.lookup(None, path.as_bytes(), false)?;
        if p.dentry.is_dir() { c.rmdir(None, path.as_bytes()) } else { c.unlink(None, path.as_bytes()) }
    }
}
