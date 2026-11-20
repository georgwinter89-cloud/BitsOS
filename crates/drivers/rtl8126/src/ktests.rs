//! In-kernel tests for the hardware day (NIC-11), run by the desk over MCP: `ktest gp.rtl.`. Each one is a statement
//! about every RTL8126 in the machine. A machine without one has no card to make it about: there each test checks
//! that the driver holds nothing and says so in its `ktest.rtl8126_absent` event. With a card in the machine a
//! missing or stopped driver is a failure, with the self-check's lines in the message. GP-ROUND: the five hardware
//! tests are class `GamingPc` (the machine that has the card); only `gp.rtl.registry` still runs everywhere.
//!
//! None of them restarts or stops the card: the desk reaches the machine through it.

use crate::device::State;
use crate::machine::Machine;
use crate::stage::{Stage, Verdict};
use crate::Rtl;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use devmodel::DevError;
use ktest::{kassert, ktest, TestResult};
use net::frames;

/// The first bring-up may wait 30 s for the firmware file and then takes about a second.
const UP_WAIT_NS: u64 = 60_000_000_000;
/// Autonegotiation: 3 to 5 s for the multi-gigabit modes.
const LINK_WAIT_NS: u64 = 15_000_000_000;
const LEASE_WAIT_NS: u64 = 30_000_000_000;
const ARP_WAIT_NS: u64 = 5_000_000_000;

fn lines(d: &Rtl) -> String {
    d.report().lines().join(" | ")
}

/// The machine's card, or `None` where there is none to test.
fn card() -> Result<Option<Arc<Rtl>>, String> {
    let present: Vec<pci::PciFunction> = pci::devices().into_iter().filter(crate::is_rtl8126).collect();
    let kept = crate::devices();
    let Some(f) = present.first() else {
        kassert!(kept.is_empty(), "the driver holds {} card(s) though no 10ec:8126 function exists", kept.len());
        klog::event!(Info, "net", "ktest.rtl8126_absent"; "no RTL8126 in this machine: the driver holds nothing");
        return Ok(None);
    };
    if let Some(key) = f.untouched() {
        return Err(format!("the card at {} is left untouched by CONFIG {key}: this boot has no driver on it", f.addr));
    }
    match kept.first() {
        Some(d) => Ok(Some(d.clone())),
        None => Err(format!("an RTL8126 is at {} but the driver kept none (see rtl8126.probe_failed)", f.addr)),
    }
}

fn wait(limit_ns: u64, mut done: impl FnMut() -> bool) -> bool {
    let t0 = time::now_ns();
    while !done() {
        if time::now_ns() - t0 > limit_ns {
            return false;
        }
        sched::sleep_ns(50_000_000);
    }
    true
}

fn running(d: &Rtl) -> TestResult {
    wait(UP_WAIT_NS, || !matches!(d.state(), State::Down));
    kassert!(d.state() == State::Running, "{} is {}: {}", d.name(), d.state().as_str(), lines(d));
    Ok(())
}

/// The driver is registered once, for PCI id 10ec:8126 and nothing else; and where no
/// such function exists it holds no card and runs no poller. Runs everywhere: on the Mini-PC and under QEMU it is the
/// proof that linking the driver changed nothing there.
fn registry() -> TestResult {
    let mine: Vec<pci::registry::Candidate> = pci::registry::registered().into_iter().filter(|c| c.name == "rtl8126").collect();
    kassert!(mine.len() == 1, "{} drivers named rtl8126", mine.len());
    let d = &mine[0];
    let hits = |vendor: u16, device: u16, class: u32| d.matches.iter().any(|m| m.hits(vendor, device, class));
    kassert!(hits(0x10ec, 0x8126, 0x02_00_00), "the Gaming-PC's card");
    // The Mini-PC's cards and QEMU's, other Realtek chips of the same Linux driver, and any class match.
    for (vendor, device) in [(0x8086, 0x125c), (0x10ec, 0xb852), (0x8086, 0x10c9), (0x1af4, 0x1000), (0x1af4, 0x1041), (0x10ec, 0x8125), (0x10ec, 0x8127), (0x10ec, 0x8168), (0x10ec, 0x5000)] {
        kassert!(!hits(vendor, device, 0x02_00_00), "{vendor:04x}:{device:04x} must not bind");
    }
    if !pci::devices().iter().any(crate::is_rtl8126) {
        kassert!(crate::devices().is_empty(), "no card without a 10ec:8126 function");
        kassert!(!hal::vectors::pollers().any(|p| p == "rtl8126"), "no poller without a card");
        kassert!(crate::report_lines().is_empty(), "nothing to report");
    }
    Ok(())
}
ktest!("gp.rtl.registry", Safe, registry);

