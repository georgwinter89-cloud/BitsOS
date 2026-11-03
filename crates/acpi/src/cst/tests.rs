use super::build::{self, State, MINIPC as MINIPC_STATES};
use super::*;
use alloc::vec;
use alloc::vec::Vec;

/// The Mini-PC's `_CST` (AMI firmware, Ryzen 7 PRO 8845HS, SSDT "AmdTable"), bytes as dumped.
const MINIPC: &[u8] = &[
    0x08, b'_', b'C', b'S', b'T', 0x12, 0x4b, 0x06, 0x04, 0x0a, 0x03, //
    0x12, 0x21, 0x04, 0x11, 0x14, 0x0a, 0x11, 0x82, 0x0c, 0x00, 0x7f, 0x02, 0x02, 0x00, 0, 0, 0, 0, 0, 0, 0, 0, 0x79, 0x00, //
    0x0a, 0x01, 0x0b, 0x01, 0x00, 0x0c, 0, 0, 0, 0, //
    0x12, 0x21, 0x04, 0x11, 0x14, 0x0a, 0x11, 0x82, 0x0c, 0x00, 0x01, 0x08, 0x00, 0x01, 0x14, 0x04, 0, 0, 0, 0, 0, 0, 0x79, 0x00, //
    0x0a, 0x02, 0x0b, 0x12, 0x00, 0x0c, 0, 0, 0, 0, //
    0x12, 0x21, 0x04, 0x11, 0x14, 0x0a, 0x11, 0x82, 0x0c, 0x00, 0x01, 0x08, 0x00, 0x01, 0x15, 0x04, 0, 0, 0, 0, 0, 0, 0x79, 0x00, //
    0x0a, 0x03, 0x0b, 0x5e, 0x01, 0x0c, 0, 0, 0, 0, //
    0x08, b'_', b'C', b'S', b'D',
];

fn minipc_states() -> Vec<CState> {
    vec![
        CState { kind: 1, entry: Entry::FixedHw(0), latency_us: 1 },
        CState { kind: 2, entry: Entry::Io(0x414), latency_us: 18 },
        CState { kind: 3, entry: Entry::Io(0x415), latency_us: 350 },
    ]
}

fn parse(aml: &[u8]) -> Option<Cst> {
    find(&build::table(aml))
}

#[test]
fn minipc_cst() {
    let c = parse(MINIPC).expect("parses");
    assert_eq!(c.states, minipc_states());
    assert!(c.skipped.is_empty() && !c.truncated);
    assert_eq!(parse(&build::name(&MINIPC_STATES)).unwrap().states, minipc_states(), "the builder writes what firmware writes");
}

#[test]
fn cut_tables_never_give_more_or_other_states() {
    for cut in 0..MINIPC.len() {
        if let Some(c) = parse(&MINIPC[..cut]) {
            assert!(c.states.len() <= 3 && c.states.iter().all(|s| minipc_states().contains(s)), "cut {cut}: {c:?}");
        }
    }
    assert_eq!(parse(&MINIPC[..MINIPC.len() - 6]), None, "cut inside the package: its length points past the end");
}

#[test]
fn a_method_or_an_unrelated_name_is_no_cst() {
    let mut bent = MINIPC.to_vec();
    bent[0] = 0x14; // a Method, not a Name
    assert_eq!(parse(&bent), None);
    let mut other = MINIPC.to_vec();
    other[4] = b'X';
    assert_eq!(parse(&other), None);
}

#[test]
fn the_state_count_must_match_the_elements_exactly() {
    let el: Vec<Vec<u8>> = MINIPC_STATES.iter().map(build::state).collect();
    assert!(parse(&build::name_raw(4, 3, &el)).is_some());
    assert_eq!(parse(&build::name_raw(4, 9, &el)), None, "more states than elements");
    assert_eq!(parse(&build::name_raw(4, 2, &el)), None, "fewer states than elements (Linux: inconsistent)");
    assert_eq!(parse(&build::name_raw(4, 0, &el)), None, "zero states");
    assert_eq!(parse(&build::name_raw(0, 0, &[])), None, "an empty package");
    assert_eq!(parse(&build::name_raw(1, 0, &[])), None);
    let mut first_not_int = build::name_raw(4, 3, &el);
    first_not_int[9] = 0x0D; // a string where the count belongs
    assert_eq!(parse(&first_not_int), None);
}

