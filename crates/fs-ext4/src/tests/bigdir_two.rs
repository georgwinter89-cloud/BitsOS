//! BIGDIR hardening: a folder of two blocks, the index compared with a fresh pass after every single change. A
//! wrong place or a wrong room for one entry is forgotten and built again by the next change that meets it, so a
//! check after a whole phase can miss it; here nothing happens between a change and its check. The changes aim
//! at the edges: an unlink and then a name into the room it freed, names that only fit where two freed entries
//! were merged, the first and the last entry of a block, a rename in place within one block and across the two.

use super::bigdir::{listing, Model};
use super::bigdir_dev::mount_counted;
use super::util::*;
use crate::inode::ROOT;
use crate::{Kind, Volume};
use std::format;
use std::vec::Vec;

fn name(i: usize, len: usize) -> Vec<u8> {
    let mut n = format!("n{i:05}-").into_bytes();
    n.resize(len.max(n.len()), b'x');
    n
}

struct Folder<'a> {
    v: &'a Volume,
    d: u32,
    model: Model,
    bs: u64,
    step: usize,
}

impl Folder<'_> {
    /// After every change: the index is there, is what a fresh pass builds, nothing was ever stale, the listing
    /// is the model, and the folder still has its two blocks.
    fn check(&mut self, what: &str) {
        self.step += 1;
        let at = format!("step {} ({what})", self.step);
        assert_eq!(self.v.dirx_check(self.d), Ok(Some(self.model.len())), "{at}");
        assert_eq!(self.v.dirx_stales(self.d), (0, false), "{at}: an index was found stale");
        assert!(listing(self.v, self.d) == self.model, "{at}: the folder is not what the model says");
        assert_eq!(self.v.stat(self.d).unwrap().size(), 2 * self.bs, "{at}: the folder no longer has two blocks");
    }
    fn add(&mut self, n: Vec<u8>) {
        let ino = self.v.create(self.d, &n, 0o100644, 1000, 1000, Kind::Reg).unwrap().ino;
        assert!(self.model.insert(n, ino).is_none());
        self.check("create");
    }
    fn del(&mut self, n: &[u8]) {
        let ino = self.model.remove(n).unwrap();
        self.v.remove(self.d, n, ino).unwrap();
        self.check("unlink");
    }
    fn mv(&mut self, old: &[u8], new: Vec<u8>) {
        let ino = self.model.remove(old).unwrap();
        self.v.rename(self.d, old, ino, self.d, &new, None).unwrap();
        self.model.insert(new, ino);
        self.check("rename in place");
    }
    /// The names in the order of their entries on the disk, per block.
    fn by_block(&self) -> [Vec<Vec<u8>>; 2] {
        let st = self.v.lock();
        let di = st.inode(None, self.d).unwrap();
        let mut out = [Vec::new(), Vec::new()];
        for n in self.model.keys() {
            let s = st.find(None, &di, n).unwrap().unwrap();
            out[s.lblk as usize].push((s.e.off, n.clone()));
        }
        out.map(|mut b| {
            b.sort();
            b.into_iter().map(|(_, n)| n).collect()
        })
    }
}

#[test]
fn a_two_block_folder_is_exact_after_every_single_change() {
    let layouts: [(&str, &[&str], u64); 3] = [("1k", &["-b", "1024", "-N", "2048", "-O", "^has_journal"], 1024), ("4k-journal", &["-b", "4096", "-N", "2048"], 4096), ("1k-nocsum", &["-b", "1024", "-N", "2048", "-O", "^64bit,^metadata_csum"], 1024)];
    for (layout, o, bs) in layouts {
        let s = Scratch::new();
        let (dev, v) = mount_counted(mkfs(&s, "16M", o, None));
        let d = v.create(ROOT, b"two", 0o40755, 1000, 1000, Kind::Dir).unwrap().ino;
        let mut model = Model::new();
        // Fill the first block and half of the second.
        let mut i = 0;
        while v.stat(d).unwrap().size() < 2 * bs {
            let n = name(i, 20);
            model.insert(n.clone(), v.create(d, &n, 0o100644, 1000, 1000, Kind::Reg).unwrap().ino);
            i += 1;
        }
        let first = i;
        let mut f = Folder { v: &v, d, model, bs, step: 0 };
        f.check("the second block");
        for _ in 0..first / 2 {
            f.add(name(i, 20));
            i += 1;
        }
        // An unlink, then a name of the same length: it goes into the room just freed (the fullest block that
        // fits), at the offset the index must have right.
        for k in (0..first).step_by(3) {
            f.del(&name(k, 20));
            f.add(name(i, 20));
            i += 1;
        }
        // Edges of each block: its first and last entry go and come back; then shorter and longer names.
        for round in 0..3 {
            let blocks = f.by_block();
            for b in &blocks {
                let (head, tail) = (b[0].clone(), b[b.len() - 1].clone());
                f.del(&head);
                f.del(&tail);
                f.add(name(i, 12 + round * 9));
                f.add(name(i + 1, 30 - round * 9));
                i += 2;
            }
        }
        // Two neighbours go, their room merges; a name that fits only the merged room comes.
        for round in 0..4 {
            let blocks = f.by_block();
            let b = &blocks[round % 2];
            let at = b.len() / 2;
            let (x, y) = (b[at].clone(), b[at + 1].clone());
            f.del(&x);
            f.del(&y);
            f.add(name(i, 44));
            i += 1;
        }
        // Renames in place: within a block and from one block to the other, to shorter, equal and longer names.
        for round in 0..30 {
            let blocks = f.by_block();
            let b = &blocks[round % 2];
            let old = b[(round * 7) % b.len()].clone();
            f.mv(&old, name(i, [8, old.len(), 26, 40][round % 4]));
            i += 1;
        }
        // A mix until the folder is nearly empty, then full again.
        let names: Vec<Vec<u8>> = f.model.keys().cloned().collect();
        for (k, n) in names.iter().enumerate() {
            if k % 5 != 0 {
                f.del(n);
            }
        }
        while f.by_block().iter().map(Vec::len).sum::<usize>() < first {
            f.add(name(i, 20 + i % 3 * 4));
            i += 1;
        }
        std::println!("bigdir two blocks, {layout}: {} single changes, each followed by the comparison", f.step);
        assert!(f.step > 150);
        v.sync_fs().unwrap();
        assert_clean(&s, &dev.inner.image(), &format!("{layout}: a two-block folder"));
    }
}
