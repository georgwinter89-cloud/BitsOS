//! The driver (WL-4, `rtw89_core::drv`) on V1's fake chip and its scripted AP, as the kernel thread
//! runs it: the bring-up is the golden trace up to stage scan; the Runner joins the configured
//! network, a DHCP discover goes out of its queue and the offer comes back; the status lines;
//! then V1's wlan-robust-test.mjs fault catalogue through fake-host.mjs's `fault` commands
//! (firmware halt, an L0 error the firmware repairs, firmware no longer ready, one dead look, the
//! RXQ index past the ring, a chip reading only ones, a transmit ring that takes nothing), each:
//! one log line, the chip off, up again, joined again; then leave (the CH12 copies of the keys
//! wiped, no rejoin), join, stop. Over the whole run no log line or Debug output shows the
//! passphrase, the PMK or a key the chip got (R8i).

use std::collections::VecDeque;

use rtw89_core::bringup::RingConfig;
use rtw89_core::drv::status::Status;
use rtw89_core::drv::{DevConfig, Device, Runner, TurnOut};
use rtw89_core::phy::Band;
use rtw89_core::sta::supervise::WlanCfg;
use rtw89_core::tables::regs::*;
use wifi::mlme::JoinTiming;
use wifi::supervisor::SvState;

use super::sta::{
    FAKE_PASS, Fail, GoldenEnv, RecBus, SCAN, SSID, dhcp_frame, dhcp_offer, hex, normalised,
};
use super::*;

struct Drv {
    fake: Fake,
    r: Runner,
    env: GoldenEnv,
    tx: VecDeque<Vec<u8>>,
    /// every log line of the run, "name text"
    log: Vec<String>,
    writes: Vec<(usize, u32, Vec<u8>)>,
}

impl Drv {
    fn new(name: &str) -> Drv {
        let fw = Firmware::parse(firmware(), 1, false).expect("rtw89_fw_recognize");
        Drv {
            fake: start(name, &["--sta"]),
            r: Runner::new(
                fw,
                DevConfig {
                    rings: RingConfig::js(),
                    keep_mac: None,
                },
            ),
            env: GoldenEnv {
                now: 0,
                rnd: 3,
                real: None,
            },
            tx: VecDeque::new(),
            log: Vec::new(),
            writes: Vec::new(),
        }
    }

    fn with<R>(
        &mut self,
        f: impl FnOnce(&mut Runner, &mut RecBus, &mut GoldenEnv, &mut VecDeque<Vec<u8>>) -> R,
    ) -> R {
        let mut bus = RecBus {
            inner: &mut self.fake.bus,
            writes: Vec::new(),
            fail: Fail::None,
        };
        let r = f(&mut self.r, &mut bus, &mut self.env, &mut self.tx);
        self.writes.extend(bus.writes);
        for e in self.r.log.drain(..) {
            self.log.push(format!("{} {}", e.name, e.text));
        }
        r
    }

    fn turn(&mut self) -> TurnOut {
        self.with(|r, b, e, t| r.turn(b, e, t))
    }

    /// Turns (the clock jumps to the next wake) until the link is up with the group key
    fn until_link(&mut self, what: &str) {
        for _ in 0..400 {
            let o = self.turn();
            if o.link == Some(true) {
                assert!(self.r.link.is_some_and(|j| j.group_key), "{what}");
                return;
            }
            let next = self.r.next_wake(self.env.now);
            self.env.now = next.max(self.env.now + 1);
        }
        panic!("{what}: no link; log {:#?}", self.log);
    }

    /// Turns over `ms` of the clock, 2 ms apart while linked (the RX poll)
    fn run_for(&mut self, ms: u64) -> Vec<TurnOut> {
        let end = self.env.now + ms;
        let mut outs = Vec::new();
        while self.env.now < end {
            outs.push(self.turn());
            let next = self.r.next_wake(self.env.now).min(end);
            self.env.now = next.max(self.env.now + 1);
        }
        outs
    }

    /// The clock to the next firmware look and one turn
    fn fw_look(&mut self) -> TurnOut {
        self.env.now = self.env.now.max(self.r.health.fw_due);
        self.turn()
    }

    fn since(&self, m: usize) -> Vec<String> {
        self.log[m..].to_vec()
    }

