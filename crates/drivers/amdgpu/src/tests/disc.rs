//! GX-6 discovery parser: the 780M's table rebuilt from Linux's sysfs view, every table type, table versions 1-4,
//! hostile blobs (each refused with its reason), random damage (never a panic), the index/data window over a
//! fake GPU, and the real Mini-PC blob once the desk adds it as a fixture.

use super::blob::{self, Blob, build, fix, fix_table, ip, minipc_ips, poke16, poke32, slot};
use crate::gpucore::disc::{self, Note, Table, Why, parse};
use crate::gpucore::hwid;
use crate::gpucore::vram::{self, IndexRegs, MM_DATA, MM_INDEX, MM_INDEX_HI};
use crate::ip::IpVersion;
use std::cell::{Cell, RefCell};
use std::format;
use std::vec;
use std::vec::Vec;

fn v(a: u8, b: u8, c: u8) -> IpVersion {
    IpVersion { major: a, minor: b, rev: c }
}

/// The signatures as the bytes the hardware stores (little endian), so a swapped constant cannot hide behind
/// synthetic blobs built from the same constant (the 27.10. Mini-PC read found exactly that).
#[test]
fn signatures_match_the_stored_bytes() {
    assert_eq!(&disc::IP_TABLE_SIGNATURE.to_le_bytes(), b"IPDS");
    assert_eq!(&disc::HARVEST_SIGNATURE.to_le_bytes(), b"HARV");
    assert_eq!(&disc::VCN_TABLE_ID.to_le_bytes(), b"VCN\0");
    assert_eq!(&disc::MALL_TABLE_ID.to_le_bytes(), b"MALL");
    assert_eq!(disc::GC_TABLE_ID, u32::from(u16::from_le_bytes(*b"GC")));
    assert_eq!(disc::BINARY_SIGNATURE, 0x2821_1407);
}

fn minipc() -> Blob {
    let mut b = Blob::new(minipc_ips());
    b.harvest = Some(vec![]);
    b.gc = Some((1, 2, (1..=30).collect()));
    b.vcn = Some(vec![(0, 0)]);
    b
}

#[test]
fn minipc_table_round_trip() {
    let want = minipc_ips();
    assert_eq!(want.len(), 39, "Linux's sysfs lists 39 IPs");
    let (buf, _) = build(&minipc());
    assert_eq!(buf.len(), disc::TMR_SIZE);
    let d = parse(&buf).expect("780M blob");
    assert_eq!((d.table_version, d.dies, d.bases_64), (4, 1, false));
    assert_eq!(d.ips.len(), 39);
    for (e, w) in d.ips.iter().zip(&want) {
        assert_eq!((e.hw_id, e.instance), (w.hw_id, w.inst));
        assert_eq!(e.version, v(w.ver.0, w.ver.1, w.ver.2), "hw_id {}", e.hw_id);
        assert_eq!(e.bases, w.bases.iter().map(|&a| a as u32).collect::<Vec<_>>());
        assert!(!d.harvested(e));
    }
    assert_eq!(d.version(hwid::GC), Some(v(11, 0, 1)));
    assert_eq!(d.version(hwid::DMU), Some(v(3, 1, 4)));
    assert_eq!(d.version(hwid::MP0), Some(v(13, 0, 4)));
    assert_eq!(d.find(hwid::UMC, 1).map(|e| e.bases.len()), Some(8));
    // Unknown IDs are kept with their number.
    assert!(d.find(282, 0).is_some() && d.find(286, 0).is_some());
    // SOC15: DCN's segment bases are the ones dcn314-regs was generated with.
    let dmu = d.find(hwid::DMU, 0).unwrap();
    assert_eq!(&dmu.bases[..4], dcn314_regs::SEG_BASES);
    assert_eq!(d.reg(hwid::MP0, 0, 1, 0x91).map(|dw| dw * 4), Some(dcn314_regs::bar5::MP0_C2PMSG_81));
    let gc = d.gc.as_ref().unwrap();
    assert_eq!((gc.major, gc.minor, gc.fields.len()), (1, 2, 30));
    assert_eq!(gc.get("gc_num_se"), Some(1));
    assert_eq!(gc.get("gc_gl2c_per_gpu"), Some(30));
    assert_eq!(d.harvest.as_deref(), Some(&[][..]));
    assert_eq!(d.vcn.as_ref().unwrap().instances.len(), 1);
    assert!(d.notes.is_empty(), "{:?}", d.notes);
}

