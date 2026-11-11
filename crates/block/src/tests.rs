//! Host tests of the block layer: the partition-table look, and `Disk` over a RAM disk with the callers
//! dispatching (no scheduler on the host). The elevator's own tests are in `queue.rs`, the rules in `policy.rs`.

use crate::bdev::DiskFile;
use crate::disk::{Data, Disk, Dispatch};
use crate::error::{BlockError, Refusal};
use crate::policy::{DiskKind, WriteCheck, WriteGate};
use crate::queue::Kind;
use crate::ramdisk::{CallOp, Fault, RamDisk};
use crate::table::{self, Scheme, Verdict};
use crate::testimg;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use devmodel::block::IoPriority::{self, *};
use devmodel::DevError;
use vfs::pagecache::SyncKind;

fn scan_of(img: &[u8], bs: usize) -> table::Scan {
    let total = (img.len() / bs) as u64;
    let mut read = |lba: u64, n: u64| -> Option<Vec<u8>> {
        let a = lba as usize * bs;
        img.get(a..a + n as usize * bs).map(|s| s.to_vec())
    };
    table::scan(&mut read, bs as u32, total)
}

fn is_windows(v: &Verdict) -> bool {
    matches!(v, Verdict::Windows(_))
}

#[test]
fn blank_and_boot_sector_disks_are_clean() {
    let s = scan_of(&vec![0u8; 512 * 64], 512);
    assert_eq!((s.scheme, s.verdict()), (Some(Scheme::None), Verdict::Clean));
    let mut img = vec![0u8; 512 * 64];
    let bs = testimg::boot_sector(b"mkfs.fat", 0x1234_5678);
    img[..512].copy_from_slice(&bs);
    let s = scan_of(&img, 512);
    assert_eq!((s.scheme, s.verdict()), (Some(Scheme::BootSector), Verdict::Clean));
    let mut first = [0u8; 96];
    first.copy_from_slice(&bs[..96]);
    assert!(s.holds_first(&first), "the boot-stick match sees block 0");
}

#[test]
fn an_unreadable_partition_start_makes_the_stick_match_a_doubt() {
    // MBR 0Ch with the boot volume's FAT boot sector at the partition start (the stick layout FS-6 supports).
    let mut img = testimg::mbr(512, 4096, &[(0x0C, 2048, 2048)]);
    let boot = testimg::boot_sector(b"mkfs.fat", 0x2468_ACE0);
    img[2048 * 512..2049 * 512].copy_from_slice(&boot);
    let mut block0 = [0u8; table::FIRST_BYTES];
    block0.copy_from_slice(&boot[..table::FIRST_BYTES]);
    let total = (img.len() / 512) as u64;
    let scan_failing = |fail: Option<u64>| {
        let mut read = |lba: u64, n: u64| -> Option<Vec<u8>> {
            if Some(lba) == fail {
                return None;
            }
            let a = lba as usize * 512;
            img.get(a..a + n as usize * 512).map(|s| s.to_vec())
        };
        table::scan(&mut read, 512, total)
    };
    use crate::policy::{decide, stick_match_with, StickMatch, Switches};
    let whole = scan_failing(None);
    assert!(!whole.incomplete);
    assert_eq!(stick_match_with(Some(block0), &whole), StickMatch::Yes);
    let mut other = block0;
    other[50] ^= 1;
    assert_eq!(stick_match_with(Some(other), &whole), StickMatch::No, "a complete scan without the block: another disk");
    // The partition start fails twice (rescan retries once): the match is a doubt, and so is the whole scan
    // (desk decision R8a, INT-A1e): the verdict is Unknown, which refuses writes before any switch is asked.
    let part = scan_failing(Some(2048));
    assert!(part.incomplete);
    assert!(matches!(part.verdict(), Verdict::Unknown(ref t) if t.contains("LBA 2048")), "{:?}", part.verdict());
    let m = stick_match_with(Some(block0), &part);
    assert_eq!(m, StickMatch::Unknown);
    let sw = Switches { stick_write: false, usb_msd_write: true, nvme_write: false };
    assert_eq!(decide(DiskKind::Usb, false, &part.verdict(), m, sw), Err(Refusal::UnknownTable), "usb_msd_write=on must not open the boot stick");
}

/// Desk decision R8a (INT-A1e): an unreadable partition start may hide a Windows file system inside a
/// non-Windows partition type, so an incomplete scan refuses writes on every kind of disk, NVMe included, with
/// every switch on. A complete scan of the same disk shows what the missing block held.
#[test]
fn an_incomplete_scan_refuses_writes_everywhere() {
    use crate::policy::{decide, StickMatch, Switches};
    // GPT-less MBR, type 83h (Linux) at 2048, but an NTFS boot sector inside it (windows_fs looks there).
    let mut img = testimg::mbr(512, 8192, &[(0x83, 2048, 4096)]);
    let mut ntfs = vec![0u8; 512];
    ntfs[3..11].copy_from_slice(b"NTFS    ");
    ntfs[510] = 0x55;
    ntfs[511] = 0xAA;
    img[2048 * 512..2049 * 512].copy_from_slice(&ntfs);
    let total = (img.len() / 512) as u64;
    let scan_failing = |fail: Option<u64>| {
        let mut read = |lba: u64, n: u64| -> Option<Vec<u8>> {
            if Some(lba) == fail {
                return None;
            }
            let a = lba as usize * 512;
            img.get(a..a + n as usize * 512).map(|s| s.to_vec())
        };
        table::scan(&mut read, 512, total)
    };
    assert!(is_windows(&scan_failing(None).verdict()), "the complete scan sees NTFS in the 83h partition");
    let part = scan_failing(Some(2048));
    assert!(part.incomplete && part.windows.is_empty(), "the unreadable start hides it");
    let v = part.verdict();
    assert!(matches!(v, Verdict::Unknown(_)), "{v:?}");
    let all = Switches { stick_write: true, usb_msd_write: true, nvme_write: true };
    for kind in [DiskKind::Nvme, DiskKind::Usb, DiskKind::Ram, DiskKind::Stacked, DiskKind::Other] {
        for stick in [StickMatch::No, StickMatch::Unknown] {
            assert_eq!(decide(kind, false, &v, stick, all), Err(Refusal::UnknownTable), "{kind:?} {stick:?}");
        }
    }
    // A readable disk without a Windows file system stays clean (the rule does not refuse every disk).
    let mut clean = testimg::mbr(512, 8192, &[(0x83, 2048, 4096)]);
    clean[2048 * 512 + 510] = 0x55;
    let total = (clean.len() / 512) as u64;
    let mut read = |lba: u64, n: u64| -> Option<Vec<u8>> {
        let a = lba as usize * 512;
        clean.get(a..a + n as usize * 512).map(|s| s.to_vec())
    };
    let s = table::scan(&mut read, 512, total);
    assert!(!s.incomplete);
    assert_eq!(s.verdict(), Verdict::Clean);
    assert_eq!(decide(DiskKind::Nvme, false, &s.verdict(), StickMatch::No, all), Ok(()));
}

