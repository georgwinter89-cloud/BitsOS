//! In-kernel tests of US-4 under QEMU (and the Safe read on the Mini-PC).
//!
//! Disks made by `host/tests/us/50-msd.sh` carry a header in block 0 (`Header`): which block ranges hold the
//! pattern (`pattern(byte offset)`), whether a range is a scratch area the write test may use, and which disk is
//! the one to pull out or to eject. Every other disk (the xtask boot stick in the gate, Georg's stick on the
//! Mini-PC) is only read and checksummed.
//!
//! - `us.msd.read` (Safe): every disk: pattern ranges compared, the whole disk checksummed when it is at most
//!   64 MiB (the scenario compares with the image file), the first 2 MiB read again in odd request sizes and
//!   compared, read speed of 64 MiB in 1 MiB requests (`kib_per_s`; on the Mini-PC the check list wants
//!   >= 16.6 MB/s). Read only; rerunnable.
//! - `us.msd.write` (QemuOnly): writes the scratch range of writable pattern disks (pattern ^ 0xA5; single blocks,
//!   runs across commands, FUA), flush, read back; every other disk refuses a write (of its own block 0 bytes).
//! - `us.msd.sense` (QemuOnly): READ past the end -> ILLEGAL REQUEST 21h, an unknown opcode -> 20h, no reset.
//! - `us.msd.reset` (QemuOnly): a broken CBW makes the device stall until reset recovery; the next read goes
//!   through the CBW timeout, reset recovery (BOT reset, both halts cleared) and works; an explicit recovery too.
//! - `us.msd.luns` (QemuOnly): a device with several LUNs (QEMU usb-bot) gives one disk per LUN.
//! - `us.msd.unplug` (QemuOnly, scenario `usb_msd_unplug_test=on`): reads while the script pulls the device
//!   (QMP device_del): the read ends with `Gone`, the disk leaves the registry, no panic.
//! - `us.msd.medium` (QemuOnly, scenario `usb_msd_medium_test=on`): the script ejects the medium and inserts
//!   another: `Gone` while empty, then the new medium with a new generation.

use crate::UsbDisk;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use devmodel::block::{BlockDevice, IoPriority};
use devmodel::DevError;
use ktest::{kassert, ktest, TestResult};

const MS: u64 = 1_000_000;
const P: IoPriority = IoPriority::Normal;
const MIB: usize = 1 << 20;

pub const MAGIC: &[u8; 24] = b"BITSOS-US4-PATTERN-V1\0\0\0";
pub const F_SCRATCH: u32 = 1;
pub const F_UNPLUG: u32 = 2;
pub const F_MEDIUM: u32 = 4;

/// The byte at `off` of a pattern range (the same function as the scenario's image maker and `sim::pattern`).
pub fn pattern(off: u64) -> u8 {
    ((off >> 9).wrapping_mul(31) ^ (off & 511).wrapping_mul(7) ^ (off >> 17)) as u8
}

/// Block 0 of a scenario disk: magic (24 bytes), id (u32), flags (u32), range count (u32), ranges (LBA u64,
/// blocks u64) in the disk's logical blocks. The first range is the scratch range when `F_SCRATCH` is set.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Header {
    pub id: u32,
    pub flags: u32,
    pub ranges: Vec<(u64, u64)>,
}

impl Header {
    pub fn parse(b: &[u8]) -> Option<Header> {
        if b.len() < 36 || &b[..24] != MAGIC {
            return None;
        }
        let u32_at = |i: usize| u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]);
        let n = u32_at(32) as usize;
        let mut ranges = Vec::new();
        for k in 0..n {
            let at = 36 + k * 16;
            let r = b.get(at..at + 16)?;
            ranges.push((u64::from_le_bytes(r[0..8].try_into().ok()?), u64::from_le_bytes(r[8..16].try_into().ok()?)));
        }
        Some(Header { id: u32_at(24), flags: u32_at(28), ranges })
    }
}

pub(crate) fn now() -> u64 {
    time::now_ns()
}

fn err(e: DevError) -> String {
    format!("{e:?}")
}

