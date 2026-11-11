//! US-R in-kernel tests with `host/tests/us/55-recover.sh` (one boot, one CPU, QEMU usb-storage on image files):
//! - `us.recover.cpu_stop`: every CPU held for longer than a stage's time while reads run: no stage timed out, no
//!   reset, no error (the bug of 3.11.).
//! - `us.recover.stall`: the script throttles the drive under the stick (QMP) until a data stage really runs out of
//!   time: it costs a retry, the same disk goes on.
//! - `us.recover.port_reset`: a stick that stalls and whose Bulk-Only reset fails: a real port reset in place while a
//!   reader and a writer have requests in flight through the block layer. Same `Device`, same disk, same `block::Disk`,
//!   no generation moved, not one request failed, written data reads back.
//! - `us.recover.boot_stick`: the same on the mounted boot stick (the script checks there was no `stick.gone`).
//! - `us.recover.give_up`: resets that all fail: only then the device goes, and its port finds it again.
//! - `us.recover.clean_stop`: the storage stop before a machine reset leaves a healthy stick alone and gives a sick
//!   one its port reset for the firmware; the kernel going on after all recovers it in place.
//! - `us.recover.hw_port_reset` (WithGeorg): the real port reset on the Mini-PC's boot stick, in place, with a reader
//!   running: what cannot be had there otherwise, because no stall can be ordered. Rerunnable, about a second of pause.
//! The faults past what QEMU's device can do (a Bulk-Only reset that fails) come from `UsbLink::inject_reset_failures`.

use crate::ktests::{now, ready_disks, settle, Header, F_SCRATCH};
use crate::UsbDisk;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use devmodel::block::{BlockDevice, IoPriority};
use ksync::{IrqSpinLock, SpinLock};
use ktest::{kassert, ktest, TestResult};

const MS: u64 = 1_000_000;
const P: IoPriority = IoPriority::Normal;
const MIB: usize = 1 << 20;
const ID_SCRATCH: u32 = 11;
const ID_GIVE_UP: u32 = 12;
const ID_STALL: u32 = 13;

fn header_of(d: &UsbDisk) -> Option<Header> {
    let bs = d.info().block_size as usize;
    crate::ktests::read(d, 0, bs).ok().and_then(|b| Header::parse(&b))
}

fn by_id(id: u32) -> Result<Arc<UsbDisk>, String> {
    ready_disks().into_iter().find(|d| header_of(d).is_some_and(|h| h.id == id)).ok_or_else(|| format!("no disk with image id {id}"))
}

fn stats(d: &UsbDisk) -> Result<crate::bot::BotStats, String> {
    d.storage().with_io(P, |io| io.bot.stats).map_err(|e| format!("{e:?}"))
}

/// What must not move while a device recovers in place.
#[derive(PartialEq, Eq, Debug)]
struct Marks {
    usb: u64,
    msd: u64,
    block: u64,
    gone: bool,
}

fn marks(d: &UsbDisk) -> Marks {
    Marks { usb: usb_core::registry::generation(), msd: crate::generation(), block: block::generation(), gone: d.is_gone() }
}

/// Requests in flight through the block layer while the device recovers.
struct Traffic {
    stop: Arc<AtomicBool>,
    done: Arc<AtomicU64>,
    errors: Arc<SpinLock<Vec<String>>>,
    threads: Vec<sched::JoinHandle>,
}

impl Traffic {
    fn new() -> Traffic {
        Traffic { stop: Arc::new(AtomicBool::new(false)), done: Arc::new(AtomicU64::new(0)), errors: Arc::new(SpinLock::named("t.recover", Vec::new())), threads: Vec::new() }
    }

    /// Reads `span` bytes from `lba` over and over; `compare`: each round compared with the first (not on a stick
    /// whose file system writes there meanwhile).
    fn reader(&mut self, disk: Arc<block::Disk>, lba: u64, span: usize, compare: bool) -> TestResult {
        let (stop, done, errors) = (self.stop.clone(), self.done.clone(), self.errors.clone());
        let want = disk.read_vec(lba, span, P).map_err(|e| format!("first read: {e:?}"))?;
        let h = sched::spawn("t-recover-read", sched::Class::Normal, move || {
            while !stop.load(Ordering::Acquire) {
                match disk.read_vec(lba, span, P) {
                    Ok(v) if !compare || v == want => {
                        done.fetch_add(1, Ordering::AcqRel);
                    }
                    Ok(_) => errors.lock().push(format!("{}: read at {lba} differs", disk.name())),
                    Err(e) => errors.lock().push(format!("{}: read at {lba}: {e:?}", disk.name())),
                }
            }
        }).map_err(|e| format!("{e:?}"))?;
        self.threads.push(h);
        Ok(())
    }

