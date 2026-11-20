//! FS-W2 (the checker's probe, taken over): a directory's blocks (journalled, with copies in the log and home
//! writes owed) are freed and taken by a file's data written straight to the disk; a cut at every device call, the
//! unflushed writes lost at random. Neither a replay nor a late home write may put a directory block over the
//! file's data (revoke records, `journal::State::join`); e2fsck finds nothing.
use super::disk::*;
use super::flight::{pattern, read_all, step, MIB};
use super::lay::{cut_image, Rng};
use super::util::*;
use super::write::file;
use crate::errno::ENOSPC;
use crate::inode::ROOT;
use crate::{Kind, Volume};
use std::collections::BTreeSet;
use std::format;
use std::string::String;
use std::sync::Arc;
use std::vec;
use std::vec::Vec;

fn long(i: usize) -> String {
    format!("an-entry-with-a-name-long-enough-to-fill-directory-blocks-quickly-{i:05}-{}", "x".repeat(120))
}

fn fill(v: &Volume, name: &str) -> u32 {
    let f = file(v, ROOT, name);
    let junk = vec![0x5Au8; MIB];
    let mut at = 0u64;
    for piece in [MIB, 64 << 10, 4096] {
        loop {
            match step(v, f, at, &junk[..piece], at + piece as u64) {
                Ok(()) => at += piece as u64,
                Err(ENOSPC) => break,
                Err(e) => panic!("filling the volume: errno {e}"),
            }
        }
    }
    f
}

#[test]
fn directory_blocks_reused_as_file_data_under_a_cut() {
    let s = Scratch::new();
    let (ld, _dev, v) = disk_volume(&s, "32M", &["-b", "4096", "-m", "0"]);
    let d = v.create(ROOT, b"d", 0o40755, 1000, 1000, Kind::Dir).unwrap().ino;
    let names: Vec<(String, u32)> = (0..300).map(|i| (long(i), file(&v, d, &long(i)))).collect();
    let newf = file(&v, ROOT, "new");
    fill(&v, "fill");
    v.sync_fs().unwrap();
    let dir_blocks: BTreeSet<u64> = blocks_of(&v, d).into_iter().collect();
    let before = ld.snapshot();
    ld.record();
    // Changes to every directory block, committed (logged copies, home writes), then the directory goes.
    for (name, ino) in names.iter().step_by(2) {
        v.remove(d, name.as_bytes(), *ino).unwrap();
    }
    v.flush().unwrap();
    for (name, ino) in names.iter().skip(1).step_by(2) {
        v.remove(d, name.as_bytes(), *ino).unwrap();
    }
    v.remove(ROOT, b"d", d).unwrap();
    let free = v.st.lock().free_now() as usize;
    let want = pattern(free * 4096, 91);
    let mut len = 0;
    while len < want.len() && step(&v, newf, len as u64, &want[len..len + 4096], (len + 4096) as u64).is_ok() {
        len += 4096;
        if len % (8 * 4096) == 0 {
            v.flush().unwrap();
        }
    }
    v.flush().unwrap();
    // More commits, and the file rewritten in place: later home writes and flushes.
    for i in 0..3 {
        file(&v, ROOT, &format!("later-{i}"));
        v.flush().unwrap();
    }
    let log = ld.take_log();
    let reused = blocks_of(&v, newf).into_iter().filter(|b| dir_blocks.contains(b)).count();
    std::eprintln!("dir: new file {len} bytes, {reused} of the directory's {} blocks reused, {} device calls", dir_blocks.len(), log.len());
    assert!(reused >= 3);
    drop(v);
    let mut bad = Vec::new();
    let mut rng = Rng(0x7777_1234_ABCD_0001);
    for k in (0..=log.len()).flat_map(|k| [k; 4]) {
        let mut kept = || rng.bit();
        let (img, flush) = cut_image(&before, &log, k, 4096, Some(&mut kept));
        let what = format!("cut after {k} of {} device calls (last flush at {flush})", log.len());
        let dev = Arc::new(crate::MemDev::new(img));
        let vol = match Volume::mount(dev.clone(), opts()) {
            Ok(v) => v,
            Err(e) => {
                bad.push(format!("{what}: mount errno {e}"));
                continue;
            }
        };
        let root = vol.list(ROOT).unwrap();
        let ino = root.iter().find(|e| e.0 == b"new").map(|e| e.1).unwrap();
        let got = read_all(&vol, ino);
        if got.len() > len || got != want[..got.len()] {
            let blk = got.chunks(4096).zip(want.chunks(4096)).position(|(a, b)| a != b).unwrap_or(0);
            bad.push(format!("{what}: block {blk} of new (size {}) is not its data", got.len()));
        }
        if let Some(di) = root.iter().find(|e| e.0 == b"d").map(|e| e.1) {
            if let Err(e) = vol.list(di) {
                bad.push(format!("{what}: the directory does not list: errno {e}"));
            }
        }
        vol.sync_fs().unwrap();
        drop(vol);
        let (code, out) = fsck(&s, &dev.image());
        if code != 0 {
            bad.push(format!("{what}: e2fsck {code}: {}", out.lines().take(4).collect::<Vec<_>>().join(" | ")));
        }
    }
    assert!(bad.is_empty(), "{} bad findings, first:\n{}", bad.len(), bad.iter().take(8).cloned().collect::<Vec<_>>().join("\n"));
}