/// Waits until USB enumeration is quiet for a second and no LUN scan runs (at most 20 s).
pub(crate) fn settle() -> bool {
    let deadline = now() + 20_000 * MS;
    let mut last = (usb_core::registry::generation(), crate::generation());
    let mut quiet_since = now();
    while now() < deadline {
        sched::sleep_ns(50 * MS);
        let g = (usb_core::registry::generation(), crate::generation());
        if g != last || crate::scanning() > 0 {
            last = g;
            quiet_since = now();
        } else if now() - quiet_since >= 1000 * MS {
            return true;
        }
    }
    false
}

pub(crate) fn ready_disks() -> Vec<Arc<UsbDisk>> {
    crate::disks().into_iter().filter(|d| d.unit().medium).collect()
}

pub(crate) fn read(d: &UsbDisk, lba: u64, bytes: usize) -> Result<Vec<u8>, String> {
    let mut v = vec![0u8; bytes];
    d.read_blocks(lba, &mut v, P).map_err(|e| format!("{} read {lba}+{bytes}: {e:?}", d.name()))?;
    Ok(v)
}

fn header(d: &UsbDisk) -> Option<Header> {
    let bs = d.info().block_size as usize;
    read(d, 0, bs).ok().and_then(|b| Header::parse(&b))
}

/// CRC-32 (IEEE, the zlib one: the scenario checks with Python's `zlib.crc32`).
pub(crate) struct Crc32 {
    table: [u32; 256],
    crc: u32,
}

impl Crc32 {
    pub(crate) fn new() -> Crc32 {
        let mut table = [0u32; 256];
        for (i, t) in table.iter_mut().enumerate() {
            let mut c = i as u32;
            for _ in 0..8 {
                c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
            }
            *t = c;
        }
        Crc32 { table, crc: 0xFFFF_FFFF }
    }
    pub(crate) fn update(&mut self, b: &[u8]) {
        for &x in b {
            self.crc = self.table[((self.crc ^ x as u32) & 0xFF) as usize] ^ (self.crc >> 8);
        }
    }
    pub(crate) fn value(&self) -> u32 {
        !self.crc
    }
}

fn check_pattern(d: &UsbDisk, lba: u64, bytes: &[u8], xor: u8) -> TestResult {
    let bs = d.info().block_size as u64;
    for (i, &b) in bytes.iter().enumerate() {
        let off = lba * bs + i as u64;
        if b != pattern(off) ^ xor {
            return Err(format!("{}: byte {off:#x} is {b:#04x}, want {:#04x}", d.name(), pattern(off) ^ xor));
        }
    }
    Ok(())
}

// ---- us.msd.read

fn read_test() -> TestResult {
    kassert!(settle(), "USB did not settle");
    let disks = ready_disks();
    if disks.is_empty() {
        return ktest::skip("no USB disk with a medium");
    }
    for d in disks {
        read_one(&d)?;
    }
    Ok(())
}
ktest!("us.msd.read", Safe, read_test);

