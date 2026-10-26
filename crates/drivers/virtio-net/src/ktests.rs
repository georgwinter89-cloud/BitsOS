//! In-kernel tests (NW-1). They need a virtio-net card: `host/tests/nw/10-virtio-net.sh` runs them under
//! `xtask qemu --net virtio` (QEMU user network: gateway 10.0.2.2 answers ARP, its DHCP server offers
//! 10.0.2.15). Without a card (the plain `xtask ktest` run) each test passes after an `ktest.no_device` event;
//! the scenario script fails if it sees that event, so the real checks cannot silently go missing.
//! `contract.quiesce` (sorted before these) quiesces every registered device, so each test first restarts
//! a stopped card.

use crate::frames;
use crate::{State, VirtioNet};
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use devmodel::net::NetDevice;
use devmodel::{DevError, Quiesce};
use ktest::{kassert, ktest, TestResult};

const GUEST_IP: [u8; 4] = [10, 0, 2, 15];
const GATEWAY: [u8; 4] = [10, 0, 2, 2];
/// Generous: QEMU timing is never judged (CONTRACTS §4).
const WAIT_NS: u64 = 5_000_000_000;

fn card(test: &str) -> Result<Option<Arc<VirtioNet>>, String> {
    let present = pci::devices().iter().any(crate::is_virtio_net);
    let d = crate::devices().into_iter().next();
    match (present, d) {
        (false, _) => {
            klog::event!(Info, "net", "ktest.no_device", test = test; "no virtio-net card in this machine");
            Ok(None)
        }
        (true, None) => Err("a virtio-net card is present but the driver did not bring it up (see virtio.probe_failed)".into()),
        (true, Some(d)) => {
            if d.state() != State::Running {
                d.restart("ktest")?;
            }
            Ok(Some(d))
        }
    }
}