/// The card is held, identified as an RTL8126A (XID 64a on the Gaming-PC), has its own MAC, runs with bus master on
/// and is in the stack's list; the first three stages of the self-check passed.
fn probe() -> TestResult {
    let Some(d) = card()? else { return Ok(()) };
    running(&d)?;
    let (xid, mac) = (d.xid(), d.mac_bytes());
    kassert!(matches!(xid & 0x7cf, 0x64a | 0x649), "XID {:#x} is no RTL8126A", xid);
    kassert!(mac != [0; 6] && mac[0] & 1 == 0, "the MAC must be unicast and non-zero");
    kassert!(d.hw().bus_master(), "bus master on while running");
    for s in [Stage::Probe, Stage::Phy] {
        kassert!(d.report().get(s).is_some_and(|l| l.verdict == Verdict::Ok), "stage {}: {}", s.name(), lines(&d));
    }
    let fw = d.report().get(Stage::Firmware).ok_or("no firmware line")?;
    kassert!(net::netdev::by_name(d.name()).is_some(), "registered with the net device list");
    kassert!(hal::vectors::pollers().any(|p| p == "rtl8126"), "poller registered");
    let (max_wait_us, slow_waits) = d.phy_waits();
    klog::event!(Info, "net", "ktest.rtl8126_probe", ifname = d.name(), xid = klog::Hex(xid as u64), state = d.state().as_str(),
        firmware = fw.verdict.name(), irq = d.irq_setup(), max_wait_us = max_wait_us, slow_waits = slow_waits; "{}", fw.text.as_str());
    Ok(())
}
ktest!("gp.rtl.probe", GamingPc, probe);

/// The cable has a link at a speed this chip has, full duplex.
fn link() -> TestResult {
    let Some(d) = card()? else { return Ok(()) };
    running(&d)?;
    kassert!(wait(LINK_WAIT_NS, || d.link().up), "no link within {} s: {}", LINK_WAIT_NS / 1_000_000_000, lines(&d));
    let l = d.link();
    kassert!(matches!(l.speed_mbps, 10 | 100 | 1000 | 2500 | 5000) && l.full_duplex, "link {:?}", l);
    klog::event!(Info, "net", "ktest.rtl8126_link", ifname = d.name(), speed_mbps = l.speed_mbps, full_duplex = l.full_duplex);
    Ok(())
}
ktest!("gp.rtl.link", GamingPc, link);

/// The card's lease: (own address, router).
fn lease(d: &Rtl) -> Result<([u8; 4], [u8; 4]), String> {
    let st = net::stack::kernel().ok_or("no kernel stack")?;
    let find = || st.interfaces().into_iter().find(|i| i.name == d.name()).and_then(|i| i.info.addr.map(|(a, _)| (a.0, i.info.router.map(|r| r.0))));
    wait(LEASE_WAIT_NS, || find().is_some());
    match find() {
        Some((addr, Some(router))) => Ok((addr, router)),
        Some((_, None)) => Err(format!("{} has an address but its lease names no router", d.name())),
        None => Err(format!("{} got no address within {} s: {}", d.name(), LEASE_WAIT_NS / 1_000_000_000, lines(d))),
    }
}

