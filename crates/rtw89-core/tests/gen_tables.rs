//! A fingerprint of every generated table (src/gen/*.rs) against the JS
//! driver's values (gen/spot-tables.mjs, straight from the JS modules): rows,
//! integer count and sum, a row-order weighted sum, and the first and last row.
//! Also checks that it covers every table src/gen declares. Needs node and the
//! read-only WLAN tools; RTW89_SKIP_FAKE=1 skips the node part with a SKIPPED line.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Command;

use rtw89_core::tables::*;

/// RTW89_SKIP_FAKE=1 skips the node parts; the line goes to the real stderr,
/// past the test harness's capture, so a skip is always seen
fn skip_js(what: &str) -> bool {
    use std::io::Write;
    if std::env::var_os("RTW89_SKIP_FAKE").is_some_and(|v| v == "1") {
        let _ = writeln!(std::io::stderr(), "SKIPPED (RTW89_SKIP_FAKE=1): {what}");
        return true;
    }
    false
}

/// The integers in a Debug text, in order: numbers, `true`/`false` as 1/0;
/// strings, field and variant names are skipped (gen/spot-tables.mjs `ints`)
fn ints(debug: &str) -> Vec<i128> {
    let b = debug.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if c == b'"' {
            i += 1;
            while i < b.len() && b[i] != b'"' {
                i += if b[i] == b'\\' { 2 } else { 1 };
            }
            i += 1;
        } else if c.is_ascii_alphabetic() || c == b'_' {
            let s = i;
            while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_') {
                i += 1;
            }
            match &debug[s..i] {
                "true" => out.push(1),
                "false" => out.push(0),
                _ => {}
            }
        } else if c.is_ascii_digit() || (c == b'-' && b.get(i + 1).is_some_and(u8::is_ascii_digit))
        {
            let s = i;
            i += 1;
            while i < b.len() && b[i].is_ascii_digit() {
                i += 1;
            }
            out.push(debug[s..i].parse().unwrap());
        } else {
            i += 1;
        }
    }
    out
}

/// len n sum wsum first_n first_sum last_n last_sum, as gen/spot-tables.mjs `fp`
fn fp<T: std::fmt::Debug>(rows: &[T]) -> Vec<i128> {
    let per: Vec<Vec<i128>> = rows.iter().map(|r| ints(&format!("{r:?}"))).collect();
    let sum = |r: &[i128]| r.iter().sum::<i128>();
    let all: Vec<i128> = per.concat();
    let w: i128 = per
        .iter()
        .enumerate()
        .map(|(i, r)| (i as i128 + 1) * sum(r))
        .sum();
    let (f, l) = (
        per.first().cloned().unwrap_or_default(),
        per.last().cloned().unwrap_or_default(),
    );
    vec![
        rows.len() as i128,
        all.len() as i128,
        sum(&all),
        w,
        f.len() as i128,
        sum(&f),
        l.len() as i128,
        sum(&l),
    ]
}

macro_rules! tables {
    ($m:ident; $($md:ident :: $name:ident),* $(,)?) => {
        $( $m.insert(concat!(stringify!($md), "::", stringify!($name)).to_string(), fp(&$md::$name[..])); )*
    };
}