/// Receives until `want` accepts a frame or `WAIT_NS` passes; `resend` runs at the start and every `every_ns`.
fn exchange(d: &VirtioNet, every_ns: u64, mut resend: impl FnMut() -> Result<(), DevError>, mut want: impl FnMut(&[u8]) -> bool) -> Result<bool, String> {
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

fn arp_gateway(d: &VirtioNet, emergency: bool) -> Result<[u8; 6], String> {
    let mac = d.mac().0;
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
    let Some(d) = card("nw.virtio.probe")? else { return Ok(()) };
    let mac = d.mac().0;
    kassert!(mac != [0; 6] && mac[0] & 1 == 0, "MAC {:x?} must be unicast and non-zero", mac);
    kassert!(d.features() & crate::caps::feat::VERSION_1 != 0);
    kassert!(d.link().up, "QEMU user net has link");
    kassert!(d.pci().command() & 0x4 != 0, "bus master on while running");
    kassert!(d.device_status() & 0x4 != 0, "DRIVER_OK set");
    let (rxs, rx_posted, txs, tx_free) = d.ring_info();
    kassert!(rxs.is_power_of_two() && txs.is_power_of_two());
    kassert!(rx_posted == rxs as usize, "all {} RX buffers with the device, {} are", rxs, rx_posted);
    kassert!(tx_free <= txs as usize);
    kassert!(net::netdev::by_name(NetDevice::name(&*d)).is_some(), "registered with the net device list");
    kassert!(hal::vectors::pollers().any(|p| p == "virtio-net"), "poller registered");
    Ok(())
}
ktest!("nw.virtio.probe", Safe, probe);

fn arp() -> TestResult {
    // NW-3: the stack keeps away from the cards while this test sends and receives raw frames.
    let _cards = net::stack::hold_cards();
    let Some(d) = card("nw.virtio.arp")? else { return Ok(()) };
    let before = d.stats();
    let gw = arp_gateway(&d, false)?;
    kassert!(gw != [0; 6] && gw != d.mac().0, "gateway MAC {:x?}", gw);
    let after = d.stats();
    kassert!(after.tx_frames > before.tx_frames && after.rx_frames > before.rx_frames, "stats moved");
    kassert!(klog::metrics::snapshot().any(|(n, v)| n == "net.virtio.rx_frames" && v > 0), "rx counter");
    Ok(())
}
ktest!("nw.virtio.arp", Safe, arp);

fn dhcp() -> TestResult {
    // NW-3: the stack keeps away from the cards while this test sends and receives raw frames.
    let _cards = net::stack::hold_cards();
    let Some(d) = card("nw.virtio.dhcp")? else { return Ok(()) };
    let mac = d.mac().0;
    let xid = (time::now_ns() as u32) ^ 0x5EED_0001;
    let disc = frames::dhcp_discover(mac, xid);
    let mut offer = None;
    let mut tries = 0u32;
    let got = exchange(&d, 1_000_000_000, || {
        tries += 1;
        if tries > 1 {
            klog::event!(Info, "net", "ktest.dhcp_retry", attempt = tries);
        }
        d.transmit(&disc)
    }, |f| {
        offer = frames::parse_dhcp_reply(f, xid);
        offer.is_some()
    })?;
    kassert!(got, "no DHCP OFFER within 5 s ({} DISCOVERs sent)", tries);
    let o = offer.ok_or("no offer")?;
    kassert!(o.kind == 2, "message type {} (want 2 = OFFER)", o.kind);
    kassert!(o.your_ip[..3] == [10, 0, 2], "offered {:?}", o.your_ip);
    klog::event!(Info, "net", "ktest.dhcp_offer", ip = format!("{}.{}.{}.{}", o.your_ip[0], o.your_ip[1], o.your_ip[2], o.your_ip[3]));
    Ok(())
}
ktest!("nw.virtio.dhcp", Safe, dhcp);

fn poll_tx() -> TestResult {
    // NW-3: the stack keeps away from the cards while this test sends and receives raw frames.
    let _cards = net::stack::hold_cards();
    let Some(d) = card("nw.virtio.poll_tx")? else { return Ok(()) };
    arp_gateway(&d, true)?;
    // Held TX lock (a panic in the middle of a transmit): the emergency path drops instead of waiting.
    let dropped = || klog::metrics::snapshot().find(|(n, _)| *n == "net.virtio.poll_tx_dropped").map_or(0, |c| c.1);
    let before = dropped();
    let req = frames::arp_request(d.mac().0, GUEST_IP, GATEWAY);
    let sent = d.with_tx_locked(|| d.poll_tx(&req));
    kassert!(!sent, "poll_tx must not wait for a held TX lock");
    kassert!(dropped() == before + 1, "drop counted");
    kassert!(d.poll_tx(&req), "works again once the lock is free");
    Ok(())
}
ktest!("nw.virtio.poll_tx", Safe, poll_tx);

fn burst() -> TestResult {
    // NW-3: the stack keeps away from the cards while this test sends and receives raw frames.
    let _cards = net::stack::hold_cards();
    let Some(d) = card("nw.virtio.burst")? else { return Ok(()) };
    let mac = d.mac().0;
    let before = d.stats().tx_frames;
    let n = 3000u32;
    let start = time::now_ns();
    let mut full = 0u32;
    for i in 0..n {
        let f = frames::junk(mac, 60 + (i as usize * 37) % 1455, i);
        loop {
            match d.transmit(&f) {
                Ok(()) => break,
                Err(DevError::Timeout) => {
                    full += 1;
                    d.poll();
                    kassert!(time::now_ns() - start < 30 * WAIT_NS, "TX ring never drained ({} sent)", i);
                }
                Err(e) => return Err(format!("frame {i}: {e:?}")),
            }
        }
    }
    kassert!(d.stats().tx_frames - before == n as u64);
    // The device hands every buffer back.
    let (_, _, txs, _) = d.ring_info();
    let t0 = time::now_ns();
    loop {
        d.poll();
        let _ = d.transmit(&frames::junk(mac, 60, n)); // reclaims on the way
        let (_, _, _, free) = d.ring_info();
        if free + 1 >= txs as usize {
            break;
        }
        kassert!(time::now_ns() - t0 < WAIT_NS, "TX buffers not returned: {} of {} free", free, txs);
    }
    kassert!(d.state() == State::Running);
    klog::event!(Info, "net", "ktest.burst", frames = n, ring_full_retries = full);
    Ok(())
}
ktest!("nw.virtio.burst", Safe, burst);

fn robust() -> TestResult {
    // NW-3: the stack keeps away from the cards while this test sends and receives raw frames.
    let _cards = net::stack::hold_cards();
    let Some(d) = card("nw.virtio.robust")? else { return Ok(()) };
    kassert!(d.transmit(&[]) == Err(DevError::InvalidArgument));
    kassert!(d.transmit(&[0u8; 13]) == Err(DevError::InvalidArgument));
    kassert!(d.transmit(&alloc::vec![0u8; crate::BUF_SIZE]) == Err(DevError::InvalidArgument));
    kassert!(!d.poll_tx(&[]), "empty emergency frame refused");
    // A receive buffer too small for the frame: the frame is dropped and counted, nothing breaks.
    let mac = d.mac().0;
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
        kassert!(time::now_ns() - start < WAIT_NS, "no frame arrived to drop");
    }
    kassert!(d.state() == State::Running);
    arp_gateway(&d, false)?;
    Ok(())
}
ktest!("nw.virtio.robust", Safe, robust);

