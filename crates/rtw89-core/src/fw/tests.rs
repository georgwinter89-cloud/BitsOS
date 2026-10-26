//! js/test-fw.mjs in Rust, check by check in its order: the real firmware file,
//! a made-up container, the whole download op by op against
//! __rtw89_fw_download, the H2C/FWDL bytes against the C structs, the failure
//! paths (status, path ready, WCPU on, lost connection, one bad try, CH12
//! timeout/stuck/gone), H2C headers and the start-up H2Cs, C2H and the register
//! path; then t2-fw.mjs's stage-2 scenarios on the bring-up. No node needed.
//! The JS modules themselves run the same in tests/fw_js.rs.

use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;
use core::cell::Cell;
use std::path::PathBuf;

use super::hookbus::HookBus;
use super::*;
use crate::bringup::{Bringup, RingConfig};
use crate::bus::{Bus, BusError, prep};
use crate::error::Rtw89Error;
use crate::membus::MemBus;
use crate::pci::{self, DmacState, PciRings};
use crate::tables::fw_feat::*;
use crate::tables::regs::*;
use crate::trace::{OpKind, TraceEntry};

fn wlan_dir() -> PathBuf {
    std::env::var_os("WLAN_TOOLS")
        .map(PathBuf::from)
        .unwrap_or_else(|| "/home/georg/BitsOS/werkzeuge/plattform/wlan".into())
}

