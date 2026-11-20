//! Stamps the version the desk gave the build (`BITSOS_VERSION`, `BITSOS_VERSION_DATE`) into `$OUT_DIR/stamp.rs`.
//! Unset or empty: 0, a development build without a number. Anything else that is no number in 1..=99999999 (the
//! largest generation folder the boot medium's rules allow) or no date YYYY-MM-DD stops the build: a typo must not
//! become a version.
use std::io::Write;

fn main() {
    println!("cargo:rerun-if-env-changed=BITSOS_VERSION");
    println!("cargo:rerun-if-env-changed=BITSOS_VERSION_DATE");
    let n: u32 = match std::env::var("BITSOS_VERSION").ok().filter(|v| !v.trim().is_empty()) {
        None => 0,
        Some(v) => {
            let v = v.trim();
            match v.parse::<u32>() {
                Ok(n) if v.bytes().all(|b| b.is_ascii_digit()) && (1..=99_999_999).contains(&n) => n,
                _ => panic!("BITSOS_VERSION={v:?}: a version is a whole number from 1 to 99999999"),
            }
        }
    };
    let date = std::env::var("BITSOS_VERSION_DATE").unwrap_or_default();
    let date = date.trim();
    let ok = date.is_empty() || (date.len() == 10 && date.bytes().enumerate().all(|(i, b)| if i == 4 || i == 7 { b == b'-' } else { b.is_ascii_digit() }));
    if !ok {
        panic!("BITSOS_VERSION_DATE={date:?}: a date is YYYY-MM-DD");
    }
    if n == 0 && !date.is_empty() {
        panic!("BITSOS_VERSION_DATE without BITSOS_VERSION: only a numbered version has a date");
    }
    let uts = if n == 0 { String::from("#1 SMP PREEMPT_DYNAMIC BitsOS V2") } else if date.is_empty() {
        format!("#{n} SMP PREEMPT_DYNAMIC BitsOS V2 Version {n}")
    } else {
        format!("#{n} SMP PREEMPT_DYNAMIC BitsOS V2 Version {n} {date}")
    };
    let out = std::path::Path::new(&std::env::var("OUT_DIR").expect("OUT_DIR")).join("stamp.rs");
    let mut f = std::fs::File::create(&out).expect("stamp.rs");
    writeln!(f, "pub const NUMBER: u32 = {n};").unwrap();
    writeln!(f, "pub const DATE: &str = {date:?};").unwrap();
    writeln!(f, "pub const UTS_VERSION: &str = {uts:?};").unwrap();
    writeln!(f, "pub const MARK: [u8; MARK_LEN] = *b\"BITSOS-VERSION={n:08};\\xb7\\n\";").unwrap();
}
