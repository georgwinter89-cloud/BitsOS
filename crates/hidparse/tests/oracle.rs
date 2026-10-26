//! V2 against V1's models as oracle (plan/spec/usb.md §10): the node driver oracle/v1-oracle.mjs
//! prints V1's answers for the named descriptors of hidparse-test.mjs, every prefix of DragonRise
//! and DualShock 4, 6,000 random/mutated descriptors and gamepad-model's 600+ report cases.
//! Set HIDPARSE_SKIP_V1_ORACLE=1 to skip where node or the V1 tree is missing.

mod common;

use common::json::{self, Json, hex};
use common::v1::{Pad, differences, pad_bits, v1_view};
use hidparse::{InputMap, InputState, parse};
use std::collections::BTreeMap;
use std::process::Command;
use std::sync::OnceLock;

fn oracle() -> Option<&'static Json> {
    static ORACLE: OnceLock<Option<Json>> = OnceLock::new();
    ORACLE
        .get_or_init(|| {
            if std::env::var_os("HIDPARSE_SKIP_V1_ORACLE").is_some() {
                eprintln!("HIDPARSE_SKIP_V1_ORACLE set: V1 oracle not run");
                return None;
            }
            let dir = env!("CARGO_MANIFEST_DIR");
            let out = Command::new("node")
                .arg(format!("{dir}/oracle/v1-oracle.mjs"))
                .output()
                .expect("node not found (set HIDPARSE_SKIP_V1_ORACLE=1 to skip the V1 oracle)");
            assert!(
                out.status.success(),
                "v1-oracle.mjs failed:\n{}",
                String::from_utf8_lossy(&out.stderr)
            );
            Some(json::parse(&String::from_utf8(out.stdout).unwrap()))
        })
        .as_ref()
}

/// Every descriptor also goes through the mapping and a few reports: none of it may panic.
fn exercise(desc: &[u8]) {
    let Ok(d) = parse(desc) else { return };
    let map = InputMap::new(&d);
    let mut state = InputState::new(&d, &map, 64);
    let mut events = Vec::new();
    for (i, len) in [0usize, 1, 3, 8, 64].into_iter().enumerate() {
        let report: Vec<u8> = (0..len)
            .map(|k| {
                desc.get((k * 7 + i) % desc.len().max(1))
                    .copied()
                    .unwrap_or(0xA5)
            })
            .collect();
        let _ = state.process(&d, &map, &report, &mut events);
    }
}

#[test]
fn parser_matches_v1_where_the_models_overlap() {
    let Some(o) = oracle() else { return };
    let report_bytes = o.get("report_bytes").num() as u64;
    let (mut compared, mut pads, mut total) = (0, 0, 0);
    let mut skipped: BTreeMap<&str, usize> = BTreeMap::new();
    let mut bad = Vec::new();
    for e in o.get("parse").arr() {
        total += 1;
        let (group, name) = (e.get("group").str(), e.get("name").str());
        let desc = hex(e.get("desc").str());
        let want = Pad::from_json(e.get("v1"));
        if group == "named" {
            assert_eq!(
                want,
                Pad::from_json(e.get("expect")),
                "V1's model disagrees with hidparse-test.mjs: {name}"
            );
        }
        exercise(&desc);
        let diffs = differences(&desc);
        if !diffs.is_empty() {
            for d in diffs {
                *skipped.entry(d).or_default() += 1;
            }
            continue;
        }
        let got = parse(&desc).ok().and_then(|d| v1_view(&d, report_bytes));
        compared += 1;
        pads += want.is_some() as usize;
        if got != want {
            bad.push(format!(
                "{group} / {name}: {}\n  V1 {want:?}\n  V2 {got:?}",
                e.get("desc").str()
            ));
        }
    }
    eprintln!(
        "V1 oracle: {compared} of {total} descriptors compared ({pads} pads), not compared: {skipped:#?}"
    );
    assert!(
        bad.is_empty(),
        "{} differ from V1:\n{}",
        bad.len(),
        bad[..bad.len().min(10)].join("\n")
    );
    assert!(
        compared * 10 >= total * 9,
        "too few descriptors in the overlap: {compared} of {total}"
    );
    assert!(pads >= 400, "too few pads compared: {pads}");
}

#[test]
fn extraction_matches_v1_pad_model() {
    let Some(o) = oracle() else { return };
    let cases = o.get("decode").arr();
    let mut bad = Vec::new();
    for c in cases {
        let pad = Pad::from_json(c.get("layout")).unwrap();
        let len = c.get("len").num() as usize;
        let report = hex(c.get("report").str());
        let want = (!c.get("want").is_null()).then(|| c.get("want").num() as u32);
        let got = pad_bits(&pad, &report[..len.min(report.len())]);
        if got != want {
            bad.push(format!(
                "{}: V1 {want:?}, V2 {got:?} ({pad:?})",
                c.get("name").str()
            ));
        }
    }
    eprintln!("V1 pad model: {} report cases", cases.len());
    assert!(cases.len() >= 600);
    assert!(
        bad.is_empty(),
        "{} differ:\n{}",
        bad.len(),
        bad[..bad.len().min(10)].join("\n")
    );
}
