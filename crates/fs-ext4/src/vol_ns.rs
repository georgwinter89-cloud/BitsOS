//! Names on the volume: create (files, directories, symlinks fast and slow, device nodes, FIFOs, sockets),
//! hard links, unlink/rmdir with the orphan list, rename (over an existing name, across directories) and
//! RENAME_EXCHANGE, following linux-6.6/fs/ext4/namei.c. Each is one transaction; freeing an inode is
//! `evict`'s two more. An inode whose last name goes is put on the orphan list in the same transaction, so a
//! cut before it is freed leaves it to the next mount's orphan cleanup, not to e2fsck.

use crate::dir::{self, FT_DIR};
use crate::extent::{self, Edit, Target};
use crate::inode::*;
use crate::le::*;
use crate::state::State;
use crate::tx::Tx;
use crate::vol::Volume;
use alloc::collections::BTreeMap;
use alloc::vec;
use crate::errno::*;

/// `EXT4_LINK_MAX`.
pub const LINK_MAX: u16 = 65000;

/// What a new node is.
#[derive(Clone, Copy)]
pub enum Kind<'a> {
    Reg,
    Dir,
    Symlink(&'a [u8]),
    /// Character or block device (`rdev` as Linux's dev_t).
    Dev(u64),
    Fifo,
    Sock,
}

/// Linux's `old_valid_dev`/`new_encode_dev` placement in `i_block` (inode.c `ext4_do_update_inode`).
fn put_rdev(ib: &mut [u8], rdev: u64) {
    let (major, minor) = (((rdev >> 8) & 0xFFF) as u32 | ((rdev >> 32) as u32 & !0xFFF), (rdev & 0xFF) as u32 | ((rdev >> 12) as u32 & !0xFF));
    if major < 256 && minor < 256 {
        p32(ib, 0, major << 8 | minor);
        p32(ib, 4, 0);
    } else {
        p32(ib, 0, 0);
        p32(ib, 4, (minor & 0xFF) | (major << 8) | ((minor & !0xFF) << 12));
        p32(ib, 8, 0);
    }
}

/// The dev_t a device inode stores.
pub fn rdev_of(di: &DiskInode) -> u64 {
    let ib = di.i_block();
    let (major, minor) = if g32(ib, 0) != 0 {
        let v = g32(ib, 0);
        ((v >> 8) & 0xFF, v & 0xFF)
    } else {
        let v = g32(ib, 4);
        ((v & 0xFFF00) >> 8, (v & 0xFF) | ((v >> 12) & 0xFFF00))
    };
    ((major as u64 & 0xFFF) << 8) | ((major as u64 & !0xFFF) << 32) | (minor as u64 & 0xFF) | ((minor as u64 & !0xFF) << 12)
}

impl State {
    fn dir_inode(&self, tx: &Tx, ino: u32) -> Result<DiskInode, i64> {
        let d = self.inode(Some(tx), ino)?;
        if !d.is_dir() {
            return Err(ENOTDIR);
        }
        Ok(d)
    }

    fn touch_dir(&self, d: &mut DiskInode) {
        let t = self.now();
        d.set_time(Stamp::M, t);
        d.set_time(Stamp::C, t);
    }

    /// A directory whose link count may pass LINK_MAX (it then stays 1): an htree directory on a dir_nlink
    /// volume (`EXT4_DIR_LINK_MAX`, linux-6.6/fs/ext4/ext4.h:2435; `ext4_inc_count`, namei.c:2766). Asked
    /// before an insert turns the directory linear (`add_name`), as Linux would still see it indexed.
    fn may_overflow(&self, d: &DiskInode) -> bool {
        self.geo.has_ro(crate::sb::ro::DIR_NLINK) && self.geo.has_compat(crate::sb::compat::DIR_INDEX) && d.flags() & FL_INDEX != 0
    }

    fn inc_dir_links(d: &mut DiskInode, overflow: bool) -> Result<(), i64> {
        match d.links() {
            1 => Ok(()),
            n if n >= LINK_MAX && overflow => {
                d.set_links(1);
                Ok(())
            }
            n if n >= LINK_MAX => Err(EMLINK),
            n => {
                d.set_links(n + 1);
                Ok(())
            }
        }
    }
    fn dec_dir_links(d: &mut DiskInode) {
        if d.links() > 2 {
            d.set_links(d.links() - 1);
        }
    }