    fn own(&self) -> [u8; 6] {
        self.r.cfg.keep_mac.expect("the first bring-up's MAC")
    }

    fn status(&self) -> Vec<String> {
        Status::of(&self.r).lines("-")
    }
}

fn cfg() -> WlanCfg {
    WlanCfg {
        ssid: SSID.to_vec(),
        pmk: wifi::crypto::psk(FAKE_PASS, SSID).unwrap(),
        band: Band::B2g,
        channels: vec![1, 6, 11],
        dwell_ms: 20,
        timing: JoinTiming::default(),
    }
}

/// The driver's bring-up (Device::bring_up with the driver's own stage hooks) makes the golden
/// trace from the safety step through add_interface: stage fw, mac, phy and chan's first part
#[test]
fn driver_bringup_trace_is_golden() {
    if skip("driver_bringup_trace_is_golden") {
        return;
    }
    let gold = golden();
    let mut fake = start("drv-golden", &["--sta"]);
    let fw = Firmware::parse(firmware(), 1, false).unwrap();
    let cfg = DevConfig {
        rings: RingConfig::js(),
        keep_mac: None,
    };
    let mut env = GoldenEnv {
        now: 0,
        rnd: 1,
        real: None,
    };
    let mut log = Vec::new();
    let dev = {
        let mut bus = RecBus {
            inner: &mut fake.bus,
            writes: Vec::new(),
            fail: Fail::None,
        };
        Device::bring_up(&mut bus, &fw, &cfg, &mut env, &mut log)
    }
    .unwrap_or_else(|e| panic!("bring-up: {e}"));
    let (ours, _) = normalised(&fake.bus.lines());
    check_golden(&ours, &gold, "the driver's bring-up");
    assert_eq!(
        ours.len(),
        SCAN - 1,
        "through add_interface, up to stage scan"
    );
    assert_eq!(dev.mac, dev.efuse_mac);
    assert!(
        log.iter().any(|e| e.name == "fw.loaded"),
        "the firmware's version logged: {log:?}"
    );
    let _ = fake.finish();
}

fn sec_cam_keys(d: &Drv) -> Vec<Vec<u8>> {
    let ch12: Vec<u32> =
        d.r.dev
            .as_ref()
            .and_then(|x| x.run.rings.as_ref())
            .and_then(|r| r.tx("CH12"))
            .map(|r| r.bufs.clone())
            .unwrap_or_default();
    d.writes
        .iter()
        .filter(|(_, a, b)| ch12.contains(a) && b.len() >= 56)
        .filter(|(_, _, b)| {
            let w0 = u32::from_le_bytes(b[24..28].try_into().unwrap());
            w0 & 0x3 == H2C_CAT_MAC && (w0 >> 2) & 0x3f == H2C_CL_MAC_SEC_CAM
        })
        .map(|(_, _, b)| b[32 + 8..32 + 24].to_vec())
        .collect()
}

