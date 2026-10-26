//! The bring-up from stage chan to its stop (golden 15606-22537) on V1's fake chip with the Rust
//! station (WL-3): `sta::add_interface`, the software scan, the join through wifi's state machine
//! and supplicant (the fake AP's network, its test passphrase), a DHCP discover and its offer,
//! the disconnect, remove_interface, then the stop's coexistence part, its flushes and its tail.
//! Nothing is replayed any more: every register op, every H2C and every frame is Rust's.
//!
//! The RX polls of the wait loops (scan dwell, handshake, DHCP, firmware answers) come a
//! timing-dependent number of times; as test-bringup.mjs does, a poll that found nothing (RPQ and
//! RXQ status clear and index read, no buffer taken) is left out of the comparison on both
//! sides. Every buffer the station kicks (H2Cs on CH12 and frames) is compared with the JS run's
//! (tests/fake/bringup-ref.mjs): byte for byte, except what is random in a handshake (nonces,
//! MICs, the keys in the SEC_CAM H2Cs) and the EAPOL version byte (V1's eapol.asm and
//! wpa_supplicant send 1, the JS fake's wpa.mjs echoed the AP's 2).

use rtw89_core::bus::{BusError, BusResult, MemWidth, Width};
use rtw89_core::macinit;
use rtw89_core::sta::io::Chip;
use rtw89_core::sta::link::{self, Joined};
use rtw89_core::sta::scan::{self, ScanReq};
use rtw89_core::sta::{self as rsta, Sta, StaEnv, StaEvent};
use rtw89_core::tables::regs::*;
use rtw89_core::tracebus::FakeState;
use rtw89_core::txrx::Txrx;
use rtw89_core::txrx::ieee;

use wifi::eapol::Supplicant;
use wifi::mlme::{JoinTiming, reason};
use wifi::rsn;

use super::*;

/// "# stage scan", "# stage assoc", "# stage dhcp", "# stop" (1-based lines)
pub(super) const SCAN: usize = 16256;
pub(super) const ASSOC: usize = 17742;
pub(super) const DHCP: usize = 22154;
pub(super) const STOP: usize = 22189;
/// the stop's tail: rtw89_hci_deinit (pci::deinit) to bus master off
pub(super) const STOP_HCI: usize = 22460;
pub(super) const ALL_LINES: usize = 22537;
pub(super) const SSID: &[u8] = b"BitsOS-Fake";
/// the fake network's test passphrase (bringup-fake.mjs: made up, never a real one)
pub(super) const FAKE_PASS: &[u8] = b"fake-mode passphrase";

/// One buffer the JS driver kicked (bringup-ref.mjs `rec`)
struct Rec {
    line: usize,
    ring: String,
    page: u32,
    bytes: Vec<u8>,
    frame: Option<Vec<u8>>,
}

fn unhex(s: &str) -> Vec<u8> {
    if s == "-" {
        return Vec::new();
    }
    (0..s.len() / 2)
        .map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).unwrap())
        .collect()
}

pub(super) fn hex(b: &[u8]) -> String {
    if b.is_empty() {
        return "-".into();
    }
    b.iter().map(|v| format!("{v:02x}")).collect()
}

/// The JS bring-up's kicked buffers, in order
fn reference() -> Vec<Rec> {
    let script = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fake/bringup-ref.mjs");
    let res = Command::new("node")
        .arg(&script)
        .env("WLAN_TOOLS", wlan_dir())
        .output()
        .expect("node");
    let text = String::from_utf8(res.stdout).unwrap();
    assert!(
        res.status.success() && text.starts_with(&format!("ok {ALL_LINES}\n")),
        "bringup-ref.mjs: {}\n{}{}",
        res.status,
        text.lines().next().unwrap_or(""),
        String::from_utf8_lossy(&res.stderr)
    );
    let mut recs: Vec<Rec> = text
        .lines()
        .filter(|l| l.starts_with("rec "))
        .map(|l| {
            let w: Vec<&str> = l.split(' ').collect();
            let line: i64 = w[1].parse().unwrap();
            assert!(line > 0, "a kick without its BD line: {l}");
            Rec {
                line: line as usize,
                ring: w[2].into(),
                page: u32::from_str_radix(w[4].trim_start_matches("0x"), 16).unwrap(),
                bytes: unhex(w[5]),
                frame: (w[6] != "-").then(|| unhex(w[6])),
            }
        })
        .collect();
    recs.sort_by_key(|r| r.line);
    recs
}

