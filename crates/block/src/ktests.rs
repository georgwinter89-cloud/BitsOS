//! In-kernel tests of FS-6.
//!
//! RAM disk (class Safe, rerunnable: every disk is the test's own; the registry test unregisters its disk):
//! - `contract.block_device`: the G0 contract on the RAM disk.
//! - `block.ram.queue`: the dispatcher thread; while the device is held busy, adjacent requests queue up and
//!   reach it merged up to its limit, in priority order, and every waiter is woken with its own bytes.
//! - `block.ram.aging`: a Background request that waited two aging steps goes before a fresh Interactive one.
//! - `block.ram.timeout`: a request that cannot reach the device times out (taken out of the queue), one the
//!   device holds too (its answer dropped); the disk works on afterwards.
//! - `block.ram.eio`: injected device errors: a merged batch with one bad block fails only that request.
//! - `block.ram.refusal`: registered RAM disks with a Windows+EFI GPT, EFI only, MBR NTFS: every writing request
//!   is EROFS and nothing reaches the device; a clean one takes writes.
//! - `block.bdev.pagecache`: a whole disk as a page cache: dirty pages go to the device as one scatter-gather
//!   write straight from the cached frames, and come back on a fresh cache.
//!
//! USB (the boot stick in every run; the scenario `host/tests/fs/20-block.sh` adds disks made by it):
//! - `block.usb.stick` (Safe, no write): the boot stick is registered, recognised as the stick and its writes
//!   are refused (`stick_write` off) without anything being sent; once FS-7 opened it, raw writes are refused by
//!   FAT's gate (only the sectors FAT announces pass).
//! - `block.usb.stick_write` (QemuOnly): a write of the stick's own block 0 is refused with EROFS.
//! - `block.usb.read` (QemuOnly): scenario disks read through the queue match their pattern.
//! - `block.usb.write` (QemuOnly): the scratch disk takes writes (CONFIG `usb_msd_write=on`), merged and FUA,
//!   flushed, read back; without the switch they are refused.
//! - `block.usb.eio` (QemuOnly): QEMU blkdebug fails one read sector and one write sector of a disk: exactly the
//!   requests that hold them fail with EIO, also inside merged batches.
//! - `block.usb.windows` (QemuOnly): a USB disk with a Microsoft basic data partition and an EFI System
//!   Partition refuses every writing request even with `usb_msd_write=on` (the script checks the image).

use crate::bdev::DiskFile;
use crate::disk::{Data, Disk, Dispatch};
use crate::error::{BlockError, Refusal};
use crate::policy::{self, DiskKind, StickMatch};
use crate::queue::Kind;
use crate::ramdisk::{CallOp, Fault, RamDisk};
use crate::{table, testimg};
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use devmodel::block::{BlockDevice, IoPriority, IoPriority::*};
use devmodel::DevError;
use ktest::{kassert, ktest, TestResult};
use vfs::pagecache::SyncKind;

const MS: u64 = 1_000_000;

fn e<T: core::fmt::Debug>(x: T) -> String {
    format!("{x:?}")
}

fn now() -> u64 {
    ksync::now_ns().unwrap_or(0)
}

/// Waits (sleeping) until `f()` or `ms` passed.
fn until(ms: u64, f: impl Fn() -> bool) -> bool {
    let end = now() + ms * MS;
    while now() < end {
        if f() {
            return true;
        }
        sched::sleep_ns(MS);
    }
    f()
}

fn pattern(lba: u64, blocks: u64, salt: u8) -> Vec<u8> {
    (0..blocks * 512).map(|i| ((lba * 512 + i) as u8).wrapping_mul(13) ^ salt).collect()
}

fn ramdisk(name: &str, blocks: u64, max: u32) -> Result<Arc<RamDisk>, String> {
    Ok(Arc::new(RamDisk::try_new(name, 512, blocks).map_err(e)?.with_max_blocks(max)))
}

fn open(r: &Arc<RamDisk>) -> Result<Arc<Disk>, String> {
    Disk::open(r.clone(), DiskKind::Ram, Dispatch::Thread).map_err(e)
}

fn close(d: &Arc<Disk>) -> TestResult {
    d.stop();
    kassert!(until(2000, || d.worker_done()), "{}: the dispatcher thread did not end", d.name());
    Ok(())
}