#[test]
fn table_versions_and_64_bit_bases() {
    for tv in 1..=4u16 {
        let mut b = Blob::new(vec![ip(hwid::GC, 0, (11, 0, 1), &[0x1260, 0xA000])]);
        b.table_version = tv;
        let d = parse(&build(&b).0).unwrap();
        assert_eq!(d.table_version, tv);
        assert_eq!(d.ips[0].bases, vec![0x1260, 0xA000]);
    }
    let mut b = Blob::new(vec![ip(hwid::GC, 0, (12, 0, 1), &[0xFFFF_FFFF_C000_1260, 0x2_0000_A000])]);
    b.bases_64 = true;
    let d = parse(&build(&b).0).unwrap();
    // Low 32 bits, bits 31:30 cleared (Linux amdgpu_discovery.c:1300-1313).
    assert_eq!(d.ips[0].bases, vec![0x1260, 0xA000]);
    b.table_version = 3;
    let d = parse(&build(&b).0).unwrap();
    assert_eq!(d.notes, vec![Note::Bases64BeforeV4]);
}

#[test]
fn several_dies() {
    let mut b = Blob::new(vec![ip(hwid::GC, 0, (9, 4, 3), &[1])]);
    b.dies.push(vec![ip(hwid::GC, 1, (9, 4, 3), &[2]), ip(hwid::SDMA0, 0, (4, 4, 2), &[3, 4])]);
    let d = parse(&build(&b).0).unwrap();
    assert_eq!(d.dies, 2);
    assert_eq!(d.ips.iter().map(|e| (e.die, e.hw_id, e.instance)).collect::<Vec<_>>(), vec![(0, 11, 0), (1, 11, 1), (1, 42, 0)]);
}

#[test]
fn harvest_vcn_and_mall() {
    let mut b = Blob::new(vec![ip(hwid::VCN, 0, (4, 0, 0x82), &[0x7800]), ip(hwid::VCN, 1, (4, 0, 2), &[0x7E00])]);
    b.harvest = Some(vec![(hwid::VCN, 1)]);
    b.vcn = Some(vec![(0, 0b0001), (1, 0)]);
    b.mall = Some((2, 0, vec![4096]));
    let d = parse(&build(&b).0).unwrap();
    // VCN's revision bits 7:6 are its capability, not the version (Linux masks them).
    assert_eq!((d.ips[0].version, d.ips[0].vcn_config), (v(4, 0, 2), 0x80));
    assert!(!d.harvested(&d.ips[0]) && d.harvested(&d.ips[1]));
    let vcn = d.vcn.as_ref().unwrap();
    assert_eq!(vcn.instances[0].fuses, 1);
    assert_eq!(d.mall.as_ref().unwrap().fields, vec![("mall_size_per_umc", 4096)]);
    // Before GC 10.2 the harvest bit is the entry's own variant nibble (Linux harvest_ip).
    let mut e = ip(hwid::VCN, 0, (2, 5, 0), &[1]);
    e.byte7 = 0x10;
    let d = parse(&build(&Blob::new(vec![ip(hwid::GC, 0, (10, 1, 10), &[1]), e])).0).unwrap();
    assert!(d.harvested(&d.ips[1]));
}

#[test]
fn mall_problems_are_notes_like_linux() {
    let mut b = Blob::new(vec![ip(hwid::GC, 0, (11, 0, 0), &[1])]);
    b.mall = Some((1, 0, vec![1, 2, 3, 4]));
    let (mut buf, l) = build(&b);
    buf[l.mall + 12] ^= 0xFF; // MALL data changed, its checksum not
    fix(&mut buf);
    let d = parse(&buf).unwrap();
    assert!(matches!(d.notes[..], [Note::MallUnchecked(Why::TableChecksum { table: Table::Mall, .. })]), "{:?}", d.notes);
    assert_eq!(d.mall.unwrap().fields.len(), 4);
}

