//! BIGDIR: every kind of change against a model on every layout, with shared hashes, and in an htree folder;
//! after each round the index is compared with a fresh pass, a fresh mount with the model, and e2fsck is clean.

use super::bigdir::{listing, make, settled, Meter, Model};
use super::bigdir_dev::{mount_counted, steam_name};
use super::util::*;
use crate::inode::ROOT;
use crate::Kind;
use std::format;

/// Both block sizes, with and without checksums and a journal, and once with only 16 different hashes (every
/// lookup then meets other names under its hash): a mix of every change against the model, the index compared
/// with a fresh pass after each round.
#[test]
fn every_change_keeps_the_index_exact_on_every_layout() {
    let layouts: [(&str, &[&str], u32); 4] = [
        ("4k-journal", &["-b", "4096", "-N", "8192"], u32::MAX),
        ("1k-nocsum", &["-b", "1024", "-N", "8192", "-O", "^64bit,^metadata_csum"], u32::MAX),
        ("4k-nojournal", &["-b", "4096", "-N", "8192", "-O", "^has_journal"], u32::MAX),
        ("4k-shared-hashes", &["-b", "4096", "-N", "8192"], 0xF),
    ];
    for (layout, o, mask) in layouts {
        let s = Scratch::new();
        let (dev, v) = mount_counted(mkfs(&s, "48M", o, None));
        v.dirx_hash_mask(mask);
        let a = v.create(ROOT, b"a", 0o40755, 0, 0, Kind::Dir).unwrap().ino;
        let b = v.create(ROOT, b"b", 0o40755, 0, 0, Kind::Dir).unwrap().ino;
        let (mut ma, mut mb) = (Model::new(), Model::new());
        let mut m = Meter::new(&v, &dev);
        for i in 0..3000 {
            make(&mut m, &mut ma, a, steam_name(i, "x"));
        }
        let mut rng = super::lay::Rng(0x9E37_79B9_7F4A_7C15 ^ layout.len() as u64);
        let mut next = 3000usize;
        for round in 0..6 {
            for _ in 0..500 {
                let pick = (0..4).fold(0usize, |a, _| a << 1 | rng.bit() as usize) % 8;
                let some = |m: &Model, r: &mut super::lay::Rng| {
                    let skip = (0..10).fold(0usize, |a, _| a << 1 | r.bit() as usize) % m.len().max(1);
                    m.iter().nth(skip).map(|(n, i)| (n.clone(), *i))
                };
                match pick {
                    0 | 1 => {
                        next += 1;
                        make(&mut m, &mut ma, a, steam_name(next, "x"));
                    }
                    2 => {
                        if let Some((n, i)) = some(&ma, &mut rng) {
                            v.remove(a, &n, i).unwrap();
                            ma.remove(&n);
                        }
                    }
                    3 => {
                        if let Some((n, i)) = some(&ma, &mut rng) {
                            next += 1;
                            let new = steam_name(next, "r");
                            v.rename(a, &n, i, a, &new, None).unwrap();
                            ma.remove(&n);
                            ma.insert(new, i);
                        }
                    }
                    4 => {
                        if let Some((n, i)) = some(&ma, &mut rng) {
                            next += 1;
                            let new = steam_name(next, "m");
                            v.rename(a, &n, i, b, &new, None).unwrap();
                            ma.remove(&n);
                            mb.insert(new, i);
                        }
                    }
                    5 => {
                        if let (Some((na, ia)), Some((nb, ib))) = (some(&ma, &mut rng), some(&mb, &mut rng)) {
                            v.exchange(a, &na, ia, b, &nb, ib).unwrap();
                            ma.insert(na, ib);
                            mb.insert(nb, ia);
                        }
                    }
                    6 => {
                        // Two names of one folder change places (often in one block).
                        if let (Some((n1, i1)), Some((n2, i2))) = (some(&ma, &mut rng), some(&ma, &mut rng)) {
                            if n1 != n2 {
                                v.exchange(a, &n1, i1, a, &n2, i2).unwrap();
                                ma.insert(n1, i2);
                                ma.insert(n2, i1);
                            }
                        }
                    }
                    _ => {
                        // A folder with a name in it moves to the other folder (its ".." follows), comes back
                        // under a name that exists there (an empty folder it replaces) and goes.
                        next += 3;
                        let (n1, n2, n3) = (steam_name(next, "dir-"), steam_name(next + 1, "dir-"), steam_name(next + 2, "dir-"));
                        let d1 = v.create(a, &n1, 0o40755, 0, 0, Kind::Dir).unwrap().ino;
                        let inner = v.create(d1, b"inner", 0o100644, 0, 0, Kind::Reg).unwrap().ino;
                        v.rename(a, &n1, d1, b, &n2, None).unwrap();
                        let d3 = v.create(a, &n3, 0o40755, 0, 0, Kind::Dir).unwrap().ino;
                        v.rename(b, &n2, d1, a, &n3, Some(d3)).unwrap();
                        assert_eq!(v.remove(a, &n3, d1).err(), Some(crate::errno::ENOTEMPTY));
                        v.remove(d1, b"inner", inner).unwrap();
                        v.remove(a, &n3, d1).unwrap();
                    }
                }
            }
            let what = format!("{layout} round {round}");
            let mut dirs = std::vec![(a, &ma)];
            if !mb.is_empty() {
                dirs.push((b, &mb));
            }
            settled(&s, &v, &dev, &dirs, &what);
        }
    }
}

