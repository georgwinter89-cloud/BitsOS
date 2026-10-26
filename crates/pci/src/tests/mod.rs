//! Host tests over a fake configuration space (`fake.rs`) and a Mini-PC-like world (`world.rs`).

mod aspm;
mod bind;
mod enable;
mod fake;
mod hostile;
mod msi;
mod raw;
mod review;
mod walk;
mod world;

use crate::enumerate::Root;
use crate::node::Node;
use crate::optin::CORE_OPT_IN;

/// The untouched rule as the kernel applies it with every key off (no registry in host tests).
pub fn keys_off(n: &Node) -> Option<&'static str> {
    CORE_OPT_IN.iter().find(|(m, _)| m.hits(n.vendor, n.device, n.class)).map(|(_, k)| *k)
}

pub fn no_keys(_: &Node) -> Option<&'static str> {
    None
}

pub const ALL: [Root; 1] = [Root { segment: 0, bus_start: 0, bus_end: 255 }];

pub fn find(nodes: &[Node], bus: u8, dev: u8, func: u8) -> Option<&Node> {
    nodes.iter().find(|n| n.loc == crate::Loc::new(0, bus, dev, func))
}
