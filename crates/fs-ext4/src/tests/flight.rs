//! FS-W: a file's data goes to the device with the volume lock released (`flight`). Judged by order, not by time:
//! the device holds a large write (`Slow`) while one thread is inside `Volume::write`, and everything else on the
//! volume must finish before it lets go. Before FS-W the data write ran under `ext4.vol` and all of them waited.

use super::util::*;
use super::write::file;
use crate::dev::{Dev, MemDev};
use crate::inode::ROOT;
use crate::{Kind, Volume};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::Duration;
use std::vec::Vec;
use std::{format, vec};

pub(super) const MIB: usize = 1 << 20;
/// The two layouts every test runs on: with a journal, and without one.
pub(super) const BOTH: [&[&str]; 2] = [&["-b", "4096"], &["-b", "4096", "-O", "^has_journal"]];

/// A memory device that holds every write of at least `big` bytes (a write-back step's data; metadata goes block
/// by block) while the gate is shut.
pub(super) struct Slow {
    pub mem: MemDev,
    pub shut: AtomicBool,
    /// Large writes at the device now, and the most there were at once.
    pub inside: AtomicU32,
    pub peak: AtomicU32,
    big: usize,
}

impl Dev for Slow {
    fn size(&self) -> u64 {
        self.mem.size()
    }
    fn read(&self, at: u64, buf: &mut [u8]) -> Result<(), i64> {
        self.mem.read(at, buf)
    }
    fn write(&self, at: u64, data: &[u8]) -> Result<(), i64> {
        if data.len() < self.big {
            return self.mem.write(at, data);
        }
        let now = self.inside.fetch_add(1, Ordering::AcqRel) + 1;
        self.peak.fetch_max(now, Ordering::AcqRel);
        while self.shut.load(Ordering::Acquire) {
            std::thread::sleep(Duration::from_millis(1));
        }
        let r = self.mem.write(at, data);
        self.inside.fetch_sub(1, Ordering::AcqRel);
        r
    }
    fn flush(&self) -> Result<(), i64> {
        self.mem.flush()
    }
    fn write_through(&self, at: u64, data: &[u8]) -> Result<(), i64> {
        self.mem.write_through(at, data)
    }
    fn take_timed_out(&self) -> bool {
        self.mem.take_timed_out()
    }
    /// FS-W2: a step's data comes this way; held at the gate like any large write.
    fn write_data(&self, at: u64, data: &[u8]) -> Result<(), i64> {
        self.write(at, data)
    }
}

/// Opens the gate when the test ends, also on a failed assert (the held threads would hang the run).
pub(super) struct Opener(pub Arc<Slow>);

impl Drop for Opener {
    fn drop(&mut self) {
        self.0.shut.store(false, Ordering::Release);
    }
}

pub(super) fn slow(s: &Scratch, size: &str, layout: &[&str]) -> (Arc<Slow>, Arc<Volume>) {
    let dev = Arc::new(Slow { mem: MemDev::new(mkfs(s, size, layout, None)), shut: AtomicBool::new(false), inside: AtomicU32::new(0), peak: AtomicU32::new(0), big: 64 << 10 });
    let v = Volume::mount(dev.clone(), opts()).expect("mount");
    (dev, v)
}

pub(super) fn wait_inside(d: &Slow, n: u32) {
    for _ in 0..10_000 {
        if d.inside.load(Ordering::Acquire) >= n {
            return;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    panic!("{n} data writes never reached the device together");
}

/// Runs `f` on a thread of its own; its answer within 10 s, or the name of what is stuck.
pub(super) fn bounded<T: Send + 'static>(what: &str, f: impl FnOnce() -> T + Send + 'static) -> T {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(f());
    });
    rx.recv_timeout(Duration::from_secs(10)).unwrap_or_else(|_| panic!("{what} waited for another file's data write at the device"))
}

pub(super) fn pattern(n: usize, seed: u32) -> Vec<u8> {
    (0..n as u32).map(|i| (i.wrapping_mul(2654435761).wrapping_add(seed) >> 11) as u8).collect()
}