#[test]
fn driver_life_and_fault_catalogue_on_the_fake() {
    if skip("driver_life_and_fault_catalogue_on_the_fake") {
        return;
    }
    let mut d = Drv::new("drv-life");
    d.r.set_network(Some(cfg()), 0);
    assert_eq!(
        Status::of(&d.r).state,
        "chip_down",
        "nothing up before the first turn"
    );
    d.until_link("the first join");
    let st = d.status();
    assert!(
        st[0].starts_with("state=connected ssid=BitsOS-Fake mac=")
            && st[0].contains("fw=0.29.29.18"),
        "{st:?}"
    );
    assert!(
        st[1].starts_with("bssid=02:aa:bb:cc:dd:06 channel=6 "),
        "{st:?}"
    );
    assert!(st[4].starts_with("joins=1 join_failures=0"), "{st:?}");

    // DHCP through the driver's queue: the discover out, the AP's offer back
    let own = d.own();
    d.tx.push_back(dhcp_frame(1, &own, &[0xff; 6], &own, 0xB175_0001));
    let mut offer = None;
    for o in d.run_for(500) {
        if let Some(y) = o.frames.iter().find_map(|f| dhcp_offer(f)) {
            offer = Some(y);
        }
    }
    assert_eq!(
        offer,
        Some([192, 168, 0, 23]),
        "the offer: {:?}",
        d.status()
    );
    assert!(d.r.stats.tx_frames >= 1 && d.r.stats.rx_frames >= 1);
    // a scan while linked is refused, the link stays
    let s = d.with(|r, b, e, _| r.scan(b, e, &[1, 6, 11], 20));
    assert!(s.is_err(), "{s:?}");
    assert!(d.r.link.is_some());

    // a firmware halt: logged with its code, the chip off and up, joined again
    let m = d.log.len();
    d.fake.bus.sync().unwrap();
    assert_eq!(d.fake.ask("fault halt 20001234"), "ok");
    let o = d.fw_look();
    assert_eq!(o.link, Some(false), "the link went with the chip");
    let seen = d.since(m);
    assert!(
        seen.iter()
            .any(|l| l.starts_with("fw.halted the firmware halted with error 0x20001234")),
        "{seen:?}"
    );
    assert!(
        seen.iter()
            .any(|l| l.starts_with("chip.failed the firmware halted"))
            && seen.iter().any(|l| l.starts_with("chip.up ")),
        "{seen:?}"
    );
    assert_eq!(d.r.stats.restarts, 1);
    d.until_link("after the halt");

    // an error the firmware repaired itself (L0): logged, the link kept, its report taken
    let m = d.log.len();
    d.fake.bus.sync().unwrap();
    assert_eq!(d.fake.ask("fault halt 1"), "ok");
    let o = d.fw_look();
    assert_eq!(o.link, None);
    assert!(
        d.since(m)
            .iter()
            .any(|l| l.starts_with("fw.repaired the firmware repaired error 0x00000001")),
        "{:?}",
        d.since(m)
    );
    assert_eq!((d.r.stats.restarts, d.r.stats.fw_repaired), (1, 1));
    assert!(d.r.link.is_some());
    d.fake.bus.sync().unwrap();
    assert_eq!(d.fake.ask("reg 164"), "reg 0", "HALT_C2H_CTRL back to 0");

    // the firmware no longer at status 7
    let m = d.log.len();
    d.fake.bus.sync().unwrap();
    assert_eq!(d.fake.ask("fault notready"), "ok");
    assert_eq!(d.fw_look().link, Some(false));
    assert!(
        d.since(m).iter().any(|l| l.starts_with(
            "chip.failed the firmware no longer runs (register 0x001e0 read 0x00000002)"
        )),
        "{:?}",
        d.since(m)
    );
    d.until_link("after not ready");

    // one dead look (the RXQ index once all ones): the link poll fails, the chip off and up
    let m = d.log.len();
    d.fake.bus.sync().unwrap();
    assert_eq!(d.fake.ask("fault deadlook"), "ok");
    let o = d.turn();
    assert_eq!(o.link, Some(false), "{:?}", d.since(m));
    assert!(
        d.since(m).iter().any(|l| l.starts_with("chip.failed ")),
        "{:?}",
        d.since(m)
    );
    d.until_link("after one dead look");

    // the RXQ index past the ring for 8 reads
    let m = d.log.len();
    d.fake.bus.sync().unwrap();
    assert_eq!(d.fake.ask("fault rxq-past 8"), "ok");
    d.until_link("after the index past the ring");
    assert!(
        d.since(m).iter().any(|l| l.contains("RingIndex")),
        "the index in the log: {:?}",
        d.since(m)
    );

    // a chip that reads only ones: the dead verdict with the register, bring-ups fail by the
    // rhythm while it stays dead, then up and joined again
    let m = d.log.len();
    let r0 = d.r.stats.restarts;
    d.fake.bus.sync().unwrap();
    assert_eq!(d.fake.ask("fault dead 1"), "ok");
    assert_eq!(d.fw_look().link, Some(false));
    for _ in 0..3 {
        d.env.now = d.r.next_wake(d.env.now);
        d.turn();
    }
    let seen = d.since(m);
    assert!(
        seen.iter()
            .any(|l| l.contains("the chip does not answer (register")),
        "{seen:?}"
    );
    let failed_ups = seen
        .iter()
        .filter(|l| l.starts_with("chip.up_failed"))
        .count();
    assert!(
        failed_ups >= 3,
        "bring-ups by the rhythm while dead: {seen:?}"
    );
    assert!(
        seen.iter()
            .filter(|l| l.contains("does not answer"))
            .count()
            <= 2,
        "said once, not for every access: {seen:?}"
    );
    assert_eq!(Status::of(&d.r).state, "chip_down");
    assert_eq!(d.r.stats.restarts, r0 + 1);
    d.fake.bus.sync().unwrap();
    assert_eq!(d.fake.ask("fault dead 0"), "ok");
    d.until_link("after the chip came back");

    // a transmit ring that takes nothing: frames stay queued, after 10 s the chip off and up
    let m = d.log.len();
    d.fake.bus.sync().unwrap();
    assert_eq!(d.fake.ask("fault txfreeze 1"), "ok");
    let mut went = false;
    for _ in 0..40 {
        for _ in 0..8 {
            d.tx.push_back(dhcp_frame(1, &own, &[0xff; 6], &own, 0xB175_0002));
        }
        if d.turn().link == Some(false) {
            went = true;
            break;
        }
        d.env.now += 500;
    }
    let seen = d.since(m);
    assert!(went, "the stall ended the link: {seen:?}");
    assert!(
        seen.iter()
            .any(|l| l.starts_with("chip.failed the transmit ring took nothing for 10 s")),
        "{seen:?}"
    );
    d.fake.bus.sync().unwrap();
    assert_eq!(d.fake.ask("fault txfreeze 0"), "ok");
    d.until_link("after the stall");

    // leave: no link, no rejoin, the CH12 copies of the keys wiped
    let keys = sec_cam_keys(&d);
    assert!(!keys.is_empty(), "SEC_CAM H2Cs with keys");
    let o = d.with(|r, b, e, _| r.leave(b, e));
    assert_eq!(o.link, Some(false));
    let ch12 =
        d.r.dev
            .as_ref()
            .unwrap()
            .run
            .rings
            .as_ref()
            .unwrap()
            .tx("CH12")
            .unwrap()
            .bufs
            .clone();
    for a in ch12 {
        let mut buf = vec![0u8; 2048];
        d.fake.bus.mem_read_bytes(a, &mut buf).unwrap();
        assert!(buf.iter().all(|&x| x == 0), "CH12 buffer {a:#x} wiped");
    }
    assert_eq!(d.r.wlan.as_ref().unwrap().sup.state(), SvState::Off);
    for o in d.run_for(70_000) {
        assert_eq!(o.link, None, "no rejoin after leave");
    }
    assert_eq!(Status::of(&d.r).state, "idle");
    // a scan without a link: the fake AP
    let list = d
        .with(|r, b, e, _| r.scan(b, e, &[1, 6, 11], 20))
        .expect("scan");
    assert!(list.iter().any(|b| b.ssid == SSID && b.ch == 6), "{list:?}");
    d.r.join(d.env.now).unwrap();
    d.until_link("join after leave");

    // stop: everything off
    let m = d.log.len();
    d.with(|r, b, e, _| r.stop(b, e));
    assert!(d.r.dev.is_none() && d.r.link.is_none());
    assert!(
        d.since(m).iter().any(|l| l == "chip.off chip switched off"),
        "{:?}",
        d.since(m)
    );
    assert_eq!(Status::of(&d.r).state, "off");

    // R8i over the whole run
    let mut seen = d.log.clone();
    seen.push(format!("{:?}", d.r));
    seen.extend(d.status());
    let mut secrets = sec_cam_keys(&d);
    secrets.push(
        wifi::crypto::psk(FAKE_PASS, SSID)
            .unwrap()
            .expose()
            .to_vec(),
    );
    for s in &seen {
        assert!(!s.contains("fake-mode"), "the passphrase in {s:?}");
        for k in &secrets {
            for shown in [
                hex(k),
                hex(k).to_uppercase(),
                format!("{:?}", &k[..8]),
                format!("{:x?}", &k[..8]),
            ] {
                assert!(!s.contains(&shown), "a key ({shown}) in {s:?}");
            }
        }
    }
    let _ = d.fake.finish();
}

