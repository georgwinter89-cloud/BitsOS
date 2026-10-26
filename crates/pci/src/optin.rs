//! Which functions the PCI core leaves alone (R8d) and who may bind mass storage (R8a).
//!
//! A function matched by an opt-in driver whose CONFIG key is not `on` is **untouched**: no BAR sizing, no
//! command register write, no driver; only the reads of the walk. The same holds for the core list below,
//! so the WLAN chip stays untouched even before its driver registers. A bridge whose functions below are all
//! untouched is untouched too (the Mini-PC's 00:02.2 above the WLAN at 04:00.0).

use crate::node::Node;
use crate::registry::Match;

/// Kept untouched without the key even when no driver claims them: the Mini-PC's RTL8852BE and every
/// wireless controller (class 02/80 is how Realtek and Intel WLAN cards report, class 0D is "wireless").
pub const CORE_OPT_IN: &[(Match, &str)] = &[
    (Match::Id { vendor: 0x10EC, device: 0xB852 }, "wlan"),
    (Match::Class { class: 0x02_80_00, mask: 0xFF_FF_00 }, "wlan"),
    (Match::Class { class: 0x0D_00_00, mask: 0xFF_00_00 }, "wlan"),
];

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