    /// A victim lost its last name: on the orphan list (freed at `forget`, or right after the commit).
    fn last_link(&mut self, tx: &mut Tx, v: &mut DiskInode) {
        self.orphan_add(tx, v);
    }

    fn create_in(&mut self, tx: &mut Tx, dir_ino: u32, name: &[u8], mode: u16, uid: u32, gid: u32, kind: Kind) -> Result<DiskInode, i64> {
        dir::check_name(name)?;
        let mut parent = self.dir_inode(tx, dir_ino)?;
        let is_dir = matches!(kind, Kind::Dir);
        let overflow = self.may_overflow(&parent);
        if is_dir && !overflow && parent.links() >= LINK_MAX {
            return Err(EMLINK);
        }
        let keep = self.keep_for(Some((uid, gid)));
        let ino = self.alloc_inode(tx, dir_ino, is_dir)?;
        self.next_gen = self.next_gen.wrapping_mul(1_103_515_245).wrapping_add(12345);
        let mut di = DiskInode::fresh(&self.geo, ino, mode, uid, gid, parent.flags(), self.next_gen, self.now());
        if parent.flags() & FL_PROJINHERIT != 0 && di.fits(PROJID + 4) {
            p32(&mut di.raw, PROJID, parent.projid());
        }
        let bs = self.geo.bs;
        match kind {
            Kind::Reg | Kind::Fifo | Kind::Sock => {}
            Kind::Dev(rdev) => put_rdev(di.i_block_mut(), rdev),
            Kind::Dir => {
                let (p, _) = self.alloc_blocks(tx, self.group_goal(ino), 1, keep)?;
                tx.put(p, dir::first_block(bs as usize, di.csum_seed(&self.geo), ino, dir_ino));
                let tree = {
                    let mut ed = Edit::new(self, tx, &mut di, p + 1);
                    ed.set(0, 1, Target::Map { pblk: p, unwritten: false })?;
                    ed.tree_blocks
                };
                di.set_size(bs);
                di.set_links(2);
                di.set_blocks512(&self.geo, (1 + tree as u64) * (bs / 512))?;
            }
            Kind::Symlink(t) => {
                if t.is_empty() || t.len() as u64 >= bs {
                    return Err(ENAMETOOLONG);
                }
                if t.len() < 60 {
                    di.i_block_mut()[..t.len()].copy_from_slice(t);
                } else {
                    di.set_flags(di.flags() | FL_EXTENTS);
                    extent::init_root(di.i_block_mut());
                    let (p, _) = self.alloc_blocks(tx, self.group_goal(ino), 1, keep)?;
                    let mut b = vec![0u8; bs as usize];
                    b[..t.len()].copy_from_slice(t);
                    tx.put(p, b);
                    let mut ed = Edit::new(self, tx, &mut di, p + 1);
                    ed.set(0, 1, Target::Map { pblk: p, unwritten: false })?;
                    di.set_blocks512(&self.geo, bs / 512)?;
                }
                di.set_size(t.len() as u64);
            }
        }
        if !is_dir {
            di.set_links(1);
        }
        self.put_inode(tx, &mut di)?;
        self.add_name(tx, &mut parent, name, ino, dir::ftype_of_mode(mode), keep)?;
        if is_dir {
            Self::inc_dir_links(&mut parent, overflow)?;
        }
        self.touch_dir(&mut parent);
        self.put_inode(tx, &mut parent)?;
        Ok(di)
    }

    fn link_in(&mut self, tx: &mut Tx, dir_ino: u32, name: &[u8], ino: u32) -> Result<(), i64> {
        dir::check_name(name)?;
        let mut parent = self.dir_inode(tx, dir_ino)?;
        let mut t = self.inode(Some(tx), ino)?;
        if t.is_dir() {
            return Err(EPERM);
        }
        if t.links() >= LINK_MAX {
            return Err(EMLINK);
        }
        if t.links() == 0 {
            // An O_TMPFILE-like inode on the orphan list gets its first name (`ext4_link`).
            self.orphan_del(tx, ino)?;
            t = self.inode(Some(tx), ino)?;
            t.set_dtime(0);
        }
        let keep = self.keep_for(None);
        self.add_name(tx, &mut parent, name, ino, dir::ftype_of_mode(t.mode()), keep)?;
        t.set_links(t.links() + 1);
        t.set_time(Stamp::C, self.now());
        self.put_inode(tx, &mut t)?;
        self.touch_dir(&mut parent);
        self.put_inode(tx, &mut parent)
    }