/// The trace bus with a record of every bulk write into DMA memory (H2C buffers, WD pages,
/// frames: untraced op by op, so the golden file cannot hold them)
pub(super) struct RecBus<'a> {
    pub(super) inner: &'a mut TraceBus<UnixStream>,
    /// (trace length when written, address, bytes)
    pub(super) writes: Vec<(usize, u32, Vec<u8>)>,
    /// fault tests: every access fails (the chip is gone), or every write
    pub(super) fail: Fail,
}

/// Which chip accesses a fault test lets fail
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(super) enum Fail {
    #[default]
    None,
    All,
    Writes,
}

impl RecBus<'_> {
    fn gone(&self) -> BusResult<()> {
        if self.fail == Fail::All {
            return Err(BusError::Gone);
        }
        Ok(())
    }
    fn gone_w(&self) -> BusResult<()> {
        if self.fail != Fail::None {
            return Err(BusError::Gone);
        }
        Ok(())
    }
}

impl Bus for RecBus<'_> {
    fn read(&mut self, w: Width, addr: u32) -> BusResult<u32> {
        self.gone()?;
        self.inner.read(w, addr)
    }
    fn write(&mut self, w: Width, addr: u32, val: u32) -> BusResult<()> {
        self.gone_w()?;
        self.inner.write(w, addr, val)
    }
    fn rmw(&mut self, w: Width, addr: u32, clear: u32, set: u32) -> BusResult<()> {
        self.gone_w()?;
        self.inner.rmw(w, addr, clear, set)
    }
    fn poll(&mut self, w: Width, addr: u32, mask: u32, want: u32, ms: u16) -> BusResult<()> {
        self.gone()?;
        self.inner.poll(w, addr, mask, want, ms)
    }
    fn delay_us(&mut self, us: u32) -> BusResult<()> {
        self.inner.delay_us(us)
    }
    fn cfg_read(&mut self, off: u16) -> BusResult<u32> {
        self.inner.cfg_read(off)
    }
    fn cfg_write(&mut self, off: u16, val: u32) -> BusResult<()> {
        self.inner.cfg_write(off, val)
    }
    fn dma_alloc(&mut self, size: usize, align: usize) -> BusResult<u32> {
        self.inner.dma_alloc(size, align)
    }
    fn dma_free(&mut self, addr: u32, size: usize) -> BusResult<()> {
        self.inner.dma_free(addr, size)
    }
    fn mem_write(&mut self, w: MemWidth, addr: u32, val: u64) -> BusResult<()> {
        self.gone_w()?;
        self.inner.mem_write(w, addr, val)
    }
    fn mem_read32(&mut self, addr: u32) -> BusResult<u32> {
        self.gone()?;
        self.inner.mem_read32(addr)
    }
    fn mem_write_bytes(&mut self, addr: u32, data: &[u8]) -> BusResult<()> {
        self.gone_w()?;
        self.writes
            .push((self.inner.trace().len(), addr, data.to_vec()));
        self.inner.mem_write_bytes(addr, data)
    }
    fn mem_read_bytes(&mut self, addr: u32, out: &mut [u8]) -> BusResult<()> {
        self.gone()?;
        self.inner.mem_read_bytes(addr, out)
    }
    fn now_ms(&mut self) -> u64 {
        self.inner.now_ms()
    }
    fn note(&mut self, text: &str) -> BusResult<()> {
        self.inner.note(text)
    }
    fn flush(&mut self) -> BusResult<()> {
        self.inner.flush()
    }
}

/// A virtual clock that moves only when the station sleeps (so the run's waits are the same
/// every time: the 2-second track work and the coex act1 work never fall due in it, as in the
/// JS run), and what the station sets told to V1's fake (FAKE_CHAN, FAKE_SCAN)
pub(super) struct GoldenEnv {
    pub(super) now: u64,
    pub(super) rnd: u8,
    /// a real clock instead (the LAN behind the fake AP answers in its own time)
    pub(super) real: Option<Instant>,
}

