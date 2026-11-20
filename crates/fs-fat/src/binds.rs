//! UPD-4: the boot volume's mount at `/boot` with the running version's parts bound over their places. kexec
//! knows which generation runs and which parts it names (`\BITSOS2\SYS\<n>\FILES`, its `install::parts_boot`): it
//! hands the planner in before any disk is probed (an initcall of the Subsys level; the stick mounts at the
//! Service level and later), and the planner reads the volume before it is visible anywhere. The mount and its
//! binds then appear in one step (`vfs::MountNs::mount_with_binds`), so every program, the GPU firmware's reader
//! and the desktop's start of `bin/bitscomp` see the running version's files at the fixed paths.

use crate::vol::Volume;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use ksync::SpinLock;
use vfs::{Dentry, Mount, MountNs};

/// One bind: the folder `from` of the volume shown at its folder `at` (each a path of names from the root).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Bind {
    pub at: Vec<Vec<u8>>,
    pub from: Vec<Vec<u8>>,
    /// The part's name and hash for the log (`mesa 1a2b…`).
    pub what: String,
}

/// What to bind on a volume about to be mounted at /boot (it reads the volume itself; nothing is mounted yet).
pub type Planner = fn(&Arc<Volume>) -> Vec<Bind>;

static PLANNER: SpinLock<Option<Planner>> = SpinLock::named("fat.binds.planner", None);
/// The binds of the current mount, for kexec's status and its store's cleanup.
static BOUND: SpinLock<Vec<Bind>> = SpinLock::named("fat.binds.bound", Vec::new());

pub fn set_planner(p: Planner) {
    *PLANNER.lock() = Some(p);
}

/// The binds the mounted boot volume has now.
pub fn bound() -> Vec<Bind> {
    BOUND.lock().clone()
}

fn walk(root: &Arc<Dentry>, comps: &[Vec<u8>]) -> Option<Arc<Dentry>> {
    let mut d = root.clone();
    for c in comps {
        d = d.child(c)?;
    }
    d.is_dir().then_some(d)
}

/// `glue::mount` of the boot volume with the planner's binds; a bind whose place or folder is missing is left
/// out with a warning (the volume's own folder shows then).
pub fn mount_boot(vol: &Arc<Volume>, ns: &MountNs, path: &str) -> Result<Arc<Mount>, i64> {
    // The planner reads the disk: never under the spin lock.
    let planner = *PLANNER.lock();
    let plan = planner.map(|p| p(vol)).unwrap_or_default();
    ns.mkdir_p(path)?;
    let at = ns.lookup_path(path.as_bytes(), true)?;
    let (sb, root) = crate::glue::super_block(vol);
    let mut pairs = Vec::new();
    let mut kept = Vec::new();
    for b in plan {
        match (walk(&root, &b.at), walk(&root, &b.from)) {
            (Some(place), Some(from)) if !pairs.iter().any(|(p, _): &(Arc<Dentry>, Arc<Dentry>)| Arc::ptr_eq(p, &place)) => {
                pairs.push((place, from));
                klog::event!(Info, "fat", "stick.bind", part = b.what.as_str(), at = String::from_utf8_lossy(&b.at.join(&b'/')).into_owned());
                kept.push(b);
            }
            (place, from) => klog::event!(Warn, "fat", "stick.bind_missing", part = b.what.as_str(), place = place.is_some(), folder = from.is_some();
                "the part is not bound: the medium's own folder shows"),
        }
    }
    let m = ns.mount_with_binds(&at, sb, root, false, &pairs)?;
    *BOUND.lock() = kept;
    Ok(m)
}

/// The boot volume went away: its mount and every bind on it.
pub fn umount_boot(ns: &MountNs, path: &str) -> Result<(), i64> {
    let p = ns.lookup_path(path.as_bytes(), true)?;
    BOUND.lock().clear();
    ns.umount_recursive(&p)
}
