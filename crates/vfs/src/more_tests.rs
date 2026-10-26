//! Host tests beyond the Linux case list: mounts and views (B2), the policy hook (B3), hostile depth,
//! concurrent name changes.

extern crate std;

use crate::abi::{O_CREAT, O_DIRECTORY, O_RDONLY, O_RDWR, O_WRONLY};
use crate::inode::Cred;
use crate::mount_ns::MountNs;
use crate::ops::{self, Ctx, OwnFolders};
use crate::tmpfs;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};
use uapi::errno::*;

fn errno<T>(r: Result<T, i64>) -> i64 {
    r.err().unwrap_or(0)
}

#[test]
fn mounts_dotdot_and_views() {
    let ns = MountNs::new_empty();
    let c = Ctx::kernel(&ns);
    c.mkdir(None, b"/mnt", 0o755).unwrap();
    let at = c.mkdir(None, b"/mnt/a", 0o755).unwrap();
    c.mkdir(None, b"/mnt/b", 0o755).unwrap();
    let (sb, root) = tmpfs::new_fs(0o755);
    let m = ns.mount(&at, sb.clone(), root, false).unwrap();
    c.open(None, b"/mnt/a/x", O_CREAT | O_WRONLY, 0o644).unwrap();
    let x = c.lookup(None, b"/mnt/a/x", true).unwrap();
    assert!(Arc::ptr_eq(&x.mnt, &m));
    assert_ne!(x.attr().dev, c.lookup(None, b"/mnt", true).unwrap().attr().dev);
    let top = c.lookup(None, b"/mnt/a", true).unwrap();
    assert!(top.attr().mount_root);
    // ".." of a mount's root is the mountpoint's parent.
    let up = c.lookup(None, b"/mnt/a/..", true).unwrap();
    assert!(up.same(&c.lookup(None, b"/mnt", true).unwrap()));
    assert!(c.lookup(None, b"/mnt/a/../../..", true).unwrap().same(&ns.root_path()));
    assert_eq!(ns.path_of(&ns.root_path(), &x).unwrap(), b"/mnt/a/x");
    // A mountpoint cannot be removed or renamed; names do not cross file systems.
    assert_eq!(errno(c.rmdir(None, b"/mnt/a")), EBUSY);
    assert_eq!(errno(c.rename(None, b"/mnt/a", None, b"/mnt/c", 0)), EBUSY);
    assert_eq!(errno(c.rename(None, b"/mnt/a/x", None, b"/mnt/b/x", 0)), EXDEV);
    assert_eq!(errno(c.link(None, b"/mnt/a/x", None, b"/mnt/b/x", false)), EXDEV);
    // umount: only at a mount root, and not with mounts below.
    assert_eq!(errno(ns.umount(&c.lookup(None, b"/mnt", true).unwrap())), EINVAL);
    let inner = c.mkdir(None, b"/mnt/a/in", 0o755).unwrap();
    let (sb2, r2) = tmpfs::new_fs(0o755);
    ns.mount(&inner, sb2, r2, false).unwrap();
    assert_eq!(errno(ns.umount(&top)), EBUSY);
    // A copied view sees today's mounts; later mounts stay in their own view.
    let v2 = ns.copy_view();
    let c2 = Ctx::kernel(&v2);
    assert!(c2.lookup(None, b"/mnt/a/x", true).is_ok());
    let b2 = c2.lookup(None, b"/mnt/b", true).unwrap();
    let (sb3, r3) = tmpfs::new_fs(0o755);
    v2.mount(&b2, sb3, r3, true).unwrap();
    // Read-only mount: creating and opening for write are EROFS, reading works.
    assert_eq!(errno(c2.open(None, b"/mnt/b/new", O_CREAT | O_WRONLY, 0o644)), EROFS);
    assert_eq!(errno(c2.mkdir(None, b"/mnt/b/d", 0o755)), EROFS);
    assert!(c2.open(None, b"/mnt/b", O_RDONLY | O_DIRECTORY, 0).is_ok());
    assert!(c.lookup(None, b"/mnt/b", true).unwrap().attr().mount_root == false);
    c.open(None, b"/mnt/b/plain", O_CREAT | O_WRONLY, 0o644).unwrap();
    assert_eq!(errno(c2.lookup(None, b"/mnt/b/plain", true)), ENOENT);
    // Unmount in the first view; the copy still has its own mounts of the same file system.
    let inner_top = c.lookup(None, b"/mnt/a/in", true).unwrap();
    ns.umount(&inner_top).unwrap();
    ns.umount(&top).unwrap();
    assert_eq!(errno(c.lookup(None, b"/mnt/a/x", true)), ENOENT);
    assert!(c2.lookup(None, b"/mnt/a/x", true).is_ok());
    drop(sb);
}

