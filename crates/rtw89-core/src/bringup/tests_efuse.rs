//! The not-Linux efuse check (`check_efuse`) and `Bringup::probe_efuse` on ChipBus with the
//! real efuse dumps of the measured chip, patched as a rewrite of the efuse would: what is
//! refused, with which register and value, and that a refused efuse leaves `rfe` at 0 so the
//! stop powers off as the probe powered on.

use alloc::vec;
use alloc::vec::Vec;
use std::path::PathBuf;

use super::*;
use crate::bringup::power::ChipVer;
use crate::bus::Width;
use crate::phy::Efuse;
use crate::phy::chipbus::{ChipBus, EfuseChip, Model, phycap_c2h};
use crate::phy::efuse::{LOG_SIZE, PHYS_SIZE, SEC_CTRL, logical_efuse_map};
use crate::tables::phy::{EF_MAC_ADDR_E, EF_RFE_TYPE};
use crate::tables::regs::*;
use crate::trace::{OpKind, TraceEntry};

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

const UNICAST_MAC: [u8; 6] = [0x02, 0xb1, 0x75, 0x00, 0x00, 0x01];

fn efuse(valid: bool, rfe: u8, mac: [u8; 6]) -> Efuse {
    Efuse {
        valid,
        rfe_type: rfe,
        addr: mac,
        ..Efuse::default()
    }
}
fn refusal(e: Rtw89Error) -> (&'static str, u32, u32) {
    match e {
        Rtw89Error::Efuse { what, addr, value } => (what, addr, value),
        e => panic!("not an efuse refusal: {e:?}"),
    }
}
const NOT_AUTOLOADED: &str = "efuse not autoloaded";
const NO_TABLES: &str = "rfe_type BitsOS has no tables for";
const NO_UNICAST: &str = "MAC address is no unicast address";

#[test]
fn check_efuse_refuses_what_has_no_tables_or_no_unicast_mac() {
    // a chip like the measured one passes
    check_efuse(&efuse(true, 1, UNICAST_MAC), Some(0x2020)).unwrap();
    // autoload: the register and the value the dump read; checked before anything else
    let ctl = R_AX_SYS_WL_EFUSE_CTRL;
    for (ef, c, value) in [
        (efuse(false, 1, UNICAST_MAC), Some(0x0000), 0),
        (efuse(false, 1, UNICAST_MAC), Some(0x2000 | 0x1f), 0x201f),
        (efuse(false, 1, UNICAST_MAC), None, 0),
        (efuse(false, 2, [0; 6]), Some(0x0001), 1),
    ] {
        let e = check_efuse(&ef, c).unwrap_err();
        assert_eq!(refusal(e), (NOT_AUTOLOADED, ctl, value), "{ef:?}");
    }
    // rfe: only the measured chip's variant; the byte's efuse offset and the value
    for rfe in [0u8, 2, 5, 0xff] {
        let e = check_efuse(&efuse(true, rfe, UNICAST_MAC), Some(0x2020)).unwrap_err();
        assert_eq!(
            refusal(e),
            (NO_TABLES, EF_RFE_TYPE as u32, u32::from(rfe)),
            "rfe {rfe}"
        );
    }
    // rfe is checked before the MAC
    let e = check_efuse(&efuse(true, 2, [0; 6]), Some(0x2020)).unwrap_err();
    assert_eq!(refusal(e).0, NO_TABLES);
    // MAC: all zero, all ff (an unprogrammed efuse), group bit set; its first four bytes
    for (mac, value) in [
        ([0u8; 6], 0),
        ([0xff; 6], 0xffff_ffff),
        ([0x01, 0x00, 0x5e, 0x00, 0x00, 0x01], 0x0100_5e00),
        ([0x6d, 0x05, 0xd3, 0xd5, 0x30, 0xf1], 0x6d05_d3d5),
    ] {
        let e = check_efuse(&efuse(true, 1, mac), Some(0x2020)).unwrap_err();
        assert_eq!(
            refusal(e),
            (NO_UNICAST, EF_MAC_ADDR_E as u32, value),
            "{mac:02x?}"
        );
    }
    // a replaced module with another plausible MAC, a locally administered one, and an
    // all-zero or all-ff MAC with only some bytes like that: all pass (spec/wlan.md §3)
    for mac in [
        [0x00, 0xe0, 0x4c, 0x12, 0x34, 0x56],
        [0x02, 0x00, 0x00, 0x00, 0x00, 0x01],
        [0x00, 0x00, 0x00, 0x00, 0x00, 0x01],
        [0xfe, 0xff, 0xff, 0xff, 0xff, 0xff],
    ] {
        check_efuse(&efuse(true, 1, mac), Some(0x2020)).unwrap();
    }
    // the xtal cap and the rest are not checked (the JS demanded 0x3C)
    let mut ef = efuse(true, 1, UNICAST_MAC);
    ef.xtal_cap = 0;
    ef.power_k_valid = true;
    check_efuse(&ef, Some(0x2020)).unwrap();
}

