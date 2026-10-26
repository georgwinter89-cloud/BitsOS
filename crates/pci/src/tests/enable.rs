//! PC-2 over the fake space: D states, the bridge path, stale MSI, refusals, disable order, save/restore.

use super::fake::{bridge, root_port, Fake, FakeFn};
use super::world::{minipc, I226, WLAN, WLAN_BRIDGE};
use super::{find, keys_off, no_keys, ALL};
use crate::enable::{self, Change, EnableError, Outcome, CMD_INTX_OFF, CMD_MASTER, CMD_MEM, D2_DELAY_NS, D3HOT_DELAY_NS};
use crate::node::{Bar, Node, Problem};
use crate::{build, ConfigAccess, Loc};
use alloc::vec::Vec;

const RP: Loc = Loc::new(0, 0, 1, 0);
const BR: Loc = Loc::new(0, 1, 0, 0);
const DEV: Loc = Loc::new(0, 2, 1, 0);

/// A device with BAR0 (32-bit, 4 KiB), BAR2 (64-bit prefetchable, 1 MiB above 4 GiB), MSI at 0x50 and
/// MSI-X at 0x70, both left enabled (a previous kernel), command 0.
fn device() -> FakeFn {
    let mut f = FakeFn::new(0x1234, 0x0011, 0x02_00_00, 0);
    f.cmd(0).bar32(0, 0xFE10_0000, 0x1000, false).bar64(2, 0x80_0000_0000, 0x10_0000, true);
    f.msi(0x50, 0x70, true).msix(0x70, 0, true, 0);
    f
}

/// Host bridge, root port 00:01.0 -> bus 1, PCI bridge 01:00.0 -> bus 2, `dev` at 02:01.0. Both bridges
/// with bus master off (the Mini-PC firmware's state above unused devices) and forwarding windows that hold
/// the device's BARs.
fn chain(dev: FakeFn) -> Fake {
    let w = Fake::new();
    let mut hb = FakeFn::new(0x8086, 0x29C0, 0x06_00_00, 0);
    hb.cmd(0x6);
    w.add(0, 0, 0, hb);
    let mut rp = root_port(0, 1, 2);
    rp.cmd(0).windows((0xFE00_0000, 0xFEFF_FFFF), (0x80_0000_0000, 0x80_FFFF_FFFF));
    w.add_at(RP, rp);
    let mut br = bridge(0x1B36, 0x0001, 1, 2, 2);
    br.cmd(CMD_MEM).windows((0xFE10_0000, 0xFE1F_FFFF), (0x80_0000_0000, 0x80_0FFF_FFFF));
    w.add_at(BR, br);
    w.add_at(DEV, dev);
    w
}

fn limit() -> u64 {
    hal::mmio::phys_limit(48)
}

fn idx(nodes: &[Node], at: Loc) -> usize {
    nodes.iter().position(|n| n.loc == at).unwrap()
}

fn cmd(w: &Fake, at: Loc) -> u16 {
    crate::access::read16(w, at, 4)
}

/// Runs `enable` and records the waits.
fn run(w: &Fake, nodes: &[Node], at: Loc, dma: bool, storage: bool) -> (Outcome, Vec<u64>) {
    let mut waits = Vec::new();
    let o = enable::enable(w, nodes, idx(nodes, at), dma, storage, true, limit(), &mut |ns| waits.push(ns));
    (o, waits)
}

fn command_writes(w: &Fake) -> Vec<Loc> {
    w.writes.lock().unwrap().iter().filter(|x| x.1 == 4).map(|x| x.0).collect()
}

