//! The WLAN as the network card, end to end on the host (V1's wlan-net-test.mjs, its first boot
//! ported; no QEMU): the Rust station on V1's fake chip, the fake AP bridged to V1's LAN
//! (net-lan.mjs through wlan-fake.mjs's lanBridge, UDP on 127.0.0.1:28400/28401 or the next free pair, the WL port
//! range), smoltcp as the IP side. The supervisor scans and joins (wifi's state machine and
//! supplicant, the fake network's test passphrase), then: DHCP (192.168.77.20), ping of the
//! router, DNS of a LAN name, an HTTP page byte for byte, a group rekey while linked (a broadcast
//! under the new group key gets its answer), a beacon loss the firmware reports (the station
//! leaves with reason 4 and joins again 2 s later by itself), a deauthentication by the AP (joins
//! again), WLAN off (no key left in the station, nothing of the passphrase in a log line).
//! What changes for V2 (spec/wlan.md §4): V1's `invoke Wlan` commands become the supervisor's
//! steps; the card choice (cable vs WLAN, one card at a time) is gone with multi-NIC (NW).

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use smoltcp::iface::{Config, Interface, SocketHandle, SocketSet};
use smoltcp::phy::{self, Device, DeviceCapabilities, Medium};
use smoltcp::socket::{dhcpv4, icmp, tcp, udp};
use smoltcp::time::Instant as SInstant;
use smoltcp::wire::{EthernetAddress, Icmpv4Packet, Icmpv4Repr, IpAddress, IpCidr, Ipv4Address};

use rtw89_core::sta::link;
use rtw89_core::sta::supervise::{Step, Wlan, WlanCfg};
use rtw89_core::sta::{LinkState, StaEvent};
use rtw89_core::tables::regs::*;

use wifi::events::LinkLoss;
use wifi::mlme::JoinTiming;

use super::life::Life;
use super::sta::{FAKE_PASS, SSID};
use super::*;

/// The LAN's UDP port (and the next): WL_LAN_PORT, else the first pair from 28400 up that is free
/// now (both in the WL range 28000-28499; another run of this test, e.g. a second session in the
/// lane, may hold 28400: the fake would die with EADDRINUSE)
fn lan_port() -> String {
    if let Ok(p) = std::env::var("WL_LAN_PORT") {
        return p;
    }
    let free = |p: u16| std::net::UdpSocket::bind(("127.0.0.1", p)).is_ok();
    (28400u16..28498)
        .step_by(2)
        .find(|&p| free(p) && free(p + 1))
        .unwrap_or(28400)
        .to_string()
}
const ROUTER: Ipv4Address = Ipv4Address::new(192, 168, 77, 1);
const GUEST: Ipv4Address = Ipv4Address::new(192, 168, 77, 20);

/// The station as smoltcp's Ethernet device: frames in from the station's polls, out to `send`
#[derive(Default)]
struct Dev {
    rx: VecDeque<Vec<u8>>,
    tx: VecDeque<Vec<u8>>,
}

struct Rx(Vec<u8>);
struct Tx<'a>(&'a mut VecDeque<Vec<u8>>);

impl phy::RxToken for Rx {
    fn consume<R, F: FnOnce(&[u8]) -> R>(self, f: F) -> R {
        f(&self.0)
    }
}

impl phy::TxToken for Tx<'_> {
    fn consume<R, F: FnOnce(&mut [u8]) -> R>(self, len: usize, f: F) -> R {
        let mut b = vec![0u8; len];
        let r = f(&mut b);
        self.0.push_back(b);
        r
    }
}

impl Device for Dev {
    type RxToken<'a> = Rx;
    type TxToken<'a> = Tx<'a>;
    fn receive(&mut self, _: SInstant) -> Option<(Rx, Tx<'_>)> {
        let f = self.rx.pop_front()?;
        Some((Rx(f), Tx(&mut self.tx)))
    }
    fn transmit(&mut self, _: SInstant) -> Option<Tx<'_>> {
        Some(Tx(&mut self.tx))
    }
    fn capabilities(&self) -> DeviceCapabilities {
        let mut c = DeviceCapabilities::default();
        c.medium = Medium::Ethernet;
        c.max_transmission_unit = 1514;
        c
    }
}