/// A rewrite of the efuse as Realtek's tools do it: a new packet after the last one, one
/// word per packet (header 0x3 | block high nibble, block low nibble | word-enable with the
/// word's bit clear), so the logical map's later packet wins
fn rewrite(phys: &mut [u8], off: usize, bytes: &[u8]) {
    let mut log = vec![0xffu8; LOG_SIZE];
    logical_efuse_map(phys, &mut log).unwrap();
    log[off..off + bytes.len()].copy_from_slice(bytes);
    let mut p = SEC_CTRL as usize;
    while phys[p] != 0xff && phys[p + 1] != 0xff {
        let present = 4 - (phys[p + 1] & 0xf).count_ones() as usize;
        p += 2 + 2 * present;
    }
    for w in ((off & !1)..off + bytes.len()).step_by(2) {
        let (blk, word) = (w >> 3, (w & 7) >> 1);
        phys[p] = 0x30 | (blk >> 4) as u8;
        phys[p + 1] = ((blk & 0xf) << 4) as u8 | (0xf & !(1u8 << word));
        phys[p + 2] = log[w];
        phys[p + 3] = log[w + 1];
        p += 4;
    }
    assert!(p < (PHYS_SIZE - SEC_CTRL) as usize, "the rewrite fits");
}

/// The measured chip after the firmware download: its efuse (patched by `patch`), the
/// autoload register, and the firmware's PHY_CAP answer
fn chip(ctrl: u16, patch: impl FnOnce(&mut Vec<u8>)) -> ChipBus {
    let mut phys = kit("efuse-phy.bin");
    patch(&mut phys);
    let mut b = ChipBus::new(Model {
        c2h: phycap_c2h(2, 2, 0, 0, 3, RTW89_FWCMD_C2HREG_FUNC_PHY_CAP),
        efuse: Some(EfuseChip {
            phys,
            dav: kit("efuse-dav.bin"),
        }),
        ..Model::default()
    });
    b.mem
        .poke(R_AX_SYS_WL_EFUSE_CTRL, Width::W16, u32::from(ctrl));
    b
}
/// A bring-up that got as far as probe_efuse: the cut read, the probe's power-on started
fn run() -> Bringup {
    let mut run = Bringup::new();
    run.chip = Some(ChipVer { cv: 1, acv: 1 });
    run.on = true;
    run
}
/// The writes of the stop's power-off: the RFE 5 SPS value and the power-off's WLON set
fn power_off_ops(b: &ChipBus, from: usize) -> (usize, usize) {
    let ops: Vec<_> = b.mem.trace[from..]
        .iter()
        .filter_map(|e| match e {
            TraceEntry::Op(o) => Some(*o),
            TraceEntry::Note(_) => None,
        })
        .collect();
    let sps = ops
        .iter()
        .filter(|o| o.kind == OpKind::Wr && o.a == R_AX_SPS_ANA_ON_CTRL2)
        .count();
    let wlon = ops
        .iter()
        .filter(|o| o.kind == OpKind::Rmw && o.a == R_AX_SYS_PW_CTRL && o.c & B_AX_EN_WLON != 0)
        .count();
    (sps, wlon)
}