fn read_ticket(d: &Arc<Disk>, lba: u64, blocks: u64, prio: IoPriority) -> Result<crate::Ticket, String> {
    d.submit(Kind::Read, lba, 0, Data::Bytes(vec![0; blocks as usize * d.block_size() as usize]), prio).map_err(e)
}

fn bytes(r: Result<Data, BlockError>) -> Result<Vec<u8>, String> {
    match r.map_err(e)? {
        Data::Bytes(v) => Ok(v),
        _ => Err("no bytes".into()),
    }
}

/// Submits `first` and waits until the device holds it (paused), so what follows queues up.
fn hold(r: &RamDisk, d: &Arc<Disk>, lba: u64) -> Result<crate::Ticket, String> {
    r.pause();
    let t = read_ticket(d, lba, 1, Normal)?;
    if !until(2000, || r.holding()) {
        r.resume();
        return Err("the device never got the first request".into());
    }
    Ok(t)
}

// ---- contract.block_device

fn ramdisk_contract() -> TestResult {
    let d = RamDisk::new("ram0", 512, 8);
    let w = [0xA5u8; 1024];
    d.write_blocks(2, &w, false, IoPriority::Normal).map_err(e)?;
    let mut r = [0u8; 1024];
    d.read_blocks(2, &mut r, IoPriority::Interactive).map_err(e)?;
    kassert!(r == w);
    kassert!(d.read_blocks(7, &mut r, IoPriority::Normal).is_err(), "past the end must fail");
    let ro = RamDisk::from_image("ro", 512, vec![0; 512], true);
    kassert!(ro.write_blocks(0, &[0; 512], true, IoPriority::Normal) == Err(DevError::Refused));
    Ok(())
}
ktest!("contract.block_device", Safe, ramdisk_contract);

// ---- block.ram.queue

fn ram_queue() -> TestResult {
    let r = ramdisk("fs6-q", 2048, 16)?;
    r.poke(0, &pattern(0, 2048, 0x31));
    let d = open(&r)?;
    let res = (|| -> TestResult {
        r.record();
        let first = hold(&r, &d, 1000)?;
        // Queued while the device is busy: 8 adjacent Background blocks, 40 adjacent Normal blocks in 4-block
        // requests (merge limit 16), one Interactive far away.
        let bg: Vec<_> = [3u64, 0, 1, 7, 2, 4, 6, 5].iter().map(|&l| read_ticket(&d, l, 1, Background).map(|t| (l, 1, t))).collect::<Result<_, _>>()?;
        let nm: Vec<_> = (0..10).map(|i| read_ticket(&d, 100 + i * 4, 4, Normal).map(|t| (100 + i * 4, 4, t))).collect::<Result<_, _>>()?;
        let it = read_ticket(&d, 1500, 2, Interactive)?;
        kassert!(d.stats().queued == 19, "queued {}", d.stats().queued);
        r.resume();
        bytes(first.wait())?;
        kassert!(bytes(it.wait())? == pattern(1500, 2, 0x31));
        for (l, n, t) in bg.into_iter().chain(nm) {
            kassert!(bytes(t.wait())? == pattern(l, n, 0x31), "lba {l}");
        }
        let calls: Vec<(u64, u64, IoPriority)> = r.calls().iter().map(|c| (c.lba, c.blocks, c.prio)).collect();
        let want = [(1000, 1, Normal), (1500, 2, Interactive), (100, 16, Normal), (116, 16, Normal), (132, 8, Normal), (0, 8, Background)];
        kassert!(calls[..] == want[..], "device calls {calls:?}");
        kassert!(d.stats().merged == 7 + 7, "merged {}", d.stats().merged);
        Ok(())
    })();
    r.resume();
    close(&d)?;
    res
}
ktest!("block.ram.queue", Safe, ram_queue);

// ---- block.ram.aging

fn ram_aging() -> TestResult {
    let r = ramdisk("fs6-age", 256, 16)?;
    let d = open(&r)?;
    d.set_age_ns(5 * MS);
    let res = (|| -> TestResult {
        r.record();
        let first = hold(&r, &d, 0)?;
        let old = read_ticket(&d, 100, 1, Background)?;
        sched::sleep_ns(20 * MS); // > two aging steps
        let fresh = read_ticket(&d, 200, 1, Interactive)?;
        r.resume();
        for t in [first, old, fresh] {
            bytes(t.wait())?;
        }
        let lbas: Vec<u64> = r.calls().iter().map(|c| c.lba).collect();
        kassert!(lbas == [0, 100, 200], "order {lbas:?}");
        kassert!(r.calls()[1].prio == Interactive, "the aged request is told as Interactive: {:?}", r.calls()[1]);
        kassert!(d.stats().aged >= 1);
        Ok(())
    })();
    r.resume();
    close(&d)?;
    res
}
ktest!("block.ram.aging", Safe, ram_aging);

