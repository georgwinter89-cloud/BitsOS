//! SX-3a: the mount tree operations under `TOPO` (linux-6.6/fs/namespace.c `clone_mnt`, `copy_tree`,
//! `attach_recursive_mnt`, `umount_tree`, `lock_mnt_tree`; fs/pnode.c `change_mnt_propagation`,
//! `propagate_mnt`, `propagate_umount`). Every function takes the locked table and a `Grave` for what it lets
//! go; nothing here drops a mount or dentry.
//!
//! Propagation by peer groups: a mount's `group` is its peer group, `master` the group it is a slave of. A new
//! mount on a shared mount becomes shared itself (`invent_group_ids`) and gets a copy on every peer of the
//! place (peers of the new mount) and on every slave (slaves of it; a slave that is shared itself gets a copy
//! in a new group of its own, and that group's peers and slaves follow in turn). V2 puts a propagated copy on
//! top of what already sits at the target place; Linux tucks it beneath (`mnt_change_mountpoint`).

use crate::dentry::Dentry;
use crate::mount_ns::*;
use crate::Errno;
use alloc::collections::{BTreeMap, BTreeSet};
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::Ordering;
use uapi::errno::*;

/// `clone_mnt` flags (fs/internal.h `CL_*`).
pub(crate) const CL_SLAVE: u32 = 0x02;
pub(crate) const CL_COPY_UNBINDABLE: u32 = 0x04;
pub(crate) const CL_MAKE_SHARED: u32 = 0x08;
pub(crate) const CL_PRIVATE: u32 = 0x10;
pub(crate) const CL_SHARED_TO_SLAVE: u32 = 0x20;

/// The mounts sitting directly on places of `m`.
pub(crate) fn children(t: &Topo, m: &Arc<Mount>) -> Vec<Arc<Mount>> {
    t.hash.range((m.id, 0)..=(m.id, u64::MAX)).map(|(_, c)| c.clone()).collect()
}

/// `m` and everything below it, parents before children (Linux `next_mnt` order).
pub(crate) fn subtree(t: &Topo, m: &Arc<Mount>) -> Vec<Arc<Mount>> {
    let mut out = Vec::new();
    let mut stack = alloc::vec![m.clone()];
    while let Some(x) = stack.pop() {
        let mut kids = children(t, &x);
        kids.sort_by_key(|k| core::cmp::Reverse(k.id));
        stack.extend(kids);
        out.push(x);
    }
    out
}

/// `attach_mnt`: `child` sits on `d` of `parent` from now on, in `parent`'s view. EBUSY if the place is taken
/// (callers step onto the topmost mount first).
pub(crate) fn attach(t: &mut Topo, g: &mut Grave, child: &Arc<Mount>, parent: &Arc<Mount>, d: &Arc<Dentry>) -> Result<(), Errno> {
    let key = (parent.id, d.id);
    if t.hash.contains_key(&key) {
        return Err(EBUSY);
    }
    t.put(key, child);
    d.mounted.fetch_add(1, Ordering::AcqRel);
    let ns = parent.ns_id();
    let old = core::mem::replace(&mut child.st.lock().mp, Some((parent.clone(), d.clone())));
    g.places.extend(old);
    for m in subtree(t, child) {
        m.st.lock().ns = ns;
    }
    Ok(())
}

/// Takes `m` out of its place (`unhash_mnt`): the table's reference goes to the grave, its view stays.
pub(crate) fn detach(t: &mut Topo, g: &mut Grave, m: &Arc<Mount>) {
    let mp = m.st.lock().mp.take();
    if let Some((p, d)) = mp {
        if let Some(x) = t.take((p.id, d.id)) {
            d.mounted.fetch_sub(1, Ordering::AcqRel);
            g.mounts.push(x);
        }
        g.places.push((p, d));
    }
}

/// Removes `m` from the table for good (it leaves its view, its propagation and its place; a view's root
/// entry too).
pub(crate) fn unhash(t: &mut Topo, g: &mut Grave, m: &Arc<Mount>) {
    change_type(t, m, Prop::Private);
    detach(t, g, m);
    let ns = m.ns_id();
    if t.hash.get(&(0, ns)).is_some_and(|r| Arc::ptr_eq(r, m)) {
        g.mounts.extend(t.take((0, ns)));
    }
    m.st.lock().ns = 0;
}