#[test]
fn a_wrong_element_is_skipped_and_the_others_stay() {
    let good = |s: &State| build::state(s);
    let c2 = good(&State::io(2, 0x414, 18));
    let cases: [(Vec<u8>, Skip); 6] = [
        (build::int(7), Skip::NotPackage),
        (build::package(3, &[build::sized(0x11, &[0x0A, 0x02, 0x79, 0x00]), build::int(2), build::int(18)].concat()), Skip::NotFourElements),
        (build::package(4, &[build::int(5), build::int(2), build::int(18), build::int(0)].concat()), Skip::NotBuffer),
        // A buffer with a memory descriptor (0x86) instead of a Generic Register.
        (build::package(4, &[build::sized(0x11, &[0x0A, 0x02, 0x86, 0x09]), build::int(2), build::int(18), build::int(0)].concat()), Skip::NoRegister),
        // The type as a string.
        ({
            let mut v = c2.clone();
            let at = v.len() - 4; // ... 0x0A 0x02 | latency 0x0A 0x12 | power 0x00
            v[at - 1] = 0x0D;
            v[at] = 0x00;
            v
        }, Skip::NotInteger),
        // A nested package where the buffer belongs.
        (build::package(4, &[build::package(1, &build::int(1)), build::int(2), build::int(18), build::int(0)].concat()), Skip::NotBuffer),
    ];
    for (bad, why) in cases {
        let c = parse(&build::name_raw(3, 2, &[bad.clone(), c2.clone()])).unwrap_or_else(|| panic!("{why:?}: {bad:x?}"));
        assert_eq!(c.states, vec![CState { kind: 2, entry: Entry::Io(0x414), latency_us: 18 }], "{why:?}");
        assert_eq!(c.skipped, vec![(1, why)]);
    }
    let only_bad = build::name_raw(2, 1, &[build::int(7)]);
    assert_eq!(parse(&only_bad), None, "no state at all: no _CST");
    // A later `_CST` in the same table that does parse counts (the Mini-PC has one per CPU).
    assert_eq!(parse(&[only_bad, build::name(&MINIPC_STATES)].concat()).unwrap().states, minipc_states());
}

#[test]
fn missing_elements_stop_the_scan_and_keep_what_came_before() {
    let el = vec![build::state(&MINIPC_STATES[0]), build::state(&MINIPC_STATES[1])];
    let c = parse(&build::name_raw(4, 3, &el)).expect("two states parsed");
    assert_eq!(c.states, minipc_states()[..2]);
    assert!(c.truncated);
    // An element whose own length points past the package: the scan cannot step over it.
    let mut long = build::state(&MINIPC_STATES[1]);
    long[1] = 0x3F;
    let c = parse(&build::name_raw(3, 2, &[build::state(&MINIPC_STATES[0]), long])).unwrap();
    assert_eq!((c.states.len(), c.truncated), (1, true));
    // An unknown opcode as an element.
    let c = parse(&build::name_raw(3, 2, &[build::state(&MINIPC_STATES[0]), vec![0x5B, 0x80]])).unwrap();
    assert_eq!((c.states.len(), c.truncated), (1, true));
}

#[test]
fn values_are_kept_as_the_table_says_them() {
    let s = |kind: u64, latency: u64, space: u8, addr: u64| State { space, addr, kind, latency_us: latency, power: 0 };
    let states = [
        s(0, 5, 1, 0x410),
        s(4, 5, 1, 0x411),
        s(300, 5, 1, 0x412),
        s(2, u64::MAX, 1, 0x413),
        s(2, 0x1_0000_0000, 1, 0x413),
        s(2, 0, 1, 0x1_0000),
        s(2, 18, 0x7F, 0x10),
        s(2, 18, 0, 0xFED0_0000),
    ];
    let c = parse(&build::name(&states)).unwrap();
    let k: Vec<(u8, Entry, u32)> = c.states.iter().map(|c| (c.kind, c.entry, c.latency_us)).collect();
    assert_eq!(
        k,
        vec![
            (0, Entry::Io(0x410), 5),
            (4, Entry::Io(0x411), 5),
            (255, Entry::Io(0x412), 5),
            (2, Entry::Io(0x413), u32::MAX),
            (2, Entry::Io(0x413), u32::MAX),
            (2, Entry::Io(0x1_0000), 0),
            (2, Entry::FixedHw(0x10), 18),
            (2, Entry::Other(0), 18),
        ]
    );
    // AML Ones (0xFF) as the latency.
    let mut ones = build::state(&State::io(2, 0x414, 18));
    let n = ones.len();
    ones[n - 3] = 0xFF; // latency 0x0A 0x12 -> 0xFF 0x00 (Ones, then ZeroOp as the power)
    ones[n - 2] = 0x00;
    ones.truncate(n - 1);
    ones[1] -= 1;
    let c = parse(&build::name_raw(2, 1, &[ones])).unwrap();
    assert_eq!(c.states[0].latency_us, u32::MAX);
}