    /// Writes rounds of known bytes to the scratch range and reads each back.
    fn writer(&mut self, disk: Arc<block::Disk>, lba: u64, blocks: u64, bs: usize) -> TestResult {
        let (stop, done, errors) = (self.stop.clone(), self.done.clone(), self.errors.clone());
        let h = sched::spawn("t-recover-write", sched::Class::Normal, move || {
            let mut round = 0u8;
            while !stop.load(Ordering::Acquire) {
                round = round.wrapping_add(1);
                let data = vec![round; blocks as usize * bs];
                if let Err(e) = disk.write(lba, &data, true, P) {
                    errors.lock().push(format!("{}: write at {lba}: {e:?}", disk.name()));
                    continue;
                }
                match disk.read_vec(lba, data.len(), P) {
                    Ok(v) if v == data => {
                        done.fetch_add(1, Ordering::AcqRel);
                    }
                    Ok(_) => errors.lock().push(format!("{}: round {round} did not read back", disk.name())),
                    Err(e) => errors.lock().push(format!("{}: read back: {e:?}", disk.name())),
                }
            }
        }).map_err(|e| format!("{e:?}"))?;
        self.threads.push(h);
        Ok(())
    }

    fn wait_rounds(&self, n: u64, ns: u64) -> bool {
        let start = self.done.load(Ordering::Acquire);
        let end = now() + ns;
        while now() < end {
            if self.done.load(Ordering::Acquire) >= start + n {
                return true;
            }
            sched::sleep_ns(10 * MS);
        }
        false
    }

    fn finish(self) -> TestResult {
        self.stop.store(true, Ordering::Release);
        for t in &self.threads {
            kassert!(t.join_timeout(180_000 * MS), "a traffic thread did not end");
        }
        let e = self.errors.lock();
        kassert!(e.is_empty(), "{} requests failed during the recovery, first: {}", e.len(), e[0]);
        Ok(())
    }
}

/// A stick that stalls (a broken CBW, BOT 6.6.1) and fails its Bulk-Only reset; the next command brings the port
/// reset. Returns when it was done.
fn wedge_and_wait(d: &UsbDisk, bot_fails: u32) -> TestResult {
    let before = stats(d)?.port_resets;
    let raw = d.storage().with_io(P, |io| {
        io.bot.link.inject_reset_failures(bot_fails, 0);
        io.bot.link.raw_out(&[0x5A; 31])
    }).map_err(|e| format!("{e:?}"))?;
    klog::event!(Info, "usb", "ktest.recover_wedged", disk = String::from(d.name()), result = format!("{raw:?}"));
    let end = now() + 120_000 * MS;
    while now() < end {
        if stats(d)?.port_resets > before {
            return Ok(());
        }
        sched::sleep_ns(50 * MS);
    }
    Err(format!("{}: no port reset came", d.name()))
}

// ---- every CPU stands still

static HELD: IrqSpinLock<()> = IrqSpinLock::named("t.recover.held", ());
/// Above the time of a CBW (5 s) and of a data phase of up to 64 KiB (10 s): the stage under way at the stop has run
/// out of time when the CPU comes back. The bug needs the waiter to run before the event thread: several tries.
const HOLD_NS: u64 = 11_000 * MS;
const HOLDS: u32 = 6;
const READ_BYTES: usize = 4096;

