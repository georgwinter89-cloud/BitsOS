//! The clock rule with a fake clock, the table both ways and its hostile forms, names out of certificates.
use super::*;
use alloc::vec;

const T: i64 = 1_800_000_000;
const C: &str = "ab12cd34ef567890ab12cd34ef567890ab12cd34ef567890ab12cd34ef567890";

fn g(until: i64, set_at: i64) -> Grant {
    Grant { client: C.into(), until, set_at, nvme: false }
}

#[test]
fn a_grant_holds_until_its_time_by_both_clocks() {
    let x = g(T + 3600, T);
    let seen = Seen { wall: T + 10, mono: 500 };
    assert_eq!(judge(&x, T + 10, seen, 500), Ok(()));
    assert_eq!(judge(&x, T + 3599, seen, 500 + 3589), Ok(()));
    assert_eq!(left_s(&x, T + 3000, seen, 500 + 2990), 600);
    assert_eq!(judge(&x, T + 3600, seen, 500 + 3590), Err(Ended::Expired));
    assert_eq!(left_s(&x, T + 3600, seen, 500 + 3590), 0);
}

#[test]
fn the_clock_going_back_or_jumping_ends_it() {
    let x = g(T + 3600, T);
    let seen = Seen { wall: T + 10, mono: 0 };
    // Back by 20 min after 30 min: the wall says 40 min left, the monotonic time says 30 min: 30 counts.
    assert_eq!(judge(&x, T + 10 + 1800 - 1200, seen, 1800), Ok(()));
    assert_eq!(left_s(&x, T + 610, seen, 1800), 3600 - 10 - 1800);
    assert_eq!(judge(&x, T + 610, seen, 3590), Err(Ended::Expired), "the monotonic end holds although the wall clock went back");
    // Back before the moment it was set.
    assert_eq!(judge(&x, T - 1, seen, 30), Err(Ended::ClockBack));
    // A jump by more than the grant's length, forward or back, ends it.
    assert_eq!(judge(&x, T + 10 + 3601, seen, 0), Err(Ended::ClockJump));
    assert_eq!(judge(&g(T + 7 * 86_400, T), T - 8 * 86_400, Seen { wall: T, mono: 0 }, 5), Err(Ended::ClockJump));
    // A jump smaller than its length forward just ends it at the wall clock's end.
    assert_eq!(judge(&x, T + 3600, Seen { wall: T + 100, mono: 0 }, 100), Err(Ended::Expired));
}

#[test]
fn bad_lengths_are_refused() {
    let seen = Seen { wall: T, mono: 0 };
    assert_eq!(judge(&g(T + MAX_S + 1, T), T, seen, 0), Err(Ended::TooLong));
    assert_eq!(judge(&g(T, T), T, seen, 0), Err(Ended::TooLong));
    assert_eq!(judge(&g(T - 5, T), T, seen, 0), Err(Ended::TooLong));
    assert_eq!(judge(&g(i64::MAX, i64::MIN), T, seen, 0), Err(Ended::TooLong));
    assert_eq!(judge(&g(T + MAX_S, T), T + 1, seen, 1), Ok(()));
}

#[test]
fn the_table_both_ways_and_hostile() {
    let v = vec![g(T + 60, T), Grant { client: C.replace('a', "b"), until: T + 7200, set_at: T + 1, nvme: true }];
    let line = encode_table(&v);
    let Some(crate::gate::Command::Grants(back)) = crate::gate::parse_command(&line) else { panic!("grants") };
    assert_eq!(back, v);
    assert_eq!(crate::gate::parse_command(b"grants {\"grants\":[]}"), Some(crate::gate::Command::Grants(vec![])));
    let twice = alloc::format!("grants {{\"grants\":[{{\"client\":\"{C}\",\"until\":5,\"set_at\":1}},{{\"client\":\"{C}\",\"until\":6,\"set_at\":1}}]}}");
    for bad in [&b"grants"[..], b"grants {}", b"grants {\"grants\":[{\"client\":\"AB\",\"until\":1,\"set_at\":0}]}", b"grants {\"grants\":[{\"client\":\"x\"}]}", twice.as_bytes()] {
        assert_eq!(crate::gate::parse_command(bad), None, "{:?}", core::str::from_utf8(bad));
    }
    let many: Vec<Grant> = (0..=TABLE_MAX).map(|i| Grant { client: alloc::format!("{i:064x}"), ..g(T + 1, T) }).collect();
    assert_eq!(crate::gate::parse_command(&encode_table(&many)), None, "more than TABLE_MAX");
    assert!(is_client(C) && !is_client(&C.to_uppercase()) && !is_client(&C[1..]));
    assert_eq!(fp_short(C), "ab12 cd34 ef56 7890");
}