/// V1's net-lan.mjs LONG: 60 rows
fn long_page() -> String {
    (0..60)
        .map(|i| format!("row {i:02} {}\n", "-".repeat(42)))
        .collect()
}

struct Net {
    l: Life,
    w: Wlan,
    dev: Dev,
    iface: Interface,
    sockets: SocketSet<'static>,
    dhcp: SocketHandle,
    start: Instant,
    joins: u32,
    losses: Vec<LinkLoss>,
    ip: Option<Ipv4Address>,
}

impl Net {
    fn now(&self) -> SInstant {
        SInstant::from_millis(self.start.elapsed().as_millis() as i64)
    }

    /// One turn of the driver thread and of the IP stack
    fn turn(&mut self) {
        let w = &mut self.w;
        let step = self
            .l
            .go(|b, c, s, e| w.step(b, c, s, e))
            .expect("a WLAN step");
        match step {
            Step::Frames(f) => self.dev.rx.extend(f),
            Step::Joined(j) => {
                assert_eq!((j.aid, j.ch, j.group_key), (1, 6, true));
                self.joins += 1;
            }
            Step::LinkLost(why) => {
                self.losses.push(why);
                // the link went: the address with it (DHCP starts again on the next link)
                self.sockets.get_mut::<dhcpv4::Socket>(self.dhcp).reset();
                self.iface.update_ip_addrs(|a| a.clear());
                self.ip = None;
            }
            Step::JoinFailed(e) => panic!("join failed: {e}"),
            Step::Idle => {}
        }
        let now = self.now();
        self.iface.poll(now, &mut self.dev, &mut self.sockets);
        if let Some(ev) = self.sockets.get_mut::<dhcpv4::Socket>(self.dhcp).poll() {
            match ev {
                dhcpv4::Event::Configured(c) => {
                    self.iface.update_ip_addrs(|a| {
                        a.clear();
                        a.push(IpCidr::Ipv4(c.address)).unwrap();
                    });
                    if let Some(r) = c.router {
                        self.iface.routes_mut().add_default_ipv4_route(r).unwrap();
                    }
                    self.ip = Some(c.address.address());
                }
                dhcpv4::Event::Deconfigured => {
                    self.iface.update_ip_addrs(|a| a.clear());
                    self.ip = None;
                }
            }
        }
        while let Some(f) = self.dev.tx.pop_front() {
            if self.l.sta.state != LinkState::Connected {
                continue; // no link: dropped as a card without carrier would
            }
            self.l
                .go(|b, c, s, e| link::send(b, c, s, e, &f))
                .unwrap_or_else(|e| panic!("send: {e}"));
        }
        std::thread::sleep(Duration::from_millis(2));
    }

    fn until(&mut self, what: &str, secs: u64, mut done: impl FnMut(&mut Net) -> bool) {
        let end = Instant::now() + Duration::from_secs(secs);
        while !done(self) {
            assert!(Instant::now() < end, "{what}: not within {secs} s");
            self.turn();
        }
    }

