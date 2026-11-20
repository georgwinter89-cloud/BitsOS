//! FS-W: the window between a step's claim and its mapping, cut at EVERY device write. The three parts of a step
//! are driven by hand on the test's thread (`Parts`), so what falls between them is fixed, not timing: another
//! program's commit before or after the step's data, a truncate with its own commit, a step over preallocated
//! blocks, a nearly full volume whose freed blocks are still busy, a device that stops at each write. Every free
//! block held another file's bytes (`OLD`) before, so a mapping that became durable before its data shows them.
//! With a journal the cut loses half of what the device's cache still held and keeps the commit blocks written
//! through (`lay::cut_image`); without one it is the prefix.

use super::cut::check_disk;
use super::flight::{pattern, read_all, step, MIB};
use super::flight_cut::{leave_old_bytes, OLD};
use super::lay::{cut_image, Rng};
use super::util::*;
use super::write::file;
use crate::dev::WriteLog;
use crate::flight::Step;
use crate::inode::ROOT;
use crate::Volume;
use std::format;

/// (name, mkfs options, with a journal)
pub(super) const LAYOUTS: [(&str, &[&str], bool); 2] = [("journal", &["-b", "4096"], true), ("bare", &["-b", "4096", "-O", "^has_journal"], false)];

/// A step in parts, as `Volume::write` runs them, with room for other calls between.
pub(super) struct Parts {
    step: Step,
    off: u64,
    len: usize,
    wrote: Option<Result<u64, i64>>,
}

impl Parts {
    /// Part 1: the claim.
    pub fn begin(v: &Volume, ino: u32, off: u64, len: usize) -> Parts {
        let step = v.lock_rw().unwrap().write_begin(ino, off, len, 0).unwrap();
        Parts { step, off, len, wrote: None }
    }
    /// Part 2: the data to the device, no lock.
    pub fn data(&mut self, v: &Volume, data: &[u8]) {
        self.wrote = Some(crate::flight::write_segs(&*v.dev, &v.geo, &self.step, &crate::flight::Src::Bytes(data)));
    }
    /// Part 3: the mapping.
    pub fn end(self, v: &Volume, size: u64) -> Result<(), i64> {
        let wrote = self.wrote.expect("the step's data was never written");
        v.lock().write_end(&self.step, wrote, self.off, self.len, size)
    }
}

/// Another program's file, written and fsync-ed: a commit of the journal.
pub(super) fn foreign_fsync(v: &Volume, name: &str) {
    let h = file(v, ROOT, name);
    step(v, h, 0, b"another program's file", 22).unwrap();
    v.flush().unwrap();
}

/// Every cut of `log` over `before`: the cut image mounts, `look` judges its files, and the disk is clean after
/// the replay (journal) or repairable (bare).
pub(super) fn every_cut(s: &Scratch, name: &str, journal: bool, before: &[u8], log: &WriteLog, look: &dyn Fn(&Volume, &str)) {
    for seed in 0..if journal { 2u64 } else { 1 } {
        let mut rng = Rng(0x2545_F491_4F6C_DD1D ^ (seed * 0x9E37_79B9));
        for k in 0..=log.len() {
            let mut kept = || rng.bit();
            let (img, flush) = cut_image(before, log, k, 4096, if journal { Some(&mut kept) } else { None });
            let what = format!("{name}, seed {seed}: cut after {k} of {} (last flush at {flush})", log.len());
            let (cut, vol) = mount(img.clone());
            look(&vol, &what);
            if journal {
                vol.sync_fs().unwrap();
                drop(vol);
                assert_clean(s, &cut.image(), &what);
            } else {
                drop(vol);
                check_disk(s, img, &what);
            }
        }
    }
}

