//! Name changes on the disk: create (file or directory), remove (unlink, rmdir), rename (also over an existing
//! name and across directories). Every one asks the stick rules (`protect`) first and refuses before anything
//! is written.
//!
//! Orders (each step one sector unless said otherwise; FSInfo "unknown" before, the real count after):
//! - mkdir: the new cluster with "." and ".." -> its FAT entry -> the entry set in the parent.
//! - create a file: the entry set (size 0, no cluster).
//! - unlink: the entry set cleared; the clusters are freed when the last reference goes (an open file stays
//!   readable and writable, POSIX), at once when nothing holds it.
//! - rmdir: the entry set cleared -> the directory's clusters freed.
//! - rename: the old set cleared first, then the new set added (or the replaced entry rewritten in place with
//!   the moved file's cluster, size and times), then ".." of a moved directory, then the replaced file's
//!   clusters freed. A cut between two steps loses at most the moved entry into lost clusters (fsck.fat
//!   reclaims them); two entries never share a chain (which fsck.fat could not repair without guessing). Linux
//!   adds the new entry first and accepts that window; we prefer the order a check can always repair.

use crate::dirent::{self, Entry, ATTR_ARCHIVE, ATTR_DIR, ENTRY};
use crate::protect;
use crate::vol::{Child, Node, State, Volume, ROOT};
use alloc::vec;
use alloc::vec::Vec;
use uapi::errno::*;

impl Volume {
    /// The first cluster ".." names for a directory whose parent is `parent` (0 for the root, FAT spec).
    fn dotdot_cluster(st: &State, parent: u64) -> Result<u32, i64> {
        Ok(if parent == ROOT { 0 } else { Self::node(st, parent)?.e.first })
    }

    /// Recomputes the class of `ino` and every loaded node below it (after a rename moved it).
    fn reclass(st: &mut State, ino: u64) -> Result<(), i64> {
        let mut work = vec![ino];
        while let Some(i) = work.pop() {
            let (parent, name, alias) = {
                let n = Self::node(st, i)?;
                (n.parent, n.name.clone(), dirent::alias_key(&n.e))
            };
            let pc = Self::node(st, parent)?.class;
            Self::node_mut(st, i)?.class = protect::child_class(pc, &name, &alias);
            work.extend(st.nodes.iter().filter(|(_, n)| n.parent == i && n.linked).map(|(k, _)| *k));
        }
        Ok(())
    }

    /// A new file (`dir == false`) or directory `name` in directory `parent`.
    pub fn create(&self, parent: u64, name: &[u8], dir: bool) -> Result<Child, i64> {
        let mut st = self.lock()?;
        self.reap(&mut st)?;
        let pclass = Self::node(&st, parent)?.class;
        if !Self::node(&st, parent)?.is_dir() {
            return Err(ENOTDIR);
        }
        let rules = self.rules();
        let by_name = Self::class_of_new(&st, parent, name, &crate::name::fold(name))?;
        rules.create(pclass, by_name).map_err(|e| self.refused("create", parent, e))?;
        let t = self.clock.stamp_now();
        let tmpl = Entry::new([b' '; 11], if dir { ATTR_DIR } else { ATTR_ARCHIVE }, 0, t);
        let (mut set, short) = self.entry_set(&mut st, parent, name, &tmpl)?;
        let class = protect::child_class(pclass, name, &crate::name::short_key(&short));
        rules.create(pclass, class).map_err(|e| self.refused("create", parent, e))?;
        self.begin(&mut st)?;
        self.room_for(&mut st, parent, set.len())?;
        let mut e = Entry::parse(set.last().ok_or(EIO)?);
        if dir {
            let c = self.allocate(&mut st, None, 1)?[0];
            let cb = self.geo.cluster_bytes();
            let mut data = vec![0u8; cb];
            let dots = dirent::dot_entries(c, Self::dotdot_cluster(&st, parent)?, t);
            data[..ENTRY].copy_from_slice(&dots[0]);
            data[ENTRY..2 * ENTRY].copy_from_slice(&dots[1]);
            self.write(self.geo.cluster_sector(c), &data)?;
            st.fat.link(&*self.dev, &[c])?;
            st.fat.write_out(&*self.dev)?;
            e.first = c;
            let last = set.len() - 1;
            set[last] = e.bytes();
        }
        let slot = self.add_set(&mut st, parent, &set)?;
        let ino = st.next_ino;
        st.next_ino += 1;
        let shown = crate::name::from_utf16(&crate::name::to_utf16(name)?);
        let node = Node {
            parent,
            name: shown,
            e,
            slot,
            lfn: set.len() as u32 - 1,
            linked: true,
            chain: Some(if dir { vec![e.first] } else { Vec::new() }),
            raw: if dir { Some(self.new_dir_raw(&st, e.first, parent, t)?) } else { None },
            entry_dirty: false,
            class,
            dgen: alloc::sync::Arc::default(),
            reserved: 0,
        };
        let child = self.child_of(&node, ino);
        st.nodes.insert(ino, node);
        self.finish(&mut st)?;
        klog::event!(Debug, "fat", "created", parent = parent, ino = ino, dir = dir);
        Ok(child)
    }