#[test]
fn windows_file_systems_as_a_whole_disk() {
    for oem in [b"NTFS    ", b"-FVE-FS-", b"ReFS\0\0\0\0"] {
        let mut img = vec![0u8; 512 * 64];
        img[..512].copy_from_slice(&testimg::boot_sector(oem, 1));
        assert!(is_windows(&scan_of(&img, 512).verdict()), "{oem:?}");
    }
}

#[test]
fn mbr_types() {
    let mut img = testimg::mbr(512, 4096, &[(0x0C, 2048, 2048)]);
    img[2048 * 512..2049 * 512].copy_from_slice(&testimg::boot_sector(b"MSWIN4.1", 7));
    let s = scan_of(&img, 512);
    assert_eq!((s.scheme, s.verdict()), (Some(Scheme::Mbr), Verdict::Clean));
    assert_eq!(s.parts.len(), 1);
    assert!(s.firsts.iter().any(|(l, f)| *l == 2048 && f[..11] == img[2048 * 512..2048 * 512 + 11]));
    for (t, windows, efi) in [(0x07u8, true, false), (0x17, true, false), (0x27, true, false), (0x42, true, false), (0xEF, false, true), (0x83, false, false)] {
        let s = scan_of(&testimg::mbr(512, 4096, &[(0x0C, 64, 64), (t, 2048, 1024)]), 512);
        assert_eq!(!s.windows.is_empty(), windows, "{t:#x}");
        assert_eq!(!s.efi.is_empty(), efi, "{t:#x}");
    }
    // An NTFS file system inside a Linux-typed partition counts too.
    let mut img = testimg::mbr(512, 4096, &[(0x83, 2048, 1024)]);
    img[2048 * 512..2049 * 512].copy_from_slice(&testimg::boot_sector(b"NTFS    ", 1));
    assert!(is_windows(&scan_of(&img, 512).verdict()));
}

#[test]
fn extended_partitions_are_walked() {
    // MBR: extended 0Fh at 100 (len 2000). EBR 1 at 100: logical 83h at +10, link to the next EBR at ext+500.
    // EBR 2 at 600: logical 07h at +10.
    let mut img = testimg::mbr(512, 4096, &[(0x0F, 100, 2000)]);
    let ebr = |img: &mut Vec<u8>, at: usize, t: u8, rel: u32, link: Option<u32>| {
        let b = &mut img[at * 512..at * 512 + 512];
        b[0x1BE + 4] = t;
        b[0x1BE + 8..0x1BE + 12].copy_from_slice(&rel.to_le_bytes());
        b[0x1BE + 12..0x1BE + 16].copy_from_slice(&50u32.to_le_bytes());
        if let Some(l) = link {
            b[0x1CE + 4] = 0x05;
            b[0x1CE + 8..0x1CE + 12].copy_from_slice(&l.to_le_bytes());
            b[0x1CE + 12..0x1CE + 16].copy_from_slice(&100u32.to_le_bytes());
        }
        b[510] = 0x55;
        b[511] = 0xAA;
    };
    ebr(&mut img, 100, 0x83, 10, Some(500));
    ebr(&mut img, 600, 0x07, 10, None);
    let s = scan_of(&img, 512);
    assert!(is_windows(&s.verdict()), "{:?}", s.windows);
    assert!(s.windows[0].contains("LBA 610"), "{:?}", s.windows);
    // FS-13: Linux's numbers (/dev/sdb<n>): the extended partition keeps slot 1, logical ones count from 5.
    assert_eq!(s.parts.iter().map(|p| (p.0, p.3)).collect::<Vec<_>>(), [(100, 1), (110, 5), (610, 6)]);
    // A chain that loops back to itself ends (no hang) and finds what it saw.
    let mut img = testimg::mbr(512, 4096, &[(0x05, 100, 2000)]);
    ebr(&mut img, 100, 0x83, 10, Some(0));
    let s = scan_of(&img, 512);
    assert_eq!(s.verdict(), Verdict::Clean);
    // An EBR past the end makes the table unknown.
    let mut img = testimg::mbr(512, 4096, &[(0x05, 100, 2000)]);
    ebr(&mut img, 100, 0x83, 10, Some(1_000_000));
    assert!(matches!(scan_of(&img, 512).verdict(), Verdict::Unknown(_)));
}

#[test]
fn gpt_with_windows_and_efi_partitions() {
    let img = testimg::gpt(512, 8192, &[(table::GUID_EFI_SYSTEM, 2048, 4095), (table::GUID_MS_BASIC_DATA, 4096, 8000)]);
    let s = scan_of(&img, 512);
    assert_eq!(s.scheme, Some(Scheme::Gpt));
    assert_eq!((s.windows.len(), s.efi.len(), s.parts.len()), (1, 1, 2));
    assert!(is_windows(&s.verdict()));
    // EFI alone.
    let s = scan_of(&testimg::gpt(512, 8192, &[(table::GUID_EFI_SYSTEM, 2048, 4095)]), 512);
    assert!(matches!(s.verdict(), Verdict::Efi(_)));
    // Microsoft reserved alone.
    let s = scan_of(&testimg::gpt(512, 8192, &[(table::GUID_MS_RESERVED, 2048, 4095)]), 512);
    assert!(is_windows(&s.verdict()));
    // Linux data only: clean.
    let s = scan_of(&testimg::gpt(512, 8192, &[(table::GUID_LINUX_DATA, 2048, 8000)]), 512);
    assert_eq!((s.scheme, s.verdict()), (Some(Scheme::Gpt), Verdict::Clean));
    // 4 KiB blocks.
    let s = scan_of(&testimg::gpt(4096, 1024, &[(table::GUID_MS_BASIC_DATA, 64, 900)]), 4096);
    assert!(is_windows(&s.verdict()));
}

