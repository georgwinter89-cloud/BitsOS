//! Hostile answers and dumps: nothing may panic or run away.

use std::string::String;
use std::vec::Vec;

use super::{AA01, ALC269};
use crate::dump::Dump;
use crate::model::Codec;
use crate::path::outputs;
use crate::setup::{Stream, output_setup};
use crate::verb::*;
use crate::walk::{check, walk};
use crate::{Nid, NoResponse, VerbSource};

/// splitmix64: a small deterministic mixer, enough to make answers look random.
pub(super) fn mix(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31)
}

/// Answers every verb with noise, biased so that the interesting shapes (audio groups, pins,
/// mixers, connection lists with ranges, wide node ranges) actually happen.
pub(super) struct Chaos(pub(super) u64);

impl VerbSource for Chaos {
    fn get(&mut self, nid: Nid, verb: u32) -> Result<u32, NoResponse> {
        let r = mix(self.0 ^ (nid as u64) << 32 ^ verb as u64);
        if r.is_multiple_of(61) {
            return Err(NoResponse);
        }
        let v = (r >> 8) as u32;
        Ok(match verb {
            _ if verb == get_param(PAR_FG_TYPE) => v & 0x101,
            _ if verb == get_param(PAR_NODE_COUNT) => v & 0x00FF_00FF,
            _ if verb == get_param(PAR_AUDIO_WIDGET_CAP) => {
                let kind = [0u32, 2, 3, 4, 4, 0xF][(r >> 40) as usize % 6];
                (v & 0xFF0F_FFFF) | kind << 20
            }
            _ if verb == get_param(PAR_CONNLIST_LEN) => v & 0xFF,
            _ if verb & 0xFFF00 == 0xF0200 && r & 1 == 0 => v & 0xFF7F_FF7F,
            _ if verb == get_param(PAR_PCM) => v | 0x0002_0040,
            _ => v,
        })
    }
}

#[test]
fn chaos_codecs_never_panic() {
    let mut paths = 0;
    for seed in 0..1500u64 {
        let Ok(codec) = Codec::read(&mut Chaos(seed), 0) else {
            continue;
        };
        for o in outputs(&codec) {
            paths += 1;
            assert!(o.path.len() >= 2 && o.path.len() <= 6);
            let mut seen: Vec<u8> = o.path.iter().map(|h| h.nid).collect();
            seen.sort_unstable();
            seen.dedup();
            assert_eq!(seen.len(), o.path.len(), "path visits a node twice");
            let _ = output_setup(&codec, &o, &Stream::default());
        }
        let lines = walk(Chaos(seed), 0);
        let mut d = Dump::new();
        for l in lines {
            d.push(l).unwrap();
        }
        let back = Dump::parse(&d.to_text()).unwrap();
        assert_eq!(back.lines(), d.lines());
        assert!(check(&back, 0).complete());
    }
    assert!(
        paths > 50,
        "the chaos source should produce some paths, got {paths}"
    );
}

#[test]
fn huge_counts_stay_in_node_space() {
    struct Wide;
    impl VerbSource for Wide {
        fn get(&mut self, nid: Nid, verb: u32) -> Result<u32, NoResponse> {
            Ok(match verb {
                _ if verb == get_param(PAR_NODE_COUNT) => 0x00FF_00FF, // start 255, 255 nodes
                _ if verb == get_param(PAR_FG_TYPE) => 1,
                _ if verb == get_param(PAR_AUDIO_WIDGET_CAP) => 0x0020_0100,
                _ if verb == get_param(PAR_CONNLIST_LEN) => 0xFF, // 127 long-form entries
                _ => 0xFFFF_FFFF ^ nid as u32,
            })
        }
    }
    let c = Codec::read(&mut Wide, 0).unwrap();
    assert!(c.groups.is_empty(), "nodes 255.. cannot be addressed");
    // The walk stops there too: the three root reads and nothing for nodes 0xFF...
    let lines = walk(Wide, 0);
    assert!(lines.iter().all(|l| l.nid == 0), "{lines:?}");

    struct Dense;
    impl VerbSource for Dense {
        fn get(&mut self, nid: Nid, verb: u32) -> Result<u32, NoResponse> {
            Ok(match verb {
                _ if verb == get_param(PAR_NODE_COUNT) && nid == 0 => 0x0001_0001,
                _ if verb == get_param(PAR_NODE_COUNT) => 0x0001_00FF, // overlaps the group itself
                _ if verb == get_param(PAR_FG_TYPE) => 1,
                _ if verb == get_param(PAR_AUDIO_WIDGET_CAP) => 0x0040_0100,
                _ if verb == get_param(PAR_PIN_CAP) => 0x10,
                _ if verb == get_param(PAR_CONNLIST_LEN) => 0x7F, // 127 short entries
                _ if verb & 0xFFF00 == 0xF0200 => 0xFF01_FF01,    // 1, ..0x7F, 1, ..0x7F
                _ => 0,
            })
        }
    }
    let c = Codec::read(&mut Dense, 0).unwrap();
    assert_eq!(c.groups.len(), 1);
    let g = &c.groups[0];
    assert_eq!(g.widgets.len(), 126);
    assert!(g.widgets.iter().all(|w| w.nid >= 2 && w.nid <= 0x7F));
    // Every node is a pin wired to every node: no converter anywhere, so no output.
    assert!(outputs(&c).is_empty());
}

