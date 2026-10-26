//! Fake USB devices for the host tests (`fake_usb.rs` puts them on the fake controller's ports): descriptors,
//! the standard and hub class requests they answer, hub ports with power, enable and change bits, and
//! interrupt IN reports (a hub's change bitmap or queued reports).

use std::collections::{BTreeMap, VecDeque};
use std::vec;
use std::vec::Vec;
use usb_core::Speed;

pub struct FakeHubPort {
    pub dev: Option<FakeDev>,
    pub powered: bool,
    pub enabled: bool,
    pub change: u16,
}

pub struct FakeHub {
    pub ss: bool,
    pub ports: Vec<FakeHubPort>,
    pub desc: Vec<u8>,
}

pub struct FakeDev {
    pub speed: Speed,
    pub device: Vec<u8>,
    pub config: Vec<u8>,
    pub mps0: u16,
    pub hub: Option<FakeHub>,
    pub configured: u8,
    pub alt: u8,
    pub hub_depth: Option<u16>,
    /// Setup packets seen, in order.
    pub requests: Vec<u64>,
    /// bRequest values answered with STALL.
    pub stall: Vec<u8>,
    /// Never answers on endpoint 0 (no event: the host times out).
    pub hang: bool,
    /// Address Device fails with a transaction error this many times.
    pub fail_addresses: usize,
    /// SET_ADDRESS straight after the reset (Address Device BSR 0 on a fresh slot) fails this many times; after
    /// a descriptor read at address 0 (BSR 1 first) it works.
    pub fail_direct_addresses: usize,
    /// Interrupt IN reports to deliver.
    pub reports: VecDeque<Vec<u8>>,
    /// Bulk/interrupt endpoints (address) whose IN transfers come from here, one entry per transfer.
    pub in_q: BTreeMap<u8, VecDeque<Vec<u8>>>,
    /// What OUT endpoints received, one entry per transfer.
    pub out_seen: Vec<(u8, Vec<u8>)>,
    /// OUT endpoints that NAK every token (the host keeps the TD queued).
    pub nak_out: Vec<u8>,
    /// Endpoints whose next token gets a STALL (and that stay halted until CLEAR_FEATURE(ENDPOINT_HALT)).
    pub stall_next: Vec<u8>,
    /// Device-side halt per endpoint: every token stalls.
    pub halted: Vec<u8>,
    /// Device-side data toggle per endpoint (reset by CLEAR_FEATURE(ENDPOINT_HALT)).
    pub toggles: BTreeMap<u8, bool>,
    pub clear_halts: Vec<u8>,
    /// A Bulk-Only mass storage disk (CBW/data/CSW; INQUIRY, READ CAPACITY(10), READ(10), TEST UNIT READY).
    pub bot: Option<Bot>,
}

pub enum BotState {
    Cbw,
    DataIn(Vec<u8>),
    Csw(Vec<u8>),
}

pub struct Bot {
    pub disk: Vec<u8>,
    pub state: BotState,
    pending_csw: Option<Vec<u8>>,
    /// QEMU usb-storage: an IN token while waiting for a CBW gets a STALL (else a NAK).
    pub stall_in_idle: bool,
    pub commands: Vec<u8>,
}