#[test]
fn gpt_damage() {
    let good = testimg::gpt(512, 8192, &[(table::GUID_MS_BASIC_DATA, 2048, 8000)]);
    // Primary header broken: the backup still shows the Windows partition.
    let mut img = good.clone();
    img[512 + 20] ^= 0xFF;
    assert!(is_windows(&scan_of(&img, 512).verdict()));
    // Both headers broken: unknown (writes refused).
    let last = 8191 * 512;
    img[last + 20] ^= 0xFF;
    assert!(matches!(scan_of(&img, 512).verdict(), Verdict::Unknown(_)));
    // Entry array broken: unknown.
    let mut img = good.clone();
    img[2 * 512 + 40] ^= 0xFF;
    assert!(matches!(scan_of(&img, 512).verdict(), Verdict::Unknown(_)));
    // No protective MBR, but "EFI PART" at LBA 1: still read.
    let mut img = good.clone();
    img[510] = 0;
    assert!(is_windows(&scan_of(&img, 512).verdict()));
    // A hostile entry count (count x size above the bound): the header is not accepted.
    let mut img = good.clone();
    let mut h = img[512..512 + 92].to_vec();
    h[80..84].copy_from_slice(&0x0100_0000u32.to_le_bytes());
    h[16..20].fill(0);
    let c = table::crc32(&h);
    h[16..20].copy_from_slice(&c.to_le_bytes());
    img[512..512 + 92].copy_from_slice(&h);
    img[last..last + 92].copy_from_slice(&h); // the backup too (its my_lba is wrong as well)
    assert!(matches!(scan_of(&img, 512).verdict(), Verdict::Unknown(_)));
    // Unreadable block 0.
    let mut read = |_: u64, _: u64| -> Option<Vec<u8>> { None };
    assert!(matches!(table::scan(&mut read, 512, 100).verdict(), Verdict::Unknown(_)));
}

#[test]
fn crc32_matches_zlib() {
    assert_eq!(table::crc32(b"123456789"), 0xCBF4_3926);
    assert_eq!(table::crc32(b""), 0);
}

// ---- Disk over a RAM disk (callers dispatch: no scheduler on the host)

fn ram(blocks: u64, max: u32) -> Arc<RamDisk> {
    Arc::new(RamDisk::new("hram", 512, blocks).with_max_blocks(max))
}

fn open(r: &Arc<RamDisk>) -> Arc<Disk> {
    Disk::open(r.clone(), DiskKind::Ram, Dispatch::Inline).unwrap()
}

fn pattern(lba: u64, blocks: u64, salt: u8) -> Vec<u8> {
    (0..blocks * 512).map(|i| ((lba * 512 + i) as u8).wrapping_mul(13) ^ salt).collect()
}

#[test]
fn read_write_and_range_errors() {
    let r = ram(256, 16);
    let d = open(&r);
    assert_eq!(d.write_policy(), Ok(()));
    d.write(10, &pattern(10, 3, 1), false, Normal).unwrap();
    assert_eq!(d.read_vec(10, 3 * 512, Interactive).unwrap(), pattern(10, 3, 1));
    let mut buf = vec![0u8; 512];
    d.read(11, &mut buf, Background).unwrap();
    assert_eq!(buf, pattern(10, 3, 1)[512..1024]);
    assert_eq!(d.write(255, &[0; 1024], false, Normal), Err(BlockError::NoSpace));
    assert_eq!(d.read_vec(255, 1024, Normal), Err(BlockError::BeyondEnd));
    assert_eq!(d.write(1, &[0; 100], false, Normal), Err(BlockError::Invalid));
    assert_eq!(d.read_vec(1, 0, Normal), Err(BlockError::Invalid));
    assert_eq!(d.discard(250, 10, Normal), Err(BlockError::NoSpace));
    d.discard(10, 1, Normal).unwrap();
    assert_eq!(d.read_vec(10, 512, Normal).unwrap(), vec![0u8; 512]);
    d.flush(Normal).unwrap();
    assert_eq!(BlockError::NoSpace.errno(), uapi::errno::ENOSPC);
    assert_eq!(BlockError::ReadOnly(Refusal::BootStick).errno(), uapi::errno::EROFS);
    assert_eq!(BlockError::Io.errno(), uapi::errno::EIO);
}

#[test]
fn windows_disk_refuses_every_writing_request() {
    let img = testimg::gpt(512, 8192, &[(table::GUID_EFI_SYSTEM, 2048, 4095), (table::GUID_MS_BASIC_DATA, 4096, 8000)]);
    let r = Arc::new(RamDisk::from_image("win", 512, img.clone(), false));
    r.record();
    let d = open(&r);
    let before = r.calls().len();
    let w = Err(BlockError::ReadOnly(Refusal::WindowsPartition));
    assert_eq!(d.write(5000, &[1; 512], false, Normal), w);
    assert_eq!(d.write(5000, &[1; 512], true, Normal), w);
    assert_eq!(d.write_preflush(5000, vec![1; 512], false, Normal), w);
    assert_eq!(d.discard(5000, 8, Normal), w);
    // Reads and a plain flush pass.
    assert_eq!(d.read_vec(0, 512, Normal).unwrap(), img[..512]);
    d.flush(Normal).unwrap();
    assert!(r.calls()[before..].iter().all(|c| matches!(c.op, CallOp::Read | CallOp::Flush)), "{:?}", r.calls());
    assert_eq!(r.snapshot(), img);
    assert_eq!(d.stats().refused, 4);
    // EFI only.
    let r = Arc::new(RamDisk::from_image("efi", 512, testimg::gpt(512, 8192, &[(table::GUID_EFI_SYSTEM, 2048, 4095)]), false));
    assert_eq!(open(&r).write(5000, &[1; 512], false, Normal), Err(BlockError::ReadOnly(Refusal::EfiPartition)));
    // A read-only device.
    let r = Arc::new(RamDisk::from_image("ro", 512, vec![0; 512 * 64], true));
    assert_eq!(open(&r).write(5, &[1; 512], false, Normal), Err(BlockError::ReadOnly(Refusal::DeviceReadOnly)));
}

#[test]
fn usb_and_unknown_kinds_without_configuration_refuse() {
    // No `configure` in this process: every switch off, the stick unknown.
    let r = ram(64, 16);
    let d = Disk::open(r.clone(), DiskKind::Usb, Dispatch::Inline).unwrap();
    assert_eq!(d.write(1, &[1; 512], false, Normal), Err(BlockError::ReadOnly(Refusal::BootStick)));
    let d = Disk::open(r.clone(), DiskKind::Other, Dispatch::Inline).unwrap();
    assert_eq!(d.write(1, &[1; 512], false, Normal), Err(BlockError::ReadOnly(Refusal::UnknownKind)));
    let d = Disk::open(r.clone(), DiskKind::Nvme, Dispatch::Inline).unwrap();
    assert_eq!(d.discard(1, 1, Normal), Err(BlockError::ReadOnly(Refusal::NvmeWritesOff)));
}

