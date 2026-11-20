//! EINS-C (K6) in the kernel: the assistant tree's trash on a fresh ext4 volume (made by V2's mkfs on a memory
//! disk, mounted in a view of its own at /data), with the kernel's locks and lockdep. All Safe and rerunnable.
//!
//! - `fs.ext4.trash`: unlink, rmdir, rename over a name, `rm -r` as one entry that comes back whole (and not
//!   into a folder the caller may not write), the tree's own files removed for real, a hard link, outside the tree
//!   everything real; no new lock-order violation.
//! - `fs.ext4.trash_full`: a nearly full volume refuses the tree's removal with ENOSPC and keeps the file.
//! - `fs.ext4.trash_calls`: every removing call through its syscall entry point (`vfs::sys`).

use crate::mkfs::{self, MkfsOptions};
use crate::{mount, MemDev, Options};
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use ktest::{kassert, ktest, TestResult};
use uapi::errno::*;
use vfs::abi::{O_CREAT, O_RDWR, O_WRONLY};
use vfs::inode::Cred;
use vfs::ops::Ctx;

fn e<T: core::fmt::Debug>(x: T) -> String {
    format!("{x:?}")
}

fn volume(mb: usize) -> Result<(vfs::MountNs, Arc<crate::Volume>), String> {
    let dev = Arc::new(MemDev::new(vec![0u8; mb << 20]));
    let mut o = MkfsOptions::new(b"trash", [7; 16], [9; 16], vfs::inode::now());
    o.zeroed = true;
    mkfs::format(&*dev, &o).map_err(e)?;
    let ns = vfs::MountNs::new_empty();
    let (v, _m) = mount::mount_dev(dev, None, &ns, "/data", Options::default()).map_err(e)?;
    Ok((ns, v))
}

fn as_proc(ns: &vfs::MountNs, pid: u32, tree: u32) -> Ctx<'_> {
    let mut c = Ctx::kernel(ns);
    c.cred = Cred { uid: 1000, gid: 1000 };
    c.umask = 0o022;
    c.subject = policy::Subject { program: policy::ProgramId(1), pid, uid: 1000, tree };
    c
}

fn georgs(ns: &vfs::MountNs, rels: &[&str]) -> TestResult {
    let k = Ctx::kernel(ns);
    let own = |p: &str| -> TestResult {
        let at = ns.lookup_path(p.as_bytes(), false).map_err(e)?;
        k.setattr(&at, |m| vfs::setattr::chown(m, Cred::ROOT, 1000, 1000)).map_err(e)
    };
    for rel in rels {
        let path = format!("/data/home/1000/{rel}");
        let (dir, _) = path.rsplit_once('/').ok_or("no dir")?;
        ns.mkdir_p(dir).map_err(e)?;
        let mut d = String::from("/data");
        for c in dir.split('/').skip(2) {
            d.push('/');
            d.push_str(c);
            own(&d)?;
        }
        ns.create_file_bytes(path.as_bytes(), 0o644, rel.as_bytes()).map_err(e)?;
        own(&path)?;
    }
    Ok(())
}

fn entries(ns: &vfs::MountNs) -> Vec<Vec<u8>> {
    ns.list_dir("/data/.papierkorb").map(|l| l.into_iter().map(|(n, _)| n).collect()).unwrap_or_default()
}

fn no_new_violations(f: impl FnOnce() -> TestResult) -> TestResult {
    let before = ksync::lockdep::counts().unexpected;
    f()?;
    let after = ksync::lockdep::counts().unexpected;
    kassert!(after == before, "{} new lock-order violations: {:?}", after - before, ksync::lockdep::list());
    Ok(())
}