#[test]
fn dma_turns_bus_master_on_every_bridge_root_first() {
    let w = chain(device());
    let nodes = build(&w, &ALL, no_keys);
    w.writes.lock().unwrap().clear();
    let (o, waits) = run(&w, &nodes, DEV, true, false);
    assert_eq!(o.error, None);
    assert!(waits.is_empty(), "no PM capability: no wait");
    // Root port first (bus master + memory forwarding), then the bridge (bus master; memory was on).
    let bridges: Vec<_> = o.changes.iter().filter_map(|c| if let Change::Bridge { at, old, new } = c { Some((*at, *old, *new)) } else { None }).collect();
    assert_eq!(bridges, [(RP, 0, CMD_MEM | CMD_MASTER), (BR, CMD_MEM, CMD_MEM | CMD_MASTER)]);
    assert!(o.changes.contains(&Change::Command { old: 0, new: CMD_MEM | CMD_MASTER | CMD_INTX_OFF }));
    assert!(o.changes.contains(&Change::MsiCleared) && o.changes.contains(&Change::MsixCleared));
    assert_eq!(command_writes(&w), [RP, BR, DEV], "order: root port, bridge, device");
    assert_eq!(cmd(&w, RP), CMD_MEM | CMD_MASTER);
    assert_eq!(cmd(&w, BR), CMD_MEM | CMD_MASTER);
    assert_eq!(cmd(&w, DEV), CMD_MEM | CMD_MASTER | CMD_INTX_OFF);
    assert_eq!(w.read32(DEV, 0x50) & (1 << 16), 0, "MSI still enabled");
    assert_eq!(w.read32(DEV, 0x70) & (1 << 31), 0, "MSI-X still enabled");
    assert_eq!(w.writes_to(0, 0, 0), 0, "host bridge written");
    assert!(!o.changes.iter().any(|c| matches!(c, Change::BarOutsideWindow { .. })), "{:?}", o.changes);
    // The copy for a later restore is the header after the enable.
    let saved = o.saved.unwrap();
    assert_eq!(saved.at, DEV);
    assert_eq!(saved.header[1] as u16, CMD_MEM | CMD_MASTER | CMD_INTX_OFF);
    assert_eq!(saved.header[4], 0xFE10_0000);
}

#[test]
fn second_enable_writes_nothing() {
    let w = chain(device());
    let nodes = build(&w, &ALL, no_keys);
    assert_eq!(run(&w, &nodes, DEV, true, false).0.error, None);
    w.writes.lock().unwrap().clear();
    let (o, _) = run(&w, &nodes, DEV, true, false);
    assert_eq!(o.error, None);
    assert!(o.changes.is_empty(), "{:?}", o.changes);
    assert!(w.writes.lock().unwrap().is_empty(), "{:?}", w.writes.lock().unwrap());
}

#[test]
fn without_dma_no_bus_master_anywhere() {
    let w = chain(device());
    let nodes = build(&w, &ALL, no_keys);
    let (o, _) = run(&w, &nodes, DEV, false, false);
    assert_eq!(o.error, None);
    assert_eq!(cmd(&w, RP), CMD_MEM, "root port: forwarding only");
    assert_eq!(cmd(&w, BR), CMD_MEM, "bridge unchanged");
    assert_eq!(cmd(&w, DEV), CMD_MEM | CMD_INTX_OFF);
    // Bits are only ever set: an enable without DMA after one with DMA keeps bus master.
    run(&w, &nodes, DEV, true, false);
    run(&w, &nodes, DEV, false, false);
    assert_eq!(cmd(&w, DEV) & CMD_MASTER, CMD_MASTER);
    assert_eq!(cmd(&w, RP) & CMD_MASTER, CMD_MASTER);
}

#[test]
fn io_only_device_gets_io_decoding_and_no_memory() {
    let mut d = FakeFn::new(0x1234, 0x12, 0x07_00_00, 0);
    d.bario(0, 0xE000, 0x20, true);
    let w = chain(d);
    let nodes = build(&w, &ALL, no_keys);
    let (o, _) = run(&w, &nodes, DEV, false, false);
    assert_eq!(cmd(&w, DEV), enable::CMD_IO | CMD_INTX_OFF);
    // No I/O window on the bridges: forwarding not switched on, and the BAR is reported.
    assert_eq!(cmd(&w, RP), 0);
    assert!(o.changes.contains(&Change::BarOutsideWindow { index: 0, bridge: BR, start: 0xE000 }), "{:?}", o.changes);
}