fn rust_tables() -> BTreeMap<String, Vec<i128>> {
    let mut m = BTreeMap::new();
    m.insert(
        "dmac::HFC_PUB".into(),
        fp(std::slice::from_ref(&dmac::HFC_PUB)),
    );
    m.insert(
        "dmac::HFC_PREC".into(),
        fp(std::slice::from_ref(&dmac::HFC_PREC)),
    );
    tables!(m; dmac::DLE_MEM, dmac::HFC_CH, fw_feat::FEAT_TBL, txrx::BITRATES, txrx::PHYST_IE_LEN,
        btc::S_DEF, btc::T_DEF_OFF, btc::MON_REG, btc::CXTBL, btc::OFF_TBL, btc::RF_UL, btc::RF_DL, btc::WL_RSSI_THRES,
        chan::RULES_DE_2G, chan::RULES_DE_5G, chan::RULES_WW_2G, chan::RULES_WW_5G, chan::SCO_BARKER, chan::SCO_CCK,
        chan::LNA_REGS, chan::TIA_REGS, chan::TXFIR, chan::CH_BASE, chan::DFIR_FLAT, chan::DFIR_SHARP,
        chan::DFIR_SHARP14, chan::TXPWR_CONF,
        phy::PMAC_HT20_MCS7, phy::PREAGC_EN, phy::PREAGC_DIS, phy::DELAY_US, phy::CH_BASE, phy::IGI_RSSI_TH,
        phy::FA_TH_2G, phy::FA_TH_5G, phy::FA_TH_NOLINK, phy::TRIM_ADDR, phy::COMP_ADDRS,
        rfk1::A_GAIN_BB, rfk1::A_IDXATTC1, rfk1::A_IDXATTC2, rfk1::A_IDXRXGAIN, rfk1::A_ITQT, rfk1::A_POWER_RANGE,
        rfk1::A_TRACK_RANGE, rfk1::AFE_INIT_DEFS, rfk1::BACKUP_BB_REGS, rfk1::BACKUP_RF_REGS, rfk1::CHECK_ADDC_DEFS_A,
        rfk1::CHECK_ADDC_DEFS_B, rfk1::CHECK_DADC_DIS_DEFS_A, rfk1::CHECK_DADC_DIS_DEFS_B, rfk1::CHECK_DADC_EN_DEFS_A,
        rfk1::CHECK_DADC_EN_DEFS_B, rfk1::DACK_S0_1_DEFS, rfk1::DACK_S0_2_DEFS, rfk1::DACK_S0_3_DEFS,
        rfk1::DACK_S1_1_DEFS, rfk1::DACK_S1_2_DEFS, rfk1::DACK_S1_3_DEFS, rfk1::G_GAIN_BB, rfk1::G_IDXATTC1,
        rfk1::G_IDXATTC2, rfk1::G_IDXRXGAIN, rfk1::G_ITQT, rfk1::G_POWER_RANGE, rfk1::G_TRACK_RANGE,
        rfk1::RESTORE_NONDBCC_PATH01, rfk1::SET_NONDBCC_PATH01,
        rfk2::DPK_AFE_DEFS, rfk2::DPK_AFE_RESTORE_DEFS, rfk2::DPK_KIP_DEFS,
        rfk2::TSSI_ALIGN_A_2G_ALL_DEFS, rfk2::TSSI_ALIGN_A_2G_PART_DEFS, rfk2::TSSI_ALIGN_A_5G1_ALL_DEFS,
        rfk2::TSSI_ALIGN_A_5G1_PART_DEFS, rfk2::TSSI_ALIGN_A_5G2_ALL_DEFS, rfk2::TSSI_ALIGN_A_5G2_PART_DEFS,
        rfk2::TSSI_ALIGN_A_5G3_ALL_DEFS, rfk2::TSSI_ALIGN_A_5G3_PART_DEFS, rfk2::TSSI_ALIGN_B_2G_ALL_DEFS,
        rfk2::TSSI_ALIGN_B_2G_PART_DEFS, rfk2::TSSI_ALIGN_B_5G1_ALL_DEFS, rfk2::TSSI_ALIGN_B_5G1_PART_DEFS,
        rfk2::TSSI_ALIGN_B_5G2_ALL_DEFS, rfk2::TSSI_ALIGN_B_5G2_PART_DEFS, rfk2::TSSI_ALIGN_B_5G3_ALL_DEFS,
        rfk2::TSSI_ALIGN_B_5G3_PART_DEFS, rfk2::TSSI_DAC_GAIN_DEFS_A, rfk2::TSSI_DAC_GAIN_DEFS_B,
        rfk2::TSSI_DCK_DEFS_A, rfk2::TSSI_DCK_DEFS_B, rfk2::TSSI_INIT_TXPWR_DEFS_A, rfk2::TSSI_INIT_TXPWR_DEFS_B,
        rfk2::TSSI_INIT_TXPWR_HE_TB_DEFS_A, rfk2::TSSI_INIT_TXPWR_HE_TB_DEFS_B, rfk2::TSSI_SLOPE_A_DEFS_2G,
        rfk2::TSSI_SLOPE_A_DEFS_5G, rfk2::TSSI_SLOPE_B_DEFS_2G, rfk2::TSSI_SLOPE_B_DEFS_5G, rfk2::TSSI_SLOPE_DEFS_A,
        rfk2::TSSI_SLOPE_DEFS_B, rfk2::TSSI_SYS_A_DEFS_2G, rfk2::TSSI_SYS_A_DEFS_5G, rfk2::TSSI_SYS_B_DEFS_2G,
        rfk2::TSSI_SYS_B_DEFS_5G, rfk2::TSSI_SYS_DEFS,
        rfk2::PICK_TSSI_SYS_PATH, rfk2::PICK_TSSI_SLOPE_ORG, rfk2::PICK_TSSI_INIT_TXPWR, rfk2::PICK_TSSI_INIT_TXPWR_HE_TB,
        rfk2::PICK_TSSI_DCK, rfk2::PICK_TSSI_DAC_GAIN, rfk2::PICK_TSSI_SLOPE, rfk2::PICK_TSSI_ALIGN,
        rfk2::DE_CCK_LONG, rfk2::DE_CCK_SHORT, rfk2::DE_MCS20M, rfk2::DE_MCS40M, rfk2::DE_MCS80M, rfk2::DE_MCS80M80M,
        rfk2::DE_MCS5M, rfk2::DE_MCS10M, rfk2::CCK_GROUPS, rfk2::OFDM_GROUPS, rfk2::TRIM_GROUPS,
        rfk2::TSSI_TRIGGER, rfk2::TSSI_CW_RPT_ADDR, rfk2::TSSI_CW_DEFAULT_ADDR, rfk2::TSSI_CW_DEFAULT_MASK,
        rfk2::ALIM_BB_REG, rfk2::POWER, rfk2::KIP_REG,
        trk::TRK_CFG);
    m
}