    /// unlink/rmdir; returns whether the victim must be freed now (no VFS holder).
    fn remove_in(&mut self, tx: &mut Tx, dir_ino: u32, name: &[u8], ino: u32) -> Result<bool, i64> {
        let mut parent = self.dir_inode(tx, dir_ino)?;
        let slot = self.find(Some(tx), &parent, name)?.ok_or(ENOENT)?;
        if slot.e.ino != ino {
            klog::event!(Warn, "ext4", "remove.mismatch", dir = dir_ino, want = ino, found = slot.e.ino);
            return Err(ENOENT);
        }
        let mut v = self.inode(Some(tx), ino)?;
        if v.is_dir() {
            if !self.is_empty_dir(&v)? {
                return Err(ENOTEMPTY);
            }
            v.set_links(0);
            Self::dec_dir_links(&mut parent);
        } else {
            v.set_links(v.links().saturating_sub(1));
        }
        self.remove_name(tx, &parent, &slot)?;
        self.touch_dir(&mut parent);
        self.put_inode(tx, &mut parent)?;
        v.set_time(Stamp::C, self.now());
        let gone = v.links() == 0;
        if gone {
            self.last_link(tx, &mut v);
        }
        self.put_inode(tx, &mut v)?;
        Ok(gone && !self.nodes.get(&ino).is_some_and(|n| n.holders > 0))
    }

    fn rename_in(&mut self, tx: &mut Tx, odir: u32, oname: &[u8], ino: u32, ndir: u32, nname: &[u8], target: Option<u32>) -> Result<Option<u32>, i64> {
        dir::check_name(nname)?;
        let mut dirs: BTreeMap<u32, DiskInode> = BTreeMap::new();
        dirs.insert(odir, self.dir_inode(tx, odir)?);
        if ndir != odir {
            dirs.insert(ndir, self.dir_inode(tx, ndir)?);
        }
        let oslot = self.find(Some(tx), &dirs[&odir], oname)?.ok_or(ENOENT)?;
        if oslot.e.ino != ino {
            return Err(ENOENT);
        }
        let mut mv = self.inode(Some(tx), ino)?;
        let mv_dir = mv.is_dir();
        let overflow = self.may_overflow(&dirs[&ndir]);
        let ft = dir::ftype_of_mode(mv.mode());
        let mut victim = None;
        match target {
            Some(t) => {
                let nd = dirs[&ndir].clone();
                let ts = self.find(Some(tx), &nd, nname)?.ok_or(ENOENT)?;
                if ts.e.ino != t {
                    return Err(ENOENT);
                }
                let mut tv = self.inode(Some(tx), t)?;
                if tv.is_dir() {
                    if !self.is_empty_dir(&tv)? {
                        return Err(ENOTEMPTY);
                    }
                    tv.set_links(0);
                    Self::dec_dir_links(dirs.get_mut(&ndir).ok_or(EUCLEAN)?);
                } else {
                    tv.set_links(tv.links().saturating_sub(1));
                }
                self.retarget_name(tx, &nd, &ts, ino, ft)?;
                tv.set_time(Stamp::C, self.now());
                if tv.links() == 0 {
                    self.last_link(tx, &mut tv);
                    if !self.nodes.get(&t).is_some_and(|n| n.holders > 0) {
                        victim = Some(t);
                    }
                }
                self.put_inode(tx, &mut tv)?;
            }
            None => {
                let mut nd = dirs[&ndir].clone();
                let keep = self.keep_for(None);
                self.add_name(tx, &mut nd, nname, ino, ft, keep)?;
                dirs.insert(ndir, nd);
            }
        }
        let od = dirs[&odir].clone();
        let oslot = self.find(Some(tx), &od, oname)?.ok_or(EUCLEAN)?;
        self.remove_name(tx, &od, &oslot)?;
        if mv_dir && odir != ndir {
            if mv.flags() & FL_INDEX != 0 {
                self.unindex(tx, &mut mv)?;
            }
            let dd = self.dotdot(Some(tx), &mv)?;
            self.retarget_name(tx, &mv, &dd, ndir, FT_DIR)?;
            Self::dec_dir_links(dirs.get_mut(&odir).ok_or(EUCLEAN)?);
            Self::inc_dir_links(dirs.get_mut(&ndir).ok_or(EUCLEAN)?, overflow)?;
        }
        mv.set_time(Stamp::C, self.now());
        self.put_inode(tx, &mut mv)?;
        for (_, mut d) in dirs {
            self.touch_dir(&mut d);
            self.put_inode(tx, &mut d)?;
        }
        Ok(victim)
    }

