//! In-kernel tests (NW-2) on QEMU's `-device igb` (82576): `cargo xtask ktest --net igb nw.igc` and the
//! scenario script `host/tests/nw/20-igb.sh` (QEMU user network: 10.0.2.2 answers ARP, its DHCP server offers
//! 10.0.2.x). Without an igb card every test ends as `KTEST skip` (the gate boots with virtio-net).
//! On the Mini-PC only `nw.igc.probe` and `nw.igc.status` run (`Safe`: they only look). Everything that talks to
//! QEMU's user network (10.0.2.2), restarts a card (that cuts the MCP link for seconds) or plays firmware on the
//! PHY semaphore is `QemuOnly`.

use crate::device::{BringUpError, Card, Config, State};
use crate::hw::{cmd, Hw};
use crate::phy::{self, PhyError};
use crate::regs::{self, Model};
use crate::Igc;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use devmodel::DevError;
use devmodel::Quiesce;
use ktest::{kassert, ktest, TestResult};
use net::frames;

const GUEST_IP: [u8; 4] = [10, 0, 2, 15];
const GATEWAY: [u8; 4] = [10, 0, 2, 2];
/// Generous: QEMU timing is never judged (CONTRACTS §4).
const WAIT_NS: u64 = 5_000_000_000;
/// Link after autonegotiation: QEMU ~0.5 s after the BMCR write, the I226 ~3 s (V1 waits up to 10 s).
const LINK_WAIT_NS: u64 = 12_000_000_000;
/// MACs the scenario script gives its cards, so these tests know a QMP partner is there.
const QMP_MAC: [u8; 6] = [0x52, 0x54, 0x00, 0x4e, 0x57, 0x02];
const TWO_FIRST_MAC: [u8; 6] = [0x52, 0x54, 0x00, 0x4e, 0x57, 0x10];