/// Turns until `until_ms` of the fake clock; the clock time of every new join failure. No turn
/// may bring a link up.
fn failures_until(d: &mut Drv, until_ms: u64, what: &str) -> Vec<u64> {
    let mut at = Vec::new();
    let mut seen = d.r.stats.join_failures;
    while d.env.now < until_ms {
        let o = d.turn();
        assert_ne!(o.link, Some(true), "{what}: a link came up");
        if d.r.stats.join_failures != seen {
            seen = d.r.stats.join_failures;
            at.push(d.env.now);
        }
        let next = d.r.next_wake(d.env.now);
        d.env.now = next.max(d.env.now + 1);
    }
    at
}

/// The supervisor's rhythm between failed joins (wifi::supervisor::Rhythm): switched on, join 1
/// comes at once, joins 2-7 10 s after the failure before them, then every 60 s (the 2 s first
/// wait is only after a lost link). Each gap: at least the wait, at most that plus one join.
fn assert_rhythm(at: &[u64], what: &str) {
    let rhythm = wifi::supervisor::Rhythm::default();
    assert!(at.len() >= 8, "{what}: only {} failed joins: {at:?}", at.len());
    assert_eq!(rhythm.delay(8), rhythm.long_ms);
    for (i, w) in at.windows(2).enumerate() {
        let gap = w[1] - w[0];
        let want = rhythm.delay(i as u32 + 2);
        assert!(
            gap >= want && gap <= want + 10_000,
            "{what}: gap {i} is {gap} ms, the rhythm waits {want} ms: {at:?}"
        );
    }
}