#[test]
fn d3hot_wake_waits_10ms_then_sizes() {
    let mut d = device();
    d.pm(0x40, 0x50, 3, false);
    let w = chain(d);
    let nodes = build(&w, &ALL, no_keys);
    let n = &nodes[idx(&nodes, DEV)];
    assert_eq!(n.bars, None, "a D3hot function is not sized at scan");
    assert!(n.problems.contains(&Problem::BarPowerState { state: 3 }));
    let (o, waits) = run(&w, &nodes, DEV, true, false);
    assert_eq!(o.error, None);
    assert_eq!(waits, [D3HOT_DELAY_NS]);
    assert_eq!(o.changes[0], Change::Woke { at: DEV, from: 3, waited_ns: D3HOT_DELAY_NS, soft_reset: false });
    assert_eq!(w.read32(DEV, 0x44) & 3, 0, "not in D0");
    let b = o.sized.expect("sized after the wake");
    assert_eq!(b[0], Some(Bar::Mem { phys: 0xFE10_0000, size: 0x1000, prefetch: false, is64: false }));
    assert_eq!(b[2], Some(Bar::Mem { phys: 0x80_0000_0000, size: 0x10_0000, prefetch: true, is64: true }));
    assert!(!o.changes.iter().any(|c| matches!(c, Change::Restored { .. })), "no reset, nothing to restore");
    assert_eq!(cmd(&w, DEV), CMD_MEM | CMD_MASTER | CMD_INTX_OFF);
    // PMCSR written as Linux does: the whole word 0 (PME_En off).
    assert!(w.writes.lock().unwrap().iter().any(|x| x.0 == DEV && x.1 == 0x44 && x.2 & 0xFFFF == 0));
}

#[test]
fn d3hot_wake_with_reset_writes_the_header_back() {
    let mut d = device();
    d.pm(0x40, 0x50, 3, true).cmd(CMD_MEM).w8(0x3C, 11).w8(0x0C, 0x10);
    let w = chain(d);
    let nodes = build(&w, &ALL, no_keys);
    w.cmd_at_bar_write.lock().unwrap().clear();
    let (o, waits) = run(&w, &nodes, DEV, true, false);
    assert_eq!(o.error, None, "{:?}", o.changes);
    assert_eq!(waits, [D3HOT_DELAY_NS]);
    assert_eq!(o.changes[0], Change::Woke { at: DEV, from: 3, waited_ns: D3HOT_DELAY_NS, soft_reset: true });
    let restored: Vec<u16> = o.changes.iter().filter_map(|c| if let Change::Restored { offset, .. } = c { Some(*offset) } else { None }).collect();
    // Interrupt line, BAR3 (upper half of BAR2; its lower half is 0 + flags anyway), BAR0, cache line,
    // then the command register.
    assert_eq!(restored, [0x3C, 0x1C, 0x10, 0x0C, 0x04]);
    assert_eq!(w.read32(DEV, 0x10), 0xFE10_0000);
    assert_eq!(w.read32(DEV, 0x18) & !0xF, 0);
    assert_eq!(w.read32(DEV, 0x1C), 0x80);
    assert_eq!(w.read32(DEV, 0x3C) & 0xFF, 11);
    assert_eq!(o.sized.unwrap()[2].unwrap().start(), 0x80_0000_0000, "sized after the restore");
    assert_eq!(cmd(&w, DEV), CMD_MEM | CMD_MASTER | CMD_INTX_OFF);
    for (at, off, c) in w.cmd_at_bar_write.lock().unwrap().iter() {
        assert!(*at != DEV || c & (CMD_MEM | enable::CMD_IO) == 0, "BAR {off:#x} written with decoding on ({c:#x})");
    }
}

#[test]
fn stuck_in_d3hot_fails_after_one_retry_and_switches_nothing_on() {
    let mut d = device();
    d.pm(0x40, 0x50, 3, false);
    d.pm.as_mut().unwrap().stuck = true;
    let w = chain(d);
    let nodes = build(&w, &ALL, no_keys);
    let before = (cmd(&w, RP), cmd(&w, BR), cmd(&w, DEV));
    let (o, waits) = run(&w, &nodes, DEV, true, false);
    assert_eq!(o.error, Some(EnableError::PowerStuck { state: 3 }));
    assert_eq!(waits, [D3HOT_DELAY_NS, D3HOT_DELAY_NS]);
    assert_eq!(o.changes, [Change::WakeRetry { at: DEV, state: 3 }]);
    assert_eq!((cmd(&w, RP), cmd(&w, BR), cmd(&w, DEV)), before, "nothing enabled for a function that stays asleep");
    assert_eq!(EnableError::PowerStuck { state: 3 }.name(), "enable.failed");
}

#[test]
fn slow_wake_succeeds_on_the_retry() {
    let mut d = device();
    d.pm(0x40, 0x50, 3, false);
    d.pm.as_mut().unwrap().lag = 1;
    let w = chain(d);
    let nodes = build(&w, &ALL, no_keys);
    let (o, waits) = run(&w, &nodes, DEV, true, false);
    assert_eq!(o.error, None);
    assert_eq!(waits, [D3HOT_DELAY_NS, D3HOT_DELAY_NS]);
    assert_eq!(o.changes[0], Change::WakeRetry { at: DEV, state: 3 });
    assert_eq!(o.changes[1], Change::Woke { at: DEV, from: 3, waited_ns: 2 * D3HOT_DELAY_NS, soft_reset: false });
}