impl<'a> StaEnv<RecBus<'a>> for GoldenEnv {
    fn now_ms(&mut self, _: &mut RecBus<'a>) -> u64 {
        match self.real {
            Some(t) => t.elapsed().as_millis() as u64,
            None => self.now,
        }
    }
    fn sleep_ms(&mut self, _: &mut RecBus<'a>, ms: u32) {
        match self.real {
            Some(_) => std::thread::sleep(Duration::from_millis(ms.into())),
            None => self.now += u64::from(ms),
        }
    }
    fn random(&mut self, out: &mut [u8]) -> bool {
        for b in out {
            self.rnd = self.rnd.wrapping_mul(29).wrapping_add(7);
            *b = self.rnd;
        }
        true
    }
    fn channel_set(&mut self, bus: &mut RecBus<'a>, _: Band, ch: u32) {
        bus.inner.state(FakeState::Chan, ch).unwrap();
    }
    fn scanning(&mut self, bus: &mut RecBus<'a>, on: bool) {
        bus.inner.state(FakeState::Scan, u32::from(on)).unwrap();
    }
}

/// test-bringup.mjs's normalisation: a poll that found nothing is left out. Returns the kept
/// lines and, for each, its index in `lines`.
pub(super) fn normalised(lines: &[String]) -> (Vec<String>, Vec<usize>) {
    let empty: [fn(&str) -> bool; 4] = [
        |l| l == "wr32 0x000010B4 0x00000000 0x00100004",
        |l| l.starts_with("rd32 0x00001054 "),
        |l| l == "wr32 0x000010B4 0x00000000 0x00080003",
        |l| l.starts_with("rd32 0x00001050 "),
    ];
    let (mut out, mut at) = (Vec::new(), Vec::new());
    let mut i = 0;
    while i < lines.len() {
        let is_empty = (0..4).all(|k| lines.get(i + k).is_some_and(|l| empty[k](l)))
            && !lines.get(i + 4).is_some_and(|l| l.starts_with("mwr32"));
        if is_empty {
            i += 4;
            continue;
        }
        out.push(lines[i].clone());
        at.push(i);
        i += 1;
    }
    (out, at)
}