/// Each hostile blob is refused, and with the right reason.
#[test]
fn hostile_blobs_refused_with_reason() {
    let mut b = minipc();
    b.mall = Some((1, 0, vec![0; 4]));
    let (good, l) = build(&b);
    let refuse = |buf: &[u8]| parse(buf).expect_err("accepted a hostile blob");

    assert_eq!(refuse(&[]), Why::Short { len: 0 });
    assert!(matches!(refuse(&good[..59]), Why::Short { len: 59 }));

    let mut x = good.clone();
    x[0] ^= 1;
    assert!(matches!(refuse(&x), Why::BinarySignature { .. }));

    let mut x = good.clone();
    x[l.die0 + 10] ^= 0x40; // a version byte, no checksum fixed
    assert!(matches!(refuse(&x), Why::BinaryChecksum { .. }));

    for size in [0u16, 59, 9000] {
        let mut x = good.clone();
        poke16(&mut x, 10, size);
        assert!(matches!(refuse(&x), Why::BinarySize { .. }), "size {size}");
    }
    // Truncated: the read gave less than binary_size.
    assert!(matches!(refuse(&good[..l.size - 1]), Why::BinarySize { .. }));

    let mut x = good.clone();
    poke16(&mut x, slot(0), 0);
    fix(&mut x);
    assert_eq!(refuse(&x), Why::NoIpTable);

    // A table offset past the end of the binary.
    let mut x = good.clone();
    poke16(&mut x, slot(1), (l.size - 4) as u16);
    fix(&mut x);
    assert!(matches!(refuse(&x), Why::TableOutside { table: Table::Gc, .. } | Why::TableSignature { table: Table::Gc, .. }));
    let mut x = good.clone();
    poke32(&mut x, l.gc + 8, 0xFFFF_0000);
    fix(&mut x);
    assert!(matches!(refuse(&x), Why::TableOutside { table: Table::Gc, .. }));

    let mut x = good.clone();
    poke32(&mut x, l.gc + 8, 8);
    fix(&mut x);
    assert!(matches!(refuse(&x), Why::TableTooSmall { table: Table::Gc, size: 8 }));

    let mut x = good.clone();
    x[l.harvest] ^= 1;
    fix(&mut x);
    assert!(matches!(refuse(&x), Why::TableSignature { table: Table::Harvest, .. }));

    let mut x = good.clone();
    x[l.ip + 20] ^= 1; // inside the IP table, its own checksum left stale
    fix(&mut x);
    assert!(matches!(refuse(&x), Why::TableChecksum { table: Table::Ip, .. }));

    let ip_fix = |x: &mut Vec<u8>| {
        let n = usize::from(u16::from_le_bytes([x[l.ip + 6], x[l.ip + 7]]));
        fix_table(x, 0, n)
    };
    let mut x = good.clone();
    poke16(&mut x, l.ip + 4, 5);
    ip_fix(&mut x);
    assert_eq!(refuse(&x), Why::TableVersion { table: Table::Ip, major: 5 });

    for dies in [0u16, 17] {
        let mut x = good.clone();
        poke16(&mut x, l.ip + 12, dies);
        ip_fix(&mut x);
        assert_eq!(refuse(&x), Why::DieCount { dies });
    }

    let mut x = good.clone();
    poke16(&mut x, l.die0, 3);
    ip_fix(&mut x);
    assert_eq!(refuse(&x), Why::DieId { die: 0, found: 3 });

    let mut x = good.clone();
    poke16(&mut x, l.ip + 16, 0xFFF0);
    ip_fix(&mut x);
    assert!(matches!(refuse(&x), Why::DieOutside { die: 0, offset: 0xFFF0 }));

    // A huge IP count.
    let mut x = good.clone();
    poke16(&mut x, l.die0 + 2, 0xFFFF);
    ip_fix(&mut x);
    assert_eq!(refuse(&x), Why::IpCount { die: 0, ips: 0xFFFF });
    // A count just past what is there: the entry after the last one runs into the GC table's bytes and on.
    let mut x = good.clone();
    poke16(&mut x, l.die0 + 2, 40 + 60);
    ip_fix(&mut x);
    assert!(matches!(refuse(&x), Why::IpOutside { die: 0, .. }));

    // num_base_address 255 on the last IP: past the end.
    let mut b2 = Blob::new(vec![ip(hwid::GC, 0, (11, 0, 1), &[1])]);
    b2.len = 0;
    let (mut x, l2) = build(&b2);
    x[l2.die0 + 4 + 3] = 255;
    fix_table(&mut x, 0, l2.size - l2.ip);
    assert!(matches!(refuse(&x), Why::IpOutside { die: 0, index: 0, .. }));

    // Two tables over the same bytes.
    let mut x = good.clone();
    poke16(&mut x, slot(3), l.gc as u16);
    fix(&mut x);
    assert!(matches!(refuse(&x), Why::TableSignature { table: Table::Vcn, .. }));
    let mut b3 = minipc();
    b3.gc = Some((1, 2, vec![0; 30]));
    let (mut x, l3) = build(&b3);
    // GC's declared size grows over the harvest table; its checksum is made right: only the overlap is wrong.
    let gc_len = l3.harvest + 136 - l3.gc;
    poke32(&mut x, l3.gc + 8, gc_len as u32);
    fix_table(&mut x, 1, gc_len);
    assert_eq!(refuse(&x), Why::Overlap { a: Table::Gc, b: Table::Harvest });

    let mut b4 = minipc();
    b4.vcn = Some(vec![(0, 0); 4]);
    let (mut x, l4) = build(&b4);
    poke32(&mut x, l4.vcn + 12, 5);
    fix_table(&mut x, 3, 96);
    assert_eq!(refuse(&x), Why::VcnInstances { count: 5 });
    // Every refusal has a text.
    assert!(!format!("{}", Why::IpCount { die: 0, ips: 9 }).is_empty());
}