#[test]
fn d2_waits_200us_and_d1_not_at_all() {
    for (state, want) in [(2u8, alloc::vec![D2_DELAY_NS]), (1, alloc::vec![])] {
        let mut d = device();
        d.pm(0x40, 0x50, state, true);
        let w = chain(d);
        let nodes = build(&w, &ALL, no_keys);
        let (o, waits) = run(&w, &nodes, DEV, false, false);
        assert_eq!(o.error, None);
        assert_eq!(waits, want, "D{state}");
        // Only D3hot resets: no restore from D1/D2 even with No_Soft_Reset clear.
        assert_eq!(o.changes[0], Change::Woke { at: DEV, from: state, waited_ns: want.iter().sum(), soft_reset: false });
        assert_eq!(w.read32(DEV, 0x44) & 3, 0);
    }
}

#[test]
fn d0_function_is_not_written_through_pmcsr() {
    let mut d = device();
    d.pm(0x40, 0x50, 0, false);
    let w = chain(d);
    let nodes = build(&w, &ALL, no_keys);
    let (o, waits) = run(&w, &nodes, DEV, true, false);
    assert_eq!(o.error, None);
    assert!(waits.is_empty());
    assert!(!w.writes.lock().unwrap().iter().any(|x| x.0 == DEV && x.1 == 0x44));
}

#[test]
fn untouched_function_is_refused_without_any_write() {
    let mut d = device();
    d.pm(0x40, 0x50, 3, false);
    let w = chain(d);
    let key = |n: &Node| (n.loc == DEV).then_some("wlan");
    let nodes = build(&w, &ALL, key);
    // Its bridge serves only it: untouched too.
    assert_eq!(nodes[idx(&nodes, BR)].untouched, Some("wlan"));
    w.writes.lock().unwrap().clear();
    let (o, waits) = run(&w, &nodes, DEV, true, false);
    assert_eq!(o.error, Some(EnableError::Untouched("wlan")));
    assert_eq!(o.error.unwrap().name(), "enable.refused");
    assert!(waits.is_empty() && o.changes.is_empty());
    assert!(w.writes.lock().unwrap().is_empty());
    // A touched function below an untouched bridge (cannot come from the scan) is refused too.
    let mut nodes = build(&w, &ALL, no_keys);
    let b = idx(&nodes, BR);
    nodes[b].untouched = Some("wlan");
    w.writes.lock().unwrap().clear();
    assert_eq!(run(&w, &nodes, DEV, true, false).0.error, Some(EnableError::BridgeUntouched(BR)));
    assert!(w.writes.lock().unwrap().is_empty());
}

#[test]
fn mass_storage_only_for_the_nvme_driver() {
    let mut d = FakeFn::new(0x126F, 0x2268, 0x01_08_02, 0);
    d.bar64(0, 0xFE10_0000, 0x4000, false);
    let w = chain(d);
    let nodes = build(&w, &ALL, no_keys);
    w.writes.lock().unwrap().clear();
    let (o, _) = run(&w, &nodes, DEV, true, false);
    assert_eq!(o.error, Some(EnableError::Storage));
    assert!(w.writes.lock().unwrap().is_empty());
    let (o, _) = run(&w, &nodes, DEV, true, true);
    assert_eq!(o.error, None);
    assert_eq!(cmd(&w, DEV), CMD_MEM | CMD_MASTER | CMD_INTX_OFF);
}

#[test]
fn gone_function_is_not_written() {
    let w = chain(device());
    let nodes = build(&w, &ALL, no_keys);
    w.fns.lock().unwrap().remove(&DEV);
    w.writes.lock().unwrap().clear();
    assert_eq!(run(&w, &nodes, DEV, true, false).0.error, Some(EnableError::Gone));
    assert!(w.writes.lock().unwrap().is_empty());
    // Another card in the same slot is not the scanned function either.
    w.add_at(DEV, FakeFn::new(0x1234, 0x0099, 0x02_00_00, 0));
    assert_eq!(run(&w, &nodes, DEV, true, false).0.error, Some(EnableError::Gone));
    assert!(w.writes.lock().unwrap().is_empty());
}

