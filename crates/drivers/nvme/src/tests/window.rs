//! NV-2p on the fake: a namespace built like Georg's Lexar SSD (GPT, Microsoft reserved, Windows data, a raw
//! partition, a free rest). Every byte outside the window must stay what it was, whatever runs above.

use super::*;
use alloc::format;
use crate::typed::{self, Refused};
use crate::window as drv;
use block::gpt::window::{Key, SIG_BITSVOL, Why};
use block::table::{GUID_EFI_SYSTEM, GUID_MS_BASIC_DATA, GUID_MS_RESERVED, guid_text};
use block::{Disk, DiskDevice, DiskKind, Dispatch};
use console::evdev::{EV_KEY, InputEvent};

const BS: usize = 512;
const BLOCKS: u64 = 1 << 19; // 256 MiB
const MSR: (u64, u64) = (2048, 34815);
const WIN: (u64, u64) = (34816, 200_000);
const DATA: (u64, u64) = (200_001, 480_000);
const SERIAL: &str = "LEXAR00012345AB";

/// The GUID `testimg::gpt` gives entry `i` (from 0).
fn uniq(i: u8) -> [u8; 16] {
    let mut g = [0u8; 16];
    g[0] = i + 1;
    g
}

/// The real disk's picture: p1 MSR, p2 Windows data (an NTFS boot sector), p3 raw, the rest free. Pattern outside
/// the table, so a stray write shows.
fn real_disk() -> Vec<u8> {
    let parts = [(GUID_MS_RESERVED, MSR.0, MSR.1), (GUID_MS_BASIC_DATA, WIN.0, WIN.1), (GUID_MS_BASIC_DATA, DATA.0, DATA.1)];
    with_pattern(block::testimg::gpt(BS, BLOCKS, &parts), &parts)
}

fn with_pattern(mut img: Vec<u8>, parts: &[([u8; 16], u64, u64)]) -> Vec<u8> {
    let (hi, lo) = (BLOCKS - 33, 34);
    for (i, b) in img.iter_mut().enumerate() {
        let l = (i / BS) as u64;
        if (lo..hi).contains(&l) {
            *b = fake::pattern(7, i as u64);
        }
    }
    // Every partition starts blank in its first 128 KiB, except what a test puts there.
    for (_, first, last) in parts {
        let a = *first as usize * BS;
        let n = (HEAD as usize).min(((last - first + 1) as usize) * BS);
        img[a..a + n].fill(0);
        let t = *last as usize * BS;
        img[t..t + BS].fill(0);
    }
    img
}
const HEAD: u64 = 128 << 10;

fn ntfs_at(img: &mut [u8], first: u64) {
    let a = first as usize * BS;
    img[a + 3..a + 11].copy_from_slice(b"NTFS    ");
}

fn key() -> Key {
    Key { serial: String::from(SERIAL), guid: uniq(2), first: DATA.0, blocks: DATA.1 - DATA.0 + 1 }
}

struct Rig {
    c: Arc<Controller<FakeHw>>,
    hw: FakeHw,
    disk: Arc<Disk>,
    img: Vec<u8>,
}

fn rig_on(img: Vec<u8>) -> Rig {
    let (c, hw) = ctrl(vec![fake::FakeNs { nsid: 1, shift: 9, ms: 0, data: img.clone() }]);
    hw.st.lock().serial = String::from(SERIAL);
    c.start().unwrap();
    let dev: Arc<dyn BlockDevice> = Arc::new(NsDevice::new(c.clone(), 1).unwrap());
    let disk = Disk::open(dev, DiskKind::Nvme, Dispatch::Inline).unwrap();
    Rig { c, hw, disk, img }
}

fn rig() -> Rig {
    let mut img = real_disk();
    ntfs_at(&mut img, WIN.0);
    rig_on(img)
}

/// Armed as the boot arms it.
fn armed() -> Rig {
    let r = rig();
    let w = drv::check(&*r.c, &r.disk, &key()).unwrap();
    assert_eq!((w.number, w.first, w.blocks), (3, DATA.0, DATA.1 - DATA.0 + 1));
    drv::arm(&*r.c, 1, &r.disk, &w);
    r
}

impl Rig {
    /// Every byte outside the window (GPTs included) is what it was; and when the whole disk must be: no difference.
    fn outside_unchanged(&self) {
        let st = self.hw.st.lock();
        let now = &st.ns[0].data;
        let (a, b) = (DATA.0 as usize * BS, (DATA.1 as usize + 1) * BS);
        assert!(now[..a] == self.img[..a], "bytes before the window changed");
        assert!(now[b..] == self.img[b..], "bytes after the window changed");
    }
    fn unchanged(&self) {
        assert!(self.hw.st.lock().ns[0].data == self.img, "the disk changed");
        assert_eq!(self.c.stats.media_writes(), 0);
    }
    fn part(&self, number: u32, first: u64, last: u64) -> Arc<Disk> {
        let lower: Arc<dyn BlockDevice> = Arc::new(self.disk.part_port(first, last - first + 1));
        let p = block::gpt::Partition::new(lower, self.disk.name(), first, last - first + 1, number, "t").unwrap();
        Disk::open(Arc::new(p), DiskKind::Stacked, Dispatch::Inline).unwrap()
    }
}