struct No;
impl WriteGate for No {
    fn name(&self) -> &'static str {
        "test-gate"
    }
    fn allows(&self, w: &WriteCheck<'_>) -> bool {
        // A range gate (the shape of FS-7's `\BITSOS2\` rule): blocks below 100 only; some writes can pass.
        match w.range {
            Some((lba, blocks)) => lba + blocks <= 100,
            None => true,
        }
    }
}

struct Never;
impl WriteGate for Never {
    fn name(&self) -> &'static str {
        "never"
    }
    fn allows(&self, _: &WriteCheck<'_>) -> bool {
        false
    }
}

#[test]
fn a_plugged_in_gate_refuses() {
    let r = ram(256, 16);
    let d = open(&r);
    d.add_gate(Arc::new(No));
    d.write(5, &[1; 512], false, Normal).unwrap();
    assert_eq!(d.write(150, &[1; 512], false, Normal), Err(BlockError::ReadOnly(Refusal::Gate("test-gate"))));
    assert_eq!(d.write(99, &[1; 1024], false, Normal), Err(BlockError::ReadOnly(Refusal::Gate("test-gate"))));
    assert_eq!(d.discard(120, 4, Normal), Err(BlockError::ReadOnly(Refusal::Gate("test-gate"))));
}

#[test]
fn a_range_gate_does_not_make_the_whole_disk_read_only() {
    use devmodel::block::BlockDevice;
    let r = ram(256, 16);
    let d = open(&r);
    d.add_gate(Arc::new(No));
    // Some ranges pass: the disk-wide answer (info().read_only, disk.registered) stays writable.
    assert_eq!(d.write_policy(), Ok(()));
    let top = crate::disk::DiskDevice(d.clone());
    assert!(!top.info().read_only);
    top.write_blocks(10, &[7; 512], false, Normal).unwrap();
    assert!(top.write_blocks(200, &[7; 512], false, Normal).is_err());
    // A gate that refuses everything does make it read-only.
    d.add_gate(Arc::new(Never));
    assert_eq!(d.write_policy(), Err(Refusal::Gate("never")));
    assert!(top.info().read_only);
}

#[test]
fn overlapping_write_backs_never_demote_a_durable_one() {
    use vfs::pagecache::Backend;
    let r = ram(64, 16);
    let f = DiskFile::new(open(&r));
    assert_eq!(f.run_priority(), Normal);
    // An fsync begins, then a background write-back begins in its middle: the fsync's runs stay Normal.
    f.begin(SyncKind::Fsync).unwrap();
    f.begin(SyncKind::Background).unwrap();
    assert_eq!(f.run_priority(), Normal);
    // The background one ends first: the fsync is still Normal (the old flag fell back to its value).
    f.commit(SyncKind::Background).unwrap();
    assert_eq!(f.run_priority(), Normal);
    // The other order: the background one is still running after the fsync ends -> Background again.
    f.begin(SyncKind::Background).unwrap();
    f.commit(SyncKind::Fsync).unwrap();
    assert_eq!(f.run_priority(), Background);
    f.commit(SyncKind::Background).unwrap();
    assert_eq!(f.run_priority(), Normal);
    // A commit without its begin does not wrap the count.
    f.commit(SyncKind::Background).unwrap();
    f.begin(SyncKind::Background).unwrap();
    assert_eq!(f.run_priority(), Background);
}

#[test]
fn no_memory_in_the_elevator_fails_the_request_instead_of_spinning() {
    let r = ram(64, 16);
    let d = open(&r);
    d.inject_pick_oom(1);
    // The waiting caller dispatches (inline): the chosen request fails with ENOMEM and the call returns.
    assert_eq!(d.read_vec(3, 512, Normal), Err(BlockError::NoMemory));
    assert_eq!(d.stats().errors, 1);
    assert_eq!(d.stats().queued, 0);
    // Memory back: the next request is served.
    d.write(3, &[9; 512], false, Normal).unwrap();
    assert_eq!(d.read_vec(3, 512, Normal).unwrap(), vec![9; 512]);
}

fn submit_read(d: &Arc<Disk>, lba: u64, blocks: u64, prio: IoPriority) -> crate::Ticket {
    d.submit(Kind::Read, lba, 0, Data::Bytes(vec![0; blocks as usize * 512]), prio).unwrap()
}

fn bytes(r: Result<Data, BlockError>) -> Vec<u8> {
    match r.unwrap() {
        Data::Bytes(v) => v,
        _ => panic!("no bytes"),
    }
}

#[test]
fn queued_adjacent_requests_reach_the_device_merged() {
    let r = ram(1024, 16);
    r.poke(0, &pattern(0, 1024, 7));
    let d = open(&r);
    let read0 = d.stats().read_bytes;
    r.record();
    // Eight adjacent single blocks (submitted out of order) and one far away: queued before anyone waits.
    let order = [3u64, 0, 1, 7, 2, 4, 6, 5];
    let ts: Vec<_> = order.iter().map(|&l| (l, submit_read(&d, l, 1, Normal))).collect();
    let far = submit_read(&d, 500, 2, Normal);
    for (l, t) in ts {
        assert_eq!(bytes(t.wait()), pattern(l, 1, 7));
    }
    assert_eq!(bytes(far.wait()), pattern(500, 2, 7));
    let calls = r.calls();
    assert_eq!(calls.len(), 2, "{calls:?}");
    assert_eq!((calls[0].op, calls[0].lba, calls[0].blocks), (CallOp::Read, 0, 8));
    assert_eq!((calls[1].lba, calls[1].blocks), (500, 2));
    assert_eq!(d.stats().merged, 7);
    // RB-7b CR2: bytes counted per completed request (10 blocks read, nothing written).
    assert_eq!((d.stats().read_bytes - read0, d.stats().write_bytes), (10 * d.block_size() as u64, 0));
    // Merging stops at the device's limit (16 blocks): 40 adjacent blocks in 4-block requests -> 16+16+8.
    r.record();
    let ts: Vec<_> = (0..10).map(|i| submit_read(&d, 100 + i * 4, 4, Normal)).collect();
    for t in ts {
        t.wait().unwrap();
    }
    let sizes: Vec<u64> = r.calls().iter().map(|c| c.blocks).collect();
    assert_eq!(sizes, [16, 16, 8]);
    // Writes merge the same way and land in order.
    r.record();
    let ts: Vec<_> = (0..4)
        .map(|i| d.submit(Kind::Write { fua: false, preflush: false }, 200 + i, 0, Data::Bytes(pattern(200 + i, 1, 9)), Normal).unwrap())
        .collect();
    for t in ts {
        t.wait().unwrap();
    }
    assert_eq!(r.calls().len(), 1);
    assert_eq!(d.read_vec(200, 4 * 512, Normal).unwrap(), pattern(200, 4, 9));
}