#[test]
fn duplicates_and_many_states_are_all_read() {
    let dup = [MINIPC_STATES[1], MINIPC_STATES[1], MINIPC_STATES[2]];
    assert_eq!(parse(&build::name(&dup)).unwrap().states.len(), 3, "judged by the idle code, not here");
    let many: Vec<State> = (0..40).map(|i| State::io(2 + (i % 2), 0x414 + i, 18 + i)).collect();
    let c = parse(&build::name(&many)).unwrap();
    assert_eq!(c.states.len(), 40, "no limit on the count (R1)");
    assert_eq!(c.states[39], CState { kind: 3, entry: Entry::Io(0x414 + 39), latency_us: 57 });
}

#[test]
fn a_cst_in_an_ssdt_is_found_by_the_table_scan() {
    use crate::testkit::*;
    let mut m = minipc();
    m.put(at::SSDT2, sdt(b"SSDT", 2, &build::name(&MINIPC_STATES)));
    let a = crate::parse::parse(&mut m, at::RSDP);
    let (c, sig, p) = a.cst.expect("found");
    assert_eq!((c.states, &sig, p), (minipc_states(), b"SSDT", at::SSDT2));
    assert!(a.s5.is_some(), "the _S5 in the DSDT is still found");
    let mut bad = sdt(b"SSDT", 2, &build::name(&MINIPC_STATES));
    bad[9] ^= 1; // checksum
    m.put(at::SSDT2, bad);
    assert_eq!(crate::parse::parse(&mut m, at::RSDP).cst, None, "an SSDT with a bad checksum is not read");
}

/// Hostile bytes: every byte of a few tables set to each of a handful of telling values (lengths, opcodes,
/// Ones, the package opcode) and one bit flipped. The scan must neither panic nor report more entries than the
/// bytes could hold; the same through the whole table parse (an SSDT with a good checksum).
#[test]
fn mutated_tables_never_panic_or_grow() {
    let many: Vec<State> = (0..40).map(|i| State::io(2 + (i % 2), 0x414 + i, 18 + i)).collect();
    let dup = [MINIPC_STATES[1], MINIPC_STATES[1], MINIPC_STATES[2]];
    let tables = [MINIPC.to_vec(), build::name(&MINIPC_STATES), build::name(&many), build::name(&dup)];
    let mut runs = 0usize;
    for t in &tables {
        for i in 0..t.len() {
            for v in [0x00, 0xFF, 0x7F, 0x12, 0x13, 0x0A, 0x0E, 0x40, t[i] ^ 0x01, t[i] ^ 0x80] {
                let mut m = t.clone();
                m[i] = v;
                if let Some(c) = parse(&m) {
                    assert!(c.states.len() + c.skipped.len() <= m.len(), "byte {i} = {v:#x}: {} states from {} bytes", c.states.len(), m.len());
                }
                runs += 1;
            }
        }
    }
    assert!(runs > 10_000);
    // The leading element count of the first package set to 0xFF: refused (count must be states + 1).
    let mut big = MINIPC.to_vec();
    big[10] = 0xFF;
    assert!(parse(&big).is_none(), "count 255 with 3 states");
    use crate::testkit::*;
    for i in 0..MINIPC.len() {
        for v in [0x00, 0xFF, 0x7F] {
            let mut aml = MINIPC.to_vec();
            aml[i] = v;
            let mut m = minipc();
            m.put(at::SSDT2, sdt(b"SSDT", 2, &aml));
            if let Some((c, _, _)) = crate::parse::parse(&mut m, at::RSDP).cst {
                assert!(c.states.len() <= aml.len(), "byte {i} = {v:#x}");
            }
        }
    }
}