#[test]
fn status_and_local_time() {
    let s = Status { granted: true, until: T, left_s: 3 * 3600 + 120, nvme: true, local: local_text(T, 60), loaded: true };
    assert_eq!(decode_status(&encode_status(&s)), Some(s.clone()));
    assert!(status_text(&s).contains("3 h 2 min left") && status_text(&s).contains("NVMe write gate is open"));
    let none = Status { granted: false, until: 0, left_s: 0, nvme: false, local: String::new(), loaded: true };
    assert!(status_text(&none).starts_with("no full access") && status_text(&none).contains("request_access"));
    // MC-11: before the installer's first table the answer says so; a pre-MC-11 kernel's answer counts as loaded.
    let early = Status { loaded: false, ..none.clone() };
    assert_eq!(decode_status(&encode_status(&early)), Some(early.clone()));
    assert!(status_text(&early).starts_with(NOT_LOADED), "{}", status_text(&early));
    let old = r#"{"granted":false,"until":0,"left_s":0,"nvme":false,"local":""}"#;
    assert_eq!(decode_status(old), Some(none.clone()));
    // 1_800_000_000 = 2027-01-15 08:00:00 UTC.
    assert_eq!(local_text(T, 0), "2027-01-15 08:00");
    assert_eq!(local_text(T, 60), "2027-01-15 09:00");
    assert_eq!(local_text(0, 0), "1970-01-01 00:00");
    assert_eq!(local_text(951_782_400, 0), "2000-02-29 00:00");
}

fn tlv_of(tag: u8, parts: &[&[u8]]) -> Vec<u8> {
    let body: Vec<u8> = parts.concat();
    let mut v = vec![tag];
    if body.len() < 0x80 {
        v.push(body.len() as u8);
    } else {
        v.extend_from_slice(&[0x82, (body.len() >> 8) as u8, body.len() as u8]);
    }
    v.extend_from_slice(&body);
    v
}

fn cert_with(cn: &[u8], version: bool) -> Vec<u8> {
    let oid = tlv_of(0x06, &[&[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x04, 0x03, 0x02]]);
    let alg = tlv_of(0x30, &[&oid]);
    let cn_oid = tlv_of(0x06, &[&[0x55, 0x04, 0x03]]);
    let ou = tlv_of(0x31, &[&tlv_of(0x30, &[&tlv_of(0x06, &[&[0x55, 0x04, 0x0b]]), &tlv_of(0x0c, &[b"desk"])])]);
    let name = tlv_of(0x30, &[&ou, &tlv_of(0x31, &[&tlv_of(0x30, &[&cn_oid, &tlv_of(0x0c, &[cn])])])]);
    let validity = tlv_of(0x30, &[&tlv_of(0x17, &[b"000101000000Z"])]);
    let ver = if version { tlv_of(0xa0, &[&tlv_of(0x02, &[&[2]])]) } else { Vec::new() };
    let tbs = tlv_of(0x30, &[&ver, &tlv_of(0x02, &[&[7; 16]]), &alg, &name, &validity, &name, &tlv_of(0x30, &[&[0u8; 200]])]);
    tlv_of(0x30, &[&tbs, &alg, &tlv_of(0x03, &[&[0, 1, 2]])])
}

#[test]
fn names_out_of_certificates() {
    assert_eq!(common_name(&cert_with(b"bitsos-desk", true)).as_deref(), Some("bitsos-desk"));
    assert_eq!(common_name(&cert_with(b"no version", false)).as_deref(), Some("no version"));
    assert_eq!(common_name(&cert_with(b"line\nbreak\x07 and a very long name that goes on and on", true)).unwrap(), "linebreak and a very long name that goes");
    let c = cert_with(b"x", true);
    for cut in 0..c.len() {
        let _ = common_name(&c[..cut]);
    }
    assert_eq!(common_name(&[]), None);
    assert_eq!(common_name(&[0x30, 0x84, 0xff, 0xff, 0xff, 0xff]), None);
    assert_eq!(common_name(&[0x30, 0x85, 1, 1, 1, 1, 1]), None);
}