fn trash() -> TestResult {
    no_new_violations(|| {
        let tree = policy::tree::new(0x7e10_0001, 0).ok_or("no tree")?;
        let (ns, v) = volume(24)?;
        georgs(&ns, &["proj/a", "proj/sub/b", "proj/sub/c", "one", "conf", "conf.new"])?;
        let t = as_proc(&ns, 0x7e10_0001, tree.id);
        for p in ["proj/sub/b", "proj/sub/c"] {
            t.unlink(None, format!("/data/home/1000/{p}").as_bytes()).map_err(e)?;
        }
        t.rmdir(None, b"/data/home/1000/proj/sub").map_err(e)?;
        t.unlink(None, b"/data/home/1000/proj/a").map_err(e)?;
        t.rmdir(None, b"/data/home/1000/proj").map_err(e)?;
        let ids = entries(&ns);
        kassert!(ids.len() == 1, "rm -r gave {} entries", ids.len());
        let id = String::from_utf8_lossy(&ids[0]).into_owned();
        let rec = ns.read_file(&format!("/data/.papierkorb/{id}/record.json"), 4096).map_err(e)?;
        kassert!(String::from_utf8_lossy(&rec).contains("\"path\":\"/data/home/1000/proj\""), "record {:?}", String::from_utf8_lossy(&rec));
        let georg = vfs::trash::Caller { pid: 0x7e10_0002, tree: 0, cred: Cred { uid: 1000, gid: 1000 }, program: policy::ProgramId(1) };
        // a restore goes only where the caller may make the name: not into root's folder, not into the trash
        Ctx::kernel(&ns).mkdir(None, b"/data/locked", 0o755).map_err(e)?;
        for (to, want) in [(&b"/data/locked/proj"[..], EACCES), (b"/data/.papierkorb/proj", EINVAL)] {
            let got = vfs::trash::restore(&ns, &georg, b"/data", id.as_bytes(), Some(to)).map(|r| r.moved);
            kassert!(got == Err(want), "restore-to {} gave {got:?}", String::from_utf8_lossy(to));
        }
        kassert!(vfs::trash::restore(&ns, &georg, b"/data", id.as_bytes(), None).map_err(e)?.moved == 1, "proj back whole");
        kassert!(ns.read_file("/data/home/1000/proj/sub/c", 100).map_err(e)? == b"proj/sub/c", "c is back");
        kassert!(entries(&ns).is_empty(), "the entry is gone after the restore");
        // a rename over a name keeps the old file; the tree's own file goes for real
        t.rename(None, b"/data/home/1000/conf.new", None, b"/data/home/1000/conf", 0).map_err(e)?;
        drop(t.open(None, b"/data/home/1000/tmp", O_WRONLY | O_CREAT, 0o600).map_err(e)?);
        t.unlink(None, b"/data/home/1000/tmp").map_err(e)?;
        let ids = entries(&ns);
        kassert!(ids.len() == 1, "{} entries after rename + own file", ids.len());
        let id = String::from_utf8_lossy(&ids[0]).into_owned();
        kassert!(ns.read_file(&format!("/data/.papierkorb/{id}/files/home/1000/conf"), 100).map_err(e)? == b"conf", "the old conf is kept");
        kassert!(ns.list_dir(&format!("/data/.papierkorb/{id}/files/home/1000")).map_err(e)?.len() == 1, "the own temp file is not kept");
        // a second name keeps the data: removed for real
        Ctx::kernel(&ns).link(None, b"/data/home/1000/one", None, b"/data/home/1000/two", false).map_err(e)?;
        vfs::trash::end_batches_of(0x7e10_0001);
        t.unlink(None, b"/data/home/1000/one").map_err(e)?;
        kassert!(entries(&ns).len() == 1, "a hard link went to the trash");
        // the tree may not empty, Georg may
        let inside = vfs::trash::Caller { tree: tree.id, ..georg };
        kassert!(vfs::trash::empty(&ns, &inside, b"/data", None) == Err(EPERM), "the tree emptied the trash");
        kassert!(vfs::trash::empty(&ns, &georg, b"/data", None) == Ok(1), "Georg empties");
        // outside the tree: real
        as_proc(&ns, 0x7e10_0003, 0).unlink(None, b"/data/home/1000/two").map_err(e)?;
        kassert!(entries(&ns).is_empty(), "outside the tree a removal went to the trash");
        let f = Ctx::kernel(&ns).open(None, b"/data/home/1000/conf", O_RDWR, 0).map_err(e)?;
        drop(f);
        v.sync_fs().map_err(e)?;
        Ok(())
    })
}
ktest!("fs.ext4.trash", Safe, trash);