/// Many base addresses are no reason to refuse (Linux 6.6 sets no cap; R1): 40 bases inside the binary parse.
#[test]
fn many_base_addresses_inside_the_binary_parse() {
    let bases: Vec<u64> = (0..40).collect();
    let d = parse(&build(&Blob::new(vec![ip(hwid::DF, 0, (4, 0, 0), &bases)])).0).unwrap();
    assert_eq!(d.ips[0].bases.len(), 40);
}

/// Random damage to a valid blob (bytes flipped, binary checksum made right again so the parser looks deeper;
/// random truncation) never panics: it parses or is refused.
#[test]
fn random_damage_never_panics() {
    let (good, l) = build(&minipc());
    let mut seed = 0x2545_F491_4F6C_DD1Du64;
    let mut next = || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    let (mut ok, mut refused) = (0, 0);
    for round in 0..4000 {
        let mut x = good.clone();
        let flips = 1 + next() % 4;
        for _ in 0..flips {
            let at = (next() as usize) % l.size;
            x[at] = next() as u8;
        }
        if round % 2 == 0 {
            fix(&mut x);
        }
        if round % 7 == 0 {
            x.truncate((next() as usize) % x.len());
        }
        match parse(&x) {
            Ok(_) => ok += 1,
            Err(_) => refused += 1,
        }
    }
    assert!(refused > 0 && ok + refused == 4000);
}

/// The real binary from the Mini-PC (gx_probe hex=true, desk), compared with Linux's sysfs view. Skipped until the
/// desk adds the file.
#[test]
fn minipc_real_blob() {
    let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../host/tests/gx/fixtures/minipc-780m-discovery.bin");
    let Ok(buf) = std::fs::read(&p) else {
        std::eprintln!("skip: {} not there yet (the desk adds it from gx_probe hex=true)", p.display());
        return;
    };
    let d = parse(&buf).unwrap_or_else(|w| panic!("real blob refused: {w}"));
    let want = minipc_ips();
    assert_eq!(d.ips.len(), want.len());
    for w in &want {
        let e = d.find(w.hw_id, w.inst).unwrap_or_else(|| panic!("hw_id {} inst {} missing", w.hw_id, w.inst));
        assert_eq!(e.version, v(w.ver.0, w.ver.1, w.ver.2), "hw_id {}", w.hw_id);
        assert_eq!(e.bases, w.bases.iter().map(|&a| a as u32).collect::<Vec<_>>(), "hw_id {}", w.hw_id);
    }
}

