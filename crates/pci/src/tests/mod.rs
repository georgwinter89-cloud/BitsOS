//! Host tests over a fake configuration space (`fake.rs`) and a Mini-PC-like world (`world.rs`).

mod aspm;
mod bind;
mod enable;
mod failed;
mod fake;
mod gpworld;
mod guard_gp;
mod hostile;
mod locks;
mod msi;
mod peer;
mod raw;
mod review;
mod smn;
mod walk;
mod world;

use crate::enumerate::Root;
use crate::node::Node;
/// The key the host tests' own rule hands out (`keys_off`).
pub const HELD: &str = "held";

/// A rule that keeps the wireless controllers untouched (as the Gaming-PC profile does with `gp_wlan`): the
/// Mini-PC world's WLAN at 04:00.0 stands for any function a rule holds.
pub fn keys_off(n: &Node) -> Option<&'static str> {
    crate::gp::is_wlan_class(n.class).then_some(HELD)
}

pub fn no_keys(_: &Node) -> Option<&'static str> {
    None
}

pub const ALL: [Root; 1] = [Root { segment: 0, bus_start: 0, bus_end: 255 }];

pub fn find(nodes: &[Node], bus: u8, dev: u8, func: u8) -> Option<&Node> {
    nodes.iter().find(|n| n.loc == crate::Loc::new(0, bus, dev, func))
}