fn read_one(d: &UsbDisk) -> TestResult {
    let info = d.info();
    let bs = info.block_size as usize;
    kassert!(matches!(bs, 512 | 1024 | 2048 | 4096) && info.blocks > 0, "{}: {info:?}", d.name());
    kassert!(info.read_only == !d.writable(), "{}: read_only disagrees with the write switch", d.name());
    let size = info.blocks.saturating_mul(bs as u64);
    let hdr = header(d);
    // Pattern ranges (scenario disks): up to 4 MiB of each.
    if let Some(h) = &hdr {
        for &(lba, blocks) in &h.ranges {
            let n = (blocks as usize).min(4 * MIB / bs);
            let mut at = 0usize;
            while at < n {
                let k = (n - at).min(MIB / bs);
                let v = read(d, lba + at as u64, k * bs)?;
                let xor = if h.flags & F_SCRATCH != 0 && h.ranges[0] == (lba, blocks) && is_written(&v, lba + at as u64, bs) { 0xA5 } else { 0 };
                check_pattern(d, lba + at as u64, &v, xor)?;
                at += k;
            }
        }
    }
    // Speed: 64 MiB (or the whole disk) in 1 MiB requests from LBA 0.
    let span = (size.min(64 * MIB as u64) as usize) / bs * bs;
    let mut first = Vec::new();
    let mut crc = Crc32::new();
    let mut busy = 0u64;
    let mut at = 0usize;
    while at < span {
        let k = (span - at).min(MIB);
        let t0 = now();
        let v = read(d, (at / bs) as u64, k)?;
        busy += now().saturating_sub(t0);
        crc.update(&v);
        if first.len() < 2 * MIB {
            first.extend_from_slice(&v[..v.len().min(2 * MIB - first.len())]);
        }
        at += k;
    }
    // The time inside read_blocks only (the checksum is not the stick's speed).
    let ns = busy.max(1);
    let whole = span as u64 == size;
    klog::event!(Info, "usb", "ktest.msd_read", disk = String::from(d.name()), device = d.storage().path.clone(), lun = d.lun(),
        id = hdr.as_ref().map_or(0, |h| h.id), bytes = span, whole = whole, crc = klog::Hex(crc.value() as u64),
        kib_per_s = (span as u64 * 1_000_000_000 / ns) / 1024, block_size = bs, blocks = info.blocks,
        max_kib = d.storage().max_bytes / 1024, speed = d.storage().speed);
    // Odd request sizes over the first 2 MiB give the same bytes.
    let max = info.max_blocks_per_request as usize;
    let firstn = first.len() / bs;
    for (start, count) in [(1usize, 1usize), (3, 7), (5, max), (2, max + 1), (0, 2 * max + 3), (7, firstn.saturating_sub(7))] {
        if count == 0 || start + count > firstn {
            continue;
        }
        let v = read(d, start as u64, count * bs)?;
        kassert!(v[..] == first[start * bs..(start + count) * bs], "{}: {count} blocks at {start} differ from the 1 MiB pass", d.name());
    }
    Ok(())
}

/// The scratch range may already hold the write test's bytes (a second run in one boot).
fn is_written(v: &[u8], lba: u64, bs: usize) -> bool {
    v.first().is_some_and(|&b| b == pattern(lba * bs as u64) ^ 0xA5)
}

// ---- us.msd.write

fn write_test() -> TestResult {
    kassert!(settle(), "USB did not settle");
    let (mut wrote, mut refused) = (0, 0);
    for d in ready_disks() {
        let bs = d.info().block_size as usize;
        let h = header(&d);
        let scratch = h.as_ref().filter(|h| h.flags & F_SCRATCH != 0 && !h.ranges.is_empty()).map(|h| h.ranges[0]);
        match scratch {
            Some((lba, blocks)) if d.writable() => {
                let n = (blocks as usize).min(2 * MIB / bs);
                let data: Vec<u8> = (0..n * bs).map(|i| pattern(lba * bs as u64 + i as u64) ^ 0xA5).collect();
                let max = d.info().max_blocks_per_request as usize;
                // One block, a run across several commands, the rest with FUA.
                let cuts = [1usize, (2 * max + 5).min(n - 1), n];
                let mut at = 0usize;
                for (i, &end) in cuts.iter().enumerate() {
                    if end <= at {
                        continue;
                    }
                    d.write_blocks(lba + at as u64, &data[at * bs..end * bs], i == 2, P).map_err(err)?;
                    at = end;
                }
                d.flush().map_err(err)?;
                let back = read(&d, lba, n * bs)?;
                kassert!(back == data, "{}: read back differs", d.name());
                klog::event!(Info, "usb", "ktest.msd_written", disk = String::from(d.name()), lba = lba, blocks = n, block_size = bs);
                wrote += 1;
            }
            _ => {
                // R8: a disk without the switch (or not a scratch disk) refuses; the bytes offered are its own.
                let own = read(&d, 0, bs)?;
                if !d.writable() {
                    kassert!(d.write_blocks(0, &own, false, P) == Err(DevError::Refused), "{}: a write was not refused", d.name());
                    kassert!(d.discard(0, 1) == Err(DevError::Refused), "{}: a discard was not refused", d.name());
                    kassert!(d.info().read_only);
                    refused += 1;
                }
            }
        }
    }
    klog::event!(Info, "usb", "ktest.msd_write", written = wrote, refused = refused);
    if wrote + refused == 0 {
        return ktest::skip("no writable scratch disk and no read-only disk");
    }
    Ok(())
}
ktest!("us.msd.write", QemuOnly, write_test);

// ---- us.msd.sense