#[test]
fn priorities_decide_the_order() {
    let r = ram(1024, 16);
    let d = open(&r);
    r.record();
    let a = submit_read(&d, 100, 1, Background);
    let b = submit_read(&d, 200, 1, Normal);
    let c = submit_read(&d, 300, 1, Interactive);
    a.wait().unwrap();
    b.wait().unwrap();
    c.wait().unwrap();
    let lbas: Vec<u64> = r.calls().iter().map(|c| c.lba).collect();
    assert_eq!(lbas, [300, 200, 100]);
    let prios: Vec<IoPriority> = r.calls().iter().map(|c| c.prio).collect();
    assert_eq!(prios, [Interactive, Normal, Background]);
}

/// VS-H CR 5: a stacked device's timeout stays a timeout on its way through a `Disk`, and the disk can tell it from
/// a media error; programs see EIO for both.
#[test]
fn a_timeout_stays_apart_from_a_media_error() {
    assert_eq!(BlockError::from_dev(DevError::Timeout), BlockError::Timeout);
    assert_eq!(BlockError::Timeout.errno(), uapi::errno::EIO);
    let r = ram(1024, 64);
    let d = open(&r);
    r.inject(Fault { write: false, lba: 13, blocks: 1, error: DevError::Timeout, times: Some(1) });
    assert_eq!(submit_read(&d, 13, 1, Normal).wait().err(), Some(BlockError::Timeout));
    assert!(d.take_timed_out(), "the disk tells a timeout");
    assert!(!d.take_timed_out(), "and forgets it");
    r.inject(Fault { write: false, lba: 14, blocks: 1, error: DevError::Timeout, times: Some(1) });
    assert!(submit_read(&d, 14, 1, Normal).wait().is_err());
    r.inject(Fault { write: false, lba: 15, blocks: 1, error: DevError::Io, times: Some(1) });
    assert_eq!(submit_read(&d, 15, 1, Normal).wait().err(), Some(BlockError::Io));
    assert!(!d.take_timed_out(), "the last failure was a media error");
}

#[test]
fn an_error_in_a_merged_batch_fails_only_its_request() {
    let r = ram(1024, 64);
    r.poke(0, &pattern(0, 1024, 3));
    let d = open(&r);
    r.inject(Fault { write: false, lba: 13, blocks: 1, error: DevError::Io, times: None });
    r.record();
    let ts: Vec<_> = (10..18).map(|l| (l, submit_read(&d, l, 1, Normal))).collect();
    for (l, t) in ts {
        let res = t.wait();
        if l == 13 {
            assert_eq!(res.err(), Some(BlockError::Io));
        } else {
            assert_eq!(bytes(res), pattern(l, 1, 3), "lba {l}");
        }
    }
    let calls = r.calls();
    assert_eq!(calls[0].blocks, 8, "merged first: {calls:?}");
    assert_eq!(calls.len(), 9, "then each alone: {calls:?}");
    assert_eq!(d.stats().merge_retries, 1);
    // A fault that passes after one try: the single request succeeds on its own retry path.
    r.clear_faults();
    r.inject(Fault { write: true, lba: 40, blocks: 1, error: DevError::Io, times: Some(1) });
    assert_eq!(d.write(40, &[5; 512], false, Normal), Err(BlockError::Io));
    d.write(40, &[5; 512], false, Normal).unwrap();
    // Gone from the device is EIO-class `Gone`.
    r.inject(Fault { write: false, lba: 0, blocks: 1024, error: DevError::Gone, times: None });
    assert_eq!(d.read_vec(1, 512, Normal), Err(BlockError::Gone));
}

#[test]
fn page_cache_pages_go_through_scatter_gather() {
    let r = ram(64, 64); // 32 KiB = 8 pages
    let d = open(&r);
    let pc = DiskFile::page_cache(d.clone());
    assert_eq!(pc.size(), 64 * 512);
    // Write 3 pages and a bit through the cache, then fsync: one write of the dirty run, straight from the pages.
    let data: Vec<u8> = (0..3 * 4096 + 1000).map(|i| (i % 251) as u8).collect();
    assert_eq!(pc.write_at(4096, &data).unwrap(), data.len());
    r.record();
    pc.sync_all(SyncKind::Fsync).unwrap();
    let calls = r.calls();
    assert_eq!(calls.len(), 1, "{calls:?}");
    assert_eq!((calls[0].op, calls[0].lba, calls[0].blocks), (CallOp::Write { fua: false }, 8, 32));
    let snap = r.snapshot();
    assert_eq!(&snap[4096..4096 + data.len()], &data[..]);
    // A fresh cache reads it back from the disk.
    let pc2 = DiskFile::page_cache(d.clone());
    let mut back = vec![0u8; data.len()];
    assert_eq!(pc2.read_at(4096, &mut back).unwrap(), data.len());
    assert_eq!(back, data);
    // Background write-back is told as Background.
    pc.write_at(0, &[9; 10]).unwrap();
    r.record();
    pc.sync_all(SyncKind::Background).unwrap();
    assert_eq!(r.calls()[0].prio, Background);
    // A page read that fails is the read's error.
    r.inject(Fault { write: false, lba: 56, blocks: 8, error: DevError::Io, times: None });
    let pc3 = DiskFile::page_cache(d);
    assert_eq!(pc3.read_at(7 * 4096, &mut [0u8; 16]), Err(uapi::errno::EIO));
}

#[test]
fn a_stopped_disk_fails_queued_and_new_requests() {
    let r = ram(64, 16);
    let d = open(&r);
    let t = submit_read(&d, 1, 1, Normal);
    d.stop();
    assert_eq!(t.wait().err(), Some(BlockError::Gone));
    assert_eq!(d.read_vec(1, 512, Normal), Err(BlockError::Gone));
    assert!(d.is_stopped());
}

#[test]
fn many_threads_share_one_disk() {
    let r = ram(4096, 32);
    let d = open(&r);
    let mut hs = Vec::new();
    for t in 0..8u64 {
        let d = d.clone();
        hs.push(std::thread::spawn(move || {
            for i in 0..50u64 {
                let lba = t * 400 + i * 2;
                let v = pattern(lba, 2, t as u8);
                d.write(lba, &v, i % 3 == 0, [Interactive, Normal, Background][(i % 3) as usize]).unwrap();
                assert_eq!(d.read_vec(lba, 1024, Normal).unwrap(), v);
            }
        }));
    }
    for h in hs {
        h.join().unwrap();
    }
    assert_eq!(d.stats().errors, 0);
    assert_eq!(d.stats().queued, 0);
}