/// A raw ARP request to the router is answered: the rings work in both directions without the stack's help. The
/// stack keeps off the card meanwhile (at most `ARP_WAIT_NS`; the desk's own connection resumes after it).
fn arp() -> TestResult {
    let Some(d) = card()? else { return Ok(()) };
    running(&d)?;
    kassert!(wait(LINK_WAIT_NS, || d.link().up), "no link: {}", lines(&d));
    let (addr, router) = lease(&d)?;
    let mac = d.mac_bytes();
    let req = frames::arp_request(mac, addr, router);
    let before = d.stats();
    let mut answer = None;
    {
        let _hold = net::stack::hold_cards();
        let mut buf = alloc::vec![0u8; 2048];
        let (start, mut next) = (time::now_ns(), 0);
        while answer.is_none() && time::now_ns() - start < ARP_WAIT_NS {
            if time::now_ns() >= next {
                match d.transmit(&req) {
                    Ok(()) | Err(DevError::Timeout) => {}
                    Err(e) => return Err(format!("send failed: {e:?}")),
                }
                next = time::now_ns() + 500_000_000;
            }
            while let Some(n) = d.receive(&mut buf) {
                answer = answer.or(frames::parse_arp_reply(&buf[..n], mac, router));
            }
            sched::sleep_ns(1_000_000);
        }
    }
    let after = d.stats();
    let gw = answer.ok_or(format!("no ARP answer from the router within {} s (sent {}, received {})", ARP_WAIT_NS / 1_000_000_000,
        after.tx_frames - before.tx_frames, after.rx_frames - before.rx_frames))?;
    kassert!(gw != [0; 6] && gw != mac, "the router's MAC must be another, non-zero one");
    kassert!(after.tx_frames > before.tx_frames && after.rx_frames > before.rx_frames, "the counters moved");
    klog::event!(Info, "net", "ktest.rtl8126_arp", ifname = d.name(), sent = after.tx_frames - before.tx_frames,
        received = after.rx_frames - before.rx_frames);
    Ok(())
}
ktest!("gp.rtl.arp", GamingPc, arp);

/// Five echo requests to the router through the stack are all answered, and every frame sent came back finished.
fn ping() -> TestResult {
    let Some(d) = card()? else { return Ok(()) };
    running(&d)?;
    let (_, router) = lease(&d)?;
    let st = net::stack::kernel().ok_or("no kernel stack")?;
    let mut worst = 0;
    for i in 0..5 {
        let rtt = st.ping(net::Ipv4(router), 2_000_000_000).map_err(|e| format!("ping {i} to the router: {e:?}"))?;
        worst = worst.max(rtt);
    }
    kassert!(wait(1_000_000_000, || d.tx_done() == Some(true)), "frames stay with the card: {:?}", d.tx_info());
    let s = d.stats();
    klog::event!(Info, "net", "ktest.rtl8126_ping", ifname = d.name(), worst_rtt_us = worst / 1000, rx_frames = s.rx_frames,
        tx_frames = s.tx_frames, rx_dropped = s.rx_dropped, irq = d.irq_setup(), irqs = d.irq_count());
    Ok(())
}
ktest!("gp.rtl.ping", GamingPc, ping);

/// The self-check's lines, each as a `ktest.rtl8126_line` event, and none of them a failure.
fn report() -> TestResult {
    let Some(d) = card()? else { return Ok(()) };
    running(&d)?;
    for l in d.report().lines() {
        klog::event!(Info, "net", "ktest.rtl8126_line", ifname = d.name(); "{}", l.as_str());
    }
    kassert!(d.report().failed_at().is_none(), "a stage failed: {}", lines(&d));
    Ok(())
}
ktest!("gp.rtl.report", GamingPc, report);