fn quiesce() -> TestResult {
    // NW-3: the stack keeps away from the cards while this test sends and receives raw frames.
    let _cards = net::stack::hold_cards();
    let Some(d) = card("nw.virtio.quiesce")? else { return Ok(()) };
    let seq = klog::next_seq();
    Quiesce::quiesce(&*d);
    kassert!(d.state() == State::Quiesced);
    kassert!(d.device_status() == 0, "device reset: status {:#x}", d.device_status());
    kassert!(d.pci().command() & 0x4 == 0, "bus master cleared");
    kassert!(!d.link().up, "no link while quiesced");
    let req = frames::arp_request(d.mac().0, GUEST_IP, GATEWAY);
    kassert!(d.transmit(&req).is_err(), "transmit refused while quiesced");
    kassert!(!d.poll_tx(&req), "poll_tx refused while quiesced");
    let mut buf = [0u8; 2048];
    kassert!(d.receive(&mut buf).is_none());
    d.poll();
    kassert!(d.state() == State::Quiesced, "the poller does not restart a quiesced card");
    // A transmit/receive that failed while racing with the quiesce must not turn it into a restartable failure.
    d.fail_for_test();
    kassert!(d.state() == State::Quiesced, "a failure does not override Quiesce");
    d.poll();
    kassert!(d.state() == State::Quiesced && d.pci().command() & 0x4 == 0, "still stopped, bus master off");
    kassert!(klog::read_since(seq, 64).iter().any(|r| r.name == "virtio.quiesce"), "quiesce event");
    // And back: the same card works again after an explicit restart (Reload's undo path, link restart).
    d.restart("ktest after quiesce")?;
    kassert!(d.pci().command() & 0x4 != 0, "bus master on again");
    arp_gateway(&d, false)?;
    Ok(())
}
ktest!("nw.virtio.quiesce", Safe, quiesce);

/// Failure handling: Failed with a scheduled restart, the poller restarts it, and the back-off keeps growing
/// across a card that fails again right after coming up (it is reset only after 10 s of stable running).
fn failover() -> TestResult {
    // NW-3: the stack keeps away from the cards while this test sends and receives raw frames.
    let _cards = net::stack::hold_cards();
    let Some(d) = card("nw.virtio.failover")? else { return Ok(()) };
    let (delay0, _) = d.backoff();
    let t0 = time::now_ns();
    d.fail_for_test();
    kassert!(d.state() == State::Failed);
    let (delay1, at1) = d.backoff();
    kassert!(at1 >= t0 + delay0 && delay1 == (delay0 * 2).min(60_000_000_000), "back-off {} -> {}", delay0, delay1);
    kassert!(!d.link().up && d.transmit(&frames::arp_request(d.mac().0, GUEST_IP, GATEWAY)).is_err(), "no traffic while failed");
    d.poll();
    kassert!(d.state() == State::Failed, "no restart before the back-off ran out");
    while d.state() != State::Running {
        kassert!(time::now_ns() - t0 < delay0 + WAIT_NS, "the poller did not restart the card");
        d.poll();
        hal::cpu::spin_hint();
    }
    kassert!(d.backoff().0 == delay1, "back-off not reset by a restart that just came up");
    d.fail_for_test();
    let (delay2, at2) = d.backoff();
    kassert!(at2 >= time::now_ns() + delay1 - 1_000_000_000 && delay2 == (delay1 * 2).min(60_000_000_000), "second failure waits longer");
    d.restart("ktest failover cleanup")?;
    arp_gateway(&d, false)?;
    Ok(())
}
ktest!("nw.virtio.failover", Safe, failover);

