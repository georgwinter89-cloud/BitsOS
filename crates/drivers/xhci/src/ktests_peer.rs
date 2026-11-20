//! GP-BUS (plan/spec/gaming-pc.md §5) for `host/tests/pc/60-peer-root.sh`: the keyboard behind the second PCI root.
//! The Gaming-PC's keyboard hangs on the xHCI at 80:14.0; here QEMU's hangs on an xHCI behind a `pxb-pcie` root at
//! bus 0x80. That controller must run and its keyboard must be enumerated and bound to the HID driver, as on every
//! machine (PCI scans every host bridge).

use crate::driver::controllers;
use crate::hc::{State, MS};
use alloc::format;
use ktest::{kassert, ktest, TestResult};
use usb_core::registry;

ktest!("pc.peer.keyboard", Run("pc/60-peer-root"), keyboard_behind_the_second_root);

const SETTLE_NS: u64 = 15_000 * MS;
/// QEMU's usb-kbd.
const QEMU_HID: u16 = 0x0627;

fn keyboard_behind_the_second_root() -> TestResult {
    let high: alloc::vec::Vec<_> = controllers().into_iter().filter(|c| c.env().pci().addr.bus >= 0x80).collect();
    // The report page's USB part: one line per xHCI function PCI found, each of them running here.
    let page = crate::gp_report::report_lines();
    let in_pci = pci::nodes().iter().filter(|n| n.class == crate::driver::CLASS_XHCI).count();
    let shown = page.iter().filter(|l| l.starts_with("USB-Baustein ")).count();
    kassert!(shown == in_pci && shown == controllers().len(), "{shown} controller lines, {in_pci} xHCI functions in PCI, {} controllers: {page:?}", controllers().len());
    kassert!(!page.iter().any(|l| l.contains("läuft NICHT")), "a controller line says it does not run: {page:?}");
    kassert!(high.len() == 1, "{} controllers behind the second root, the scenario has one", high.len());
    let c = &high[0];
    kassert!(c.state() == State::Running, "{} at {} is {}: {}", c.name(), c.env().pci().addr, c.state().as_str(), c.reason());
    let deadline = time::now_ns().saturating_add(SETTLE_NS);
    while !c.settled() && time::now_ns() < deadline {
        sched::sleep_ns(20 * MS);
    }
    kassert!(c.settled(), "{}: hotplug not settled within {} s", c.name(), SETTLE_NS / 1_000_000_000);
    let devs: alloc::vec::Vec<_> = registry::devices().into_iter().filter(|d| d.host_name() == c.name()).collect();
    let Some(kbd) = devs.iter().find(|d| d.vendor() == QEMU_HID) else {
        return ktest::fail(&format!("{}: no keyboard among {} devices", c.name(), devs.len()));
    };
    let drivers = registry::bindings_of(kbd.id);
    kassert!(drivers.iter().any(|(_, d)| *d == "usb-hid"), "the keyboard's drivers: {drivers:?}");
    let page = crate::gp_report::report_lines();
    let head = format!("USB-Baustein {}: läuft, ", c.env().pci().loc());
    let Some(at) = page.iter().position(|l| l.starts_with(&head)) else { return ktest::fail(&format!("no line '{head}...' on the page: {page:?}")) };
    let kbd_line = format!("Eingabegerät (Tastatur, Maus oder Ähnliches) [{:04x}:{:04x}]", kbd.vendor(), kbd.product());
    kassert!(page[at + 1..].iter().take_while(|l| l.starts_with("  ")).any(|l| l.contains(&kbd_line)), "the keyboard is not listed below its controller: {page:?}");
    klog::event!(Info, "usb", "peer.checked", ctrl = c.name(), function = format!("{}", c.env().pci().addr), keyboard = kbd.path(),
        devices = devs.len());
    Ok(())
}
