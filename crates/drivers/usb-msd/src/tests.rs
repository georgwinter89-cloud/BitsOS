//! Host tests: the BOT and SCSI layers and the block device against the simulated stick (`sim`).

use crate::bot::{self, Bot, BotError, Csw, Op};
use crate::disk::{Gate, MsdDisk, Storage, BECOMING_NS};
use crate::link::{Cmd, Dir, Link, Ran};
use crate::scsi::{self, Sense};
use crate::sim::{pattern_block, Fault, Lun, SimDev, SimLink, Stage};
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use devmodel::block::{BlockDevice, IoPriority};
use devmodel::DevError;
use usb_core::{Request, UsbError, XferResult};

const P: IoPriority = IoPriority::Normal;

type Setup = (Arc<SimDev>, Arc<Storage<SimLink>>, Vec<Arc<MsdDisk<SimLink>>>);

fn setup_with(luns: Vec<Lun>, max_bytes: usize, writes: bool) -> Setup {
    let dev = SimDev::new(luns);
    let s = Arc::new(Storage::new(SimLink(dev.clone()), max_bytes, writes, false));
    let disks = s.scan().expect("scan");
    (dev, s, disks)
}

fn setup(lun: Lun) -> (Arc<SimDev>, Arc<MsdDisk<SimLink>>) {
    let (dev, _s, mut d) = setup_with(vec![lun], 120 * 1024, false);
    assert_eq!(d.len(), 1);
    (dev, d.remove(0))
}

fn expect(lba: u64, n: u64, bs: usize) -> Vec<u8> {
    (lba..lba + n).flat_map(|b| pattern_block(b, bs)).collect()
}

fn read(d: &MsdDisk<SimLink>, lba: u64, n: usize) -> Result<Vec<u8>, DevError> {
    let mut v = vec![0u8; n * d.info().block_size.max(512) as usize];
    d.read_blocks(lba, &mut v, P).map(|_| v)
}

// ---- wire formats

#[test]
fn cbw_and_csw_on_the_wire() {
    let c = bot::cbw(0x1234_5678, 4096, Dir::In, 3, &scsi::rw(false, 8, 8, false, false));
    assert_eq!(&c[0..4], b"USBC");
    assert_eq!(&c[4..8], &[0x78, 0x56, 0x34, 0x12]);
    assert_eq!(&c[8..12], &4096u32.to_le_bytes());
    assert_eq!((c[12], c[13], c[14]), (0x80, 3, 10));
    assert_eq!(&c[15..25], &[0x28, 0, 0, 0, 0, 8, 0, 0, 8, 0]);
    assert!(c[25..].iter().all(|&b| b == 0));
    let c = bot::cbw(1, 0, Dir::None, 0x1F, &scsi::test_unit_ready());
    assert_eq!((c[12], c[13], c[14]), (0, 0x0F, 6), "LUN is 4 bits");
    let s = Csw::parse(&[b'U', b'S', b'B', b'S', 2, 0, 0, 0, 0x10, 0, 0, 0, 1]);
    assert_eq!(s, Csw { signature: *b"USBS", tag: 2, residue: 16, status: 1 });
}

// ---- bring-up

#[test]
fn bring_up_two_luns_with_512_and_4k_blocks() {
    let (dev, s, d) = setup_with(vec![Lun::new(512, 0x10_0000), Lun::new(4096, 0x8000)], 120 * 1024, false);
    assert_eq!(d.len(), 2);
    assert_eq!((d[0].lun(), d[1].lun()), (0, 1));
    let (a, b) = (d[0].info(), d[1].info());
    assert_eq!((a.block_size, a.blocks, a.read_only), (512, 0x10_0000, true), "read-only without the R8 switch");
    assert_eq!((b.block_size, b.blocks), (4096, 0x8000));
    assert_eq!(a.max_blocks_per_request, 240);
    assert_eq!(b.max_blocks_per_request, 30, "the byte limit counts, not 240 blocks");
    assert!(!a.supports_discard && a.has_volatile_cache);
    let u = d[0].unit();
    assert!(u.medium && u.removable && !u.use16);
    assert_eq!((u.inquiry.vendor.as_str(), u.inquiry.product.as_str()), ("BITSSIM", "SIMULATED STICK"));
    // The fresh stick's UNIT ATTENTION 29h was taken by TEST UNIT READY and asked again.
    assert!(dev.with(|s| s.log.iter().any(|l| l.starts_with("cbw 00 lun 0"))));
    assert_eq!(read(&d[0], 5, 3).unwrap(), expect(5, 3, 512));
    assert_eq!(read(&d[1], 7, 2).unwrap(), expect(7, 2, 4096));
    assert!(dev.with(|s| s.log.iter().any(|l| l.starts_with("cbw 28 lun 1"))), "LUN 1 in its CBWs");
    assert_eq!(s.path, "sim0-1 if0");
}