/// The negative joins of the brief and WL-3's open points: a wrong passphrase (the 4-way handshake
/// fails), an SSID no AP sends; each: no link ever, the failures counted at the supervisor's
/// rhythm, the last error in the status, no chip restart (a failed join is no chip fault), no
/// secret in any text; then leave while the supervisor waits (no further join), and stop.
#[test]
fn driver_join_failures_on_the_fake() {
    if skip("driver_join_failures_on_the_fake") {
        return;
    }
    let wrong: &[u8] = b"not the passphrase of the fake AP";
    let mut d = Drv::new("drv-wrongpsk");
    let mut c = cfg();
    c.pmk = wifi::crypto::psk(wrong, SSID).unwrap();
    d.r.set_network(Some(c), 0);
    let at = failures_until(&mut d, 200_000, "wrong passphrase");
    assert_rhythm(&at, "wrong passphrase");
    assert!(d.r.link.is_none());
    assert_eq!(d.r.stats.restarts, 0, "a failed handshake restarted the chip");
    let st = d.status();
    let last = st.iter().find(|l| l.starts_with("last_error=")).unwrap();
    assert!(last != "last_error=-", "{st:?}");
    // leave while the supervisor waits for the next try: no join from then on
    let n = d.r.stats.join_failures;
    let out = d.with(|r, b, e, _| r.leave(b, e));
    assert_ne!(out.link, Some(true));
    let until = d.env.now + 70_000;
    let none = failures_until(&mut d, until, "after leave");
    assert!(none.is_empty() && d.r.stats.join_failures == n, "a join after leave: {none:?}");
    // stop: bounded (the fake clock), the chip off
    let t0 = d.env.now;
    d.with(|r, b, e, _| r.stop(b, e));
    assert!(d.env.now - t0 < 5_000, "stop took {} ms", d.env.now - t0);
    assert!(d.r.dev.is_none());
    assert_eq!(Status::of(&d.r).state, "off");
    let pmk = wifi::crypto::psk(wrong, SSID).unwrap().expose().to_vec();
    let mut texts = d.log.clone();
    texts.push(format!("{:?}", d.r));
    texts.extend(st);
    for s in &texts {
        assert!(!s.contains("not the passphrase"), "the passphrase in {s:?}");
        assert!(!s.contains(&hex(&pmk)) && !s.contains(&format!("{:?}", &pmk[..8])), "the PMK in {s:?}");
    }
    let _ = d.fake.finish();

    // an SSID no AP sends
    let mut d = Drv::new("drv-noap");
    let mut c = cfg();
    c.ssid = b"NoSuchNetwork".to_vec();
    d.r.set_network(Some(c), 0);
    let at = failures_until(&mut d, 200_000, "no AP");
    assert_rhythm(&at, "no AP");
    assert_eq!(d.r.stats.restarts, 0);
    let st = d.status();
    assert!(
        st.iter().any(|l| l.starts_with("last_error=") && l != "last_error=-"),
        "{st:?}"
    );
    let _ = d.fake.finish();
}
