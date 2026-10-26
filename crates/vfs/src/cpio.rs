//! The initramfs: cpio "newc" (070701/070702) archives unpacked into tmpfs at boot, the way Linux's
//! init/initramfs.c does it: concatenated archives (each ended by `TRAILER!!!`, zero padding between them),
//! directories, regular files (an existing node of another type is replaced), symlinks, hard links (entries
//! with nlink >= 2 and the same inode share one inode; the entry carrying the data fills it), mode, owner and
//! mtime from the header. FS-3: FIFOs, socket names and device nodes (by number, `init_mknod`) are made too.
//! Everything is copied, so
//! the module's memory may be released afterwards. Hostile archives end with an error, never a panic.

use crate::abi::Time;
use crate::mount_ns::MountNs;
use crate::ops::Ctx;
use crate::Errno;
use alloc::collections::BTreeMap;
use alloc::vec::Vec;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CpioError {
    Truncated,
    BadMagic,
    BadNumber,
    /// gzip/xz/zstd archives: V2 builds uncompressed ones.
    Compressed,
}

/// What an unpack did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Unpacked {
    pub files: usize,
    pub dirs: usize,
    pub symlinks: usize,
    pub hardlinks: usize,
    /// FIFOs, socket names and device nodes (FS-3).
    pub nodes: usize,
    /// Entries that could not be placed (bad names, a directory in the way).
    pub skipped: usize,
    pub bytes: u64,
}

const HEADER: usize = 110;

fn hex(b: &[u8]) -> Result<u32, CpioError> {
    let s = core::str::from_utf8(b).map_err(|_| CpioError::BadNumber)?;
    u32::from_str_radix(s, 16).map_err(|_| CpioError::BadNumber)
}

struct Entry<'a> {
    ino: u32,
    mode: u32,
    uid: u32,
    gid: u32,
    nlink: u32,
    mtime: u32,
    dev: (u32, u32),
    /// The device number of a device node (`c_rdevmajor`, `c_rdevminor`).
    rdev: (u32, u32),
    name: &'a [u8],
    body: &'a [u8],
}

/// Parses the entry at `off`; returns it and the offset of the next one.
fn entry(data: &[u8], off: usize) -> Result<(Entry<'_>, usize), CpioError> {
    let h = data.get(off..off.checked_add(HEADER).ok_or(CpioError::Truncated)?).ok_or(CpioError::Truncated)?;
    if &h[0..6] != b"070701" && &h[0..6] != b"070702" {
        return Err(CpioError::BadMagic);
    }
    let f = |i: usize| hex(&h[6 + i * 8..14 + i * 8]);
    let filesize = f(6)? as usize;
    let namesize = f(11)? as usize;
    if namesize == 0 || namesize > crate::abi::PATH_MAX {
        return Err(CpioError::BadNumber);
    }
    let name_start = off + HEADER;
    let name_end = name_start.checked_add(namesize).ok_or(CpioError::Truncated)?;
    let name = data.get(name_start..name_end - 1).ok_or(CpioError::Truncated)?;
    let body_start = name_end.checked_add(3).ok_or(CpioError::Truncated)? & !3;
    let body_end = body_start.checked_add(filesize).ok_or(CpioError::Truncated)?;
    let body = data.get(body_start..body_end).ok_or(CpioError::Truncated)?;
    let next = body_end.checked_add(3).ok_or(CpioError::Truncated)? & !3;
    let e = Entry { ino: f(0)?, mode: f(1)?, uid: f(2)?, gid: f(3)?, nlink: f(4)?, mtime: f(5)?, dev: (f(7)?, f(8)?), rdev: (f(9)?, f(10)?), name, body };
    Ok((e, next))
}

struct Errors(usize);

impl Errors {
    fn note(&mut self, name: &[u8], what: &str, e: Errno) {
        self.0 += 1;
        // A bound for the log, not for the archive: the count covers the rest.
        if self.0 <= 8 {
            klog::event!(Warn, "vfs", "initramfs.entry_skipped", name = &*alloc::string::String::from_utf8_lossy(name), what = what, errno = e);
        }
    }
}