#[test]
fn max_lun_answers_and_luns_that_are_no_disk() {
    // A stall (or any refusal) of GET MAX LUN means one LUN; a value above 15 too.
    for answer in [Err(UsbError::Stall), Ok(20)] {
        let dev = SimDev::new(vec![Lun::new(512, 1000), Lun::new(512, 1000)]);
        dev.with(|s| s.max_lun = answer);
        let s = Arc::new(Storage::new(SimLink(dev.clone()), 65536, false, false));
        assert_eq!(s.scan().unwrap().len(), 1, "{answer:?}");
    }
    let mut cd = Lun::new(2048, 1000);
    cd.device_type = 5;
    let (_dev, _s, d) = setup_with(vec![cd, Lun::new(512, 64)], 65536, false);
    assert_eq!(d.len(), 1, "the CD-ROM LUN is left out");
    assert_eq!(d[0].lun(), 1);
}

#[test]
fn becoming_ready_waits_and_an_empty_reader_is_kept() {
    let mut l = Lun::new(512, 1000);
    l.becoming_ready = 30; // 30 x 50 ms: past 3 s, within 15 s once it said "becoming ready"
    let (dev, d) = setup(l);
    assert!(d.unit().medium);
    assert!(dev.with(|s| s.luns[0].becoming_ready) == 0);
    // Longer than 15 s: kept without medium.
    let mut l = Lun::new(512, 1000);
    l.becoming_ready = (BECOMING_NS / crate::disk::RETRY_PAUSE_NS) as u32 + 10;
    let (_dev, d) = setup(l);
    assert!(!d.unit().medium && d.info().blocks == 0);
    // No medium: at once, kept; reads say Gone; a medium put in is found on the next request.
    let mut l = Lun::new(512, 1000);
    l.medium = false;
    let (dev, d) = setup(l);
    assert!(!d.unit().medium);
    assert_eq!(read(&d, 0, 1), Err(DevError::Gone));
    let g = d.generation();
    dev.with(|s| {
        s.luns[0].medium = true;
        s.luns[0].blocks = 2000;
        s.luns[0].pending.push_back(Sense { key: 6, asc: 0x28, ascq: 0 });
    });
    assert_eq!(read(&d, 1500, 2).unwrap(), expect(1500, 2, 512));
    assert!(d.generation() > g && d.info().blocks == 2000);
}

#[test]
fn big_disks_use_read_capacity_16_and_16_byte_commands() {
    let (dev, d) = setup(Lun::new(512, 0x1_8000_0000));
    assert_eq!(d.info().blocks, 0x1_8000_0000);
    assert!(d.unit().use16);
    assert!(dev.opcodes().contains(&0x9E), "READ CAPACITY(16) after (10) said 0xFFFFFFFF");
    let lba = 0xFFFF_FFF0u64; // crosses 2^32
    assert_eq!(read(&d, lba, 64).unwrap(), expect(lba, 64, 512));
    assert_eq!(read(&d, 0x1_7FFF_FFFF, 1).unwrap(), expect(0x1_7FFF_FFFF, 1, 512), "the last block");
    assert_eq!(read(&d, 0x1_7FFF_FFFF, 2), Err(DevError::InvalidArgument), "past the end: refused before sending");
    let ops = dev.opcodes();
    assert!(ops.contains(&0x88) && !ops.contains(&0x28), "READ(16) only");
    // A small disk: READ(10).
    let (dev, d) = setup(Lun::new(512, 4096));
    read(&d, 0, 8).unwrap();
    assert!(dev.opcodes().contains(&0x28) && !dev.opcodes().contains(&0x9E), "no (16) for an old-style stick");
}

#[test]
fn unusable_block_sizes_are_kept_without_medium() {
    let (_dev, d) = setup(Lun::new(520, 1000));
    assert!(!d.unit().medium);
    assert_eq!(read(&d, 0, 1), Err(DevError::Gone));
}

