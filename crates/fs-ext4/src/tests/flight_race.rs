//! FS-W: what touches a file while one of its write-back steps is at the device (`flight`): truncate and fallocate
//! wait for the step, unlink and rename do not (the file's blocks go when the step ended), a freeze waits for every
//! step, and a device that fails under the step costs only the step.

use super::flight::{bounded, free_now, pattern, read_all, settled, slow, step, wait_inside, Opener, BOTH, MIB};
use super::util::*;
use super::write::file;
use crate::dev::Dev;
use crate::inode::ROOT;
use crate::Kind;
use std::format;
use std::sync::atomic::Ordering;
use std::time::Duration;

/// `f` on a thread that must still be running after a moment (it waits for the step at the device).
fn waits<T: Send + 'static>(what: &str, f: impl FnOnce() -> T + Send + 'static) -> std::thread::JoinHandle<T> {
    let t = std::thread::spawn(f);
    std::thread::sleep(Duration::from_millis(150));
    assert!(!t.is_finished(), "{what} did not wait for the file's step at the device");
    t
}

#[test]
fn a_truncate_and_a_fallocate_wait_for_the_step_of_their_file() {
    for layout in BOTH {
        let s = Scratch::new();
        let (dev, v) = slow(&s, "64M", layout);
        let what = format!("{layout:?}");
        let free = free_now(&v);
        let f = file(&v, ROOT, "f");
        let want = pattern(2 * MIB, 3);
        step(&v, f, 0, &want[..MIB], MIB as u64).unwrap();

        dev.shut.store(true, Ordering::Release);
        let open = Opener(dev.clone());
        let (w, wd) = (v.clone(), want.clone());
        let writer = std::thread::spawn(move || step(&w, f, MIB as u64, &wd[MIB..], 2 * MIB as u64));
        wait_inside(&dev, 1);
        let w = v.clone();
        let cut = waits("a truncate", move || w.set_size(f, 4096 + 100));
        let w = v.clone();
        let pre = waits("a fallocate", move || w.fallocate(f, 3 * MIB as u64, 8192));
        drop(open);
        assert_eq!(writer.join().unwrap(), Ok(()));
        assert_eq!(cut.join().unwrap(), Ok(()));
        assert_eq!(pre.join().unwrap(), Ok(()));

        // The step was whole before the truncate cut the file.
        assert_eq!(v.stat(f).unwrap().size(), 4096 + 100);
        assert!(read_all(&v, f) == want[..4096 + 100]);
        // And so the disk has it: the cut file, the fallocate's blocks behind it, nothing of the step's megabyte.
        v.sync_fs().unwrap();
        let img = dev.mem.image();
        assert_clean(&s, &img, &what);
        let (_d2, v2) = mount(img);
        let f2 = lookup(&v2, "/f");
        assert!(read_all(&v2, f2) == want[..4096 + 100], "{what}: the cut file after the remount");
        // Two blocks of data; the fallocate's two behind the end are there if it ran after the truncate.
        let blocks = v2.stat(f2).unwrap().blocks512(&v2.geo);
        assert!(blocks == 2 * 8 || blocks == 4 * 8, "{what}: the file has {blocks} sectors after the remount");
        drop(v2);
        v.remove(ROOT, b"f", f).unwrap();
        settled(&v, free, &what);
        v.sync_fs().unwrap();
        assert_clean(&s, &dev.mem.image(), &what);
    }
}