/// Link watch against a real cable pull: `host/tests/nw/10-virtio-net.sh` (third run) answers the
/// `ktest.link_wait` event with QMP `set_link off`, then `on` after `ktest.link_down_seen`. Not under the
/// `nw.virtio` prefix, so the runs without QMP do not include it. The script gives its card the MAC
/// `QMP_MAC`; any other card (the gate boots with one) means nobody will pull the cable, so the test skips.
fn link_toggle() -> TestResult {
    // NW-3: the stack keeps away from the cards while this test sends and receives raw frames.
    let _cards = net::stack::hold_cards();
    let Some(d) = card("nw.link.qmp")? else { return ktest::skip("no virtio-net card") };
    if d.mac().0 != QMP_MAC {
        return ktest::skip("not the QMP scenario card (host/tests/nw/10-virtio-net.sh)");
    }
    let seq = klog::next_seq();
    let wait = |up: bool| {
        let t0 = time::now_ns();
        while d.link().up != up {
            if time::now_ns() - t0 > 12 * WAIT_NS {
                return false;
            }
            d.poll();
            hal::cpu::spin_hint();
        }
        true
    };
    kassert!(d.link().up, "link up at the start");
    klog::event!(Info, "net", "ktest.link_wait"; "waiting for QMP set_link off");
    kassert!(wait(false), "link never went down");
    let req = frames::arp_request(d.mac().0, GUEST_IP, GATEWAY);
    kassert!(d.transmit(&req) == Err(DevError::Io), "no frames to a card without link");
    klog::event!(Info, "net", "ktest.link_down_seen");
    kassert!(wait(true), "link never came back");
    arp_gateway(&d, false)?;
    let ev = klog::read_since(seq, 256);
    kassert!(ev.iter().any(|r| r.name == "link.down") && ev.iter().any(|r| r.name == "link.up"), "link events");
    Ok(())
}
ktest!("nw.link.qmp", Safe, link_toggle);

const QMP_MAC: [u8; 6] = [0x52, 0x54, 0x00, 0x4e, 0x57, 0x01];

/// NW-4: the card runs on MSI-X (configuration on vector 0, both queues on vector 1) through PC-3, INTx off;
/// the stack is told the card is interrupt-driven and sleeps until a wake; a ping to the gateway raises the
/// queue vector's count and is answered.
fn irq() -> TestResult {
    let Some(d) = card("nw.virtio.irq")? else { return Ok(()) };
    if d.irq_setup() == "polled" {
        if net::config("net_irq").is_some_and(|v| v.trim() == "off") {
            return ktest::skip("net_irq=off: polled by request");
        }
        return Err("virtio-net runs polled: no MSI-X (see virtio.irq_fallback / pci msi.refused)".into());
    }
    let (stats, released) = d.irq_stats().ok_or("no interrupt set kept")?;
    kassert!(!released, "set released while the card runs");
    kassert!(d.pci().command() & (1 << 10) != 0, "INTx off");
    kassert!(stats.iter().all(|x| x.vector.is_some() && !x.stormed), "every index has a vector: {stats:?}");
    let q = stats.len() - 1;
    d.report_irq();
    kassert!(net::netdev::is_irq_driven(NetDevice::name(&*d)), "the stack was told");
    let st = net::stack::kernel().ok_or("no kernel stack")?;
    st.wait_configured(30_000_000_000);
    let gw = net::Ipv4(GATEWAY);
    let before = stats[q].deliveries;
    for _ in 0..5 {
        st.ping(gw, 2_000_000_000).map_err(|e| format!("ping 10.0.2.2: {e:?}"))?;
    }
    let after = d.irq_stats().map(|x| x.0[q].deliveries).unwrap_or(0);
    klog::event!(Info, "net", "ktest.virtio_irq", mode = d.irq_setup(), vectors = stats.len(), queue_before = before,
        queue_after = after, all_cards_irq = net::netdev::all_irq_driven());
    kassert!(after > before, "queue vector count did not rise with 5 pings");
    Ok(())
}
ktest!("nw.virtio.irq", Safe, irq);