fn cpu_stop() -> TestResult {
    kassert!(sched::online_cpus() == 1, "this test needs one CPU to stop all of them; the scenario boots with --smp 1");
    kassert!(settle(), "USB did not settle");
    let d = by_id(ID_SCRATCH)?;
    let disk = block::find(d.name()).ok_or("not in the block layer")?;
    let before = stats(&d)?;
    let m = marks(&d);
    let mut t = Traffic::new();
    t.reader(disk, 4097, READ_BYTES, true)?;
    for round in 0..HOLDS {
        kassert!(t.wait_rounds(2, 60_000 * MS), "round {round}: the reader does not move");
        let end = now() + HOLD_NS;
        let g = HELD.lock();
        while now() < end {
            core::hint::spin_loop();
        }
        drop(g);
    }
    kassert!(t.wait_rounds(2, 60_000 * MS), "the reader does not move after the last stop");
    t.finish()?;
    let after = stats(&d)?;
    kassert!(after.resets == before.resets && after.transport_errors == before.transport_errors,
        "the stops cost {} resets and {} transport errors", after.resets - before.resets, after.transport_errors - before.transport_errors);
    kassert!(marks(&d) == m);
    Ok(())
}
ktest!("us.recover.cpu_stop", Run("us/55-recover"), cpu_stop);

// ---- a real stall under the stick

fn stall() -> TestResult {
    kassert!(settle(), "USB did not settle");
    let d = by_id(ID_STALL)?;
    let bs = d.info().block_size as usize;
    let before = stats(&d)?;
    let m = marks(&d);
    klog::event!(Info, "usb", "ktest.recover_stall_ready", disk = String::from(d.name()));
    // The script throttles the drive; a read then runs out of time, the script lets go, the read goes through.
    let end = now() + 180_000 * MS;
    let mut lba = 1u64;
    while stats(&d)?.resets == before.resets {
        kassert!(now() < end, "the stall never came");
        if lba + (65536 / bs) as u64 > d.info().blocks {
            lba = 1;
        }
        let v = crate::ktests::read(&d, lba, 65536)?;
        let off = lba * bs as u64;
        kassert!(v.iter().enumerate().all(|(i, &b)| b == crate::ktests::pattern(off + i as u64)), "{}: wrong bytes at {lba}", d.name());
        lba += (65536 / bs) as u64;
    }
    kassert!(marks(&d) == m, "the disk changed: {m:?} -> {:?}", marks(&d));
    let after = stats(&d)?;
    kassert!(after.port_resets == before.port_resets, "a Bulk-Only reset was enough here, yet {} port resets", after.port_resets - before.port_resets);
    kassert!(!d.storage().with_io(P, |io| io.bot.unhealthy()).map_err(|e| format!("{e:?}"))?);
    Ok(())
}
ktest!("us.recover.stall", Run("us/55-recover"), stall);

// ---- a port reset in place, with requests in flight

fn port_reset() -> TestResult {
    kassert!(settle(), "USB did not settle");
    let d = by_id(ID_SCRATCH)?;
    let h = header_of(&d).ok_or("no header")?;
    kassert!(h.flags & F_SCRATCH != 0, "the scratch image");
    let (lba, blocks) = h.ranges[0];
    let bs = d.info().block_size as usize;
    let disk = block::find(d.name()).ok_or("not in the block layer")?;
    let m = marks(&d);
    let dev = crate::device_of_disk(&d).ok_or("no device")?;
    let (id, slot) = (dev.id, dev.slot);
    let mut t = Traffic::new();
    t.reader(disk.clone(), 4097, MIB, true)?;
    t.writer(disk.clone(), lba, blocks.min(256), bs)?;
    kassert!(t.wait_rounds(4, 60_000 * MS), "no traffic");
    wedge_and_wait(&d, 1)?;
    kassert!(t.wait_rounds(4, 120_000 * MS), "no traffic after the port reset");
    t.finish()?;
    kassert!(marks(&d) == m, "something moved: {m:?} -> {:?}", marks(&d));
    let now_dev = crate::device_of_disk(&d).ok_or("the device left")?;
    kassert!(now_dev.id == id && now_dev.slot == slot, "another device or slot");
    kassert!(now_dev.address() != 0);
    kassert!(block::find(d.name()).is_some_and(|x| Arc::ptr_eq(&x, &disk)), "another block disk under the same name");
    kassert!(crate::disks().iter().any(|x| Arc::ptr_eq(x, &d)), "another msd disk");
    disk.flush(P).map_err(|e| format!("flush: {e:?}"))?;
    klog::event!(Info, "usb", "ktest.recover_port_reset", disk = String::from(d.name()), id = id, port_resets = stats(&d)?.port_resets,
        reset_failures = stats(&d)?.reset_failures);
    Ok(())
}
ktest!("us.recover.port_reset", Run("us/55-recover"), port_reset);

