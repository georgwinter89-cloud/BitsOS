//! js/test-phy.mjs ported check by check (no node): the real efuse dumps and the real
//! firmware file on ChipBus (test-phy.mjs's chip model), the C's register sequences where
//! test-phy.mjs compares against them, and an independent table selection for the walker.
//! tests/phy_js.rs compares everything once more against the JS driver itself.

use crate::rfk1::MacLink;
use alloc::vec;
use alloc::vec::Vec;
use std::path::PathBuf;

use super::chipbus::{ChipBus, Model, phycap_c2h};
use super::io::{BB, phy_mask32, phy_r32_mask, rf_w};
use super::*;
use crate::bus::{Width, field};
use crate::fw::{Firmware, FwState, RfRegInfo};
use crate::pci::{PciRings, RingConfig, pci_alloc};
use crate::tables::phy::*;
use crate::trace::{Op, OpKind, TraceEntry};

fn wz() -> PathBuf {
    std::env::var_os("WLAN_TOOLS")
        .map(PathBuf::from)
        .unwrap_or_else(|| "/home/georg/BitsOS/werkzeuge/plattform/wlan".into())
}
fn kit(f: &str) -> Vec<u8> {
    std::fs::read(wz().join("kit").join(f)).expect("the kit's efuse dumps")
}
fn firmware() -> Firmware {
    let f = std::fs::read(wz().join("firmware/rtw8852b_fw-2.bin")).expect("the firmware file");
    Firmware::parse(f, 1, false).unwrap()
}

fn ops(b: &ChipBus, from: usize) -> Vec<Op> {
    b.mem.trace[from..]
        .iter()
        .filter_map(|e| match e {
            TraceEntry::Op(o) => Some(*o),
            TraceEntry::Note(_) => None,
        })
        .collect()
}
/// What test-phy.mjs compares: kind, size, address, mask/bits, value (poll times are the kit's)
fn norm(v: &[Op]) -> Vec<(OpKind, u8, u32, u32, u32)> {
    v.iter().map(|o| (o.kind, o.size, o.a, o.b, o.c)).collect()
}
fn same(got: &[Op], exp: &[Op], what: &str) {
    let (g, e) = (norm(got), norm(exp));
    for i in 0..g.len().max(e.len()) {
        assert_eq!(g.get(i), e.get(i), "{what}: op {i} of {}", e.len());
    }
}
/// expectOps: a C transcription's ops on a fresh chip with `model`
fn expect(model: Model, f: impl FnOnce(&mut ChipBus)) -> Vec<Op> {
    let mut b = ChipBus::new(model);
    f(&mut b);
    ops(&b, 0)
}