/// One write-back step as the page cache hands it over: promised, then written.
pub(super) fn step(v: &Volume, ino: u32, off: u64, data: &[u8], size: u64) -> Result<(), i64> {
    let pages = off / 4096..(off + data.len() as u64).div_ceil(4096);
    v.reserve(ino, pages.start, pages.end - 1)?;
    v.write(ino, off, data, size, pages)
}

pub(super) fn read_all(v: &Volume, ino: u32) -> Vec<u8> {
    let mut b = vec![0u8; v.stat(ino).unwrap().size() as usize];
    let n = v.read(ino, 0, &mut b).unwrap();
    b.truncate(n);
    b
}

#[test]
fn a_data_write_at_the_device_holds_up_no_lookup_create_stat_or_read_mapping() {
    for layout in BOTH {
        let s = Scratch::new();
        let (dev, v) = slow(&s, "64M", layout);
        let d = v.create(ROOT, b"d", 0o40755, 1000, 1000, Kind::Dir).unwrap().ino;
        for i in 0..40 {
            file(&v, d, &format!("name-{i:02}"));
        }
        let other = file(&v, ROOT, "other");
        let small = pattern(3 * 4096, 7);
        step(&v, other, 0, &small, small.len() as u64).unwrap();
        let big = file(&v, ROOT, "big");
        let data = pattern(MIB, 1);

        dev.shut.store(true, Ordering::Release);
        let _open = Opener(dev.clone());
        let (w, wd) = (v.clone(), data.clone());
        let writer = std::thread::spawn(move || step(&w, big, 0, &wd, MIB as u64));
        wait_inside(&dev, 1);

        let w = v.clone();
        assert_eq!(bounded("a lookup", move || lookup(&w, "/d/name-17")), d + 18);
        let w = v.clone();
        let made = bounded("a create", move || w.create(d, b"new", 0o100644, 1000, 1000, Kind::Reg).map(|i| i.ino));
        assert!(made.is_ok(), "{made:?}");
        let w = v.clone();
        assert_eq!(bounded("a stat", move || (w.stat(other).unwrap().size(), w.stat(big).unwrap().size())), (small.len() as u64, 0));
        let w = v.clone();
        assert_eq!(bounded("a read mapping", move || w.file_plan(other, 0, 8192).map(|p| p.map(|p| p.total()))), Ok(Some(8192)));
        let (w, sm) = (v.clone(), small.clone());
        assert_eq!(bounded("a read", move || read_all(&w, other) == sm), true);
        let w = v.clone();
        assert_eq!(bounded("a write(2)'s promise", move || w.reserve(other, 10, 12)), Ok(()));
        // All of that came back while the data write was still at the device.
        assert_eq!(dev.inside.load(Ordering::Acquire), 1);
        assert!(!writer.is_finished());

        dev.shut.store(false, Ordering::Release);
        assert_eq!(writer.join().unwrap(), Ok(()));
        assert_eq!(read_all(&v, big), data);
        v.sync_fs().unwrap();
        let img = dev.mem.image();
        assert_clean(&s, &img, "a data write beside other calls");
        let (_d2, v2) = mount(img);
        assert_eq!(read_all(&v2, lookup(&v2, "/big")), data);
        assert_eq!(read_all(&v2, lookup(&v2, "/other")), small);
    }
}

/// The volume holds no step, no claim and no promise, and its free count is `free`.
pub(super) fn settled(v: &Volume, free: u64, what: &str) {
    let st = v.st.lock();
    assert!(st.flights.is_empty(), "{what}: a step left in flight");
    assert_eq!(st.flights.claimed(), 0, "{what}: claimed blocks left over");
    assert_eq!(st.reserved, 0, "{what}: promises left over");
    assert_eq!(st.free_now(), free, "{what}: free blocks");
}

pub(super) fn free_now(v: &Volume) -> u64 {
    v.st.lock().free_now()
}