// ---- splitting and chaining

#[test]
fn the_smi_quirk_keeps_every_command_at_32_kib() {
    for bs in [512usize, 4096] {
        let dev = SimDev::new(vec![Lun::new(bs, 1 << 16)]);
        dev.with(|s| s.max_xfer = Some(32 * 1024));
        let q = crate::quirks::lookup(0x090C, 0x1000, 0x1100);
        let max = crate::quirks::max_bytes(usb_core::Speed::High, q);
        let s = Arc::new(Storage::new(SimLink(dev.clone()), max, false, false));
        let d = s.scan().unwrap().remove(0);
        let n = (1 << 20) / bs; // 1 MiB
        assert_eq!(read(&d, 3, n).unwrap(), expect(3, n as u64, bs));
        let reads: Vec<u32> = dev.with(|s| s.cdbs.iter().filter(|c| c[0] == 0x28).map(|c| u16::from_be_bytes([c[7], c[8]]) as u32).collect());
        assert_eq!(reads.len(), 32, "{bs}: 1 MiB in 32 KiB commands");
        assert!(reads.iter().all(|&k| k as usize * bs == 32 * 1024));
        assert_eq!(dev.log_has("too big"), 0);
        assert_eq!(d.info().max_blocks_per_request as usize, 32 * 1024 / bs);
    }
}

#[test]
fn a_large_read_runs_in_chains_of_a_mebibyte() {
    let (dev, _s, d) = setup_with(vec![Lun::new(512, 1 << 16)], 120 * 1024, false);
    let runs0 = dev.with(|s| s.runs);
    let n = (4 << 20) / 512; // 4 MiB
    assert_eq!(read(&d[0], 0, n).unwrap(), expect(0, n as u64, 512));
    let runs = dev.with(|s| s.runs) - runs0;
    assert_eq!(runs, 4, "one run of chained commands per MiB");
    // An odd length: whole commands of 240 blocks and a short last one.
    let before = dev.with(|s| s.cdbs.len());
    assert_eq!(read(&d[0], 1, 500).unwrap(), expect(1, 500, 512));
    let lens: Vec<u16> = dev.with(|s| s.cdbs[before..].iter().map(|c| u16::from_be_bytes([c[7], c[8]])).collect());
    assert_eq!(lens, vec![240, 240, 20]);
}

#[test]
fn small_dma_memory_still_reads_one_command_at_a_time() {
    let (dev, _s, d) = setup_with(vec![Lun::new(512, 1 << 16)], 64 * 1024, false);
    dev.with(|s| s.alloc_limit = Some(64 * 1024));
    let n = (1 << 20) / 512;
    assert_eq!(read(&d[0], 0, n).unwrap(), expect(0, n as u64, 512));
}

// ---- Bulk-Only error paths

fn recovered(dev: &SimDev) -> bool {
    dev.with(|s| {
        let i = s.log.iter().rposition(|l| l == "reset");
        i.is_some_and(|i| s.log.get(i + 1).map(|x| x.as_str()) == Some("clear in") && s.log.get(i + 2).map(|x| x.as_str()) == Some("clear out"))
    })
}

#[test]
fn a_stalled_data_phase_keeps_the_data_before_the_stall() {
    let (dev, d) = setup(Lun::new(512, 4096));
    dev.fault(Fault::StallData);
    assert_eq!(read(&d, 10, 64).unwrap(), expect(10, 64, 512));
    assert!(dev.log_has("clear in") >= 1, "the stalled IN endpoint was cleared");
    assert_eq!(dev.log_has("reset"), 0, "a stall is no transport error");
    // The command went again only for the half that did not come.
    let lens: Vec<(u32, u16)> = dev.with(|s| s.cdbs.iter().filter(|c| c[0] == 0x28).map(|c| (u32::from_be_bytes([c[2], c[3], c[4], c[5]]), u16::from_be_bytes([c[7], c[8]]))).collect());
    assert_eq!(lens[lens.len() - 2..], [(10, 64), (42, 32)]);
}