/// The firmware file, read only (spec/wlan.md §2.2); missing fails the test
fn file() -> Vec<u8> {
    let p = wlan_dir().join("firmware/rtw8852b_fw-2.bin");
    std::fs::read(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
}

fn fw1() -> Firmware {
    Firmware::parse(file(), 1, false).expect("the real file")
}

fn hexs(b: &[u8]) -> String {
    b.iter().map(|x| alloc::format!("{x:02x}")).collect()
}

type O = (u8, u8, u16, u32, u32, u32);

/// test-fw.mjs `ops`: kinds 1-5 (wr, rd, rmw, poll, delay) since `from`
fn ops(b: &MemBus, from: usize) -> Vec<O> {
    b.trace[from..]
        .iter()
        .filter_map(|e| match e {
            TraceEntry::Op(o) if (1..=5).contains(&(o.kind as u8)) => {
                Some((o.kind as u8, o.size, o.ms, o.a, o.b, o.c))
            }
            _ => None,
        })
        .collect()
}

const PE: u32 = 0x88;
const FWC: u32 = 0x1e0;
const CLK: u32 = 0x8;
const IDX: u32 = R_AX_CH12_TXBD_IDX;

// ---- the real file (fwinfo.mjs lists the same) ----

#[test]
fn the_real_file_parses_as_linux_reads_it() {
    let fw = fw1();
    let s = &fw.suit;
    let e = s.entry.unwrap();
    assert_eq!(
        (s.typ, e.cv, e.mp, s.size, e.shift),
        (RTW89_FW_NORMAL_CE, 1, 0, 326_728, 53_072),
        "NORMAL_CE cv 1 entry"
    );
    assert!(
        fw.version_text()
            .starts_with("Firmware version 0.29.29.18 (9e3d7"),
        "{}",
        fw.version_text()
    );
    let i = &s.info;
    assert_eq!(
        (
            s.hdr_ver,
            i.hdr_len,
            i.base_hdr_len(),
            i.dynamic_hdr_len,
            i.section_num,
            i.part_size
        ),
        (0, 144, 80, 64, 3, 2020),
        "header"
    );
    let secs: Vec<_> = i
        .sections
        .iter()
        .map(|x| (x.len, x.typ, x.mssc, x.checksum, x.offset))
        .collect();
    assert_eq!(
        secs,
        vec![
            (311_104, 2, 0, false, 144),
            (13_432, 1, 0, false, 311_248),
            (2048, 9, 0, false, 324_680)
        ],
        "sections"
    );
    let dl: Vec<u32> = i.sections.iter().map(|x| x.dl_addr).collect();
    assert_eq!(
        dl,
        vec![
            0xb897_0000 & 0x1fff_ffff,
            0xb8e1_2400 & 0x1fff_ffff,
            0xb8e1_1928 & 0x1fff_ffff
        ],
        "download addresses"
    );
    let pieces: Vec<Vec<Piece>> = i
        .sections
        .iter()
        .map(|x| section_pieces(x, i.part_size).collect())
        .collect();
    assert_eq!(
        pieces.iter().map(Vec::len).collect::<Vec<_>>(),
        vec![155, 7, 2],
        "piece counts"
    );
    assert!(
        pieces
            .iter()
            .all(|p| p[..p.len() - 1].iter().all(|x| x.len == 2020)),
        "pieces of 2020 bytes"
    );
    let lasts: Vec<usize> = pieces.iter().map(|p| p[p.len() - 1].len).collect();
    assert_eq!(
        lasts,
        vec![311_104 - 154 * 2020, 13_432 - 6 * 2020, 28],
        "last piece sizes"
    );
    let e0 = &fw.elements[0];
    assert_eq!(
        (
            fw.elements.len(),
            e0.name,
            e0.size,
            fw.payload(e0).len(),
            fw.payload(e0)[0]
        ),
        (28, "BB_REG", 8104, 8112, 0),
        "elements"
    );
    let rb: Vec<u8> = fw
        .elements
        .iter()
        .filter(|e| e.name == "RADIO_B")
        .map(|e| fw.payload(e)[0])
        .collect();
    assert_eq!(rb, vec![1], "RADIO_B idx 1");
    let last = fw.elements.last().unwrap().clone();
    assert_eq!(last.name, "REGD", "last element");
    assert!(fw.missing_elements.is_empty(), "no needed element missing");
    let cut = file()[..last.offset + last.size as usize].to_vec();
    assert_eq!(
        Firmware::parse(cut, 1, false).unwrap().elements.len(),
        27,
        "element running past the end dropped (fw.c:1588)"
    );
    let mut names = fw.feat_names();
    names.sort_unstable();
    assert_eq!(
        names,
        vec![
            "BEACON_FILTER",
            "BEACON_LOSS_COUNT_V1",
            "CRASH_TRIGGER_TYPE_0",
            "NO_LPS_PG",
            "NO_WOW_CPU_IO_RX",
            "SCAN_OFFLOAD",
            "TX_WAKE"
        ],
        "features of 0.29.29.18"
    );
    let f = |c, bit| features_for(c).contains(&bit);
    assert_eq!(
        (
            f(ver_code(0, 29, 29, 6), RTW89_FW_FEATURE_BEACON_FILTER),
            f(ver_code(0, 29, 29, 7), RTW89_FW_FEATURE_BEACON_FILTER)
        ),
        (false, true),
        "ge boundary"
    );
    assert_eq!(
        (
            f(ver_code(0, 29, 29, 255), RTW89_FW_FEATURE_NO_WOW_CPU_IO_RX),
            f(ver_code(0, 29, 30, 0), RTW89_FW_FEATURE_NO_WOW_CPU_IO_RX)
        ),
        (true, false),
        "lt boundary"
    );
    assert_eq!(
        fw.suit.ver.code, 0x001d_1d12,
        "the code btc's version check needs"
    );
    assert_eq!(
        Firmware::parse(file(), 2, false)
            .unwrap()
            .suit
            .entry
            .unwrap()
            .shift,
        919_688,
        "cv 2 takes the cv 2 entry"
    );
    assert!(
        matches!(
            Firmware::parse(file(), 0, false),
            Err(Rtw89Error::NoFirmware { cv: 0, .. })
        ),
        "cv 0: no entry"
    );
    assert_eq!(
        Firmware::parse(file(), 1, true),
        Err(Rtw89Error::FwSecureBoot),
        "secure boot refused"
    );
    // a legacy file (no container) holds the normal type only, and Linux finds no elements in it
    let legacy = file()[53_072..53_072 + 326_728].to_vec();
    assert_eq!(
        mfw_recognize(&legacy, RTW89_FW_NORMAL, 1).unwrap().size,
        326_728,
        "legacy: NORMAL is the whole file"
    );
    assert!(
        mfw_recognize(&legacy, RTW89_FW_NORMAL_CE, 1).is_err(),
        "legacy: no NORMAL_CE"
    );
    assert_eq!(
        Firmware::parse(legacy, 1, false),
        Err(Rtw89Error::FwFile {
            what: "not mfw format",
            value: 0
        }),
        "legacy: element list -EINVAL"
    );
}

// ---- a made-up container: checksum bytes, an mp entry, signatures after a security section ----

#[derive(Default)]
struct Synth {
    mssc: u32,
    formatted: bool,
    truncate: usize,
    part: u32,
    dyn_: usize,
    dyn_len: Option<u32>,
}

fn put(b: &mut [u8], at: usize, v: u32) {
    b[at..at + 4].copy_from_slice(&v.to_le_bytes());
}

/// test-fw.mjs `synth`
fn synth(o: Synth) -> Vec<u8> {
    let part = if o.part == 0 { 2020 } else { o.part };
    let mssc = if o.mssc == 0 && !o.formatted {
        1
    } else {
        o.mssc
    };
    let dyn_ = o.dyn_;
    let mut hdr = vec![0u8; 64 + dyn_];
    put(&mut hdr, 4, 0x1d1d_0000); // version 0.0.29.29 (w1)
    put(&mut hdr, 12, prep(FW_HDR_W3_LEN, 64 + dyn_ as u32)); // w3: header version 0, length
    put(&mut hdr, 24, 2 << 8); // w6: 2 sections
    put(
        &mut hdr,
        28,
        (0x5a << 24) | if dyn_ != 0 { FW_HDR_W7_DYN_HDR } else { 0 } | part,
    );
    if dyn_ != 0 {
        put(&mut hdr, 64, o.dyn_len.unwrap_or(dyn_ as u32));
    }
    put(&mut hdr, 32, 0x1890_0000);
    put(&mut hdr, 36, (2 << 24) | (1 << 28) | 3000);
    put(&mut hdr, 48, 0x18e0_0000);
    put(&mut hdr, 52, (9 << 24) | 100);
    put(&mut hdr, 56, if o.formatted { 0xff } else { mssc });
    let sec1 = vec![0x11u8; 3008];
    let mut sec2 = vec![0x22u8; 100];
    let mut sig = vec![0x33u8; mssc as usize * 512];
    if o.formatted {
        // struct rtw89_fw_mss_pool_hdr + 1 table byte + 2 keys of 512
        sig = vec![0x44u8; 32 + 1 + 2 * 512];
        sig[..8].copy_from_slice(b"MSSKPOOL");
        put(&mut sig, 12, 33);
        sig[16] = 0;
        sig[20] = 0;
        sig[21] = 1;
        sig[22..24].copy_from_slice(&2u16.to_le_bytes());
        sig[24..26].copy_from_slice(&1u16.to_le_bytes());
        sig[26..28].copy_from_slice(&8u16.to_le_bytes());
        sec2[60..62].copy_from_slice(&0u16.to_le_bytes()); // key_sign_len 0: 512
    }
    let mut suit = [hdr, sec1, sec2, sig].concat();
    suit.truncate(suit.len() - o.truncate);
    let mut mfw = vec![0u8; 16 + 2 * 16];
    mfw[0] = 0xff;
    mfw[1] = 2;
    for (i, mp) in [(0usize, 1u8), (1, 0)] {
        let at = 16 + i * 16;
        mfw[at] = 1;
        mfw[at + 1] = 5;
        mfw[at + 2] = mp;
        put(&mut mfw, at + 4, 48);
        put(&mut mfw, at + 8, suit.len() as u32);
    }
    [mfw, suit].concat()
}

#[test]
fn a_made_up_container_parses_and_tweaks_as_linux() {
    let sy = Firmware::parse(synth(Synth::default()), 1, false).unwrap();
    let secs: Vec<_> = sy
        .suit
        .info
        .sections
        .iter()
        .map(|x| (x.len, x.offset, x.mssc, x.checksum))
        .collect();
    assert_eq!(
        (sy.suit.entry.unwrap().mp, secs),
        (0, vec![(3008, 64, 0, true), (100, 3072, 1, false)]),
        "synthetic: checksum +8, mp skipped, mssc 1"
    );
    let lens: Vec<Vec<usize>> = sy
        .suit
        .info
        .sections
        .iter()
        .map(|x| section_pieces(x, 2020).map(|p| p.len).collect())
        .collect();
    assert_eq!(
        lens,
        vec![vec![2020, 988], vec![100]],
        "synthetic pieces, signature not sent"
    );
    let fm = Firmware::parse(
        synth(Synth {
            formatted: true,
            ..Synth::default()
        }),
        1,
        false,
    )
    .unwrap();
    assert_eq!(
        fm.suit.info.sections[1].mssc_len, 1057,
        "formatted MSSC length"
    );
    assert!(
        matches!(
            Firmware::parse(
                synth(Synth {
                    truncate: 4,
                    ..Synth::default()
                }),
                1,
                false
            ),
            Err(Rtw89Error::FwFile {
                what: "fw bin size",
                ..
            })
        ),
        "size check"
    );
    assert!(
        matches!(
            Firmware::parse(
                synth(Synth {
                    dyn_: 16,
                    dyn_len: Some(12),
                    ..Synth::default()
                }),
                1,
                false
            ),
            Err(Rtw89Error::FwFile {
                what: "invalid fw dynamic header len",
                ..
            })
        ),
        "dynamic header length check"
    );
    // __rtw89_fw_download_tweak_hdr_v0: PART_SIZE replaced in the H2C copy, the other w7 bits
    // kept; len_override in w1
    let mut sd = Firmware::parse(
        synth(Synth {
            part: 0x100,
            dyn_: 16,
            ..Synth::default()
        }),
        1,
        false,
    )
    .unwrap();
    assert_eq!(
        (
            sd.suit.info.hdr_len,
            sd.suit.info.dynamic_hdr_len,
            sd.suit.info.sections[0].offset
        ),
        (80, 16, 80),
        "synthetic dynamic header"
    );
    sd.suit.info.sections[1].len_override = 60;
    let hh = fw_hdr_h2c(&FwState::new(), &sd).unwrap();
    let w = |at: usize| u32::from_le_bytes([hh[at], hh[at + 1], hh[at + 2], hh[at + 3]]);
    assert_eq!(
        (hh.len(), w(4), w(8 + 28), w(8 + 52)),
        (
            8 + 64,
            8 + 64,
            (0x5a << 24) | FW_HDR_W7_DYN_HDR | 2020,
            (9 << 24) | 60
        ),
        "header H2C: part size and len_override"
    );
    let d = sd.suit_data();
    let dw = |at: usize| u32::from_le_bytes([d[at], d[at + 1], d[at + 2], d[at + 3]]);
    assert_eq!(
        (dw(28) & 0xffff, dw(52) & 0xff_ffff),
        (0x100, 100),
        "the file itself untouched"
    );
    let l = |f: &Firmware| -> Vec<usize> {
        section_pieces(&f.suit.info.sections[1], 2020)
            .map(|p| p.len)
            .collect()
    };
    assert_eq!(l(&sd), vec![60], "len_override cuts the section");
    sd.suit.info.sections[1].len_override = 200;
    assert_eq!(
        l(&sd),
        vec![100],
        "len_override above len: ignored (fw.c:1825)"
    );
}

#[test]
fn hostile_files_are_errors_never_panics() {
    let real = file();
    // every cut of the container's head, and a few through the suit
    for n in (0..200).chain([53_072, 53_100, 53_200, 380_000, 380_001]) {
        let _ = Firmware::parse(real[..n.min(real.len())].to_vec(), 1, false);
    }
    // bytes flipped in the MFW header, the suit header and the element area
    for at in (0..64)
        .chain(53_072..53_072 + 160)
        .chain([380_000, 380_004, 380_008])
    {
        for v in [0x00, 0xff, 0x7f, 0x80] {
            let mut f = real.clone();
            if at < f.len() {
                f[at] = v;
            }
            let _ = Firmware::parse(f, 1, false);
        }
    }
    // the made-up container cut everywhere, and with a formatted pool cut short
    let s = synth(Synth {
        formatted: true,
        ..Synth::default()
    });
    for n in 0..s.len() {
        let _ = Firmware::parse(s[..n].to_vec(), 1, false);
    }
    // an element whose size reaches just short of the end: its payload is cut, not read past
    let mut f = real.clone();
    let fw = Firmware::parse(real, 1, false).unwrap();
    let last = fw.elements.last().unwrap();
    let size = (f.len() - last.offset - 1) as u32;
    put(&mut f, last.offset + 4, size);
    let g = Firmware::parse(f, 1, false).unwrap();
    let l = g.elements.last().unwrap();
    assert_eq!(
        g.payload(l).len(),
        g.file().len() - l.offset - 24,
        "payload cut at the file's end"
    );
    assert_eq!(
        Firmware::parse(
            vec![0xff, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
            1,
            false
        ),
        Err(Rtw89Error::FwFile {
            what: "mfw header has no fw entry",
            value: 0
        })
    );
}

// ---- a device ready for the download (scaffold steps as rtw89_mac_partial_init) ----

struct Ready<'a> {
    b: HookBus<'a>,
    rings: PciRings,
    st: FwState,
    mark: usize,
}

fn ready<'a>() -> Ready<'a> {
    let mut m = MemBus::new();
    m.cfg.insert(0x80, 0x1011_0000);
    let mut b = HookBus::new(m);
    b.set32(R_AX_DMAC_FUNC_EN, B_AX_MAC_FUNC_EN | B_AX_DMAC_FUNC_EN)
        .unwrap(); // as after pwrOn
    pci::hci_dma_trx(&mut b, true).unwrap();
    pci::dmac_pre_init(&mut b, &mut DmacState::default()).unwrap();
    let mut rings = pci::pci_alloc(&mut b, &RingConfig::js()).unwrap();
    pci::pre_init(&mut b, &mut rings).unwrap();
    b.writes.clear();
    let mark = b.mem.trace.len();
    Ready {
        b,
        rings,
        st: FwState::new(),
        mark,
    }
}

impl Ready<'_> {
    fn ops(&self) -> Vec<O> {
        ops(&self.b.mem, self.mark)
    }
    fn download(&mut self, fw: &Firmware) -> Result<(), Rtw89Error> {
        download(&mut self.b, &mut self.rings, fw, &mut self.st)
    }
    fn count(&self, kind: u8, a: u32) -> usize {
        self.ops()
            .iter()
            .filter(|o| o.0 == kind && o.3 == a)
            .count()
    }
}

