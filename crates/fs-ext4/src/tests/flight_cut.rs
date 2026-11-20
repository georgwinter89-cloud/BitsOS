//! FS-W: the power cut at every device write of a run of parallel writers (their own files, and two on one file),
//! with a journal (the writes the device's cache still held half lost and reordered, `lay::cut_image`) and without
//! one (a prefix). Every free block held old bytes of another file before the run, so a block that was mapped
//! before its data reached the device would show them: under each file's size there is only the run's data, or
//! the zeros of a hole. Where the cuts fall between the threads is timing; the window between a step's claim and
//! its mapping is cut deterministically in `flight_window`.

use super::cut::check_disk;
use super::flight::{pattern, read_all, step, MIB};
use super::lay::{cut_image, Rng};
use super::util::*;
use super::write::file;
use crate::inode::ROOT;
use crate::Volume;
use std::format;
use std::vec::Vec;

const STEP: usize = MIB / 4;
pub(super) const OLD: u8 = 0xEE;
/// (name, bytes, writers)
const FILES: [(&str, usize, usize); 3] = [("a", 6 * STEP + 100, 1), ("b", 5 * STEP, 1), ("shared", 8 * STEP + 5000, 2)];

fn want(k: usize) -> Vec<u8> {
    pattern(FILES[k].1, 40 + k as u32)
}

/// Fills most of the free space with `OLD` and frees it again: the free blocks keep the bytes.
pub(super) fn leave_old_bytes(v: &Volume) {
    let junk = file(v, ROOT, "junk");
    let bytes = v.st.lock().free_now() as usize * 4096 / 4 * 3 / MIB * MIB;
    let old = std::vec![OLD; MIB];
    for at in (0..bytes).step_by(MIB) {
        step(v, junk, at as u64, &old, bytes as u64).unwrap();
    }
    v.flush().unwrap();
    v.remove(ROOT, b"junk", junk).unwrap();
    v.sync_fs().unwrap();
}

/// The run: every writer its steps, an fsync in the middle (commits fall between other writers' steps).
fn run(v: &std::sync::Arc<Volume>, inos: &[u32]) {
    let mut threads = Vec::new();
    for (k, &(_, _, writers)) in FILES.iter().enumerate() {
        for t in 0..writers {
            let (w, ino) = (v.clone(), inos[k]);
            threads.push(std::thread::spawn(move || {
                let data = want(k);
                let steps = data.len().div_ceil(STEP);
                for i in (t..steps).step_by(writers) {
                    let at = i * STEP;
                    step(&w, ino, at as u64, &data[at..(at + STEP).min(data.len())], data.len() as u64).unwrap();
                    if i == steps / 2 {
                        w.flush().unwrap();
                    }
                }
            }));
        }
    }
    threads.into_iter().for_each(|t| t.join().unwrap());
    v.flush().unwrap();
}

/// Mounts the cut image and reads every file: only the run's data (or a hole's zeros in the file two writers
/// shared) under its size. Returns the volume for the caller's check of the disk, and whether all files are whole.
fn only_written_data(img: Vec<u8>, what: &str) -> (std::sync::Arc<crate::MemDev>, std::sync::Arc<Volume>, bool) {
    let (dev, v) = mount(img);
    let mut whole = true;
    for (k, &(name, len, writers)) in FILES.iter().enumerate() {
        let ino = lookup(&v, &format!("/{name}"));
        let got = read_all(&v, ino);
        let data = want(k);
        assert!(got.len() <= len, "{what}: {name} is {} bytes", got.len());
        whole &= got.len() == len && got == data;
        for (i, blk) in got.chunks(4096).enumerate() {
            let same = blk == &data[i * 4096..i * 4096 + blk.len()];
            let hole = writers > 1 && blk.iter().all(|&b| b == 0);
            assert!(same || hole, "{what}: block {i} of {name} (size {}) is not the run's data{}", got.len(), if blk.contains(&OLD) { ": it holds another file's old bytes" } else { "" });
        }
    }
    (dev, v, whole)
}

#[test]
fn a_cut_at_every_write_of_parallel_writers_shows_only_written_data() {
    let layouts: [(&str, &[&str], bool); 2] = [("journal", &["-b", "4096"], true), ("bare", &["-b", "4096", "-O", "^has_journal"], false)];
    for (name, extra, journal) in layouts {
        let s = Scratch::new();
        let (dev, v) = mount(mkfs(&s, "32M", extra, None));
        leave_old_bytes(&v);
        let inos: Vec<u32> = FILES.iter().map(|f| file(&v, ROOT, f.0)).collect();
        v.sync_fs().unwrap();
        let before = dev.image();
        dev.record();
        run(&v, &inos);
        let log = dev.take_log();
        // The run's blocks did hold old bytes: the check below can see a mapping that came before its data.
        let di = v.stat(inos[0]).unwrap();
        let first = v.st.lock().extents(&di).unwrap()[0].pblk as usize;
        assert_eq!(before[first * 4096..first * 4096 + 4096], [OLD; 4096], "{name}: the run's first block was never used before");
        drop(v);

        let mut rng = Rng(0x2545_F491_4F6C_DD1D);
        let mut wholes = 0;
        for k in 0..=log.len() {
            // Reordering shows nothing about a journal-less disk (Linux's neither): its order is the prefix's.
            let mut kept = || rng.bit();
            let (img, flush) = cut_image(&before, &log, k, 4096, if journal { Some(&mut kept) } else { None });
            let what = format!("{name}: cut after {k} of {} writes (last flush at {flush})", log.len());
            let (cut, vol, whole) = only_written_data(img.clone(), &what);
            wholes += whole as u32;
            if journal {
                // The replay leaves nothing for e2fsck: no block marked used that no file points at.
                vol.sync_fs().unwrap();
                drop(vol);
                assert_clean(&s, &cut.image(), &what);
            } else {
                drop(vol);
                check_disk(&s, img, &what);
            }
        }
        assert!(wholes >= 1, "{name}: no cut showed the whole run");
        if !journal {
            // A journal-less disk whose cache also loses and reorders: nothing is promised about the data then
            // (Linux's neither), but e2fsck must still be able to repair such a cut (every fourth: four e2fsck
            // runs each).
            for k in (0..=log.len()).step_by(4) {
                let (img, flush) = cut_image(&before, &log, k, 4096, Some(&mut || rng.bit()));
                check_disk(&s, img, &format!("{name}: cut after {k} of {} writes (last flush at {flush}), half of the rest lost", log.len()));
            }
        }
    }
}