// ---- block.ram.timeout

fn ram_timeout() -> TestResult {
    let r = ramdisk("fs6-to", 256, 16)?;
    let d = open(&r)?;
    let res = (|| -> TestResult {
        r.record();
        let held = hold(&r, &d, 5)?;
        let queued = read_ticket(&d, 50, 1, Interactive)?;
        kassert!(queued.wait_timeout(20 * MS).err() == Some(BlockError::Timeout), "queued request");
        kassert!(held.wait_timeout(20 * MS).err() == Some(BlockError::Timeout), "held request");
        kassert!(d.stats().timeouts == 2 && d.stats().queued == 0);
        r.resume();
        // The disk goes on; the timed-out queued request never reached the device.
        r.poke(60 * 512, &pattern(60, 1, 2));
        kassert!(d.read_vec(60, 512, Normal).map_err(e)? == pattern(60, 1, 2));
        let lbas: Vec<u64> = r.calls().iter().map(|c| c.lba).collect();
        kassert!(lbas == [5, 60], "device calls {lbas:?}");
        Ok(())
    })();
    r.resume();
    close(&d)?;
    res
}
ktest!("block.ram.timeout", Safe, ram_timeout);

// ---- block.ram.eio

fn ram_eio() -> TestResult {
    let r = ramdisk("fs6-eio", 512, 64)?;
    r.poke(0, &pattern(0, 512, 0x44));
    let d = open(&r)?;
    let res = (|| -> TestResult {
        r.inject(Fault { write: false, lba: 13, blocks: 1, error: DevError::Io, times: None });
        r.inject(Fault { write: true, lba: 300, blocks: 1, error: DevError::Io, times: None });
        r.record();
        let first = hold(&r, &d, 400)?;
        let ts: Vec<_> = (10..18).map(|l| read_ticket(&d, l, 1, Normal).map(|t| (l, t))).collect::<Result<_, _>>()?;
        r.resume();
        bytes(first.wait())?;
        for (l, t) in ts {
            let x = t.wait();
            if l == 13 {
                kassert!(x.as_ref().err() == Some(&BlockError::Io), "lba 13: {:?}", x.err());
            } else {
                kassert!(bytes(x)? == pattern(l, 1, 0x44), "lba {l}");
            }
        }
        kassert!(r.calls()[1].blocks == 8 && r.calls().len() == 10, "calls {:?}", r.calls());
        kassert!(d.stats().merge_retries == 1);
        // Writes: the bad block fails, its neighbours land.
        kassert!(d.write(300, &[1; 512], false, Normal) == Err(BlockError::Io));
        d.write(299, &[2; 512], true, Normal).map_err(e)?;
        d.write(301, &[3; 512], false, Normal).map_err(e)?;
        kassert!(d.read_vec(299, 3 * 512, Normal).map_err(e)?[1024..] == [3; 512]);
        kassert!(BlockError::Io.errno() == uapi::errno::EIO);
        Ok(())
    })();
    r.resume();
    close(&d)?;
    res
}
ktest!("block.ram.eio", Safe, ram_eio);

// ---- block.ram.refusal