#[test]
fn an_unlink_and_a_rename_over_a_file_whose_step_is_at_the_device() {
    for layout in BOTH {
        let s = Scratch::new();
        let (dev, v) = slow(&s, "64M", layout);
        let what = format!("{layout:?}");
        let other = file(&v, ROOT, "other");
        let free = free_now(&v);
        let data = pattern(MIB, 5);
        // (the name, whether the VFS holds the file, a rename over it instead of an unlink)
        for (name, held, over) in [("gone", false, false), ("open", true, false), ("under", false, true)] {
            let f = file(&v, ROOT, name);
            step(&v, f, 0, &data[..8192], 8192).unwrap();
            let hold = held.then(|| v.hold(f).unwrap());
            dev.shut.store(true, Ordering::Release);
            let open = Opener(dev.clone());
            let (w, wd) = (v.clone(), data.clone());
            let writer = std::thread::spawn(move || step(&w, f, 0, &wd, MIB as u64));
            wait_inside(&dev, 1);
            // Neither waits for the device: the name goes now, the blocks when the step ended.
            let w = v.clone();
            let n = name.as_bytes().to_vec();
            let r = bounded("an unlink", move || if over { w.rename(ROOT, b"other", other, ROOT, &n, Some(f)) } else { w.remove(ROOT, &n, f) });
            assert_eq!(r, Ok(()), "{what} {name}");
            assert_eq!(dev.inside.load(Ordering::Acquire), 1);
            if over {
                v.rename(ROOT, name.as_bytes(), other, ROOT, b"other", None).unwrap();
            }
            assert!(v.list(ROOT).unwrap().iter().all(|(n, _, _)| n != name.as_bytes()));
            drop(open);
            assert_eq!(writer.join().unwrap(), Ok(()), "{what} {name}");
            if let Some(h) = hold {
                // Unlinked and open: the step's data is the file's until the last holder goes.
                assert!(read_all(&v, f) == data, "{what}: the open file's data");
                v.forget(h.slot);
            }
            // The next call on the volume frees a forgotten file; a file nobody holds went with its step.
            v.stat(ROOT).unwrap();
            assert!(!v.st.lock().inode_in_use(f).unwrap(), "{what} {name}: the inode is still in use");
            settled(&v, free, &format!("{what} {name}"));
            // A step for a file that is gone writes nothing.
            assert_eq!(step(&v, f, 0, &data[..4096], 4096), Err(crate::errno::ENOENT), "{what} {name}");
            let st = v.st.lock();
            assert_eq!(st.reserved, 0);
        }
        settled(&v, free, &what);
        v.sync_fs().unwrap();
        let img = dev.mem.image();
        assert_clean(&s, &img, &what);
        // The disk agrees: the three names are gone, their blocks free, the file renamed back is there.
        let (_d2, v2) = mount(img);
        let names: std::vec::Vec<_> = v2.list(ROOT).unwrap().into_iter().map(|(n, _, _)| n).filter(|n| n != b"lost+found").collect();
        assert_eq!(names, [b"other".to_vec()], "{what}: the names after the remount");
        assert_eq!(v2.stat(lookup(&v2, "/other")).unwrap().size(), 0, "{what}");
        assert_eq!(free_now(&v2), free, "{what}: free blocks after the remount");
    }
}