// bringup-steps.mjs dhcpFrame: a DHCP discover as Ethernet frame (test help, not driver)
pub(super) fn dhcp_frame(op: u8, src: &[u8; 6], dst: &[u8; 6], mac: &[u8; 6], xid: u32) -> Vec<u8> {
    let mut bootp = vec![0u8; 240];
    bootp[0] = op;
    bootp[1] = 1;
    bootp[2] = 6;
    bootp[4..8].copy_from_slice(&xid.to_be_bytes());
    bootp[10..12].copy_from_slice(&0x8000u16.to_be_bytes()); // broadcast flag
    bootp[28..34].copy_from_slice(mac);
    bootp[236..240].copy_from_slice(&0x6382_5363u32.to_be_bytes()); // magic cookie
    let opts = [53, 1, if op == 1 { 1 } else { 2 }, 55, 3, 1, 3, 6, 255];
    let udp_len = (8 + bootp.len() + opts.len()) as u16;
    let mut udp = vec![0u8; 8];
    let (sp, dp): (u16, u16) = if op == 1 { (68, 67) } else { (67, 68) };
    udp[0..2].copy_from_slice(&sp.to_be_bytes());
    udp[2..4].copy_from_slice(&dp.to_be_bytes());
    udp[4..6].copy_from_slice(&udp_len.to_be_bytes());
    let mut ip = vec![0u8; 20];
    ip[0] = 0x45;
    ip[2..4].copy_from_slice(&(20 + udp_len).to_be_bytes());
    ip[8] = 64;
    ip[9] = 17;
    if op != 1 {
        ip[12..16].copy_from_slice(&[192, 168, 0, 1]);
    }
    ip[16..20].copy_from_slice(&[255; 4]);
    let mut sum: u32 = ip
        .chunks(2)
        .map(|c| u32::from(u16::from_be_bytes([c[0], c[1]])))
        .sum();
    while sum > 0xffff {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    ip[10..12].copy_from_slice(&(!(sum as u16)).to_be_bytes());
    let mut eth = dst.to_vec();
    eth.extend_from_slice(src);
    eth.extend_from_slice(&[0x08, 0x00]);
    [eth, ip, udp, bootp, opts.to_vec()].concat()
}

/// The yiaddr of a DHCP offer in an Ethernet frame
pub(super) fn dhcp_offer(eth: &[u8]) -> Option<[u8; 4]> {
    let udp = 14 + 20;
    if eth.len() < udp + 8 + 240
        || u16::from_be_bytes([eth[12], eth[13]]) != 0x0800
        || eth[udp + 3] != 68
    {
        return None;
    }
    let y = &eth[udp + 8 + 16..udp + 8 + 20];
    Some([y[0], y[1], y[2], y[3]])
}

/// What the run left for the checks
struct Run {
    joined: Joined,
    offer: [u8; 4],
    events: Vec<StaEvent>,
    ap_log: String,
    writes: Vec<(usize, u32, Vec<u8>)>,
}

/// Stage chan to the stop with the Rust station (golden 15606-22537)
fn chan_to_stop(fake: &mut Fake, run: &mut Bringup, hooks: GoldenHooks) -> Run {
    let GoldenHooks {
        btc: mut bst,
        mut rfk,
        mut rfk2,
        chan: mut cs,
        ..
    } = hooks;
    bst.take_events();
    cs.take_events();
    let fw = Firmware::parse(firmware(), 1, false).expect("rtw89_fw_recognize");
    rfk2.load_txpwr_trk(&fw, 0).expect("TXPWR_TRK element");
    let own = run.phy.efuse.as_ref().unwrap().addr;
    let mut txrx = Txrx::new();
    let mut sta = Sta::new();
    let mut env = GoldenEnv {
        now: 0,
        rnd: 1,
        real: None,
    };
    rsta::track::track_start(&mut sta, 0); // rtw89_core_start queued it at stage phy
    let mut bus = RecBus {
        inner: &mut fake.bus,
        writes: Vec::new(),
        fail: Fail::None,
    };
    let beacon_filter = fw.has_feat_name("BEACON_FILTER");
    assert!(beacon_filter, "fw 0.29.29.18 has BEACON_FILTER");
    let mut chip = Chip {
        run,
        btc: &mut bst,
        rfk: &mut rfk,
        rfk2: &mut rfk2,
        chan: &mut cs,
        txrx: &mut txrx,
        beacon_filter,
    };

    bus.note("stage chan").unwrap();
    rsta::add_interface(&mut bus, &mut chip, &mut sta, &mut env, own).expect("add_interface");
    assert_eq!(
        chip.mac().rx_fltr,
        macinit::default_rx_fltr(),
        "hal.rx_fltr"
    );

    bus.note("stage scan").unwrap();
    let req = ScanReq::new(Band::B2g, &[1, 6, 11], 20);
    let list = scan::scan(&mut bus, &mut chip, &mut sta, &mut env, &req).expect("scan");
    let mut l = wifi::ScanList::new();
    for b in &list {
        l.add(b.clone());
    }
    let bss = l
        .choose(SSID)
        .expect("the fake AP in the list")
        .expect("usable");
    assert_eq!((bss.ch, bss.dtim_period, bss.beacon_int), (6, Some(3), 100));
    // the best RSSI of its frames (sta-scan.mjs collect); V1's fake gives its broadcast beacon a
    // PPDU status of RSSI 0, which the query turns into +16 dBm (the JS's list says the same)
    assert!(bss.rssi.is_some(), "{list:?}");

    bus.note("stage assoc").unwrap();
    let pmk = wifi::crypto::psk(FAKE_PASS, SSID).unwrap();
    let own_ie = rsn::own_ie(bss.group_cipher());
    let sup = Supplicant::new(pmk, own, bss.bssid, &own_ie, bss.rsn_ie.as_deref()).unwrap();
    let joined = link::connect(
        &mut bus,
        &mut chip,
        &mut sta,
        &mut env,
        &bss,
        sup,
        JoinTiming::default(),
    )
    .unwrap_or_else(|e| panic!("connect: {e}"));

    bus.note("stage dhcp").unwrap();
    let disc = dhcp_frame(1, &own, &[0xff; 6], &own, 0xB175_0001);
    link::send(&mut bus, &mut chip, &mut sta, &mut env, &disc).expect("send");
    let mut offer = None;
    for _ in 0..50 {
        let p = link::poll(&mut bus, &mut chip, &mut sta, &mut env).expect("poll");
        if let Some(y) = p.eth.iter().find_map(|e| dhcp_offer(e)) {
            offer = Some(y);
            break;
        }
        env.now += 20;
    }
    let offer = offer.expect("the DHCP offer");

    bus.note("stop").unwrap();
    link::disconnect(
        &mut bus,
        &mut chip,
        &mut sta,
        &mut env,
        reason::DEAUTH_LEAVING,
    )
    .expect("disconnect");
    rsta::remove_interface(&mut bus, &mut chip, &mut sta, &mut env).expect("remove_interface");
    rsta::track::track_stop(&mut sta);
    // rtw89_core_stop: the coexistence off (WL-2 btc), the flushes, then Bringup's tail
    // (rtw89_hci_deinit, the power-off, rtw89_hci_reset, bus master off)
    {
        let rings = chip.run.rings.as_mut().unwrap();
        let mut link = H2cLink {
            fw: &mut chip.run.fw,
            rings,
        };
        btc::radio_off(&mut bus, chip.btc, &mut link).expect("btc radio off");
        btc::power_off(&mut bus, chip.btc, &mut link).expect("btc power off");
    }
    rsta::ops::hci_flush_queues(&mut bus, &mut chip, &mut sta).expect("flush");
    rsta::ops::mac_flush_txq(&mut bus, &mut sta).expect("flush txq");
    chip.run.stop_hci(&mut bus).expect("the stop's tail");
    let writes = std::mem::take(&mut bus.writes);
    let ap_log = fake.ask("ap-log");
    Run {
        joined,
        offer,
        events: sta.take_events(),
        ap_log,
        writes,
    }
}

/// Every buffer the JS run kicked from stage chan on against the Rust run's last write to the
/// same page before the same golden line; returns how many
fn check_buffers(recs: &[Rec], writes: &[(usize, u32, Vec<u8>)], at: &[usize]) -> usize {
    // the golden line (0-based) of a raw trace index: the first kept line at or after it
    let norm_of = |raw: usize| at.partition_point(|&a| a < raw);
    let last_write = |page: u32, before_line: usize| {
        writes
            .iter()
            .rev()
            .find(|(t, a, _)| *a == page && norm_of(*t) < before_line)
            .map(|w| w.2.clone())
    };
    let mut checked = 0;
    for r in recs.iter().filter(|r| r.line > PHY_LINES) {
        let ours = last_write(r.page, r.line - 1).unwrap_or_else(|| {
            panic!(
                "no Rust write of {} buffer {:#x} before line {}",
                r.ring, r.page, r.line
            )
        });
        assert!(
            ours.len() >= r.bytes.len(),
            "line {}: {} bytes, JS {}",
            r.line,
            ours.len(),
            r.bytes.len()
        );
        let mut ours = ours[..r.bytes.len()].to_vec();
        let mut js = r.bytes.clone();
        if r.ring == "CH12" {
            // WD body (24), H2C header (8), payload: the SEC_CAM H2C carries the key (random per run)
            let hdr = u32::from_le_bytes(js[24..28].try_into().unwrap());
            if hdr & 0x3 == H2C_CAT_MAC && (hdr >> 2) & 0x3f == H2C_CL_MAC_SEC_CAM {
                ours[24 + 8 + 8..24 + 8 + 24].fill(0);
                js[24 + 8 + 8..24 + 8 + 24].fill(0);
            }
            assert_eq!(hex(&ours), hex(&js), "H2C at golden line {}", r.line);
        } else {
            assert_eq!(
                hex(&ours),
                hex(&js),
                "{} WD page at golden line {}",
                r.ring,
                r.line
            );
            let jf = r.frame.as_ref().unwrap();
            let wd0 = u32::from_le_bytes(ours[0..4].try_into().unwrap());
            let info = if wd0 & RTW89_TXWD_BODY0_WD_INFO_EN != 0 {
                24
            } else {
                0
            };
            let a = 24 + info + 8;
            let addr = u32::from_le_bytes(ours[a + 4..a + 8].try_into().unwrap());
            let f = last_write(addr, r.line - 1)
                .unwrap_or_else(|| panic!("no frame for line {}", r.line));
            let mut f = f[..jf.len().min(f.len())].to_vec();
            let mut jf = jf.clone();
            assert_eq!(f.len(), jf.len(), "frame length at golden line {}", r.line);
            if f.len() > 32 + 97 && u16::from_be_bytes([jf[30], jf[31]]) == 0x888e {
                // EAPOL: the version byte (1 as wpa_supplicant, the JS echoed 2), nonce, MIC
                for x in [&mut f, &mut jf] {
                    x[32] = 0;
                    x[32 + 17..32 + 49].fill(0);
                    x[32 + 81..32 + 97].fill(0);
                }
            }
            assert_eq!(
                hex(&f),
                hex(&jf),
                "{} frame at golden line {}",
                r.ring,
                r.line
            );
        }
        checked += 1;
    }
    checked
}

#[test]
fn whole_bringup_trace_is_golden() {
    if skip("whole_bringup_trace_is_golden") {
        return;
    }
    let gold = golden();
    assert_eq!(gold.len(), ALL_LINES);
    for (l, want) in [
        (PHY_LINES + 1, "# stage chan"),
        (SCAN, "# stage scan"),
        (ASSOC, "# stage assoc"),
        (DHCP, "# stage dhcp"),
        (STOP, "# stop"),
    ] {
        assert_eq!(gold[l - 1], want);
    }
    assert_eq!(
        gold[STOP_HCI - 1],
        "rmw32 0x00000074 0x00000020 0x00000000",
        "rtw89_pci_ops_deinit starts the stop's tail"
    );
    let recs = reference();
    let (mut fake, mut run, hooks) = run_to_phy_stage_with("whole", &["--sta"]);
    check_golden(&fake.bus.lines(), &gold, "stages fw to phy");
    assert_eq!(fake.bus.lines().len(), PHY_LINES);
    let r = chan_to_stop(&mut fake, &mut run, hooks);
    let raw = fake.bus.lines();
    let (ours, at) = normalised(&raw);
    check_golden(
        &ours,
        &gold,
        "the Rust station's trace (empty RX polls left out)",
    );
    assert_eq!(
        ours.len(),
        ALL_LINES,
        "the whole bring-up, nothing replayed"
    );
    eprintln!(
        "stages chan to stop: {} golden lines from Rust, none replayed ({} empty RX polls left out)",
        ALL_LINES - PHY_LINES,
        (raw.len() - ours.len()) / 4
    );
    let checked = check_buffers(&recs, &r.writes, &at);
    eprintln!("{checked} kicked buffers (H2Cs and frames) equal the JS run's");
    assert!(checked >= 60, "{checked} kicked buffers compared");
    // what the station did, as the fake AP saw it and as the station says
    assert_eq!(
        (r.joined.aid, r.joined.ch, r.joined.group_key),
        (1, 6, true)
    );
    assert_eq!(ieee::mac_str(&r.joined.bssid), "02:aa:bb:cc:dd:06");
    assert_eq!(
        r.offer,
        [192, 168, 0, 23],
        "the DHCP offer through the pairwise key"
    );
    for want in [
        "auth", "assoc", "msg1", "msg2 ok", "msg3", "msg4 ok", "deauth 3",
    ] {
        assert!(
            r.ap_log.contains(want),
            "the fake AP saw {want}: {}",
            r.ap_log
        );
    }
    let names: Vec<String> = r.events.iter().map(|e| e.to_string()).collect();
    for want in [
        "authenticated",
        "associated, AID 1",
        "pairwise key in",
        "group key 1 in",
        "connected, AID 1, group key in",
    ] {
        assert!(
            names.iter().any(|n| n == want),
            "event {want:?} missing: {names:?}"
        );
    }
    let seen = strip_index(&fake.finish());
    let (seen, _) = normalised(&seen);
    check_golden(&seen, &gold, "the fake's trace");
    assert_eq!(seen.len(), ALL_LINES, "the fake saw the whole bring-up");
}
