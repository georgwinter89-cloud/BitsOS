//! Small hand-made graphs: loops, depth, connection-list forms, pin selection.

use std::vec;
use std::vec::Vec;

use crate::conn::ConnError;
use crate::dump::{Dump, Line};
use crate::model::Codec;
use crate::path::{Hop, OutputKind, outputs};
use crate::setup::{SetupError, Stream, output_setup};
use crate::verb::*;

const LINE_OUT: u32 = 0x0101_4010; // jack, line out

/// A codec with one audio group (node 1) whose widgets are `0x02..=last`.
struct G(Dump);

impl G {
    fn new(last: u8) -> G {
        let mut g = G(Dump::new());
        g.put(0, get_param(PAR_VENDOR_ID), 0x1111_2222);
        g.put(0, get_param(PAR_REVISION_ID), 0);
        g.put(0, get_param(PAR_NODE_COUNT), 0x0001_0001);
        g.put(1, get_param(PAR_FG_TYPE), 1);
        g.put(1, get_param(PAR_PCM), 0x000E_0560);
        g.put(1, get_param(PAR_STREAM), 1);
        g.put(1, get_param(PAR_AMP_OUT_CAP), 0);
        g.put(1, get_param(PAR_AMP_IN_CAP), 0);
        g.put(
            1,
            get_param(PAR_NODE_COUNT),
            0x0002_0000 | (last as u32 - 1),
        );
        for nid in 0x02..=last {
            g.put(nid, get_param(PAR_AUDIO_WIDGET_CAP), 0x00F0_0000);
        }
        g
    }
    fn put(&mut self, nid: u8, verb: u32, answer: u32) {
        // Later puts replace earlier ones (a widget turned from vendor into something else).
        let mut d = Dump::new();
        for l in self
            .0
            .lines()
            .iter()
            .filter(|l| (l.nid, l.verb) != (nid, verb))
        {
            d.push(*l).unwrap();
        }
        d.push(Line {
            cad: 0,
            nid,
            verb,
            answer: Some(answer),
        })
        .unwrap();
        self.0 = d;
    }
    fn conns_raw(&mut self, nid: u8, len: u32, answers: &[u32]) {
        self.put(nid, get_param(PAR_CONNLIST_LEN), len);
        for (i, a) in answers.iter().enumerate() {
            let step = if len & 0x80 != 0 { 2 } else { 4 };
            self.put(nid, get_conn_list((i * step) as u8), *a);
        }
    }
    fn conns(&mut self, nid: u8, list: &[u8]) {
        let answers: Vec<u32> = list
            .chunks(4)
            .map(|c| c.iter().rev().fold(0, |a, &n| a << 8 | n as u32))
            .collect();
        self.conns_raw(nid, list.len() as u32, &answers);
    }
    fn dac(&mut self, nid: u8) {
        self.put(nid, get_param(PAR_AUDIO_WIDGET_CAP), 0x0000_0001);
    }
    fn node(&mut self, nid: u8, caps: u32, list: &[u8]) {
        self.put(nid, get_param(PAR_AUDIO_WIDGET_CAP), caps);
        self.conns(nid, list);
    }
    fn pin(&mut self, nid: u8, pin_caps: u32, config: u32, list: &[u8]) {
        self.node(nid, 0x0040_0100, list);
        self.put(nid, get_param(PAR_PIN_CAP), pin_caps);
        self.put(nid, GET_CONFIG_DEFAULT, config);
    }
    fn codec(&self) -> Codec {
        Codec::read(&mut self.0.source(0), 0).unwrap()
    }
}

const MIXER: u32 = 0x0020_0100;
const SELECTOR: u32 = 0x0030_0100;

fn path_of(g: &G, pin: u8) -> Option<Vec<Hop>> {
    outputs(&g.codec())
        .into_iter()
        .find(|o| o.pin == pin)
        .map(|o| o.path)
}

#[test]
fn loop_without_converter_ends() {
    let mut g = G::new(0x12);
    g.pin(0x10, 0x10, LINE_OUT, &[0x11]);
    g.node(0x11, MIXER, &[0x12]);
    g.node(0x12, MIXER, &[0x11, 0x12]);
    assert_eq!(path_of(&g, 0x10), None);
}

#[test]
fn loop_is_cut_from_path() {
    let mut g = G::new(0x12);
    g.dac(0x02);
    g.pin(0x10, 0x10, LINE_OUT, &[0x11]);
    g.node(0x11, MIXER, &[0x12, 0x02]);
    g.node(0x12, SELECTOR, &[0x11]);
    let path = path_of(&g, 0x10).unwrap();
    let want = vec![
        Hop {
            nid: 0x10,
            input: Some(0),
        },
        Hop {
            nid: 0x11,
            input: Some(1),
        },
        Hop {
            nid: 0x02,
            input: None,
        },
    ];
    assert_eq!(path, want);
}

#[test]
fn depth_five_reaches_six_does_not() {
    for (chain, found) in [(4u8, true), (5, false)] {
        let mut g = G::new(0x20);
        g.dac(0x02);
        g.pin(0x10, 0x10, LINE_OUT, &[0x11]);
        for i in 0..chain {
            let next = if i + 1 == chain { 0x02 } else { 0x12 + i };
            g.node(0x11 + i, SELECTOR, &[next]);
        }
        assert_eq!(
            path_of(&g, 0x10).is_some(),
            found,
            "chain of {chain} selectors"
        );
    }
}

