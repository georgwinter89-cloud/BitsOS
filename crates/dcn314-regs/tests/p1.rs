//! Cross-check against the G-1 snapshot. Its names and field texts were produced by V1's
//! werkzeuge/plattform/gpu/messung/decode.mjs (an independent JavaScript parser), its offsets are
//! the BAR5 bytes V1 actually read on the Mini-PC.

use dcn314_regs::{P1_SNAPSHOT, at_byte, bar5, lookup};

const TEXT: &str = include_str!("data/p1-decoded.txt");
const NOT_DCN: &str = "(not a DCN 3.1.4 header register)";

struct Line<'a> {
    off: u32,
    val: u32,
    names: Vec<&'a str>,
    fields: Vec<&'a str>,
}

fn hex(s: &str) -> u32 {
    u32::from_str_radix(s.trim_start_matches("0x"), 16).expect(s)
}

fn lines() -> Vec<Line<'static>> {
    TEXT.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            let mut t = l.split_whitespace();
            let off = hex(t.next().unwrap());
            let val = hex(t.next().unwrap());
            if l.contains(NOT_DCN) {
                return Line { off, val, names: vec![], fields: vec![] };
            }
            let names = t.next().unwrap().split('/').collect();
            Line { off, val, names, fields: t.collect() }
        })
        .collect()
}

#[test]
fn fixture_matches_the_text() {
    let l = lines();
    assert_eq!(l.len(), 179);
    assert_eq!(P1_SNAPSHOT.len(), 179);
    for (a, &(o, v)) in l.iter().zip(P1_SNAPSHOT) {
        assert_eq!((a.off, a.val), (o, v));
    }
}

#[test]
fn every_named_register_has_v1s_offset() {
    let mut named = 0;
    for l in lines() {
        if l.names.is_empty() {
            assert_eq!(at_byte(l.off).count(), 0, "{:#x} should not be a DCN register", l.off);
            assert!([bar5::RCC_CONFIG_MEMSIZE, bar5::RCC_CONFIG_APER_SIZE].contains(&l.off), "{:#x}", l.off);
            continue;
        }
        named += 1;
        for n in &l.names {
            let r = lookup(n).unwrap_or_else(|| panic!("{n} missing"));
            assert_eq!(r.byte, l.off, "{n}");
        }
        let mut here: Vec<&str> = at_byte(l.off).map(|r| r.name).collect();
        let mut want = l.names.clone();
        here.sort();
        want.sort();
        assert_eq!(here, want, "{:#x}", l.off);
    }
    assert_eq!(named, 177);
}

#[test]
fn field_decode_matches_decode_mjs() {
    for l in lines() {
        let Some(first) = l.names.first() else { continue };
        let r = lookup(first).unwrap();
        let ours: Vec<String> = r.decode(l.val).map(|(n, x)| format!("{n}=0x{x:x}")).collect();
        assert_eq!(ours, l.fields, "{first} = {:#x}", l.val);
    }
}