/// FS-13: an empty table slot keeps its number free, as in Linux (`/dev/sdb2` stays 2 with slot 1 empty).
#[test]
fn partition_numbers_skip_empty_slots() {
    let s = scan_of(&testimg::mbr(512, 4096, &[(0, 0, 0), (0x0C, 2048, 1024)]), 512);
    assert_eq!(s.parts.iter().map(|p| (p.0, p.3)).collect::<Vec<_>>(), [(2048, 2)]);
    let s = scan_of(&testimg::gpt(512, 8192, &[([0; 16], 0, 0), (table::GUID_MS_BASIC_DATA, 4096, 8000)]), 512);
    assert_eq!(s.parts.iter().map(|p| (p.0, p.3)).collect::<Vec<_>>(), [(4096, 2)]);
}

/// FS-14b: blank cache pages for the backend tests (a cache whose backend reads nothing).
fn blank_pages(n: u64) -> Vec<Arc<vfs::pagecache::CachePage>> {
    use vfs::pagecache::{Backend, CachePage, PageCache};
    struct Nothing;
    impl Backend for Nothing {
        fn read_page(&self, _: u64, _: &Arc<CachePage>) -> Result<(), i64> {
            Ok(())
        }
        fn write_run(&self, _: u64, _: &[Arc<CachePage>], _: u64) -> Result<(), i64> {
            Ok(())
        }
    }
    let c = PageCache::with_backend(Arc::new(Nothing), n * 4096);
    (0..n).map(|i| c.page_for_map(i, false).unwrap()).collect()
}

#[test]
fn read_pages_scatters_the_same_bytes_as_page_by_page_reads_in_fewer_requests() {
    use vfs::pagecache::Backend;
    // 63 blocks: the last page is 7 blocks long.
    let r = ram(63, 16);
    let d = open(&r);
    d.write(0, &pattern(0, 63, 7), false, Normal).unwrap();
    let f = DiskFile::new(d);
    r.record();
    let reads = |r: &RamDisk| r.calls().iter().filter(|c| c.op == CallOp::Read).count();
    let base = reads(&r);
    let one = blank_pages(8);
    for (i, p) in one.iter().enumerate() {
        f.read_page(i as u64, p).unwrap();
    }
    let by_page = reads(&r) - base;
    let run = blank_pages(9);
    // One page past the end: no segment for it, the page stays zeros.
    f.read_pages(0, &run).unwrap();
    let by_run = reads(&r) - base - by_page;
    let want = pattern(0, 63, 7);
    for (i, (a, b)) in one.iter().zip(&run).enumerate() {
        let (mut x, mut y) = (vec![0u8; 4096], vec![0u8; 4096]);
        a.read(0, &mut x);
        b.read(0, &mut y);
        assert_eq!(x, y, "page {i}");
        let end = (want.len() - i * 4096).min(4096);
        assert_eq!(&x[..end], &want[i * 4096..i * 4096 + end], "page {i}");
    }
    let mut past = vec![1u8; 4096];
    run[8].read(0, &mut past);
    assert!(past.iter().all(|&b| b == 0));
    assert_eq!(by_page, 8);
    assert_eq!(by_run, 1, "the run is one request (it fits the dispatcher's bounce chunk)");
    // A run that starts past the end reads nothing.
    f.read_pages(8, &run[..1]).unwrap();
    assert_eq!(reads(&r) - base, by_page + by_run);
}

#[test]
fn the_named_boot_partition_is_the_one_esp_a_window_disk_may_hold() {
    use crate::gpt::window::{Key, Why, check_table};
    use alloc::string::String;
    // The Lexar since 6.11.2026: Windows data, BitsOS's boot partition made in Windows, the BitsOS window.
    let parts = [(table::GUID_MS_BASIC_DATA, 2048, 4095), (table::GUID_EFI_SYSTEM, 4096, 5119), (table::GUID_MS_BASIC_DATA, 5120, 8000)];
    let img = testimg::gpt(512, 8192, &parts);
    let s = scan_of(&img, 512);
    let g = s.gpt.as_ref().unwrap();
    let key_of = |n: usize| Key { serial: String::from("LEXAR"), guid: g.entries[n].unique, first: g.entries[n].first, blocks: g.entries[n].last - g.entries[n].first + 1 };
    let (data, esp) = (key_of(2), key_of(1));
    let check = |e: Option<&Key>| check_table(&data.guid, 8192, &s, &data, e).map(|e| e.number);
    let Err(Why::Efi(text)) = check(None) else { panic!("no nvme_boot: refused as before") };
    assert!(text.ends_with(&alloc::format!("nvme_boot={} names it if it is BitsOS's own", esp.render().unwrap())), "{text}");
    assert_eq!(check(Some(&esp)), Ok(3), "the named one is allowed");
    for bad in [
        Key { serial: String::from("OTHER"), ..esp.clone() },
        Key { guid: g.entries[0].unique, ..esp.clone() },
        Key { first: esp.first + 1, ..esp.clone() },
        Key { blocks: esp.blocks - 1, ..esp.clone() },
        data.clone(),
    ] {
        assert!(matches!(check(Some(&bad)), Err(Why::Efi(_))), "{bad:?}");
    }
    // A second ESP the key does not name.
    let two = [(table::GUID_EFI_SYSTEM, 2048, 4095), (table::GUID_EFI_SYSTEM, 4096, 5119), (table::GUID_MS_BASIC_DATA, 5120, 8000)];
    let s2 = scan_of(&testimg::gpt(512, 8192, &two), 512);
    let g2 = s2.gpt.as_ref().unwrap();
    let k2 = |n: usize| Key { serial: String::from("LEXAR"), guid: g2.entries[n].unique, first: g2.entries[n].first, blocks: g2.entries[n].last - g2.entries[n].first + 1 };
    assert!(matches!(check_table(&k2(2).guid, 8192, &s2, &k2(2), Some(&k2(1))), Err(Why::Efi(_))));
}