#[test]
fn ranges_in_short_and_long_form() {
    let mut g = G::new(0x12);
    for d in 0x02..=0x05 {
        g.dac(d);
    }
    g.pin(0x10, 0x10, LINE_OUT, &[]);
    g.conns_raw(0x10, 2, &[0x85 << 8 | 0x02]); // 0x02, range to 0x05
    g.node(0x11, MIXER, &[]);
    g.conns_raw(0x11, 0x80 | 3, &[0x8004 << 16 | 0x0002, 0x0003]); // long: 2, ..4, 3
    g.node(0x12, MIXER, &[]);
    g.conns_raw(0x12, 0x80 | 2, &[0x0002 << 16 | 0x0080]); // long: node 0x80 is out of range
    let c = g.codec();
    let grp = &c.groups[0];
    assert_eq!(grp.widget(0x10).unwrap().conns.nodes, vec![2, 3, 4, 5]);
    assert_eq!(grp.widget(0x11).unwrap().conns.nodes, vec![2, 3, 4, 3]);
    let bad = &grp.widget(0x12).unwrap().conns;
    assert!(bad.nodes.is_empty() && bad.error == Some(ConnError::NodeOutOfRange(0x80)));
}

#[test]
fn backwards_and_leading_ranges_are_skipped() {
    let mut g = G::new(0x10);
    g.pin(0x10, 0x10, LINE_OUT, &[]);
    g.conns_raw(0x10, 4, &[0x03 << 24 | 0x82 << 16 | 0x05 << 8 | 0x84]);
    let w = g.codec().groups[0].widget(0x10).unwrap().conns.clone();
    assert_eq!(w.nodes, vec![5, 3]);
    assert_eq!(w.skipped_ranges, 2);
}

#[test]
fn pin_selection_rules() {
    let mut g = G::new(0x16);
    g.dac(0x02);
    g.pin(0x10, 0x10, 0x4111_11F0, &[0x02]); // not connected
    g.pin(0x11, 0x20, LINE_OUT, &[0x02]); // input only
    g.pin(0x12, 0, 0x0221_101F, &[0x02]); // no pin caps: trust the config (headphone)
    g.pin(0x13, 0x94, 0x1856_00F0, &[0x02]); // HDMI
    g.pin(0x14, 0x10, 0x02A1_1030, &[0x02]); // mic
    g.pin(0x15, 0x10, 0x9017_0110, &[0x16, 0x02]); // speaker; 0x16 is a mono DAC
    g.put(0x16, get_param(PAR_AUDIO_WIDGET_CAP), 0);
    let c = g.codec();
    let outs = outputs(&c);
    let found: Vec<_> = outs.iter().map(|o| (o.pin, o.kind)).collect();
    assert_eq!(
        found,
        vec![
            (0x12, OutputKind::Headphone),
            (0x13, OutputKind::Digital),
            (0x15, OutputKind::Speaker)
        ]
    );
    // The mono DAC is passed over for the stereo one.
    assert_eq!(outs[2].converter(), 0x02);
    // No pin caps: headphone drive is assumed, as V1 does.
    let list = output_setup(&c, &outs[0], &Stream::default()).unwrap();
    assert!(list.contains(&crate::setup::Action::Set {
        nid: 0x12,
        verb: 0x707C0
    }));
}

#[test]
fn incomplete_widget_is_ignored() {
    let mut g = G::new(0x10);
    g.dac(0x02);
    g.pin(0x10, 0x10, LINE_OUT, &[0x02]);
    let mut d = Dump::new();
    for l in g.0.lines().iter().filter(|l| l.verb != GET_CONFIG_DEFAULT) {
        d.push(*l).unwrap();
    }
    let c = Codec::read(&mut d.source(0), 0).unwrap();
    assert_eq!(c.missing, vec![(0x10, GET_CONFIG_DEFAULT)]);
    assert!(outputs(&c).is_empty());
}

#[test]
fn list_stops_at_256_entries() {
    let mut g = G::new(0x11);
    g.dac(0x05);
    g.pin(0x10, 0x10, LINE_OUT, &[0x11]);
    g.node(0x11, SELECTOR, &[]);
    // 0x20, ..0x7F, 0x20, ..0x7F, 0x20, ..0x7F, 0x05: the DAC would sit at index 288, which the
    // select verb cannot carry, so the list ends at 256 entries and there is no path.
    g.conns_raw(0x11, 7, &[0xFF20_FF20, 0x0005_FF20]);
    let c = g.codec();
    let l = &c.groups[0].widget(0x11).unwrap().conns;
    assert_eq!(l.nodes.len(), crate::conn::MAX_ENTRIES);
    assert!(l.truncated && l.error.is_none());
    assert!(outputs(&c).is_empty());

    // 0x20, ..0x7F, 0x20, ..0x7F, 0x41, ..0x7F, 0x05: the DAC at index 255 is still selectable.
    g.conns_raw(0x11, 7, &[0xFF20_FF20, 0x0005_FF41]);
    let c = g.codec();
    let l = &c.groups[0].widget(0x11).unwrap().conns;
    assert_eq!(l.nodes.len(), 256);
    assert!(!l.truncated);
    let outs = outputs(&c);
    assert_eq!(
        outs[0].path[1],
        Hop {
            nid: 0x11,
            input: Some(255)
        }
    );
    let list = output_setup(&c, &outs[0], &Stream::default()).unwrap();
    assert!(list.contains(&crate::setup::Action::Set {
        nid: 0x11,
        verb: 0x701FF
    }));

    // A hand-made path with a larger index is an error, not a panic.
    let mut bad = outs[0].clone();
    bad.path[1].input = Some(288);
    assert_eq!(
        output_setup(&c, &bad, &Stream::default()),
        Err(SetupError::IndexTooLarge(0x11))
    );
}