#[test]
fn stalled_and_empty_csws_are_read_again() {
    let (dev, d) = setup(Lun::new(512, 4096));
    dev.fault(Fault::ZeroLenCsw);
    assert_eq!(read(&d, 0, 8).unwrap(), expect(0, 8, 512));
    dev.fault(Fault::StallCsw);
    assert_eq!(read(&d, 8, 8).unwrap(), expect(8, 8, 512));
    dev.fault(Fault::ZeroLenCsw);
    dev.fault(Fault::StallCsw);
    assert_eq!(read(&d, 16, 8).unwrap(), expect(16, 8, 512), "both second chances in one status phase");
    assert_eq!(dev.log_has("reset"), 0);
    let st = d.storage().with_io(P, |io| io.bot.stats).unwrap();
    assert_eq!(st.csw_retries, 4);
}

#[test]
fn a_csw_in_the_data_phase_is_taken_as_the_status() {
    let (dev, d) = setup(Lun::new(512, 4096));
    dev.fault(Fault::CswInData);
    // The device skipped the data: the read passes with nothing moved and is sent again.
    assert_eq!(read(&d, 4, 4).unwrap(), expect(4, 4, 512));
    let st = d.storage().with_io(P, |io| io.bot.stats).unwrap();
    assert_eq!(st.skipped_data, 1);
    assert_eq!(dev.log_has("reset"), 0);
}

#[test]
fn phase_errors_wrong_tags_and_timeouts_get_reset_recovery() {
    for f in [Fault::PhaseError, Fault::WrongTag, Fault::Timeout(Stage::Csw), Fault::Timeout(Stage::Data), Fault::Timeout(Stage::Cbw), Fault::StallCbw] {
        let (dev, d) = setup(Lun::new(512, 4096));
        dev.fault(f);
        assert_eq!(read(&d, 100, 16).unwrap(), expect(100, 16, 512), "{f:?}");
        assert!(dev.log_has("reset") >= 1 && recovered(&dev), "{f:?}: Bulk-Only reset, then both halts cleared");
        assert!(dev.with(|s| s.log.iter().rposition(|l| l == "abort") < s.log.iter().rposition(|l| l == "reset")), "aborted first");
        let st = d.storage().with_io(P, |io| io.bot.stats).unwrap();
        assert!(st.transport_errors >= 1 && st.resets >= 1, "{f:?}");
        assert!(!d.storage().with_io(P, |io| io.bot.needs_reset()).unwrap());
    }
}

#[test]
fn reset_recovery_that_keeps_failing_ends_the_request_and_is_tried_again_later() {
    let (dev, d) = setup(Lun::new(512, 4096));
    dev.with(|s| s.reset_failures_left = 100);
    dev.fault(Fault::PhaseError);
    assert_eq!(read(&d, 0, 8), Err(DevError::Io));
    assert!(d.storage().with_io(P, |io| io.bot.needs_reset()).unwrap());
    dev.with(|s| s.reset_failures_left = 0);
    assert_eq!(read(&d, 0, 8).unwrap(), expect(0, 8, 512), "the due reset runs before the next command");
    assert!(recovered(&dev));
}

#[test]
fn bogus_residues_are_learnt_from_inquiry_and_then_ignored() {
    let dev = SimDev::new(vec![Lun::new(512, 4096)]);
    dev.fault(Fault::BogusResidue(36)); // on the INQUIRY that passes whole
    let s = Arc::new(Storage::new(SimLink(dev.clone()), 65536, false, false));
    let d = s.scan().unwrap().remove(0);
    assert!(s.with_io(P, |io| io.bot.ignore_residue).unwrap());
    dev.fault(Fault::BogusResidue(512));
    assert_eq!(read(&d, 0, 8).unwrap(), expect(0, 8, 512));
    // Without the flag a residue on a read makes it go again for the rest.
    let (dev, d) = setup(Lun::new(512, 4096));
    dev.fault(Fault::BogusResidue(512));
    assert_eq!(read(&d, 0, 8).unwrap(), expect(0, 8, 512));
    assert_eq!(dev.with(|s| s.cdbs.iter().filter(|c| c[0] == 0x28).count()), 2);
}

#[test]
fn a_device_short_on_every_read_still_finishes_while_it_makes_progress() {
    // 64 KiB per READ with a passed CSW and the rest as residue: every run's first command is short, so each
    // run moves the request on by what `keep` takes. A long read must not fail after `ATTEMPTS` such runs.
    let (dev, d) = setup(Lun::new(512, 8192));
    dev.with(|s| s.short_reads = Some(64 * 1024));
    assert_eq!(read(&d, 0, 2048).unwrap(), expect(0, 2048, 512), "1 MiB in 64 KiB steps");
    // No progress at all (nothing moves) still ends after the bound.
    dev.with(|s| s.short_reads = Some(0));
    assert_eq!(read(&d, 0, 8), Err(DevError::Io));
}