fn ram_refusal() -> TestResult {
    let cases: [(&str, Vec<u8>, Option<Refusal>); 4] = [
        ("fs6-win", testimg::gpt(512, 8192, &[(table::GUID_EFI_SYSTEM, 2048, 4095), (table::GUID_MS_BASIC_DATA, 4096, 8000)]), Some(Refusal::WindowsPartition)),
        ("fs6-efi", testimg::gpt(512, 8192, &[(table::GUID_EFI_SYSTEM, 2048, 4095)]), Some(Refusal::EfiPartition)),
        ("fs6-ntfs", testimg::mbr(512, 8192, &[(0x07, 2048, 4096)]), Some(Refusal::WindowsPartition)),
        ("fs6-linux", testimg::gpt(512, 8192, &[(table::GUID_LINUX_DATA, 2048, 8000)]), None),
    ];
    for (name, img, want) in cases {
        crate::unregister(name); // a run that failed half-way
        let r = Arc::new(RamDisk::from_image(name, 512, img.clone(), false));
        let d = crate::register(r.clone(), DiskKind::Ram).map_err(e)?;
        let res = (|| -> TestResult {
            kassert!(crate::find(name).is_some());
            r.record();
            let ops: [Result<(), BlockError>; 4] = [
                d.write(5000, &[1; 512], false, Normal),
                d.write(5000, &[1; 512], true, Interactive),
                d.write_preflush(5001, vec![1; 512], false, Normal),
                d.discard(5002, 4, Background),
            ];
            match want {
                Some(why) => {
                    for (i, o) in ops.iter().enumerate() {
                        kassert!(*o == Err(BlockError::ReadOnly(why)), "{name} op {i}: {o:?}");
                    }
                    kassert!(r.calls().is_empty(), "{name}: the device got {:?}", r.calls());
                    kassert!(r.snapshot() == img, "{name}: the image changed");
                    kassert!(d.write_policy() == Err(why));
                    kassert!(crate::DiskDevice(d.clone()).info().read_only, "{name}: a stacked view must say read-only");
                    kassert!(BlockError::ReadOnly(why).errno() == uapi::errno::EROFS);
                }
                None => {
                    for (i, o) in ops.iter().enumerate() {
                        kassert!(o.is_ok(), "{name} op {i}: {o:?}");
                    }
                    kassert!(r.calls().iter().any(|c| c.op == CallOp::Discard));
                }
            }
            // Reads and a plain flush always pass.
            kassert!(d.read_vec(0, 512, Normal).map_err(e)? == img[..512]);
            d.flush(Normal).map_err(e)?;
            Ok(())
        })();
        kassert!(crate::unregister(name), "{name} was not registered");
        kassert!(until(2000, || d.worker_done()));
        res?;
    }
    Ok(())
}
ktest!("block.ram.refusal", Safe, ram_refusal);

// ---- block.bdev.pagecache

fn bdev_pagecache() -> TestResult {
    let r = ramdisk("fs6-pc", 256, 64)?; // 128 KiB = 32 pages
    let d = open(&r)?;
    let res = (|| -> TestResult {
        let pc = DiskFile::page_cache(d.clone());
        kassert!(pc.size() == 256 * 512);
        let data: Vec<u8> = (0..5 * 4096 + 700).map(|i| (i % 253) as u8).collect();
        kassert!(pc.write_at(3 * 4096, &data).map_err(e)? == data.len());
        r.record();
        pc.sync_all(SyncKind::Fsync).map_err(e)?;
        let calls = r.calls();
        // Pages 3..=8 dirty: one write of 6 pages (48 blocks) from LBA 24, gathered from six cache frames.
        kassert!(calls.len() == 1 && calls[0].op == (CallOp::Write { fua: false }) && calls[0].lba == 24 && calls[0].blocks == 48, "calls {calls:?}");
        kassert!(r.snapshot()[3 * 4096..3 * 4096 + data.len()] == data[..]);
        kassert!(pc.cached(3).and_then(|p| p.phys()).is_some(), "cache pages are frames");
        // A fresh cache reads through the disk into its frames (scatter).
        let pc2 = DiskFile::page_cache(d.clone());
        let mut back = vec![0u8; data.len()];
        kassert!(pc2.read_at(3 * 4096, &mut back).map_err(e)? == data.len());
        kassert!(back == data);
        // An error of the device is the read's errno.
        r.inject(Fault { write: false, lba: 240, blocks: 8, error: DevError::Io, times: None });
        let pc3 = DiskFile::page_cache(d.clone());
        kassert!(pc3.read_at(30 * 4096, &mut [0u8; 8]) == Err(uapi::errno::EIO));
        Ok(())
    })();
    close(&d)?;
    res
}
ktest!("block.bdev.pagecache", Safe, bdev_pagecache);

// ---- USB

/// Block 0 of a scenario disk (`host/tests/fs/20-block.sh`): magic, id, flags, a, b.
const MAGIC: &[u8; 24] = b"BITSOS-FS6-BLOCK-V1\0\0\0\0\0";
const F_SCRATCH: u32 = 1;
const F_EIO: u32 = 2;