#[test]
fn a_window_beside_the_named_boot_partition_writes_and_survives_a_rescan() {
    use devmodel::block::BlockDevice;
    let lexar = [(table::GUID_MS_BASIC_DATA, 2048, 4095), (table::GUID_EFI_SYSTEM, 4096, 5119), (table::GUID_MS_BASIC_DATA, 5120, 8000)];
    let (win, esp) = ((5120, 2881), Some((4096, 1024)));
    let b = [1u8; 512];
    let d = open(&Arc::new(RamDisk::from_image("lexar", 512, testimg::gpt(512, 8192, &lexar), false)));
    d.arm_window(win.0, win.1, esp);
    let port = d.part_port(win.0, win.1);
    port.write_blocks(win.0, &b, false, Normal).unwrap();
    d.rescan();
    assert_eq!(d.window(), Some(win), "the named ESP does not end the window");
    port.write_blocks(win.0 + 1, &b, false, Normal).unwrap();
    assert!(d.part_port(4096, 1024).write_blocks(4096, &b, false, Normal).is_err(), "the ESP itself takes no write");
    assert!(d.write(4096, &b, false, Normal).is_err(), "nor the whole disk");
    // Armed without naming it, or naming another place: refused, and a rescan ends the window.
    for other in [None, Some((4096, 1023)), Some((2048, 2048))] {
        d.arm_window(win.0, win.1, other);
        assert!(port.write_blocks(win.0, &b, false, Normal).is_err(), "{other:?}");
        d.rescan();
        assert_eq!(d.window(), None, "{other:?}");
    }
    // A second ESP on the disk refuses even with the first one named.
    let two = [(table::GUID_EFI_SYSTEM, 2048, 4095), (table::GUID_EFI_SYSTEM, 4096, 5119), (table::GUID_MS_BASIC_DATA, 5120, 8000)];
    let d = open(&Arc::new(RamDisk::from_image("two", 512, testimg::gpt(512, 8192, &two), false)));
    d.arm_window(win.0, win.1, esp);
    assert!(d.part_port(win.0, win.1).write_blocks(win.0, &b, false, Normal).is_err());
}

/// The controller's side in the tests: what it was told, and whether it agrees to open.
struct Lower {
    agree: core::sync::atomic::AtomicBool,
    open: ksync::SpinLock<Option<(u64, u64)>>,
}
impl crate::disk::LowerWindow for Lower {
    fn open_boot(&self, first: u64, blocks: u64) -> bool {
        let ok = self.agree.load(core::sync::atomic::Ordering::Relaxed);
        if ok {
            *self.open.lock() = Some((first, blocks));
        }
        ok
    }
    fn close_boot(&self) {
        *self.open.lock() = None;
    }
}

/// The Lexar booted from its ESP (6.11.2026): MSR, Windows, the ESP nvme_boot= names, the bitsvol window.
const LEXAR: [([u8; 16], u64, u64); 4] =
    [(table::GUID_MS_RESERVED, 34, 2047), (table::GUID_MS_BASIC_DATA, 2048, 4095), (table::GUID_EFI_SYSTEM, 4096, 5119), (table::GUID_MS_BASIC_DATA, 5120, 8000)];

#[test]
fn the_boot_esp_gets_a_second_window_of_its_own() {
    use devmodel::block::BlockDevice;
    let (win, esp) = ((5120, 2881), (4096, 1024));
    let b = [1u8; 512];
    let r = Arc::new(RamDisk::from_image("lexar", 512, testimg::gpt(512, 8192, &LEXAR), false));
    let d = Disk::open(r.clone(), DiskKind::Nvme, Dispatch::Inline).unwrap();
    let lower = Arc::new(Lower { agree: core::sync::atomic::AtomicBool::new(false), open: ksync::SpinLock::new(None) });
    d.set_lower(lower.clone());
    assert_eq!(d.open_boot_window(esp.0, esp.1), Err("no nvme_data= window is armed on this disk"));
    d.arm_window(win.0, win.1, Some(esp));
    let (eport, dport) = (d.part_port(esp.0, esp.1), d.part_port(win.0, win.1));
    assert!(eport.info().read_only, "shut until fs-fat opens it");
    assert!(eport.write_blocks(4096 + 10, &b, false, Normal).is_err());
    assert!(d.open_boot_window(esp.0, esp.1 - 1).is_err(), "only exactly the named ESP");
    assert!(d.open_boot_window(2048, 2048).is_err(), "never a Windows partition");
    assert_eq!(d.open_boot_window(esp.0, esp.1), Err("the controller did not open the boot partition's window"));
    assert_eq!(d.boot_window(), None);
    lower.agree.store(true, core::sync::atomic::Ordering::Relaxed);
    d.open_boot_window(esp.0, esp.1).unwrap();
    assert_eq!((d.boot_window(), *lower.open.lock()), (Some(esp), Some(esp)));
    assert!(!eport.info().read_only);
    r.record();
    eport.write_blocks(4096, &b, false, Normal).unwrap();
    eport.write_blocks(5119, &b, false, Normal).unwrap();
    dport.write_blocks(5120, &b, false, Normal).unwrap();
    dport.write_blocks(8000, &b, false, Normal).unwrap();
    // Around and between: the last Windows block, the first data block through the ESP's port, both across the edge.
    for (lba, n) in [(4095u64, 1u64), (4095, 2), (5120, 1), (5119, 2), (0, 1), (1, 1)] {
        assert!(eport.write_blocks(lba, &vec![1u8; 512 * n as usize], false, Normal).is_err(), "ESP port {lba}+{n}");
    }
    for (lba, n) in [(5119u64, 1u64), (5119, 2), (4096, 1), (8001, 1)] {
        assert!(dport.write_blocks(lba, &vec![1u8; 512 * n as usize], false, Normal).is_err(), "data port {lba}+{n}");
    }
    for (first, blocks) in [(34u64, 2014u64), (2048, 2048)] {
        assert!(d.part_port(first, blocks).write_blocks(first, &b, false, Normal).is_err(), "Windows partition at {first}");
    }
    for lba in [0u64, 1, 2047, 2048, 4096, 5120, 8191] {
        assert!(d.write(lba, &b, false, Normal).is_err(), "the whole disk at {lba}");
    }
    let wrote: Vec<u64> = r.calls().iter().filter(|c| matches!(c.op, CallOp::Write { .. })).map(|c| c.lba).collect();
    assert_eq!(wrote, [4096, 5119, 5120, 8000], "nothing else reached the device");
    // The table goes on matching: both stay. Disarming closes both layers.
    d.rescan();
    assert_eq!((d.window(), d.boot_window()), (Some(win), Some(esp)));
    d.disarm_window();
    assert_eq!((d.boot_window(), *lower.open.lock()), (None, None));
    assert!(eport.write_blocks(4096, &b, false, Normal).is_err());
    // A table that no longer names the ESP: a rescan ends both windows.
    d.arm_window(win.0, win.1, Some(esp));
    d.open_boot_window(esp.0, esp.1).unwrap();
    let moved = [LEXAR[0], LEXAR[1], (table::GUID_EFI_SYSTEM, 4096, 5000), LEXAR[3]];
    r.write_blocks(0, &testimg::gpt(512, 8192, &moved)[..34 * 512], false, Normal).unwrap();
    d.rescan();
    assert_eq!((d.window(), d.boot_window(), *lower.open.lock()), (None, None, None));
}