fn want_download() -> Vec<O> {
    let mut want: Vec<O> = vec![
        // rtw89_mac_disable_cpu_ax (mac.c:4150-4158) with rtw89_disable_fw_watchdog (mac.c:4131-4133)
        (3, 4, 0, PE, 0x2, 0),
        (3, 4, 0, FWC, 0x7, 0),
        (3, 4, 0, CLK, 0x4000, 0),
        (3, 4, 0, PE, 0x4, 0),
        (3, 4, 0, PE, 0, 0x4),
        (3, 4, 0, PE, 0x1, 0),
        (3, 4, 0, PE, 0, 0x1),
        // rtw89_mac_enable_cpu_ax (mac.c:4167-4195): read, UDM1/2, HALT x4, CPU clock, FW_CTRL,
        // SEC_CTRL, BOOT_REASON, WCPU_EN
        (2, 4, 0, PE, 0, 0),
        (1, 4, 0, 0x1f4, 0, 0),
        (1, 4, 0, 0x1f8, 0, 0),
        (1, 4, 0, 0x160, 0, 0),
        (1, 4, 0, 0x164, 0, 0),
        (1, 4, 0, 0x168, 0, 0),
        (1, 4, 0, 0x16c, 0, 0),
        (3, 4, 0, CLK, 0, 0x4000),
        (3, 4, 0, FWC, 0xe7, 0x1),
        (3, 4, 0, 0xc00, 0x30000, 0x20000),
        (3, 2, 0, 0x1e6, 0x7, 0),
        (3, 4, 0, PE, 0, 0x2),
        // rtw89_fw_download_suit: H2C path ready (mac.c:7431, FWDL_WAIT_CNT 400 ms)
        (4, 1, 400, FWC, 0x2, 0x2),
        // header H2C on CH12: kick-off (pci.c:1342) and BitsOS's wait for the chip's index
        (1, 2, 0, IDX, 0, 1),
        (4, 4, 50, IDX, 0x0fff_0000, 1 << 16),
        // rtw89_fw_download_hdr: FWDL path ready, HALT_H2C_CTRL/HALT_C2H_CTRL = 0 (fw.c:1798-1805)
        (4, 1, 400, FWC, 0x4, 0x4),
        (1, 4, 0, 0x160, 0, 0),
        (1, 4, 0, 0x164, 0, 0),
    ];
    for i in 0..164u32 {
        let wp = (i + 2) % 16;
        want.push((1, 2, 0, IDX, 0, wp));
        want.push((4, 4, 50, IDX, 0x0fff_0000, wp << 16));
    }
    // mdelay(5), rtw89_fw_check_rdy
    want.push((5, 0, 0, 5000, 0, 0));
    want.push((4, 1, 400, FWC, 0xe0, 0xe0));
    want
}