#[derive(Clone, Copy, Debug)]
struct Header {
    id: u32,
    flags: u32,
    /// SCRATCH: first block of the scratch range; EIO: the read sector blkdebug fails.
    a: u64,
    /// SCRATCH: blocks of the scratch range; EIO: the write sector blkdebug fails.
    b: u64,
}

fn header(b: &[u8]) -> Option<Header> {
    if b.len() < 48 || &b[..24] != MAGIC {
        return None;
    }
    let u32_at = |i: usize| u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]);
    let u64_at = |i: usize| u64::from_le_bytes(b[i..i + 8].try_into().unwrap_or([0; 8]));
    Some(Header { id: u32_at(24), flags: u32_at(28), a: u64_at(32), b: u64_at(40) })
}

/// The scenario's pattern: byte `off` of the disk (blocks 1..; the same formula as the script).
fn spattern(off: u64) -> u8 {
    ((off >> 9).wrapping_mul(31).wrapping_add((off & 511).wrapping_mul(7))) as u8
}

/// `v` holds the disk's bytes from byte `start`.
fn check_spattern(name: &str, start: u64, v: &[u8], xor: u8) -> TestResult {
    for (i, &b) in v.iter().enumerate() {
        let off = start + i as u64;
        if b != spattern(off) ^ xor {
            return Err(format!("{name}: byte {off:#x} is {b:#04x}, want {:#04x}", spattern(off) ^ xor));
        }
    }
    Ok(())
}

/// Waits until USB disks stopped appearing (block registry quiet for 2 s, at least one USB disk; 40 s at most).
fn usb_disks() -> Vec<Arc<Disk>> {
    let end = now() + 40_000 * MS;
    let mut last = crate::generation();
    let mut quiet = now();
    while now() < end {
        sched::sleep_ns(50 * MS);
        let g = crate::generation();
        if g != last {
            last = g;
            quiet = now();
        } else if now() - quiet >= 2000 * MS && crate::disks().iter().any(|d| d.kind() == DiskKind::Usb) {
            break;
        }
    }
    crate::disks().into_iter().filter(|d| d.kind() == DiskKind::Usb).collect()
}

fn scenario_disks() -> Vec<(Arc<Disk>, Header)> {
    usb_disks()
        .into_iter()
        .filter_map(|d| {
            let b = d.read_vec(0, d.block_size() as usize, Normal).ok()?;
            header(&b).map(|h| (d, h))
        })
        .collect()
}

fn usb_stick() -> TestResult {
    let disks = usb_disks();
    if disks.is_empty() {
        return ktest::skip("no USB disk registered");
    }
    if !crate::configured() {
        return Err("block::configure never ran (vfs-sys's block.boot_facts initcall)".into());
    }
    let sticks: Vec<_> = disks.iter().filter(|d| d.stick() == StickMatch::Yes).collect();
    let ident = policy::stick_identity();
    if ident.and_then(|s| s.block0).is_none() {
        // Without the loader's block 0 every USB disk counts as the stick.
        kassert!(disks.iter().all(|d| d.stick() == StickMatch::Unknown));
        return ktest::skip("the loader could not read the stick's block 0: every USB disk is treated as the stick");
    }
    kassert!(sticks.len() == 1, "{} disks match the boot stick (want 1)", sticks.len());
    let s = sticks[0];
    let sw = policy::switches();
    klog::event!(Info, "block", "ktest.stick", disk = String::from(s.name()), table = s.scan().scheme_str(),
        policy = s.write_policy().err().map_or("allowed", |r| r.as_str()), stick_write = sw.stick_write);
    if s.stick_opened() {
        // FS-7 mounted it read-write: writes pass only where its gate allows (the sectors FAT announces);
        // a raw write of block 0 is refused by the gate, before anything is queued.
        kassert!(s.write_policy() == Ok(()), "{}: {:?}", s.name(), s.write_policy());
        let raw = s.check_write(crate::queue::Kind::Write { fua: false, preflush: false }, Some((0, 1)));
        kassert!(raw == Err(Refusal::Gate("fat.stick")), "{}: a raw write of block 0: {raw:?}", s.name());
        kassert!(!crate::DiskDevice(s.clone()).info().read_only);
        return Ok(());
    }
    if sw.stick_write {
        return ktest::skip("stick_write=on: the stick takes writes");
    }
    kassert!(s.write_policy() == Err(stick_refusal(s)), "{}: {:?}", s.name(), s.write_policy());
    // Under the device's own read-only answer the stick rule holds too.
    let rule = policy::decide(DiskKind::Usb, false, &s.scan().verdict(), s.stick(), sw);
    kassert!(rule == Err(Refusal::BootStick), "{}: the stick rule says {rule:?}", s.name());
    kassert!(crate::DiskDevice(s.clone()).info().read_only);
    Ok(())
}