#[test]
fn unplugged_in_any_stage_ends_with_gone_and_no_reset() {
    for stage in [Stage::Cbw, Stage::Data, Stage::Csw] {
        let (dev, d) = setup(Lun::new(512, 4096));
        dev.fault(Fault::GoneAt(stage));
        assert_eq!(read(&d, 0, 64), Err(DevError::Gone), "{stage:?}");
        assert_eq!(dev.log_has("reset"), 0, "{stage:?}: no reset of a gone device");
        assert!(d.is_gone());
        assert_eq!(read(&d, 0, 1), Err(DevError::Gone));
        assert_eq!(d.flush(), Err(DevError::Gone));
    }
    // Gone before a request: refused at once; a write on an unplugged device too.
    let (dev, _s, d) = setup_with(vec![Lun::new(512, 4096)], 65536, true);
    dev.with(|s| s.gone = true);
    assert_eq!(d[0].write_blocks(0, &[0; 512], false, P), Err(DevError::Gone));
}

// ---- sense handling

#[test]
fn unit_attention_is_retried_and_a_medium_change_fails_removable_requests() {
    let (dev, d) = setup(Lun::new(512, 4096));
    dev.fault(Fault::Check(Sense { key: 6, asc: 0x29, ascq: 0 }));
    assert_eq!(read(&d, 0, 1).unwrap(), expect(0, 1, 512), "power-on UA: retried");
    let g = d.generation();
    dev.with(|s| s.luns[0].blocks = 8192);
    dev.fault(Fault::Check(Sense { key: 6, asc: 0x28, ascq: 0 }));
    assert_eq!(read(&d, 0, 1), Err(DevError::Io), "removable medium changed: the request fails (Linux)");
    assert_eq!(d.generation(), g + 1);
    assert_eq!(d.info().blocks, 8192, "the size was read again");
    assert_eq!(read(&d, 8000, 1).unwrap(), expect(8000, 1, 512));
    // Not removable: retried.
    let mut l = Lun::new(512, 4096);
    l.removable = false;
    let (dev, d) = setup(l);
    dev.fault(Fault::Check(Sense { key: 6, asc: 0x28, ascq: 0 }));
    assert_eq!(read(&d, 0, 1).unwrap(), expect(0, 1, 512));
    assert_eq!(d.generation(), 1);
}

#[test]
fn not_ready_medium_errors_illegal_requests_and_lost_media() {
    let (dev, d) = setup(Lun::new(512, 4096));
    dev.fault(Fault::Check(Sense { key: 2, asc: 0x04, ascq: 0x01 }));
    assert_eq!(read(&d, 0, 1).unwrap(), expect(0, 1, 512), "becoming ready: waited and retried");
    dev.fault(Fault::Check(Sense { key: 3, asc: 0x11, ascq: 0 }));
    assert_eq!(read(&d, 0, 1), Err(DevError::Io), "unrecovered read error");
    dev.fault(Fault::Check(Sense { key: 5, asc: 0x24, ascq: 0 }));
    assert_eq!(read(&d, 0, 1), Err(DevError::InvalidArgument));
    dev.fault(Fault::Check(Sense { key: 1, asc: 0x17, ascq: 0 }));
    assert_eq!(read(&d, 3, 1).unwrap(), expect(3, 1, 512), "recovered error: the data counts");
    // Three UNIT ATTENTIONs in a row (each try uses one): the request gives up with Io.
    for _ in 0..3 {
        dev.fault(Fault::Check(Sense { key: 0xB, asc: 0, ascq: 0 }));
    }
    assert_eq!(read(&d, 0, 1), Err(DevError::Io));
    dev.with(|s| s.luns[0].medium = false);
    assert_eq!(read(&d, 0, 1), Err(DevError::Gone), "the medium was taken out");
    assert!(!d.unit().medium);
}

// ---- writes, R8, FUA, flush, discard

#[test]
fn writes_are_refused_without_the_switch_and_nothing_is_sent() {
    let (dev, d) = setup(Lun::new(512, 4096));
    assert!(d.info().read_only && !d.writable());
    assert_eq!(d.write_blocks(0, &[0xEE; 512], false, P), Err(DevError::Refused));
    assert_eq!(d.discard(0, 8), Err(DevError::Refused));
    assert_eq!(d.flush(), Ok(()));
    assert!(dev.with(|s| s.cdbs.iter().all(|c| ![0x2A, 0x8A, 0x35, 0x42].contains(&c[0]))), "no write, flush or unmap went out");
}