    fn exchange_in(&mut self, tx: &mut Tx, adir: u32, aname: &[u8], a: u32, bdir: u32, bname: &[u8], b: u32) -> Result<(), i64> {
        let mut dirs: BTreeMap<u32, DiskInode> = BTreeMap::new();
        dirs.insert(adir, self.dir_inode(tx, adir)?);
        dirs.insert(bdir, self.dir_inode(tx, bdir)?);
        let sa = self.find(Some(tx), &dirs[&adir], aname)?.ok_or(ENOENT)?;
        let sb = self.find(Some(tx), &dirs[&bdir], bname)?.ok_or(ENOENT)?;
        if sa.e.ino != a || sb.e.ino != b {
            return Err(ENOENT);
        }
        let mut ia = self.inode(Some(tx), a)?;
        let mut ib = self.inode(Some(tx), b)?;
        let da = dirs[&adir].clone();
        let db = dirs[&bdir].clone();
        self.retarget_name(tx, &da, &sa, b, dir::ftype_of_mode(ib.mode()))?;
        self.retarget_name(tx, &db, &sb, a, dir::ftype_of_mode(ia.mode()))?;
        if adir != bdir {
            for (n, from, to) in [(&mut ia, adir, bdir), (&mut ib, bdir, adir)] {
                if n.is_dir() {
                    if n.flags() & FL_INDEX != 0 {
                        self.unindex(tx, n)?;
                    }
                    let dd = self.dotdot(Some(tx), n)?;
                    self.retarget_name(tx, n, &dd, to, FT_DIR)?;
                    Self::dec_dir_links(dirs.get_mut(&from).ok_or(EUCLEAN)?);
                    let overflow = self.may_overflow(&dirs[&to]);
                    Self::inc_dir_links(dirs.get_mut(&to).ok_or(EUCLEAN)?, overflow)?;
                }
            }
        }
        let t = self.now();
        for n in [&mut ia, &mut ib] {
            n.set_time(Stamp::C, t);
        }
        self.put_inode(tx, &mut ia)?;
        self.put_inode(tx, &mut ib)?;
        for (_, mut d) in dirs {
            self.touch_dir(&mut d);
            self.put_inode(tx, &mut d)?;
        }
        Ok(())
    }
}

impl Volume {
    pub fn create(&self, dir: u32, name: &[u8], mode: u16, uid: u32, gid: u32, kind: Kind) -> Result<DiskInode, i64> {
        let mut st = self.lock_rw()?;
        st.migrate(dir)?;
        st.with_tx(|st, tx| st.create_in(tx, dir, name, mode, uid, gid, kind))
    }

    pub fn link(&self, dir: u32, name: &[u8], ino: u32) -> Result<(), i64> {
        let mut st = self.lock_rw()?;
        st.migrate(dir)?;
        st.with_tx(|st, tx| st.link_in(tx, dir, name, ino))
    }

    pub fn remove(&self, dir: u32, name: &[u8], ino: u32) -> Result<(), i64> {
        let mut st = self.lock_rw()?;
        if st.with_tx(|st, tx| st.remove_in(tx, dir, name, ino))? {
            st.evict(ino, true)?;
        }
        Ok(())
    }

    pub fn rename(&self, odir: u32, oname: &[u8], ino: u32, ndir: u32, nname: &[u8], target: Option<u32>) -> Result<(), i64> {
        let mut st = self.lock_rw()?;
        st.migrate(ndir)?;
        if let Some(v) = st.with_tx(|st, tx| st.rename_in(tx, odir, oname, ino, ndir, nname, target))? {
            st.evict(v, true)?;
        }
        Ok(())
    }

    pub fn exchange(&self, adir: u32, aname: &[u8], a: u32, bdir: u32, bname: &[u8], b: u32) -> Result<(), i64> {
        let mut st = self.lock_rw()?;
        st.with_tx(|st, tx| st.exchange_in(tx, adir, aname, a, bdir, bname, b))
    }
}
