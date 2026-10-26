mod fixtures;
mod graph;
mod hostile;
mod input;
mod minipc;
mod parse;
mod dpaudio;
mod eld;
mod units;
mod v1ref;

use std::vec::Vec;

use crate::dump::Dump;
use crate::model::Codec;
use crate::path::{Output, outputs};

pub(crate) const ALC269: &str = include_str!("../../fixtures/alc269-synthetic.txt");
pub(crate) const AA01: &str = include_str!("../../fixtures/aa01-synthetic.txt");

pub(crate) fn load(text: &str) -> (Dump, Codec, Vec<Output>) {
    let dump = Dump::parse(text).expect("fixture parses");
    let codec = Codec::read(&mut dump.source(0), 0).expect("codec answers");
    let outs = outputs(&codec);
    (dump, codec, outs)
}