/// The file on the stick with dirty pages over the reset; the script finds this text in the image afterwards.
const DIRTY_PATH: &[u8] = b"/boot/BITSOS2/RECOVER.TXT";
const DIRTY_TEXT: &[u8] = b"BITSOS-RECOVER-DIRTY-PAGES-WRITTEN-AFTER-THE-PORT-RESET";

/// The xtask boot stick, mounted at /boot: a file with dirty pages stays dirty over the reset and goes out after it.
fn boot_stick() -> TestResult {
    kassert!(settle(), "USB did not settle");
    let disk = block::disks().into_iter().find(|d| d.kind() == block::DiskKind::Usb && d.stick() == block::StickMatch::Yes)
        .ok_or("no boot stick")?;
    let d = crate::find(disk.name()).ok_or("the stick is no msd disk")?;
    // The mount comes a moment after the disk (FAT's gate opens the stick when it has checked its identity).
    let end = now() + 40_000 * MS;
    while !disk.stick_opened() && now() < end {
        sched::sleep_ns(50 * MS);
    }
    kassert!(disk.stick_opened(), "the stick was not opened for writing below \\BITSOS2\\");
    let ns = vfs::root_ns();
    let id_before = ns.read_file("/boot/BITSOS2/STICK.ID", 128).map_err(|x| format!("{x:?}"))?;
    let node = ns.create_file_bytes(DIRTY_PATH, 0o644, DIRTY_TEXT).map_err(|x| format!("{DIRTY_PATH:?}: {x}"))?;
    let m = marks(&d);
    let id = crate::device_of_disk(&d).ok_or("no device")?.id;
    let mut t = Traffic::new();
    t.reader(disk.clone(), 0, MIB, false)?;
    kassert!(t.wait_rounds(2, 60_000 * MS), "no traffic");
    wedge_and_wait(&d, 1)?;
    kassert!(t.wait_rounds(2, 120_000 * MS), "no traffic after the port reset");
    t.finish()?;
    kassert!(marks(&d) == m, "something moved: {m:?} -> {:?}", marks(&d));
    kassert!(crate::device_of_disk(&d).is_some_and(|x| x.id == id), "another device");
    kassert!(block::find(d.name()).is_some_and(|x| Arc::ptr_eq(&x, &disk)), "another block disk");
    // The file system above never noticed: the dirty pages go out, the mount answers.
    node.inode.sync(vfs::pagecache::SyncKind::Fsync).map_err(|x| format!("fsync after the reset: {x:?}"))?;
    disk.flush(P).map_err(|x| format!("flush: {x:?}"))?;
    kassert!(ns.read_file("/boot/BITSOS2/STICK.ID", 128).map_err(|x| format!("{x:?}"))? == id_before, "the mount answers differently");
    kassert!(ns.read_file("/boot/BITSOS2/RECOVER.TXT", 128).map_err(|x| format!("{x:?}"))? == DIRTY_TEXT, "the dirty file differs");
    klog::event!(Info, "usb", "ktest.recover_boot_stick", disk = String::from(d.name()));
    Ok(())
}
ktest!("us.recover.boot_stick", Run("us/55-recover"), boot_stick);

// ---- nothing helps

fn give_up() -> TestResult {
    kassert!(settle(), "USB did not settle");
    let d = by_id(ID_GIVE_UP)?;
    let old = crate::device_of_disk(&d).ok_or("no device")?;
    let raw = d.storage().with_io(P, |io| {
        io.bot.link.inject_reset_failures(100, 100);
        io.bot.link.raw_out(&[0x5A; 31])
    }).map_err(|e| format!("{e:?}"))?;
    klog::event!(Info, "usb", "ktest.recover_hopeless", disk = String::from(d.name()), result = format!("{raw:?}"));
    let bs = d.info().block_size as usize;
    let mut buf = vec![0u8; bs];
    let r = d.read_blocks(0, &mut buf, P);
    kassert!(r.is_err(), "a read went through a stick that takes no reset");
    let end = now() + 60_000 * MS;
    while !old.is_gone() && now() < end {
        sched::sleep_ns(20 * MS);
    }
    kassert!(old.is_gone(), "the ladder ended and the device stayed");
    kassert!(d.is_gone() || d.read_blocks(0, &mut buf, P).is_err());
    kassert!(settle(), "USB did not settle after the detach");
    let again = by_id(ID_GIVE_UP)?;
    kassert!(!Arc::ptr_eq(&again, &d), "the same disk object");
    kassert!(crate::device_of_disk(&again).is_some_and(|x| x.id != old.id), "the old device");
    klog::event!(Info, "usb", "ktest.recover_gave_up", old = String::from(d.name()), new = String::from(again.name()));
    Ok(())
}
ktest!("us.recover.give_up", Run("us/55-recover"), give_up);