/// An htree folder (from `e2fsck -D`): unlinks and renames out of it go through the index while it stays an
/// htree on the disk (its index blocks have no room); the first add turns it linear and builds the index anew.
#[test]
fn an_htree_folder_is_changed_through_the_index() {
    let s = Scratch::new();
    let t = tree(&s);
    let img = mkfs(&s, "64M", &["-b", "4096"], Some(&t));
    let p = s.path("idx.img");
    std::fs::write(&p, &img).unwrap();
    assert!(std::process::Command::new("e2fsck").args(["-fyD"]).arg(&p).output().unwrap().status.code().unwrap() <= 1);
    let (dev, v) = mount_counted(std::fs::read(&p).unwrap());
    let many = lookup(&v, "/many");
    assert!(v.stat(many).unwrap().flags() & crate::inode::FL_INDEX != 0, "/many is not an htree");
    let mut model = listing(&v, many);
    assert_eq!(model.len(), 600);
    let mut m = Meter::new(&v, &dev);
    // The first change of each folder builds its index (not metered).
    m.run(|v| v.rename(many, b"entry-with-a-long-name-0000", model.remove(&b"entry-with-a-long-name-0000"[..]).unwrap(), ROOT, b"out-0", None).unwrap());
    let mut m = Meter::new(&v, &dev);
    for i in (1..600).step_by(5) {
        let n = format!("entry-with-a-long-name-{i:04}").into_bytes();
        let ino = model.remove(&n).unwrap();
        if i % 2 == 0 {
            m.run(|v| v.remove(many, &n, ino).unwrap());
        } else {
            m.run(|v| v.rename(many, &n, ino, ROOT, format!("out-{i}").as_bytes(), None).unwrap());
        }
    }
    assert!(m.worst.walks == 0 && m.worst.dir_reads <= 2, "changes of the htree folder: {:?}", m.worst);
    assert!(v.stat(many).unwrap().flags() & crate::inode::FL_INDEX != 0, "removing names keeps the htree");
    assert_eq!(v.dirx_check(many), Ok(Some(model.len())));
    v.sync_fs().unwrap();
    assert_clean(&s, &dev.inner.image(), "an htree folder after unlinks and renames out");
    for i in 0..300 {
        make(&mut m, &mut model, many, steam_name(i, "into-the-htree-"));
    }
    assert!(v.stat(many).unwrap().flags() & crate::inode::FL_INDEX == 0, "an add turns the folder linear");
    settled(&s, &v, &dev, &[(many, &model)], "an htree folder turned linear");
}
