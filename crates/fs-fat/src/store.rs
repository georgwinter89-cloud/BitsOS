//! UPD-4: `\BITSOS2\SYS\STORE\`, where a version's parts lie (its `\BITSOS2\bin` and its program trees, one folder
//! per part and content hash; the rules and names are kexec's `install::parts_rules`). Below `SYS` like the
//! generations, so only the SYS grant changes anything here: no program, no MCP file tool, only the kernel's
//! update node. The mechanism only: what is written, kept and removed is kexec's.

use crate::sysapi::Sys;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use uapi::errno::*;
use vfs::OpenFile;
use vfs::abi::{O_CREAT, O_EXCL, O_WRONLY};
use vfs::ops::Ctx;
use vfs::pagecache::SyncKind;

/// One name below STORE (`/`-separated); every component a plain name, never `.`, `..` or a backslash (the
/// caller checked the part's rules already: this keeps every path inside STORE whatever it was handed).
fn safe(rel: &str) -> Result<(), i64> {
    let ok = !rel.is_empty() && rel.split('/').all(|c| !c.is_empty() && c != "." && c != ".." && !c.contains('\\') && !c.contains('\0'));
    if ok { Ok(()) } else { Err(EINVAL) }
}

impl Sys<'_> {
    fn store(&self) -> String {
        format!("{}/BITSOS2/SYS/STORE", self.mount)
    }

    pub fn store_path(&self, rel: &str) -> Result<String, i64> {
        safe(rel)?;
        Ok(format!("{}/{rel}", self.store()))
    }

    /// STORE's entries: (name, is a folder). None made yet: empty.
    pub fn store_list(&self) -> Result<Vec<(String, bool)>, i64> {
        match self.ns.list_dir(&self.store()) {
            Ok(l) => Ok(l.into_iter().map(|(n, a)| (String::from_utf8_lossy(&n).into_owned(), a.mode & uapi::S_IFMT == uapi::S_IFDIR)).collect()),
            Err(ENOENT) => Ok(Vec::new()),
            Err(e) => Err(e),
        }
    }

    /// Makes `rel` and every folder above it inside STORE (and SYS, STORE themselves).
    pub fn store_mkdir(&self, rel: &str) -> Result<(), i64> {
        safe(rel)?;
        let _g = self.vol.grant_sys();
        let c = Ctx::kernel(self.ns);
        let mut at = format!("{}/BITSOS2/SYS", self.mount);
        for comp in core::iter::once("STORE").chain(rel.split('/')) {
            at.push('/');
            at.push_str(comp);
            match c.mkdir(None, at.as_bytes(), 0o755) {
                Ok(_) | Err(EEXIST) => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    /// A new file `rel` (EEXIST if it is there), open for writing with `store_write`.
    pub fn store_create(&self, rel: &str) -> Result<Arc<OpenFile>, i64> {
        let path = self.store_path(rel)?;
        let _g = self.vol.grant_sys();
        Ctx::kernel(self.ns).open(None, path.as_bytes(), O_WRONLY | O_CREAT | O_EXCL, 0o644)
    }

    /// `data` at `off` of a file `store_create` opened (all of it, or an error).
    pub fn store_write(&self, f: &OpenFile, off: u64, data: &[u8]) -> Result<(), i64> {
        let _g = self.vol.grant_sys();
        let mut done = 0;
        while done < data.len() {
            let w = f.pwrite(off + done as u64, &data[done..])?;
            if w == 0 {
                return Err(EIO);
            }
            done += w;
        }
        Ok(())
    }

    pub fn store_fsync(&self, f: &OpenFile) -> Result<(), i64> {
        let _g = self.vol.grant_sys();
        f.inode().ok_or(EIO)?.sync(SyncKind::Fsync)
    }

    /// A small file in one step (a part's record), durable.
    pub fn store_put(&self, rel: &str, data: &[u8]) -> Result<(), i64> {
        let f = self.store_create(rel)?;
        self.store_write(&f, 0, data)?;
        self.store_fsync(&f)?;
        drop(f);
        self.vol.sync(true)
    }

    pub fn store_read(&self, rel: &str, max: usize) -> Result<Vec<u8>, i64> {
        self.ns.read_file(&self.store_path(rel)?, max)
    }

    /// Renames `from` to `to` inside STORE (a sealed part's folder), durable.
    pub fn store_rename(&self, from: &str, to: &str) -> Result<(), i64> {
        let (a, b) = (self.store_path(from)?, self.store_path(to)?);
        let _g = self.vol.grant_sys();
        Ctx::kernel(self.ns).rename(None, a.as_bytes(), None, b.as_bytes(), 0)?;
        drop(_g);
        self.vol.sync(true)
    }

    /// Removes `rel` (a file, or a folder with everything in it), durable; gone already is no error.
    pub fn store_remove(&self, rel: &str) -> Result<(), i64> {
        let path = self.store_path(rel)?;
        let _g = self.vol.grant_sys();
        let c = Ctx::kernel(self.ns);
        // Depth first with an explicit stack: a tree's depth is whatever it brought.
        let mut stack: Vec<(String, bool)> = alloc::vec![(path, false)];
        while let Some((p, listed)) = stack.pop() {
            let dir = match self.ns.lookup_path(p.as_bytes(), false) {
                Ok(found) => found.dentry.is_dir(),
                Err(ENOENT) => continue,
                Err(e) => return Err(e),
            };
            if !dir {
                c.unlink(None, p.as_bytes())?;
            } else if listed {
                c.rmdir(None, p.as_bytes())?;
            } else {
                let children = self.ns.list_dir(&p)?;
                stack.push((p.clone(), true));
                for (n, _) in children {
                    stack.push((format!("{p}/{}", String::from_utf8_lossy(&n)), false));
                }
            }
        }
        drop(_g);
        self.vol.sync(true)
    }

    /// `\BITSOS2\<name>` as a folder (where the kernel binds a part): made when missing; ENOTDIR when a file is
    /// there. A plain folder below `\BITSOS2\` (FS-7's rules decide, as for any other).
    pub fn mount_point(&self, name: &str) -> Result<(), i64> {
        if name.is_empty() || name.contains('/') || name.contains('\\') || name == "." || name == ".." {
            return Err(EINVAL);
        }
        let path = format!("{}/BITSOS2/{name}", self.mount);
        match self.ns.lookup_path(path.as_bytes(), false) {
            Ok(p) if p.dentry.is_dir() => Ok(()),
            Ok(_) => Err(ENOTDIR),
            Err(ENOENT) => {
                Ctx::kernel(self.ns).mkdir(None, path.as_bytes(), 0o755)?;
                self.vol.sync(true)
            }
            Err(e) => Err(e),
        }
    }
}