    fn ping(&mut self, n: u16) -> u16 {
        let h = self.sockets.add(icmp::Socket::new(
            icmp::PacketBuffer::new(vec![icmp::PacketMetadata::EMPTY; 8], vec![0; 2048]),
            icmp::PacketBuffer::new(vec![icmp::PacketMetadata::EMPTY; 8], vec![0; 2048]),
        ));
        self.sockets
            .get_mut::<icmp::Socket>(h)
            .bind(icmp::Endpoint::Ident(0x4243))
            .unwrap();
        let caps = self.dev.capabilities();
        let mut replies = 0;
        for seq in 0..n {
            let repr = Icmpv4Repr::EchoRequest {
                ident: 0x4243,
                seq_no: seq,
                data: b"bitsos v2 wlan",
            };
            let mut buf = vec![0u8; repr.buffer_len()];
            repr.emit(&mut Icmpv4Packet::new_unchecked(&mut buf), &caps.checksum);
            self.sockets
                .get_mut::<icmp::Socket>(h)
                .send_slice(&buf, IpAddress::Ipv4(ROUTER))
                .unwrap();
            let end = Instant::now() + Duration::from_secs(5);
            while Instant::now() < end {
                self.turn();
                let s = self.sockets.get_mut::<icmp::Socket>(h);
                if s.can_recv() {
                    let (p, _) = s.recv().unwrap();
                    let pk = Icmpv4Packet::new_checked(p).unwrap();
                    if let Ok(Icmpv4Repr::EchoReply { seq_no, .. }) =
                        Icmpv4Repr::parse(&pk, &caps.checksum)
                        && seq_no == seq
                    {
                        replies += 1;
                        break;
                    }
                }
            }
        }
        self.sockets.remove(h);
        replies
    }

    fn dns(&mut self, name: &str) -> Option<[u8; 4]> {
        let h = self.sockets.add(udp::Socket::new(
            udp::PacketBuffer::new(vec![udp::PacketMetadata::EMPTY; 4], vec![0; 2048]),
            udp::PacketBuffer::new(vec![udp::PacketMetadata::EMPTY; 4], vec![0; 2048]),
        ));
        self.sockets.get_mut::<udp::Socket>(h).bind(53_530).unwrap();
        let mut q = vec![0x12, 0x34, 1, 0, 0, 1, 0, 0, 0, 0, 0, 0];
        for part in name.split('.') {
            q.push(part.len() as u8);
            q.extend_from_slice(part.as_bytes());
        }
        q.extend_from_slice(&[0, 0, 1, 0, 1]);
        self.sockets
            .get_mut::<udp::Socket>(h)
            .send_slice(&q, (IpAddress::Ipv4(ROUTER), 53))
            .unwrap();
        let end = Instant::now() + Duration::from_secs(5);
        let mut ans = None;
        while Instant::now() < end && ans.is_none() {
            self.turn();
            let s = self.sockets.get_mut::<udp::Socket>(h);
            if s.can_recv() {
                let (m, _) = s.recv().unwrap();
                // the A record's data: the last 4 bytes of a one-answer reply
                if m.len() >= q.len() + 16 && m[0..2] == [0x12, 0x34] {
                    let a = &m[m.len() - 4..];
                    ans = Some([a[0], a[1], a[2], a[3]]);
                }
            }
        }
        self.sockets.remove(h);
        ans
    }

    fn http(&mut self, port: u16) -> Vec<u8> {
        let h = self.sockets.add(tcp::Socket::new(
            tcp::SocketBuffer::new(vec![0; 16384]),
            tcp::SocketBuffer::new(vec![0; 2048]),
        ));
        let ctx = self.iface.context();
        self.sockets
            .get_mut::<tcp::Socket>(h)
            .connect(ctx, (IpAddress::Ipv4(ROUTER), port), 49_152)
            .unwrap();
        let mut got = Vec::new();
        let mut sent = false;
        let end = Instant::now() + Duration::from_secs(15);
        while Instant::now() < end {
            self.turn();
            let s = self.sockets.get_mut::<tcp::Socket>(h);
            if !sent && s.may_send() {
                s.send_slice(b"GET / HTTP/1.0\r\n\r\n").unwrap();
                sent = true;
            }
            while s.can_recv() {
                s.recv(|b| {
                    got.extend_from_slice(b);
                    (b.len(), ())
                })
                .unwrap();
            }
            if sent && !s.may_recv() {
                break;
            }
        }
        self.sockets.remove(h);
        got
    }