#[test]
fn builder_refuses_outside_the_window() {
    let w = cmd::WriteWindow::new(1, 100, 50);
    let at = |lba, n| IoCmd::write(&w, 1, lba, n, 0x1000, 0, false).is_some();
    assert!(!at(99, 1), "first-1");
    assert!(at(100, 1), "first");
    assert!(at(149, 1), "last");
    assert!(!at(150, 1), "last+1");
    assert!(!at(99, 2), "across the start");
    assert!(!at(149, 2), "across the end");
    assert!(at(100, 50), "all of it");
    assert!(!at(100, 51));
    assert!(!at(u64::MAX, 2), "wraps");
    assert!(IoCmd::write(&w, 2, 100, 1, 0x1000, 0, false).is_none(), "another namespace");
    assert!(IoCmd::deallocate(&w, 1, 100, 50, 0x1000).is_some());
    assert!(IoCmd::deallocate(&w, 1, 100, 51, 0x1000).is_none());
    assert!(IoCmd::deallocate(&w, 1, 99, 1, 0x1000).is_none());
    assert!(IoCmd::deallocate(&w, 2, 100, 1, 0x1000).is_none());
    // The bytes the device reads: FUA is CDW12 bit 30, the count 0-based; deallocate is AD with one range.
    let e = IoCmd::write(&w, 1, 100, 8, 0x1000, 0x2000, true).unwrap().encode(7);
    assert_eq!(e[0], 0x01);
    assert_eq!(u32::from_le_bytes([e[48], e[49], e[50], e[51]]), 7 | 1 << 30);
    let e = IoCmd::deallocate(&w, 1, 100, 8, 0x1000).unwrap().encode(7);
    assert_eq!(e[0], 0x09);
    assert_eq!(u32::from_le_bytes([e[40], e[41], e[42], e[43]]), 0, "one range");
    assert_eq!(u32::from_le_bytes([e[44], e[45], e[46], e[47]]), 1 << 2, "deallocate");
}

#[test]
fn builder_takes_the_boot_esp_beside_the_window() {
    let w = cmd::WriteWindow::new(1, 100, 50);
    assert!(w.with_boot(120, 10).is_none(), "overlapping: refused");
    assert!(w.with_boot(90, 11).is_none(), "touching the first block");
    assert!(w.with_boot(10, 0).is_none(), "empty");
    let b = w.with_boot(10, 50).unwrap();
    let at = |w: &cmd::WriteWindow, lba, n| IoCmd::write(w, 1, lba, n, 0x1000, 0, false).is_some();
    for (lba, n, ok) in [(9, 1, false), (10, 1, true), (59, 1, true), (60, 1, false), (59, 2, false), (99, 1, false), (100, 1, true), (59, 42, false), (10, 50, true)] {
        assert_eq!(at(&b, lba, n), ok, "{lba}+{n}");
    }
    assert!(!at(&w, 10, 1), "without the boot range: refused");
    assert!(IoCmd::write(&b, 2, 10, 1, 0x1000, 0, false).is_none(), "another namespace");
    assert!(IoCmd::deallocate(&b, 1, 10, 50, 0x1000).is_some());
    assert!(IoCmd::deallocate(&b, 1, 60, 1, 0x1000).is_none());
    assert_eq!(b.without_boot(), w);
}

/// /boot on the Lexar's ESP (6.11.2026): p1 MSR, p2 Windows, p3 the ESP nvme_boot= names, p4 the window.
#[test]
fn the_boot_esp_opens_at_both_layers_and_nowhere_else() {
    const ESP: (u64, u64) = (150_001, 200_000);
    const WIN2: (u64, u64) = (34816, 150_000);
    let parts = [(GUID_MS_RESERVED, MSR.0, MSR.1), (GUID_MS_BASIC_DATA, WIN2.0, WIN2.1), (GUID_EFI_SYSTEM, ESP.0, ESP.1), (GUID_MS_BASIC_DATA, DATA.0, DATA.1)];
    let mut img = with_pattern(block::testimg::gpt(BS, BLOCKS, &parts), &parts);
    ntfs_at(&mut img, WIN2.0);
    let r = rig_on(img);
    let (esp, win) = ((ESP.0, ESP.1 - ESP.0 + 1), (DATA.0, DATA.1 - DATA.0 + 1));
    drv::tie(&r.c, 1, &r.disk);
    drv::arm(&*r.c, 1, &r.disk, &block::gpt::window::Window { number: 4, first: win.0, blocks: win.1, esp: Some(esp) });
    let one = vec![0x5Cu8; BS];
    let d = NsDevice::new(r.c.clone(), 1).unwrap();
    assert_eq!(r.c.write(1, ESP.0, &one, false), Err(CmdError::OutsideWindow), "shut until /boot opens it");
    let p3 = r.part(3, ESP.0, ESP.1);
    assert!(p3.write(0, &one, false, IoPriority::Normal).is_err());
    r.disk.open_boot_window(esp.0, esp.1).unwrap();
    assert_eq!(r.c.window().and_then(|w| w.boot()), Some(esp), "the builder opened with the block layer");
    p3.write(0, &one, false, IoPriority::Normal).unwrap();
    p3.write(esp.1 - 1, &one, true, IoPriority::Normal).unwrap();
    assert!(p3.write(esp.1, &one, false, IoPriority::Normal).is_err(), "past the ESP");
    for lba in [ESP.0 - 1, DATA.1 + 1, MSR.0, WIN2.0, WIN2.1, 0, 1, BLOCKS - 1] {
        assert_eq!(d.write_blocks(lba, &one, false, IoPriority::Normal), Err(DevError::Refused), "namespace device at {lba}");
        assert!(r.disk.write(lba, &one, false, IoPriority::Normal).is_err(), "whole disk at {lba}");
    }
    assert!(r.part(2, WIN2.0, WIN2.1).write(0, &one, false, IoPriority::Normal).is_err(), "Windows");
    assert!(r.part(1, MSR.0, MSR.1).write(0, &one, false, IoPriority::Normal).is_err(), "MSR");
    let p4 = r.part(4, DATA.0, DATA.1);
    p4.write(0, &one, false, IoPriority::Normal).unwrap();
    {
        let st = r.hw.st.lock();
        let lbas: Vec<u64> = st.writes.iter().map(|w| w.lba).collect();
        assert_eq!(lbas, [ESP.0, ESP.1, DATA.0], "only those reached the device");
    }
    // Closing /boot's window closes the builder's too; nvme_data's stays.
    r.disk.close_boot_window();
    assert_eq!(r.c.window().map(|w| (w.range(), w.boot())), Some((win, None)));
    assert_eq!(r.c.write(1, ESP.0, &one, false), Err(CmdError::OutsideWindow));
    assert!(p3.write(0, &one, false, IoPriority::Normal).is_err());
    p4.write(1, &one, false, IoPriority::Normal).unwrap();
    // Disarmed: both gone.
    r.disk.open_boot_window(esp.0, esp.1).unwrap();
    drv::disarm(&*r.c, &r.disk);
    assert!(r.c.window().is_none() && r.disk.boot_window().is_none());
    assert!(p3.write(0, &one, false, IoPriority::Normal).is_err());
}