/// `path` holds `want` up to one of the `sizes`, and nothing else.
pub(super) fn holds(v: &Volume, path: &str, want: &[u8], sizes: &[usize], what: &str) {
    let got = read_all(v, lookup(v, path));
    assert!(sizes.contains(&got.len()), "{what}: {path} is {} bytes", got.len());
    assert!(got == want[..got.len()], "{what}: {path} holds bytes never written to it{}", if got.contains(&OLD) { " (another file's old bytes)" } else { "" });
}

/// The step's data and another program's commit in both orders, and a truncate of a third file with commits of
/// its own between the data and the mapping (its freed blocks are busy, then free, while the step holds claims).
#[test]
fn the_window_with_another_commit_in_it_cut_at_every_write() {
    for (name, extra, journal) in LAYOUTS {
        for order in ["data, commit, mapping", "commit, data, mapping", "data, commit, truncate, mapping"] {
            let s = Scratch::new();
            let (dev, v) = mount(mkfs(&s, "32M", extra, None));
            leave_old_bytes(&v);
            let (f, g) = (file(&v, ROOT, "f"), file(&v, ROOT, "g"));
            let data = pattern(2 * MIB, 6);
            let other = pattern(MIB / 2, 77);
            step(&v, f, 0, &data[..MIB], MIB as u64).unwrap();
            step(&v, g, 0, &other, other.len() as u64).unwrap();
            v.sync_fs().unwrap();
            let before = dev.image();
            dev.record();

            let mut p = Parts::begin(&v, f, MIB as u64, MIB);
            if order != "commit, data, mapping" {
                p.data(&v, &data[MIB..]);
            }
            foreign_fsync(&v, "h");
            if order == "data, commit, truncate, mapping" {
                v.set_size(g, 4096).unwrap();
                v.flush().unwrap();
                let i = file(&v, ROOT, "i");
                step(&v, i, 0, &other, other.len() as u64).unwrap();
                v.flush().unwrap();
            }
            if order == "commit, data, mapping" {
                p.data(&v, &data[MIB..]);
            }
            p.end(&v, 2 * MIB as u64).unwrap();
            v.flush().unwrap();
            let log = dev.take_log();
            drop(v);

            let whole = core::cell::Cell::new(false);
            every_cut(&s, &format!("{name}, {order}"), journal, &before, &log, &|vol, what| {
                holds(vol, "/f", &data, &[MIB, 2 * MIB], what);
                whole.set(whole.get() || vol.stat(lookup(vol, "/f")).unwrap().size() == 2 * MIB as u64);
            });
            assert!(whole.get(), "{name}, {order}: no cut showed the step");
        }
    }
}

/// A step over preallocated (unwritten) blocks and a hole behind them: the extents turn written only with the
/// step's mapping, after its data.
#[test]
fn a_step_over_preallocated_blocks_cut_at_every_write() {
    for (name, extra, journal) in LAYOUTS {
        let s = Scratch::new();
        let (dev, v) = mount(mkfs(&s, "32M", extra, None));
        leave_old_bytes(&v);
        let f = file(&v, ROOT, "f");
        let data = pattern(2 * MIB, 6);
        v.fallocate(f, 0, 3 * MIB as u64 / 2).unwrap();
        v.sync_fs().unwrap();
        let before = dev.image();
        dev.record();
        let mut p = Parts::begin(&v, f, 0, 2 * MIB);
        p.data(&v, &data);
        foreign_fsync(&v, "h");
        p.end(&v, 2 * MIB as u64).unwrap();
        v.flush().unwrap();
        let log = dev.take_log();
        drop(v);
        every_cut(&s, &format!("{name}, preallocated"), journal, &before, &log, &|vol, what| holds(vol, "/f", &data, &[0, 2 * MIB], what));
    }
}