/// Every public table of src/gen (statics, and consts that are arrays), by its file's text
fn declared_tables() -> Vec<String> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/gen");
    let mut out = Vec::new();
    for e in std::fs::read_dir(&dir).unwrap() {
        let p = e.unwrap().path();
        let module = p.file_stem().unwrap().to_str().unwrap().to_string();
        if module == "regs" || module == "mod" {
            continue; // regs: names, checked above; mod: the types
        }
        for line in std::fs::read_to_string(&p).unwrap().lines() {
            let Some(rest) = line
                .strip_prefix("pub static ")
                .or(line.strip_prefix("pub const "))
            else {
                continue;
            };
            let (name, ty) = rest.split_once(": ").unwrap();
            if line.starts_with("pub static ") || ty.starts_with('[') {
                out.push(format!("{module}::{name}"));
            }
        }
    }
    out.sort();
    out
}

#[test]
fn every_generated_table_matches_the_js_tables() {
    let rust = rust_tables();
    assert_eq!(
        rust.keys().cloned().collect::<Vec<_>>(),
        declared_tables(),
        "the test covers every table src/gen declares"
    );
    if skip_js("the JS table fingerprints") {
        return;
    }
    let script = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("gen/spot-tables.mjs");
    let out = Command::new("node")
        .arg(&script)
        .output()
        .expect("node (RTW89_SKIP_FAKE=1 skips)");
    assert!(
        out.status.success(),
        "spot-tables.mjs: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let js: BTreeMap<String, Vec<i128>> = String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .map(|l| {
            let mut w = l.split(' ');
            let k = w.next().unwrap().strip_prefix("tbl.").unwrap().to_string();
            (k, w.map(|v| v.parse().unwrap()).collect())
        })
        .collect();
    assert_eq!(
        js.keys().collect::<Vec<_>>(),
        rust.keys().collect::<Vec<_>>(),
        "the same tables on both sides"
    );
    const WHAT: [&str; 8] = [
        "rows",
        "integers",
        "sum",
        "weighted sum",
        "first row integers",
        "first row sum",
        "last row integers",
        "last row sum",
    ];
    for (k, v) in &js {
        for (i, what) in WHAT.iter().enumerate() {
            assert_eq!(rust[k][i], v[i], "{k}: {what}");
        }
    }
}