#[test]
fn no_window_no_write_at_any_layer() {
    let r = rig();
    let b = vec![0xAAu8; BS];
    let d = NsDevice::new(r.c.clone(), 1).unwrap();
    assert!(d.info().read_only);
    assert!(!d.info().supports_discard);
    assert_eq!(d.write_blocks(DATA.0, &b, false, IoPriority::Normal), Err(DevError::Refused));
    assert_eq!(d.discard(DATA.0, 1), Err(DevError::Refused));
    assert_eq!(r.c.write(1, DATA.0, &b, false), Err(CmdError::OutsideWindow));
    assert_eq!(r.c.deallocate(1, DATA.0, 1), Err(CmdError::OutsideWindow));
    assert!(r.disk.write(DATA.0, &b, false, IoPriority::Normal).is_err());
    let p = r.part(3, DATA.0, DATA.1);
    assert!(p.write(0, &b, false, IoPriority::Normal).is_err());
    assert!(p.discard(0, 1, IoPriority::Normal).is_err());
    assert!(p.write_policy().is_err());
    r.unchanged();
}

#[test]
fn window_writes_only_inside_at_every_layer() {
    let r = armed();
    let one = vec![0x5Au8; BS];
    let two = vec![0x5Bu8; 2 * BS];
    let (first, last) = DATA;
    // The namespace device, the lowest place left above the builder.
    let d = NsDevice::new(r.c.clone(), 1).unwrap();
    assert!(!d.info().read_only);
    assert!(d.info().supports_discard);
    assert_eq!(d.write_blocks(first - 1, &one, false, IoPriority::Normal), Err(DevError::Refused), "first-1");
    assert_eq!(d.write_blocks(last + 1, &one, false, IoPriority::Normal), Err(DevError::Refused), "last+1");
    assert_eq!(d.write_blocks(first - 1, &two, false, IoPriority::Normal), Err(DevError::Refused), "across the start");
    assert_eq!(d.write_blocks(last, &two, false, IoPriority::Normal), Err(DevError::Refused), "across the end");
    assert_eq!(d.discard(first - 1, 1), Err(DevError::Refused));
    assert_eq!(d.discard(last, 2), Err(DevError::Refused));
    assert!(r.c.stats.window_refused.load(core::sync::atomic::Ordering::Relaxed) >= 6, "each refusal is counted");
    d.write_blocks(first, &one, false, IoPriority::Normal).unwrap();
    d.write_blocks(last, &one, true, IoPriority::Normal).unwrap();
    {
        let st = r.hw.st.lock();
        assert!(st.ns[0].data[first as usize * BS..(first as usize + 1) * BS] == one[..]);
        assert!(st.ns[0].data[last as usize * BS..(last as usize + 1) * BS] == one[..]);
        assert_eq!(st.writes.last().map(|w| (w.lba, w.fua)), Some((last, true)));
    }
    // The whole-disk device: refused everywhere, inside the range too.
    assert!(r.disk.write(first, &one, false, IoPriority::Normal).is_err(), "whole disk, inside");
    assert!(r.disk.write(0, &one, false, IoPriority::Normal).is_err(), "whole disk, LBA 0");
    assert!(r.disk.discard(first, 1, IoPriority::Normal).is_err());
    assert!(r.disk.write_policy().is_err());
    // Every other partition refused, also when it asks for the window's blocks through its own port.
    let p1 = r.part(1, MSR.0, MSR.1);
    let p2 = r.part(2, WIN.0, WIN.1);
    assert!(p1.write(0, &one, false, IoPriority::Normal).is_err());
    assert!(p2.write(0, &one, false, IoPriority::Normal).is_err());
    assert!(p2.discard(0, 1, IoPriority::Normal).is_err());
    assert!(p2.write_policy().is_err());
    let wrong = r.disk.part_port(WIN.0, WIN.1 - WIN.0 + 1);
    assert_eq!(wrong.write_blocks(first, &one, false, IoPriority::Normal), Err(DevError::Refused), "another partition's port");
    // A port of the window's own range, past its end: the block layer refuses before the driver has to.
    let port = r.disk.part_port(first, last - first + 1);
    assert_eq!(port.write_blocks(first - 1, &one, false, IoPriority::Normal), Err(DevError::Refused));
    assert_eq!(port.write_blocks(last, &two, false, IoPriority::Normal), Err(DevError::Refused));
    // The partition itself: its range stops it at its own end.
    let p3 = r.part(3, first, last);
    p3.write(0, &one, false, IoPriority::Normal).unwrap();
    assert!(p3.write(last - first + 1, &one, false, IoPriority::Normal).is_err());
    assert!(p3.write(last - first, &two, false, IoPriority::Normal).is_err());
    p3.discard(1, 8, IoPriority::Normal).unwrap();
    assert_eq!(r.hw.st.lock().deallocs.last().copied(), Some((1, first + 1, 8)));
    r.outside_unchanged();
    // Disarmed: read-only again at every layer.
    drv::disarm(&*r.c, &r.disk);
    assert!(p3.write(0, &one, false, IoPriority::Normal).is_err());
    assert_eq!(d.write_blocks(first, &one, false, IoPriority::Normal), Err(DevError::Refused));
    r.outside_unchanged();
}