// ---- the stop before a machine reset

fn clean_stop() -> TestResult {
    kassert!(settle(), "USB did not settle");
    let d = by_id(ID_SCRATCH)?;
    let s = d.storage().clone();
    let m = marks(&d);
    let deadline = || now() + 5_000 * MS;
    // Healthy: nothing happens to it.
    let before = stats(&d)?;
    crate::shutdown::stop_storages(core::slice::from_ref(&s), deadline());
    kassert!(!block::shutdown::cold_reset_asked(), "a healthy stick asked for a cold reset");
    s.resume_io();
    kassert!(stats(&d)?.resets == before.resets);
    // Sick: its port is reset. The kernel goes on (a Reload that did not jump): the same disk works on.
    s.with_io(P, |io| io.bot.mark_reset_due()).map_err(|e| format!("{e:?}"))?;
    crate::shutdown::stop_storages(core::slice::from_ref(&s), deadline());
    kassert!(block::shutdown::cold_reset_asked(), "the sick stick did not ask for the cold reset");
    s.resume_io();
    block::shutdown::withdraw_cold_reset();
    kassert!(!block::shutdown::cold_reset_asked(), "the request stayed after the test");
    let back = crate::ktests::read(&d, 4097, 65536)?;
    kassert!(back.iter().enumerate().all(|(i, &b)| b == crate::ktests::pattern(4097 * 512 + i as u64)), "wrong bytes after the clean stop");
    kassert!(marks(&d) == m, "something moved: {m:?} -> {:?}", marks(&d));
    Ok(())
}
ktest!("us.recover.clean_stop", Run("us/55-recover"), clean_stop);

// ---- the real thing on the Mini-PC

fn hw_port_reset() -> TestResult {
    kassert!(settle(), "USB did not settle");
    let disk = block::disks().into_iter().find(|d| d.kind() == block::DiskKind::Usb && d.stick() == block::StickMatch::Yes)
        .ok_or("no boot stick")?;
    let d = crate::find(disk.name()).ok_or("the stick is no msd disk")?;
    let m = marks(&d);
    let dev = crate::device_of_disk(&d).ok_or("no device")?;
    let ns = vfs::root_ns();
    let id_before = ns.read_file("/boot/BITSOS2/STICK.ID", 128).map_err(|x| format!("{x:?}"))?;
    let mut t = Traffic::new();
    t.reader(disk.clone(), 0, 64 * 1024, false)?;
    kassert!(t.wait_rounds(2, 60_000 * MS), "no traffic");
    let t0 = now();
    let r = d.storage().with_io(P, |io| io.bot.port_reset_now(false, usb_core::reset::RESET_WAIT_NS)).map_err(|e| format!("{e:?}"))?;
    let ms = (now() - t0) / MS;
    kassert!(t.wait_rounds(2, 120_000 * MS), "no traffic after the port reset");
    t.finish()?;
    kassert!(r.is_ok(), "the port reset: {r:?}");
    kassert!(marks(&d) == m, "something moved: {m:?} -> {:?}", marks(&d));
    kassert!(crate::device_of_disk(&d).is_some_and(|x| x.id == dev.id && x.slot == dev.slot), "another device");
    kassert!(ns.read_file("/boot/BITSOS2/STICK.ID", 128).map_err(|x| format!("{x:?}"))? == id_before, "the mount answers differently");
    klog::event!(Info, "usb", "ktest.recover_hw", disk = String::from(d.name()), device = dev.path(), speed = dev.speed().as_str(),
        address = dev.address(), ms = ms);
    Ok(())
}
// `WithGeorg`, not `Hardware`: it resets the boot stick in use, and if that goes wrong only Georg gets the machine back
// (a replug, or the reboot that may come up in Windows). It must not ride along in every round on the Mini-PC.
ktest!("us.recover.hw_port_reset", WithGeorg, hw_port_reset);