    fn new_dir_raw(&self, st: &State, me: u32, parent: u64, t: dirent::Stamp) -> Result<Vec<u8>, i64> {
        let mut raw = vec![0u8; self.geo.cluster_bytes()];
        let dots = dirent::dot_entries(me, Self::dotdot_cluster(st, parent)?, t);
        raw[..ENTRY].copy_from_slice(&dots[0]);
        raw[ENTRY..2 * ENTRY].copy_from_slice(&dots[1]);
        Ok(raw)
    }

    /// unlink or rmdir of `ino` (a child of `parent`).
    pub fn remove(&self, parent: u64, ino: u64) -> Result<(), i64> {
        let mut st = self.lock()?;
        self.reap(&mut st)?;
        let (class, slot, lfn, is_dir, p) = {
            let n = Self::node(&st, ino)?;
            (n.class, n.slot, n.lfn, n.is_dir(), n.parent)
        };
        if p != parent {
            return Err(ENOENT);
        }
        self.rules().remove(class).map_err(|e| self.refused("remove", ino, e))?;
        if is_dir && !self.dir_empty(&mut st, ino)? {
            return Err(ENOTEMPTY);
        }
        self.begin(&mut st)?;
        self.clear_set(&mut st, parent, slot, lfn)?;
        Self::node_mut(&mut st, ino)?.linked = false;
        if is_dir {
            let chain = self.chain(&mut st, ino)?.clone();
            st.fat.release(&*self.dev, &chain)?;
            st.fat.write_out(&*self.dev)?;
            st.nodes.remove(&ino);
        }
        self.finish(&mut st)?;
        klog::event!(Debug, "fat", "removed", parent = parent, ino = ino, dir = is_dir);
        Ok(())
    }

    /// Writes ".." of directory `ino` (now below `parent`).
    fn fix_dotdot(&self, st: &mut State, ino: u64, parent: u64) -> Result<(), i64> {
        let pc = Self::dotdot_cluster(st, parent)?;
        let raw = self.dir_raw(st, ino)?;
        let mut e = Entry::parse(raw.get(ENTRY..2 * ENTRY).ok_or(EIO)?);
        if &e.name != b"..         " {
            klog::event!(Warn, "fat", "dir.no_dotdot", ino = ino; "a moved directory has no '..' entry: left as it is");
            return Ok(());
        }
        e.first = pc;
        raw[ENTRY..2 * ENTRY].copy_from_slice(&e.bytes());
        self.begin(st)?;
        let s = self.slot_sector(st, ino, 1)?;
        let raw = Self::node(st, ino)?.raw.as_ref().ok_or(EIO)?;
        let bytes = raw.get(..self.geo.bps).ok_or(EIO)?.to_vec();
        self.write(s, &bytes)
    }