/// With both USB switches off usb-msd's unit refuses writes itself (`usb_writes_possible`), which the hook
/// reports first; otherwise the stick rule answers.
fn stick_refusal(s: &Arc<Disk>) -> Refusal {
    if s.dev_info().read_only { Refusal::DeviceReadOnly } else { Refusal::BootStick }
}
ktest!("block.usb.stick", Safe, usb_stick);

fn usb_stick_write() -> TestResult {
    let Some(s) = usb_disks().into_iter().find(|d| d.stick() == StickMatch::Yes) else {
        return ktest::skip("no USB disk recognised as the boot stick");
    };
    if policy::switches().stick_write && !s.stick_opened() {
        return ktest::skip("stick_write=on");
    }
    let own = s.read_vec(0, s.block_size() as usize, Normal).map_err(e)?;
    let before = s.stats().dispatches;
    // FS-7's gate (armed) refuses every write FAT did not announce; without it the stick rule refuses.
    let w = Err(BlockError::ReadOnly(if s.stick_opened() { Refusal::Gate("fat.stick") } else { stick_refusal(&s) }));
    kassert!(s.write(0, &own, false, Normal) == w);
    kassert!(s.discard(0, 1, Normal) == w);
    kassert!(s.write(0, &own, true, Normal) == w);
    kassert!(s.stats().dispatches == before, "a refused write reached the queue");
    Ok(())
}
ktest!("block.usb.stick_write", QemuOnly, usb_stick_write);

fn usb_read() -> TestResult {
    let disks = scenario_disks();
    if disks.is_empty() {
        return ktest::skip("no scenario disk (host/tests/fs/20-block.sh)");
    }
    for (d, h) in &disks {
        if h.flags & F_EIO != 0 {
            continue; // block.usb.eio reads it
        }
        let blocks = d.dev_info().blocks.min(8192);
        // Requests of odd sizes, all queued at once (merged by the queue), then compared.
        let mut ts = Vec::new();
        let mut at = 1u64;
        let mut k = 1u64;
        while at < blocks {
            let n = k.min(blocks - at);
            ts.push((at, n, read_ticket(d, at, n, [Interactive, Normal, Background][(k % 3) as usize])?));
            at += n;
            k = k % 97 + 7;
        }
        let bs = d.block_size() as usize;
        for (lba, n, t) in ts {
            let v = bytes(t.wait())?;
            kassert!(v.len() == n as usize * bs);
            for (i, blk) in v.chunks(bs).enumerate() {
                let l = lba + i as u64;
                if h.flags & F_SCRATCH != 0 && l >= h.a && l < h.a + h.b {
                    continue; // the write test may have changed it (checked there and by the script)
                }
                check_spattern(d.name(), l * bs as u64, blk, 0)?;
            }
        }
        let s = d.stats();
        klog::event!(Info, "block", "ktest.usb_read", disk = String::from(d.name()), id = h.id, blocks = blocks, requests = s.requests, merged = s.merged,
            dispatches = s.dispatches);
    }
    Ok(())
}
ktest!("block.usb.read", QemuOnly, usb_read);