#[test]
fn the_whole_download_in_linux_order_with_the_bytes_of_the_c_structs() {
    let fw = fw1();
    let mut d = ready();
    d.download(&fw).unwrap();
    assert!(
        d.st.ready && d.st.tries == 1 && d.st.pieces == 164,
        "download passed in one try"
    );
    assert!(d.st.events().is_empty());
    assert_eq!(
        d.ops(),
        want_download(),
        "register order of __rtw89_fw_download"
    );

    // ---- header H2C and first piece bytes against the C structs ----
    let f = file();
    let (hdr_w, piece0_w) = (&d.b.writes[0], &d.b.writes[1]);
    let mut wd_h2c = vec![0u8; 24];
    put(&mut wd_h2c, 0, 0x000c_0000); // CHANNEL_DMA 12
    put(&mut wd_h2c, 8, 88); // TXPKT_SIZE
    let mut fwdl_hdr = vec![0u8; 8];
    put(&mut fwdl_hdr, 0, 0x0000_000d); // CAT 1, CLASS 3, FUNC 0
    put(&mut fwdl_hdr, 4, 88); // len 80 + 8
    assert_eq!(
        hdr_w.1,
        [wd_h2c.clone(), fwdl_hdr, f[53_072..53_072 + 80].to_vec()].concat(),
        "header H2C bytes"
    );
    assert_eq!(
        hdr_w.1,
        [wd_h2c, fw_hdr_h2c(&FwState::new(), &fw).unwrap()].concat(),
        "fw_hdr_h2c gives the same"
    );
    let mut wd_dl = vec![0u8; 24];
    put(&mut wd_dl, 0, 0x001c_0000); // + FW_DL (BIT 20)
    put(&mut wd_dl, 8, 2020);
    assert_eq!(
        piece0_w.1,
        [wd_dl, f[53_072 + 144..53_072 + 144 + 2020].to_vec()].concat(),
        "first FWDL piece bytes"
    );
    assert_eq!(
        d.b.writes.last().unwrap().1.len(),
        24 + 28,
        "last piece 28 bytes"
    );
    let sent: usize = d.b.writes[1..].iter().map(|w| w.1.len() - 24).sum();
    assert_eq!(sent, 311_104 + 13_432 + 2048, "all section bytes sent");
    assert!(
        d.st.h2c_seq == 0
            && fw.has_feat_name("SCAN_OFFLOAD")
            && !fw.has_feat_name("BEACON_TRACKING"),
        "hasFeat"
    );
    // test-fw.mjs's "largest script" check is about MCP's 1 MB script limit: no counterpart here
}

// ---- failures: status 2 at the end, retries and dumps; H2C path; WCPU still on; lost connection ----

#[test]
fn status_2_every_try_five_tries_with_dumps() {
    let fw = fw1();
    let mut bad = ready();
    bad.b.on_poll(|a, m| Ok(!(a == FWC && m == 0xe0)));
    bad.b.on_read(|a| (a == FWC).then_some(0x40));
    let err = bad.download(&fw).unwrap_err();
    assert!(
        matches!(
            err,
            Rtw89Error::FwStatus {
                status: 2,
                reg: 0x1e0,
                value: 0x40
            }
        ),
        "status 2: checksum fail: {err:?}"
    );
    assert_eq!(err.fw_status(), Some(2));
    assert_eq!(download::fwdl_status_text(2), Some("fw checksum fail"));
    assert_eq!(
        bad.count(2, R_AX_DBG_PORT_SEL),
        75,
        "5 tries, 15 PC reads each"
    );
    let ev = bad.st.take_events();
    assert_eq!(
        ev.iter()
            .filter(|e| matches!(e, FwEvent::TryFailed { .. }))
            .count(),
        5,
        "five failed tries logged"
    );
    assert!(
        ev.iter()
            .any(|e| alloc::format!("{e}").starts_with("[ERR]fwdl 0x1E0 = 0x00000040"))
    );
    assert!(!bad.st.ready);
}

#[test]
fn h2c_path_never_ready_five_tries() {
    let fw = fw1();
    let mut np = ready();
    np.b.on_poll(|a, m| Ok(!(a == FWC && m == 0x2)));
    let err = np.download(&fw).unwrap_err();
    assert!(
        matches!(
            err,
            Rtw89Error::FwPathNotReady {
                path: "H2C",
                reg: 0x1e0,
                ..
            }
        ),
        "{err:?}"
    );
    assert_eq!(np.count(2, PE), 5, "five tries");
}

#[test]
fn wcpu_already_on_is_retried_without_a_dump() {
    let fw = fw1();
    let mut on = ready();
    on.b.on_read(|a| (a == PE).then_some(0x3));
    let err = on.download(&fw).unwrap_err();
    assert_eq!(
        err,
        Rtw89Error::WcpuAlreadyOn { reg: PE, value: 3 },
        "WCPU already on"
    );
    assert_eq!(
        on.count(2, R_AX_DBG_PORT_SEL),
        0,
        "no dump after enable_cpu fails"
    );
    assert_eq!(on.count(2, PE), 5, "five tries");
}