/// Thread `k` of `n` writes every n-th step of `steps` (each `len` bytes of `want`) into `ino`.
fn write_steps(v: &Volume, ino: u32, want: &[u8], len: usize, k: usize, n: usize) {
    for i in (k..want.len().div_ceil(len)).step_by(n) {
        let at = i * len;
        step(v, ino, at as u64, &want[at..(at + len).min(want.len())], want.len() as u64).unwrap();
    }
}

#[test]
fn two_and_eight_writers_on_their_files_and_on_one_file_remount_with_every_byte() {
    for layout in BOTH {
        for n in [2usize, 8] {
            let s = Scratch::new();
            let (dev, v) = slow(&s, "96M", layout);
            let own: Vec<(u32, Vec<u8>)> = (0..n).map(|k| (file(&v, ROOT, &format!("own-{k}")), pattern(3 * MIB + 1000 * k + 7, k as u32))).collect();
            let shared = (file(&v, ROOT, "shared"), pattern(n * 3 * (MIB / 4) + 321, 99));

            // Different files: their first steps are all at the device at once.
            dev.shut.store(true, Ordering::Release);
            let open = Opener(dev.clone());
            let threads: Vec<_> = own.iter().cloned().map(|(ino, want)| {
                let w = v.clone();
                std::thread::spawn(move || write_steps(&w, ino, &want, MIB, 0, 1))
            }).collect();
            wait_inside(&dev, n as u32);
            drop(open);
            threads.into_iter().for_each(|t| t.join().unwrap());
            assert!(dev.peak.load(Ordering::Acquire) >= n as u32);

            // One file, its steps dealt out to the writers: one step of a file at the device at a time.
            dev.peak.store(0, Ordering::Release);
            let threads: Vec<_> = (0..n).map(|k| {
                let (w, (ino, want)) = (v.clone(), shared.clone());
                std::thread::spawn(move || write_steps(&w, ino, &want, MIB / 4, k, n))
            }).collect();
            threads.into_iter().for_each(|t| t.join().unwrap());
            assert_eq!(dev.peak.load(Ordering::Acquire), 1, "{n} writers: two steps of one file were at the device together");

            let what = format!("{layout:?}, {n} writers");
            settled(&v, free_now(&v), &what);
            v.sync_fs().unwrap();
            let img = dev.mem.image();
            assert_clean(&s, &img, &what);
            let (_d2, v2) = mount(img);
            for (k, (_, want)) in own.iter().enumerate() {
                assert!(read_all(&v2, lookup(&v2, &format!("/own-{k}"))) == *want, "{what}: own-{k} differs after the remount");
            }
            assert!(read_all(&v2, lookup(&v2, "/shared")) == shared.1, "{what}: the shared file differs after the remount");
        }
    }
}