    fn ap_log(&mut self) -> Vec<String> {
        self.l.ap_log()
    }
}

#[test]
fn wlan_as_netdev_on_the_fake_ap_and_lan() {
    if skip("wlan_as_netdev_on_the_fake_ap_and_lan") {
        return;
    }
    let port = lan_port();
    let p: u32 = port.parse().expect("WL_LAN_PORT");
    assert!(
        (28000..28499).contains(&p),
        "the LAN port {p} is outside WL's range"
    );
    let mut l = Life::with_flags("net", &["--lan", &port]);
    l.env.real = Some(Instant::now());
    let own = l.own();
    l.go(|b, c, s, e| rtw89_core::sta::add_interface(b, c, s, e, own))
        .unwrap();
    let cfg = WlanCfg {
        ssid: SSID.to_vec(),
        pmk: wifi::crypto::psk(FAKE_PASS, SSID).unwrap(),
        band: Band::B2g,
        channels: vec![1, 6, 11],
        dwell_ms: 20,
        timing: JoinTiming::default(),
    };
    let shown_cfg = format!("{cfg:?}");
    assert!(
        shown_cfg.contains("<secret 32 bytes>") && !shown_cfg.contains("fake-mode"),
        "{shown_cfg}"
    );
    let mut w = Wlan::new(cfg);
    let t0 = l.env.real.unwrap().elapsed().as_millis() as u64;
    w.sup.on(t0);
    let mut dev = Dev::default();
    let start = Instant::now();
    let iface = Interface::new(
        Config::new(EthernetAddress(own).into()),
        &mut dev,
        SInstant::from_millis(0),
    );
    let mut sockets = SocketSet::new(vec![]);
    let dhcp = sockets.add(dhcpv4::Socket::new());
    let mut n = Net {
        l,
        w,
        dev,
        iface,
        sockets,
        dhcp,
        start,
        joins: 0,
        losses: Vec::new(),
        ip: None,
    };

    // the supervisor scans and joins; DHCP over the WLAN gives net-lan's address
    n.until("join and DHCP", 60, |n| n.ip.is_some());
    assert_eq!((n.joins, n.ip), (1, Some(GUEST)));
    let log = n.ap_log();
    assert!(
        log.iter().any(|x| x == "msg4 ok"),
        "the 4-way handshake went through: {log:?}"
    );
    assert_eq!(n.ping(4), 4, "ping of the router over WLAN");
    assert_eq!(
        n.dns("end.test"),
        Some([192, 168, 77, 5]),
        "DNS of a net-lan name over WLAN"
    );
    let page = n.http(85);
    let text = String::from_utf8_lossy(&page);
    let body = text.split_once("\r\n\r\n").map(|x| x.1).unwrap_or("");
    assert_eq!(
        body,
        long_page(),
        "HTTP of {} bytes byte for byte over WLAN",
        long_page().len()
    );

    // a group key rekey while linked: answered, the new key (another key ID) takes group frames
    let id_before = n.l.sta.keys.gtk.iter().map(|k| k.key_idx).max();
    n.l.ap("rekey 63636363636363636363636363636363");
    n.until("group 2/2", 10, |n| {
        n.ap_log().last().is_some_and(|x| x == "group2 ok")
    });
    let id_after = n.l.sta.keys.gtk.iter().map(|k| k.key_idx).max();
    assert_ne!(id_before, id_after, "the new GTK under another key ID");
    assert_eq!(n.ping(3), 3, "frames still flow after the rekey");
    let arp_before: u32 =
        n.l.ask("lan stats")
            .split(' ')
            .nth(1)
            .unwrap()
            .parse()
            .unwrap();
    assert_eq!(n.l.ask("lan arp"), "ok");
    n.until(
        "the answer to a broadcast under the new group key",
        10,
        |n| {
            n.l.ask("lan stats")
                .split(' ')
                .nth(1)
                .unwrap()
                .parse::<u32>()
                .unwrap()
                > arp_before
        },
    );

    // the AP goes quiet: the firmware's beacon filter reports the loss; the station leaves
    // (reason 4, its own frame) and joins again 2 s later by itself
    let loss = {
        let mut b = Vec::new();
        b.extend_from_slice(
            &(RTW89_C2H_CAT_MAC
                | (RTW89_MAC_C2H_CLASS_OFLD << 2)
                | (RTW89_MAC_C2H_FUNC_BCNFLTR_RPT << 8))
                .to_le_bytes(),
        );
        b.extend_from_slice(&12u32.to_le_bytes());
        b.extend_from_slice(&(RTW89_BCN_FLTR_BEACON_LOSS << 8).to_le_bytes());
        super::sta::hex(&b)
    };
    n.l.ap(&format!("c2h {loss}"));
    n.until("the beacon loss", 10, |n| {
        n.losses.contains(&LinkLoss::BeaconLoss)
    });
    assert!(
        n.ap_log().iter().any(|x| x == "deauth 4"),
        "left with reason 4"
    );
    let lost_at = Instant::now();
    n.until("joined again after the loss", 30, |n| {
        n.joins == 2 && n.ip.is_some()
    });
    assert!(
        lost_at.elapsed() >= Duration::from_millis(1900),
        "the first rejoin 2 s after the loss"
    );
    assert_eq!(n.ping(3), 3, "ping after joining again");

    // the AP ends the link (reason 2): no frame of ours, joined again by the supervisor
    let deauths = n
        .ap_log()
        .iter()
        .filter(|x| x.starts_with("deauth"))
        .count();
    n.l.ap("deauth 2 sta");
    n.until("the AP's deauthentication", 10, |n| {
        n.losses.contains(&LinkLoss::EndedByAp { reason: 2 })
    });
    assert_eq!(
        n.ap_log()
            .iter()
            .filter(|x| x.starts_with("deauth"))
            .count(),
        deauths,
        "no deauthentication of ours"
    );
    n.until("joined again after the deauthentication", 30, |n| {
        n.joins == 3 && n.ip.is_some()
    });
    assert_eq!(n.ping(2), 2);

    // WLAN off: the link left (reason 3), no key left in the station or the security CAM
    let left = n.ap_log().iter().filter(|x| *x == "deauth 3").count();
    let w = &mut n.w;
    n.l.go(|b, c, s, e| w.off(b, c, s, e)).unwrap();
    assert_eq!(
        n.ap_log().iter().filter(|x| *x == "deauth 3").count(),
        left + 1,
        "left with reason 3"
    );
    let s = &n.l.sta;
    assert!(
        s.keys.ptk.is_none() && s.keys.gtk.is_empty() && s.dev.cam.sec_in_use() == 0,
        "no key in the station"
    );
    assert!(
        s.mlme
            .as_ref()
            .is_some_and(|m| m.phase() == wifi::Phase::Down),
        "the handshake context closed"
    );
    let events: Vec<String> =
        n.l.sta
            .take_events()
            .iter()
            .map(|e| e.to_string())
            .collect();
    for want in [
        "joining again in 2000 ms (try 1): no beacons from the AP",
        "joining again in 2000 ms (try 1): ended by the AP, reason 2",
        "WLAN off",
    ] {
        assert!(
            events.iter().any(|e| e == want),
            "event {want:?}: {events:?}"
        );
    }
    assert!(
        events.iter().all(|e| !e.contains("fake-mode")),
        "no passphrase in a log line"
    );
    assert!(
        !events
            .iter()
            .any(|e| e.contains(&format!("{:?}", StaEvent::BeaconLoss)) && e.contains("0x"))
    );
    let stats = n.l.ask("lan stats");
    eprintln!("LAN: {stats}; joins {}, losses {:?}", n.joins, n.losses);
    let _ = n.l.fake.finish();
}
