//! Spot checks of the generated tables (src/gen/*.rs) against the JS driver's
//! own values, read by gen/spot.mjs straight from the JS modules and
//! kit/regs.mjs (every table as a whole: tests/gen_tables.rs). Needs node and the read-only WLAN tools; RTW89_SKIP_FAKE=1 skips the node
//! parts with a SKIPPED line on stderr (refused under BITSOS_GATE or CI).

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Command;

use rtw89_core::tables::*;

fn rfk_last(e: &Rfk) -> i64 {
    match *e {
        Rfk::Wm { data, .. } | Rfk::Wrf { data, .. } => data.into(),
        Rfk::Ws { bits, .. } | Rfk::Wc { bits, .. } => bits.into(),
        Rfk::Delay { us } => us.into(),
    }
}

/// The same keys gen/spot.mjs prints, from the Rust tables
fn ours(regs: &[&str]) -> BTreeMap<String, i64> {
    let mut m = BTreeMap::new();
    let mut put = |k: &str, v: i64| {
        m.insert(k.to_string(), v);
    };
    for n in regs {
        put(
            &format!("reg.{n}"),
            reg_value(n).unwrap_or_else(|| panic!("{n} missing in regs.rs")),
        );
    }
    let scc = dmac::DLE_MEM.iter().find(|d| d.mode == "SCC").unwrap();
    let dlfw = dmac::DLE_MEM.iter().find(|d| d.mode == "DLFW").unwrap();
    put("dmac.scc.ple_max.4", scc.ple_max[4].into());
    put("dmac.dlfw.wde.unlnk", dlfw.wde_size.unlnk_pge_num.into());
    put("dmac.hfc_ch.len", dmac::HFC_CH.len() as i64);
    put("dmac.hfc_ch.12.min", dmac::HFC_CH[12].min.into());
    put("dmac.hfc_pub.pub_max", dmac::HFC_PUB.pub_max.into());
    put("btc.rsn.ACT1_WORK", btc::rsn::ACT1_WORK.into());
    put("btc.wscb.ALL", btc::wscb::ALL.into());
    put("btc.cxtbl.3", btc::CXTBL[3].into());
    put("btc.cxtbl.len", btc::CXTBL.len() as i64);
    put("btc.s_def.1.cxtbl", btc::S_DEF[1].cxtbl.into());
    put("btc.s_def.len", btc::S_DEF.len() as i64);
    put("btc.mon_reg.12.offset", btc::MON_REG[12].offset.into());
    put("btc.rf_dl.5.1", btc::RF_DL[5][1].into());
    put("chan.sco_cck.13", chan::SCO_CCK[13].into());
    put("chan.txfir.2.ch14", chan::TXFIR[2].ch14.into());
    put("chan.lna.6.g5.1", chan::LNA_REGS[6].g5[1].into());
    put("chan.rules.de.5g.2.last", chan::RULES_DE_5G[2].last.into());
    put(
        "chan.rules.ww.2g.1.no_ir",
        chan::RULES_WW_2G[1].no_ir as i64,
    );
    // fw-parse.mjs featuresFor(0.29.29.18), the firmware on the stick, from the generated rows
    let code = (29u32 << 16) | (29 << 8) | 18;
    let on: std::collections::BTreeSet<&str> = fw_feat::FEAT_TBL
        .iter()
        .filter(|r| match r.cond {
            FeatCond::Ge => code >= r.code,
            FeatCond::Le => code <= r.code,
            FeatCond::Lt => code < r.code,
        })
        .map(|r| r.name)
        .collect();
    put("fw_feat.29_29_18.count", on.len() as i64);
    put(
        "fw_feat.29_29_18.scan_offload",
        on.contains("SCAN_OFFLOAD") as i64,
    );
    put("phy.pmac.len", phy::PMAC_HT20_MCS7.len() as i64);
    put("phy.pmac.7.data", phy::PMAC_HT20_MCS7[7].data.into());
    put(
        "phy.delay_us.252",
        phy::DELAY_US
            .iter()
            .find(|(a, _)| *a == 252)
            .unwrap()
            .1
            .into(),
    );
    put("phy.ef.mac_addr_e", phy::EF_MAC_ADDR_E as i64);
    put("rfk1.afeInitDefs.len", rfk1::AFE_INIT_DEFS.len() as i64);
    put("rfk1.afeInitDefs.0.data", rfk_last(&rfk1::AFE_INIT_DEFS[0]));
    put("rfk1.dackS0_1Defs.len", rfk1::DACK_S0_1_DEFS.len() as i64);
    put(
        "rfk1.setNondbccPath01.len",
        rfk1::SET_NONDBCC_PATH01.len() as i64,
    );
    put("rfk1.aIdxrxgain.2", rfk1::A_IDXRXGAIN[2].into());
    put("rfk1.backupRfRegs.len", rfk1::BACKUP_RF_REGS.len() as i64);
    let Rfk::Wrf { path, .. } = rfk1::CHECK_DADC_EN_DEFS_B[6] else {
        panic!("not WRF")
    };
    put("rfk1.checkDadcEnDefsB.6.path", path.into());
    put(
        "rfk1.checkDadcEnDefsB.6.data",
        rfk_last(&rfk1::CHECK_DADC_EN_DEFS_B[6]),
    );
    put(
        "rfk1.dackS0_1Defs.17.us",
        rfk_last(&rfk1::DACK_S0_1_DEFS[17]),
    );
    put(
        "rfk2.tssi_dac_gain_defs_b.len",
        rfk2::TSSI_DAC_GAIN_DEFS_B.len() as i64,
    );
    let Rfk::Wm { mask, .. } = rfk2::TSSI_SYS_B_DEFS_5G[0] else {
        panic!("not WM")
    };
    put("rfk2.tssi_sys_b_defs_5g.0.mask", mask.into());
    let Rfk::Wm { addr, .. } = rfk2::DPK_AFE_DEFS[28] else {
        panic!("not WM")
    };
    put("rfk2.dpk_afe_defs.28.addr", addr.into());
    put("rfk2.ofdm_groups.len", rfk2::OFDM_GROUPS.len() as i64);
    put("rfk2.kip_reg.2", rfk2::KIP_REG[2].into());
    let first = trk::TRK_CFG.iter().position(Option::is_some).unwrap();
    put("trk.first_type", first as i64);
    put("trk.first.0.29", trk::TRK_CFG[first].unwrap()[0][29].into());
    put(
        "trk.13.0.0",
        trk::TRK_CFG[13].map_or(-999, |r| r[0][0].into()),
    );
    put("txrx.bitrates.len", txrx::BITRATES.len() as i64);
    put(
        "txrx.bitrates.last",
        (*txrx::BITRATES.last().unwrap()).into(),
    );
    put("txrx.physt.11", txrx::PHYST_IE_LEN[11].into());
    m
}