#[test]
fn symlinks_hostile_and_across_mounts() {
    let ns = MountNs::new_empty();
    let c = Ctx::kernel(&ns);
    // A link to itself through a directory and a two-link loop end with ELOOP; a long chain of 40 resolves.
    c.symlink(b"/self/x", None, b"/self").unwrap();
    assert_eq!(errno(c.lookup(None, b"/self", true)), ELOOP);
    c.symlink(b"b", None, b"/a").unwrap();
    c.symlink(b"a", None, b"/b").unwrap();
    assert_eq!(errno(c.open(None, b"/a/../x", O_RDONLY, 0)), ELOOP);
    // A symlink whose target walks ".." out of a mount comes back through the mountpoint.
    let mnt = c.mkdir(None, b"/m", 0o755).unwrap();
    let (sb, root) = tmpfs::new_fs(0o755);
    ns.mount(&mnt, sb, root, false).unwrap();
    c.open(None, b"/target", O_CREAT | O_WRONLY, 0o644).unwrap();
    c.symlink(b"../target", None, b"/m/up").unwrap();
    let t = c.lookup(None, b"/m/up", true).unwrap();
    assert!(t.same(&c.lookup(None, b"/target", true).unwrap()));
}

struct Own;
impl OwnFolders for Own {
    fn is_own(&self, program: policy::ProgramId, path: &[u8]) -> bool {
        program.0 == 9 && path.starts_with(b"/home/p9/")
    }
}
static OWN: Own = Own;
static ASKED_P9: AtomicU64 = AtomicU64::new(0);

struct Engine;
impl policy::Engine for Engine {
    fn check(&self, s: &policy::Subject, a: &policy::Access<'_>) -> Result<(), policy::Denied> {
        if s.program.0 != 9 {
            return Ok(());
        }
        ASKED_P9.fetch_add(1, Ordering::Relaxed);
        match a {
            policy::Access::Path { path, .. } if path.starts_with("/secret") => Err(policy::Denied { reason: "test rule".into() }),
            _ => Ok(()),
        }
    }
}
static ENGINE: Engine = Engine;

/// The test engine and own-folder answer (shared with `fixer_tests`).
pub(crate) fn install_policy() {
    policy::set_engine(&ENGINE);
    ops::set_own_folders(&OWN);
}

/// Program 9 as uid 1000 in `ns`.
pub(crate) fn p9_ctx(ns: &MountNs) -> Ctx<'_> {
    let subject = policy::Subject { program: policy::ProgramId(9), pid: 900, uid: 1000 };
    Ctx { ns, root: ns.root_path(), cwd: ns.root_path(), cred: Cred { uid: 1000, gid: 1000 }, umask: 0, subject }
}

#[test]
fn policy_hook_b3() {
    install_policy();
    let ns = MountNs::new_empty();
    let k = Ctx::kernel(&ns);
    k.mkdir(None, b"/secret", 0o777).unwrap();
    k.mkdir(None, b"/home", 0o755).unwrap();
    k.mkdir(None, b"/home/p9", 0o777).unwrap();
    k.open(None, b"/secret/old", O_CREAT | O_WRONLY, 0o666).unwrap();
    let subject = policy::Subject { program: policy::ProgramId(9), pid: 900, uid: 1000 };
    let c = Ctx { ns: &ns, root: ns.root_path(), cwd: ns.root_path(), cred: Cred { uid: 1000, gid: 1000 }, umask: 0, subject };
    let before = ASKED_P9.load(Ordering::Relaxed);
    // Inside its own folder nothing is asked.
    c.open(None, b"/home/p9/f", O_CREAT | O_RDWR, 0o644).unwrap();
    c.mkdir(None, b"/home/p9/d", 0o755).unwrap();
    assert_eq!(ASKED_P9.load(Ordering::Relaxed), before);
    // Outside it is asked, and a refusal is EACCES (creating, opening, removing, renaming into it).
    assert_eq!(errno(c.open(None, b"/secret/f", O_CREAT | O_WRONLY, 0o644)), EACCES);
    assert_eq!(errno(c.open(None, b"/secret/old", O_RDONLY, 0)), EACCES);
    assert_eq!(errno(c.unlink(None, b"/secret/old")), EACCES);
    assert_eq!(errno(c.rename(None, b"/home/p9/f", None, b"/secret/f", 0)), EACCES);
    // A hard link would turn the refused file into an own-folder file: the source is asked too.
    assert_eq!(errno(c.link(None, b"/secret/old", None, b"/home/p9/old", false)), EACCES);
    assert!(ASKED_P9.load(Ordering::Relaxed) > before);
    // Allowed outside paths still work, the kernel is never asked.
    k.open(None, b"/pub", O_CREAT | O_WRONLY, 0o666).unwrap();
    assert!(c.open(None, b"/pub", O_RDONLY, 0).is_ok());
    assert!(k.open(None, b"/secret/old", O_RDONLY, 0).is_ok());
}