#[test]
fn a_whole_run_leaves_everything_outside_as_it_was() {
    let r = armed();
    let p3 = r.part(3, DATA.0, DATA.1);
    assert!(p3.write_policy().is_ok());
    // What bitsdisk runs: pool and ext4.
    fs_ext4::data::format_pool(Arc::new(DiskDevice(p3.clone())), b"bitsdata").unwrap();
    assert_eq!(fs_ext4::data::look_at_pool(Arc::new(DiskDevice(p3.clone()))), fs_ext4::data::PoolLook::Data);
    assert_eq!(drv::read_signature(&r.disk, DATA.0, DATA.1 - DATA.0 + 1).unwrap(), Some(SIG_BITSVOL));
    // A burst of odd sizes and places, then discards, all through the partition.
    let total = p3.dev_info().blocks;
    let mut seed = 0x1234_5678u64;
    let mut next = || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    for _ in 0..400 {
        let n = 1 + next() % 64;
        let at = next() % (total - n);
        let b = vec![next() as u8; n as usize * BS];
        p3.write(at, &b, next() & 1 == 0, IoPriority::Normal).unwrap();
    }
    for _ in 0..100 {
        let n = 1 + next() % 2048;
        let at = next() % (total - n);
        p3.discard(at, n, IoPriority::Normal).unwrap();
    }
    p3.flush(IoPriority::Normal).unwrap();
    r.outside_unchanged();
    assert!(r.c.stats.media_writes() > 400);
    let st = r.hw.st.lock();
    assert!(st.writes.iter().all(|w| w.lba >= DATA.0 && w.lba + w.blocks <= DATA.1 + 1), "a write command left the window");
    assert!(st.deallocs.iter().all(|d| d.1 >= DATA.0 && d.1 + d.2 <= DATA.1 + 1), "a deallocation left the window");
    assert!(st.flushes > 0);
    drop(st);
    // A second look, the next boot's: the pool does not disturb the check, the GPTs still agree.
    let again = Disk::open(Arc::new(NsDevice::new(r.c.clone(), 1).unwrap()), DiskKind::Nvme, Dispatch::Inline).unwrap();
    assert!(drv::check(&*r.c, &again, &key()).is_ok());
}

/// A disk without Windows on it: the table's verdict is clean, so nothing but the armed window keeps the whole-disk
/// device out (DiskKind::Stacked: step 3 of `policy::decide` passes as with `nvme_write=on`).
#[test]
fn whole_disk_refused_without_windows_too() {
    let parts = [(block::table::GUID_LINUX_DATA, MSR.0, MSR.1), (block::table::GUID_LINUX_DATA, DATA.0, DATA.1)];
    let img = with_pattern(block::testimg::gpt(BS, BLOCKS, &parts), &parts);
    let (c, hw) = ctrl(vec![fake::FakeNs { nsid: 1, shift: 9, ms: 0, data: img.clone() }]);
    hw.st.lock().serial = String::from(SERIAL);
    c.start().unwrap();
    let dev: Arc<dyn BlockDevice> = Arc::new(NsDevice::new(c.clone(), 1).unwrap());
    let disk = Disk::open(dev, DiskKind::Stacked, Dispatch::Inline).unwrap();
    assert_eq!(disk.scan().verdict(), block::table::Verdict::Clean);
    let k = Key { guid: uniq(1), ..key() };
    let w = drv::check(&*c, &disk, &k).unwrap();
    drv::arm(&*c, 1, &disk, &w);
    let b = vec![9u8; BS];
    assert!(disk.write(DATA.0, &b, false, IoPriority::Normal).is_err(), "whole disk, inside the window");
    assert!(disk.write(0, &b, false, IoPriority::Normal).is_err(), "whole disk, LBA 0");
    assert!(disk.write_policy().is_err());
    assert_eq!(c.stats.media_writes(), 0);
    disk.part_port(DATA.0, DATA.1 - DATA.0 + 1).write_blocks(DATA.0, &b, false, IoPriority::Normal).unwrap();
}

/// A preflush write and FUA reach the device in the order a journal needs.
/// A command given up twice (its Abort ignored, as QEMU does, and lost again after the reset) may stay on the
/// controller: the device is stopped before the call returns (bus master off, CC.EN = 0), so no page of it is
/// reused while the device could still fetch it, and nothing is placed next to it until a restart.
#[test]
fn an_abandoned_command_blocks_the_queue() {
    let r = armed();
    // A Dataset Management lost twice.
    {
        let mut st = r.hw.st.lock();
        st.inject.drop_io = 2;
        st.inject.abort_ignored = true;
    }
    assert_eq!(r.c.deallocate(1, DATA.0, 8), Err(CmdError::Timeout));
    assert_eq!(r.c.state(), State::Failed);
    {
        let st = r.hw.st.lock();
        assert_eq!(st.cc & regs::CC_EN, 0, "the controller is off");
        assert!(!st.bus_master, "bus master off");
        assert_eq!(st.aborts_seen, 2, "each timeout sent its Abort first");
    }
    let seen = r.hw.st.lock().seen_io.len();
    let b = vec![0u8; 512];
    assert_eq!(r.c.write(1, DATA.0, &b, false), Err(CmdError::NotReady));
    let mut big = vec![0u8; 3 * 4096];
    assert_eq!(r.c.read(1, DATA.0, &mut big, true), Err(CmdError::NotReady));
    assert_eq!(r.c.deallocate(1, DATA.0, 8), Err(CmdError::NotReady));
    assert_eq!(r.hw.st.lock().seen_io.len(), seen, "nothing reached the queue");
    // A three-page Read lost twice, after a restart.
    r.c.restart().unwrap();
    r.hw.st.lock().inject.drop_io = 2;
    assert_eq!(r.c.read(1, DATA.0, &mut big, true), Err(CmdError::Timeout));
    assert_eq!(r.c.state(), State::Failed);
    assert!(!r.hw.st.lock().bus_master);
    let seen = r.hw.st.lock().seen_io.len();
    assert_eq!(r.c.deallocate(1, DATA.0, 8), Err(CmdError::NotReady));
    assert_eq!(r.hw.st.lock().seen_io.len(), seen, "the range page is not written next to a lost PRP list");
    r.outside_unchanged();
}

/// A recovery that finds another namespace closes the window; the retry must not build from what it knew before.
#[test]
fn a_retry_builds_from_the_window_as_it_is_now() {
    let r = armed();
    let writes = |r: &Rig| r.hw.st.lock().seen_io.iter().filter(|o| **o == cmd::IoOp::Write as u8).count();
    let dsms = |r: &Rig| r.hw.st.lock().seen_io.iter().filter(|o| **o == cmd::IoOp::Dsm as u8).count();
    let b = vec![0x77u8; 512];
    r.c.write(1, DATA.0, &b, false).unwrap();
    assert_eq!(writes(&r), 1);
    {
        let mut st = r.hw.st.lock();
        st.inject.drop_io = 1;
        // QEMU's way: the Abort leaves the command out, so the controller is reset.
        st.inject.abort_ignored = true;
        let n = st.ns[0].data.len() - 4096;
        st.ns[0].data.truncate(n);
    }
    assert_eq!(r.c.write(1, DATA.0 + 8, &b, false), Err(CmdError::OutsideWindow));
    assert_eq!(writes(&r), 2, "no Write after the restart");
    assert!(r.c.window().is_none(), "the restart closed the window");
    assert_eq!(r.c.state(), State::Ready);
    assert_eq!(r.c.write(1, DATA.0, &b, false), Err(CmdError::OutsideWindow));
    assert_eq!(r.c.deallocate(1, DATA.0, 8), Err(CmdError::OutsideWindow));
    assert_eq!((writes(&r), dsms(&r)), (2, 0));
}

