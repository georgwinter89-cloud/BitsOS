//! Snapshots mounted read-only (VS-2/VS-3: a file or folder is copied back from one). The snapshot's volume is
//! opened as a stacked disk `<pool>.v<id>` and ext4 mounts it without writing (a frozen snapshot is clean; a
//! journal would be replayed in memory only). While mounted the pool refuses to delete it (EBUSY).

use crate::{Target, own_snapshot, vol_errno};
use alloc::collections::{BTreeMap, BTreeSet};
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use uapi::errno::*;
use vfs::{Errno, MountNs};

/// Where `/dev/bitssnap` mounts snapshot `<id>` in the system view.
pub const BASE: &str = "/run/bitsos/snap";

struct Ro {
    disk: String,
    at: String,
    ns: Arc<MountNs>,
}

/// A table entry. The table lock is held only to look up and change entries, never across the slow work
/// (opening the volume, ext4's mount with its in-memory replay, waiting for the device to be let go), so a
/// `list` is never held up by a mount or umount; the busy states keep a second mount or umount of the same
/// snapshot out meanwhile.
enum State {
    Mounting,
    Mounted(Ro),
    Unmounting,
}

static MOUNTS: ksync::Mutex<BTreeMap<(String, u64), State>> = ksync::Mutex::named("snap.mounts", BTreeMap::new());
const WAIT_NS: u64 = 10_000_000_000;
/// How long an umount waits for the snapshot's device to be closed (files still open in it keep it longer).
const RELEASE_WAIT_NS: u64 = 2_000_000_000;

fn table() -> Result<ksync::MutexGuard<'static, BTreeMap<(String, u64), State>>, Errno> {
    MOUNTS.lock_timeout(WAIT_NS).ok_or_else(|| {
        klog::event!(Warn, "snap", "mounts.busy", wait_ns = WAIT_NS);
        EBUSY
    })
}

/// The snapshots of `pool` that are mounted or being mounted or unmounted (all of them keep the volume open).
pub fn mounted_ids(pool: &str) -> Result<BTreeSet<u64>, Errno> {
    Ok(table()?.keys().filter(|(p, _)| p == pool).map(|(_, id)| *id).collect())
}

pub fn is_mounted(pool: &str, id: u64) -> Result<bool, Errno> {
    Ok(table()?.contains_key(&(String::from(pool), id)))
}

/// Where snapshot `id` is mounted, if it is (and not just being mounted or unmounted).
pub fn mounted_at(pool: &str, id: u64) -> Option<String> {
    match table().ok()?.get(&(String::from(pool), id)) {
        Some(State::Mounted(r)) => Some(r.at.clone()),
        _ => None,
    }
}

/// Mounts snapshot `id` of the target read-only at `BASE/<id>` of the system view (again: the same place).
pub fn mount(t: &Target, id: u64) -> Result<String, Errno> {
    mount_at(t, id, vfs::root_ns(), &format!("{BASE}/{id}"))
}

/// Drops a `Mounting` entry when the mount did not get through (every early return).
struct Claim(Option<(String, u64)>);

impl Drop for Claim {
    fn drop(&mut self) {
        if let Some(k) = self.0.take() {
            // A bounded wait; the entry only ever blocks this one snapshot's mount when it stays.
            if let Ok(mut t) = table() {
                t.remove(&k);
            }
        }
    }
}