#[test]
fn disable_switches_msi_off_before_bus_master_and_keeps_the_rest() {
    let w = chain(device());
    let nodes = build(&w, &ALL, no_keys);
    run(&w, &nodes, DEV, true, false);
    // The driver set its MSI and MSI-X up again.
    {
        let mut fns = w.fns.lock().unwrap();
        let f = fns.get_mut(&DEV).unwrap();
        let (m, x) = (f.r32(0x50), f.r32(0x70));
        f.w32(0x50, m | 1 << 16).w32(0x70, x | 1 << 31);
    }
    w.writes.lock().unwrap().clear();
    let d = enable::disable(&w, DEV, 0x50, 0x70);
    assert!(d.msi_was_on && d.msix_was_on && d.bus_master_was_on);
    let offs: Vec<u16> = w.writes.lock().unwrap().iter().map(|x| { assert_eq!(x.0, DEV); x.1 }).collect();
    assert_eq!(offs, [0x50, 0x70, 0x04], "MSI, MSI-X, then the command register; nothing else");
    assert_eq!(cmd(&w, DEV), CMD_MEM | CMD_INTX_OFF, "bus master off, memory and INTx-off kept");
    assert_eq!(cmd(&w, RP) & CMD_MASTER, CMD_MASTER, "bridges stay as they are");
    assert_eq!(w.read32(DEV, 0x50) & 1 << 16, 0);
    assert_eq!(w.read32(DEV, 0x70) & 1 << 31, 0);
    // Again: nothing on, nothing written.
    w.writes.lock().unwrap().clear();
    assert_eq!(enable::disable(&w, DEV, 0x50, 0x70), enable::Disabled::default());
    assert!(w.writes.lock().unwrap().is_empty());
    // A function that vanished (all ones) is not written.
    w.fns.lock().unwrap().remove(&DEV);
    assert_eq!(enable::disable(&w, DEV, 0x50, 0x70), enable::Disabled::default());
    assert!(w.writes.lock().unwrap().is_empty());
}

#[test]
fn restore_after_a_link_retrain() {
    let w = chain(device());
    let nodes = build(&w, &ALL, no_keys);
    let saved = run(&w, &nodes, DEV, true, false).0.saved.unwrap();
    // V1's I226 after a retrain: BAR0/BAR1 read 0 (here BAR0 and the upper half of the 64-bit BAR2),
    // command kept.
    {
        let mut fns = w.fns.lock().unwrap();
        let f = fns.get_mut(&DEV).unwrap();
        f.w32(0x10, 0).w32(0x1C, 0);
    }
    w.cmd_at_bar_write.lock().unwrap().clear();
    w.writes.lock().unwrap().clear();
    let ch = enable::restore(&w, &saved, false, true).unwrap();
    let offs: Vec<u16> = ch.iter().map(|c| if let Change::Restored { offset, .. } = c { *offset } else { panic!("{c:?}") }).collect();
    assert_eq!(offs, [0x1C, 0x10], "only the lost dwords; the command register was right all along");
    assert_eq!(w.read32(DEV, 0x10), 0xFE10_0000);
    assert_eq!(w.read32(DEV, 0x1C), 0x80);
    assert!(w.cmd_at_bar_write.lock().unwrap().iter().all(|x| x.2 & CMD_MEM == 0), "BARs moved with decoding on");
    let cmds: Vec<u32> = w.writes.lock().unwrap().iter().filter(|x| x.1 == 4).map(|x| x.2).collect();
    assert_eq!(cmds, [(CMD_MASTER | CMD_INTX_OFF) as u32, (CMD_MEM | CMD_MASTER | CMD_INTX_OFF) as u32], "decode off, then back last");
    // Nothing lost: nothing written.
    w.writes.lock().unwrap().clear();
    assert!(enable::restore(&w, &saved, false, true).unwrap().is_empty());
    assert!(w.writes.lock().unwrap().is_empty());
    // Another device answers there now: refused.
    w.add_at(DEV, FakeFn::new(0x1234, 0x0099, 0x02_00_00, 0));
    assert_eq!(enable::restore(&w, &saved, false, true), Err(EnableError::Gone));
}

#[test]
fn bar_outside_the_bridge_windows_is_reported() {
    let mut d = device();
    d.bar32(0, 0xF000_0000, 0x1000, false);
    let w = chain(d);
    let nodes = build(&w, &ALL, no_keys);
    let (o, _) = run(&w, &nodes, DEV, true, false);
    assert_eq!(o.error, None);
    let out: Vec<_> = o.changes.iter().filter(|c| matches!(c, Change::BarOutsideWindow { .. })).collect();
    assert_eq!(out, [&Change::BarOutsideWindow { index: 0, bridge: BR, start: 0xF000_0000 }]);
}