/// Same for Dataset Management.
#[test]
fn a_dsm_retry_builds_from_the_window_as_it_is_now() {
    let r = armed();
    {
        let mut st = r.hw.st.lock();
        st.inject.drop_io = 1;
        // QEMU's way: the Abort leaves the command out, so the controller is reset.
        st.inject.abort_ignored = true;
        let n = st.ns[0].data.len() - 4096;
        st.ns[0].data.truncate(n);
    }
    assert_eq!(r.c.deallocate(1, DATA.0, 8), Err(CmdError::OutsideWindow));
    let dsms = r.hw.st.lock().seen_io.iter().filter(|o| **o == cmd::IoOp::Dsm as u8).count();
    assert_eq!(dsms, 1, "no Dataset Management after the restart");
}

fn entry(r: &Rig, number: u32) -> block::table::GptEntry {
    r.disk.scan().gpt.as_ref().unwrap().entries.iter().find(|e| e.number == number).unwrap().clone()
}

/// What `bitsdisk` is offered, partition by partition: its first look at a disk and what it does with each answer.
#[test]
fn what_bitsdisk_is_offered() {
    use crate::setup::{Offer, key_of, offer};
    const SEC: usize = 512;
    let on = |put: &dyn Fn(&mut [u8])| {
        let mut img = real_disk();
        ntfs_at(&mut img, WIN.0);
        put(&mut img[DATA.0 as usize * SEC..]);
        rig_on(img)
    };
    let of = |r: &Rig, number: u32, stick: Option<&str>| offer(SERIAL, &r.disk, &entry(r, number), stick);
    // The raw partition; the types Windows does not give a data partition; what holds a file system.
    let r = on(&|_| {});
    assert_eq!(of(&r, 3, None), Ok(Offer::New));
    assert_eq!(of(&r, 1, None), Err(Why::Kind), "Microsoft reserved shows no signature, and is still no choice");
    assert!(matches!(of(&r, 2, None), Err(Why::Signature(s)) if s.contains("NTFS")));
    r.unchanged();
    let sig = |put: &dyn Fn(&mut [u8]), want: &str| {
        let r = on(put);
        match of(&r, 3, None) {
            Err(Why::Signature(s)) => assert!(s.contains(want), "{s} is no {want}"),
            other => panic!("{want}: {other:?}"),
        }
        r.unchanged();
    };
    sig(&|p| p[3..11].copy_from_slice(b"NTFS    "), "NTFS");
    sig(&|p| p[3..11].copy_from_slice(b"-FVE-FS-"), "BitLocker");
    sig(&|p| p[3..11].copy_from_slice(b"EXFAT   "), "exFAT");
    sig(&|p| p[82..90].copy_from_slice(b"FAT32   "), "FAT");
    sig(&|p| p[3..11].copy_from_slice(b"ReFS\0\0\0\0"), "ReFS");
    sig(&|p| {
        p[1024 + 56] = 0x53;
        p[1024 + 57] = 0xEF;
    }, "ext");
    sig(&|p| p[4096 - 10..4096].copy_from_slice(b"SWAPSPACE2"), "swap");
    sig(&|p| p[..8].copy_from_slice(b"BITSVOL1"), "bitsvol");
    // A bitsvol pool: another pool's, or the one the stick's key names.
    let r = on(&|p| p[..8].copy_from_slice(b"BITSVOL1"));
    let other = Key { guid: uniq(1), ..key() }.render().unwrap();
    assert!(matches!(of(&r, 3, Some(&other)), Err(Why::Signature(s)) if s == SIG_BITSVOL));
    assert!(matches!(of(&r, 3, Some("not a key")), Err(Why::Signature(_))));
    assert_eq!(of(&r, 3, Some(&key_of(SERIAL, &entry(&r, 3)).render().unwrap())), Ok(Offer::ThisPool));
    r.unchanged();
}

/// A window armed at boot is never replaced: another partition is refused, the armed one goes on taking writes.
#[test]
fn a_setup_never_takes_an_armed_window() {
    use block::gpt::window::Boot;
    use crate::setup::busy;
    let boot = Boot::Armed { disk: String::from("nvme0n1"), part: String::from("nvme0n1p3") };
    assert!(busy(&boot, true, true).unwrap().contains("nvme0n1p3"));
    assert!(busy(&boot, false, false).is_some(), "whichever disk it is on: CONFIG.TXT holds one key");
    assert!(busy(&Boot::Looking, true, false).is_some());
    assert!(busy(&Boot::Looking, false, true).is_some());
    assert_eq!(busy(&Boot::Looking, false, false), None);
    assert_eq!(busy(&Boot::Refused { word: "serial", text: String::new() }, false, false), None);
}

/// Only a pool known to be half-made is made again.
#[test]
fn a_pool_that_cannot_be_judged_is_never_erased() {
    use crate::setup::{Offer, Plan, plan_of};
    use fs_ext4::data::PoolLook;
    assert_eq!(plan_of(Offer::New, || panic!("nothing to look at")), Ok(Plan::Make { remake: false }));
    assert_eq!(plan_of(Offer::ThisPool, || PoolLook::Data), Ok(Plan::Already));
    assert_eq!(plan_of(Offer::ThisPool, || PoolLook::HalfMade), Ok(Plan::Make { remake: true }));
    let a = plan_of(Offer::ThisPool, || PoolLook::Unclear(String::from("Dev(Io)"))).unwrap_err();
    assert!(a.starts_with("refused unclear "), "{a}");
}

