//! Host tests of the disk copy's pure parts: the slot format, the ring's next slot, the extent check and the
//! mapping of a slot onto the disk, the desktop's line.

use super::extents::{self, FileFacts, MAX_RUNS};
use super::format::{self, Head, Seen, SlotError};
use super::show::{self, Rec, Source};
use super::Run;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

#[test]
fn slot_round_trip_and_damage() {
    let h = Head { kind: format::KIND_RECORD, slot: 1, seq: 42, flags: format::flags::EXCEPTION, boot_id: [7; 16], time_ns: 9, wall_s: -1, generation: 150, reload: 2, cpu: 5, cpus_asked: 15, cpus_answered: 14, tag: 99, ..Head::default() };
    let body: Vec<u8> = (0..200_000u32).map(|i| b"abcdefgh\n"[(i % 9) as usize]).collect();
    let s = format::encode(&h, &body).unwrap();
    let (g, b) = format::decode(&s).unwrap();
    assert_eq!((g.seq, g.slot, g.generation, g.cpus_answered, g.wall_s, g.tag, b.len()), (42, 1, 150, 14, -1, 99, body.len()));
    // Every header field and the body are under the checksum.
    for at in [12usize, 16, 30, 40, 60, 80, 119, 128, 128 + 100_000, s.len() - 1] {
        let mut d = s.clone();
        d[at] ^= 0x40;
        assert!(format::decode(&d).is_err(), "byte {at}");
    }
    let mut d = s.clone();
    d[8] = 9;
    assert_eq!(format::decode(&d), Err(SlotError::UnknownVersion(9)));
    let mut d = s.clone();
    d[24..28].copy_from_slice(&(format::body_capacity() as u32 + 1).to_le_bytes());
    assert_eq!(format::decode(&d), Err(SlotError::BadLength));
    assert!(format::encode(&h, &vec![0; format::body_capacity() + 1]).is_none());
    assert_eq!(format::decode(&[0u8; 512]), Err(SlotError::NoMagic));
    // A torn write: the header of the new record over an older body.
    let mut torn = s.clone();
    torn[200] = b'X';
    assert_eq!(format::decode(&torn), Err(SlotError::BadChecksum));
}

#[test]
fn ring_order() {
    let e = Seen::Empty { tag: 1 };
    assert_eq!(format::next_slot(&[e, e, e, e]), (0, 1));
    assert_eq!(format::next_slot(&[Seen::Record { seq: 1 }, e, e, e]), (1, 2));
    assert_eq!(format::next_slot(&[Seen::Record { seq: 5 }, Seen::Record { seq: 6 }, Seen::Record { seq: 3 }, Seen::Record { seq: 4 }]), (2, 7));
    assert_eq!(format::next_slot(&[Seen::Record { seq: 1 }, Seen::Record { seq: 2 }, Seen::Record { seq: 3 }, Seen::Record { seq: 4 }]), (0, 5));
    assert_eq!(format::next_slot(&[Seen::Damaged, Seen::Unprepared, Seen::Record { seq: 8 }, e]), (3, 9));
    let empty = Head { kind: format::KIND_EMPTY, slot: 3, tag: format::prep_tag(&[1; 16], 3), ..Head::default() };
    let b = format::encode(&empty, &[]).unwrap();
    assert_eq!(format::seen(&b, 3), Seen::Empty { tag: format::prep_tag(&[1; 16], 3) });
    assert_eq!(format::seen(&b, 2), Seen::Damaged, "a slot header naming another slot");
    assert_ne!(format::prep_tag(&[1; 16], 3), format::prep_tag(&[1; 16], 2));
}

fn facts(runs: &[(u64, u64)]) -> FileFacts<'_> {
    FileFacts { runs, size: (format::SLOTS * format::SLOT_BYTES) as u64, bps: 512, data_start: 16_384, vol_total: 500_000, part: (559_104, 524_288), vol_offset: 0, foreign: 0 }
}

#[test]
fn extents_checked_and_mapped() {
    let want = (format::SLOTS * format::SLOT_BYTES) as u64;
    let allowed = (559_104, 524_288);
    let runs = [(20_000u64, 700u64), (30_000, 1348)];
    let ext = extents::resolve(&facts(&runs), 512, allowed, want).unwrap();
    assert_eq!(ext.runs(), &[(579_104, 700), (589_104, 1348)]);
    // Slot 1 (byte 256 KiB = block 512) is in the first run until block 700, then the second.
    let mut out = [Run::default(); 8];
    let n = ext.map(256 << 10, 256 << 10, 0, &mut out).unwrap();
    assert_eq!(&out[..n], &[Run { lba: 579_104 + 512, off: 0, blocks: 188 }, Run { lba: 589_104, off: 188 * 512, blocks: 324 }]);
    for r in &out[..n] {
        assert!(ext.holds(r.lba, r.blocks as u64));
    }
    assert!(!ext.holds(579_104 + 699, 2), "across the gap between runs");
    assert_eq!(ext.lba_of(3 * (256 << 10)), Some(589_104 + 3 * 512 - 700));
    assert!(ext.map(want, 512, 0, &mut out).is_none() && ext.map(10, 512, 0, &mut out).is_none());
    assert!(ext.map(0, want, 0, &mut out[..1]).is_none(), "more pieces than room");
    // The rules.
    let r = |f: FileFacts<'_>, bs, allowed| extents::resolve(&f, bs, allowed, want);
    assert!(r(facts(&runs), 4096, allowed).is_err());
    assert!(r(facts(&runs), 512, (559_104, 524_287)).is_err());
    assert!(r(facts(&runs), 512, (2048, 524_288)).is_err());
    assert!(r(facts(&[(10, 2048)]), 512, allowed).is_err(), "in the FATs");
    assert!(r(facts(&[(499_000, 2048)]), 512, allowed).is_err(), "past the volume");
    assert!(r(facts(&[(20_000, 1024), (20_500, 1024)]), 512, allowed).is_err(), "overlap");
    assert!(r(facts(&[(20_000, 2047)]), 512, allowed).is_err(), "short");
    assert!(r(FileFacts { size: want - 512, ..facts(&runs) }, 512, allowed).is_err(), "size");
    let many: Vec<(u64, u64)> = (0..MAX_RUNS as u64 + 1).map(|i| (20_000 + i * 100, 32)).collect();
    assert!(r(facts(&many), 512, allowed).is_err(), "too many pieces");
    assert!(r(FileFacts { part: (559_104, 400_000), ..facts(&runs) }, 512, (559_104, 400_000)).is_err(), "volume past its partition");
    assert_eq!(r(FileFacts { foreign: 1, ..facts(&runs) }, 512, allowed).err(), Some("another file or folder shares the area's clusters (a cross-linked FAT)"));
}

