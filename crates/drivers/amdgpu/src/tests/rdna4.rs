//! GP-GPU1: the RX 9070 XT attached read-only, against a card built from the table measured on the Gaming-PC
//! (`rdna4_core::ipfacts`, sysfs of both cards on 5.11.): the modules its versions resolve to, its firmware set and
//! the stick's list, the attach step by step with every stop, and the proof that nothing is written.

use super::devs::{key_a, key_b};
use crate::devs::{Device, Family};
use crate::gpucore::disc::parse;
use crate::gpucore::fwload::{FwSource, LoadError};
use crate::gpucore::psp::Hw;
use crate::ipmod::{self, Choice, Role, State};
use crate::rdna4::attach::{self, BarFact, Link, PciFacts, Stop};
use crate::rdna4::fakecard::{FakeCard, blob};
use crate::rdna4::{Card, fw, policy, snapshot};
use rdna4_core::card::{CARD_A, CARD_B};
use rdna4_core::ipfacts::{self, TABLE};
use rdna4_core::attach::{MailboxRefusal, Source};
use rdna4_core::layout::{BlobRefusal, DISCOVERY_READ, MemsizeError};
use std::collections::BTreeMap;
use std::format;
use std::string::String;
use std::vec::Vec;

/// Where the PSP's runtime database lies on the measured card: 1 MiB below the end of its 16304 MiB.
const DB: u64 = 0x3_FAF0_0000;

/// A card's configuration space as lspci showed it on 5.11. (plan/spec/gpu-rdna4.md section 1).
pub fn facts(c: &rdna4_core::card::Card) -> PciFacts {
    let a = c.at.bus == CARD_A.at.bus;
    let x8 = Link { speed: 5, width: 8, max_speed: 5, max_width: 8 };
    PciFacts {
        key: if a { key_a() } else { key_b() },
        vendor: 0x1002,
        device: 0x7550,
        revision: 0xC0,
        subsystem: (0x148c, 0x2435),
        class: 0x03_00_00,
        command: 0x0006,
        power_state: Some(0),
        bar0: Some(BarFact { phys: c.bar0_base, bytes: c.bar0_bytes, is64: true, prefetch: true }),
        bar2: Some(BarFact { phys: c.bar0_base + (16 << 30), bytes: 256 << 20, is64: true, prefetch: true }),
        bar5: Some(BarFact { phys: if a { 0x9120_0000 } else { 0x9100_0000 }, bytes: 512 << 10, is64: false, prefetch: false }),
        link: Some(Link { speed: 5, width: 16, max_speed: 5, max_width: 16 }),
        upstream: Some((crate::devs::Key::new(0, 0, 6, if a { 0 } else { 3 }), x8)),
        // The one card of 18.11., whose start Linux's trace holds: the card, its own switch's two ports and the
        // root port, each at 32 GT/s x16 (sysfs on 19.11.; the trace's OverridePcieParameters say x16 too).
        path: std::vec![crate::rdna4::attach::LinkNow { speed: 5, width: 16 }; 4],
        link_host: crate::rdna4::attach::LinkHost::Fixed,
        msi: true,
        msix: false,
    }
}

fn attach_a(fake: FakeCard) -> (Card, FakeCard) {
    Card::attach(facts(&CARD_A), fake)
}