impl Bot {
    pub fn new(disk: Vec<u8>) -> Bot {
        Bot { disk, state: BotState::Cbw, pending_csw: None, stall_in_idle: true, commands: Vec::new() }
    }
    /// A CBW arrived: prepare the data phase and the CSW. False if it is no valid CBW.
    fn cbw(&mut self, c: &[u8]) -> bool {
        if c.len() != 31 || &c[0..4] != b"USBC" {
            return false;
        }
        let tag = [c[4], c[5], c[6], c[7]];
        let want = u32::from_le_bytes([c[8], c[9], c[10], c[11]]) as usize;
        let cb = &c[15..31];
        self.commands.push(cb[0]);
        let blocks = (self.disk.len() / 512) as u32;
        let (data, status): (Vec<u8>, u8) = match cb[0] {
            0x00 => (vec![], 0),
            0x12 => {
                let mut d = vec![0u8; 36];
                d[4] = 31;
                d[8..16].copy_from_slice(b"BitsFake");
                (d, 0)
            }
            0x25 => {
                let mut d = (blocks - 1).to_be_bytes().to_vec();
                d.extend_from_slice(&512u32.to_be_bytes());
                (d, 0)
            }
            0x28 => {
                let lba = u32::from_be_bytes([cb[2], cb[3], cb[4], cb[5]]) as usize;
                let n = u16::from_be_bytes([cb[7], cb[8]]) as usize;
                match self.disk.get(lba * 512..(lba + n) * 512) {
                    Some(d) => (d.to_vec(), 0),
                    None => (vec![], 1),
                }
            }
            _ => (vec![], 1),
        };
        let sent = data.len().min(want);
        let residue = (want - sent) as u32;
        let mut csw = b"USBS".to_vec();
        csw.extend_from_slice(&tag);
        csw.extend_from_slice(&residue.to_le_bytes());
        csw.push(status);
        self.state = if sent > 0 { BotState::DataIn(data[..sent].to_vec()) } else { BotState::Csw(csw.clone()) };
        if sent > 0 {
            self.pending_csw = Some(csw);
        }
        true
    }
}

fn device_desc(usb: u16, class: u8, protocol: u8, mps0_raw: u8, vid: u16, pid: u16) -> Vec<u8> {
    let (u, v, p) = (usb.to_le_bytes(), vid.to_le_bytes(), pid.to_le_bytes());
    vec![18, 1, u[0], u[1], class, 0, protocol, mps0_raw, v[0], v[1], p[0], p[1], 0, 1, 0, 0, 0, 1]
}

fn config_desc(body: &[u8], interfaces: u8) -> Vec<u8> {
    let total = (9 + body.len()) as u16;
    let mut v = vec![9, 2, total as u8, (total >> 8) as u8, interfaces, 1, 0, 0xA0, 50];
    v.extend_from_slice(body);
    v
}

impl FakeDev {
    fn new(speed: Speed, device: Vec<u8>, config: Vec<u8>) -> FakeDev {
        let mps0 = if speed.is_superspeed() { 512 } else { device[7] as u16 };
        FakeDev {
            speed,
            device,
            config,
            mps0,
            hub: None,
            configured: 0,
            alt: 0,
            hub_depth: None,
            requests: Vec::new(),
            stall: Vec::new(),
            hang: false,
            fail_addresses: 0,
            fail_direct_addresses: 0,
            reports: VecDeque::new(),
            in_q: BTreeMap::new(),
            out_seen: Vec::new(),
            nak_out: Vec::new(),
            stall_next: Vec::new(),
            halted: Vec::new(),
            toggles: BTreeMap::new(),
            clear_halts: Vec::new(),
            bot: None,
        }
    }

    /// A boot keyboard (QEMU usb-kbd shape: endpoint 0 packet size 8 at full speed, 0627:0001, EP 0x81, 10 ms).
    pub fn keyboard(speed: Speed) -> FakeDev {
        let mps0 = if speed.is_superspeed() { 9 } else if speed == Speed::High { 64 } else { 8 };
        let body = [9, 4, 0, 0, 1, 3, 1, 1, 0, 9, 0x21, 0x11, 1, 0, 1, 0x22, 63, 0, 7, 5, 0x81, 3, 8, 0, 10];
        FakeDev::new(speed, device_desc(0x0110, 0, 0, mps0, 0x0627, 0x0001), config_desc(&body, 1))
    }