/// RTW89_SKIP_FAKE=1 skips the node parts; the line goes to the real stderr,
/// past the test harness's capture, so a skip is always seen
fn skip_js(what: &str) -> bool {
    use std::io::Write;
    if std::env::var_os("RTW89_SKIP_FAKE").is_some_and(|v| v == "1") {
        // a gate run never skips the JS oracle: refuse the variable there
        for gate in ["BITSOS_GATE", "CI"] {
            assert!(
                std::env::var_os(gate).is_none_or(|v| v.is_empty() || v == "0"),
                "RTW89_SKIP_FAKE=1 together with {gate}: the gate must run the JS oracle"
            );
        }
        let _ = writeln!(std::io::stderr(), "SKIPPED (RTW89_SKIP_FAKE=1): {what}");
        return true;
    }
    false
}

#[test]
fn generated_tables_match_the_js_driver() {
    if skip_js("the JS spot check") {
        return;
    }
    // every 61st register name, the signed ones, and the ones the bring-up prefix uses
    let mut regs: Vec<&str> = regs::NAMES.iter().step_by(61).map(|(n, _)| *n).collect();
    regs.extend(regs::NAMES.iter().filter(|(_, v)| *v < 0).map(|(n, _)| *n));
    regs.extend([
        "R_AX_SYS_CFG1",
        "R_AX_IC_PWR_STATE",
        "R_AX_WLAN_XTAL_SI_CTRL",
        "B_AX_WL_XTAL_SI_DATA_MASK",
        "R_AX_RXQ_RXBD_IDX",
    ]);
    let spot = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("gen/spot.mjs");
    let out = Command::new("node")
        .arg(&spot)
        .args(&regs)
        .output()
        .expect("node (RTW89_SKIP_FAKE=1 skips)");
    assert!(
        out.status.success(),
        "spot.mjs: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let js: BTreeMap<String, i64> = String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .map(|l| {
            let (k, v) = l.split_once(' ').unwrap();
            (k.to_string(), v.parse().unwrap())
        })
        .collect();
    let rust = ours(&regs);
    assert!(js.len() > 60, "too few spot values: {}", js.len());
    assert_eq!(
        js.keys().collect::<Vec<_>>(),
        rust.keys().collect::<Vec<_>>(),
        "the same keys on both sides"
    );
    for (k, v) in &js {
        assert_eq!(rust[k], *v, "{k}");
    }
    // the typed constants are the same numbers as the name list
    assert_eq!(
        i64::from(regs::R_AX_SYS_CFG1),
        reg_value("R_AX_SYS_CFG1").unwrap()
    );
}

#[test]
fn name_list_is_sorted_unique_and_complete() {
    let names = regs::NAMES;
    assert!(
        names.windows(2).all(|w| w[0].0 < w[1].0),
        "sorted, no duplicates"
    );
    assert_eq!(names.len(), 2613, "the names V1's regs.inc has");
    assert_eq!(reg_value("R_AX_IC_PWR_STATE"), Some(0x3F0));
    assert_eq!(reg_value("NOT_A_REGISTER"), None);
    assert_eq!(reg_value(""), None);
}

#[test]
fn generated_files_are_up_to_date() {
    if skip_js("emit.mjs --check") {
        return;
    }
    // no write permission at all: --check only reads
    let emit = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("gen/emit.mjs");
    let out = Command::new("node")
        .args(["--permission", "--allow-fs-read=*"])
        .arg(&emit)
        .arg("--check")
        .output()
        .expect("node");
    assert!(
        out.status.success(),
        "emit.mjs --check: {}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}