#[derive(Default)]
struct Spy(Vec<(&'static str, usize)>);
impl Spy {
    fn note(&mut self, n: &'static str, b: &mut ChipBus) {
        self.0.push((n, b.mem.trace.len()));
    }
}
impl ChipOps<ChipBus> for Spy {
    fn init_txpwr_unit(&mut self, b: &mut ChipBus, _: &Phy) -> Rtw89Result<()> {
        self.note("initTxpwrUnit", b);
        Ok(())
    }
    fn rfk_init(
        &mut self,
        b: &mut ChipBus,
        _: &mut Phy,
        _: Option<MacLink<'_>>,
    ) -> Rtw89Result<()> {
        self.note("rfkInit", b);
        Ok(())
    }
    fn set_txpwr_ctrl(&mut self, b: &mut ChipBus, _: &Phy) -> Rtw89Result<()> {
        self.note("setTxpwrCtrl", b);
        Ok(())
    }
    fn power_trim(&mut self, b: &mut ChipBus, _: &Phy) -> Rtw89Result<()> {
        self.note("powerTrim", b);
        Ok(())
    }
    fn cfg_txrx_path(&mut self, b: &mut ChipBus, _: &Phy) -> Rtw89Result<()> {
        self.note("cfgTxrxPath", b);
        Ok(())
    }
}

struct Dev {
    b: ChipBus,
    rings: PciRings,
    fws: FwState,
    phy: Phy,
    maps: EfuseMaps,
}

// test-phy.mjs newDev
fn dev(fw: &Firmware, c2h: [u32; 4], rdy_fails: u32) -> Dev {
    let mut b = ChipBus::new(Model {
        c2h,
        rdy_fails,
        ..Model::default()
    });
    let rings = pci_alloc(&mut b, &RingConfig::js()).unwrap();
    b.mem.trace.clear();
    let mut phy = Phy::new(1);
    phy.load_tables(fw);
    Dev {
        b,
        rings,
        fws: FwState::new(),
        phy,
        maps: EfuseMaps {
            ctrl: Some(0x2020),
            log: kit("efuse-log.bin"),
            dav: Some(kit("efuse-dav.bin")),
            phycap: None,
        },
    }
}
fn c2h22() -> [u32; 4] {
    phycap_c2h(2, 2, 0, 0, 3, RTW89_FWCMD_C2HREG_FUNC_PHY_CAP)
}

fn ready(fw: &Firmware) -> Dev {
    let mut d = dev(fw, c2h22(), 0);
    efuse_parse(&mut d.b, &mut d.phy, &mut d.maps).unwrap();
    mac_setup_phycap(&mut d.b, &mut d.phy, &mut d.fws, 0).unwrap();
    d
}

// ---- 1. efuse ----

#[test]
fn logical_map_from_the_dumps() {
    let log = kit("efuse-log.bin");
    let mut l = vec![0xffu8; 2048];
    efuse::logical_efuse_map(&kit("efuse-phy.bin"), &mut l).unwrap();
    assert_eq!(l[..], log[..2048], "efuse-phy.bin -> efuse-log.bin");
    let mut dav = vec![0xffu8; 16];
    efuse::logical_efuse_map(&kit("efuse-dav.bin"), &mut dav).unwrap();
    assert_eq!(dav[..], log[2048..2064], "DAV map = efuse-log.bin tail");
}

#[test]
fn efuse_values_are_the_bytes_of_the_dump() {
    let fw = firmware();
    let mut d = dev(&fw, c2h22(), 0);
    efuse_parse(&mut d.b, &mut d.phy, &mut d.maps).unwrap();
    let log = kit("efuse-log.bin");
    let ef = d.phy.efuse.clone().unwrap();
    assert_eq!(
        (&ef.addr[..], ef.rfe_type, ef.xtal_cap, ef.valid),
        (&log[EF_MAC_ADDR_E..EF_MAC_ADDR_E + 6], 1, 0x3c, true)
    );
    assert_eq!(
        (ef.rfe_type, ef.xtal_cap),
        (log[EF_RFE_TYPE], log[EF_XTAL_K])
    );
    assert_eq!(
        ef.country_code,
        [log[EF_COUNTRY_CODE], log[EF_COUNTRY_CODE + 1]]
    );
    assert_eq!(ef.tssi.thermal, [0x1a, 0x18]);
    let s8 = |r: core::ops::Range<usize>| log[r].iter().map(|&x| x as i8).collect::<Vec<_>>();
    for (p, o) in [(0, EF_PATH_A_TSSI), (1, EF_PATH_B_TSSI)] {
        assert_eq!(ef.tssi.tssi_cck[p].to_vec(), s8(o..o + 6), "tssi cck {p}");
        let mut mcs = s8(o + 6..o + 11);
        mcs.extend(s8(o + 18..o + 32));
        assert_eq!(ef.tssi.tssi_mcs[p].to_vec(), mcs, "tssi mcs {p}");
    }
    assert_eq!(ef.tssi.tssi_cck[0], [17, 17, 18, 19, 19, 20]);
    assert_eq!(ef.tssi.tssi_mcs[0][5..8], [-8, -8, -7]);
    assert_eq!(ef.tssi.tssi_cck[1][0], 12);
    assert!(ef.gain.offset_valid);
    assert!(ef.gain.offset.iter().all(|p| p.iter().all(|&v| v == 0)));
    assert_eq!(ef.dav_log[..], log[2048..2064]);
    assert!(d.phy.hal.support_cckpd && !d.phy.hal.support_igi);
    // _decode_efuse_gain: high nibble -> path A, low nibble -> path B, signed; all 0xff -> not valid
    let mut g2 = log.clone();
    g2[EF_RX_GAIN2G_CCK] = 0x3e;
    g2[EF_RX_GAIN5G_HIGH] = 0xf1;
    let mut e2 = Efuse::default();
    efuse::read_efuse_8852bx(&g2, &mut e2);
    let at = |e: &Efuse, i: u32| [e.gain.offset[0][i as usize], e.gain.offset[1][i as usize]];
    assert_eq!(at(&e2, RTW89_GAIN_OFFSET_2G_CCK), [3, -2]);
    assert_eq!(at(&e2, RTW89_GAIN_OFFSET_5G_HIGH), [-1, 1]);
    for o in [
        EF_RX_GAIN2G_CCK,
        EF_RX_GAIN2G_OFDM,
        EF_RX_GAIN5G_LOW,
        EF_RX_GAIN5G_MID,
        EF_RX_GAIN5G_HIGH,
    ] {
        g2[o] = 0xff;
    }
    efuse::read_efuse_8852bx(&g2, &mut e2);
    assert!(!e2.gain.offset_valid);
    // a short map never panics: missing bytes read as 0xff
    let mut e3 = Efuse::default();
    efuse::read_efuse_8852bx(&[0x12, 0x34], &mut e3);
    assert_eq!((e3.rfe_type, e3.addr), (0xff, [0xff; 6]));
}

#[test]
fn phycap_dump_and_its_retries() {
    let fw = firmware();
    let mut d = dev(&fw, c2h22(), 0);
    efuse_parse(&mut d.b, &mut d.phy, &mut d.maps).unwrap();
    let o = ops(&d.b, 0);
    let head: Vec<(OpKind, u32)> = o[..5].iter().map(|x| (x.kind, x.a)).collect();
    assert_eq!(
        head,
        [
            (OpKind::Rmw, R_AX_PMC_DBG_CTRL2),
            (OpKind::Rmw, R_AX_SYS_ISO_CTRL),
            (OpKind::Delay, 1000),
            (OpKind::Rmw, R_AX_SYS_ISO_CTRL),
            (OpKind::Rmw, R_AX_SYS_ISO_CTRL)
        ]
    );
    let ef: Vec<u32> = o
        .iter()
        .filter(|x| x.kind == OpKind::Wr && x.a == R_AX_EFUSE_CTRL)
        .map(|x| field(x.c, B_AX_EF_ADDR_MASK))
        .collect();
    assert_eq!((ef.len(), ef[0], ef[127]), (128, 0x580, 0x5ff));
    let e = d.phy.efuse.as_ref().unwrap();
    assert!(!e.power_k_valid && !e.pwr_trim.pg_thermal_trim && e.tssi.tssi_trim == [[0; 8]; 2]);
    // rtw89_dump_physical_efuse_map: five tries when a byte's RDY poll times out
    let cuts = |b: &ChipBus| {
        ops(b, 0)
            .iter()
            .filter(|x| x.kind == OpKind::Rmw && x.a == R_AX_PMC_DBG_CTRL2 && x.c != 0)
            .count()
    };
    let mut d = dev(&fw, c2h22(), 4);
    efuse_parse(&mut d.b, &mut d.phy, &mut d.maps).unwrap();
    assert_eq!(
        (cuts(&d.b), d.maps.phycap.as_ref().unwrap().len()),
        (5, 128)
    );
    assert_eq!(d.phy.take_events().len(), 4, "four retry warnings");
    let mut d = dev(&fw, c2h22(), 5);
    let e = efuse_parse(&mut d.b, &mut d.phy, &mut d.maps).unwrap_err();
    assert!(matches!(
        e,
        Rtw89Error::Bus(crate::bus::BusError::PollTimeout {
            addr: R_AX_EFUSE_CTRL,
            ..
        })
    ));
    assert_eq!(cuts(&d.b), 5);
    assert!(d.phy.efuse.is_none());
}

#[test]
fn phycap_parser_offsets() {
    let mut pc = vec![0xffu8; 128];
    for (a, v) in [
        (0x5e9, 0xaa),
        (0x5d6, 0x05),
        (0x5cf, 0xfe),
        (0x5ab, 0x81),
        (0x5df, 0x12),
        (0x5dc, 0x13),
        (0x5de, 0x21),
        (0x5db, 0x43),
        (0x5bb, 0x0f),
        (0x58d, 0x07),
    ] {
        pc[a - 0x580] = v;
    }
    let mut e = Efuse::default();
    efuse::read_phycap_8852bx(&pc, &mut e);
    assert!(e.power_k_valid);
    assert_eq!(e.tssi.tssi_trim[0], [5, -1, -1, -1, -1, -1, -1, -2]);
    assert_eq!(e.tssi.tssi_trim[1][0], -127);
    assert_eq!(
        (e.pwr_trim.thermal_trim, e.pwr_trim.pa_bias_trim),
        ([0x12, 0x13], [0x21, 0x43])
    );
    assert_eq!(
        (e.gain.comp[0][0], e.gain.comp[1][4], e.gain.comp_valid),
        (-1, 7, true)
    );
    // an empty phycap map never panics (all reads 0xff)
    let mut e = Efuse::default();
    efuse::read_phycap_8852bx(&[], &mut e);
    assert!(!e.power_k_valid && !e.gain.comp_valid);
}

#[test]
fn logical_map_out_of_range_is_an_error() {
    // headers whose words run past the physical map's end: Linux's -EINVAL, no panic
    let phy = vec![0u8; 1216];
    let mut log = vec![0xffu8; 2048];
    let e = efuse::logical_efuse_map(&phy, &mut log).unwrap_err();
    assert!(matches!(e, Rtw89Error::Efuse { .. }), "{e:?}");
}

// ---- 2. firmware tables and the walker ----

/// an if/elif/else branch: its target (None for ELSE) and its rows
type Branch = (Option<u32>, Vec<(u32, u32)>);

/// phy-test-expect.mjs blockSelect: each if/elif/else chain parsed as a block, the branch
/// whose target is the headline (rfe, cv) picked (cases 1 and 2); (out, aborted)
fn block_select(t: &RegTable, rfe: u32, cv: u32) -> (Vec<(u32, u32)>, bool) {
    let r = &t.regs;
    let c = |i: usize| r[i].0 >> 28;
    let n = r.len();
    let mut i = 0;
    let mut heads = Vec::new();
    while i < n && c(i) == 0xf {
        heads.push(r[i].0 & 0x0fff_ffff);
        i += 1;
    }
    let cfg = if heads.is_empty() {
        Some(r[0].0 & 0x0fff_ffff)
    } else {
        heads
            .iter()
            .find(|&&h| h == (rfe << 16) | cv)
            .or_else(|| heads.iter().find(|&&h| h == (rfe << 16) | 0xff))
            .copied()
    };
    let mut out = Vec::new();
    let plain = |i: &mut usize| {
        let mut v = Vec::new();
        while *i < n && c(*i) == 0 {
            v.push(r[*i]);
            *i += 1;
        }
        v
    };
    while i < n {
        if c(i) == 0 {
            out.extend(plain(&mut i));
            continue;
        }
        assert_eq!(c(i), 8, "chain at {i} starts with IF");
        let mut branches: Vec<Branch> = Vec::new();
        loop {
            match c(i) {
                8 | 9 => {
                    let target = r[i].0 & 0x0fff_ffff;
                    i += 1;
                    assert_eq!(c(i), 4, "CHECK after IF/ELIF");
                    i += 1;
                    branches.push((Some(target), plain(&mut i)));
                }
                0xa => {
                    i += 1;
                    branches.push((None, plain(&mut i)));
                }
                0xb => {
                    i += 1;
                    break;
                }
                k => panic!("unexpected condition {k} at {i}"),
            }
        }
        let hit = branches.iter().find(|b| cfg.is_some() && b.0 == cfg);
        if hit.is_none() && branches.iter().any(|b| b.0.is_none()) {
            return (out, true);
        }
        if let Some(h) = hit {
            out.extend(h.1.iter().copied());
        }
    }
    (out, false)
}

fn walk(t: &RegTable, rfe: u32, cv: u32) -> (RegLoad, Vec<(u32, u32)>) {
    let mut seen = Vec::new();
    let (r, _) = tbl::init_reg(t, rfe, cv, |a, d| {
        seen.push((a, d));
        Ok(())
    })
    .unwrap();
    (r, seen)
}

#[test]
fn firmware_tables_and_the_walker() {
    let t = PhyTables::from_firmware(&firmware(), 0);
    let len = |x: &Option<RegTable>| x.as_ref().unwrap().regs.len();
    assert_eq!(
        [
            len(&t.bb),
            len(&t.bb_gain),
            len(&t.rf[0]),
            len(&t.rf[1]),
            len(&t.nctl)
        ],
        [1013, 66, 8295, 8392, 1320]
    );
    assert_eq!(
        (
            t.rf[0].as_ref().unwrap().rf_path,
            t.rf[1].as_ref().unwrap().rf_path,
            t.afe.is_none()
        ),
        (0, 1, true)
    );
    let applied = [1013, 66, 947, 932, 1320];
    for (i, tb) in [&t.bb, &t.bb_gain, &t.rf[0], &t.rf[1], &t.nctl]
        .into_iter()
        .enumerate()
    {
        let tb = tb.as_ref().unwrap();
        let (r, seen) = walk(tb, 1, 1);
        let (ind, aborted) = block_select(tb, 1, 1);
        assert!(!r.aborted && !aborted);
        assert_eq!(seen, ind, "table {i}: walker = block selection");
        assert_eq!(seen.len(), applied[i]);
    }
    for (rfe, cv) in [(1, 0), (2, 1), (0x29, 1), (1, 2), (5, 1)] {
        let tb = t.rf[0].as_ref().unwrap();
        let (r, seen) = walk(tb, rfe, cv);
        let (ind, aborted) = block_select(tb, rfe, cv);
        if !r.aborted && !aborted {
            assert_eq!(seen, ind, "{rfe}/{cv}");
        }
    }
}

#[test]
fn walker_rules_on_small_tables() {
    let mk = |p: &[(u32, u32)]| RegTable {
        regs: p.to_vec(),
        rf_path: 0,
        ver: [0; 4],
    };
    let h = |rfe: u32, cv: u32| 0xf000_0000 | (rfe << 16) | cv;
    let (iff, elif) = (|x: u32| 0x8000_0000 | x, |x: u32| 0x9000_0000 | x);
    let (chk, els, end) = (0x4000_0000, 0xa000_0000, 0xb000_0000);
    let small = mk(&[
        (h(1, 0), 0),
        (h(1, 3), 0),
        (h(0xff, 2), 0),
        (0x10, 1),
        (iff(0x10003), 0),
        (chk, 0),
        (0x11, 2),
        (elif(0x10000), 0),
        (chk, 0),
        (0x12, 3),
        (els, 0),
        (0x13, 4),
        (end, 0),
        (0x14, 5),
    ]);
    assert_eq!(
        walk(&small, 1, 1).1,
        [(0x10, 1), (0x11, 2), (0x14, 5)],
        "case 3"
    );
    assert_eq!(
        walk(&small, 1, 0).1,
        [(0x10, 1), (0x12, 3), (0x14, 5)],
        "case 1"
    );
    let (r, seen) = walk(&small, 7, 0);
    assert_eq!(
        (seen, r.aborted),
        (vec![(0x10, 1)], true),
        "case 4, ELSE without a match"
    );
    let c2 = mk(&[(h(1, 3), 0), (h(1, 0xff), 0), (h(1, 0xff), 0), (0x10, 1)]);
    assert_eq!(tbl::sel_headline(&c2, 1, 1).unwrap().idx, 1, "case 2");
    assert_eq!(
        tbl::sel_headline(&mk(&[(h(2, 3), 0)]), 1, 1),
        None,
        "no headline"
    );
    // an empty table: nothing applied, no panic
    let (r, seen) = walk(&mk(&[]), 1, 1);
    assert!(seen.is_empty() && !r.aborted);
}

#[test]
fn config_functions() {
    let mut b = ChipBus::new(Model::default());
    for (a, v) in [
        (0xfe, 0),
        (0xfd, 0),
        (0xfc, 0),
        (0xfb, 0),
        (0xfa, 0),
        (0xf9, 0),
        (0x900, BYPASS_CR_DATA),
        (0x904, 7),
    ] {
        tbl::config_bb_reg(&mut b, a, v).unwrap();
    }
    let d = |us| Op::new(OpKind::Delay, 0, 0, us, 0, 0);
    same(
        &ops(&b, 0),
        &[
            d(50000),
            d(5000),
            d(1000),
            d(50),
            d(5),
            d(1),
            Op::new(OpKind::Wr, 4, 0, BB + 0x904, 0, 7),
        ],
        "config_bb_reg",
    );
    // only 0xfe is a delay; from 0x100 up the value also goes to the H2C store
    let mut b = ChipBus::new(Model::default());
    let mut info = RfRegInfo::new(RF_PATH_A);
    let list = [(0xfe, 0), (0x10005, 3), (0x05, 7), (0x105, 9), (0xfd, 1)];
    for (a, v) in list {
        tbl::config_rf_reg_v1(&mut b, a, v, 0, &mut info).unwrap();
    }
    let exp = expect(Model::default(), |e| {
        for (a, v) in list {
            if a == 0xfe {
                e.delay_us(50000).unwrap();
            } else {
                rf_w(e, 0, a, 0xfffff, v).unwrap();
            }
        }
    });
    same(&ops(&b, 0), &exp, "config_rf_reg_v1");
    assert_eq!(info.curr_idx, 2);
    assert_eq!(
        info.h2c[0][..2],
        [0x0050_0003, 0x1050_0009],
        "(addr << 20) | data"
    );
}

// ---- 3. PHY capabilities ----

#[test]
fn phy_capabilities_from_the_firmware() {
    let fw = firmware();
    let cap = |c2h| {
        let mut d = dev(&fw, c2h, 0);
        efuse_parse(&mut d.b, &mut d.phy, &mut d.maps).unwrap();
        let t0 = d.b.mem.trace.len();
        let r = mac_setup_phycap(&mut d.b, &mut d.phy, &mut d.fws, 0);
        (d, t0, r)
    };
    let (d, t0, r) = cap(phycap_c2h(1, 2, 2, 2, 3, RTW89_FWCMD_C2HREG_FUNC_PHY_CAP));
    let c = r.unwrap();
    assert_eq!((c.tx_nss, c.rx_nss, c.tx_ant, c.rx_ant), (1, 2, 2, 2));
    let w: Vec<Op> = ops(&d.b, t0)
        .into_iter()
        .filter(|o| o.kind == OpKind::Wr && o.a == R_AX_H2CREG_DATA0)
        .collect();
    assert_eq!(w.len(), 1);
    assert_eq!(
        field(w[0].c, RTW89_H2CREG_HDR_FUNC_MASK),
        RTW89_FWCMD_H2CREG_FUNC_GET_FEATURE
    );
    assert_eq!(field(w[0].c, RTW89_H2CREG_GET_FEATURE_PART_NUM), 0);
    let h = &d.phy.hal;
    assert_eq!(
        (
            h.tx_nss,
            h.rx_nss,
            h.antenna_tx,
            h.antenna_rx,
            h.tx_path_diversity,
            h.no_eht
        ),
        (1, 2, RF_B, 0, true, true)
    );
    // phy_dm_init refuses before the capabilities (hal rx_nss 0)
    let mut fresh = dev(&fw, c2h22(), 0);
    efuse_parse(&mut fresh.b, &mut fresh.phy, &mut fresh.maps).unwrap();
    assert!(
        phy_dm_init(
            &mut fresh.b,
            &mut fresh.phy,
            None,
            0,
            None,
            &mut Spy::default()
        )
        .is_err()
    );
    let (d2, _, r) = cap(phycap_c2h(
        0,
        3,
        1,
        2,
        RTW89_C2HREG_PHYCAP_W1_PROT_11BE,
        RTW89_FWCMD_C2HREG_FUNC_PHY_CAP,
    ));
    r.unwrap();
    let h = &d2.phy.hal;
    assert_eq!(
        (
            h.tx_nss,
            h.rx_nss,
            h.antenna_tx,
            h.antenna_rx,
            h.tx_path_diversity,
            h.no_eht
        ),
        (2, 2, RF_B, 0, false, false)
    );
    let (d3, _, r) = cap(phycap_c2h(2, 2, 2, 1, 3, RTW89_FWCMD_C2HREG_FUNC_PHY_CAP));
    r.unwrap();
    assert_eq!((d3.phy.hal.antenna_tx, d3.phy.hal.antenna_rx), (0, RF_B));
    let (_, _, r) = cap(phycap_c2h(2, 2, 0, 0, 3, RTW89_FWCMD_C2HREG_FUNC_NULL));
    assert!(matches!(
        r,
        Err(Rtw89Error::C2hRegAnswer {
            what: "phycap",
            reg: R_AX_C2HREG_DATA0,
            ..
        })
    ));
}

// ---- 4. BB/RF reset, BB table, RF tables ----

#[test]
fn reset_bb_rf_clears_then_sets_the_rfc_bytes() {
    let fw = firmware();
    let mut d = ready(&fw);
    let t0 = d.b.mem.trace.len();
    reset_bb_rf(&mut d.b).unwrap();
    let xt: Vec<(u32, u32)> = ops(&d.b, t0)
        .iter()
        .filter(|o| {
            o.kind == OpKind::Wr
                && o.a == R_AX_WLAN_XTAL_SI_CTRL
                && field(o.c, B_AX_WL_XTAL_SI_MODE_MASK) == XTAL_SI_NORMAL_WRITE
        })
        .map(|o| {
            (
                field(o.c, B_AX_WL_XTAL_SI_ADDR_MASK),
                field(o.c, B_AX_WL_XTAL_SI_DATA_MASK),
            )
        })
        .collect();
    assert_eq!(
        xt,
        [
            (XTAL_SI_WL_RFC_S0, 0xc0),
            (XTAL_SI_WL_RFC_S1, 0xc0),
            (XTAL_SI_WL_RFC_S0, 0xc7),
            (XTAL_SI_WL_RFC_S1, 0xc7)
        ],
        "RF00S/RF10S cleared, then 0xc7"
    );
}

fn h2c_hdr(b: &[u8]) -> (u32, u32, Vec<u32>) {
    let le = |o: usize| u32::from_le_bytes(b[o..o + 4].try_into().unwrap());
    let (w0, w1) = (le(24), le(28));
    let n = (field(w1, H2C_HDR_TOTAL_LEN) - H2C_HEADER_LEN) as usize / 4;
    (w0, w1, (0..n).map(|k| le(32 + 4 * k)).collect())
}

#[test]
fn bb_table_gain_table_and_rf_tables() {
    let fw = firmware();
    let mut d = ready(&fw);
    let mut spy = Spy::default();
    let t0 = d.b.mem.trace.len();
    init_bb_reg(&mut d.b, &mut d.phy, &mut spy).unwrap();
    let t = d.phy.tables.clone().unwrap();
    let (bb_sel, _) = block_select(t.bb.as_ref().unwrap(), 1, 1);
    let exp = expect(Model::default(), |e| {
        for &(a, v) in &bb_sel {
            tbl::config_bb_reg(e, a, v).unwrap();
        }
        bbx::bb_reset(e).unwrap();
    });
    same(
        &ops(&d.b, t0),
        &exp,
        "init_bb_reg: BB_REG, then rtw8852b_bb_reset",
    );
    assert_eq!(
        spy.0,
        [("initTxpwrUnit", t0 + bb_sel.len())],
        "init_txpwr_unit right after the BB table"
    );
    // rtw89_phy_config_bb_gain_ax: union rtw89_phy_bb_gain_arg
    let g = &d.phy.bb_gain;
    let (gain_sel, _) = block_select(t.bb_gain.as_ref().unwrap(), 1, 1);
    for (a, v) in gain_sel {
        let (typ, rx, bw) = ((a & 0xff) as usize, (a & 0xf) as usize, (a >> 4) & 0xf);
        let (path, gb, cfg) = (
            ((a >> 8) & 0xff) as usize,
            ((a >> 16) & 0xff) as usize,
            a >> 24,
        );
        let got: Vec<i8> = if cfg == 0 {
            match typ {
                0 => g.lna_gain[gb][path][..4].to_vec(),
                1 => g.lna_gain[gb][path][4..7].to_vec(),
                _ => g.tia_gain[gb][path].to_vec(),
            }
        } else if bw == 0 {
            vec![g.rpl_ofst_20[gb][path]]
        } else {
            let w = if rx == 0 {
                1
            } else if bw == 1 || rx == 9 {
                2
            } else {
                4
            };
            let arr: &[i8] = if bw == 1 {
                &g.rpl_ofst_40[gb][path]
            } else {
                &g.rpl_ofst_80[gb][path]
            };
            arr[rx..rx + w].to_vec()
        };
        let bytes: Vec<i8> = (0..got.len()).map(|i| (v >> (8 * i)) as u8 as i8).collect();
        assert_eq!(got, bytes, "bb gain {a:#x}");
    }

    let t0 = d.b.mem.trace.len();
    let m0 = d.b.writes.len();
    init_rf_reg(&mut d.b, &mut d.phy, &mut d.rings, &mut d.fws).unwrap();
    let all = ops(&d.b, t0);
    let mut at = 0;
    let bufs = d.rings.tx("CH12").unwrap().bufs.clone();
    let h2cs: Vec<Vec<u8>> = d.b.writes[m0..]
        .iter()
        .filter(|(a, _)| bufs.contains(a))
        .map(|(_, b)| b.clone())
        .collect();
    for path in [0u32, 1] {
        let (sel, _) = block_select(t.rf[path as usize].as_ref().unwrap(), 1, 1);
        let exp = expect(Model::default(), |e| {
            for &(a, v) in &sel {
                if a == 0xfe {
                    e.delay_us(50000).unwrap();
                } else {
                    rf_w(e, path, a, 0xfffff, v).unwrap();
                }
            }
        });
        same(&all[at..at + exp.len()], &exp, "RADIO table");
        at += exp.len();
        // rtw89_h2c_tx of the page: BD length/address, index, the wait for the index
        let kinds: Vec<OpKind> = all[at..at + 4].iter().map(|o| o.kind).collect();
        assert_eq!(kinds, [OpKind::Mwr, OpKind::Mwr, OpKind::Wr, OpKind::Poll]);
        at += 4;
        let words: Vec<u32> = sel
            .iter()
            .filter(|(a, _)| *a != 0xfe && *a >= 0x100)
            .map(|(a, v)| (a << 20) | v)
            .collect();
        let (w0, _, got) = h2c_hdr(&h2cs[path as usize]);
        let cls = if path == 0 {
            H2C_CL_OUTSRC_RF_REG_A
        } else {
            H2C_CL_OUTSRC_RF_REG_B
        };
        assert_eq!(
            (
                field(w0, H2C_HDR_CAT),
                field(w0, H2C_HDR_CLASS),
                field(w0, H2C_HDR_FUNC)
            ),
            (H2C_CAT_OUTSRC, cls, 0)
        );
        assert_eq!(got, words, "H2C words = (addr << 20) | data");
        let l = &d.phy.loads.rf[path as usize];
        assert_eq!((l.h2c_values as usize, l.h2c_ok), (words.len(), true));
    }
    assert_eq!(at, all.len());
    assert_eq!(
        (d.phy.loads.rf[0].h2c_values, d.phy.loads.rf[1].h2c_values),
        (294, 294)
    );

    // more than a page: 1100 values in pages of 500; 1600: the store keeps 1500 and warns
    let big = |n: u32, p: u32| RegTable {
        regs: (0..n).map(|i| (0x10000 | (i & 0xff), i)).collect(),
        rf_path: p,
        ver: [0; 4],
    };
    d.phy.tables.as_mut().unwrap().rf = [Some(big(1100, 0)), Some(big(1600, 1)), None, None];
    d.phy.take_events();
    let m0 = d.b.writes.len();
    init_rf_reg(&mut d.b, &mut d.phy, &mut d.rings, &mut d.fws).unwrap();
    let pages: Vec<(u32, u32, usize)> = d.b.writes[m0..]
        .iter()
        .filter(|(a, _)| bufs.contains(a))
        .map(|(_, b)| {
            let (w0, _, w) = h2c_hdr(b);
            (field(w0, H2C_HDR_CLASS), field(w0, H2C_HDR_FUNC), w.len())
        })
        .collect();
    let (a, b) = (H2C_CL_OUTSRC_RF_REG_A, H2C_CL_OUTSRC_RF_REG_B);
    assert_eq!(
        pages,
        [
            (a, 0, 500),
            (a, 1, 500),
            (a, 2, 100),
            (b, 0, 500),
            (b, 1, 500),
            (b, 2, 500)
        ]
    );
    let full = d
        .phy
        .take_events()
        .iter()
        .filter(|e| matches!(e, PhyEvent::RfStoreFull { path: 1, idx: 1500 }))
        .count();
    assert_eq!(full, 100);
    // a CH12 index that never moves: Linux only warns, so does init_rf_reg
    d.phy.tables.as_mut().unwrap().rf = [Some(big(3, 0)), Some(big(3, 1)), None, None];
    let ch12 = d.rings.tx("CH12").unwrap().idx;
    d.b.poll_ok = Some(alloc::boxed::Box::new(move |a| a != ch12));
    init_rf_reg(&mut d.b, &mut d.phy, &mut d.rings, &mut d.fws).unwrap();
    assert_eq!(
        d.phy.loads.rf.iter().map(|l| l.h2c_ok).collect::<Vec<_>>(),
        [false, false]
    );
    let warned = d
        .phy
        .take_events()
        .iter()
        .filter(|e| matches!(e, PhyEvent::RfH2cFailed { error: Some(_), .. }))
        .count();
    assert_eq!(warned, 2);
    // a lost bus while the H2C goes out is returned, not hidden behind that warning
    d.b.poll_ok = None;
    let rf_ops = expect(Model::default(), |e| {
        for i in 0..3u32 {
            rf_w(e, 0, 0x10000 | i, 0xfffff, i).unwrap();
        }
    })
    .len();
    d.b.mem.fail_at_op = Some(d.b.mem.trace.len() + rf_ops + 1);
    let e = init_rf_reg(&mut d.b, &mut d.phy, &mut d.rings, &mut d.fws).unwrap_err();
    assert_eq!(e, Rtw89Error::Bus(crate::bus::BusError::Gone));
}

#[test]
fn missing_tables_stop() {
    let fw = firmware();
    let mut d = ready(&fw);
    d.phy.tables.as_mut().unwrap().bb = None;
    let e = init_bb_reg(&mut d.b, &mut d.phy, &mut Spy::default()).unwrap_err();
    assert!(
        matches!(
            e,
            Rtw89Error::PhyTable {
                what: "no BB_REG element",
                rfe: 1,
                cv: 1,
                ..
            }
        ),
        "{e:?}"
    );
    assert!(
        d.phy.tables.is_some(),
        "the tables are put back after an error"
    );
    assert!(check_tables(&d.phy).is_err(), "nothing loaded yet");
    d.phy.tables.as_mut().unwrap().rf[1] = None;
    let e = init_rf_reg(&mut d.b, &mut d.phy, &mut d.rings, &mut d.fws).unwrap_err();
    assert!(matches!(e, Rtw89Error::PhyTable { path: 1, .. }), "{e:?}");
    // rfe 0 has no headline in the RADIO tables: left out (Linux), then BitsOS's check stops
    let mut d = ready(&fw);
    d.phy.efuse.as_mut().unwrap().rfe_type = 0;
    init_bb_reg(&mut d.b, &mut d.phy, &mut Spy::default()).unwrap();
    init_rf_reg(&mut d.b, &mut d.phy, &mut d.rings, &mut d.fws).unwrap();
    assert!(d.phy.loads.rf.iter().all(|l| l.aborted));
    assert!(matches!(
        check_tables(&d.phy),
        Err(Rtw89Error::PhyTable {
            what: "RADIO table not loaded",
            rfe: 0,
            ..
        })
    ));
}

// ---- 5. dm init ----

#[test]
fn dm_init_order_and_state() {
    let fw = firmware();
    let mut d = ready(&fw);
    let mut spy = Spy::default();
    let t0 = d.b.mem.trace.len();
    phy_dm_init(&mut d.b, &mut d.phy, None, 0, None, &mut spy).unwrap();
    let got = ops(&d.b, t0);
    assert_eq!(got.len(), 1563, "test-phy.mjs: dm init 1563 ops");
    let n = d.b.mem.trace.len();
    assert_eq!(
        spy.0,
        [
            ("rfkInit", n),
            ("setTxpwrCtrl", n),
            ("powerTrim", n),
            ("cfgTxrxPath", n)
        ],
        "rfk_init, set_txpwr_ctrl, power_trim, cfg_txrx_path after the NCTL table"
    );
    let p = &d.phy;
    assert_eq!(
        (
            p.avg_thermal,
            p.stat.last_thermal_max,
            p.cfo.crystal_cap,
            p.cfo.x_cap_ofst,
            p.ul_tb.def_if_bandedge,
            p.nctl_ready
        ),
        ([0x1a, 0x1a], 0x1a, 0x3c, 0, 1, true)
    );
    let g = &p.efuse.as_ref().unwrap().gain;
    assert_eq!(
        (g.offset_base[0], g.rssi_base[0]),
        (-12, 16),
        "bb_sethw: s8 of R_P0_RPL1 / R_P1_RPL1"
    );
    assert_eq!(
        (p.env.ifs_clm_th_l, p.env.ifs_clm_th_h),
        ([0, 3, 9, 33], [2, 8, 32, 128])
    );
    assert_eq!(
        (p.dig.fa_th, p.dig.force_gaincode_idx_en),
        ([196, 352, 440, 528], false)
    );
    // the PHY status bitmaps from the fake's 0x0F0F00F0
    let sts = |page: u32| {
        let a = BB + R_PHY_STS_BITMAP_ADDR_START + ((if page > 9 { page - 1 } else { page }) << 2);
        got.iter()
            .rev()
            .find(|o| o.kind == OpKind::Wr && o.a == a)
            .unwrap()
            .c
    };
    assert_eq!(
        (sts(6), sts(10), sts(11), sts(13)),
        (0x0f0f_20f0, 0x0f0f_20f2, 0x0f0f_0002, 0x0f1f_0000)
    );
    // dig_reset's CCK PD lower bound: max(0 - 23, -18) - 110 = -128
    assert!(got.iter().any(|o| o.kind == OpKind::Rmw
        && o.a == BB + R_BMODE_PDTH_V1
        && o.c == prep(B_BMODE_PDTH_LOWER_BOUND_MSK_V1, (-128i32) as u32)));
    // rtw89_phy_nctl_poll until 0x8080 reads 4: 3 rounds; then a timeout after 101
    for (rounds, nctl, ok) in [(3usize, 2u32, true), (101, u32::MAX, false)] {
        d.b.model.nctl_reads = 0;
        d.b.model.nctl = alloc::boxed::Box::new(move |n| if n >= nctl { 4 } else { 0 });
        let t0 = d.b.mem.trace.len();
        init_rf_nctl(&mut d.b, &mut d.phy).unwrap();
        let o = ops(&d.b, t0);
        let reads = o
            .iter()
            .filter(|x| x.kind == OpKind::Rd && x.a == BB + 0x8080)
            .count();
        assert_eq!((reads, d.phy.nctl_ready), (rounds, ok));
        let gaps = o
            .iter()
            .filter(|x| x.kind == OpKind::Delay && x.a == 10)
            .count();
        assert_eq!(
            gaps,
            rounds - 1,
            "10 us between rounds, none after the last"
        );
    }
    assert!(matches!(
        d.phy.take_events()[..],
        [PhyEvent::NctlPollFailed {
            reg: 0x18080,
            value: 0
        }]
    ));
}

// ---- 6. tracking ----

#[test]
fn thermal_average_and_cfo_tracking() {
    let fw = firmware();
    let mut d = ready(&fw);
    phy_dm_init(&mut d.b, &mut d.phy, None, 0, None, &mut Spy::default()).unwrap();
    // ewma_thermal (average.h, precision 4, weight_rcp 4)
    let mut ew = 0x1a << 4;
    d.b.model.thermal = 0x20;
    trk::stat_thermal_update(&mut d.b, &mut d.phy).unwrap();
    ew = (ew * 4 - ew + (0x20 << 4)) >> 2;
    assert_eq!(
        (d.phy.avg_thermal, d.phy.stat.last_thermal_max),
        ([ew >> 4, ew >> 4], 0x20)
    );
    d.b.model.thermal = 0;
    trk::stat_thermal_update(&mut d.b, &mut d.phy).unwrap();
    assert_eq!(
        (d.phy.avg_thermal, d.phy.stat.last_thermal_max),
        ([0x1b, 0x1b], 0),
        "a zero reading is not averaged"
    );
    for _ in 0..10 {
        trk::cfo_parse(&mut d.phy, 200, 0);
    }
    trk::cfo_parse(&mut d.phy, 200, 64);
    assert!(matches!(
        d.phy.take_events()[..],
        [PhyEvent::CfoMacIdRange { mac_id: 64 }]
    ));
    let c = &d.phy.cfo;
    assert_eq!(
        (c.cfo_tail[0], c.cfo_cnt[0], c.packet_count),
        (2000, 10, 10)
    );
    let dcfo = |e: &mut ChipBus, v: i32| {
        phy_mask32(e, R_DCFO_COMP_S0, B_DCFO_COMP_S0_MSK, v as u32).unwrap();
    };
    let cap = |e: &mut ChipBus, cap: u32| {
        trk::set_crystal_cap(e, &mut Cfo::default(), cap, true).unwrap()
    };
    // avg 200 > CFO_TRK_STOP_TH_4: cap 0x3c + 3; dcfo (2000 << 10) / 10 - (8 << 10) = 196608
    // -> 196608 / 625 + 5 = 319
    let t0 = d.b.mem.trace.len();
    trk::cfo_track(&mut d.b, &mut d.phy.cfo, 1, 0, 0).unwrap();
    let exp = expect(Model::default(), |e| {
        cap(e, 0x3f);
        phy_r32_mask(e, R_DCFO, B_DCFO).unwrap();
        dcfo(e, 319);
    });
    same(&ops(&d.b, t0), &exp, "cfo_dm");
    let c = &d.phy.cfo;
    assert_eq!(
        (
            c.crystal_cap,
            c.x_cap_ofst,
            c.is_adjust,
            c.packet_count,
            c.cfo_tail[0],
            c.cfo_cnt[0]
        ),
        (0x3f, 3, true, 0, 0, 0)
    );
    // high throughput: ENHANCE, the timer work does the tracking
    let t0 = d.b.mem.trace.len();
    trk::cfo_track(&mut d.b, &mut d.phy.cfo, 1, 150, 0).unwrap();
    let c = &d.phy.cfo;
    assert_eq!(
        (
            d.b.mem.trace.len() - t0,
            c.phy_cfo_status,
            c.cfo_trig_by_timer_en,
            c.cfo_timer_ms
        ),
        (0, RTW89_PHY_DCFO_STATE_ENHANCE, true, 250)
    );
    for _ in 0..4 {
        trk::cfo_parse(&mut d.phy, -50, 0);
    }
    // avg -50 > CFO_TRK_STOP_TH_2: cap - 1; dcfo (-200 << 10) / 4 + (8 << 10) = -43008
    // -> trunc(-43008 / 625) - 5 = -73
    d.b.model.xtal.clear();
    let t0 = d.b.mem.trace.len();
    assert!(trk::cfo_track_work(&mut d.b, &mut d.phy.cfo, 1).unwrap());
    let exp = expect(
        Model {
            xtal: Default::default(),
            ..Model::default()
        },
        |e| {
            cap(e, 0x3e);
            phy_r32_mask(e, R_DCFO, B_DCFO).unwrap();
            dcfo(e, -73);
        },
    );
    same(&ops(&d.b, t0), &exp, "cfo work");
    trk::cfo_track(&mut d.b, &mut d.phy.cfo, 1, 10, 0).unwrap();
    assert_eq!(
        (d.phy.cfo.phy_cfo_status, d.phy.cfo.cfo_trig_by_timer_en),
        (RTW89_PHY_DCFO_STATE_NORMAL, false)
    );
    assert!(!trk::cfo_track_work(&mut d.b, &mut d.phy.cfo, 1).unwrap());
    // not associated: one step back towards the efuse cap
    let t0 = d.b.mem.trace.len();
    trk::cfo_track(&mut d.b, &mut d.phy.cfo, 0, 0, 0).unwrap();
    same(
        &ops(&d.b, t0),
        &expect(Model::default(), |e| cap(e, 0x3d)),
        "cfo_reset",
    );
    assert_eq!(d.phy.cfo.crystal_cap, 0x3d);
}

// ---- 7. AFE, RX status, the BB helpers ----

#[test]
fn afe_rows_and_rx_status() {
    let fw = firmware();
    let mut d = ready(&fw);
    assert_eq!(
        bb_afe(&mut d.b, &mut d.phy).unwrap(),
        0,
        "no AFE_PWR_SEQ element in this firmware"
    );
    let rows: [[u32; 6]; 5] = [
        [0, 2, 5, 0x1234, 0xff00, 0x12],
        [0, 0, 0, 0x4000, 0xf, 3],
        [1, 0, 5, 50, 0, 0],
        [2, 2, 0, 0x88, 0x1, 0],
        [1, 0, 5, u32::MAX, 0, 0],
    ];
    let mut afe = vec![0u8; 8 + rows.len() * 24];
    for (i, r) in rows.iter().enumerate() {
        for (j, v) in r.iter().enumerate() {
            afe[8 + i * 24 + j * 4..][..4].copy_from_slice(&v.to_le_bytes());
        }
    }
    // a size that claims more rows than the payload has: only the whole rows run
    d.phy.tables.as_mut().unwrap().afe = Some((rows.len() as u32 * 24 + 240, afe));
    let t0 = d.b.mem.trace.len();
    assert_eq!(bb_afe(&mut d.b, &mut d.phy).unwrap(), 5);
    let got: Vec<(OpKind, u32, u32, u32)> = ops(&d.b, t0)
        .iter()
        .map(|o| (o.kind, o.a, o.b, o.c))
        .collect();
    assert_eq!(
        got,
        [
            (OpKind::Rmw, 0x1234, 0xff00, 0x1200),
            (OpKind::Rmw, BB + 0x4000, 0xf, 3),
            (OpKind::Delay, 50, 0, 0),
            (OpKind::Rd, 0x88, 0, 0),
            (OpKind::Delay, 10_000_000, 0, 0)
        ]
    );
    assert!(matches!(
        d.phy.take_events()[..],
        [PhyEvent::WaitClamped {
            want_us: u32::MAX,
            us: 10_000_000,
            ..
        }]
    ));

    let rx = |rssi_avg, rpl_avg, a, b, chan_idx| {
        rx_status(&mut Ppdu {
            rssi_avg,
            rpl_avg,
            rssi: [a, b],
            chan_idx,
            valid: true,
        })
    };
    let s = rx(100, 110, 100, 90, 0x06);
    assert_eq!(
        (s.signal, s.chain_signal, s.ch, s.freq),
        (-55, [-55, -60], Some(6), Some(2437))
    );
    let s = rx(0, 0, 80, 60, 0x24);
    assert_eq!(
        (s.signal, s.ch, s.freq, s.band),
        (-70, Some(44), Some(5220), Some(Band::B5g))
    );
    let s = rx(120, 100, 130, 110, 0); // u8 delta 236
    assert_eq!((s.signal, s.chain_signal, s.ch), (-55, [-55, -65], None));
    for idx in 0..=255u8 {
        let _ = decode_chan_idx(idx); // every index decodes, none panics
    }
}

#[test]
fn bb_helpers_and_their_tables() {
    // rtw8852bx_pmac_ht20_mcs7_tbl / btc_preagc tables as the generator took them from the C
    assert_eq!(
        (PMAC_HT20_MCS7.len(), PREAGC_EN.len(), PREAGC_DIS.len()),
        (120, 14, 14)
    );
    let mut b = ChipBus::new(Model::default());
    bbx::bb_set_plcp_tx(&mut b).unwrap();
    let o = ops(&b, 0);
    assert_eq!(o.len(), 120);
    assert!(o.iter().zip(PMAC_HT20_MCS7).all(|(op, r)| {
        if r.mask == 0xffff_ffff {
            op.kind == OpKind::Wr && op.a == BB + r.addr && op.c == r.data
        } else {
            op.kind == OpKind::Rmw
                && op.a == BB + r.addr
                && op.b == r.mask
                && op.c == prep(r.mask, r.data)
        }
    }));
    // TSSI backup: the 9-bit TX power is sign-extended
    let mut b = ChipBus::new(Model::default());
    b.mem
        .poke(BB + R_TXPATH_SEL, Width::W32, prep(B_TXPATH_SEL_MSK, 3));
    b.mem
        .poke(BB + R_TXPWR, Width::W32, prep(B_TXPWR_MSK, 0x1f0));
    let bak = bbx::bb_backup_tssi(&mut b).unwrap();
    assert_eq!((bak.tx_path, bak.tx_pwr), (3, -16));
    // get_thermal in TSSI mode reads 0x3c10 bits 29:24 (path B)
    let mut b = ChipBus::new(Model::default());
    b.mem.poke(BB + 0x3c10, Width::W32, 0x2a00_0000);
    assert_eq!(bbx::get_thermal(&mut b, 1, true).unwrap(), 0x2a);
    assert_eq!(norm(&ops(&b, 0)), [(OpKind::Rd, 4, BB + 0x3c10, 0, 0)]);
    // a direct RF write on a path the chip does not have is refused, nothing written
    let mut b = ChipBus::new(Model::default());
    assert!(matches!(
        rf_w(&mut b, 2, 0x10018, 0xfffff, 1),
        Err(Rtw89Error::Invalid { .. })
    ));
    assert!(b.mem.trace.is_empty());
}