enum Got {
    Card(Arc<Igc>),
    Skip(&'static str),
}

/// A card, running; with `need_link` the first one that has link (on the Mini-PC the first I226 has no cable).
/// Cards an earlier test left stopped are restarted.
fn card(need_link: bool) -> Result<Got, String> {
    let list = crate::devices();
    if list.is_empty() {
        return if pci::devices().iter().any(crate::is_igc) {
            Err("an igb/I226 card is present but the driver kept none (see igc.probe_failed / igc.refused)".into())
        } else {
            Ok(Got::Skip("no igb/I226 card in this machine"))
        };
    }
    for d in &list {
        if d.state() != State::Running {
            d.restart("ktest").map_err(|e| e.text())?;
        }
    }
    if !need_link {
        return Ok(Got::Card(list[0].clone()));
    }
    let t0 = time::now_ns();
    loop {
        if let Some(d) = list.iter().find(|d| d.link().up) {
            return Ok(Got::Card(d.clone()));
        }
        if time::now_ns() - t0 > LINK_WAIT_NS {
            let st: alloc::vec::Vec<String> = list.iter().map(|d| format!("{} STATUS {:#x}", d.name(), d.hw().rd(regs::STATUS))).collect();
            return Err(format!("no card got link within {} s ({})", LINK_WAIT_NS / 1_000_000_000, st.join(", ")));
        }
        list.iter().for_each(|d| d.poll());
        hal::cpu::spin_hint();
    }
}

macro_rules! card {
    ($need_link:expr) => {
        match card($need_link)? {
            Got::Card(d) => d,
            Got::Skip(why) => return ktest::skip(why),
        }
    };
}

fn wait_link(d: &Igc, up: bool, limit_ns: u64) -> bool {
    let t0 = time::now_ns();
    while d.link().up != up {
        if time::now_ns() - t0 > limit_ns {
            return false;
        }
        d.poll();
        hal::cpu::spin_hint();
    }
    true
}

/// Receives until `want` accepts a frame or `WAIT_NS` passes; `resend` runs at the start and every `every_ns`.
fn exchange(d: &Igc, every_ns: u64, mut resend: impl FnMut() -> Result<(), DevError>, mut want: impl FnMut(&[u8]) -> bool) -> Result<bool, String> {
    let start = time::now_ns();
    let mut next = start;
    let mut buf = alloc::vec![0u8; 2048];
    while time::now_ns() - start < WAIT_NS {
        if time::now_ns() >= next {
            match resend() {
                Ok(()) | Err(DevError::Timeout) => {}
                Err(e) => return Err(format!("send failed: {e:?}")),
            }
            next = time::now_ns() + every_ns;
        }
        while let Some(n) = d.receive(&mut buf) {
            if want(&buf[..n]) {
                return Ok(true);
            }
        }
        d.poll();
        hal::cpu::spin_hint();
    }
    Ok(false)
}

fn arp_gateway(d: &Igc, emergency: bool) -> Result<[u8; 6], String> {
    let mac = d.mac_bytes();
    let req = frames::arp_request(mac, GUEST_IP, GATEWAY);
    let mut gw = None;
    let got = exchange(d, 500_000_000, || {
        if emergency {
            if d.poll_tx(&req) { Ok(()) } else { Err(DevError::Io) }
        } else {
            d.transmit(&req)
        }
    }, |f| {
        gw = frames::parse_arp_reply(f, mac, GATEWAY);
        gw.is_some()
    })?;
    if !got {
        return Err("no ARP reply from 10.0.2.2 within 5 s".into());
    }
    gw.ok_or_else(|| "ARP reply without MAC".into())
}

fn probe() -> TestResult {
    let d = card!(true);
    let mac = d.mac_bytes();
    kassert!(mac != [0; 6] && mac[0] & 1 == 0, "MAC {:x?} must be unicast and non-zero", mac);
    let hw = d.hw();
    kassert!(hw.pci_command() & cmd::BUS_MASTER != 0, "bus master on while running");
    kassert!(hw.pci_command() & cmd::INTX_DISABLE != 0, "INTx off");
    kassert!(hw.rd(regs::CTRL_EXT) & regs::CTRL_EXT_DRV_LOAD != 0, "DRV_LOAD set");
    kassert!(hw.rd(regs::RCTL) == regs::RCTL_ON, "RCTL {:#x}", hw.rd(regs::RCTL));
    kassert!(hw.rd(regs::TCTL) & regs::TCTL_EN != 0);
    let (rxs, txs, _, cap) = d.ring_info();
    kassert!(hw.rd(regs::RDLEN) as usize == rxs * regs::DESC_SIZE && hw.rd(regs::TDLEN) as usize == txs * regs::DESC_SIZE);
    kassert!(cap == txs - 1);
    match d.irq_setup() {
        "polled" => {
            kassert!(hw.rd(d.model().ims()) == 0 && hw.rd(regs::EIMS) == 0, "causes masked while polled");
            kassert!(hw.msi_enabled() == (false, false), "MSI/MSI-X off while polled");
        }
        "msix" => kassert!(hw.msi_enabled() == (false, true), "MSI-X on, MSI off: {:?}", hw.msi_enabled()),
        _ => kassert!(hw.msi_enabled() == (true, false), "MSI on, MSI-X off: {:?}", hw.msi_enabled()),
    }
    let l = d.link();
    kassert!(l.up && l.speed_mbps >= 1000, "link {:?}", l);
    kassert!(net::netdev::by_name(d.name()).is_some(), "registered with the net device list");
    kassert!(hal::vectors::pollers().any(|p| p == "igc"), "poller registered");
    Ok(())
}
ktest!("nw.igc.probe", Safe, probe);

/// NW-4: the card runs on MSI-X (two vectors: link causes on 0, queue 0 on 1) through PC-3; INTx stays off; the
/// stack is told the card is interrupt-driven; a ping to the leased router raises the queue vector's count and
/// is answered; the watchdog's software interrupt (EICS, once a second) raises the link vector's count. Only
/// looks and pings the router: Safe on the Mini-PC (the I226 behind 03:00.0).
fn irq() -> TestResult {
    let d = card!(true);
    if d.irq_setup() == "polled" {
        if net::config("net_irq").is_some_and(|v| v.trim() == "off") {
            return ktest::skip("net_irq=off: polled by request");
        }
        return Err(format!("{} runs polled: no MSI-X or MSI (see igc.irq_fallback / pci msi.refused)", d.name()));
    }
    let hw = d.hw();
    let (mode, stats, released) = hw.irq_stats().ok_or("no interrupt set kept")?;
    kassert!(!released, "set released while the card runs");
    kassert!(hw.pci_command() & cmd::INTX_DISABLE != 0, "INTx off");
    let q = if mode == pci::IrqMode::MsiX {
        kassert!(hw.msi_enabled() == (false, true), "MSI-X enabled, MSI off: {:?}", hw.msi_enabled());
        kassert!(stats.len() == 2, "two vectors, got {}", stats.len());
        kassert!(hw.rd(regs::GPIE) & regs::GPIE_MSIX_MODE != 0, "GPIE.MSIX_MODE");
        kassert!(hw.rd(regs::IVAR0) == 0x8181 && hw.rd(regs::IVAR_MISC) == 0x8000, "IVAR0 {:#x} IVAR_MISC {:#x}",
            hw.rd(regs::IVAR0), hw.rd(regs::IVAR_MISC));
        1
    } else {
        kassert!(hw.msi_enabled() == (true, false), "MSI enabled: {:?}", hw.msi_enabled());
        0
    };
    kassert!(stats.iter().all(|x| x.vector.is_some() && !x.stormed), "every index has a vector, none stormed: {stats:?}");
    d.report_irq();
    kassert!(net::netdev::is_irq_driven(d.name()), "the stack was told the card is interrupt-driven");
    let st = net::stack::kernel().ok_or("no kernel stack")?;
    st.wait_configured(30_000_000_000);
    let Some(router) = st.interfaces().iter().find(|i| i.name == d.name()).and_then(|i| i.info.router) else {
        return ktest::skip("the card has no lease with a router yet");
    };
    let before = hw.irq_stats().map(|x| x.1).unwrap_or_default();
    let mut rtts = alloc::vec::Vec::new();
    for _ in 0..5 {
        rtts.push(st.ping(router, 2_000_000_000).map_err(|e| format!("ping {}: {e:?}", net::stack::ip_str(router)))?);
    }
    // The watchdog's EICS comes once a second from the poller.
    sched::sleep_ns(1_500_000_000);
    let after = hw.irq_stats().map(|x| x.1).unwrap_or_default();
    let count = |v: &[pci::IrqStats], i: usize| v.get(i).map_or(0, |x| x.deliveries);
    klog::event!(Info, "net", "ktest.igc_irq", ifname = d.name(), mode = d.irq_setup(), queue_before = count(&before, q),
        queue_after = count(&after, q), other_before = count(&before, 0), other_after = count(&after, 0),
        rtt_us = rtts.iter().map(|r| r / 1000).max().unwrap_or(0), all_cards_irq = net::netdev::all_irq_driven());
    kassert!(count(&after, q) > count(&before, q), "queue vector count did not rise with 5 pings");
    if q != 0 {
        kassert!(count(&after, 0) > count(&before, 0), "link vector count did not rise with the watchdog's EICS");
    }
    Ok(())
}
ktest!("nw.igc.irq", Safe, irq);

/// Every card's state, link, STATUS and counters as `ktest.igc_status` events (the Mini-PC check list reads
/// them: link 1000 full duplex, STATUS 0x280683, frames counted, 0 missed). Passes when every card runs and at
/// least one has link.
fn status() -> TestResult {
    let d = card!(true);
    for c in crate::devices() {
        let (l, s, h) = (c.link(), c.stats(), c.hw_stats());
        klog::event!(Info, "net", "ktest.igc_status", ifname = c.name(), model = c.model().name(), state = c.state().as_str(),
            link = l.up, speed_mbps = l.speed_mbps, full_duplex = l.full_duplex, status = klog::Hex(c.hw().rd(regs::STATUS) as u64),
            rx_frames = s.rx_frames, tx_frames = s.tx_frames, rx_dropped = s.rx_dropped, hw_missed = h.missed,
            hw_no_buffer = h.no_buffer, hw_crc_errors = h.crc_errors);
        kassert!(c.state() == State::Running, "{} is {}", c.name(), c.state().as_str());
    }
    kassert!(d.link().up);
    Ok(())
}
ktest!("nw.igc.status", Safe, status);

fn arp() -> TestResult {
    // NW-3: the stack keeps away from the cards while this test sends and receives raw frames.
    let _cards = net::stack::hold_cards();
    let d = card!(true);
    let before = d.stats();
    let gw = arp_gateway(&d, false)?;
    kassert!(gw != [0; 6] && gw != d.mac_bytes(), "gateway MAC {:x?}", gw);
    let after = d.stats();
    kassert!(after.tx_frames > before.tx_frames && after.rx_frames > before.rx_frames, "stats moved");
    kassert!(klog::metrics::snapshot().any(|(n, v)| n == "net.igc.rx_frames" && v > 0), "rx counter");
    Ok(())
}
ktest!("nw.igc.arp", QemuOnly, arp);

fn dhcp() -> TestResult {
    // NW-3: the stack keeps away from the cards while this test sends and receives raw frames.
    let _cards = net::stack::hold_cards();
    let d = card!(true);
    let mac = d.mac_bytes();
    let xid = (time::now_ns() as u32) ^ 0x5EED_0002;
    let disc = frames::dhcp_discover(mac, xid);
    let mut offer = None;
    let mut tries = 0u32;
    let got = exchange(&d, 1_000_000_000, || {
        tries += 1;
        d.transmit(&disc)
    }, |f| {
        offer = frames::parse_dhcp_reply(f, xid);
        offer.is_some()
    })?;
    kassert!(got, "no DHCP OFFER within 5 s ({} DISCOVERs sent)", tries);
    let o = offer.ok_or("no offer")?;
    kassert!(o.kind == 2, "message type {} (want 2 = OFFER)", o.kind);
    klog::event!(Info, "net", "ktest.dhcp_offer", ifname = d.name(),
        ip = format!("{}.{}.{}.{}", o.your_ip[0], o.your_ip[1], o.your_ip[2], o.your_ip[3]));
    Ok(())
}
ktest!("nw.igc.dhcp", QemuOnly, dhcp);

fn poll_tx() -> TestResult {
    // NW-3: the stack keeps away from the cards while this test sends and receives raw frames.
    let _cards = net::stack::hold_cards();
    let d = card!(true);
    arp_gateway(&d, true)?;
    let dropped = || klog::metrics::snapshot().find(|(n, _)| *n == "net.igc.poll_tx_dropped").map_or(0, |c| c.1);
    let before = dropped();
    let req = frames::arp_request(d.mac_bytes(), GUEST_IP, GATEWAY);
    kassert!(!d.with_tx_locked(|| d.poll_tx(&req)), "poll_tx must not wait for a held TX lock");
    kassert!(dropped() == before + 1, "drop counted");
    kassert!(d.poll_tx(&req), "works again once the lock is free");
    Ok(())
}
ktest!("nw.igc.poll_tx", QemuOnly, poll_tx);

/// Thousands of frames of every size through the ring (it wraps many times; ring-full answers are retried),
/// a 14-byte frame among them (padded to 17), and every descriptor comes back.
fn burst() -> TestResult {
    // NW-3: the stack keeps away from the cards while this test sends and receives raw frames.
    let _cards = net::stack::hold_cards();
    let d = card!(true);
    let mac = d.mac_bytes();
    let before = d.stats().tx_frames;
    let n = 3000u32;
    let start = time::now_ns();
    let mut full = 0u32;
    for i in 0..n {
        let f = if i == 7 { frames::junk(mac, 60, i)[..14].to_vec() } else { frames::junk(mac, 60 + (i as usize * 37) % 1455, i) };
        loop {
            match d.transmit(&f) {
                Ok(()) => break,
                Err(DevError::Timeout) => {
                    full += 1;
                    d.poll();
                    kassert!(time::now_ns() - start < 6 * WAIT_NS, "TX ring never drained ({} sent)", i);
                }
                Err(e) => return Err(format!("frame {i}: {e:?}")),
            }
        }
    }
    kassert!(d.stats().tx_frames - before == n as u64);
    let t0 = time::now_ns();
    loop {
        d.poll();
        let _ = d.transmit(&frames::junk(mac, 60, n));
        let (_, _, held, _) = d.ring_info();
        if held <= 1 {
            break;
        }
        kassert!(time::now_ns() - t0 < WAIT_NS, "TX descriptors not written back: {} held", held);
    }
    kassert!(d.state() == State::Running);
    klog::event!(Info, "net", "ktest.burst", ifname = d.name(), frames = n, ring_full_retries = full);
    Ok(())
}
ktest!("nw.igc.burst", QemuOnly, burst);

fn robust() -> TestResult {
    // NW-3: the stack keeps away from the cards while this test sends and receives raw frames.
    let _cards = net::stack::hold_cards();
    let d = card!(true);
    kassert!(d.transmit(&[]) == Err(DevError::InvalidArgument));
    kassert!(d.transmit(&[0u8; 13]) == Err(DevError::InvalidArgument));
    kassert!(d.transmit(&alloc::vec![0u8; 1519]) == Err(DevError::InvalidArgument));
    kassert!(!d.poll_tx(&[]), "empty emergency frame refused");
    let mac = d.mac_bytes();
    let dropped = d.stats().rx_dropped;
    let req = frames::arp_request(mac, GUEST_IP, GATEWAY);
    let start = time::now_ns();
    let mut tiny = [0u8; 10];
    let mut next = start;
    while d.stats().rx_dropped == dropped {
        if time::now_ns() >= next {
            d.transmit(&req).map_err(|e| format!("{e:?}"))?;
            next = time::now_ns() + 500_000_000;
        }
        kassert!(d.receive(&mut tiny).is_none(), "nothing fits 10 bytes");
        d.poll();
        kassert!(time::now_ns() - start < WAIT_NS, "no frame arrived to drop");
    }
    kassert!(d.state() == State::Running);
    arp_gateway(&d, false)?;
    Ok(())
}
ktest!("nw.igc.robust", QemuOnly, robust);

fn quiesce() -> TestResult {
    // NW-3: the stack keeps away from the cards while this test sends and receives raw frames.
    let _cards = net::stack::hold_cards();
    let d = card!(true);
    let seq = klog::next_seq();
    Quiesce::quiesce(&*d);
    let hw = d.hw();
    kassert!(d.state() == State::Quiesced);
    kassert!(hw.pci_command() & cmd::BUS_MASTER == 0, "bus master cleared");
    kassert!(hw.rd(regs::CTRL_EXT) & regs::CTRL_EXT_DRV_LOAD == 0, "DRV_LOAD cleared");
    kassert!(hw.rd(regs::RCTL) & 2 == 0 && hw.rd(regs::TCTL) & regs::TCTL_EN == 0, "RX/TX off");
    kassert!(hw.msi_enabled() == (false, false), "MSI off");
    kassert!(!d.link().up, "no link while quiesced");
    let req = frames::arp_request(d.mac_bytes(), GUEST_IP, GATEWAY);
    kassert!(d.transmit(&req).is_err() && !d.poll_tx(&req), "nothing sent while quiesced");
    let mut buf = [0u8; 2048];
    kassert!(d.receive(&mut buf).is_none());
    d.fail_for_test();
    d.poll();
    kassert!(d.state() == State::Quiesced, "neither a failure nor the poller restarts a quiesced card");
    kassert!(klog::read_since(seq, 64).iter().any(|r| r.name == "igc.quiesce"), "quiesce event");
    d.restart("ktest after quiesce").map_err(|e| e.text())?;
    kassert!(wait_link(&d, true, LINK_WAIT_NS), "link back after the restart");
    arp_gateway(&d, false)?;
    Ok(())
}
ktest!("nw.igc.quiesce", QemuOnly, quiesce);

/// The restart path as the running kernel takes it: a failure, then nothing but sleeping here. The poller
/// (idle thread) queues the retry, the `igc.restart` worker runs it with sleeping waits; the card comes back
/// and carries traffic.
fn worker_restart() -> TestResult {
    // NW-3: the stack keeps away from the cards while this test sends and receives raw frames.
    let _cards = net::stack::hold_cards();
    let d = card!(true);
    let seq = klog::next_seq();
    d.fail_for_test();
    // SC-3: the poll-duty CPU and the worker run on other CPUs now and may have picked the failure up already.
    let st = d.state();
    kassert!(st != State::Running || klog::read_since(seq, 256).iter().any(|r| r.name == "igc.restart"), "failed card counts as running ({})", st.as_str());
    let t0 = time::now_ns();
    while d.state() != State::Running {
        kassert!(time::now_ns() - t0 < WAIT_NS, "the worker never restarted the card (state {})", d.state().as_str());
        sched::sleep_ms(10);
    }
    let ev = klog::read_since(seq, 256);
    let queued = ev.iter().any(|r| r.name == "igc.restart_queued");
    let restarted = ev.iter().any(|r| r.name == "igc.restart" && r.text.contains("retry after a failure"));
    kassert!(queued && restarted, "queued by the poller, run by the worker (queued {}, restart {})", queued, restarted);
    let t0 = time::now_ns();
    while !d.link().up {
        kassert!(time::now_ns() - t0 < LINK_WAIT_NS, "no link after the worker's restart");
        sched::sleep_ms(10);
    }
    arp_gateway(&d, false)?;
    Ok(())
}
ktest!("nw.igc.worker_restart", QemuOnly, worker_restart);

/// nic-test boot 1: a firmware (or a previous kernel) left bus master, receive, transmit and DRV_LOAD on with
/// rings pointing at memory that is not ours. The bring-up stops all of it before anything else and comes up.
fn firmware_left_on() -> TestResult {
    // NW-3: the stack keeps away from the cards while this test sends and receives raw frames.
    let _cards = net::stack::hold_cards();
    let d = card!(true);
    Quiesce::quiesce(&*d);
    let hw = d.hw();
    hw.wr(regs::RDBAL, 0x0010_0000);
    hw.wr(regs::TDBAL, 0x0010_1000);
    hw.set_bits(regs::CTRL_EXT, regs::CTRL_EXT_DRV_LOAD);
    hw.wr(regs::RCTL, regs::RCTL_ON);
    hw.set_bits(regs::TCTL, regs::TCTL_EN);
    hw.set_pci_command(hw.pci_command() | cmd::BUS_MASTER);
    d.restart("ktest: firmware left the card running").map_err(|e| e.text())?;
    kassert!(hw.rd(regs::RDBAL) != 0x0010_0000 && hw.rd(regs::TDBAL) != 0x0010_1000, "rings are ours");
    kassert!(wait_link(&d, true, LINK_WAIT_NS));
    arp_gateway(&d, false)?;
    Ok(())
}
ktest!("nw.igc.firmware_left_on", QemuOnly, firmware_left_on);

/// V1 `nicpci.asm`: a link retrain left BAR0/BAR1 at 0 on the running I226. NW-3b's bring-up writes the header
/// back through PC-2's `restore_config` (from the copy the last `enable_device` saved) before memory decoding
/// goes on, and the card comes back and carries traffic.
fn bars_lost() -> TestResult {
    let _cards = net::stack::hold_cards();
    let d = card!(true);
    Quiesce::quiesce(&*d);
    let hw = d.hw();
    let f = hw.pci();
    let (bar0, bar1) = (f.read32(0x10), f.read32(0x14));
    let seq = klog::next_seq();
    f.set_command(f.command() & !cmd::MEMORY);
    f.write32(0x10, 0);
    if (bar0 >> 1) & 3 == 2 {
        f.write32(0x14, 0);
    }
    kassert!(f.read32(0x10) & !0xF == 0, "BAR0 zeroed for the test");
    d.restart("ktest: BARs lost").map_err(|e| e.text())?;
    kassert!(f.read32(0x10) == bar0 && f.read32(0x14) == bar1, "BARs written back: {:#x} {:#x}", f.read32(0x10), f.read32(0x14));
    kassert!(f.command() & cmd::MEMORY != 0, "memory decoding on again");
    let ev = klog::read_since(seq, 256);
    kassert!(ev.iter().any(|r| r.name == "igc.config_restored"), "igc.config_restored logged");
    kassert!(wait_link(&d, true, LINK_WAIT_NS), "link after the restore");
    arp_gateway(&d, false)?;
    Ok(())
}
ktest!("nw.igc.bars_lost", QemuOnly, bars_lost);

/// igb-test: the I226's PHY semaphore paths on QEMU's SWSM/SW_FW_SYNC, which behave as the I226's (V1 measured
/// it): free, held by the "firmware" (bit 17 set by us), SMBI left set by a read, our own bit left set.
fn semaphore() -> TestResult {
    // NW-3: the stack keeps away from the cards while this test sends and receives raw frames.
    let _cards = net::stack::hold_cards();
    let d = card!(false);
    let hw = d.hw();
    let ms = |t0: u64| (time::now_ns() - t0) / 1_000_000;
    let clean = || {
        hw.wr(regs::SW_FW_SYNC, 0);
        hw.wr(regs::SWSM, 0);
    };
    clean();
    let t0 = time::now_ns();
    let a = phy::acquire(hw).map_err(|e| e.as_str())?;
    kassert!(a == phy::Acquired::default() && ms(t0) < 500, "free: taken at once ({} ms, {:?})", ms(t0), a);
    kassert!(hw.rd(regs::SW_FW_SYNC) == regs::SWFW_PHY0_SW, "PHY0 ours");
    phy::release(hw).map_err(|e| e.as_str())?;
    kassert!(hw.rd(regs::SW_FW_SYNC) == 0, "given back");
    hw.wr(regs::SW_FW_SYNC, regs::SWFW_PHY0_FW);
    let t0 = time::now_ns();
    let r = phy::acquire(hw);
    let held = ms(t0);
    let sync = hw.rd(regs::SW_FW_SYNC);
    clean();
    kassert!(r == Err(PhyError::FirmwareHoldsPhy) && (1000..3000).contains(&held), "held by the firmware: {:?} after {} ms", r, held);
    kassert!(sync == regs::SWFW_PHY0_FW, "its bit untouched: {:#x}", sync);
    let _ = hw.rd(regs::SWSM); // a read sets SMBI and nobody clears it
    let t0 = time::now_ns();
    let a = phy::acquire(hw).map_err(|e| e.as_str())?;
    let t = ms(t0);
    phy::release(hw).map_err(|e| e.as_str())?;
    kassert!(a.smbi_cleared && (1000..3000).contains(&t), "SMBI left set: cleared once after a second ({} ms)", t);
    hw.wr(regs::SW_FW_SYNC, regs::SWFW_PHY0_SW);
    let t0 = time::now_ns();
    let a = phy::acquire(hw).map_err(|e| e.as_str())?;
    phy::release(hw).map_err(|e| e.as_str())?;
    kassert!(a.took_over_own_bit && ms(t0) < 500, "own stale bit taken over at once");
    clean();
    Ok(())
}
ktest!("nw.igc.semaphore", QemuOnly, semaphore);

/// igb-test: the whole bring-up as an I226 on QEMU's 82576. Its PHY address 0 is an MDIC error there, so it
/// must fail, after running the I226-only steps (packet buffer values, the erratum bit), and leave nothing
/// claimed. Then the real card comes back.
fn i226_path() -> TestResult {
    // NW-3: the stack keeps away from the cards while this test sends and receives raw frames.
    let _cards = net::stack::hold_cards();
    let d = card!(false);
    if d.model() != Model::I82576 {
        return ktest::skip("runs the I226 steps on QEMU's 82576 only");
    }
    Quiesce::quiesce(&*d);
    let hw = crate::MmioHw::map(d.hw().pci())?;
    let t = Card::new(String::from("igc-i226-test"), Model::I226, hw, Config { rx_ring: 8, tx_ring: 8, irqs: false, ..Config::default() })?;
    let r = t.start();
    let h = t.hw();
    kassert!(r == Err(BringUpError::Phy(PhyError::MdicError)), "PHY at address 0 must fail on the 82576: {:?}", r);
    // RXPBS/TXPBS survive the failure path's reset in QEMU (RFCTL does not, so it is checked by the host tests).
    kassert!(h.rd(regs::RXPBS) == regs::RXPBS_I226 && h.rd(regs::TXPBS) == regs::TXPBS_I226, "I226 buffer values written");
    kassert!(h.pci_command() & cmd::BUS_MASTER == 0, "bus master off after the failure");
    kassert!(h.rd(regs::CTRL_EXT) & regs::CTRL_EXT_DRV_LOAD == 0, "no DRV_LOAD");
    kassert!(h.rd(regs::SW_FW_SYNC) & regs::SWFW_PHY0_SW == 0 && h.rd(regs::SWSM) & 2 == 0, "semaphore given back");
    Quiesce::quiesce(&t);
    d.restart("ktest after the I226 path").map_err(|e| e.text())?;
    kassert!(wait_link(&d, true, LINK_WAIT_NS));
    arp_gateway(&d, false)?;
    Ok(())
}
ktest!("nw.igc.i226_path", QemuOnly, i226_path);

/// Cable pull through QMP (`20-igb.sh`): `ktest.link_wait` -> `set_link off`, `ktest.link_down_seen` ->
/// `set_link on`. Skips unless the card has the scenario's MAC.
fn link_qmp() -> TestResult {
    // NW-3: the stack keeps away from the cards while this test sends and receives raw frames.
    let _cards = net::stack::hold_cards();
    let d = card!(true);
    if d.mac_bytes() != QMP_MAC {
        return ktest::skip("not the QMP scenario card (host/tests/nw/20-igb.sh)");
    }
    let seq = klog::next_seq();
    klog::event!(Info, "net", "ktest.link_wait"; "waiting for QMP set_link off");
    kassert!(wait_link(&d, false, 60_000_000_000), "link never went down");
    let req = frames::arp_request(d.mac_bytes(), GUEST_IP, GATEWAY);
    kassert!(d.transmit(&req) == Err(DevError::Io), "no frames to a card without link");
    klog::event!(Info, "net", "ktest.link_down_seen");
    kassert!(wait_link(&d, true, 60_000_000_000), "link never came back");
    arp_gateway(&d, false)?;
    let ev = klog::read_since(seq, 256);
    kassert!(ev.iter().any(|r| r.name == "link.down") && ev.iter().any(|r| r.name == "link.up"), "link events");
    Ok(())
}
ktest!("nw.igc.link_qmp", QemuOnly, link_qmp);

/// nic-test boots 1/7: two cards, the first without link (the script switches its link off through QMP before
/// the kernel starts). Both run; the second carries traffic; the first refuses frames, then gets its link when
/// QMP plugs it (`ktest.two_wait` -> `set_link on`).
fn two_cards() -> TestResult {
    // NW-3: the stack keeps away from the cards while this test sends and receives raw frames.
    let _cards = net::stack::hold_cards();
    let list = crate::devices();
    let Some(first) = list.first().filter(|c| c.mac_bytes() == TWO_FIRST_MAC).cloned() else {
        return ktest::skip("not the two-card scenario (host/tests/nw/20-igb.sh)");
    };
    kassert!(list.len() == 2, "{} cards", list.len());
    let second = list[1].clone();
    for c in &list {
        if c.state() != State::Running {
            c.restart("ktest").map_err(|e| e.text())?;
        }
    }
    kassert!(wait_link(&second, true, LINK_WAIT_NS), "second card has link");
    // Give the first one the same chance: it must stay without link.
    let t0 = time::now_ns();
    while time::now_ns() - t0 < 1_500_000_000 {
        first.poll();
        hal::cpu::spin_hint();
    }
    kassert!(!first.link().up && first.state() == State::Running, "first card runs without link");
    let req = frames::arp_request(first.mac_bytes(), GUEST_IP, GATEWAY);
    kassert!(first.transmit(&req) == Err(DevError::Io), "no frame to the card without link");
    arp_gateway(&second, false)?;
    klog::event!(Info, "net", "ktest.two_wait"; "waiting for QMP set_link on (first card)");
    kassert!(wait_link(&first, true, 60_000_000_000), "first card's link never came");
    arp_gateway(&first, false)?;
    Ok(())
}
ktest!("nw.igc.two_cards", QemuOnly, two_cards);