/// A fake GPU behind the index/data window: MM_INDEX/MM_INDEX_HI select a dword of `vram`.
struct FakeWindow {
    vram: Vec<u32>,
    index: Cell<u32>,
    hi: Cell<u32>,
    writes: RefCell<Vec<(u32, u32)>>,
    locks: Cell<u32>,
    fail_data_at: Option<u64>,
    busy: bool,
}

impl FakeWindow {
    fn new(dwords: usize) -> FakeWindow {
        FakeWindow {
            vram: (0..dwords as u32).map(|i| i.wrapping_mul(0x9E37_79B9)).collect(),
            index: Cell::new(0x1234),
            hi: Cell::new(0x5),
            writes: RefCell::new(Vec::new()),
            locks: Cell::new(0),
            fail_data_at: None,
            busy: false,
        }
    }
    fn pos(&self) -> u64 {
        u64::from(self.hi.get()) << 31 | u64::from(self.index.get() & 0x7FFF_FFFF)
    }
}

impl IndexRegs for FakeWindow {
    fn read(&self, byte: u32) -> Option<u32> {
        match byte {
            MM_INDEX => Some(self.index.get()),
            MM_INDEX_HI => Some(self.hi.get()),
            MM_DATA => {
                if self.fail_data_at == Some(self.pos()) {
                    return None;
                }
                self.vram.get((self.pos() / 4) as usize).copied().or(Some(0))
            }
            _ => None,
        }
    }
    fn write(&self, byte: u32, value: u32) -> bool {
        self.writes.borrow_mut().push((byte, value));
        match byte {
            MM_INDEX => self.index.set(value),
            MM_INDEX_HI => self.hi.set(value),
            _ => return false,
        }
        true
    }
    fn locked<T>(&self, f: impl FnOnce(&Self) -> T) -> Option<T> {
        if self.busy {
            return None;
        }
        self.locks.set(self.locks.get() + 1);
        Some(f(self))
    }
}

#[test]
fn indirect_window_reads_and_restores() {
    let w = FakeWindow::new(4096);
    let got = vram::read_indirect(&w, 0x1000, 1024).unwrap();
    let want: Vec<u8> = w.vram[0x400..0x400 + 256].iter().flat_map(|d| d.to_le_bytes()).collect();
    assert_eq!(got, want);
    assert_eq!((w.index.get(), w.hi.get()), (0x1234, 0x5), "index registers restored");
    let chunks = 256 / vram::CHUNK_DWORDS as u32;
    assert!(vram::CHUNK_DWORDS <= 16, "interrupts off for a short chunk only (B4)");
    assert_eq!(w.locks.get(), chunks, "one lock per chunk");
    // Linux's index value: the position with bit 31 set; MM_INDEX_HI once per chunk (not per dword).
    let writes = w.writes.borrow();
    assert_eq!(writes[0], (MM_INDEX, 0x8000_1000));
    assert_eq!(writes.iter().filter(|(r, _)| *r == MM_INDEX_HI).count() as u32, chunks * 2);
    drop(writes);
    assert_eq!(vram::read_indirect(&w, 2, 4), Err(vram::VramError::Range));
    // A failed data read halfway still puts the index registers back.
    let mut w = FakeWindow::new(4096);
    w.fail_data_at = Some(0x1010);
    assert!(vram::read_indirect(&w, 0x1000, 256).is_err());
    assert_eq!((w.index.get(), w.hi.get()), (0x1234, 0x5));
    // A window held elsewhere: refused as Busy, nothing written.
    let mut w = FakeWindow::new(16);
    w.busy = true;
    assert_eq!(vram::read_indirect(&w, 0, 16), Err(vram::VramError::Busy));
    assert!(w.writes.borrow().is_empty());
    // Above 2 GB MM_INDEX_HI carries bits 63:31.
    let w = FakeWindow::new(16);
    let _ = vram::read_indirect(&w, 0x1_8000_0000, 4);
    assert!(w.writes.borrow().contains(&(MM_INDEX_HI, 3)));
}

#[test]
fn where_the_binary_lies() {
    assert_eq!(vram::discovery_pos(512), Some((512 << 20) - (64 << 10)));
    assert_eq!(vram::discovery_pos(0), None);
    // DCN_VM_FB_OFFSET 0x123 (16 MB units) with 512 MB.
    let _ = blob::slot(0);
}