/// Nearly full: a step holds claims while a big file goes (its blocks are busy until the next commit) and another
/// file's step has to commit for them. Neither file may show the other's or the deleted file's bytes.
#[test]
fn claims_beside_busy_blocks_on_a_nearly_full_volume_cut_at_every_write() {
    for (name, extra, journal) in LAYOUTS {
        let s = Scratch::new();
        let (dev, v) = mount(mkfs(&s, "32M", extra, None));
        leave_old_bytes(&v);
        let (f, g, fill) = (file(&v, ROOT, "f"), file(&v, ROOT, "g"), file(&v, ROOT, "fill"));
        let data = pattern(MIB, 6);
        let other = pattern(MIB, 77);
        // All but one step's worth and a little: the second step finds its room only in the deleted file's blocks.
        let room = (v.st.lock().free_now() as usize * 4096 - 5 * MIB / 4 - 64 * 4096) / MIB * MIB;
        let junk = std::vec![0x55u8; MIB];
        for at in (0..room).step_by(MIB) {
            step(&v, fill, at as u64, &junk, room as u64).unwrap();
        }
        v.sync_fs().unwrap();
        let before = dev.image();
        dev.record();
        let mut p = Parts::begin(&v, f, 0, MIB);
        p.data(&v, &data);
        v.remove(ROOT, b"fill", fill).unwrap();
        v.write(g, 0, &other, other.len() as u64, 0..0).unwrap();
        p.end(&v, MIB as u64).unwrap();
        v.flush().unwrap();
        {
            let st = v.st.lock();
            assert!(st.flights.is_empty() && st.flights.claimed() == 0, "{name}: claims left over");
        }
        let log = dev.take_log();
        drop(v);
        every_cut(&s, &format!("{name}, nearly full"), journal, &before, &log, &|vol, what| {
            holds(vol, "/f", &data, &[0, MIB], what);
            holds(vol, "/g", &other, &[0, MIB], what);
        });
    }
}

/// The device stops at each write of the window (data, another program's commit, mapping, commit): the file is
/// as before the step or whole, nothing stays claimed, the image mounts and checks.
#[test]
fn the_device_stops_at_each_write_of_the_window() {
    for (name, extra, journal) in LAYOUTS {
        let s = Scratch::new();
        let data = pattern(2 * MIB, 6);
        let base = {
            let (dev, v) = mount(mkfs(&s, "32M", extra, None));
            leave_old_bytes(&v);
            let f = file(&v, ROOT, "f");
            step(&v, f, 0, &data[..MIB], MIB as u64).unwrap();
            v.sync_fs().unwrap();
            drop(v);
            dev.image()
        };
        for n in 0u64.. {
            assert!(n < 200, "{name}: the window never ran through");
            let (dev, v) = mount(base.clone());
            let f = lookup(&v, "/f");
            dev.stop_after(n);
            let begun = v.lock_rw().and_then(|mut st| st.write_begin(f, MIB as u64, MIB, 0));
            let mut through = false;
            if let Ok(stp) = begun {
                let wrote = crate::flight::write_segs(&*v.dev, &v.geo, &stp, &crate::flight::Src::Bytes(&data[MIB..]));
                let mut ok = wrote.is_ok();
                ok &= v.create(ROOT, b"h", 0o100644, 0, 0, crate::Kind::Reg).and_then(|h| step(&v, h.ino, 0, b"another", 7)).is_ok();
                ok &= v.flush().is_ok();
                ok &= v.lock().write_end(&stp, wrote, MIB as u64, MIB, 2 * MIB as u64).is_ok();
                ok &= v.flush().is_ok();
                let st = v.st.lock();
                assert!(st.flights.is_empty() && st.flights.claimed() == 0, "{name}, stopped after {n} writes: claims left over");
                through = ok;
            }
            let what = format!("{name}: the device stopped after {n} writes");
            let img = dev.image();
            drop(v);
            let (cut, vol) = mount(img.clone());
            holds(&vol, "/f", &data, &[MIB, 2 * MIB], &what);
            if journal {
                vol.sync_fs().unwrap();
                drop(vol);
                assert_clean(&s, &cut.image(), &what);
            } else {
                drop(vol);
                check_disk(&s, img, &what);
            }
            if through {
                break;
            }
        }
    }
}
