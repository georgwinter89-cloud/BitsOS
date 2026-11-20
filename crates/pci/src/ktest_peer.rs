//! GP-BUS ktest (plan/spec/gaming-pc.md §5) for `host/tests/pc/60-peer-root.sh`: q35 with a `pxb-pcie` root at bus
//! 0x80, one root port at 80:14.0 (bus 80 has no device 0, as on the Gaming-PC) and an xHCI with a keyboard behind
//! it. The device behind the second root must be found, sized at the firmware's address and bound, as on every
//! machine (Linux scans every host bridge). Reads only.

use crate::{bind, Loc, Node};
use alloc::vec::Vec;
use ktest::{kassert, ktest};

type R = ktest::TestResult;

ktest!("pc.peer.second_root", Run("pc/60-peer-root"), second_root);

const PEER_BUS: u8 = 0x80;
const QEMU_XHCI: (u16, u16) = (0x1B36, 0x000D);

fn second_root() -> R {
    let nodes = crate::tree();
    let roots = crate::roots();
    let planned = crate::peer_plan();
    let high: Vec<&Node> = nodes.iter().filter(|n| n.loc.bus >= PEER_BUS).collect();
    kassert!(planned.len() == 1 && planned[0].cut() && !planned[0].from_facts, "plan: {planned:?}");
    kassert!(planned[0].plan.peers.len() == 1 && planned[0].plan.peers[0].bus == PEER_BUS, "peers: {:?}", planned[0].plan.peers);
    kassert!(planned[0].plan.peers[0].first_device == 0x14, "bus 80 answered first at device {:#x}", planned[0].plan.peers[0].first_device);
    kassert!(roots.len() == 2 && roots[0].bus_end == PEER_BUS - 1 && roots[1].bus_start == PEER_BUS, "roots: {roots:?}");
    kassert!(roots[1].bus_end == planned[0].window.bus_end, "the ECAM end moved: {roots:?}");
    // The root port sits on the root bus with no parent; the xHCI hangs below it.
    let Some(port) = nodes.iter().position(|n| n.loc == Loc::new(0, PEER_BUS, 0x14, 0)) else { return ktest::fail("no function at 80:14.0") };
    kassert!(nodes[port].parent.is_none() && nodes[port].pcie.is_some_and(|p| p.is_root_port()), "80:14.0 is not a root port on a root bus");
    let Some(x) = nodes.iter().find(|n| (n.vendor, n.device) == QEMU_XHCI && n.parent == Some(port)) else {
        return ktest::fail("no xHCI behind the second root's port");
    };
    kassert!(x.problems.is_empty() && nodes[port].problems.is_empty(), "problems: {:?} {:?}", x.problems, nodes[port].problems);
    let Some(bar) = x.bars.and_then(|b| b[0]) else { return ktest::fail("the xHCI's BAR0 was not sized") };
    kassert!(bar.assigned() && bar.size() >= 0x1000, "BAR0 {:#x} size {:#x}", bar.start(), bar.size());
    // The firmware's address stands: the register still holds what the scan read.
    let f = x.function();
    let now = (f.read32(0x14) as u64) << 32 | (f.read32(0x10) & !0xF) as u64;
    kassert!(now == bar.start(), "BAR0 register {now:#x}, scanned {:#x}", bar.start());
    let b = nodes[port].bridge.ok_or("80:14.0 has no bridge header")?;
    kassert!(b.secondary == x.loc.bus && b.secondary > PEER_BUS, "bridge {:02x}-{:02x}, xHCI on bus {:02x}", b.secondary, b.subordinate, x.loc.bus);
    kassert!(bind::bound_driver(x.loc) == Some("xhci"), "driver of {}: {:?}", x.loc, bind::binding(x.loc));
    // The report page's line for it, from the live binding (nothing else reads the page before B10).
    let page = crate::gp_report_lines();
    let want = alloc::format!("  {:02x}:{:02x}.{} USB-Anschlüsse [1b36:000d]: läuft mit xhci", x.loc.bus, x.loc.dev, x.loc.func);
    kassert!(page.first().is_some_and(|l| l.starts_with("Anschluss-Stränge: 2 ")), "the page's first line: {:?}", page.first());
    kassert!(page.contains(&want), "no line '{want}' on the page: {page:?}");
    kassert!(crate::failed::last(x.loc).is_none(), "a failed probe is noted for the bound xHCI: {:?}", crate::failed::last(x.loc));
    // Nothing below bus 80 hangs below the peer root.
    kassert!(nodes.iter().filter(|n| n.loc.bus < PEER_BUS).all(|n| n.parent.is_none_or(|p| nodes[p].loc.bus < PEER_BUS)), "a low function below the peer root");
    klog::event!(Info, "pci", "peer.checked", roots = roots.len(), functions = nodes.len(), high = high.len(),
        xhci = alloc::format!("{}", x.loc), bar0 = klog::Hex(bar.start()));
    Ok(())
}