#[test]
fn the_measured_table_resolves_to_the_planned_modules() {
    let d = parse(&blob(&TABLE, DISCOVERY_READ)).unwrap();
    assert_eq!(d.ips.len(), 46);
    let p = ipmod::plan(&d);
    let got: Vec<(Role, &str, String, &str)> = p
        .choices
        .iter()
        .map(|c| match *c {
            Choice::Chosen { role, version, module } => match module.state {
                State::Planned(item) => (role, module.name, format!("{version}"), item),
                // Wave 42: the three blocks Linux starts before the secure processor have their code.
                State::Present(_) => (role, module.name, format!("{version}"), "present"),
            },
            other => panic!("{other:?}"),
        })
        .collect();
    let want = [
        (Role::Psp, "psp14_0_3", "14.0.3", "GP-gpu-7"),
        (Role::Smu, "smu14_0_3", "14.0.3", "GP-gpu-9"),
        (Role::Nbio, "nbif6_3_1", "6.3.1", "present"),
        (Role::Gmc, "gmc12", "12.0.1", "present"),
        (Role::Gfxhub, "gfxhub12_0", "12.0.1", "GP-gpu-10"),
        (Role::Mmhub, "mmhub4_1_0", "4.1.0", "present"),
        (Role::Ih, "ih7_0", "7.0.0", "present"),
        (Role::Gfx, "gfx12_0_1", "12.0.1", "present"),
        (Role::Mes, "mes12", "12.0.1", "present"),
        (Role::Sdma, "sdma7_0_1", "7.0.1", "present"),
        (Role::Display, "dcn4_0_1", "4.1.0", "GP-gpu-16"),
        (Role::Vcn, "vcn5_0_0", "5.0.0", "GP-gpu-20"),
    ];
    assert_eq!(got, want.map(|(r, n, v, i)| (r, n, String::from(v), i)));
    assert_eq!(p.missing().count(), 0);
    // `planned` is exactly the list of what the card cannot do yet; the 780M's plan has only its two old entries.
    assert_eq!(p.planned().iter().map(|(_, m, item)| (m.name, *item)).collect::<Vec<_>>(), want.iter().filter(|w| w.3 != "present").map(|(_, n, _, i)| (*n, *i)).collect::<Vec<_>>());
    // The same twelve roles rdna4-core listed as missing while there was no module at all.
    assert_eq!(ipfacts::missing_roles(&TABLE, &[]).count(), 12);
    // Navi 44 (GC 12.0.0) is a neighbour: a candidate by Linux's block, never a choice.
    let mut navi44: Vec<ipfacts::Entry> = TABLE.to_vec();
    for e in navi44.iter_mut().filter(|e| e.hw_id == ipfacts::hw::GC) {
        e.version = ipfacts::v(12, 0, 0);
    }
    let p44 = ipmod::plan(&parse(&blob(&navi44, DISCOVERY_READ)).unwrap());
    match p44.choices.iter().find(|c| c.role() == Role::Gfx) {
        Some(Choice::Missing { candidate: Some(m), .. }) => assert_eq!(m.name, "gfx12_0_1"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn the_stick_list_matches_the_modules() {
    let list: Vec<&str> = include_str!("../../firmware-gaming-pc.list").lines().filter(|l| !l.is_empty() && !l.starts_with('#')).collect();
    let p = ipmod::plan(&parse(&blob(&TABLE, DISCOVERY_READ)).unwrap());
    let mut modules: Vec<String> = p.files().into_iter().map(|f| f.name).collect();
    assert_eq!(modules.len(), 12);
    assert!(p.files().iter().all(|f| f.fallback.is_none()));
    let mut listed: Vec<String> = list.iter().map(|s| String::from(*s)).collect();
    modules.sort();
    listed.sort();
    assert_eq!(listed, modules, "firmware-gaming-pc.list and the modules' files");
    // The same set as rdna4-core measured (names, sizes, hashes), by the board's id and revision.
    let wanted = fw::wanted(0x7550, 0xC0);
    let mut measured: Vec<String> = wanted.iter().map(|w| String::from(w.file)).collect();
    measured.sort();
    assert_eq!(measured, modules);
    assert!(wanted.iter().all(|w| w.expected.is_some()));
    // The secure OS's file is one of them: the card starts cold, and the driver loads it.
    assert!(wanted.iter().any(|w| w.file == "amdgpu/psp_14_0_3_sos.bin"));
    // One MES file for both pipes, not the pair; no TOC file (PSP load, not the RLC backdoor).
    assert!(measured.iter().any(|f| f == "amdgpu/gc_12_0_1_uni_mes.bin"));
    assert!(!measured.iter().any(|f| f.ends_with("_mes.bin") && !f.ends_with("uni_mes.bin") || f.ends_with("_mes1.bin") || f.ends_with("_toc.bin")));
    // A kicker board (amdgpu_ucode.c:33: 0x7551 rev 0xC8) asks for other PSP, SMU, RLC and IMU files, unmeasured.
    let kicker: Vec<&str> = fw::wanted(0x7551, 0xC8).iter().filter(|w| w.expected.is_none()).map(|w| w.file).collect();
    assert_eq!(kicker, ["amdgpu/psp_14_0_3_sos_kicker.bin", "amdgpu/psp_14_0_3_ta_kicker.bin", "amdgpu/smu_14_0_3_kicker.bin", "amdgpu/gc_12_0_1_rlc_kicker.bin", "amdgpu/gc_12_0_1_imu_kicker.bin"]);
    assert!(fw::wanted(0x7550, 0xC8).iter().all(|w| w.expected.is_some()), "the rule needs id and revision together");
    // The 780M's list names none of these (its own test keeps it equal to its modules).
    let minipc = include_str!("../../firmware.list");
    assert!(list.iter().all(|f| !minipc.contains(f)));
}

#[test]
fn the_attach_reads_the_measured_card_and_writes_nothing() {
    let (card, fake) = attach_a(FakeCard::measured());
    let a = card.state.as_ref().expect("the measured card attaches");
    assert_eq!((a.memsize_mib, a.vram_bytes), (16_304, 16_304 << 20));
    // plan/spec/gpu-rdna4.md section 1, discovery blob: the table at 0x3FAFF0000 in BAR0, phys 0xABFAFF0000 on card A.
    assert_eq!((a.discovery_offset, a.discovery_phys), (0x3_FAFF_0000, 0xAB_FAFF_0000));
    assert_eq!(a.discovery.ips.len(), 46);
    assert!(a.differs.is_empty(), "{:?}", a.differs);
    assert_eq!(a.ready_after_ms, 0);
    assert!(usize::from(a.binary_size) < DISCOVERY_READ);
    // Nothing was written, and nothing was even tried: the owner counted no write, the fake saw none.
    assert!(card.clean());
    assert_eq!(card.totals, Default::default());
    assert!(fake.tried.is_empty(), "{:?}", fake.tried);
    // The reads are Linux's two fixed dwords (amdgpu_discovery.c:142-143), one read of the 10 KiB, and the PSP's
    // runtime database 1 MiB below the end of VRAM as `psp_sw_init` reads it (amdgpu_psp.c:389-470): the header,
    // the directory, the SCPM entry. After the table, the revision as Linux's `soc24_common_early_init` reads it
    // (STRAP0, discovery.txt line 6).
    assert_eq!(fake.reg_reads, [0x16061 * 4, 0xDE3 * 4, 0xD3C * 4]);
    assert_eq!(a.rev_id, Some(1));
    assert_eq!(fake.mem_reads, [(0x3_FAFF_0000, 10 << 10), (DB, 4), (DB + 4, 516), (DB + 16, 4)]);
    assert_eq!(a.scpm, rdna4_core::scpm::Scpm::Enabled { status: 1 });
    // Card B: the same card at another address; only the address and the physical place differ.
    let (b, fake_b) = Card::attach(facts(&CARD_B), FakeCard::measured());
    let ab = b.state.as_ref().unwrap();
    assert_eq!((ab.discovery_offset, ab.discovery_phys), (0x3_FAFF_0000, 0xA3_FAFF_0000));
    assert!(b.clean() && fake_b.tried.is_empty());
    assert_eq!(b.facts.key, key_b());
}

/// The fake itself refuses every write and keeps it: what the read-only proof rests on.
#[test]
fn the_fake_fails_on_any_write() {
    use crate::rdna4::fakecard::Tried;
    let mut f = FakeCard::measured();
    assert!(!f.reg_write(0x58200, 1));
    assert!(!f.mem_write(0x1000, &[1, 2, 3]));
    assert!(!f.mem_write64(0x2000, 7));
    assert!(!f.doorbell_write64(4, 9));
    assert_eq!(f.tried, [Tried::Reg { byte: 0x58200, value: 1 }, Tried::Mem { at: 0x1000, bytes: 3 }, Tried::Mem { at: 0x2000, bytes: 8 }, Tried::Doorbell { index_dw: 4 }]);
    // Behind the owner with the wave's gate a write does not even reach it, and is counted as refused.
    let mut owner = crate::gpucore::bringup::Bringup::new(FakeCard::measured(), policy::GATE, 16);
    assert!(!owner.reg_write(0x58200, 1));
    assert!(!owner.mem_write(0x1000, &[0; 8]));
    let t = owner.log().totals();
    assert_eq!((t.writes, t.refused, t.mem_refused), (1, 1, 1));
    assert!(owner.into_hw().tried.is_empty());
}

#[test]
fn every_stop_is_named_bounded_and_leaves_the_card_alone() {
    let stop = |fake: FakeCard, p: PciFacts| -> (Stop, FakeCard, u64) {
        let (card, fake) = Card::attach(p, fake);
        assert_eq!(card.totals, Default::default());
        assert!(fake.tried.is_empty());
        (card.state.expect_err("must stop"), fake, card.took_us)
    };
    let a = || facts(&CARD_A);
    // Step 1, configuration space: no MMIO read at all.
    for (p, want) in [
        (PciFacts { device: 0x1900, ..a() }, Stop::NotOurs),
        (PciFacts { class: 0x04_03_00, ..a() }, Stop::NotOurs),
        (PciFacts { command: 0x0004, ..a() }, Stop::DecodeOff),
        (PciFacts { power_state: Some(3), ..a() }, Stop::Asleep(3)),
        (PciFacts { bar5: None, ..a() }, Stop::Bar5("missing")),
        (PciFacts { bar5: Some(BarFact { phys: 0x9120_0000, bytes: 256 << 10, is64: false, prefetch: false }), ..a() }, Stop::Bar5("not the 512 KiB 32-bit window")),
        (PciFacts { bar0: None, ..a() }, Stop::Bar0("missing")),
        (PciFacts { bar0: Some(BarFact { phys: 0xE000_0000, bytes: 256 << 20, is64: false, prefetch: true }), ..a() }, Stop::Bar0("not a placed 64-bit window")),
    ] {
        let (s, fake, _) = stop(FakeCard::measured(), p);
        assert_eq!(s, want);
        assert_eq!(s.step().0, 1);
        assert!(fake.reg_reads.is_empty() && fake.mem_reads.is_empty(), "{s}: no read before the gate");
    }
    // Step 2: the card's firmware never reports ready: 2 s (Linux's wait, amdgpu_discovery.c:303), then stop.
    let (s, fake, took) = stop(FakeCard { ready_after_ms: u64::MAX, ..FakeCard::measured() }, a());
    assert_eq!((s.clone(), s.step()), (Stop::NotReady, (2, "firmware ready")));
    assert_eq!(took, 2_000_000, "bounded by the 2 s");
    assert_eq!(fake.reg_reads.len(), 2000);
    assert!(fake.mem_reads.is_empty());
    // A card that is ready after 80 ms attaches, and says so.
    let (late, _) = attach_a(FakeCard { ready_after_ms: 80, ..FakeCard::measured() });
    assert_eq!(late.state.as_ref().unwrap().ready_after_ms, 80);
    // Step 3: all ones (the card does not answer), zero (an APU's layout), more memory than BAR0.
    assert_eq!(stop(FakeCard { gone: true, ..FakeCard::measured() }, a()).0, Stop::Memsize(MemsizeError::Gone));
    assert_eq!(stop(FakeCard::with(0, blob(&TABLE, DISCOVERY_READ)), a()).0, Stop::Memsize(MemsizeError::Zero));
    let (s, fake, _) = stop(FakeCard::with(16_385, blob(&TABLE, DISCOVERY_READ)), a());
    assert_eq!((s.clone(), s.step().0), (Stop::OutsideBar0 { memsize_mib: 16_385 }, 3));
    assert!(fake.mem_reads.is_empty(), "nothing is read outside the card's own window");
    // Step 5: a table whose size reaches past the 10 KiB read, a damaged byte, no signature.
    let mut long = blob(&TABLE, DISCOVERY_READ);
    long[10..12].copy_from_slice(&((DISCOVERY_READ + 4) as u16).to_le_bytes());
    assert!(matches!(stop(FakeCard::with(16_304, long), a()).0, Stop::Blob(BlobRefusal::Size { .. })));
    let mut bad = blob(&TABLE, DISCOVERY_READ);
    bad[200] ^= 0x40;
    assert!(matches!(stop(FakeCard::with(16_304, bad), a()).0, Stop::Blob(BlobRefusal::Checksum { .. })));
    let (s, _, _) = stop(FakeCard::with(16_304, std::vec![0u8; DISCOVERY_READ]), a());
    assert!(matches!(s, Stop::Blob(BlobRefusal::Signature { found: 0 })));
    assert_eq!(s.step(), (5, "table check"));
    // 8 KiB read where 10 KiB exist (Linux 6.6's size): a table that needs the last 2 KiB is refused, never cut.
    let mut big: Vec<ipfacts::Entry> = Vec::new();
    while blob(&big, 0).len() <= 8 << 10 {
        big.extend_from_slice(&TABLE);
        for (i, e) in big.iter_mut().enumerate() {
            e.instance = i as u8;
        }
    }
    let whole = blob(&big, DISCOVERY_READ);
    assert!(whole.len() == DISCOVERY_READ && parse(&whole).is_ok(), "the 10 KiB read holds it");
    assert!(matches!(parse(&whole[..8 << 10]), Err(crate::gpucore::disc::Why::BinarySize { .. })), "an 8 KiB read is refused by its size");
    assert!(matches!(rdna4_core::layout::accept(&whole[..8 << 10]), Err(BlobRefusal::Size { .. })));
}

/// Linux's second place for the table (amdgpu_discovery.c:338-349): a card whose default place holds no signature
/// names the place in DRIVER_SCRATCH_0..2. Read there if all of it lies in BAR0; a place that does not is a named
/// stop, and nothing outside the card's own window is read.
#[test]
fn a_table_the_cards_mailbox_names_is_read_there() {
    let (c2pmsg, memsize, scratch) = (0x16061 * 4, 0xDE3 * 4, [0x94 * 4, 0x95 * 4, 0x96 * 4]);
    let default_at = 0x3_FAFF_0000u64;
    // The same numbers as the 780M's reader (`gpucore::disc::tmr_place`): one rule from the same Linux lines.
    assert_eq!(scratch, crate::gpucore::disc::DRIVER_SCRATCH);
    assert_eq!(rdna4_core::layout::MAILBOX_READ_MAX, crate::gpucore::disc::SCRATCH_MAX);

    let (card, fake) = attach_a(FakeCard::with_mailbox(0x3_F000_0000, DISCOVERY_READ as u32));
    let a = card.state.as_ref().expect("the table at the mailbox's place attaches");
    assert_eq!((a.source, a.discovery_offset, a.discovery_phys, a.read_bytes), (Source::Mailbox, 0x3_F000_0000, 0xAB_F000_0000, DISCOVERY_READ));
    assert_eq!(a.discovery.ips.len(), 46);
    assert!(a.differs.is_empty() && card.clean() && fake.tried.is_empty(), "{:?} {:?}", a.differs, fake.tried);
    // Linux's order: the default place first, then the size, then the offset (low, high); the second read there.
    assert_eq!(fake.reg_reads, [c2pmsg, memsize, scratch[2], scratch[0], scratch[1], 0xD3C * 4]);
    assert_eq!(fake.mem_reads[..2], [(default_at, DISCOVERY_READ), (0x3_F000_0000, DISCOVERY_READ)]);
    assert_eq!(fake.mem_reads[2..], [(DB, 4), (DB + 4, 516), (DB + 16, 4)], "then the runtime database");
    let dev = Device::detached(key_a(), 0, Family::Rdna4, true);
    let text = snapshot::text(dev, &card, &[]);
    assert!(text.contains("discovery: at VRAM offset 0x3f0000000 (phys 0xabf0000000), binary 1.0, "), "{text}");
    assert!(text.contains("place: the card's MAILBOX (DRIVER_SCRATCH_0..2); the default place (memory size - 64 KiB) held no table"), "{text}");
    assert_eq!(snapshot::report_line(dev, &card), "Grafikkarte 1 (zeigt das Bild): erkannt, 16 GB Speicher, Bauteilliste gelesen (wie unter Linux gemessen, am Ersatzort gefunden). Nichts verändert.");
    // The measured card: the default place answers and the mailbox is not even asked.
    let (measured, fake) = attach_a(FakeCard::measured());
    assert_eq!(measured.state.as_ref().unwrap().source, Source::Default);
    assert_eq!(fake.reg_reads, [c2pmsg, memsize, 0xD3C * 4]);

    // A mailbox that names a whole megabyte: its first 64 KiB are read (no table is longer), never more.
    let (big, fake) = attach_a(FakeCard::with_mailbox(0x1000_0000, 1 << 20));
    assert_eq!(big.state.as_ref().map(|a| (a.source, a.read_bytes, a.discovery.ips.len())).unwrap(), (Source::Mailbox, 64 << 10, 46));
    assert_eq!(fake.mem_reads[1], (0x1000_0000, 64 << 10));

    let stop = |fake: FakeCard| -> (Stop, FakeCard) {
        let (card, fake) = attach_a(fake);
        assert!(card.totals == Default::default() && fake.tried.is_empty());
        (card.state.expect_err("must stop"), fake)
    };
    // A place outside BAR0 (16 GiB): named, and the only memory read is the one at the default place.
    for (lo, hi, size, want) in [
        (0, 0x10, 0x2800, MailboxRefusal::OutsideBar0 { offset: 0x10_0000_0000, size: 0x2800 }),
        (0xFFFF_F000, 3, 0x2800, MailboxRefusal::OutsideBar0 { offset: 0x3_FFFF_F000, size: 0x2800 }),
        (0x1000, 0, 8, MailboxRefusal::Size { size: 8 }),
        (0x1002, 0, 0x2800, MailboxRefusal::Unaligned { offset: 0x1002, size: 0x2800 }),
    ] {
        let (s, fake) = stop(FakeCard { scratch: [lo, hi, size], ..FakeCard::with_mailbox(0x3_F000_0000, 0x2800) });
        assert_eq!((s.clone(), s.step()), (Stop::Mailbox(want), (4, "table read")));
        assert_eq!(fake.mem_reads, [(default_at, DISCOVERY_READ)], "{s}: nothing is read at a refused place");
    }
    let (s, fake) = stop(FakeCard { scratch: [u32::MAX; 3], ..FakeCard::with_mailbox(0x3_F000_0000, 0x2800) });
    assert_eq!(s, Stop::Mailbox(MailboxRefusal::Gone));
    assert_eq!(fake.reg_reads.len(), 3, "all ones in the size: the offset is not read");
    // The mailbox names a place inside BAR0 that holds no table either.
    let (s, fake) = stop(FakeCard { scratch: [0x2000_0000, 0, 0x2800], ..FakeCard::with_mailbox(0x3_F000_0000, 0x2800) });
    assert_eq!((s.clone(), s.step()), (Stop::MailboxBlob { offset: 0x2000_0000, why: BlobRefusal::Signature { found: 0 } }, (5, "table check")));
    assert_eq!(fake.mem_reads.len(), 2);
    // No signature and an empty mailbox: the stop says both, after exactly one more register read (the size).
    let (s, fake) = stop(FakeCard::with(16_304, std::vec![0u8; DISCOVERY_READ]));
    assert_eq!(s, Stop::Blob(BlobRefusal::Signature { found: 0 }));
    assert_eq!(fake.reg_reads, [c2pmsg, memsize, scratch[2]]);
    assert!(format!("{s}").contains("the card's mailbox names no other"), "{s}");
    // A table that IS at the default place but damaged: Linux does not ask the mailbox then, and neither does this.
    let mut bad = blob(&TABLE, DISCOVERY_READ);
    bad[200] ^= 0x40;
    let (s, fake) = stop(FakeCard { scratch: [0x2000_0000, 0, 0x2800], ..FakeCard::with(16_304, bad) });
    assert!(matches!(s, Stop::Blob(BlobRefusal::Checksum { .. })), "{s}");
    assert_eq!(fake.reg_reads, [c2pmsg, memsize]);
}

/// The report page's line for a stopped attach: the step and, in plain German, the cause. Two causes at one step must
/// not look the same on the screen, which Georg reads before the desk has any connection.
#[test]
fn every_stop_has_its_own_line_on_the_report_page() {
    use crate::gpucore::disc::Why;
    let sig = BlobRefusal::Signature { found: 0 };
    let sum = BlobRefusal::Checksum { stored: 1, computed: 2 };
    let size = BlobRefusal::Size { binary_size: 0x3000, read: DISCOVERY_READ };
    let stops = [
        (1, Stop::NotOurs),
        (1, Stop::DecodeOff),
        (1, Stop::Asleep(3)),
        (1, Stop::Bar5("missing")),
        (1, Stop::Bar0("missing")),
        (2, Stop::Unreadable(0x16061 * 4)),
        (2, Stop::NotReady),
        (3, Stop::Memsize(MemsizeError::Gone)),
        (3, Stop::Memsize(MemsizeError::Zero)),
        (3, Stop::OutsideBar0 { memsize_mib: 16_385 }),
        (4, Stop::VramRead { offset: 0x3_FAFF_0000 }),
        (4, Stop::Mailbox(MailboxRefusal::Gone)),
        (4, Stop::Mailbox(MailboxRefusal::Size { size: 8 })),
        (4, Stop::Mailbox(MailboxRefusal::Unaligned { offset: 2, size: 0x2800 })),
        (4, Stop::Mailbox(MailboxRefusal::OutsideBar0 { offset: 1 << 36, size: 0x2800 })),
        (5, Stop::Blob(sig)),
        (5, Stop::Blob(sum)),
        (5, Stop::Blob(size)),
        (5, Stop::MailboxBlob { offset: 0x1000, why: sig }),
        (5, Stop::MailboxBlob { offset: 0x1000, why: sum }),
        (5, Stop::MailboxBlob { offset: 0x1000, why: size }),
        (5, Stop::Parse(Why::NoIpTable)),
    ];
    let dev = Device::detached(key_b(), 1, Family::Rdna4, false);
    let mut lines: Vec<String> = Vec::new();
    let mut reasons: Vec<&str> = Vec::new();
    for (step, stop) in stops {
        assert_eq!(stop.step().0, step, "{stop}");
        let reason = snapshot::reason(&stop);
        let card = Card { facts: facts(&CARD_B), state: Err(stop), totals: Default::default(), took_us: 0 };
        let line = snapshot::report_line(dev, &card);
        assert!(line.starts_with(&format!("Grafikkarte 2: Lesen angehalten bei Schritt {step} von 5 (")) && line.ends_with(&format!("): {reason}. Nichts verändert.")), "{line}");
        // Plain German: no code words, no numbers from the code.
        for word in ["BAR", "PCI", "0x", "SCRATCH", "MMIO", "Signature", "Checksum", "VRAM", "Register"] {
            assert!(!line.contains(word), "{line}");
        }
        lines.push(line);
        reasons.push(reason);
    }
    let n = lines.len();
    lines.sort();
    lines.dedup();
    reasons.sort_unstable();
    reasons.dedup();
    assert_eq!((lines.len(), reasons.len()), (n, n), "two stops share a line");
    // A register that does not read back names the step it was read for.
    assert_eq!([0x16061u32, 0xDE3, 0x96, 0x94].map(|d| Stop::Unreadable(d * 4).step().0), [2, 3, 4, 4]);
}

/// The attach may sleep up to 2 s for a card whose firmware is not ready. A caller that arrives meanwhile waits on
/// the slot's lock (in the kernel: asleep, ktest `gx.devs.slow_attach`), gets the same card, and attaches nothing.
#[test]
fn a_second_caller_waits_for_a_running_attach() {
    use crate::rdna4::CardSlot;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    let slot = CardSlot::new();
    let (inside, release, second_done) = (AtomicBool::new(false), AtomicBool::new(false), AtomicBool::new(false));
    let attaches = AtomicUsize::new(0);
    std::thread::scope(|s| {
        let first = s.spawn(|| {
            let card = slot.get_or_attach(|| {
                attaches.fetch_add(1, Ordering::AcqRel);
                inside.store(true, Ordering::Release);
                while !release.load(Ordering::Acquire) {
                    std::thread::yield_now();
                }
                attach_a(FakeCard { ready_after_ms: 80, ..FakeCard::measured() }).0
            });
            card as *const Card as usize
        });
        while !inside.load(Ordering::Acquire) {
            std::thread::yield_now();
        }
        // While it runs there is no card yet, and asking for one without waiting answers at once.
        assert!(slot.attaching() && slot.get().is_none());
        let second = s.spawn(|| {
            let card = slot.get_or_attach(|| {
                attaches.fetch_add(1, Ordering::AcqRel);
                attach_a(FakeCard::measured()).0
            });
            second_done.store(true, Ordering::Release);
            card as *const Card as usize
        });
        std::thread::sleep(std::time::Duration::from_millis(30));
        assert!(!second_done.load(Ordering::Acquire), "the second caller went past a running attach");
        release.store(true, Ordering::Release);
        assert_eq!(first.join().unwrap(), second.join().unwrap(), "two cards for one device");
    });
    assert_eq!(attaches.load(Ordering::Acquire), 1);
    assert_eq!(slot.get().and_then(|c| c.state.as_ref().ok()).map(|a| a.ready_after_ms), Some(80));
    assert!(!slot.attaching() && slot.waiting() == 0);
    // A later caller never attaches again.
    slot.get_or_attach(|| panic!("attached twice"));
}

#[test]
fn a_table_that_differs_from_the_measured_one_is_named() {
    let mut other: Vec<ipfacts::Entry> = TABLE.to_vec();
    let smu = other.iter().position(|e| e.hw_id == ipfacts::hw::MP1).unwrap();
    other[smu].version = ipfacts::v(14, 0, 2);
    let gone = other.iter().position(|e| e.hw_id == ipfacts::hw::HDP).unwrap();
    other.remove(gone);
    other.push(ipfacts::Entry { hw_id: 299, instance: 0, version: ipfacts::v(1, 0, 0), bases: &[0x100] });
    let (card, fake) = attach_a(FakeCard::with(16_304, blob(&other, DISCOVERY_READ)));
    let a = card.state.as_ref().unwrap();
    assert_eq!(a.differs, ["hw id 1 instance 0: version 14.0.2 (measured 14.0.3)", "hw id 41 instance 0 is missing", "hw id 299 instance 0 is new"]);
    // nbif6_3_1's one read-only fact: the table places the memory-size register where Linux's fixed dword is; a
    // table that moves NBIF's third base says so instead of trusting a number read from the wrong place.
    assert_eq!(crate::ipmod::rdna4::memsize_dword(&parse(&blob(&TABLE, DISCOVERY_READ)).unwrap()), Some(0xDE3));
    let mut moved: Vec<ipfacts::Entry> = TABLE.to_vec();
    let nbif = moved.iter().position(|e| e.hw_id == ipfacts::hw::NBIF).unwrap();
    moved[nbif].bases = &[0, 0x14, 0xE20, 0x10400, 0x0241B000, 0x04040000];
    let (m, _) = attach_a(FakeCard::with(16_304, blob(&moved, DISCOVERY_READ)));
    assert_eq!(m.state.as_ref().unwrap().differs, ["hw id 108 instance 0: other register bases", "the table places the memory-size register at dword 0xee3, it was read at 0xde3"]);
    assert!(card.clean() && fake.tried.is_empty(), "a surprise in the table is reported, nothing more");
    let text = snapshot::text(Device::detached(key_a(), 0, Family::Rdna4, true), &card, &[]);
    assert!(text.contains("measured: 3 DIFFERENCES to the table read under Linux on 5.11.:\n  hw id 1 instance 0: version 14.0.2"), "{text}");
    // SMU 14.0.2 has no module: the plan says so by name.
    assert!(text.contains("module smu: MISSING for 14.0.2 (Linux serves it with smu14_0_3's block)"), "{text}");
}

/// The rule of item 22, and the sources of the attach checked for anything that writes.
#[test]
fn the_policy_and_the_sources_hold_no_write() {
    use policy::Ask;
    assert!(policy::allowed(Ask::Read, 1).is_ok());
    for stage in 1..=2 {
        assert!(policy::allowed(Ask::BusMaster, stage).is_err(), "no bus master in G{stage}");
    }
    assert!(policy::allowed(Ask::BusMaster, 3).is_ok());
    for stage in 1..=9 {
        for ask in [Ask::Layout, Ask::Reset, Ask::Power] {
            assert!(policy::allowed(ask, stage).is_err(), "{ask:?} in G{stage}");
        }
    }
    assert_eq!(policy::DMA_BITS, 44);
    assert!(policy::dma_reachable((1 << 44) - 4096, 4096) && !policy::dma_reachable(1 << 44, 4096));
    assert!(matches!(policy::GATE, crate::gpucore::bringup::Gate::Refuse(_)));
    // No call of a write, an enable or an interrupt request in the attach's sources. The two `fn` definitions in the
    // glue's `Hw` (which refuse and log) are definitions, not calls: a call has a `.` or `::` before the name.
    for (file, text) in [
        ("rdna4/mod.rs", include_str!("../rdna4/mod.rs")),
        ("rdna4/attach.rs", include_str!("../rdna4/attach.rs")),
        ("rdna4/live.rs", include_str!("../rdna4/live.rs")),
        ("rdna4/snapshot.rs", include_str!("../rdna4/snapshot.rs")),
        ("rdna4/fw.rs", include_str!("../rdna4/fw.rs")),
    ] {
        let code: String = text.lines().filter(|l| !l.trim_start().starts_with("//")).collect::<Vec<_>>().join("\n");
        for call in policy::FORBIDDEN_CALLS {
            for lead in [".", "::"] {
                assert!(!code.contains(&format!("{lead}{call}(")), "{file} calls {call}");
            }
        }
        assert!(!code.contains("unsafe"), "{file}");
    }
}

struct Stick(BTreeMap<String, Vec<u8>>);

impl FwSource for Stick {
    fn read(&mut self, path: &str) -> Result<Option<Vec<u8>>, LoadError> {
        Ok(self.0.get(path).cloned())
    }
}

/// A firmware file of `bytes` bytes with a common header (amdgpu_ucode.h:28-39) and nothing else.
fn fw_file(bytes: u32, ip: (u16, u16), version: u32) -> Vec<u8> {
    let mut b = std::vec![0u8; bytes as usize];
    b[0..4].copy_from_slice(&bytes.to_le_bytes());
    b[4..8].copy_from_slice(&0x100u32.to_le_bytes());
    b[8..10].copy_from_slice(&2u16.to_le_bytes());
    b[12..14].copy_from_slice(&ip.0.to_le_bytes());
    b[14..16].copy_from_slice(&ip.1.to_le_bytes());
    b[16..20].copy_from_slice(&version.to_le_bytes());
    b[20..24].copy_from_slice(&(bytes - 0x100).to_le_bytes());
    b[24..28].copy_from_slice(&0x100u32.to_le_bytes());
    b
}

#[test]
fn the_snapshot_says_what_the_remote_desk_needs() {
    let (card, _) = attach_a(FakeCard::measured());
    let wanted = fw::wanted(0x7550, 0xC0);
    let dir = "/boot/BITSOS2/firmware/";
    let mut stick = Stick(BTreeMap::new());
    stick.0.insert(format!("{dir}amdgpu/psp_14_0_3_sos.bin"), fw_file(435_632, (14, 0), 0x0031_0045));
    stick.0.insert(format!("{dir}amdgpu/psp_14_0_3_ta.bin"), fw_file(1000, (14, 0), 7));
    stick.0.insert(format!("{dir}amdgpu/smu_14_0_3.bin.zst"), std::vec![1, 2, 3]);
    stick.0.insert(format!("{dir}amdgpu/gc_12_0_1_pfp.bin"), std::vec![0xFF; 64]);
    let files = fw::on_stick(&wanted, &mut stick);
    assert_eq!(files.iter().filter(|f| f.usable()).map(|f| f.wanted.file).collect::<Vec<_>>(), ["amdgpu/psp_14_0_3_sos.bin"]);
    let text = snapshot::text(Device::detached(key_a(), 0, Family::Rdna4, true), &card, &files);
    for line in [
        "gpu card0 0000:04:00.0: Radeon RX 9070 XT (1002:7550 rev 0xc0, subsystem 148c:2435), the display card (its BAR0 holds the firmware's framebuffer), read-only attach (GP-GPU1)",
        "writes: 0 register writes, 0 refused, 0 memory writes refused: nothing was written, nothing was tried",
        "pci: command 0x0006 (memory decode on, bus master on as the firmware left it), power D0, MSI yes, MSI-X no",
        "bar0: 0xa800000000, 16384 MiB, 64-bit prefetchable (graphics memory; only the table's pages are read, through a window of at most 68 KiB)",
        "bar2: 0xac00000000, 256 MiB, 64-bit prefetchable (doorbells; not mapped)",
        "bar5: 0x91200000, 512 KiB, 32-bit (registers; mapped uncached, at most five fixed dwords read)",
        "link: 32 GT/s x16 (can 32 GT/s x16); narrowest above it 32 GT/s x8 (can 32 GT/s x8) at 0000:00:06.0",
        "dma: 44 bits (recorded for later waves); no memory handed to the card, bus master not touched",
        "attach: PASS in 0 ms (firmware ready after 0 ms)",
        "memsize: 16304 MiB (RCC_CONFIG_MEMSIZE), 17095983104 bytes of VRAM, all of it visible through BAR0",
        "discovery: at VRAM offset 0x3faff0000 (phys 0xabfaff0000), binary 1.0, ",
        " of 10240 bytes read are the table, 46 entries, table version 4",
        "place: the default one (memory size - 64 KiB); the card's mailbox was not asked",
        "measured: the table equals the one read under Linux on 5.11. (46 entries, versions and bases)",
        "ip GC 11 0 12.0.1 var 0 sub 0 harvested no bases 0x00001260 0x0000a000 0x0001c000 0x02402c00",
        "ip MP0 255 0 14.0.3 ",
        "module psp: psp14_0_3 for 14.0.3, not built yet (GP-gpu-7; Linux psp_v14_0)",
        "module display: dcn4_0_1 for 4.1.0, not built yet (GP-gpu-16; Linux dm (dcn401))",
        "firmware amdgpu/psp_14_0_3_sos.bin: present, 435632 bytes (as measured), header 2.0, ip 14.0, version 0x00310045",
        "firmware amdgpu/psp_14_0_3_ta.bin: present, 1000 bytes (NOT the measured size), header 2.0, ip 14.0, version 0x00000007",
        "firmware amdgpu/smu_14_0_3.bin: only the .zst is on the stick (not unpacked)",
        "firmware amdgpu/gc_12_0_1_pfp.bin: 64 bytes, header not accepted: ",
        "firmware amdgpu/vcn_5_0_0.bin: MISSING on the stick",
        "stages: next of the chain nbif; nbif not run, gart off, ih off, window none, bootloader commands 0 of 9, secure os None, ring none, ",
    ] {
        assert!(text.contains(line), "missing: {line}\n{text}");
    }
    assert_eq!(text.lines().filter(|l| l.starts_with("ip ")).count(), 46);
    assert_eq!(text.lines().filter(|l| l.starts_with("module ")).count(), 12);
    assert_eq!(text.lines().filter(|l| l.starts_with("firmware ")).count(), 12);
    // The other card: the same, said to be not the display card.
    let (b, _) = Card::attach(facts(&CARD_B), FakeCard::measured());
    let dev_b = Device::detached(key_b(), 1, Family::Rdna4, false);
    assert!(snapshot::text(dev_b, &b, &[]).starts_with("gpu card1 0000:07:00.0: Radeon RX 9070 XT (1002:7550 rev 0xc0, subsystem 148c:2435), not the display card, read-only attach"));
    // The report page's lines (plain German).
    let dev_a = Device::detached(key_a(), 0, Family::Rdna4, true);
    assert_eq!(snapshot::report_line(dev_a, &card), "Grafikkarte 1 (zeigt das Bild): erkannt, 16 GB Speicher, Bauteilliste gelesen (wie unter Linux gemessen). Nichts verändert.");
    assert_eq!(snapshot::report_line(dev_b, &b), "Grafikkarte 2: erkannt, 16 GB Speicher, Bauteilliste gelesen (wie unter Linux gemessen). Nichts verändert.");
    let (stopped, _) = Card::attach(facts(&CARD_B), FakeCard { gone: true, ..FakeCard::measured() });
    assert_eq!(snapshot::report_line(dev_b, &stopped), "Grafikkarte 2: Lesen angehalten bei Schritt 3 von 5 (Speichergröße lesen): Speichergröße unlesbar, Karte antwortet nicht. Nichts verändert.");
    let text = snapshot::text(dev_b, &stopped, &[]);
    assert!(text.contains("attach: STOPPED at step 3 of 5 (memory size) after 0 ms: the memory-size register reads all ones: the card does not answer"), "{text}");
    assert_eq!(text.lines().filter(|l| l.starts_with("ip ") || l.starts_with("module ")).count(), 0);
    let _ = attach::CMD_MEMORY;
}