/// Unpacks every archive in `data` into `ns`. On a malformed archive the entries before it stay.
pub fn unpack(data: &[u8], ns: &MountNs) -> Result<Unpacked, CpioError> {
    if data.starts_with(&[0x1f, 0x8b]) || data.starts_with(&[0xfd, b'7', b'z', b'X', b'Z']) || data.starts_with(&[0x28, 0xb5, 0x2f, 0xfd]) {
        return Err(CpioError::Compressed);
    }
    let c = Ctx::kernel(ns);
    let mut st = Unpacked::default();
    let mut errs = Errors(0);
    // Hard links of the current archive: (ino, dev major, dev minor, mode) -> first name.
    let mut links: BTreeMap<(u32, u32, u32, u32), Vec<u8>> = BTreeMap::new();
    let mut off = 0usize;
    loop {
        // Zero padding between concatenated archives (4-byte steps, as Linux).
        while off + 4 <= data.len() && data[off..off + 4] == [0; 4] {
            off += 4;
        }
        if off >= data.len() || data[off..].iter().all(|&b| b == 0) {
            break;
        }
        let (e, next) = entry(data, off)?;
        off = next;
        if e.name == b"TRAILER!!!" {
            links.clear();
            continue;
        }
        if e.name.is_empty() {
            continue;
        }
        let mut path = alloc::vec![b'/'];
        path.extend_from_slice(e.name);
        place(&c, ns, &e, &path, &mut links, &mut st, &mut errs);
    }
    st.skipped = errs.0;
    Ok(st)
}

fn set_meta(d: &crate::dentry::Dentry, e: &Entry<'_>) {
    d.inode.meta.set_owner(e.uid, e.gid);
    d.inode.meta.set_perm(e.mode & 0o7777);
    d.inode.meta.set_mtime(Time { sec: e.mtime as i64, nsec: 0 });
}