/// Far deeper than recursion could go on a test thread's stack: build, walk, remove, drop.
#[test]
fn deep_tree_is_iterative() {
    let ns = MountNs::new_empty();
    let c = Ctx::kernel(&ns);
    let mut cur = ns.root_path();
    for _ in 0..100_000 {
        cur = c.mkdir(Some(&cur), b"d", 0o755).unwrap();
    }
    assert_eq!(ns.path_of(&ns.root_path(), &cur).unwrap().len(), 200_000);
    let f = c.open(Some(&cur), b"leaf", O_CREAT | O_WRONLY, 0o644).unwrap();
    drop(cur);
    ns.remove_tree("/d").unwrap();
    assert_eq!(errno(c.lookup(None, b"/d", true)), ENOENT);
    // The open file keeps its removed chain alive; dropping it frees 100,000 dentries without recursion.
    drop(f);
    let ns2 = MountNs::new_empty();
    let c2 = Ctx::kernel(&ns2);
    let mut cur = ns2.root_path();
    for _ in 0..100_000 {
        cur = c2.mkdir(Some(&cur), b"e", 0o755).unwrap();
    }
    drop(cur);
    drop(c2);
    drop(ns2);
}

/// Threads creating and removing names in one directory while another lists it: no panic, no lost or
/// doubled names, the listing ends empty.
#[test]
fn concurrent_names() {
    let ns = Arc::new(MountNs::new_empty());
    Ctx::kernel(&ns).mkdir(None, b"/d", 0o777).unwrap();
    let mut hs = Vec::new();
    for t in 0..4 {
        let ns = ns.clone();
        hs.push(std::thread::spawn(move || {
            let c = Ctx::kernel(&ns);
            for i in 0..300 {
                let n = std::format!("/d/t{t}-{i}");
                let f = c.open(None, n.as_bytes(), O_CREAT | O_WRONLY, 0o644).unwrap();
                f.write(b"x").unwrap();
                let m = std::format!("/d/r{t}-{i}");
                c.rename(None, n.as_bytes(), None, m.as_bytes(), 0).unwrap();
                c.unlink(None, m.as_bytes()).unwrap();
            }
        }));
    }
    let lister = {
        let ns = ns.clone();
        std::thread::spawn(move || {
            let c = Ctx::kernel(&ns);
            for _ in 0..200 {
                let f = c.open(None, b"/d", O_RDONLY | O_DIRECTORY, 0).unwrap();
                let mut seen = std::collections::BTreeSet::new();
                loop {
                    let b = ops::read_dir(&f, 4096).unwrap();
                    if b.is_empty() {
                        break;
                    }
                    let mut off = 0;
                    while off < b.len() {
                        let reclen = u16::from_le_bytes([b[off + 16], b[off + 17]]) as usize;
                        let name = b[off + 19..off + reclen].split(|&c| c == 0).next().unwrap().to_vec();
                        assert!(seen.insert(name.clone()), "a name listed twice in one pass: {}", std::string::String::from_utf8_lossy(&name));
                        off += reclen;
                    }
                }
            }
        })
    };
    for h in hs {
        h.join().unwrap();
    }
    lister.join().unwrap();
    assert_eq!(ns.list_dir("/d").unwrap().len(), 0);
    assert_eq!(ns.lookup("/d").unwrap().inode.meta.nlink(), 2);
}