    /// rename of `ino` (in `odir`) to `nname` in `ndir`, replacing `target` when given.
    pub fn rename(&self, odir: u64, ino: u64, ndir: u64, nname: &[u8], target: Option<u64>) -> Result<(), i64> {
        let mut st = self.lock()?;
        self.reap(&mut st)?;
        let rules = self.rules();
        let (sclass, sslot, slfn, sdir, sp) = {
            let n = Self::node(&st, ino)?;
            (n.class, n.slot, n.lfn, n.is_dir(), n.parent)
        };
        if sp != odir {
            return Err(ENOENT);
        }
        rules.remove(sclass).map_err(|e| self.refused("rename", ino, e))?;
        let npclass = Self::node(&st, ndir)?.class;
        match target {
            Some(t) => {
                let (tclass, tslot, tlfn, tdir, tname, tshort, tntres) = {
                    let n = Self::node(&st, t)?;
                    (n.class, n.slot, n.lfn, n.is_dir(), n.name.clone(), n.e.name, n.e.ntres)
                };
                rules.remove(tclass).map_err(|e| self.refused("rename", t, e))?;
                let newclass = protect::child_class(npclass, &tname, &crate::name::short_key(&tshort));
                rules.create(npclass, newclass).map_err(|e| self.refused("rename", ndir, e))?;
                if tdir && !self.dir_empty(&mut st, t)? {
                    return Err(ENOTEMPTY);
                }
                self.begin(&mut st)?;
                self.clear_set(&mut st, odir, sslot, slfn)?;
                {
                    let n = Self::node_mut(&mut st, ino)?;
                    n.parent = ndir;
                    n.slot = tslot;
                    n.lfn = tlfn;
                    n.e.name = tshort;
                    n.e.ntres = tntres;
                    n.name = tname;
                }
                self.write_entry(&mut st, ino)?;
                Self::node_mut(&mut st, t)?.linked = false;
                if sdir && odir != ndir {
                    self.fix_dotdot(&mut st, ino, ndir)?;
                }
                if tdir {
                    let chain = self.chain(&mut st, t)?.clone();
                    st.fat.release(&*self.dev, &chain)?;
                    st.fat.write_out(&*self.dev)?;
                    st.nodes.remove(&t);
                }
            }
            None => {
                let by_name = protect::child_class(npclass, nname, &crate::name::fold(nname));
                rules.create(npclass, by_name).map_err(|e| self.refused("rename", ndir, e))?;
                let e = Self::node(&st, ino)?.e;
                let (set, short) = self.entry_set(&mut st, ndir, nname, &e)?;
                let newclass = protect::child_class(npclass, nname, &crate::name::short_key(&short));
                rules.create(npclass, newclass).map_err(|e| self.refused("rename", ndir, e))?;
                self.begin(&mut st)?;
                // Room first: a full volume refuses before the old entry is cleared (never a lost file).
                self.room_for(&mut st, ndir, set.len())?;
                self.clear_set(&mut st, odir, sslot, slfn)?;
                let slot = self.add_set(&mut st, ndir, &set)?;
                let shown = crate::name::from_utf16(&crate::name::to_utf16(nname)?);
                {
                    let n = Self::node_mut(&mut st, ino)?;
                    n.parent = ndir;
                    n.slot = slot;
                    n.lfn = set.len() as u32 - 1;
                    n.e.name = short;
                    n.e.ntres = 0;
                    n.name = shown;
                }
                if sdir && odir != ndir {
                    self.fix_dotdot(&mut st, ino, ndir)?;
                }
            }
        }
        Self::reclass(&mut st, ino)?;
        self.finish(&mut st)?;
        klog::event!(Debug, "fat", "renamed", ino = ino, from = odir, to = ndir, replaced = target.unwrap_or(0));
        Ok(())
    }
}