#[test]
fn a_lost_connection_is_not_retried() {
    let fw = fw1();
    let mut lost = ready();
    lost.b.on_poll(|a, m| {
        if a == FWC && m == 0x2 {
            Err(BusError::Gone)
        } else {
            Ok(true)
        }
    });
    let err = lost.download(&fw).unwrap_err();
    assert_eq!(
        err,
        Rtw89Error::Bus(BusError::Gone),
        "a lost connection is no firmware error"
    );
    assert_eq!(lost.count(2, PE), 1, "and is not retried");
    assert_eq!(lost.count(2, R_AX_DBG_PORT_SEL), 0, "nor dumped");
}

#[test]
fn one_bad_try_then_a_good_one_dump_and_resync_in_order() {
    let fw = fw1();
    let rdy_polls = Cell::new(0);
    let pc_read = Cell::new(0u32);
    let mut once = ready();
    once.b.on_poll(|a, m| {
        let first = a == FWC && m == 0xe0 && rdy_polls.get() == 0;
        if a == FWC && m == 0xe0 {
            rdy_polls.set(rdy_polls.get() + 1);
        }
        Ok(!first)
    });
    once.b.on_read(|a| match a {
        FWC => Some(0x1234_0040),
        R_AX_BOOT_DBG => Some(0xb007_0000),
        R_AX_DBG_PORT_SEL => {
            let v = 0x1000 + pc_read.get();
            pc_read.set(pc_read.get() + 1);
            Some(v)
        }
        _ => None,
    });
    once.download(&fw).unwrap();
    assert!(once.st.ready && once.st.tries == 2, "second try passes");
    assert_eq!(
        once.st.fail_dump,
        Some(FailDump {
            fw_ctrl: 0x1234_0040,
            boot_dbg: 0xb007_0000,
            pc: (0..15).map(|i| 0x1000 + i).collect()
        }),
        "fail dump values"
    );
    let o1 = once.ops();
    let at = o1
        .iter()
        .position(|o| o.0 == 4 && o.3 == FWC && o.4 == 0xe0)
        .unwrap();
    let mut dump: Vec<O> = vec![
        (2, 1, 0, FWC, 0, 0), // the status for the message
        (2, 4, 0, FWC, 0, 0),
        (2, 4, 0, R_AX_BOOT_DBG, 0, 0),
        (1, 4, 0, R_AX_DBG_CTRL, 0, 0x00f2_00f2),
        (3, 4, 0, R_AX_SYS_STATUS1, 0x30000, 0x10000),
    ];
    for _ in 0..15 {
        dump.push((2, 4, 0, R_AX_DBG_PORT_SEL, 0, 0));
        dump.push((5, 0, 0, 10, 0, 0));
    }
    // CH12 resync, next try
    dump.push((2, 4, 0, IDX, 0, 0));
    dump.push((4, 4, 50, IDX, 0x0fff_0000, 5 << 16));
    dump.push((3, 4, 0, PE, 0x2, 0));
    assert_eq!(
        &o1[at + 1..at + 1 + dump.len()],
        &dump[..],
        "fail dump, CH12 resync and the next try in order"
    );
}

// ---- a CH12 index timeout in the middle of a batch: the next try continues from the chip's index ----

#[test]
fn a_ch12_timeout_retries_from_the_chips_index() {
    let fw = fw1();
    let idx_polls = Cell::new(0);
    let mut slow = ready();
    slow.b.on_poll(|a, _| {
        if a == IDX {
            idx_polls.set(idx_polls.get() + 1);
        }
        Ok(!(a == IDX && idx_polls.get() == 30))
    });
    slow.download(&fw).unwrap();
    let so = slow.ops();
    let mut seen = 0;
    let cut = so
        .iter()
        .position(|o| {
            if o.0 == 4 && o.3 == IDX {
                seen += 1;
            }
            o.0 == 4 && o.3 == IDX && seen == 30
        })
        .unwrap();
    let kicks = |r: &[O]| -> Vec<u32> {
        r.iter()
            .filter(|o| o.0 == 1 && o.3 == IDX)
            .map(|o| o.5)
            .collect()
    };
    assert_eq!(
        (
            slow.st.tries,
            *kicks(&so[..cut]).last().unwrap(),
            kicks(&so[cut..])[0]
        ),
        (2, 14, 15),
        "retry kicks the slot after the last one the chip saw"
    );
}

#[test]
fn a_stuck_ch12_ends_the_tries() {
    let fw = fw1();
    let idx_polls = Cell::new(0);
    let mut stuck = ready();
    stuck.b.on_poll(|a, _| {
        if a == IDX {
            idx_polls.set(idx_polls.get() + 1);
        }
        Ok(!(a == IDX && idx_polls.get() >= 30))
    });
    let err = stuck.download(&fw).unwrap_err();
    assert!(
        matches!(&err, Rtw89Error::Ch12Stuck { reg, host: 14, before, .. }
            if *reg == IDX && matches!(**before, Rtw89Error::Bus(BusError::PollTimeout { addr, .. }) if addr == IDX)),
        "CH12 stuck: {err:?}"
    );
    assert_eq!(stuck.count(2, PE), 1, "no further try with a stuck CH12");
}

#[test]
fn a_ch12_index_of_all_ones_is_link_gone() {
    let fw = fw1();
    let mut gone = ready();
    gone.b.on_poll(|a, m| Ok(!(a == FWC && m == 0x2)));
    gone.b.on_read(|a| (a == IDX).then_some(0xffff_ffff));
    let err = gone.download(&fw).unwrap_err();
    assert!(
        matches!(&err, Rtw89Error::FwLinkGone { reg, value: 0xffff_ffff, before }
            if *reg == IDX && matches!(**before, Rtw89Error::FwPathNotReady { .. })),
        "CH12 index all ones: no further try: {err:?}"
    );
    assert_eq!(gone.count(2, PE), 1);
}

// ---- the start-up H2Cs after the download ----

#[test]
fn early_h2c_and_ofld_cfg_after_the_download() {
    let fw = fw1();
    let mut st = ready();
    st.download(&fw).unwrap();
    send_all_early_h2c(&mut st.b, &mut st.rings, &st.st).unwrap();
    h2c_set_ofld_cfg(&mut st.b, &mut st.rings, &mut st.st).unwrap();
    assert_eq!(
        (
            st.st.ready,
            st.st.h2c_seq,
            st.st.early_h2c.len(),
            fw.has_feat(RTW89_FW_FEATURE_SCAN_OFFLOAD)
        ),
        (true, 1, 0, true),
        "state after the download and one H2C"
    );
    st.st.early_h2c.push(vec![0x01, 0, 0, 0x01, 0x08, 0, 0, 0]); // rtw89_fw_h2c_raw: sent as given
    send_all_early_h2c(&mut st.b, &mut st.rings, &st.st).unwrap();
    assert_eq!(
        &st.b.writes.last().unwrap().1[24..],
        &st.st.early_h2c[0][..],
        "early H2C kicked as given"
    );
}