fn place(c: &Ctx<'_>, ns: &MountNs, e: &Entry<'_>, path: &[u8], links: &mut BTreeMap<(u32, u32, u32, u32), Vec<u8>>, st: &mut Unpacked, errs: &mut Errors) {
    match e.mode & uapi::S_IFMT {
        uapi::S_IFDIR => {
            let d = match c.lookup(None, path, false) {
                Ok(p) if p.dentry.is_dir() => Ok(p.dentry),
                _ => ns.mkdir_p_bytes(path, e.mode & 0o7777),
            };
            match d {
                Ok(d) => {
                    set_meta(&d, e);
                    st.dirs += 1;
                }
                Err(err) => errs.note(e.name, "dir", err),
            }
        }
        uapi::S_IFREG => {
            if e.nlink >= 2 {
                let key = (e.ino, e.dev.0, e.dev.1, e.mode);
                if let Some(first) = links.get(&key).cloned() {
                    let _ = ns.remove(core::str::from_utf8(path).unwrap_or(""));
                    let r = c.link(None, &first, None, path, false).and_then(|_| {
                        if e.body.is_empty() {
                            return Ok(());
                        }
                        let p = c.lookup(None, path, false)?;
                        p.dentry.inode.truncate(0)?;
                        p.dentry.inode.write_at(0, e.body).map(|_| ())
                    });
                    match r {
                        Ok(()) => {
                            st.hardlinks += 1;
                            st.bytes += e.body.len() as u64;
                        }
                        Err(err) => errs.note(e.name, "hardlink", err),
                    }
                    return;
                }
                links.insert(key, path.to_vec());
            }
            match ns.create_file_bytes(path, e.mode, e.body) {
                Ok(d) => {
                    set_meta(&d, e);
                    st.files += 1;
                    st.bytes += e.body.len() as u64;
                }
                Err(err) => errs.note(e.name, "file", err),
            }
        }
        uapi::S_IFLNK => {
            let target = e.body.strip_suffix(&[0]).unwrap_or(e.body);
            let r = core::str::from_utf8(path).map_err(|_| uapi::errno::EINVAL).and_then(|p| {
                let t = core::str::from_utf8(target).map_err(|_| uapi::errno::EINVAL)?;
                ns.create_symlink(p, t)
            });
            match r {
                Ok(d) => {
                    d.inode.meta.set_owner(e.uid, e.gid);
                    st.symlinks += 1;
                }
                Err(err) => errs.note(e.name, "symlink", err),
            }
        }
        uapi::S_IFIFO => match ns.create_fifo(path, e.mode) {
            Ok(d) => {
                set_meta(&d, e);
                st.nodes += 1;
            }
            Err(err) => errs.note(e.name, "fifo", err),
        },
        uapi::S_IFCHR | crate::abi::S_IFBLK | crate::abi::S_IFSOCK => {
            let rdev = if e.mode & uapi::S_IFMT == crate::abi::S_IFSOCK { 0 } else { crate::abi::encode_dev(e.rdev.0 & 0xfff, e.rdev.1 & 0xf_ffff) };
            match ns.create_special(path, e.mode & (uapi::S_IFMT | 0o7777), rdev) {
                Ok(d) => {
                    set_meta(&d, e);
                    st.nodes += 1;
                }
                Err(err) => errs.note(e.name, "device node", err),
            }
        }
        _ => errs.note(e.name, "unknown file type", uapi::errno::EINVAL),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    extern crate std;
    use std::vec::Vec;

    pub(crate) fn entry_full(out: &mut Vec<u8>, name: &str, mode: u32, body: &[u8], ino: u32, nlink: u32) {
        let hdr = std::format!(
            "070701{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}",
            ino, mode, 7, 8, nlink, 1234, body.len(), 0, 0, 0, 0, name.len() + 1, 0
        );
        out.extend_from_slice(hdr.as_bytes());
        out.extend_from_slice(name.as_bytes());
        out.push(0);
        while out.len() % 4 != 0 {
            out.push(0);
        }
        out.extend_from_slice(body);
        while out.len() % 4 != 0 {
            out.push(0);
        }
    }
    fn entry(out: &mut Vec<u8>, name: &str, mode: u32, body: &[u8]) {
        entry_full(out, name, mode, body, 1, 1);
    }

    #[test]
    fn unpacks_and_rejects_garbage() {
        let mut a = Vec::new();
        entry(&mut a, "bin", 0o040755, b"");
        entry(&mut a, "bin/hello", 0o100755, b"ELF!");
        entry(&mut a, "sbin", 0o120777, b"bin");
        entry(&mut a, "dev/null", 0o020666, b"");
        entry(&mut a, "p", 0o010640, b"");
        entry_full(&mut a, "h1", 0o100644, b"", 50, 2);
        entry_full(&mut a, "h2", 0o100644, b"shared", 50, 2);
        entry(&mut a, "TRAILER!!!", 0, b"");
        a.extend_from_slice(&[0; 512]);
        // A second archive appended (what tools that add files to an initramfs do).
        entry(&mut a, "./etc/motd", 0o100600, b"hi");
        entry(&mut a, "bin/hello", 0o100700, b"ELF2");
        entry(&mut a, "TRAILER!!!", 0, b"");
        let ns = MountNs::new_empty();
        let st = unpack(&a, &ns).unwrap();
        assert_eq!((st.files, st.dirs, st.symlinks, st.hardlinks, st.nodes, st.skipped), (4, 1, 1, 1, 2, 0));
        let p = ns.lookup("/p").unwrap();
        assert_eq!(p.inode.meta.mode(), uapi::S_IFIFO | 0o640);
        assert_eq!(ns.lookup("/dev/null").unwrap().inode.meta.mode(), 0o020666);
        let f = ns.open("/sbin/hello").unwrap();
        let mut b = [0u8; 4];
        assert_eq!(f.read_at(0, &mut b).unwrap(), 4);
        assert_eq!(&b, b"ELF2");
        let h = ns.lookup("/bin/hello").unwrap();
        assert_eq!(h.inode.meta.mode(), 0o100700);
        assert_eq!((h.inode.meta.uid(), h.inode.meta.gid()), (7, 8));
        let h1 = ns.lookup("/h1").unwrap();
        let h2 = ns.lookup("/h2").unwrap();
        assert!(Arc::ptr_eq(&h1.inode, &h2.inode));
        assert_eq!(h1.inode.meta.nlink(), 2);
        assert_eq!(ns.read_file("/h1", 100).unwrap(), b"shared");
        assert_eq!(ns.read_file("/etc/motd", 100).unwrap(), b"hi");
        assert_eq!(unpack(&a[..50], &ns).err(), Some(CpioError::Truncated));
        assert_eq!(unpack(&[b'x'; 200], &ns).err(), Some(CpioError::BadMagic));
        assert_eq!(unpack(&[0x1f, 0x8b, 8, 0], &ns).err(), Some(CpioError::Compressed));
    }

    use alloc::sync::Arc;

    #[test]
    fn hostile_archives_never_panic() {
        let mut a = Vec::new();
        entry(&mut a, "d", 0o040755, b"");
        entry(&mut a, "d/f", 0o100644, b"0123456789");
        entry(&mut a, "../../x", 0o100644, b"up");
        entry(&mut a, "l", 0o120777, b"/l");
        entry(&mut a, "l/x", 0o100644, b"loop");
        entry(&mut a, "TRAILER!!!", 0, b"");
        let ns = MountNs::new_empty();
        // "../../x" stays inside the root (".." at the root is the root, as Linux).
        assert_eq!(unpack(&a, &ns).unwrap().files, 2);
        assert_eq!(ns.read_file("/x", 10).unwrap(), b"up");
        let mut seed = 0x1234_5678u32;
        for round in 0..3000 {
            let mut b = a.clone();
            for _ in 0..1 + round % 5 {
                seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12345);
                let i = (seed as usize >> 8) % b.len();
                b[i] = (seed >> 24) as u8;
            }
            if round % 7 == 0 {
                b.truncate((seed as usize) % b.len());
            }
            let ns = MountNs::new_empty();
            let _ = unpack(&b, &ns);
        }
    }
}