fn trash_full() -> TestResult {
    let tree = policy::tree::new(0x7e10_0004, 0).ok_or("no tree")?;
    let (ns, _v) = volume(8)?;
    georgs(&ns, &["keep"])?;
    let k = Ctx::kernel(&ns);
    let f = k.open(None, b"/data/fill", O_WRONLY | O_CREAT, 0o600).map_err(e)?;
    let block = vec![0x5Au8; 128 * 1024];
    let mut at = 0u64;
    while f.pwrite(at, &block).is_ok_and(|n| n == block.len()) {
        at += block.len() as u64;
    }
    drop(f);
    let t = as_proc(&ns, 0x7e10_0004, tree.id);
    kassert!(t.unlink(None, b"/data/home/1000/keep").err() == Some(ENOSPC), "a full volume took the removal");
    kassert!(ns.read_file("/data/home/1000/keep", 100).map_err(e)? == b"keep", "the file was lost");
    k.unlink(None, b"/data/fill").map_err(e)?;
    t.unlink(None, b"/data/home/1000/keep").map_err(e)?;
    kassert!(entries(&ns).len() == 1, "with room again the removal went to the trash");
    Ok(())
}
ktest!("fs.ext4.trash_full", Safe, trash_full);

/// A program's memory for the calls' path arguments: NUL-terminated strings at `AT`.
struct Mem(Vec<u8>);
const AT: u64 = 0x1000_0000;

impl Mem {
    fn of(paths: &[&str]) -> (Mem, Vec<u64>) {
        let (mut b, mut at) = (Vec::new(), Vec::new());
        for p in paths {
            at.push(AT + b.len() as u64);
            b.extend_from_slice(p.as_bytes());
            b.push(0);
        }
        (Mem(b), at)
    }
}

impl vfs::sys::UserMem for Mem {
    fn read(&self, addr: u64, buf: &mut [u8]) -> bool {
        let o = addr.wrapping_sub(AT) as usize;
        match self.0.get(o..o.saturating_add(buf.len())) {
            Some(s) => {
                buf.copy_from_slice(s);
                true
            }
            None => false,
        }
    }
    fn write(&self, _addr: u64, _data: &[u8]) -> bool {
        false
    }
    fn read_cstr(&self, addr: u64, max: usize) -> Result<Vec<u8>, vfs::Errno> {
        let rest = self.0.get(addr.wrapping_sub(AT) as usize..).ok_or(EFAULT)?;
        let n = rest.iter().take(max).position(|&b| b == 0).ok_or(ENAMETOOLONG)?;
        Ok(rest[..n].to_vec())
    }
}