#[test]
fn desktop_line() {
    let tz = time::tz::EUROPE_BERLIN;
    let rec = |source, wall_s, flags| Rec { source, head: Head { wall_s, flags, ..Head::default() }, text: String::from("panic on cpu 0"), reported_before: false };
    // 13.11.2026 17:49 local (Berlin, CET: UTC+1) = 16:49 UTC.
    let at = 1_794_588_540;
    let now = at + 3600;
    let t = show::notice_text(&rec(Source::Disk { slot: 0 }, at, 0), now, tz);
    assert_eq!(t, "BitsOS ist um 17:49 abgestürzt. Der Bericht ist gesichert. Zum Lesen „absturz“ in die Adresszeile tippen.");
    let t = show::notice_text(&rec(Source::Ram, at, format::flags::HANG), now + 86_400, tz);
    assert!(t.starts_with("BitsOS ist am 13.11. um 17:49 hängen geblieben") && t.contains("(nur im Speicher)"), "{t}");
    let t = show::notice_text(&rec(Source::Both { slot: 1 }, 0, 0), now, tz);
    assert!(t.starts_with("BitsOS ist beim letzten Mal abgestürzt."), "{t}");
}

#[test]
fn a_volume_inside_its_partition() {
    let runs = [(100u64, 2048u64)];
    let f = FileFacts { runs: &runs, size: (format::SLOTS * format::SLOT_BYTES) as u64, bps: 512, data_start: 50, vol_total: 10_000, part: (5000, 20_000), vol_offset: 2048, foreign: 0 };
    let ext = extents::resolve(&f, 512, (5000, 20_000), f.size).unwrap();
    assert_eq!(ext.runs(), &[(5000 + 2048 + 100, 2048)]);
    assert!(extents::resolve(&FileFacts { vol_offset: 15_000, ..f.clone() }, 512, (5000, 20_000), f.size).is_err(), "volume past its partition");
}

#[test]
fn disk_line_says_when_the_writer_refuses() {
    let armed = || super::State::Armed { disk: String::from("nvme0n1"), slot: 1, seq: 2, runs: 1, first_lba: 600_000 };
    let line = |r| crate::crash::tool::line_of(armed(), r);
    assert!(line(Some(Ok(()))).starts_with("disk copy: armed on nvme0n1, \\BITSOS2\\CRASH\\SLOTS.BIN, the next panic writes slot 1 as number 2"));
    let off = line(Some(Err("the boot partition's window closed or changed since boot")));
    assert_eq!(off, "disk copy: armed on nvme0n1 at boot, now RAM only: the boot partition's window closed or changed since boot\n");
    assert!(line(None).contains("now RAM only"));
    assert!(crate::crash::tool::line_of(super::State::Off(String::from("x")), None).starts_with("disk copy: off, RAM only: x"));
}

#[test]
fn the_hang_mark_belongs_to_its_cpu() {
    use crate::crash::again;
    // The reporter's CPU 5 marks its hang; CPU 2's ordinary panic that wins the report is not a hang's.
    again::mark_hang(5);
    assert!(again::hang(5));
    assert!(!again::hang(2) && !again::hang(0));
    again::mark_hang(2);
    assert!(again::hang(5) && !again::hang(2), "the first mark stays");
}

#[test]
fn new_without_the_reported_flag() {
    let (me, other) = ([1u8; 16], [2u8; 16]);
    let crash = 1_794_588_540;
    // A warm reset: the RAM marker names the record's boot.
    assert!(show::new_unmarked(&me, crash, Some(&me), None));
    // A cold reset soon after: new; the same record at a boot an hour later: old, so it is not shown at every boot.
    assert!(show::new_unmarked(&me, crash, None, Some(crash + 45)));
    assert!(show::new_unmarked(&me, crash, Some(&other), Some(crash + show::UNMARKED_NEW_S)));
    assert!(!show::new_unmarked(&me, crash, Some(&other), Some(crash + 3600)));
    assert!(!show::new_unmarked(&me, crash, None, None), "no clock, no marker: old");
    assert!(!show::new_unmarked(&me, 0, None, Some(crash)), "no wall time in the record");
    assert!(!show::new_unmarked(&me, crash + 600, None, Some(crash)), "a record from after this boot began");
}