    /// A UAC1 headset shaped like Georg's JBL Quantum 800 dongle (plan/hw/jbl-quantum-800-lsusb.txt): audio
    /// control 0; playback streaming 1 with alternate settings 1 (EP 0x02 OUT isochronous adaptive, 288 bytes,
    /// 1 ms) and 2 (the same endpoint at 144 bytes); capture streaming 3, alternate setting 1 (EP 0x81 IN, 144
    /// bytes). 9-byte audio endpoint descriptors.
    pub fn audio(speed: Speed) -> FakeDev {
        let iface = |n: u8, alt: u8, eps: u8, sub: u8| [9u8, 4, n, alt, eps, 1, sub, 0, 0];
        let ep = |addr: u8, mps: u16| [9u8, 5, addr, 0x0D, mps as u8, (mps >> 8) as u8, 1, 0, 0];
        let mut body = Vec::new();
        body.extend_from_slice(&iface(0, 0, 0, 1));
        body.extend_from_slice(&iface(1, 0, 0, 2));
        body.extend_from_slice(&iface(1, 1, 1, 2));
        body.extend_from_slice(&ep(0x02, 0x0120));
        body.extend_from_slice(&iface(1, 2, 1, 2));
        body.extend_from_slice(&ep(0x02, 0x0090));
        body.extend_from_slice(&iface(3, 0, 0, 2));
        body.extend_from_slice(&iface(3, 1, 1, 2));
        body.extend_from_slice(&ep(0x81, 0x0090));
        FakeDev::new(speed, device_desc(0x0110, 0, 0, 64, 0x0ECB, 0x203E), config_desc(&body, 3))
    }

    /// A mass storage stick (class 08/06/50, bulk 0x81 and 0x02; SuperSpeed with companions).
    pub fn storage(speed: Speed) -> FakeDev {
        let (mps0, mps) = match speed {
            Speed::Super | Speed::SuperPlus => (9u8, 1024u16),
            Speed::High => (64, 512),
            _ => (64, 64),
        };
        let m = mps.to_le_bytes();
        let mut body = vec![9, 4, 0, 0, 2, 8, 6, 0x50, 0, 7, 5, 0x81, 2, m[0], m[1], 0];
        if speed.is_superspeed() {
            body.extend_from_slice(&[6, 0x30, 15, 0, 0, 0]);
        }
        body.extend_from_slice(&[7, 5, 0x02, 2, m[0], m[1], 0]);
        if speed.is_superspeed() {
            body.extend_from_slice(&[6, 0x30, 15, 0, 0, 0]);
        }
        FakeDev::new(speed, device_desc(0x0200, 0, 0, mps0, 0x46F4, 0x0001), config_desc(&body, 1))
    }

    /// A hub with `n` ports: USB 2 (full or high speed; `mtt` offers one TT per port in alternate setting 1)
    /// or SuperSpeed.
    pub fn hub(speed: Speed, n: u8, mtt: bool) -> FakeDev {
        let ss = speed.is_superspeed();
        let mps0 = if ss { 9 } else { 64 };
        let proto = if speed == Speed::High { if mtt { 2 } else { 1 } } else if ss { 3 } else { 0 };
        let mut body = vec![9, 4, 0, 0, 1, 9, 0, if mtt { 1 } else { 0 }, 0, 7, 5, 0x81, 3, 2, 0, if ss { 8 } else { 12 }];
        if ss {
            body.extend_from_slice(&[6, 0x30, 0, 0, 2, 0]);
        }
        if mtt {
            body.extend_from_slice(&[9, 4, 0, 1, 1, 9, 0, 2, 0, 7, 5, 0x81, 3, 2, 0, 12]);
        }
        let mut d = FakeDev::new(speed, device_desc(if ss { 0x0300 } else { 0x0200 }, 9, proto, mps0, 0x0409, 0x55AA), config_desc(&body, 1));
        // wHubCharacteristics: TT think time 2 (bits 6:5) on a high speed hub; bPwrOn2PwrGood 50 (100 ms).
        let desc = if ss { vec![12, 0x2A, n, 0, 0, 50, 0, 0, 0, 0, 0, 0] } else { vec![9, 0x29, n, 0x40, 0, 50, 100, 0, 0xFF] };
        let ports = (0..n).map(|_| FakeHubPort { dev: None, powered: false, enabled: false, change: 0 }).collect();
        d.hub = Some(FakeHub { ss, ports, desc });
        d
    }