/// Each removing call as a program makes it (`vfs::sys`, the syscalls' own entry points): unlink, unlinkat,
/// rmdir, unlinkat(AT_REMOVEDIR), rename, renameat (across directories, by dirfd), renameat2 over a name; each
/// answers 0 and leaves the old file in the trash. Shared with the host test `tests::trash::sys_variants`.
pub(crate) fn sys_variants(ns: &Arc<vfs::MountNs>, tree: u32) -> TestResult {
    georgs(ns, &["u/a", "u/b", "u/d1/x", "u/d2/x", "u/r/old", "u/r/new", "u/s/old", "u/t/new", "u/q/old", "u/q/new"])?;
    let k = Ctx::kernel(ns);
    for d in ["/data/home/1000/u/d1/x", "/data/home/1000/u/d2/x"] {
        k.unlink(None, d.as_bytes()).map_err(e)?;
    }
    let fds = vfs::FdTable::new();
    let fs = vfs::FsState::new();
    let dir_s = k.open(None, b"/data/home/1000/u/s", vfs::abi::O_RDONLY | vfs::abi::O_DIRECTORY, 0).map_err(e)?;
    let dir_t = k.open(None, b"/data/home/1000/u/t", vfs::abi::O_RDONLY | vfs::abi::O_DIRECTORY, 0).map_err(e)?;
    let dir_u = k.open(None, b"/data/home/1000/u", vfs::abi::O_RDONLY | vfs::abi::O_DIRECTORY, 0).map_err(e)?;
    let (fs_, ft, fu) = (fds.install(dir_s).map_err(e)? as u64, fds.install(dir_t).map_err(e)? as u64, fds.install(dir_u).map_err(e)? as u64);
    let (mem, p) = Mem::of(&["/data/home/1000/u/a", "b", "/data/home/1000/u/d1", "d2", "/data/home/1000/u/r/new", "/data/home/1000/u/r/old", "old", "new", "q/new", "q/old"]);
    let s = vfs::sys::Sys { mem: &mem, fds: &fds, fs: &fs, ns, cred: Cred { uid: 1000, gid: 1000 }, subject: policy::Subject { program: policy::ProgramId(1), pid: 0x7e10_0005, uid: 1000, tree } };
    let calls: [(&str, i64); 7] = [
        ("unlink", vfs::sys::unlink(&s, [p[0], 0, 0, 0, 0, 0])),
        ("unlinkat", vfs::sys::unlinkat(&s, [fu, p[1], 0, 0, 0, 0])),
        ("rmdir", vfs::sys::rmdir(&s, [p[2], 0, 0, 0, 0, 0])),
        ("unlinkat(AT_REMOVEDIR)", vfs::sys::unlinkat(&s, [fu, p[3], vfs::abi::AT_REMOVEDIR as u64, 0, 0, 0])),
        ("rename", vfs::sys::rename(&s, [p[4], p[5], 0, 0, 0, 0])),
        ("renameat across directories", vfs::sys::renameat(&s, [ft, p[7], fs_, p[6], 0, 0])),
        ("renameat2", vfs::sys::renameat2(&s, [fu, p[8], fu, p[9], 0, 0])),
    ];
    for (what, r) in calls {
        kassert!(r == 0, "{what} = {r}");
    }
    vfs::trash::end_batches_of(0x7e10_0005);
    let ids = entries(ns);
    kassert!(ids.len() == 1, "{} entries for one process's calls", ids.len());
    let base = format!("/data/.papierkorb/{}/files/home/1000/u", String::from_utf8_lossy(&ids[0]));
    for (kept, was) in [("a", "u/a"), ("b", "u/b"), ("r/old", "u/r/old"), ("s/old", "u/s/old"), ("q/old", "u/q/old")] {
        kassert!(ns.read_file(&format!("{base}/{kept}"), 100).map_err(e)? == was.as_bytes(), "{kept} is not in the trash");
    }
    for d in ["d1", "d2"] {
        kassert!(ns.lookup(&format!("{base}/{d}")).map_err(e)?.is_dir(), "{d} is not in the trash");
    }
    kassert!(ns.read_file("/data/home/1000/u/r/old", 100).map_err(e)? == b"u/r/new", "rename did not replace");
    kassert!(ns.read_file("/data/home/1000/u/s/old", 100).map_err(e)? == b"u/t/new", "renameat did not replace");
    Ok(())
}

fn trash_calls() -> TestResult {
    let tree = policy::tree::new(0x7e10_0005, 0).ok_or("no tree")?;
    let (ns, _v) = volume(16)?;
    sys_variants(&Arc::new(ns), tree.id)
}
ktest!("fs.ext4.trash_calls", Safe, trash_calls);