#[test]
fn writes_with_the_switch_fua_and_flush() {
    let (dev, _s, d) = setup_with(vec![Lun::new(512, 4096)], 120 * 1024, true);
    let d = &d[0];
    assert!(!d.info().read_only && d.writable());
    let data: Vec<u8> = (0..300 * 512).map(|i| (i * 13 % 251) as u8).collect();
    d.write_blocks(7, &data, false, P).unwrap();
    assert_eq!(read(d, 7, 300).unwrap(), data);
    assert_eq!(read(d, 6, 1).unwrap(), expect(6, 1, 512), "the block before untouched");
    // FUA without DPOFUA: write, then SYNCHRONIZE CACHE.
    let n35 = || dev.with(|s| s.cdbs.iter().filter(|c| c[0] == 0x35).count());
    d.write_blocks(1, &[1; 512], true, P).unwrap();
    assert_eq!(n35(), 1);
    assert!(dev.with(|s| s.log.iter().any(|l| l == "write 1 1 fua false")));
    d.flush().unwrap();
    assert_eq!(n35(), 2);
    // With DPOFUA: the FUA bit, no flush.
    let mut l = Lun::new(512, 4096);
    l.dpofua = true;
    let (dev, _s, d) = setup_with(vec![l], 120 * 1024, true);
    d[0].write_blocks(2, &[2; 1024], true, P).unwrap();
    assert!(dev.with(|s| s.log.iter().any(|l| l == "write 2 2 fua true")));
    assert_eq!(dev.with(|s| s.cdbs.iter().filter(|c| c[0] == 0x35).count()), 0);
    // SYNCHRONIZE CACHE not supported: remembered, flush is then a no-op.
    let mut l = Lun::new(512, 4096);
    l.sync_supported = false;
    let (dev, _s, d) = setup_with(vec![l], 120 * 1024, true);
    d[0].flush().unwrap();
    d[0].flush().unwrap();
    assert_eq!(dev.with(|s| s.cdbs.iter().filter(|c| c[0] == 0x35).count()), 1);
    assert!(!d[0].info().has_volatile_cache);
}

#[test]
fn write_protection_from_mode_sense_and_from_data_protect() {
    let mut l = Lun::new(512, 4096);
    l.write_protected = true;
    let (dev, _s, d) = setup_with(vec![l], 65536, true);
    assert!(d[0].info().read_only);
    assert_eq!(d[0].write_blocks(0, &[0; 512], false, P), Err(DevError::Refused));
    // The switch moved after attach: DATA PROTECT on the write marks the unit.
    let (dev2, _s, d2) = setup_with(vec![Lun::new(512, 4096)], 65536, true);
    dev2.with(|s| s.luns[0].write_protected = true);
    assert_eq!(d2[0].write_blocks(0, &[0; 512], false, P), Err(DevError::Refused));
    assert!(d2[0].info().read_only && !d2[0].writable());
    let _ = dev;
}

#[test]
fn discard_where_the_device_offers_unmap() {
    let mut l = Lun::new(512, 1 << 20);
    l.version = 6;
    l.lbpme = true;
    l.unmap = Some((1000, 2));
    let (dev, _s, d) = setup_with(vec![l], 120 * 1024, true);
    let d = &d[0];
    assert!(d.info().supports_discard);
    d.discard(10, 4500).unwrap();
    let unmaps: Vec<String> = dev.with(|s| s.log.iter().filter(|l| l.starts_with("unmap")).cloned().collect());
    assert_eq!(unmaps, ["unmap 10 1000", "unmap 1010 1000", "unmap 2010 1000", "unmap 3010 1000", "unmap 4010 500"]);
    assert_eq!(dev.with(|s| s.cdbs.iter().filter(|c| c[0] == 0x42).count()), 3, "two descriptors per command");
    assert_eq!(d.discard(1 << 20, 1), Err(DevError::InvalidArgument));
    // SPC-3 without provisioning: no discard, ignored.
    let mut l = Lun::new(512, 4096);
    l.version = 5;
    let (dev, _s, d) = setup_with(vec![l], 120 * 1024, true);
    assert!(!d[0].info().supports_discard);
    assert_eq!(d[0].discard(0, 8), Ok(()));
    assert_eq!(dev.with(|s| s.cdbs.iter().filter(|c| c[0] == 0x42).count()), 0);
    assert!(dev.opcodes().contains(&0x9E), "SPC-3 is asked READ CAPACITY(16)");
}

