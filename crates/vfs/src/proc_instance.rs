//! SX-3b: one proc super block per proc mount, bound to the mounter's pid namespace (linux-6.6/fs/proc/root.c:226
//! `proc_get_tree` = `get_tree_nodev`, `proc_fill_super` keeps `fc->pid_ns`; SX-3a decision 8 showed the boot
//! instance instead). The boot /proc (devfs::populate) and every instance are built from the same registries:
//! the `procfile!`/`procdir!` entries below /proc. A source that depends on the pid namespace (the process
//! directories, `self`, `thread-self`: crates/syscall procfs) answers `DirSource::for_pid_ns` with its form for
//! the instance's namespace; vfs knows the namespace only as an opaque object, its owner downcasts it.
//!
//! A mount of proc therefore never meets "the same file system already right there" (EBUSY), as on Linux:
//! bwrap's `--proc` over a bind of the host's /proc in a new pid namespace works.

use alloc::sync::Arc;
use core::any::Any;
use core::sync::atomic::{AtomicU64, Ordering};

use crate::inode::{Cred, Inode};
use crate::synth::{DirSource, SynthDir};
use crate::{Dentry, Errno, ProcDirEntry, ProcFileEntry, SuperBlock};
use uapi::errno::*;

/// Proc instances made since boot (the `proc.instance` event carries it: a program mounting proc in a loop shows).
static MADE: AtomicU64 = AtomicU64::new(0);

/// A new proc super block and its root for a mount in pid namespace `pid_ns`, from the registries.
pub fn instance(pid_ns: Arc<dyn Any + Send + Sync>) -> Result<(Arc<SuperBlock>, Arc<Dentry>), Errno> {
    instance_from(pid_ns, crate::PROCFILES.entries(), crate::PROCDIRS.entries())
}

/// `instance` with the lists given (host tests pass their own).
pub fn instance_from(
    pid_ns: Arc<dyn Any + Send + Sync>,
    files: &[ProcFileEntry],
    dirs: &[ProcDirEntry],
) -> Result<(Arc<SuperBlock>, Arc<Dentry>), Errno> {
    let sb = SuperBlock::new_fixed("proc", crate::dentry::Fixed::Proc);
    let root = Dentry::new_root(sb.clone(), Inode::new(sb.next_ino(), uapi::S_IFDIR | 0o555, Cred::ROOT, crate::inode::Data::SynthDir(Arc::new(SynthDir::new()))));
    let mut failed = 0usize;
    for f in files {
        let Some(rel) = below_proc(f.path) else { continue };
        if rel.is_empty() || make_file(&root, rel, f.render).is_err() {
            failed += 1;
        }
    }
    for d in dirs {
        let Some(rel) = below_proc(d.path) else { continue };
        let src = (d.make)();
        let src = src.for_pid_ns(&pid_ns).unwrap_or(src);
        if add_source(&root, rel, src).is_err() {
            failed += 1;
        }
    }
    if failed > 0 {
        klog::event!(Warn, "vfs", "proc.instance_partial", failed = failed; "some /proc nodes could not be made in a new proc instance");
    }
    // The cost of one instance is bounded by the registries (one node per `procfile!`/`procdir!` entry below
    // /proc plus their parent directories, fixed per kernel build); the process directories stay lazy sources.
    let made = MADE.fetch_add(1, Ordering::Relaxed) + 1;
    klog::event!(Info, "vfs", "proc.instance", dev = sb.dev, files = files.len(), dirs = dirs.len(), made = made);
    Ok((sb, root))
}

/// `path` below /proc ("" for /proc itself), None for paths elsewhere.
fn below_proc(path: &str) -> Option<&[u8]> {
    let p = path.as_bytes();
    let rest = p.strip_prefix(b"/proc")?;
    match rest {
        [] => Some(b""),
        [b'/', more @ ..] => Some(more),
        _ => None,
    }
}

/// The directory `rel` below `root` (components separated by '/'), made as needed (0755 as `MountNs::mkdir_p`).
fn dir_at(root: &Arc<Dentry>, rel: &[u8]) -> Result<Arc<Dentry>, Errno> {
    let mut cur = root.clone();
    for comp in rel.split(|&b| b == b'/').filter(|c| !c.is_empty()) {
        cur = match cur.child(comp) {
            Some(d) if d.is_dir() => d,
            Some(_) => return Err(EEXIST),
            None => {
                let sb = cur.sb.clone();
                let _g = sb.lock_names();
                cur.create_child(comp, uapi::S_IFDIR | 0o755, Cred::ROOT, crate::inode::Data::SynthDir(Arc::new(SynthDir::new())))?
            }
        };
    }
    Ok(cur)
}

fn make_file(root: &Arc<Dentry>, rel: &[u8], render: fn() -> alloc::string::String) -> Result<(), Errno> {
    let (dir, name) = match rel.iter().rposition(|&b| b == b'/') {
        Some(i) => (dir_at(root, &rel[..i])?, &rel[i + 1..]),
        None => (root.clone(), rel),
    };
    if name.is_empty() || dir.child(name).is_some() {
        return Err(EEXIST);
    }
    let sb = dir.sb.clone();
    let _g = sb.lock_names();
    dir.create_child(name, uapi::S_IFREG | 0o444, Cred::ROOT, crate::inode::Data::Proc(render)).map(|_| ())
}

fn add_source(root: &Arc<Dentry>, rel: &[u8], src: Arc<dyn DirSource>) -> Result<(), Errno> {
    let d = dir_at(root, rel)?;
    match &d.inode.data {
        crate::inode::Data::SynthDir(sd) => {
            sd.add(src);
            Ok(())
        }
        _ => Err(ENOTDIR),
    }
}

// The vfs host tests register no /proc file of their own: one here so the registry's section exists there.
#[cfg(test)]
fn test_text() -> alloc::string::String {
    alloc::string::String::from("sx3b\n")
}
#[cfg(test)]
crate::procfile!("/proc/sx3b-instance-test", test_text);