    fn port_status(hub: &FakeHub, p: &FakeHubPort) -> u16 {
        let mut s = 0u16;
        if p.powered {
            s |= if hub.ss { 1 << 9 } else { 1 << 8 };
            if let Some(d) = p.dev.as_ref() {
                s |= 1;
                if p.enabled {
                    s |= 2;
                    if !hub.ss {
                        s |= match d.speed {
                            Speed::Low => 1 << 9,
                            Speed::High => 1 << 10,
                            _ => 0,
                        };
                    }
                }
            }
        }
        s
    }

    /// A control request: the IN data, or Err for STALL.
    pub(crate) fn request(&mut self, setup: u64, out: &[u8]) -> Result<Vec<u8>, ()> {
        let (rt, req, val, idx) = (setup as u8, (setup >> 8) as u8, (setup >> 16) as u16, (setup >> 32) as u16);
        self.requests.push(setup);
        if self.stall.contains(&req) {
            return Err(());
        }
        let _ = out;
        match (rt, req) {
            (0x80, 6) => match (val >> 8) as u8 {
                1 => Ok(self.device.clone()),
                2 => Ok(self.config.clone()),
                _ => Err(()),
            },
            (0x00, 9) => {
                self.configured = val as u8;
                Ok(vec![])
            }
            (0x01, 11) => {
                self.alt = val as u8;
                Ok(vec![])
            }
            (0x02, 1) => {
                // CLEAR_FEATURE(ENDPOINT_HALT): halt off, data toggle back to 0 (USB 2.0 9.4.5).
                let ep = idx as u8;
                self.halted.retain(|&e| e != ep);
                self.toggles.insert(ep, false);
                self.clear_halts.push(ep);
                Ok(vec![])
            }
            _ => self.hub_request(rt, req, val, idx),
        }
    }

    fn hub_request(&mut self, rt: u8, req: u8, val: u16, idx: u16) -> Result<Vec<u8>, ()> {
        let Some(h) = self.hub.as_mut() else { return Err(()) };
        let port = (idx & 0xFF) as usize;
        match (rt, req) {
            (0xA0, 6) if (val >> 8) as u8 == h.desc[1] => Ok(h.desc.clone()),
            (0xA0, 0) => Ok(vec![0, 0, 0, 0]),
            (0x20, 12) => {
                self.hub_depth = Some(val);
                Ok(vec![])
            }
            (0x20, 1) => Ok(vec![]),
            (0xA3, 0) | (0x23, _) if port == 0 || port > h.ports.len() => Err(()),
            (0xA3, 0) => {
                let p = &h.ports[port - 1];
                let s = FakeDev::port_status(h, p);
                Ok(vec![s as u8, (s >> 8) as u8, p.change as u8, (p.change >> 8) as u8])
            }
            (0x23, 3) => {
                let ss = h.ss;
                let p = &mut h.ports[port - 1];
                match val {
                    8 => {
                        if !p.powered && p.dev.is_some() {
                            p.change |= 1;
                        }
                        p.powered = true;
                    }
                    4 | 28 if p.powered && p.dev.is_some() => {
                        p.enabled = true;
                        p.change |= if val == 28 { 0x20 } else { 0x10 };
                    }
                    5 if ss && (idx >> 8) == 4 => p.enabled = false,
                    _ => {}
                }
                Ok(vec![])
            }
            (0x23, 1) => {
                let p = &mut h.ports[port - 1];
                match val {
                    1 => p.enabled = false,
                    16 => p.change &= !1,
                    17 => p.change &= !2,
                    18 => p.change &= !4,
                    19 => p.change &= !8,
                    20 => p.change &= !0x10,
                    25 => p.change &= !0x40,
                    26 => p.change &= !0x80,
                    29 => p.change &= !0x20,
                    _ => {}
                }
                Ok(vec![])
            }
            _ => Err(()),
        }
    }