/// The window's claim follows the disk: gone means free, a second one means a doubt.
#[test]
fn the_claim_follows_the_disk() {
    drv::forget("nvme8n1");
    assert_eq!(drv::claim_for_test("nvme8n1"), None);
    drv::forget("nvme8n1");
    assert_eq!(drv::claim_for_test("nvme9n1"), None, "the old disk is gone: no conflict");
    assert_eq!(drv::claim_for_test("nvme8n1").as_deref(), Some("nvme9n1"), "two disks answer");
    drv::forget("nvme9n1");
    assert_eq!(drv::claim_for_test("nvme9n1").as_deref(), Some("nvme8n1"), "only the disk that has it gives it up");
    drv::forget("nvme9n1");
}

#[test]
fn no_controller_to_come() {
    assert!(crate::all_seen(true, 0, 0, 0), "a machine with no NVMe controller");
    assert!(!crate::all_seen(false, 0, 0, 0), "the scan is not done");
    assert!(!crate::all_seen(true, 2, 1, 0), "the second function is not probed yet");
    assert!(!crate::all_seen(true, 2, 2, 1), "one is still coming up");
    assert!(crate::all_seen(true, 2, 2, 0));
}

#[test]
fn the_move_ended_well_only_by_its_last_line() {
    use crate::tool::copy_finished;
    assert!(copy_finished(b"start=5\ndone status=0\n"));
    assert!(!copy_finished(b"start=5\n"));
    assert!(!copy_finished(b"start=5\ndone status=1\n"));
    assert!(!copy_finished(b"done status=0\ncp: something after it\n"));
    assert!(!copy_finished(b"done status=01\n"));
    assert!(!copy_finished(b""));
}