#[test]
fn dma_status_names_the_path_and_the_trouble() {
    let w = chain(device());
    let nodes = build(&w, &ALL, no_keys);
    // A master abort seen on the bridge's secondary side, UR on the root port (the Mini-PC's round 2).
    {
        let mut fns = w.fns.lock().unwrap();
        fns.get_mut(&BR).unwrap().w16(0x1E, 1 << 13);
        fns.get_mut(&RP).unwrap().w16(0x58 + 0x0A, 1 << 3 | 1 << 5);
    }
    let s = enable::dma_status(&w, &nodes, idx(&nodes, DEV));
    assert_eq!(s.iter().map(|x| x.at).collect::<Vec<_>>(), [DEV, BR, RP]);
    assert!(!s[0].trouble() && s[0].secondary_status.is_none() && s[0].pcie_dev_status.is_none());
    assert!(s[1].trouble() && !s[1].bus_master());
    assert!(s[2].trouble() && s[2].pcie_dev_status == Some(0x28));
}

#[test]
fn minipc_i226_switches_on_only_its_root_port() {
    let w = minipc();
    // Firmware state of round 2: 00:02.1 has no bus master; its window holds the I226's BARs.
    {
        let mut fns = w.fns.lock().unwrap();
        fns.get_mut(&Loc::new(0, 0, 2, 1)).unwrap().cmd(CMD_MEM).windows((0xDCA0_0000, 0xDCBF_FFFF), (0, 0));
    }
    let nodes = build(&w, &ALL, keys_off);
    w.writes.lock().unwrap().clear();
    let nic = Loc::new(0, I226.0, 0, 0);
    let (o, _) = run(&w, &nodes, nic, true, false);
    assert_eq!(o.error, None);
    let bridges: Vec<_> = o.changes.iter().filter_map(|c| if let Change::Bridge { at, .. } = c { Some(*at) } else { None }).collect();
    assert_eq!(bridges, [Loc::new(0, 0, 2, 1)]);
    assert!(!o.changes.iter().any(|c| matches!(c, Change::BarOutsideWindow { .. })), "{:?}", o.changes);
    let touched: alloc::collections::BTreeSet<Loc> = w.writes.lock().unwrap().iter().map(|x| x.0).collect();
    assert_eq!(touched.into_iter().collect::<Vec<_>>(), [Loc::new(0, 0, 2, 1), nic]);
    // The WLAN and its port: refused, not a single write (R8d).
    w.writes.lock().unwrap().clear();
    let (o, _) = run(&w, &nodes, Loc::new(0, WLAN.0, 0, 0), true, false);
    assert_eq!(o.error, Some(EnableError::Untouched("wlan")));
    let wb = find(&nodes, WLAN_BRIDGE.0, WLAN_BRIDGE.1, WLAN_BRIDGE.2).unwrap();
    assert_eq!(wb.untouched, Some("wlan"));
    assert!(w.writes.lock().unwrap().is_empty());
    // The NVMe without the NVMe driver: refused, nothing written.
    let (o, _) = run(&w, &nodes, Loc::new(0, 1, 0, 0), true, false);
    assert_eq!(o.error, Some(EnableError::Storage));
    assert!(w.writes.lock().unwrap().is_empty());
}

/// Checks that every command-register and MSI/MSI-X control write happens inside `modify32` (the kernel's
/// wrapper holds `pci.cfg` across it, so two enables below one bridge cannot lose each other's bits).
struct RmwOnly<'a> {
    inner: &'a Fake,
    in_modify: std::sync::Mutex<bool>,
    guarded: &'a [(Loc, u16)],
}

impl ConfigAccess for RmwOnly<'_> {
    fn read32(&self, at: Loc, off: u16) -> u32 {
        self.inner.read32(at, off)
    }
    fn write32(&self, at: Loc, off: u16, v: u32) -> bool {
        let rmw = off & !3 == 4 || self.guarded.contains(&(at, off & !3));
        assert!(!rmw || *self.in_modify.lock().unwrap(), "{at} {off:#x} written outside a read-modify-write");
        self.inner.write32(at, off, v)
    }
    fn modify32(&self, at: Loc, off: u16, f: &mut dyn FnMut(u32) -> Option<u32>) -> u32 {
        *self.in_modify.lock().unwrap() = true;
        let old = self.read32(at, off);
        if let Some(new) = f(old) {
            self.write32(at, off, new);
        }
        *self.in_modify.lock().unwrap() = false;
        old
    }
    fn space_size(&self, segment: u16, bus: u8) -> u16 {
        self.inner.space_size(segment, bus)
    }
    fn name(&self) -> &'static str {
        "rmw"
    }
}