#[test]
fn h2c_headers_and_the_start_up_h2cs_byte_by_byte() {
    let mut h = FwState::new();
    let hd = |b: [u8; 8]| {
        (
            u32::from_le_bytes([b[0], b[1], b[2], b[3]]),
            u32::from_le_bytes([b[4], b[5], b[6], b[7]]),
        )
    };
    let a = H2c {
        cat: 1,
        cls: 9,
        func: 0x14,
        rack: false,
        dack: true,
    };
    assert_eq!(
        hd(h2c_hdr(&mut h, FWCMD_TYPE_H2C, &a, 8)),
        (0x1425, 16 | 0x4000 | 0x8000),
        "seq 0: rack forced"
    );
    let b = H2c {
        cat: 2,
        cls: 8,
        func: 1,
        ..H2c::default()
    };
    assert_eq!(
        hd(h2c_hdr(&mut h, FWCMD_TYPE_H2C, &b, 2000)),
        (0x0100_0122, 2008),
        "seq 1: no rack"
    );
    h.h2c_seq = 255;
    h2c_hdr(
        &mut h,
        FWCMD_TYPE_H2C,
        &H2c {
            cat: 1,
            ..H2c::default()
        },
        0,
    );
    assert_eq!(h.h2c_seq, 0, "u8 sequence wraps");

    let mut hs = ready();
    h2c_set_ofld_cfg(&mut hs.b, &mut hs.rings, &mut hs.st).unwrap();
    assert!(
        hs.ops().iter().any(|o| o.0 == 1 && o.3 == IDX),
        "H2C kicked when the call returns"
    );
    h2c_fw_log(&mut hs.b, &mut hs.rings, &mut hs.st, false).unwrap();
    assert_eq!(
        hexs(&hs.b.writes[0].1[24..]),
        "25140000".to_string() + "10c00000" + "090000005e000000",
        "ofld cfg H2C (fw.c:5322), seq 0 with receive ack"
    );
    assert_eq!(
        hexs(&hs.b.writes[1].1[24..]),
        "01000001".to_string() + "14000000" + "040200000000000000000000",
        "fw log off H2C"
    );
    let mut info = RfRegInfo::new(RF_PATH_B);
    assert!(info.store(0x10000, 0x12345) && info.store(0x1001f, 0xfffff));
    assert!(
        rf_reg_upload(&mut hs.b, &mut hs.rings, &mut hs.st, &mut info).unwrap(),
        "rf upload"
    );
    assert_eq!(
        hexs(&hs.b.writes[2].1[24..]),
        "26000002".to_string() + "10000000" + "45230100" + "ffffff01",
        "RF reg H2C path B, page 0"
    );
    assert_eq!(info.curr_idx, 0, "rf store emptied");
    // phy builds the same struct (rtw89_phy_config_rf_h2c pages)
    let mut phy_info = RfRegInfo::new(RF_PATH_A);
    phy_info.h2c[1][0] = 0x1234_5678;
    h2c_rf_reg(&mut hs.b, &mut hs.rings, &mut hs.st, &phy_info, 4, 1).unwrap();
    assert_eq!(
        hexs(&hs.b.writes[3].1[24..]),
        "22010003".to_string() + "0c000000" + "78563412",
        "RF reg H2C path A (class 8), page 1"
    );
    let mut big = RfRegInfo::new(0);
    let kept = (0..1501).filter(|&i| big.store(0x100, i)).count();
    assert_eq!((big.curr_idx, kept), (1500, 1500), "1500 values at most");
    // all three pages go out; a page past the struct is refused
    let n = hs.b.writes.len();
    assert!(rf_reg_upload(&mut hs.b, &mut hs.rings, &mut hs.st, &mut big).unwrap());
    let lens: Vec<usize> = hs.b.writes[n..]
        .iter()
        .map(|w| w.1.len() - 24 - 8)
        .collect();
    assert_eq!(lens, vec![2000, 2000, 2000], "three full pages");
    assert!(h2c_rf_reg(&mut hs.b, &mut hs.rings, &mut hs.st, &big, 4, 3).is_err());
    assert!(h2c_rf_reg(&mut hs.b, &mut hs.rings, &mut hs.st, &big, 2004, 0).is_err());
    // Linux's "total len larger than" warning: nothing sent, the store emptied
    big.curr_idx = 1501;
    let n = hs.b.writes.len();
    assert!(!rf_reg_upload(&mut hs.b, &mut hs.rings, &mut hs.st, &mut big).unwrap());
    assert_eq!((hs.b.writes.len(), big.curr_idx), (n, 0));
    assert_eq!(
        hs.st.take_events(),
        vec![FwEvent::RfUploadTooLong {
            len: 1501,
            max: 1500
        }]
    );
}

// ---- C2H ----

#[test]
fn c2h_acks_and_payloads() {
    let mut c2 = vec![0u8; 12];
    put(&mut c2, 0, (1 << 8) | 1);
    put(&mut c2, 4, 12);
    put(&mut c2, 8, (5 << 24) | (0x14 << 8) | (9 << 2) | 1);
    let p = c2h_parse(&c2).unwrap();
    let a = p.ack.unwrap();
    assert_eq!(
        (
            p.cat,
            p.cls,
            p.func,
            p.len,
            p.seq,
            p.payload.len(),
            a.kind,
            a.cls,
            a.func,
            a.ret
        ),
        (1, 0, 1, 12, Some(5), 4, AckKind::Done, 9, 0x14, Some(0)),
        "done ack"
    );
    put(&mut c2, 0, 1);
    put(&mut c2, 8, (7 << 16) | (0x14 << 8) | (9 << 2) | 1);
    let r = c2h_parse(&c2).unwrap();
    assert_eq!(
        (r.ack.unwrap().kind, r.seq),
        (AckKind::Rec, Some(7)),
        "receive ack"
    );
    let longer = [c2.clone(), vec![0xee; 8]].concat();
    assert_eq!(
        c2h_parse(&longer).unwrap().payload,
        c2[8..].to_vec(),
        "payload ends at len"
    );
    put(&mut c2, 0, (1 << 2) | 1);
    let o = c2h_parse(&c2).unwrap();
    assert_eq!(
        (o.cls, o.ack, o.seq),
        (1, None, None),
        "func 0 of another class is no ack"
    );
    // not in test-fw.mjs: short buffers never panic
    assert!(c2h_parse(&c2[..7]).is_err());
    let short = c2h_parse(&c2[..9]).unwrap();
    assert_eq!(
        (short.ack, short.payload.len()),
        (None, 1),
        "no ack word: no ack"
    );
    put(&mut c2, 4, 3);
    assert!(
        c2h_parse(&c2).unwrap().payload.is_empty(),
        "len below the header: empty payload"
    );
    assert_eq!(rx_pkt(&[1, 2, 3]), None);
    let p = rx_pkt(&[0xff, 0xff, 0xff, 0xff]).unwrap();
    assert!(
        p.body.is_empty(),
        "a descriptor longer than the buffer: empty body"
    );
}