fn pick() -> Option<Arc<UsbDisk>> {
    let d = ready_disks();
    d.iter().find(|x| header(x).is_some_and(|h| h.flags & (F_UNPLUG | F_MEDIUM) == 0)).cloned().or_else(|| d.into_iter().next())
}

fn resets(d: &UsbDisk) -> u64 {
    d.storage().with_io(P, |io| io.bot.stats.resets).unwrap_or(u64::MAX)
}

fn sense_test() -> TestResult {
    kassert!(settle(), "USB did not settle");
    let Some(d) = pick() else { return ktest::skip("no USB disk with a medium") };
    let info = d.info();
    let r0 = resets(&d);
    if info.blocks < 0xFFFF_FFFF {
        let (r, _) = d.raw(crate::scsi::rw(false, info.blocks, 1, false, false), crate::link::Dir::In, info.block_size as usize).map_err(err)?;
        kassert!(matches!(r, crate::disk::Reply::Check(s) if s.key == 5 && s.asc == 0x21), "{}: READ past the end: {r:?}", d.name());
    }
    let cdb = crate::scsi::Cdb { bytes: [0xC7, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0], len: 6 };
    let (r, _) = d.raw(cdb, crate::link::Dir::None, 0).map_err(err)?;
    kassert!(matches!(r, crate::disk::Reply::Check(s) if s.key == 5 && s.asc == 0x20), "{}: unknown opcode: {r:?}", d.name());
    let (r, _) = d.raw(crate::scsi::test_unit_ready(), crate::link::Dir::None, 0).map_err(err)?;
    kassert!(matches!(r, crate::disk::Reply::Good(_)), "{}: TEST UNIT READY: {r:?}", d.name());
    kassert!(d.read_blocks(info.blocks, &mut vec![0u8; info.block_size as usize], P) == Err(DevError::InvalidArgument));
    read(&d, 0, 8 * info.block_size as usize)?;
    kassert!(resets(&d) == r0, "{}: a CHECK CONDITION is no transport error", d.name());
    Ok(())
}
ktest!("us.msd.sense", QemuOnly, sense_test);

// ---- us.msd.reset

fn reset_test() -> TestResult {
    kassert!(settle(), "USB did not settle");
    let Some(d) = pick() else { return ktest::skip("no USB disk with a medium") };
    let bs = d.info().block_size as usize;
    let before = read(&d, 0, 64 * bs)?;
    let r0 = resets(&d);
    // A CBW with a wrong signature: the device stalls (both pipes until reset recovery, BOT 6.6.1).
    let raw = d.storage().with_io(P, |io| io.bot.link.raw_out(&[0x5A; 31])).map_err(err)?;
    klog::event!(Info, "usb", "ktest.msd_bad_cbw", disk = String::from(d.name()), result = format!("{raw:?}"));
    let t0 = now();
    let after = read(&d, 0, 64 * bs)?;
    kassert!(after == before, "{}: data after reset recovery differs", d.name());
    kassert!(resets(&d) > r0, "{}: no reset recovery happened", d.name());
    klog::event!(Info, "usb", "ktest.msd_recovered", disk = String::from(d.name()), ms = (now() - t0) / MS, resets = resets(&d));
    // An explicit recovery between commands.
    let r = d.storage().with_io(P, |io| io.bot.reset_recovery()).map_err(err)?;
    kassert!(r.is_ok(), "{}: reset recovery: {r:?}", d.name());
    kassert!(read(&d, 0, 64 * bs)? == before);
    Ok(())
}
ktest!("us.msd.reset", QemuOnly, reset_test);

// ---- us.msd.luns

fn luns_test() -> TestResult {
    kassert!(settle(), "USB did not settle");
    for s in crate::storages() {
        let disks: Vec<Arc<UsbDisk>> = crate::disks().into_iter().filter(|d| Arc::ptr_eq(d.storage(), &s)).collect();
        if disks.len() < 2 {
            continue;
        }
        let mut ids = Vec::new();
        for d in &disks {
            kassert!(!ids.iter().any(|&(l, _)| l == d.lun()), "LUN {} twice", d.lun());
            let h = header(d).ok_or_else(|| format!("{}: no header", d.name()))?;
            kassert!(!ids.iter().any(|&(_, id)| id == h.id), "{}: same image as another LUN", d.name());
            ids.push((d.lun(), h.id));
        }
        klog::event!(Info, "usb", "ktest.msd_luns", device = s.path.clone(), luns = format!("{ids:?}"));
        return Ok(());
    }
    ktest::skip("no device with several LUNs (scenario: usb-bot with two scsi-hd)")
}
ktest!("us.msd.luns", QemuOnly, luns_test);