/// Mounts snapshot `id` read-only at `at` in `ns` (made when missing). EBUSY while it is being mounted or
/// unmounted by another caller.
pub fn mount_at(t: &Target, id: u64, ns: Arc<MountNs>, at: &str) -> Result<String, Errno> {
    let key = (t.pool_name.clone(), id);
    {
        let mut tab = table()?;
        match tab.get(&key) {
            Some(State::Mounted(r)) => return Ok(r.at.clone()),
            Some(_) => return Err(EBUSY),
            None => {}
        }
        tab.insert(key.clone(), State::Mounting);
    }
    let mut claim = Claim(Some(key.clone()));
    {
        let p = t.pool.lock().map_err(|e| vol_errno(e.into()))?;
        own_snapshot(&p, t, id)?;
    }
    let made = ns.lookup_path(at.as_bytes(), true).is_err();
    ns.mkdir_p(at)?;
    let undo = |e: Errno| {
        if made {
            let _ = ns.remove(at);
        }
        e
    };
    let vd = t.pool.open_volume(id).map_err(vol_errno).map_err(undo)?;
    let disk = block::register(Arc::new(vd), block::DiskKind::Stacked).map_err(|e| {
        klog::event!(Warn, "snap", "mount.register_failed", id = id, error = format!("{e:?}").as_str());
        undo(EBUSY)
    })?;
    let blocks = disk.dev_info().blocks;
    let opts = fs_ext4::Options { writable: false, ..fs_ext4::Options::default() };
    match fs_ext4::mount::mount_disk(disk.clone(), 0, blocks, &ns, at, opts) {
        Ok((_vol, m)) => {
            m.sb.set_source(&format!("/dev/mapper/{}-snap{id}", t.pool_name));
            klog::event!(Info, "snap", "mounted", pool = t.pool_name.as_str(), id = id, at = at);
            let ro = State::Mounted(Ro { disk: disk.name().into(), at: at.into(), ns: ns.clone() });
            match table() {
                Ok(mut tab) => {
                    tab.insert(key, ro);
                    claim.0 = None;
                    Ok(at.into())
                }
                // Not recorded, so not reachable by umount: take it down again.
                Err(e) => {
                    if let Ok(p) = ns.lookup_path(at.as_bytes(), true) {
                        let _ = ns.umount(&p);
                    }
                    block::unregister(disk.name());
                    Err(undo(e))
                }
            }
        }
        Err(e) => {
            klog::event!(Warn, "snap", "mount_failed", pool = t.pool_name.as_str(), id = id, errno = e);
            block::unregister(disk.name());
            Err(undo(e))
        }
    }
}

/// Unmounts snapshot `id` (ENOENT when it is not mounted, EBUSY while another caller mounts or unmounts it).
/// Files still open in it keep it readable until they are closed; the pool lets it go (and delete it) after
/// that.
pub fn umount(t: &Target, id: u64) -> Result<(), Errno> {
    let key = (t.pool_name.clone(), id);
    let r = {
        let mut tab = table()?;
        match tab.get(&key) {
            None => return Err(ENOENT),
            Some(State::Mounted(_)) => {}
            Some(_) => return Err(EBUSY),
        }
        match tab.insert(key.clone(), State::Unmounting) {
            Some(State::Mounted(r)) => r,
            _ => return Err(EBUSY),
        }
    };
    // Unmounted from outside (umount(2) by root): only the disk and the entry are left to drop.
    if let Ok(p) = r.ns.lookup_path(r.at.as_bytes(), true)
        && p.is_mount_root()
        && let Err(e) = r.ns.umount(&p)
    {
        // Still in use (a process has its working directory there): stays mounted.
        if let Ok(mut tab) = table() {
            tab.insert(key, State::Mounted(r));
        }
        return Err(e);
    }
    block::unregister(&r.disk);
    let _ = r.ns.remove(&r.at);
    // The disk's last references (its worker, the file system's caches) go a moment after the unregister;
    // wait for that (bounded) so a delete right after the umount is not refused.
    let end = ksync::now_ns().unwrap_or(0).saturating_add(RELEASE_WAIT_NS);
    while t.pool.lock().is_ok_and(|p| p.is_open(id)) && ksync::now_ns().unwrap_or(u64::MAX) < end {
        sched::sleep_ns(5_000_000);
    }
    klog::event!(Info, "snap", "unmounted", pool = t.pool_name.as_str(), id = id);
    // A bounded wait; should it time out, the entry stays Unmounting and the next umount says EBUSY.
    table()?.remove(&key);
    Ok(())
}