#[test]
fn command_and_msi_writes_are_read_modify_writes() {
    let w = chain(device());
    let nodes = build(&w, &ALL, no_keys);
    let c = RmwOnly { inner: &w, in_modify: std::sync::Mutex::new(false), guarded: &[(DEV, 0x50), (DEV, 0x70)] };
    let o = enable::enable(&c, &nodes, idx(&nodes, DEV), true, false, true, limit(), &mut |_| {});
    assert_eq!(o.error, None);
    assert_eq!(cmd(&w, RP), CMD_MEM | CMD_MASTER);
    assert!(o.changes.contains(&Change::MsiCleared));
    // Restore after a retrain and disable go through it too.
    w.fns.lock().unwrap().get_mut(&DEV).unwrap().w32(0x10, 0);
    assert!(!enable::restore(&c, &o.saved.unwrap(), false, true).unwrap().is_empty());
    assert!(enable::disable(&c, DEV, 0x50, 0x70).bus_master_was_on);
}

#[test]
fn later_enables_keep_the_drivers_msi() {
    let w = chain(device());
    let nodes = build(&w, &ALL, no_keys);
    assert_eq!(run(&w, &nodes, DEV, true, false).0.error, None);
    // The driver sets its MSI up, then enables again (a retry, a mode switch): not the first enable.
    {
        let mut fns = w.fns.lock().unwrap();
        let f = fns.get_mut(&DEV).unwrap();
        let m = f.r32(0x50);
        f.w32(0x50, m | 1 << 16);
    }
    w.writes.lock().unwrap().clear();
    let o = enable::enable(&w, &nodes, idx(&nodes, DEV), true, false, false, limit(), &mut |_| {});
    assert_eq!(o.error, None);
    assert!(o.changes.is_empty(), "{:?}", o.changes);
    assert!(w.writes.lock().unwrap().is_empty());
    assert_ne!(w.read32(DEV, 0x50) & 1 << 16, 0, "the driver's MSI was switched off");
}

#[test]
fn bridge_in_d3hot_is_woken_first() {
    let w = chain(device());
    w.fns.lock().unwrap().get_mut(&RP).unwrap().pm(0x40, 0x58, 3, false);
    let nodes = build(&w, &ALL, no_keys);
    w.writes.lock().unwrap().clear();
    let (o, waits) = run(&w, &nodes, DEV, true, false);
    assert_eq!(o.error, None, "{:?}", o.changes);
    assert_eq!(waits, [D3HOT_DELAY_NS]);
    assert_eq!(o.changes[0], Change::Woke { at: RP, from: 3, waited_ns: D3HOT_DELAY_NS, soft_reset: false });
    assert_eq!(w.read32(RP, 0x44) & 3, 0, "root port not in D0");
    assert_eq!(w.writes.lock().unwrap()[0].0, RP, "the bridge is woken before anything else is written");
    assert_eq!(cmd(&w, RP), CMD_MEM | CMD_MASTER);
    assert_eq!(cmd(&w, DEV), CMD_MEM | CMD_MASTER | CMD_INTX_OFF);
    // A bridge that stays asleep: nothing below it is touched.
    let w = chain(device());
    {
        let mut fns = w.fns.lock().unwrap();
        let rp = fns.get_mut(&RP).unwrap();
        rp.pm(0x40, 0x58, 3, false);
        rp.pm.as_mut().unwrap().stuck = true;
    }
    let nodes = build(&w, &ALL, no_keys);
    w.writes.lock().unwrap().clear();
    let (o, _) = run(&w, &nodes, DEV, true, false);
    assert_eq!(o.error, Some(EnableError::BridgeAsleep { at: RP, state: 3 }));
    assert_eq!(o.error.unwrap().name(), "enable.failed");
    assert!(w.writes.lock().unwrap().iter().all(|x| x.0 == RP), "written below a sleeping bridge");
    assert_eq!(cmd(&w, DEV), 0);
}