// ---- register path ----

fn trace4(b: &MemBus) -> Vec<(u8, u8, u16, u32, u32)> {
    b.trace
        .iter()
        .filter_map(|e| match e {
            TraceEntry::Op(o) if (1..=4).contains(&(o.kind as u8)) => {
                Some((o.kind as u8, o.size, o.ms, o.a, o.c))
            }
            _ => None,
        })
        .collect()
}

#[test]
fn the_register_path_in_linux_order() {
    let mut m = HookBus::new(MemBus::new());
    m.on_read(|a| match a {
        0x8164 => Some(1),
        0x8150 => Some(0x0011_0403),
        0x8154 => Some(0xabcd),
        _ => None,
    });
    let mut st = FwState::new();
    let h = H2cReg {
        id: 3,
        content_len: 0,
        w: [2 << 16, 0, 0, 0],
    };
    let c = msg_reg(&mut m, &mut st, Some(&h), Some(0))
        .unwrap()
        .unwrap();
    assert_eq!(
        (c.id, c.content_len, c.w[1]),
        (3, 14, 0xabcd),
        "C2H register answer"
    );
    assert_eq!(
        trace4(&m.mem),
        vec![
            (4, 1, 5, 0x8160, 0),
            (1, 4, 0, 0x8140, 0x0002_0103),
            (1, 4, 0, 0x8144, 0),
            (1, 4, 0, 0x8148, 0),
            (1, 4, 0, 0x814c, 0),
            (3, 1, 0, 0x1f5, 1),
            (1, 1, 0, 0x8160, 1),
            (2, 1, 0, 0x8164, 0),
            (2, 4, 0, 0x8150, 0),
            (2, 4, 0, 0x8154, 0),
            (2, 4, 0, 0x8158, 0),
            (2, 4, 0, 0x815c, 0),
            (1, 1, 0, 0x8164, 0),
            (3, 1, 0, 0x1f5, 0x10)
        ],
        "rtw89_fw_msg_reg order (fw.c:9178-9250)"
    );
    assert_eq!((st.h2c_counter, st.c2h_counter), (1, 1));
    assert_eq!(
        msg_reg(&mut m, &mut st, Some(&h), None).unwrap(),
        None,
        "no C2H asked: None"
    );
    assert!(
        msg_reg(&mut m, &mut st, None, None).is_err(),
        "nothing to do"
    );
}

#[test]
fn c2h_register_timeouts_are_bounded_even_with_a_frozen_clock() {
    let mut quiet = MemBus::new(); // C2HREG_CTRL reads 0, the clock never moves
    let mut st = FwState::new();
    let err = msg_reg(&mut quiet, &mut st, None, Some(20_000)).unwrap_err();
    assert_eq!(
        err,
        Rtw89Error::C2hRegTimeout {
            reg: R_AX_C2HREG_CTRL,
            value: 0
        },
        "C2H timeout given"
    );
    let delays = quiet
        .trace
        .iter()
        .filter(|e| matches!(e, TraceEntry::Op(o) if o.kind == OpKind::Delay))
        .count();
    assert_eq!(delays, 20_000, "1 us steps up to the 20 ms given");
    assert_eq!(
        st.take_events(),
        vec![FwEvent::C2hRegTimeout {
            reg: R_AX_C2HREG_CTRL,
            value: 0,
            timeout_us: 20_000
        }],
        "the timeout is an event for the log (B6)"
    );
    let mut quiet = MemBus::new();
    assert!(msg_reg(&mut quiet, &mut st, None, Some(0)).is_err());
    let delays = quiet
        .trace
        .iter()
        .filter(|e| matches!(e, TraceEntry::Op(o) if o.kind == OpKind::Delay))
        .count();
    assert_eq!(delays as u32, RTW89_C2H_TIMEOUT, "RTW89_C2H_TIMEOUT (1 s)");
    // an answer that comes on the last read after the deadline still counts (Linux's macro)
    let reads = Cell::new(0u32);
    let mut late = HookBus::new(MemBus::new());
    late.on_read(|a| {
        if a != R_AX_C2HREG_CTRL {
            return None;
        }
        reads.set(reads.get() + 1);
        Some(u32::from(reads.get() > 21))
    });
    assert!(
        msg_reg(&mut late, &mut st, None, Some(20_000))
            .unwrap()
            .is_some()
    );
    let mut busy = HookBus::new(MemBus::new());
    busy.on_poll(|a, _| Ok(a != 0x8160));
    let h = H2cReg {
        id: 3,
        ..H2cReg::default()
    };
    assert!(
        matches!(
            msg_reg(&mut busy, &mut st, Some(&h), Some(0)),
            Err(Rtw89Error::H2cRegBusy { reg: 0x8160, .. })
        ),
        "H2CREG busy"
    );
}

// ---- t2-fw.mjs: stage 2 on the bring-up (probe power-on, partial init with the download, look, stop) ----

/// t2-fw.mjs `fakeDev`'s answers that let every step pass (as WL-1's `chip()`)
fn t2_chip() -> MemBus {
    let mut b = MemBus::new();
    b.cfg.insert(4, 0x0010_0002);
    b.cfg.insert(0x80, 0x1011_0000);
    b.forced.insert(R_AX_SYS_CFG1, prep(B_AX_CHIP_VER_MASK, 1));
    b.forced
        .insert(R_AX_WLAN_XTAL_SI_CTRL, prep(B_AX_WL_XTAL_SI_DATA_MASK, 1));
    b.forced.insert(R_AX_EFUSE_CTRL, B_AX_EF_RDY | 0xff);
    b
}

fn golden() -> Vec<String> {
    let p = wlan_dir().join("bringup-golden.txt");
    let text = std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()));
    text.lines()
        .filter(|l| !l.is_empty())
        .map(String::from)
        .collect()
}

