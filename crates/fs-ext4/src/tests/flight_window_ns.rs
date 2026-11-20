//! FS-W: a file that loses its name, or is cut short, while one of its steps is in the window between claim and
//! mapping; every device write of the sequence is a cut (`flight_window::every_cut`).

use super::flight::{pattern, read_all, slow, step, wait_inside, Opener, MIB};
use super::flight_cut::leave_old_bytes;
use super::flight_window::{every_cut, foreign_fsync, holds, Parts, LAYOUTS};
use super::util::*;
use super::write::file;
use crate::inode::ROOT;
use std::format;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::vec::Vec;

/// The name goes in one transaction while the step's data is on the device; the file itself goes with the
/// step's end (nobody holds it) or with its last holder. Until the cut state has lost the name the file is what
/// it was before the step; afterwards its blocks are free again, never marked used and owned by no one.
#[test]
fn an_unlink_inside_the_window_cut_at_every_write() {
    for (name, extra, journal) in LAYOUTS {
        for held in [false, true] {
            let s = Scratch::new();
            let (dev, v) = mount(mkfs(&s, "32M", extra, None));
            leave_old_bytes(&v);
            let f = file(&v, ROOT, "f");
            let data = pattern(2 * MIB, 6);
            step(&v, f, 0, &data[..MIB], MIB as u64).unwrap();
            v.sync_fs().unwrap();
            let before = dev.image();
            dev.record();
            let hold = held.then(|| v.hold(f).unwrap());
            // Half over the file's blocks, half over a hole behind them.
            let mut p = Parts::begin(&v, f, MIB as u64 / 2, MIB);
            p.data(&v, &data[MIB / 2..MIB / 2 + MIB]);
            v.remove(ROOT, b"f", f).unwrap();
            foreign_fsync(&v, "h");
            p.end(&v, 3 * MIB as u64 / 2).unwrap();
            v.flush().unwrap();
            if let Some(h) = hold {
                assert!(read_all(&v, f) == data[..3 * MIB / 2], "{name}: the open file's step");
                v.forget(h.slot);
                // The next call on the volume frees a forgotten file.
                v.stat(ROOT).unwrap();
                v.flush().unwrap();
            }
            assert!(!v.st.lock().inode_in_use(f).unwrap(), "{name}, held {held}: the inode is still in use");
            let log = dev.take_log();
            drop(v);
            every_cut(&s, &format!("{name}, unlink in the window, held {held}"), journal, &before, &log, &|vol, what| {
                if vol.list(ROOT).unwrap().iter().any(|(n, _, _)| n == b"f") {
                    holds(vol, "/f", &data, &[MIB], what);
                }
            });
        }
    }
}

/// A truncate that waited for the step (two threads, the device holds the step's data), with another program's
/// commit while it waits. Under the size the cut shows there is only the file's data; the one exception is older
/// than FS-W and Linux has it too (`ext4_block_truncate_page` in ordered mode): `set_file_size` zeroes the rest
/// of the edge block in place before its transaction, so a cut between shows zeros from the new end to that
/// block's end under the old size.
#[test]
fn a_truncate_behind_the_step_cut_at_every_write() {
    const CUT: usize = 4096 + 100;
    for (name, extra, journal) in LAYOUTS {
        let s = Scratch::new();
        let (dev, v) = slow(&s, "32M", extra);
        leave_old_bytes(&v);
        let f = file(&v, ROOT, "f");
        let data = pattern(2 * MIB, 6);
        step(&v, f, 0, &data[..MIB], MIB as u64).unwrap();
        v.sync_fs().unwrap();
        let before = dev.mem.image();
        dev.mem.record();
        dev.shut.store(true, Ordering::Release);
        let open = Opener(dev.clone());
        let (w, wd) = (v.clone(), data.clone());
        let writer = std::thread::spawn(move || step(&w, f, MIB as u64, &wd[MIB..], 2 * MIB as u64));
        wait_inside(&dev, 1);
        let w = v.clone();
        let cut = std::thread::spawn(move || w.set_size(f, CUT as u64));
        std::thread::sleep(Duration::from_millis(100));
        assert!(!cut.is_finished(), "{name}: the truncate did not wait for the step");
        foreign_fsync(&v, "h");
        drop(open);
        writer.join().unwrap().unwrap();
        cut.join().unwrap().unwrap();
        v.flush().unwrap();
        let log = dev.mem.take_log();
        drop(v);
        every_cut(&s, &format!("{name}, truncate behind the step"), journal, &before, &log, &|vol, what| {
            let got = read_all(vol, lookup(vol, "/f"));
            assert!([MIB, 2 * MIB, CUT].contains(&got.len()), "{what}: f is {} bytes", got.len());
            let bad: Vec<usize> = (0..got.len()).filter(|&i| got[i] != data[i]).collect();
            let edge_only = bad.iter().all(|&i| (CUT..8192).contains(&i) && got[i] == 0);
            assert!(edge_only, "{what}: f ({} bytes) differs from its data in {} bytes from {}", got.len(), bad.len(), bad[0]);
        });
    }
}
