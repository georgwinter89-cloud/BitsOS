//! The read walk: every read-only verb a complete dump holds, in a fixed order that depends only
//! on earlier answers. The model reads (what [`Codec::read`] asks) come first, then state reads
//! that show what firmware or another OS left behind (power, pin control, amps, sense), which
//! help when comparing with Linux but are not needed for the model. `DUMP-FORMAT.md` spells the
//! walk out for whoever reads a codec without this crate (the desk on V1, before V2 runs).

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::vec::Vec;

use crate::caps::WidgetType;
use crate::dump::{Dump, Line};
use crate::model::Codec;
use crate::verb::*;
use crate::{Nid, NoResponse, VerbSource};

/// Wraps a source and records every read once, in first-read order.
pub struct Recorder<S> {
    inner: S,
    cad: u8,
    lines: Vec<Line>,
    seen: BTreeMap<(Nid, u32), Option<u32>>,
}

impl<S: VerbSource> Recorder<S> {
    pub fn new(inner: S, cad: u8) -> Self {
        Recorder {
            inner,
            cad,
            lines: Vec::new(),
            seen: BTreeMap::new(),
        }
    }
    pub fn into_lines(self) -> Vec<Line> {
        self.lines
    }
}

impl<S: VerbSource> VerbSource for Recorder<S> {
    fn get(&mut self, nid: Nid, verb: u32) -> Result<u32, NoResponse> {
        if let Some(&a) = self.seen.get(&(nid, verb)) {
            return a.ok_or(NoResponse);
        }
        let a = self.inner.get(nid, verb).ok();
        self.seen.insert((nid, verb), a);
        self.lines.push(Line {
            cad: self.cad,
            nid,
            verb,
            answer: a,
        });
        a.ok_or(NoResponse)
    }
}

/// The state reads for a model already read (see the module text).
pub fn state_reads(codec: &Codec) -> Vec<(Nid, u32)> {
    let mut reads = Vec::new();
    for g in codec.audio_groups() {
        reads.push((g.nid, GET_POWER_STATE));
        reads.push((g.nid, GET_SUBSYSTEM_ID));
        reads.push((g.nid, get_param(PAR_POWER_STATE)));
        reads.push((g.nid, get_param(PAR_GPIO_CAP)));
        for w in &g.widgets {
            let n = w.nid;
            if w.caps.power_control() {
                reads.push((n, GET_POWER_STATE));
            }
            if matches!(w.kind(), WidgetType::Output | WidgetType::Input) {
                reads.push((n, GET_STREAM_FORMAT));
                reads.push((n, GET_CONV));
            }
            if w.caps.out_amp() {
                reads.push((n, get_amp(true, true, 0)));
                reads.push((n, get_amp(true, false, 0)));
            }
            if w.caps.in_amp() {
                for i in 0..w.conns.nodes.len().clamp(1, 16) as u8 {
                    reads.push((n, get_amp(false, true, i)));
                    reads.push((n, get_amp(false, false, i)));
                }
            }
            if w.kind() != WidgetType::Mixer && w.conns.nodes.len() > 1 {
                reads.push((n, GET_CONNECT_SEL));
            }
            if w.kind() == WidgetType::Pin {
                reads.push((n, GET_PIN_WIDGET_CONTROL));
                if w.caps.unsol() {
                    reads.push((n, GET_UNSOLICITED_RESPONSE));
                }
                if w.pin_caps.presence_detect() {
                    reads.push((n, GET_PIN_SENSE));
                }
                if w.pin_caps.eapd() {
                    reads.push((n, GET_EAPD_BTLENABLE));
                }
                if codec.is_ati() {
                    reads.push((n, ATI_GET_HBR_CONTROL));
                }
            }
        }
    }
    reads
}

/// Runs the whole walk on codec `cad` and returns every read with its answer (or none).
pub fn walk<S: VerbSource>(src: S, cad: u8) -> Vec<Line> {
    let mut rec = Recorder::new(src, cad);
    if let Ok(codec) = Codec::read(&mut rec, cad) {
        for (nid, verb) in state_reads(&codec) {
            let _ = rec.get(nid, verb);
        }
    }
    rec.into_lines()
}

/// How a dump compares with the walk for one codec.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Check {
    /// Reads the walk wants that the dump does not have (not even as `-`).
    pub missing: Vec<(Nid, u32)>,
    /// Reads in the dump the walk does not make.
    pub extra: Vec<(Nid, u32)>,
}

impl Check {
    pub fn complete(&self) -> bool {
        self.missing.is_empty() && self.extra.is_empty()
    }
}

/// Compares `dump` with the walk for codec `cad`.
pub fn check(dump: &Dump, cad: u8) -> Check {
    let wanted = walk(dump.source(cad), cad);
    let wanted_set: BTreeSet<(Nid, u32)> = wanted.iter().map(|l| (l.nid, l.verb)).collect();
    Check {
        missing: wanted
            .iter()
            .filter(|l| dump.get(cad, l.nid, l.verb).is_none())
            .map(|l| (l.nid, l.verb))
            .collect(),
        extra: dump
            .lines()
            .iter()
            .filter(|l| l.cad == cad && !wanted_set.contains(&(l.nid, l.verb)))
            .map(|l| (l.nid, l.verb))
            .collect(),
    }
}

/// A dump of codec `cad` made by walking `src`, with `header` fields first.
pub fn dump_codec<S: VerbSource>(src: S, cad: u8, header: &[(&str, &str)]) -> Dump {
    let mut d = Dump::new();
    d.header = header.iter().map(|&(k, v)| (k.into(), v.into())).collect();
    for line in walk(src, cad) {
        // The walk asks each read once and only read-only verbs, so this cannot conflict.
        let _ = d.push(line);
    }
    d
}