    /// The next interrupt IN report: a hub's change bitmap, else a queued report.
    pub(crate) fn report(&mut self) -> Option<Vec<u8>> {
        if let Some(h) = self.hub.as_ref() {
            let mut bits = vec![0u8; (h.ports.len() + 1).div_ceil(8)];
            let mut any = false;
            for (i, p) in h.ports.iter().enumerate() {
                if p.change != 0 {
                    bits[(i + 1) / 8] |= 1 << ((i + 1) % 8);
                    any = true;
                }
            }
            return any.then_some(bits);
        }
        self.reports.pop_front()
    }
}

impl FakeDev {
    pub fn with_hang(mut self) -> Self {
        self.hang = true;
        self
    }
    pub fn plug(&mut self, port: u8, dev: FakeDev) {
        let h = self.hub.as_mut().expect("a hub");
        h.ports[port as usize - 1].dev = Some(dev);
    }
}

/// What a device does with a token on a bulk/interrupt endpoint.
pub enum Token {
    /// IN: the data of one transfer; OUT: accepted.
    Data(Vec<u8>),
    Nak,
    Stall,
}

impl FakeDev {
    /// A stick with a BOT disk of `blocks` 512-byte blocks whose bytes are a pattern of their offset.
    pub fn bot_storage(speed: Speed, blocks: usize) -> FakeDev {
        let mut d = FakeDev::storage(speed);
        let disk: Vec<u8> = (0..blocks * 512).map(|i| (i ^ (i >> 9) ^ (i >> 17)) as u8).collect();
        d.bot = Some(Bot::new(disk));
        d
    }

    fn halt_check(&mut self, ep: u8) -> bool {
        if let Some(i) = self.stall_next.iter().position(|&e| e == ep) {
            self.stall_next.remove(i);
            self.halted.push(ep);
        }
        self.halted.contains(&ep)
    }

    /// An IN token on `ep`: one transfer's data, a NAK (nothing to send) or a STALL.
    pub(crate) fn token_in(&mut self, ep: u8, is_interrupt: bool) -> Token {
        if self.halt_check(ep) {
            return Token::Stall;
        }
        if let Some(d) = self.in_q.get_mut(&ep).and_then(|q| q.pop_front()) {
            return Token::Data(d);
        }
        if is_interrupt {
            return self.report().map(Token::Data).unwrap_or(Token::Nak);
        }
        let Some(b) = self.bot.as_mut() else { return Token::Nak };
        match core::mem::replace(&mut b.state, BotState::Cbw) {
            BotState::DataIn(d) => {
                b.state = BotState::Csw(b.pending_csw.take().unwrap_or_default());
                Token::Data(d)
            }
            BotState::Csw(c) => Token::Data(c),
            BotState::Cbw if b.stall_in_idle => {
                self.halted.push(ep);
                Token::Stall
            }
            BotState::Cbw => Token::Nak,
        }
    }

    /// Data an IN transfer did not take (the host's TD was shorter): it is the next transfer's.
    pub(crate) fn give_back(&mut self, ep: u8, rest: Vec<u8>) {
        if let Some(b) = self.bot.as_mut() {
            let next = core::mem::replace(&mut b.state, BotState::Cbw);
            let csw = match next {
                BotState::Csw(c) => c,
                _ => vec![],
            };
            b.pending_csw = Some(csw);
            b.state = BotState::DataIn(rest);
            return;
        }
        self.in_q.entry(ep).or_default().push_front(rest);
    }

    /// An OUT transfer on `ep`.
    pub(crate) fn token_out(&mut self, ep: u8, data: &[u8]) -> Token {
        if self.halt_check(ep) {
            return Token::Stall;
        }
        if self.nak_out.contains(&ep) {
            return Token::Nak;
        }
        self.out_seen.push((ep, data.to_vec()));
        // A CBW out of turn or a bad one: the endpoint stalls (BOT 6.6.1).
        if let Some(b) = self.bot.as_mut()
            && (!matches!(b.state, BotState::Cbw) || !b.cbw(data))
        {
            self.halted.push(ep);
            return Token::Stall;
        }
        Token::Data(vec![])
    }
}