/// The root copy's script itself, on the host's shell in a folder of its own: a failed copy can be started again, a
/// finished one refuses and leaves its log alone.
#[test]
fn the_move_script_can_be_retried_and_keeps_its_log() {
    use std::{fs, process::Command};
    let dir = std::env::temp_dir().join(format!("nv-move-test-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    let (run, stick, data) = (dir.join("run"), dir.join("stick"), dir.join("data"));
    fs::create_dir_all(&run).unwrap();
    fs::create_dir_all(&data).unwrap();
    let script = crate::tool::MOVE_SCRIPT
        .replace("/run/bitsos", run.to_str().unwrap())
        .replace("/stickdata", stick.to_str().unwrap())
        .replace(" /data", &format!(" {}", data.to_str().unwrap()));
    let go = || Command::new("sh").arg("-c").arg(&script).status().unwrap().code();
    let log = run.join("data-move.log");
    // No source: the copy fails, its folder is gone, a retry may start.
    assert_ne!(go(), Some(0));
    assert!(!crate::tool::copy_finished(&fs::read(&log).unwrap()));
    assert!(!run.join("data-move").exists(), "a failed copy leaves no folder that blocks the retry");
    // The retry.
    fs::create_dir_all(&stick).unwrap();
    fs::write(stick.join("f"), b"bytes").unwrap();
    assert_eq!(go(), Some(0));
    let first = fs::read(&log).unwrap();
    assert!(crate::tool::copy_finished(&first), "{}", String::from_utf8_lossy(&first));
    assert_eq!(fs::read(data.join("f")).unwrap(), b"bytes");
    assert!(run.join("data-move/data.sums").exists(), "the lists stay for the compare");
    // A second start refuses and leaves the first one's log.
    assert_eq!(go(), Some(4));
    assert_eq!(fs::read(&log).unwrap(), first);
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn flush_and_fua_order() {
    let r = armed();
    let p3 = r.part(3, DATA.0, DATA.1);
    let b = vec![1u8; BS];
    p3.write(0, &b, false, IoPriority::Normal).unwrap();
    let before = r.hw.st.lock().flushes;
    p3.write_preflush(1, b.clone(), true, IoPriority::Normal).unwrap();
    let st = r.hw.st.lock();
    let w = st.writes.last().copied().unwrap();
    assert_eq!((w.lba, w.fua), (DATA.0 + 1, true));
    assert_eq!(w.after_flushes, before + 1, "the flush came before the commit block");
}

/// Quiesce (Reload, reboot) with a window: shuts the controller down cleanly, no more writes, a recovered
/// controller keeps the window, one with other namespaces loses it.
#[test]
fn quiesce_and_restart() {
    let r = armed();
    let b = vec![2u8; BS];
    r.c.write(1, DATA.0, &b, true).unwrap();
    r.c.flush(1).unwrap();
    r.c.quiesce();
    assert_eq!(r.hw.st.lock().shutdowns, 1, "CC.SHN normal shutdown");
    assert_eq!(r.c.write(1, DATA.0, &b, false), Err(CmdError::Gone));
    r.c.restart().unwrap();
    assert!(r.c.window().is_some(), "same namespaces, same window");
    r.c.write(1, DATA.0, &b, false).unwrap();
    r.c.quiesce();
    r.hw.st.lock().ns[0].data.truncate(1 << 20);
    r.c.restart().unwrap();
    assert!(r.c.window().is_none(), "other namespaces, no window");
}

fn refused(img: Vec<u8>, k: &Key) -> Why {
    let r = rig_on(img);
    let w = drv::check(&*r.c, &r.disk, k).unwrap_err();
    r.unchanged();
    w
}

/// Rewrites one GPT entry in place, both copies, with the CRCs made right again.
fn set_entry(img: &mut [u8], i: usize, first: u64, last: u64) {
    let e = 128 * i;
    for (hdr, arr) in [(1usize, 2usize), (BLOCKS as usize - 1, BLOCKS as usize - 33)] {
        let a = arr * BS + e;
        img[a + 32..a + 40].copy_from_slice(&first.to_le_bytes());
        img[a + 40..a + 48].copy_from_slice(&last.to_le_bytes());
        let c = block::table::crc32(&img[arr * BS..arr * BS + 128 * 128]);
        let h = hdr * BS;
        img[h + 88..h + 92].copy_from_slice(&c.to_le_bytes());
        img[h + 16..h + 20].fill(0);
        let c = block::table::crc32(&img[h..h + 92]);
        img[h + 16..h + 20].copy_from_slice(&c.to_le_bytes());
    }
}

#[test]
fn every_refusal_leaves_the_disk_alone() {
    let k = key();
    // Wrong serial: not this disk.
    assert_eq!(refused(real_disk(), &Key { serial: String::from("OTHER"), ..k.clone() }), Why::OtherSerial);
    // Wrong GUID.
    assert_eq!(refused(real_disk(), &Key { guid: uniq(9), ..k.clone() }), Why::Guid);
    // The key's place differs from the table: moved, resized, either way round.
    assert!(matches!(refused(real_disk(), &Key { first: k.first + 1, ..k.clone() }), Why::Moved { .. }));
    assert!(matches!(refused(real_disk(), &Key { blocks: k.blocks - 1, ..k.clone() }), Why::Resized { .. }));
    let mut moved = real_disk();
    set_entry(&mut moved, 2, DATA.0 + 8, DATA.1);
    assert!(matches!(refused(moved, &k), Why::Moved { .. }));
    let mut shrunk = real_disk();
    set_entry(&mut shrunk, 2, DATA.0, DATA.1 - 8);
    assert!(matches!(refused(shrunk, &k), Why::Resized { .. }));
    // Overlap with another entry (p2 grown over the window's start).
    let mut over = real_disk();
    set_entry(&mut over, 1, WIN.0, DATA.0 + 10);
    assert_eq!(refused(over, &k), Why::Overlap(2));
    // The backup GPT says something else.
    let mut back = real_disk();
    let a = (BLOCKS as usize - 33) * BS + 128 * 2 + 32;
    back[a] ^= 1;
    assert!(matches!(refused(back, &k), Why::Backup(_)));
    let mut gone = real_disk();
    gone[(BLOCKS as usize - 1) * BS..].fill(0);
    assert!(matches!(refused(gone, &k), Why::Backup(_)));
    // An EFI System partition anywhere on the disk, whatever the keys say.
    let parts = [(GUID_EFI_SYSTEM, MSR.0, MSR.1), (GUID_MS_BASIC_DATA, WIN.0, WIN.1), (GUID_MS_BASIC_DATA, DATA.0, DATA.1)];
    let efi = with_pattern(block::testimg::gpt(BS, BLOCKS, &parts), &parts);
    assert!(matches!(refused(efi, &k), Why::Efi(_)));
    // NTFS in the named partition.
    let mut ntfs = real_disk();
    ntfs_at(&mut ntfs, DATA.0);
    assert!(matches!(refused(ntfs, &k), Why::Unclear(_) | Why::Signature(_)), "NTFS at its start");
    let mut tail = real_disk();
    let t = DATA.1 as usize * BS;
    tail[t + 3..t + 11].copy_from_slice(b"NTFS    ");
    assert_eq!(refused(tail, &k), Why::Signature("NTFS boot sector"));
    // Microsoft reserved may never be named, it shows no signature.
    let msr = Key { guid: uniq(0), first: MSR.0, blocks: MSR.1 - MSR.0 + 1, serial: k.serial.clone() };
    assert_eq!(refused(real_disk(), &msr), Why::Kind);
    // No GPT at all.
    assert_eq!(refused(vec![0u8; BLOCKS as usize * BS], &k), Why::NoGpt);
}

#[test]
fn the_key_round_trips() {
    let k = key();
    let v = k.render().unwrap();
    assert_eq!(v, format!("{SERIAL}:{}:{}:{}", guid_text(&uniq(2)), k.first, k.blocks));
    assert_eq!(Key::parse(&v).unwrap(), k);
    assert_eq!(k.code(), "2345AB");
    assert!(Key::parse("x:y:1:2").is_err());
    assert!(Key::parse(&format!("{SERIAL}:{}:1:0", guid_text(&uniq(2)))).is_err(), "empty range");
    assert!(Key::parse(&format!(":{}:1:2", guid_text(&uniq(2)))).is_err(), "no serial");
    assert!(Key { serial: String::from("A#B"), ..k.clone() }.render().is_none(), "a # would start a comment");
}

fn key_ev(code: u16, value: i32, injected: bool, t: u64) -> InputEvent {
    InputEvent { time_ns: t, kind: EV_KEY, code, value, device: 1, injected }
}

/// "2345AB" + Enter as key presses and releases, German or US, on real devices.
fn line() -> Vec<InputEvent> {
    let mut v = Vec::new();
    for (i, c) in [3u16, 4, 5, 6, 30, 48, 28].into_iter().enumerate() {
        v.push(key_ev(c, 1, false, 1000 + i as u64));
        v.push(key_ev(c, 0, false, 1000 + i as u64));
    }
    v
}

#[test]
fn the_typed_code() {
    let at = 2000;
    let j = |e: &[InputEvent], code: &str| typed::judge(e, true, code, at, 1000);
    assert_eq!(j(&line(), "2345AB"), Ok(()));
    assert_eq!(j(&line(), "2345ab"), Ok(()), "case");
    assert_eq!(j(&line(), "2345AC"), Err(Refused::Wrong));
    assert_eq!(j(&line(), "2345A"), Err(Refused::Wrong), "too long");
    assert_eq!(j(&line()[..12], "2345AB"), Err(Refused::NoEnter));
    assert_eq!(typed::judge(&line(), false, "2345AB", at, 1000), Err(Refused::TooOld));
    assert_eq!(typed::judge(&line(), true, "2345AB", 1_000_000, 1000), Err(Refused::Stale));
    // One injected key anywhere, also after the Enter, refuses.
    let mut i = line();
    i[4].injected = true;
    assert_eq!(j(&i, "2345AB"), Err(Refused::Injected));
    let mut i = line();
    i.push(key_ev(30, 1, true, 1500));
    assert_eq!(j(&i, "2345AB"), Err(Refused::Injected));
    // All injected (MCP send_keys).
    let all: Vec<InputEvent> = line().into_iter().map(|e| InputEvent { injected: true, ..e }).collect();
    assert_eq!(j(&all, "2345AB"), Err(Refused::Injected));
    // Backspace corrects, Shift types nothing, repeats type nothing.
    let mut b = vec![key_ev(42, 1, false, 900), key_ev(31, 1, false, 900), key_ev(31, 2, false, 900), key_ev(14, 1, false, 900)];
    b.extend(line());
    assert_eq!(j(&b, "2345AB"), Ok(()));
    // Y and Z trade places on a German keyboard; the keypad's digits count.
    assert!(typed::types(&[21], "Z") && typed::types(&[44], "Z") && typed::types(&[21], "Y") && typed::types(&[44], "Y"));
    assert!(typed::types(&[79, 82, 73], "109"));
    assert!(!typed::types(&[30], "B"));
    assert!(typed::types(&[12], "-"), "a sign has no one place");
}

/// A pool image of 512-byte blocks on the host, read only (fs-ext4's helper has 4096-byte ones).
struct HostPool(std::fs::File, u64);

impl BlockDevice for HostPool {
    fn name(&self) -> &str {
        "host"
    }
    fn info(&self) -> devmodel::block::BlockInfo {
        devmodel::block::BlockInfo { block_size: 512, blocks: self.1 / 512, read_only: true, max_blocks_per_request: 2048, supports_discard: false, has_volatile_cache: false }
    }
    fn read_blocks(&self, lba: u64, buf: &mut [u8], _p: IoPriority) -> Result<(), DevError> {
        std::os::unix::fs::FileExt::read_exact_at(&self.0, buf, lba * 512).map_err(|_| DevError::Io)
    }
    fn write_blocks(&self, _: u64, _: &[u8], _: bool, _: IoPriority) -> Result<(), DevError> {
        Err(DevError::Refused)
    }
    fn flush(&self) -> Result<(), DevError> {
        Ok(())
    }
}

/// BITSVOL_POOL_IMG=<the window's bytes> BITSVOL_OUT=<file>: volume "data" of the pool, for the host's e2fsck
/// (host/tests/nv/41-window-reload.sh, 42-window-setup.sh).
#[test]
#[ignore]
fn extract_volume() {
    let src = std::env::var("BITSVOL_POOL_IMG").expect("BITSVOL_POOL_IMG");
    let out = std::env::var("BITSVOL_OUT").expect("BITSVOL_OUT");
    let f = std::fs::File::open(&src).unwrap();
    let len = f.metadata().unwrap().len();
    let dev: Arc<dyn BlockDevice> = Arc::new(HostPool(f, len));
    let pool = bitsvol::Pool::open(dev, bitsvol::Config::default()).expect("pool opens");
    let id = pool.list().unwrap().into_iter().find(|v| v.label == fs_ext4::data::VOLUME && !v.snapshot).expect("volume data").id;
    let h = bitsvol::PoolHandle::new(pool, "x");
    let vd = h.open_volume(id).unwrap();
    let info = vd.info();
    let bs = info.block_size as u64;
    let total = info.blocks * bs;
    let o = std::fs::File::create(&out).unwrap();
    o.set_len(total).unwrap();
    let mut buf = vec![0u8; 1 << 20];
    let mut at = 0u64;
    while at < total {
        let n = (total - at).min(1 << 20) as usize;
        vd.read_blocks(at / bs, &mut buf[..n], IoPriority::Normal).unwrap();
        // Zeros stay holes: the volume is mostly empty.
        if buf[..n].iter().any(|b| *b != 0) {
            std::os::unix::fs::FileExt::write_all_at(&o, &buf[..n], at).unwrap();
        }
        at += n as u64;
    }
}

#[test]
fn frame_writes_meet_the_window_exactly_as_byte_writes() {
    // BL-2 fix: NsDevice::write_frames (no gather) is refused where write_blocks is, counted the same, and lands
    // only inside the window; through a partition port and the stacked partition too.
    use block::HeapFrames;
    use devmodel::block::IoBuf;
    let fr = |n: usize, v: u8| IoBuf::new(Arc::new(HeapFrames::new(vec![v; n * BS])));
    let r = armed();
    let (first, last) = DATA;
    let d = NsDevice::new(r.c.clone(), 1).unwrap();
    let before = r.c.stats.window_refused.load(core::sync::atomic::Ordering::Relaxed);
    for (lba, n) in [(first - 1, 1usize), (last + 1, 1), (first - 1, 2), (last, 2), (0, 1)] {
        let a = d.write_blocks(lba, &vec![1u8; n * BS], false, IoPriority::Normal);
        let b = d.write_frames(lba, &fr(n, 1), false, IoPriority::Normal);
        assert_eq!((a, b), (Err(DevError::Refused), Err(DevError::Refused)), "{lba}+{n}");
    }
    assert!(r.c.stats.window_refused.load(core::sync::atomic::Ordering::Relaxed) >= before + 10, "each refusal is counted");
    // Larger than one transfer: cut into commands, every one inside.
    let big = r.c.info().max_transfer / BS + 3;
    d.write_frames(first, &fr(big, 0x6C), true, IoPriority::Normal).unwrap();
    {
        let st = r.hw.st.lock();
        assert!(st.ns[0].data[first as usize * BS..(first as usize + big) * BS].iter().all(|&x| x == 0x6C));
        assert_eq!(st.writes.last().map(|w| w.fua), Some(true));
    }
    let port = r.disk.part_port(first, last - first + 1);
    assert_eq!(port.write_frames(first - 1, &fr(1, 2), false, IoPriority::Normal), Err(DevError::Refused));
    assert_eq!(r.disk.part_port(WIN.0, WIN.1 - WIN.0 + 1).write_frames(first, &fr(1, 2), false, IoPriority::Normal), Err(DevError::Refused));
    port.write_frames(last, &fr(1, 0x6D), false, IoPriority::Normal).unwrap();
    assert!(r.disk.write_frames(first, &fr(1, 3), false, IoPriority::Normal).is_err(), "whole disk");
    let p3 = r.part(3, first, last);
    assert!(p3.write_frames(last - first, &fr(2, 4), false, IoPriority::Normal).is_err(), "past the partition");
    r.outside_unchanged();
    drv::disarm(&*r.c, &r.disk);
    assert_eq!(d.write_frames(first, &fr(1, 5), false, IoPriority::Normal), Err(DevError::Refused));
    assert!(p3.write_frames(0, &fr(1, 5), false, IoPriority::Normal).is_err());
    r.outside_unchanged();
}