/// The members of peer group `group` in the table (none for 0).
pub(crate) fn peers(t: &Topo, group: u64) -> Vec<Arc<Mount>> {
    if group == 0 {
        return Vec::new();
    }
    t.peers(group)
}

/// The slaves of peer group `group` in the table (none for 0).
pub(crate) fn slaves(t: &Topo, group: u64) -> Vec<Arc<Mount>> {
    if group == 0 {
        return Vec::new();
    }
    t.slaves(group)
}

/// `lock_mount` under the table's lock: the topmost mount stacked at `p`, or `p` when nothing sits there. A
/// change that looks up its place before taking `TOPO` steps up again here, so its attach finds the place free.
pub(crate) fn topmost(t: &Topo, p: &Path) -> Path {
    let mut at = p.clone();
    while let Some(x) = t.hash.get(&(at.mnt.id, at.dentry.id)).cloned() {
        at = Path { dentry: x.root.clone(), mnt: x };
    }
    at
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Prop {
    Shared,
    Slave,
    Private,
    Unbindable,
}

/// `change_mnt_propagation` in peer-group terms (fs/pnode.c `do_make_slave`): MS_SHARED gives a group (the
/// master stays); the others leave the group first: the last peer hands its slaves to its own master (or
/// frees them), a peer with others left becomes their slave. MS_PRIVATE and MS_UNBINDABLE drop the master too.
pub(crate) fn change_type(t: &mut Topo, m: &Arc<Mount>, ty: Prop) {
    let (group, master) = m.propagation();
    if ty == Prop::Shared {
        if group == 0 {
            t.set_prop(m, new_group(), master);
        }
        m.store_flags(m.flags() & !MNT_UNBINDABLE);
        return;
    }
    let mut new_master = master;
    if group != 0 {
        if t.has_other_peer(group, m.id) {
            new_master = group;
        } else {
            for x in slaves(t, group) {
                let xg = x.propagation().0;
                t.set_prop(&x, xg, master);
            }
        }
    }
    if ty != Prop::Slave {
        new_master = 0;
        let f = m.flags();
        m.store_flags(if ty == Prop::Unbindable { f | MNT_UNBINDABLE } else { f & !MNT_UNBINDABLE });
    }
    t.set_prop(m, 0, new_master);
}

/// `clone_mnt`: a new mount of `old`'s file system at `root`, with `old`'s flags (without the lock on being
/// unmounted alone) and the propagation `cl` asks for, in view `ns` (not attached).
pub(crate) fn clone_mnt(old: &Arc<Mount>, root: &Arc<Dentry>, cl: u32, ns: u64) -> Arc<Mount> {
    // The flags (MNT_READONLY among them) are old's current ones, not those it was made with.
    let m = Mount::new(old.sb.clone(), root.clone(), false);
    m.store_flags(old.flags());
    m.set_source(old.devname());
    let (og, om) = old.propagation();
    let (mut group, mut master) = (0, 0);
    if cl & CL_SLAVE != 0 || (cl & CL_SHARED_TO_SLAVE != 0 && og != 0) {
        master = og;
    } else if cl & CL_PRIVATE == 0 {
        group = og;
        master = om;
    }
    if cl & CL_MAKE_SHARED != 0 && group == 0 {
        group = new_group();
    }
    {
        let mut s = m.st.lock();
        s.group = group;
        s.master = master;
        s.ns = ns;
    }
    m
}

/// `copy_tree`: copies `src` (rooted at `dentry`) and the mounts below it whose place lies under `dentry`,
/// attached to each other as in the source, the copy's top attached nowhere. Unbindable mounts are left out
/// unless `CL_COPY_UNBINDABLE` (an unbindable and locked one is EPERM, the whole copy is undone).
pub(crate) fn copy_tree(t: &mut Topo, g: &mut Grave, src: &Arc<Mount>, dentry: &Arc<Dentry>, cl: u32, ns: u64) -> Result<Arc<Mount>, Errno> {
    if cl & CL_COPY_UNBINDABLE == 0 {
        // Checked before anything is made, so a refusal leaves nothing behind.
        let mut stack = children(t, src).into_iter().filter(|c| c.mountpoint().is_some_and(|(_, d)| d.is_descendant_of(dentry))).collect::<Vec<_>>();
        while let Some(c) = stack.pop() {
            if c.flags() & MNT_UNBINDABLE != 0 {
                if c.flags() & MNT_LOCKED != 0 {
                    return Err(EPERM);
                }
                continue;
            }
            stack.extend(children(t, &c));
        }
    }
    Ok(copy_tree_map(t, g, src, dentry, cl, ns).0)
}

/// `copy_tree` with the map "source mount id -> copy" (`copy_mnt_ns` needs it). The caller checked the
/// unbindable rule.
pub(crate) fn copy_tree_map(t: &mut Topo, g: &mut Grave, src: &Arc<Mount>, dentry: &Arc<Dentry>, cl: u32, ns: u64) -> (Arc<Mount>, BTreeMap<u64, Weak<Mount>>) {
    let top = clone_mnt(src, dentry, cl, ns);
    let mut map = BTreeMap::new();
    map.insert(src.id, Arc::downgrade(&top));
    // Parents before children, children in mount order (Linux's `next_mnt` walk), so the copies' ids follow
    // the source's order.
    let kids = |t: &Topo, m: &Arc<Mount>| {
        let mut v = children(t, m);
        v.sort_by_key(|k| core::cmp::Reverse(k.id));
        v
    };
    let mut work: Vec<(Arc<Mount>, Arc<Mount>)> = kids(t, src).into_iter().filter(|c| c.mountpoint().is_some_and(|(_, d)| d.is_descendant_of(dentry))).map(|c| (c, top.clone())).collect();
    while let Some((s, parent_copy)) = work.pop() {
        if cl & CL_COPY_UNBINDABLE == 0 && s.flags() & MNT_UNBINDABLE != 0 {
            continue;
        }
        let Some((_, at)) = s.mountpoint() else { continue };
        let q = clone_mnt(&s, &s.root, cl, ns);
        if attach(t, g, &q, &parent_copy, &at).is_err() {
            g.mounts.push(q);
            continue;
        }
        map.insert(s.id, Arc::downgrade(&q));
        work.extend(kids(t, &s).into_iter().map(|c| (c, q.clone())));
    }
    (top, map)
}

/// `lock_mnt_tree`: an unprivileged view's copies keep their flags and cannot be unmounted one by one.
pub(crate) fn lock_tree(t: &Topo, top: &Arc<Mount>) {
    for m in subtree(t, top) {
        let mut f = m.flags() | MNT_LOCK_ATIME | MNT_LOCKED;
        for (on, lock) in [(MNT_READONLY, MNT_LOCK_READONLY), (MNT_NODEV, MNT_LOCK_NODEV), (MNT_NOSUID, MNT_LOCK_NOSUID), (MNT_NOEXEC, MNT_LOCK_NOEXEC)] {
            if f & on != 0 {
                f |= lock;
            }
        }
        m.store_flags(f);
    }
}

fn owner_of(t: &Topo, ns: u64) -> u32 {
    t.owners.get(&ns).copied().unwrap_or(0)
}

/// `graft_tree` + `attach_recursive_mnt`: puts the (unattached) tree `src` on `d` of `dest` (the topmost mount
/// there) and propagates copies when `dest` is shared. ENOTDIR when one is a directory and the other is not.
pub(crate) fn graft(t: &mut Topo, g: &mut Grave, src: &Arc<Mount>, dest: &Arc<Mount>, d: &Arc<Dentry>) -> Result<(), Errno> {
    if d.is_dir() != src.root.is_dir() {
        return Err(ENOTDIR);
    }
    let shared = dest.propagation().0 != 0;
    if shared {
        // `invent_group_ids`: the new tree becomes shared.
        for m in subtree(t, src) {
            let (g0, m0) = m.propagation();
            if g0 == 0 {
                t.set_prop(&m, new_group(), m0);
            }
        }
    }
    attach(t, g, src, dest, d)?;
    if shared {
        propagate(t, g, dest, d, src);
    }
    Ok(())
}

/// Puts the copy `c` on (`m`, `d`), on top of what is there; a copy into a view of another owner is locked
/// below its top (`attach_recursive_mnt`'s `lock_mnt_tree`).
fn place(t: &mut Topo, g: &mut Grave, c: &Arc<Mount>, m: &Arc<Mount>, d: &Arc<Dentry>, from_owner: u32) {
    let at = topmost(t, &Path { mnt: m.clone(), dentry: d.clone() });
    if at.dentry.is_dir() != c.root.is_dir() || attach(t, g, c, &at.mnt, &at.dentry).is_err() {
        g.mounts.push(c.clone());
        return;
    }
    if owner_of(t, m.ns_id()) != from_owner {
        lock_tree(t, c);
        c.store_flags(c.flags() & !MNT_LOCKED);
    }
    klog::event!(Debug, "vfs", "mount.propagated", mount = c.id, to = m.id, ns = m.ns_id());
}

/// `propagate_mnt`: copies of `src` (just put on `d` of the shared `dest`) on the peers and slaves of `dest`.
fn propagate(t: &mut Topo, g: &mut Grave, dest: &Arc<Mount>, d: &Arc<Dentry>, src: &Arc<Mount>) {
    let from_owner = owner_of(t, dest.ns_id());
    let mut done: BTreeSet<u64> = BTreeSet::new();
    done.insert(dest.id);
    let first = dest.propagation().0;
    let mut seen: BTreeSet<u64> = BTreeSet::new();
    seen.insert(first);
    let mut queue: Vec<(u64, Arc<Mount>)> = alloc::vec![(first, src.clone())];
    while let Some((h, s)) = queue.pop() {
        let under = |m: &Arc<Mount>| m.ns_id() != 0 && d.is_descendant_of(&m.root);
        for m in peers(t, h) {
            if done.insert(m.id) && under(&m) {
                let c = copy_tree_map(t, g, &s, &s.root, CL_MAKE_SHARED, m.ns_id()).0;
                place(t, g, &c, &m, d, from_owner);
            }
        }
        for x in slaves(t, h) {
            if !done.insert(x.id) || !under(&x) {
                continue;
            }
            let k = x.propagation().0;
            let cl = if k != 0 { CL_SLAVE | CL_MAKE_SHARED } else { CL_SLAVE };
            let c = copy_tree_map(t, g, &s, &s.root, cl, x.ns_id()).0;
            place(t, g, &c, &x, d, from_owner);
            if k != 0 && seen.insert(k) {
                queue.push((k, c));
            }
        }
    }
}

/// The mounts that receive what happens on `m` (its peers and slaves, and theirs), `m` left out.
fn receivers(t: &Topo, m: &Arc<Mount>) -> Vec<Arc<Mount>> {
    let mut out = Vec::new();
    let mut seen: BTreeSet<u64> = BTreeSet::new();
    let mut groups = alloc::vec![m.propagation().0];
    while let Some(h) = groups.pop() {
        if h == 0 || !seen.insert(h) {
            continue;
        }
        for p in peers(t, h) {
            if !Arc::ptr_eq(&p, m) {
                out.push(p);
            }
        }
        for s in slaves(t, h) {
            groups.push(s.propagation().0);
            out.push(s);
        }
    }
    out.sort_by_key(|x| x.id);
    out.dedup_by_key(|x| x.id);
    out
}

/// `umount_tree`: takes `m` and everything below it out of the view (children first), each made private; with
/// `propagate`, also the copies on the receivers of `m`'s parent at the same place that have nothing mounted
/// below them and are not locked (`propagate_umount`).
pub(crate) fn umount_tree(t: &mut Topo, g: &mut Grave, m: &Arc<Mount>, propagate: bool) {
    let place = m.mountpoint();
    let mut list = subtree(t, m);
    if propagate && let Some((p, d)) = &place {
        for r in receivers(t, p) {
            if let Some(c) = t.hash.get(&(r.id, d.id)).cloned()
                && c.flags() & MNT_LOCKED == 0
                && children(t, &c).is_empty()
                && Arc::ptr_eq(&c.sb, &m.sb)
            {
                list.push(c);
            }
        }
    }
    for x in list.iter().rev() {
        unhash(t, g, x);
    }
    g.mounts.extend(list);
}