#[test]
fn restore_keeps_bus_master_off_after_a_disable() {
    let w = chain(device());
    let nodes = build(&w, &ALL, no_keys);
    let saved = run(&w, &nodes, DEV, true, false).0.saved.unwrap();
    assert!(enable::disable(&w, DEV, 0x50, 0x70).bus_master_was_on);
    // Nothing lost: nothing written, bus master stays off.
    w.writes.lock().unwrap().clear();
    assert_eq!(enable::restore(&w, &saved, false, false), Ok(alloc::vec![]));
    assert!(w.writes.lock().unwrap().is_empty(), "{:?}", w.writes.lock().unwrap());
    // A retrain afterwards: BAR0 back, decoding back, bus master still off.
    w.fns.lock().unwrap().get_mut(&DEV).unwrap().w32(0x10, 0);
    let ch = enable::restore(&w, &saved, false, false).unwrap();
    assert_eq!(ch, [Change::Restored { at: DEV, offset: 0x10, was: 0, now: 0xFE10_0000 }], "the command ends as it was: not reported");
    assert_eq!(cmd(&w, DEV), CMD_MEM | CMD_INTX_OFF);
    // Command lost too (a reset): decoding comes back from the copy, bus master only when asked.
    {
        let mut fns = w.fns.lock().unwrap();
        fns.get_mut(&DEV).unwrap().w32(0x10, 0).cmd(0);
    }
    let ch = enable::restore(&w, &saved, false, false).unwrap();
    assert!(ch.contains(&Change::Restored { at: DEV, offset: 4, was: 0, now: CMD_MEM as u32 }), "{ch:?}");
    assert_eq!(cmd(&w, DEV), CMD_MEM);
    w.fns.lock().unwrap().get_mut(&DEV).unwrap().cmd(0);
    enable::restore(&w, &saved, false, true).unwrap();
    assert_eq!(cmd(&w, DEV), CMD_MEM | CMD_MASTER);
}

#[test]
fn pcie_control_words_are_saved_and_restored() {
    let mut d = device();
    d.pcie(0x90, 0, 0).w16(0x90 + 0x08, 0x2830).w16(0x90 + 0x10, 0x0042);
    d.cfg[0x71] = 0x90;
    let w = chain(d);
    let nodes = build(&w, &ALL, no_keys);
    let saved = run(&w, &nodes, DEV, true, false).0.saved.unwrap();
    assert_eq!(saved.pcie, Some(enable::PcieCtl { cap: 0x90, dev_ctl: 0x2830, link_ctl: Some(0x0042), slot_ctl: None, root_ctl: None, dev_ctl2: Some(0), link_ctl2: Some(0) }));
    // A reset put max payload, relaxed ordering and ASPM back to their defaults.
    {
        let mut fns = w.fns.lock().unwrap();
        fns.get_mut(&DEV).unwrap().w16(0x98, 0x2810).w16(0xA0, 0);
    }
    w.writes.lock().unwrap().clear();
    let ch = enable::restore(&w, &saved, false, true).unwrap();
    assert_eq!(ch, [
        Change::Restored { at: DEV, offset: 0x98, was: 0x2810, now: 0x2830 },
        Change::Restored { at: DEV, offset: 0xA0, was: 0, now: 0x42 },
    ]);
    assert_eq!(crate::access::read16(&w, DEV, 0x98), 0x2830);
    assert_eq!(crate::access::read16(&w, DEV, 0xA0), 0x42);
    // Retrain Link is never written back as 1, and Link Status (write-1-to-clear) is written 0.
    assert!(w.writes.lock().unwrap().iter().filter(|x| x.1 == 0xA0).all(|x| x.2 & (1 << 5 | 0xFFFF_0000) == 0));
    let again = enable::restore(&w, &saved, false, true).unwrap();
    assert!(again.is_empty(), "{again:?}");
}

#[test]
fn quiet_access_does_not_wait_for_a_held_cfg_lock() {
    // A CPU stopped inside a configuration access holds `pci.cfg`: the quiesce path must still finish.
    let g = crate::CFG_LOCK.lock();
    let q = crate::device::Quiet::new();
    let d = enable::disable(&q, Loc::new(0, 0, 1, 0), 0x50, 0x70);
    drop(g);
    // On the host 0xCF8 reads all ones: nothing was on, nothing written; the pair itself was free.
    assert_eq!(d, enable::Disabled::default());
    assert!(!q.gave_up());
}
