//! BIGDIR: power cut at every block write of a burst of 200 changes (creates, renames in place, unlinks; an
//! fsync every 20) in a folder of 5000 names, on a volume with a journal. After the replay the folder is the
//! state after some number of the changes, in their order (each name the old or the new state, never both or
//! neither), at least the changes an fsync had returned for; e2fsck finds nothing after V2's replay, and
//! replays the cut log itself to a clean volume.

use super::bigdir_dev::steam_name;
use super::util::*;
use crate::inode::ROOT;
use crate::{Kind, MemDev, Volume};
use std::collections::BTreeMap;
use std::format;
use std::sync::Arc;
use std::vec::Vec;

type Model = BTreeMap<Vec<u8>, u32>;

enum Change {
    Create(Vec<u8>),
    Rename(Vec<u8>, Vec<u8>),
    Unlink(Vec<u8>),
}

const NAMES: usize = 5000;
const CHANGES: usize = 200;

fn plan() -> Vec<Change> {
    (0..CHANGES)
        .map(|i| match i % 5 {
            0 | 1 => Change::Create(steam_name(i, "burst-")),
            2 | 3 => Change::Rename(steam_name(i * 23 % NAMES, "n"), steam_name(i, "renamed-")),
            _ => Change::Unlink(steam_name((i * 23 + 11) % NAMES, "n")),
        })
        .collect()
}

/// Applies `c` to the volume (when given) and to the model.
fn apply(v: Option<&Volume>, d: u32, m: &mut Model, c: &Change, new_ino: u32) -> u32 {
    match c {
        Change::Create(n) => {
            let ino = match v {
                Some(v) => v.create(d, n, 0o100644, 1000, 1000, Kind::Reg).unwrap().ino,
                None => new_ino,
            };
            m.insert(n.clone(), ino);
            ino
        }
        Change::Rename(o, n) => {
            let ino = m.remove(o).unwrap();
            if let Some(v) = v {
                v.rename(d, o, ino, d, n, None).unwrap();
            }
            m.insert(n.clone(), ino);
            0
        }
        Change::Unlink(n) => {
            let ino = m.remove(n).unwrap();
            if let Some(v) = v {
                v.remove(d, n, ino).unwrap();
            }
            0
        }
    }
}

/// How many of the changes the folder `got` shows: it must be exactly the state after that many.
fn changes_in(base: &Model, got: &Model, plan: &[Change], inos: &[u32], what: &str) -> usize {
    let applied = |c: &Change| match c {
        Change::Create(n) => got.contains_key(n),
        Change::Rename(o, n) => {
            assert!(got.contains_key(o) != got.contains_key(n), "{what}: a renamed file has both names or none");
            got.contains_key(n)
        }
        Change::Unlink(n) => !got.contains_key(n),
    };
    let k = plan.iter().take_while(|c| applied(c)).count();
    let mut want = base.clone();
    for (c, ino) in plan.iter().zip(inos).take(k) {
        apply(None, 0, &mut want, c, *ino);
    }
    assert!(*got == want, "{what}: the folder is not the state after {k} changes ({} names, {} wanted)", got.len(), want.len());
    k
}

#[test]
fn a_burst_in_a_big_folder_cut_at_every_block_write() {
    for (layout, o, step) in [("4k-v3", &["-b", "4096", "-N", "8192"][..], 1usize), ("1k-nocsum", &["-b", "1024", "-N", "8192", "-O", "^64bit,^metadata_csum"], 5)] {
        let s = Scratch::new();
        let (dev, v) = mount(mkfs(&s, "24M", o, None));
        let d = v.create(ROOT, b"big", 0o40755, 1000, 1000, Kind::Dir).unwrap().ino;
        let mut model = Model::new();
        for i in 0..NAMES {
            let n = steam_name(i, "n");
            model.insert(n.clone(), v.create(d, &n, 0o100644, 1000, 1000, Kind::Reg).unwrap().ino);
        }
        v.sync_fs().unwrap();
        let base = model.clone();
        let before = dev.image();
        let bs = v.geo.bs as usize;
        dev.record();
        let w0 = dev.writes() as usize;
        let plan = plan();
        let mut inos = Vec::new();
        // (device requests done when an fsync returned, changes made by then)
        let mut synced = Vec::new();
        for (i, c) in plan.iter().enumerate() {
            inos.push(apply(Some(&v), d, &mut model, c, 0));
            if i % 20 == 19 {
                v.flush().unwrap();
                synced.push((dev.writes() as usize - w0, i + 1));
            }
        }
        assert_eq!(v.dirx_check(d), Ok(Some(model.len())), "{layout}: the index after the burst");
        v.sync_fs().unwrap();
        let log = dev.take_log();
        drop(v);
        // Single blocks with the request each came from (a cut can fall inside a request).
        let mut blocks: Vec<(usize, u64, Vec<u8>)> = Vec::new();
        for (i, (at, data)) in log.iter().filter(|(a, _)| !crate::dev::is_mark(*a)).enumerate() {
            for (k, c) in data.chunks(bs).enumerate() {
                blocks.push((i + 1, at + (k * bs) as u64, c.to_vec()));
            }
        }
        assert!(blocks.len() > 300, "{layout}: {} block writes", blocks.len());
        let mut img = before.clone();
        let mut seen = std::collections::BTreeSet::new();
        for k in 0..=blocks.len() {
            if k > 0 {
                let (_, at, data) = &blocks[k - 1];
                img[*at as usize..*at as usize + data.len()].copy_from_slice(data);
            }
            if k % step != 0 && k != blocks.len() {
                continue;
            }
            let what = format!("{layout}: cut after block write {k} of {}", blocks.len());
            let req = if k == blocks.len() { blocks.last().map_or(0, |b| b.0) } else { blocks[k].0 - 1 };
            let cut = Arc::new(MemDev::new(img.clone()));
            let v = Volume::mount(cut.clone(), opts()).unwrap_or_else(|e| panic!("{what}: mount {e}"));
            let got: Model = v.list(d).unwrap().into_iter().map(|(n, i, _)| (n, i)).collect();
            let n = changes_in(&base, &got, &plan, &inos, &what);
            let durable = synced.iter().filter(|(at, _)| *at <= req).map(|(_, c)| *c).max().unwrap_or(0);
            assert!(n >= durable, "{what}: {n} changes survived, an fsync had returned for {durable}");
            seen.insert(n);
            if k % (8 * step) == 0 || k == blocks.len() {
                // A change after the replay goes through the index built from the replayed folder.
                let extra = v.create(d, b"after-the-cut", 0o100644, 1000, 1000, Kind::Reg).unwrap().ino;
                v.remove(d, b"after-the-cut", extra).unwrap();
                assert_eq!(v.dirx_check(d), Ok(Some(got.len())), "{what}");
                v.sync_fs().unwrap();
                drop(v);
                assert_clean(&s, &cut.image(), &what);
            }
            if k % (40 * step) == 0 {
                let (r1, r2, out) = repair(&s, &img);
                assert!(r1 <= 1 && r2 == 0, "{what}: e2fsck's own replay: -fy {r1}, then -fn {r2}\n{out}");
            }
        }
        assert!(seen.len() >= 8 && seen.contains(&0) && seen.contains(&CHANGES), "{layout}: the cuts showed only these states: {seen:?}");
    }
}