/// bringup-golden.txt through the firmware download and the look after it
const FW_LINES: usize = 1149;

#[test]
fn stage_fw_through_the_download_is_golden_on_the_memory_bus() {
    let mut b = t2_chip();
    let mut run = Bringup::new();
    run.probe_power_on(&mut b, &RingConfig::js()).unwrap();
    let fw = Firmware::parse(
        file(),
        run.chip.unwrap().cv,
        run.power.unwrap().secure_boot.unwrap_or(false),
    )
    .unwrap();
    run.partial_init(&mut b, &fw).unwrap();
    run.fwdl_look(&mut b).unwrap();
    let ours = b.lines();
    let gold = golden();
    assert_eq!(ours.len(), FW_LINES, "trace length");
    if let Err(m) = crate::trace::compare_prefix(&ours, &gold) {
        panic!(
            "differs from bringup-golden.txt at line {}: {:?} vs {:?}",
            m.line + 1,
            m.ours,
            m.golden
        );
    }
    assert!(run.fw.ready && run.fw.pieces == 164 && run.fw.tries == 1);
    assert!(
        run.tx.is_some(),
        "the WD pages are built after the download"
    );
    let look = run.look.as_ref().unwrap();
    assert_eq!(
        (look.bufs, look.c2h.len()),
        (0, 0),
        "RXQ empty on the memory bus"
    );
    run.stop(&mut b).unwrap();
    assert!(!run.fw.ready, "power off clears RTW89_FLAG_FW_RDY");
    let tail = &b.lines()[FW_LINES..];
    assert_eq!(tail[0], "# stop");
    assert_eq!(
        tail[1], "# not Linux: DMA off before this power-off",
        "the core never ran"
    );
}

#[test]
fn t2_a_c2h_in_rxq_after_the_download_is_seen_by_the_look() {
    let mut b = t2_chip();
    let mut run = Bringup::new();
    run.probe_power_on(&mut b, &RingConfig::js()).unwrap();
    run.partial_init(&mut b, &fw1()).unwrap();
    // t2-fw.mjs `fakeC2h`: rxbd_info, short RX descriptor, a receive ack of the FWDL class
    let mut c2h = vec![0u8; 12];
    put(
        &mut c2h,
        0,
        prep(RTW89_C2H_HDR_W0_CATEGORY, RTW89_C2H_CAT_MAC)
            | prep(RTW89_C2H_HDR_W0_FUNC, RTW89_MAC_C2H_FUNC_REC_ACK),
    );
    put(&mut c2h, 4, 12);
    put(
        &mut c2h,
        8,
        prep(RTW89_C2H_REV_ACK_W2_CAT, H2C_CAT_MAC)
            | prep(RTW89_C2H_REV_ACK_W2_CLASS, H2C_CL_MAC_FWDL),
    );
    let mut desc = vec![0u8; 16];
    put(
        &mut desc,
        0,
        prep(AX_RXD_RPKT_LEN_MASK, 12) | prep(AX_RXD_RPKT_TYPE_MASK, RTW89_CORE_RX_TYPE_C2H),
    );
    let mut info = vec![0u8; 4];
    put(
        &mut info,
        0,
        RTW89_PCI_RXBD_FS | RTW89_PCI_RXBD_LS | (4 + 16 + 12),
    );
    let buf0 = run.rings.as_ref().unwrap().rx("RXQ").unwrap().bufs[0];
    b.mem_write_bytes(buf0, &[info, desc, c2h].concat())
        .unwrap();
    b.forced
        .insert(R_AX_RXQ_RXBD_IDX, prep(TXBD_HW_IDX_MASK, 1));
    run.fwdl_look(&mut b).unwrap();
    let look = run.look.as_ref().unwrap();
    assert_eq!((look.status, look.bufs, look.other.len()), (0, 1, 0));
    let a = look.c2h[0].ack.unwrap();
    assert_eq!(
        (a.kind, a.cat, a.cls, a.seq),
        (AckKind::Rec, H2C_CAT_MAC, H2C_CL_MAC_FWDL, 0)
    );
    run.stop(&mut b).unwrap();
}

#[test]
fn t2_fail_five_failed_downloads_then_the_stop_powers_off() {
    let mut hb = HookBus::new(t2_chip());
    hb.on_poll(|a, m| Ok(!(a == FWC && m == B_AX_WCPU_FWDL_STS_MASK)));
    hb.on_read(|a| (a == FWC).then_some(prep(B_AX_WCPU_FWDL_STS_MASK, RTW89_FWDL_CHECKSUM_FAIL)));
    let mut run = Bringup::new();
    run.probe_power_on(&mut hb, &RingConfig::js()).unwrap();
    let err = run.partial_init(&mut hb, &fw1()).unwrap_err();
    assert_eq!(err.fw_status(), Some(RTW89_FWDL_CHECKSUM_FAIL));
    assert_eq!(
        run.fw
            .take_events()
            .iter()
            .filter(|e| matches!(e, FwEvent::FailDump(_)))
            .count(),
        5
    );
    assert!(run.tx.is_none() && run.pci);
    let n = hb.mem.trace.len();
    run.stop(&mut hb).unwrap();
    let tail = hb.mem.lines()[n..].to_vec();
    assert_eq!(tail[1], "# not Linux: DMA off before this power-off");
    assert!(
        tail.iter()
            .any(|l| l == "poll32 0x00000004 0x00000200 0x00000000 20 ms"),
        "power off"
    );
    assert_eq!(
        tail[tail.len() - 2],
        "cfgr 0x00000004 0x00000000 0x00000000"
    );
}

#[test]
fn pieces_are_lazy_even_for_a_tiny_part_size() {
    // a 24-bit section in 1-byte pieces: about 16M pieces, none of them held in memory
    let sec = FwSection {
        len: 0xff_ffff,
        offset: 100,
        ..FwSection::default()
    };
    let mut p = section_pieces(&sec, 1);
    assert_eq!(p.len(), 0xff_ffff, "count without a list");
    assert_eq!(
        p.next(),
        Some(Piece {
            at: 100,
            len: 1,
            key: None
        })
    );
    assert_eq!(p.len(), 0xff_fffe);
    assert_eq!(section_pieces(&sec, 0).len(), 0, "part size 0: none");
    let ignored = FwSection {
        ignore: true,
        ..sec.clone()
    };
    assert_eq!(section_pieces(&ignored, 2020).count(), 0, "ignored section");
    let last = section_pieces(&sec, 0x10_0000).last().unwrap();
    assert_eq!(
        (last.at, last.len),
        (100 + 0xf0_0000, 0xf_ffff),
        "short last piece"
    );
}