#[test]
fn enospc_with_parallel_writers_leaves_the_counts_right() {
    for layout in BOTH {
        let s = Scratch::new();
        let (dev, v) = slow(&s, "24M", layout);
        let what = format!("{layout:?}");
        const STEP: usize = MIB / 4;
        // More than the volume holds: no writer gets to its end.
        const WANT: usize = 32 * MIB;
        let fill = |k: usize, step: usize| (k * 40 + step % 40 + 1) as u8;
        let files: Vec<u32> = (0..6).map(|k| file(&v, ROOT, &format!("w{k}"))).collect();
        // Held as the VFS holds an open file: only then `reserve` keeps promises for it.
        let holds: Vec<_> = files.iter().map(|&f| v.hold(f).unwrap()).collect();
        let start = Arc::new(std::sync::Barrier::new(files.len()));
        let threads: Vec<_> = files.iter().enumerate().map(|(k, &ino)| {
            let (w, start) = (v.clone(), start.clone());
            std::thread::spawn(move || {
                start.wait();
                let mut done = 0usize;
                while done < WANT {
                    let pages = (done / 4096) as u64..((done + STEP) / 4096) as u64;
                    // The promise decides: a step that got one is written, whatever the other writers do.
                    match w.reserve(ino, pages.start, pages.end - 1) {
                        Ok(()) => {}
                        Err(e) => {
                            assert_eq!(e, crate::errno::ENOSPC);
                            break;
                        }
                    }
                    w.write(ino, done as u64, &vec![fill(k, done / STEP); STEP], (done + STEP) as u64, pages).unwrap_or_else(|e| panic!("w{k}: a promised step at {done} answered {e}"));
                    done += STEP;
                }
                done
            })
        }).collect();
        let done: Vec<usize> = threads.into_iter().map(|t| t.join().unwrap()).collect();
        holds.into_iter().for_each(|h| v.forget(h.slot));
        assert!(done.iter().all(|&d| d < WANT), "{what}: the volume never filled: {done:?}");
        assert!(done.iter().sum::<usize>() > 12 * MIB, "{what}: only {done:?} written");
        {
            let st = v.st.lock();
            assert!(st.flights.is_empty() && st.flights.claimed() == 0 && st.reserved == 0, "{what}: left over");
            // Full for a user: what is left is the root reserve and less than one more step.
            let left = st.free_now().saturating_sub(crate::sb::reserved_blocks(&st.sb, &st.geo));
            assert!(left < 2 * (STEP as u64 / 4096), "{what}: {left} blocks left for users after ENOSPC");
        }
        v.sync_fs().unwrap();
        let img = dev.mem.image();
        assert_clean(&s, &img, &what);
        let (_d2, v2) = mount(img);
        for (k, &d) in done.iter().enumerate() {
            let got = read_all(&v2, lookup(&v2, &format!("/w{k}")));
            assert!(got.len() == d && got.chunks(STEP).enumerate().all(|(i, c)| c.iter().all(|&b| b == fill(k, i))), "{what}: w{k} after the remount");
        }
    }
}

/// Another thread's fsync commits the journal while a step's data is still on its way: the commit holds nothing of
/// the step. A cut right after it finds the file as it was and no block marked used that no file points at.
#[test]
fn a_commit_between_the_claim_and_the_mapping_carries_nothing_of_the_step() {
    for layout in BOTH {
        let s = Scratch::new();
        let (dev, v) = slow(&s, "64M", layout);
        let what = format!("{layout:?}");
        let f = file(&v, ROOT, "f");
        let data = pattern(2 * MIB, 6);
        step(&v, f, 0, &data[..MIB], MIB as u64).unwrap();
        v.flush().unwrap();

        dev.shut.store(true, Ordering::Release);
        let open = Opener(dev.clone());
        let (w, wd) = (v.clone(), data.clone());
        let writer = std::thread::spawn(move || step(&w, f, MIB as u64, &wd[MIB..], 2 * MIB as u64));
        wait_inside(&dev, 1);
        let w = v.clone();
        let synced = bounded("another program's fsync", move || {
            let g = file(&w, ROOT, "g");
            step(&w, g, 0, b"another program's file", 22)?;
            w.flush()
        });
        assert_eq!(synced, Ok(()), "{what}");
        assert_eq!(dev.inside.load(Ordering::Acquire), 1);
        // The cut: everything the device has now.
        let cut = dev.mem.image();
        let (d2, v2) = mount(cut);
        assert!(read_all(&v2, lookup(&v2, "/f")) == data[..MIB], "{what}: the file after the cut is not what it was before the step");
        assert_eq!(read_all(&v2, lookup(&v2, "/g")), b"another program's file", "{what}");
        v2.sync_fs().unwrap();
        drop(v2);
        assert_clean(&s, &d2.image(), &format!("{what}: the cut between the step's parts"));

        drop(open);
        assert_eq!(writer.join().unwrap(), Ok(()), "{what}");
        v.flush().unwrap();
        let (_d3, v3) = mount(dev.mem.image());
        assert!(read_all(&v3, lookup(&v3, "/f")) == data, "{what}: the whole file after the step's own fsync");
        v.sync_fs().unwrap();
        assert_clean(&s, &dev.mem.image(), &what);
    }
}