/// /boot's FAT gate sits on the ESP's own stacked disk: nvme_data's partition, stacked on the same namespace, never
/// asks it.
#[test]
fn the_boot_volumes_gate_is_its_partitions_alone() {
    let (win, esp) = ((5120, 2881), (4096, 1024));
    let b = [1u8; 512];
    let r = Arc::new(RamDisk::from_image("lexar", 512, testimg::gpt(512, 8192, &LEXAR), false));
    let d = Disk::open(r, DiskKind::Nvme, Dispatch::Inline).unwrap();
    d.arm_window(win.0, win.1, Some(esp));
    d.open_boot_window(esp.0, esp.1).unwrap();
    let stack = |first, blocks, n| {
        let p = crate::gpt::Partition::new(Arc::new(d.part_port(first, blocks)), "nvme0n1", first, blocks, n, "x").unwrap();
        Disk::open(Arc::new(p), DiskKind::Stacked, Dispatch::Inline).unwrap()
    };
    let (boot, data) = (stack(esp.0, esp.1, 3), stack(win.0, win.1, 4));
    boot.add_gate(Arc::new(Never));
    assert!(boot.write(10, &b, false, Normal).is_err(), "the boot partition's own gate refuses");
    data.write(10, &b, false, Normal).unwrap();
    data.write(win.1 - 1, &b, false, Normal).unwrap();
    assert!(data.write(win.1, &b, false, Normal).is_err(), "past the partition");
    let open = stack(esp.0, esp.1, 3);
    open.write(0, &b, false, Normal).unwrap();
    assert!(!open.dev_info().read_only);
}

/// A hybrid MBR entry (0Bh, not EFh) mirroring the ESP after the window was armed: its stacked disk's port would be
/// the boot window's, so the boot window stays shut.
#[test]
fn the_boot_window_needs_the_esps_range_to_be_one_partitions() {
    use devmodel::block::BlockDevice;
    let (win, esp) = ((5120, 2881), (4096, 1024));
    let r = Arc::new(RamDisk::from_image("lexar", 512, testimg::gpt(512, 8192, &LEXAR), false));
    let d = Disk::open(r.clone(), DiskKind::Nvme, Dispatch::Inline).unwrap();
    d.arm_window(win.0, win.1, Some(esp));
    let mut mbr = d.read_vec(0, 512, Normal).unwrap();
    let e = &mut mbr[446 + 16..446 + 32];
    e[4] = 0x0B;
    e[8..12].copy_from_slice(&(esp.0 as u32).to_le_bytes());
    e[12..16].copy_from_slice(&(esp.1 as u32).to_le_bytes());
    r.write_blocks(0, &mbr, false, Normal).unwrap();
    d.rescan();
    assert_eq!(d.scan().parts.iter().filter(|p| (p.0, p.1) == esp).count(), 2, "the scan lists the mirror");
    assert_eq!(d.open_boot_window(esp.0, esp.1), Err("not exactly one partition of the disk has the boot partition's range"));
    assert_eq!(d.boot_window(), None);
}

#[test]
fn frame_writes_through_a_port_meet_the_same_window_rules_per_request() {
    // BL-2 fix: write_frames through PartPort goes through `check_port` per request, with the same answer as
    // write_blocks, and a refused write never reaches the device (NV-2p, the boot window, foreign ESPs).
    use crate::frames::HeapFrames;
    use devmodel::block::{BlockDevice, IoBuf};
    let (win, esp) = ((5120, 2881), (4096, 1024));
    let fr = |n: u64| IoBuf::new(Arc::new(HeapFrames::new(vec![7u8; 512 * n as usize])));
    let r = Arc::new(RamDisk::from_image("lexar", 512, testimg::gpt(512, 8192, &LEXAR), false));
    let d = Disk::open(r.clone(), DiskKind::Nvme, Dispatch::Inline).unwrap();
    let lower = Arc::new(Lower { agree: core::sync::atomic::AtomicBool::new(true), open: ksync::SpinLock::new(None) });
    d.set_lower(lower);
    let (eport, dport) = (d.part_port(esp.0, esp.1), d.part_port(win.0, win.1));
    let same = |p: &crate::disk::PartPort, lba: u64, n: u64, what: &str| {
        let a = p.write_blocks(lba, &vec![7u8; 512 * n as usize], false, Normal);
        let b = p.write_frames(lba, &fr(n), false, Normal);
        assert_eq!(a, b, "{what}: {lba}+{n}");
        b
    };
    r.record();
    // Not armed: refused.
    assert!(same(&dport, win.0, 1, "no window").is_err());
    d.arm_window(win.0, win.1, Some(esp));
    // The ESP is shut until its own window opens; outside and across the edges: refused.
    assert!(same(&eport, 4096 + 10, 1, "shut ESP").is_err());
    for (lba, n) in [(5119u64, 1u64), (5119, 2), (4096, 1), (8001, 1)] {
        assert!(same(&dport, lba, n, "data port").is_err());
    }
    for (first, blocks) in [(34u64, 2014u64), (2048, 2048)] {
        assert!(same(&d.part_port(first, blocks), first, 1, "Windows").is_err());
    }
    assert!(r.calls().iter().all(|c| !matches!(c.op, CallOp::Write { .. })), "nothing refused reached the device");
    // Inside: both kinds land.
    d.open_boot_window(esp.0, esp.1).unwrap();
    assert!(same(&dport, 5120, 2, "inside").is_ok());
    assert!(same(&eport, 5118, 2, "inside the ESP").is_ok());
    for (lba, n) in [(4095u64, 2u64), (5119, 2)] {
        assert!(same(&eport, lba, n, "ESP edge").is_err());
    }
    let wrote: Vec<(u64, u64)> = r.calls().iter().filter(|c| matches!(c.op, CallOp::Write { .. })).map(|c| (c.lba, c.blocks)).collect();
    assert_eq!(wrote, [(5120, 2), (5120, 2), (5118, 2), (5118, 2)], "only the writes inside the windows");
    // A foreign second ESP refuses frame writes too.
    let two = [(table::GUID_EFI_SYSTEM, 2048, 4095), (table::GUID_EFI_SYSTEM, 4096, 5119), (table::GUID_MS_BASIC_DATA, 5120, 8000)];
    let d2 = open(&Arc::new(RamDisk::from_image("two", 512, testimg::gpt(512, 8192, &two), false)));
    d2.arm_window(win.0, win.1, Some(esp));
    assert!(same(&d2.part_port(win.0, win.1), win.0, 1, "foreign ESP").is_err());
}