#[test]
fn probe_efuse_sets_rfe_from_the_measured_chips_efuse() {
    let fw = firmware();
    let mut b = chip(0x2020, |_| {});
    let mut r = run();
    r.probe_efuse(&mut b, &fw).unwrap();
    let log = kit("efuse-log.bin");
    let maps = r.efuse_maps.as_ref().unwrap();
    assert_eq!(maps.log[..], log[..LOG_SIZE], "the dumped logical map");
    let ef = r.phy.efuse.as_ref().unwrap();
    assert_eq!(ef.dav_log[..], log[LOG_SIZE..], "the DAV map");
    assert_eq!((&ef.addr[..], ef.rfe_type, ef.valid), (&log[EF_MAC_ADDR_E..EF_MAC_ADDR_E + 6], 1, true));
    assert_eq!(r.rfe, EXPECT_RFE, "Bringup::rfe from the parsed efuse");
    assert!(
        r.phycap.is_some(),
        "the firmware's PHY capabilities asked for"
    );
    assert_eq!(r.pwr_opts().unwrap().rfe, 1);
}

#[test]
fn probe_efuse_takes_a_replaced_but_plausible_mac() {
    let fw = firmware();
    let mac = [0x00, 0xe0, 0x4c, 0x12, 0x34, 0x56];
    let mut b = chip(0x2020, |p| rewrite(p, EF_MAC_ADDR_E, &mac));
    let mut r = run();
    r.probe_efuse(&mut b, &fw).unwrap();
    let ef = r.phy.efuse.as_ref().unwrap();
    assert_eq!((ef.mac_text().as_str(), r.rfe), ("00:e0:4c:12:34:56", 1));
}

/// name, R_AX_SYS_WL_EFUSE_CTRL, efuse rewrites (offset, bytes), the refusal
type RefusalCase = (
    &'static str,
    u16,
    &'static [(usize, &'static [u8])],
    (&'static str, u32, u32),
);

#[test]
fn a_refused_efuse_leaves_rfe_0_and_the_stop_powers_off_with_it() {
    let fw = firmware();
    let cases: [RefusalCase; 5] = [
        (
            "rfe 2",
            0x2020,
            &[(EF_RFE_TYPE, &[2])],
            (NO_TABLES, EF_RFE_TYPE as u32, 2),
        ),
        // rfe 5 would take the power-off's SPS branch; refused, the stop must not
        (
            "rfe 5",
            0x2020,
            &[(EF_RFE_TYPE, &[5])],
            (NO_TABLES, EF_RFE_TYPE as u32, 5),
        ),
        (
            "no autoload",
            0x2000,
            &[],
            (NOT_AUTOLOADED, R_AX_SYS_WL_EFUSE_CTRL, 0x2000),
        ),
        (
            "multicast MAC",
            0x2020,
            &[(EF_MAC_ADDR_E, &[0x01, 0x00, 0x5e, 0x7f, 0x00, 0x01])],
            (NO_UNICAST, EF_MAC_ADDR_E as u32, 0x0100_5e7f),
        ),
        (
            "zero MAC",
            0x2020,
            &[(EF_MAC_ADDR_E, &[0; 6])],
            (NO_UNICAST, EF_MAC_ADDR_E as u32, 0),
        ),
    ];
    for (name, ctrl, patches, want) in cases {
        let mut b = chip(ctrl, |p| {
            for (off, bytes) in patches {
                rewrite(p, *off, bytes);
            }
        });
        let mut r = run();
        let e = r.probe_efuse(&mut b, &fw).unwrap_err();
        assert_eq!(refusal(e), want, "{name}");
        assert_eq!(r.rfe, 0, "{name}: rfe stays 0");
        assert!(r.phycap.is_none(), "{name}: no GET_FEATURE after a refusal");
        // the dumps and the parse are kept for the log
        assert!(r.efuse_maps.is_some() && r.phy.efuse.is_some(), "{name}");
        let from = b.mem.trace.len();
        r.stop(&mut b).unwrap();
        assert_eq!(
            power_off_ops(&b, from),
            (0, 1),
            "{name}: powered off, without the RFE 5 SPS write"
        );
    }
    // the check above sees the RFE 5 branch when rfe is 5
    let mut b = chip(0x2020, |_| {});
    let mut r = run();
    r.rfe = 5;
    let from = b.mem.trace.len();
    r.stop(&mut b).unwrap();
    assert_eq!(power_off_ops(&b, from), (1, 1));
}