#[test]
fn silent_codec_is_an_error() {
    struct Silent;
    impl VerbSource for Silent {
        fn get(&mut self, _: Nid, _: u32) -> Result<u32, NoResponse> {
            Err(NoResponse)
        }
    }
    assert_eq!(Codec::read(&mut Silent, 0), Err(NoResponse));
    assert_eq!(walk(Silent, 3).len(), 1);
}

#[test]
fn mutated_dumps_never_panic() {
    let (mut parsed, mut with_outputs, mut complete) = (0, 0, 0);
    for (k, base) in [ALC269, AA01].iter().enumerate() {
        let bytes = base.as_bytes();
        for seed in 0..1500u64 {
            let mut b: Vec<u8> = bytes.to_vec();
            let r = mix(seed ^ (k as u64) << 40);
            for i in 0..(1 + r % 8) {
                let rr = mix(r ^ i);
                let at = (rr % b.len() as u64) as usize;
                const CHARS: &[u8] = b"0123456789ABCDEFx#-: \n\tZ";
                let ch = CHARS[(rr >> 32) as usize % CHARS.len()];
                match rr >> 60 {
                    0..=9 => b[at] = ch,
                    10..=12 => b.insert(at, ch),
                    _ => {
                        b.remove(at);
                    }
                }
            }
            let text = String::from_utf8_lossy(&b);
            let Ok(d) = Dump::parse(&text) else {
                continue;
            };
            parsed += 1;
            for cad in d.codecs() {
                if let Ok(c) = Codec::read(&mut d.source(cad), cad) {
                    for o in outputs(&c) {
                        with_outputs += 1;
                        assert!(o.path.len() >= 2 && o.path.len() <= 6);
                        let mut seen: Vec<u8> = o.path.iter().map(|h| h.nid).collect();
                        seen.sort_unstable();
                        seen.dedup();
                        assert_eq!(seen.len(), o.path.len(), "path visits a node twice");
                        let _ = output_setup(&c, &o, &Stream::default());
                    }
                }
                let ck = check(&d, cad);
                if ck.complete() {
                    complete += 1;
                    // A complete walk asks nothing the dump lacks, so the model misses nothing
                    // that was not recorded as unanswered.
                    if let Ok(c) = Codec::read(&mut d.source(cad), cad) {
                        assert!(
                            c.missing
                                .iter()
                                .all(|&(n, v)| d.get(cad, n, v) == Some(None))
                        );
                    }
                }
            }
        }
    }
    // The test must not pass by rejecting every mutant.
    assert!(parsed > 300, "only {parsed} mutants parsed");
    assert!(
        with_outputs > 300,
        "only {with_outputs} outputs from mutants"
    );
    assert!(
        complete > 100,
        "only {complete} mutants were complete walks"
    );
}

#[test]
fn dump_with_corrupt_graph_still_works() {
    // The ALC269 fixture with mixer 0x0C wired back to the pins and itself: still the V1 path.
    let mut text = String::from(ALC269);
    text = text.replace("0 0C F000E 00000002", "0 0C F000E 00000004");
    text = text.replace("0 0C F0200 00000B02", "0 0C F0200 0C150B02");
    let d = Dump::parse(&text).unwrap();
    let c = Codec::read(&mut d.source(0), 0).unwrap();
    assert_eq!(
        c.groups[0].widget(0x0C).unwrap().conns.nodes,
        [0x02, 0x0B, 0x15, 0x0C]
    );
    let outs = outputs(&c);
    assert_eq!(outs.len(), 2);
    assert!(
        outs.iter()
            .all(|o| o.converter() == 0x02 && o.path.len() == 3)
    );
}

#[test]
fn big_dumps_parse_without_trouble() {
    // 100k repeats of one read plus irrelevant reads of every node: fine, repeats fold into one.
    let mut text = String::from("# synthetic: yes\n");
    for i in 0..100_000u32 {
        text.push_str("0 01 F0005 00000001\n");
        text.push_str(&std::format!(
            "{:X} {:02X} F0009 {:08X}\n",
            (i / 128) % 16,
            i % 128,
            0
        ));
    }
    let d = Dump::parse(&text).unwrap();
    assert_eq!(d.lines().len(), 1 + 16 * 128);
    // One 1 MB line: an error naming that line.
    let long = std::format!("0 01 F0005 {}\n", "0".repeat(1 << 20));
    let e = Dump::parse(&(String::from(ALC269) + &long)).unwrap_err();
    assert_eq!(e.line, ALC269.lines().count() + 1);
    // A 1 MB comment and a 1 MB header value are kept as text, nothing else.
    let comment = std::format!("# {}\n# k: {}\n", "x".repeat(1 << 20), "y".repeat(1 << 20));
    let d = Dump::parse(&(comment + ALC269)).unwrap();
    assert_eq!(d.header_value("k").map(str::len), Some(1 << 20));
    // A leading '+' or other non-hex characters are not numbers.
    for bad in [
        "0 +5 F0000 1",
        "0 05 F0000 +1",
        "0 05 F0000 0x",
        "0 05 F0000 1_0",
    ] {
        assert!(Dump::parse(bad).is_err(), "{bad}");
    }
}