// ---- us.msd.unplug

fn unplug_test() -> TestResult {
    if !pci::registry::opted_in("usb_msd_unplug_test") {
        return ktest::skip("scenario only: host/tests/us/50-msd.sh pulls the device (usb_msd_unplug_test=on)");
    }
    kassert!(settle(), "USB did not settle");
    let d = ready_disks().into_iter().find(|d| header(d).is_some_and(|h| h.flags & F_UNPLUG != 0)).ok_or("no unplug disk")?;
    let bs = d.info().block_size as usize;
    let blocks = d.info().blocks;
    klog::event!(Info, "usb", "ktest.msd_unplug_ready", disk = String::from(d.name()));
    let deadline = now() + 90_000 * MS;
    let mut buf = vec![0u8; MIB];
    let (mut n, mut at) = (0u64, 0u64);
    let e = loop {
        if now() > deadline {
            return Err("the device was never pulled".into());
        }
        let k = (MIB / bs) as u64;
        if at + k > blocks {
            at = 0;
        }
        match d.read_blocks(at, &mut buf, P) {
            Ok(()) => n += 1,
            Err(e) => break e,
        }
        at += k;
    };
    klog::event!(Info, "usb", "ktest.msd_unplugged", disk = String::from(d.name()), reads = n, error = err(e));
    kassert!(e == DevError::Gone, "the read ended with {e:?}, not Gone");
    let t = now() + 10_000 * MS;
    while crate::disks().iter().any(|x| Arc::ptr_eq(x, &d)) && now() < t {
        sched::sleep_ns(20 * MS);
    }
    kassert!(!crate::disks().iter().any(|x| Arc::ptr_eq(x, &d)), "the disk stayed in the registry");
    kassert!(d.is_gone() && d.read_blocks(0, &mut buf[..bs], P) == Err(DevError::Gone));
    kassert!(d.flush() == Err(DevError::Gone));
    Ok(())
}
ktest!("us.msd.unplug", QemuOnly, unplug_test);

// ---- us.msd.medium

fn medium_test() -> TestResult {
    if !pci::registry::opted_in("usb_msd_medium_test") {
        return ktest::skip("scenario only: host/tests/us/50-msd.sh ejects and changes the medium (usb_msd_medium_test=on)");
    }
    kassert!(settle(), "USB did not settle");
    let d = ready_disks().into_iter().find(|d| header(d).is_some_and(|h| h.flags & F_MEDIUM != 0)).ok_or("no medium-change disk")?;
    let id0 = header(&d).map(|h| h.id);
    let g0 = d.generation();
    let bs = d.info().block_size as usize;
    klog::event!(Info, "usb", "ktest.msd_eject_ready", disk = String::from(d.name()), generation = g0);
    let mut b = vec![0u8; 4096.max(bs)];
    let t = now() + 60_000 * MS;
    loop {
        let bs_now = d.info().block_size as usize;
        match d.read_blocks(0, &mut b[..bs_now], P) {
            Err(DevError::Gone) => break,
            _ if now() > t => return Err("the medium never went".into()),
            _ => sched::sleep_ns(100 * MS),
        }
    }
    kassert!(!d.is_gone(), "the device must stay, only its medium went");
    kassert!(d.info().blocks == 0);
    klog::event!(Info, "usb", "ktest.msd_ejected", disk = String::from(d.name()), generation = d.generation());
    let t = now() + 60_000 * MS;
    let h = loop {
        if let Some(h) = header(&d) {
            break h;
        }
        if now() > t {
            return Err("no new medium seen".into());
        }
        sched::sleep_ns(100 * MS);
    };
    kassert!(Some(h.id) != id0, "the old medium came back (id {})", h.id);
    kassert!(d.generation() > g0, "the generation did not change");
    klog::event!(Info, "usb", "ktest.msd_new_medium", disk = String::from(d.name()), id = h.id, blocks = d.info().blocks,
        generation = d.generation());
    Ok(())
}
ktest!("us.msd.medium", QemuOnly, medium_test);