#[test]
fn a_device_that_fails_or_stops_between_the_claim_and_the_mapping() {
    for layout in BOTH {
        for how in ["fails", "times out", "stops"] {
            let s = Scratch::new();
            let (dev, v) = slow(&s, "64M", layout);
            let what = format!("{layout:?}, the device {how}");
            let f = file(&v, ROOT, "f");
            let keep = file(&v, ROOT, "keep");
            let data = pattern(MIB, 9);
            step(&v, keep, 0, &data, MIB as u64).unwrap();
            v.flush().unwrap();
            let free = free_now(&v);
            // Held as the VFS holds an open file: only then the volume keeps promises for it.
            let hold = v.hold(f).unwrap();

            dev.shut.store(true, Ordering::Release);
            let open = Opener(dev.clone());
            let (w, wd) = (v.clone(), data.clone());
            let writer = std::thread::spawn(move || step(&w, f, 0, &wd, MIB as u64));
            wait_inside(&dev, 1);
            // A write(2) into a page the step is writing: no promise of its own, the step maps the page.
            assert_eq!(v.reserve(f, 10, 10), Ok(()), "{what}");
            assert_eq!(v.st.lock().reserved, 0, "{what}: a page of a step in flight was promised twice");
            match how {
                "fails" => dev.mem.fail_writes(true),
                "times out" => dev.mem.time_out(true),
                _ => dev.mem.stop_after(0),
            }
            drop(open);
            assert_eq!(writer.join().unwrap(), Err(crate::errno::EIO), "{what}");
            // The disk never heard of the blocks: nothing to undo, the volume stays writable (Linux: a failed data
            // write is the file's error, not the file system's).
            assert!(v.writable(), "{what}");
            assert_eq!(v.stat(f).unwrap().size(), 0, "{what}");
            {
                // Nothing claimed, nothing taken; the step's pages are dirty again in the cache and have their
                // promises again (also the page written to meanwhile), so their next write-back finds its room.
                let st = v.st.lock();
                assert!(st.flights.is_empty() && st.flights.claimed() == 0, "{what}: left in flight");
                assert_eq!(st.free_now(), free, "{what}: free blocks");
                assert_eq!(st.reserved, (MIB / 4096) as u64, "{what}: the failed step's pages have no promises");
            }
            if how == "stops" {
                // The next change meets the dead device at its commit and stops the volume.
                let _ = v.create(ROOT, b"late", 0o100644, 0, 0, Kind::Reg);
                let _ = v.flush();
                assert!(!v.writable(), "{what}");
                super::cut::check_disk(&s, dev.mem.image(), &what);
                let (_d2, v2) = mount(dev.mem.image());
                assert!(read_all(&v2, lookup(&v2, "/keep")) == data, "{what}: the file synced before");
                continue;
            }
            dev.mem.fail_writes(false);
            dev.mem.time_out(false);
            let _ = dev.mem.take_timed_out();
            assert_eq!(step(&v, f, 0, &data, MIB as u64), Ok(()), "{what}: the same step again");
            settled(&v, free - (MIB / 4096) as u64, &what);
            v.forget(hold.slot);
            v.sync_fs().unwrap();
            let img = dev.mem.image();
            assert_clean(&s, &img, &what);
            let (_d2, v2) = mount(img);
            assert!(read_all(&v2, lookup(&v2, "/f")) == data, "{what}");
        }
    }
}

#[test]
fn a_freeze_waits_for_the_step_at_the_device() {
    for layout in BOTH {
        let s = Scratch::new();
        let (dev, v) = slow(&s, "64M", layout);
        let what = format!("{layout:?}");
        let f = file(&v, ROOT, "f");
        let data = pattern(2 * MIB, 4);
        dev.shut.store(true, Ordering::Release);
        let open = Opener(dev.clone());
        let (w, wd) = (v.clone(), data.clone());
        let writer = std::thread::spawn(move || step(&w, f, 0, &wd[..MIB], MIB as u64));
        wait_inside(&dev, 1);
        let w = v.clone();
        let freezer = waits("a freeze", move || w.freeze());
        drop(open);
        assert_eq!(writer.join().unwrap(), Ok(()), "{what}");
        assert_eq!(freezer.join().unwrap(), Ok(()), "{what}");
        // The frozen image holds the whole step and checks clean without a replay.
        let img = dev.mem.image();
        assert_clean(&s, &img, &what);
        let (_d2, v2) = mount(img);
        assert!(read_all(&v2, lookup(&v2, "/f")) == data[..MIB], "{what}: the step in the frozen image");
        // The next step waits for the thaw.
        let (w, wd) = (v.clone(), data.clone());
        let next = waits("a step on a frozen volume", move || step(&w, f, MIB as u64, &wd[MIB..], 2 * MIB as u64));
        assert_eq!(dev.inside.load(Ordering::Acquire), 0, "{what}: data reached a frozen volume");
        v.thaw();
        assert_eq!(next.join().unwrap(), Ok(()), "{what}");
        assert!(read_all(&v, f) == data, "{what}");
        v.sync_fs().unwrap();
        assert_clean(&s, &dev.mem.image(), &what);
    }
}