/// A device allowing "any number" of descriptors: one UNMAP carries at most 4095 (65528 bytes), so the
/// 16-bit lengths never wrap (4096 descriptors would make 65544 bytes, sent as 8).
#[test]
fn discard_with_unlimited_descriptors_keeps_lengths_in_16_bits() {
    let mut l = Lun::new(512, 1 << 20);
    l.version = 6;
    l.lbpme = true;
    l.unmap = Some((1, u32::MAX));
    let (dev, _s, d) = setup_with(vec![l], 120 * 1024, true);
    let d = &d[0];
    d.discard(0, 4097).unwrap();
    let lens: Vec<u16> =
        dev.with(|s| s.cdbs.iter().filter(|c| c[0] == 0x42).map(|c| u16::from_be_bytes([c[7], c[8]])).collect());
    assert_eq!(lens, [8 + 16 * 4095, 8 + 16 * 2]);
    let unmaps: Vec<String> = dev.with(|s| s.log.iter().filter(|l| l.starts_with("unmap")).cloned().collect());
    assert_eq!(unmaps.len(), 4097, "every block reached the device");
    assert_eq!(unmaps[4095], "unmap 4095 1");
    assert_eq!(unmaps[4096], "unmap 4096 1");
}

#[test]
fn bad_request_shapes_are_refused() {
    let (_dev, d) = setup(Lun::new(4096, 100));
    let mut b = vec![0u8; 512];
    assert_eq!(d.read_blocks(0, &mut b, P), Err(DevError::InvalidArgument), "not a whole block");
    assert_eq!(d.read_blocks(u64::MAX, &mut vec![0u8; 4096], P), Err(DevError::InvalidArgument), "no overflow");
    assert_eq!(d.read_blocks(0, &mut [], P), Ok(()));
}

#[test]
fn buffers_are_kept_between_requests_and_dropped_after_a_given_up_run() {
    let (dev, d) = setup(Lun::new(512, 4096));
    read(&d, 0, 64).unwrap();
    let a0 = dev.with(|s| s.allocs);
    read(&d, 64, 64).unwrap();
    assert_eq!(dev.with(|s| s.allocs), a0, "the data buffer is reused");
    dev.with(|s| s.poisoned = true);
    assert_eq!(read(&d, 128, 64).unwrap(), expect(128, 64, 512));
    assert!(dev.with(|s| s.allocs) > a0, "fresh buffers after a given-up run");
}

// ---- the priority gate (B4)

#[test]
fn the_gate_serves_higher_priorities_first() {
    use std::sync::Mutex as StdMutex;
    use std::thread;
    let gate = Arc::new(Gate::new());
    let order = Arc::new(StdMutex::new(Vec::new()));
    let held = gate.acquire(IoPriority::Normal, 1_000_000_000).unwrap();
    let mut hs = Vec::new();
    for (name, p) in [("bg", IoPriority::Background), ("ia", IoPriority::Interactive)] {
        let (g, o) = (gate.clone(), order.clone());
        hs.push(thread::spawn(move || {
            let _x = g.acquire(p, 10_000_000_000).unwrap();
            o.lock().unwrap().push(name);
            std::thread::sleep(std::time::Duration::from_millis(5));
        }));
        // Both wait before the holder lets go.
        while gate.state().1[match p { IoPriority::Interactive => 0, IoPriority::Normal => 1, IoPriority::Background => 2 }] == 0 {
            thread::yield_now();
        }
    }
    drop(held);
    for h in hs {
        h.join().unwrap();
    }
    assert_eq!(*order.lock().unwrap(), ["ia", "bg"]);
    assert_eq!(gate.state(), (false, [0, 0, 0]));
}

// ---- robustness: devices that answer garbage

struct Chaos {
    dev: Arc<SimDev>,
    seed: core::cell::Cell<u64>,
}

impl Chaos {
    fn next(&self) -> u64 {
        let x = self.seed.get().wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        self.seed.set(x);
        x >> 33
    }
}

