//! Which functions the PCI core leaves alone (R8d) and who may bind mass storage (R8a).
//!
//! A function a rule keeps **untouched** (the Gaming-PC's never-touch list and network rule, `gp`; the disk
//! guard's `nvme_bind=off`; the test function below) gets no BAR sizing, no command register write, no driver;
//! only the reads of the walk. A bridge whose functions below are all untouched is untouched too (the
//! Gaming-PC's 80:1c.3 above its WLAN card at 85:00.0).

use crate::node::Node;
use crate::registry::Match;

/// The ktest `pc.untouched.held`'s function: QEMU q35's ICH9 SMBus (no machine BitsOS runs on has this id), held
/// in every boot so the untouched rules are tested where a test driver matches it.
pub const TEST_HELD: &[(Match, &str)] = &[(Match::Id { vendor: 0x8086, device: 0x2930 }, "ktest_untouched")];

/// The only driver that may bind a class 01 (mass storage) function (R8a: the write gate lives in it).
pub const STORAGE_DRIVER: &str = "nvme";

/// R8a: mass storage binds to the NVMe driver and nothing else.
pub fn may_bind(driver: &str, n: &Node) -> bool {
    !n.is_storage() || driver == STORAGE_DRIVER
}

/// Sets `untouched` on every node `key_for` names a key for, then on every bridge whose functions below
/// are all untouched (children have larger indices than their bridge, so one pass from the end suffices).
pub fn mark_untouched(nodes: &mut [Node], key_for: impl Fn(&Node) -> Option<&'static str>) {
    for n in nodes.iter_mut() {
        n.untouched = key_for(n);
    }
    // For each bridge: (has a child, every child untouched, first child's key).
    let mut agg: alloc::vec::Vec<(bool, bool, Option<&'static str>)> = alloc::vec![(false, true, None); nodes.len()];
    for i in (0..nodes.len()).rev() {
        if nodes[i].is_bridge() && nodes[i].untouched.is_none() {
            let (has, all, key) = agg[i];
            if has && all {
                nodes[i].untouched = key;
            }
        }
        if let Some(p) = nodes[i].parent.filter(|&p| p < i) {
            let a = &mut agg[p];
            a.0 = true;
            a.1 &= nodes[i].untouched.is_some();
            if a.2.is_none() {
                a.2 = nodes[i].untouched;
            }
        }
    }
}