fn usb_write() -> TestResult {
    let Some((d, h)) = scenario_disks().into_iter().find(|(_, h)| h.flags & F_SCRATCH != 0) else {
        return ktest::skip("no scratch disk (host/tests/fs/20-block.sh)");
    };
    if !policy::switches().usb_msd_write {
        kassert!(d.write(h.a, &vec![0; 512], false, Normal) == Err(BlockError::ReadOnly(Refusal::UsbWritesOff)));
        return Ok(());
    }
    kassert!(d.stick() == StickMatch::No, "the scratch disk is taken for the stick");
    let n = h.b.min(4096);
    let data: Vec<u8> = (0..n * 512).map(|i| spattern(h.a * 512 + i) ^ 0xA5).collect();
    // Queued at once in 8-block requests (merged up to the stick's limit), every third with FUA.
    let mut ts = Vec::new();
    for (i, c) in data.chunks(8 * 512).enumerate() {
        let lba = h.a + i as u64 * 8;
        let k = Kind::Write { fua: i % 3 == 2, preflush: false };
        ts.push(d.submit(k, lba, 0, Data::Bytes(c.to_vec()), Normal).map_err(e)?);
    }
    for t in ts {
        t.wait().map_err(e)?;
    }
    d.flush(Normal).map_err(e)?;
    let back = d.read_vec(h.a, data.len(), Normal).map_err(e)?;
    kassert!(back == data, "{}: read back differs", d.name());
    let s = d.stats();
    klog::event!(Info, "block", "ktest.usb_written", disk = String::from(d.name()), lba = h.a, blocks = n, requests = s.requests, merged = s.merged);
    Ok(())
}
ktest!("block.usb.write", QemuOnly, usb_write);

fn usb_eio() -> TestResult {
    let Some((d, h)) = scenario_disks().into_iter().find(|(_, h)| h.flags & F_EIO != 0) else {
        return ktest::skip("no blkdebug disk (host/tests/fs/20-block.sh)");
    };
    let bs = d.block_size() as u64;
    kassert!(bs == 512);
    let bad = h.a;
    // Alone.
    kassert!(d.read_vec(bad, 512, Normal) == Err(BlockError::Io), "the bad sector read did not fail");
    kassert!(d.read_vec(bad - 4, 4 * 512, Normal).is_ok(), "the blocks before it");
    // Merged: 8 adjacent requests around it; only the one holding it fails.
    let ts: Vec<_> = (bad - 4..bad + 4).map(|l| read_ticket(&d, l, 1, Normal).map(|t| (l, t))).collect::<Result<_, _>>()?;
    for (l, t) in ts {
        let x = t.wait();
        if l == bad {
            kassert!(x.as_ref().err() == Some(&BlockError::Io), "lba {l}: {:?}", x.err());
        } else {
            check_spattern(d.name(), l * 512, &bytes(x)?, 0)?;
        }
    }
    if policy::switches().usb_msd_write {
        let wbad = h.b;
        kassert!(d.write(wbad, &[0x5A; 512], false, Normal) == Err(BlockError::Io), "the bad sector write did not fail");
        let fine = (wbad + 1..wbad + 3).map(spattern_block).collect::<Vec<_>>().concat();
        d.write(wbad + 1, &fine, false, Normal).map_err(e)?;
    }
    klog::event!(Info, "block", "ktest.usb_eio", disk = String::from(d.name()), read_bad = bad, write_bad = h.b, errors = d.stats().errors,
        merge_retries = d.stats().merge_retries);
    Ok(())
}
ktest!("block.usb.eio", QemuOnly, usb_eio);

fn spattern_block(lba: u64) -> Vec<u8> {
    (0..512).map(|i| spattern(lba * 512 + i)).collect()
}

fn usb_windows() -> TestResult {
    let disks = usb_disks();
    let win: Vec<_> = disks.iter().filter(|d| matches!(d.scan().verdict(), table::Verdict::Windows(_))).collect();
    if win.is_empty() {
        return ktest::skip("no USB disk with a Windows partition (host/tests/fs/20-block.sh)");
    }
    for d in win {
        let scan = d.scan();
        kassert!(!scan.efi.is_empty(), "{}: the scenario image also has an EFI partition", d.name());
        let before = d.stats().dispatches;
        let own = d.read_vec(2048, 512, Normal).map_err(e)?;
        let w = Err(BlockError::ReadOnly(Refusal::WindowsPartition));
        kassert!(d.write(2048, &own, false, Normal) == w);
        kassert!(d.write(4096, &own, true, Normal) == w);
        kassert!(d.write_preflush(1, own.clone(), false, Normal) == w);
        kassert!(d.discard(5000, 8, Normal) == w);
        kassert!(d.stats().dispatches == before + 1, "only the read reached the device");
        klog::event!(Info, "block", "ktest.usb_windows", disk = String::from(d.name()), windows = scan.windows.join("; "), efi = scan.efi.join("; "),
            usb_msd_write = policy::switches().usb_msd_write);
    }
    Ok(())
}
ktest!("block.usb.windows", QemuOnly, usb_windows);