#[test]
fn random_faults_never_panic_and_never_return_wrong_data() {
    let faults = [
        Fault::StallData, Fault::StallCsw, Fault::ZeroLenCsw, Fault::CswInData, Fault::PhaseError, Fault::WrongTag,
        Fault::BogusResidue(7), Fault::Timeout(Stage::Csw), Fault::Timeout(Stage::Data), Fault::StallCbw,
        Fault::Check(Sense { key: 6, asc: 0x29, ascq: 0 }), Fault::Check(Sense { key: 0xB, asc: 0, ascq: 0 }),
        Fault::Check(Sense { key: 2, asc: 4, ascq: 1 }),
    ];
    let c = Chaos { dev: SimDev::new(vec![Lun::new(512, 1 << 16)]), seed: core::cell::Cell::new(42) };
    let s = Arc::new(Storage::new(SimLink(c.dev.clone()), 64 * 1024, true, false));
    let d = s.scan().unwrap().remove(0);
    let (mut ok, mut err) = (0, 0);
    for round in 0..300u64 {
        if c.next().is_multiple_of(3) {
            c.dev.fault(faults[(c.next() % faults.len() as u64) as usize]);
        }
        let lba = c.next() % ((1 << 16) - 300);
        let n = 1 + (c.next() % 280) as usize;
        if round % 5 == 0 {
            let data: Vec<u8> = (0..n * 512).map(|i| (i as u64 ^ round) as u8).collect();
            match d.write_blocks(lba, &data, round % 2 == 0, P) {
                Ok(()) => {
                    c.dev.with(|s| s.faults.clear());
                    assert_eq!(read(&d, lba, n).unwrap(), data, "round {round}");
                    ok += 1;
                }
                Err(e) => {
                    assert!(matches!(e, DevError::Io), "round {round}: {e:?}");
                    err += 1;
                }
            }
        } else {
            match read(&d, lba, n) {
                Ok(v) => {
                    let want: Vec<u8> = (lba..lba + n as u64).flat_map(|b| c.dev.with(|s| s.luns[0].block(b))).collect();
                    assert_eq!(v, want, "round {round}");
                    ok += 1;
                }
                Err(e) => {
                    assert!(matches!(e, DevError::Io), "round {round}: {e:?}");
                    err += 1;
                }
            }
        }
        c.dev.with(|s| s.faults.clear());
    }
    assert!(ok > 250, "{ok} ok, {err} failed");
}

// ---- the link's default run stops at the first unclean command

#[test]
fn a_run_stops_after_the_first_command_that_is_not_clean() {
    let dev = SimDev::new(vec![Lun::new(512, 4096)]);
    dev.with(|s| s.luns[0].pending.clear());
    let link = SimLink(dev.clone());
    let buf = link.alloc(3 * 4096).unwrap();
    let mut bot = Bot::new(link);
    let ops: Vec<Op> = (0..3).map(|i| Op::read(0, scsi::rw(false, i * 8, 8, false, false), Request::new(buf.clone(), i as usize * 4096, 4096))).collect();
    dev.fault(Fault::Check(Sense { key: 0, asc: 0, ascq: 0 }));
    let r = bot.run(&ops);
    assert_eq!(r.len(), 1, "the first command failed: the run stopped");
    assert!(matches!(r[0], Ok(o) if !o.passed));
    let r = bot.run(&ops);
    assert_eq!(r.len(), 3);
    assert!(r.iter().all(|x| matches!(x, Ok(o) if o.passed && o.residue == 0 && o.moved == 4096)));
    // `clean` itself.
    let c = Cmd { cbw: bot::cbw(9, 0, Dir::None, 0, &scsi::test_unit_ready()), dir: Dir::None, data: None, data_timeout_ns: 1, residue_ignored: false };
    let mut csw = [0u8; 13];
    csw[0..4].copy_from_slice(b"USBS");
    csw[4] = 9;
    let ran = Ran { cbw: XferResult::ok(31), data: None, csw: Some((XferResult::ok(13), csw)) };
    assert!(bot::clean(&c, &ran));
    csw[8] = 1;
    let ran2 = Ran { csw: Some((XferResult::ok(13), csw)), ..ran };
    assert!(!bot::clean(&c, &ran2), "a residue is not clean");
    assert!(bot::clean(&Cmd { residue_ignored: true, ..c.clone() }, &ran2), "unless residues are ignored");
    assert!(!bot::clean(&c, &Ran { cbw: XferResult::ok(30), ..ran }));
    assert_eq!(BotError::Transport("x").as_str(), "x");
}
