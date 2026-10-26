//! The supervisor when the chip fails (WL-3 fixer): on V1's fake chip and its scripted AP, the
//! interface joins, takes a group rekey, then the chip stops answering while the link is up. The
//! link must end locally (no keys, no AP station, the handshake context wiped), the supervisor
//! must plan the rejoin by its rhythm (2 s, then 10 s six times, then 60 s) and log the error
//! (B6), and the interface joins again once the chip answers. Then a beacon loss whose leaving
//! frame cannot go out, and WLAN off with the chip gone: the keys go in any case.
//!
//! Over the whole run, no log line, error or Debug output shows the passphrase, the PMK or a key
//! the station gave the chip (R8i).

use rtw89_core::phy::Band;
use rtw89_core::sta::LinkState;
use rtw89_core::sta::supervise::{Step, Wlan, WlanCfg};
use rtw89_core::tables::regs::*;
use wifi::events::LinkLoss;
use wifi::mlme::JoinTiming;
use wifi::supervisor::SvState;

use super::life::Life;
use super::sta::{FAKE_PASS, Fail, SSID, hex};
use super::*;

/// One turn of the driver thread; what it logged goes to `seen`
fn turn(l: &mut Life, w: &mut Wlan, seen: &mut Vec<String>) -> Step {
    let step = l
        .go(|b, c, s, e| w.step(b, c, s, e))
        .expect("a step never fails: errors end the link");
    for e in l.sta.take_events() {
        seen.push(format!("{} {e} {e:?}", e.name()));
    }
    seen.push(format!("{step:?}"));
    step
}

/// Turns until the supervisor joined (the clock jumps to its next wake when it sleeps)
fn until_joined(l: &mut Life, w: &mut Wlan, seen: &mut Vec<String>, what: &str) {
    for _ in 0..50 {
        match turn(l, w, seen) {
            Step::Joined(j) => {
                assert_eq!((j.aid, j.group_key), (1, true), "{what}");
                return;
            }
            Step::Idle => l.env.now = w.next_wake().unwrap_or(l.env.now + 10),
            Step::JoinFailed(e) => panic!("{what}: join failed: {e}"),
            s => panic!("{what}: {s:?}"),
        }
    }
    panic!("{what}: not joined");
}

fn assert_link_gone(l: &Life, what: &str) {
    let s = &l.sta;
    assert_eq!(
        s.state,
        LinkState::Up,
        "{what}: the interface is up, unlinked"
    );
    assert!(s.ap.is_none(), "{what}: no AP station");
    assert!(
        s.keys.ptk.is_none() && s.keys.gtk.is_empty() && s.dev.cam.sec_in_use() == 0,
        "{what}: no key in the station or the security CAM"
    );
    assert!(
        s.mlme
            .as_ref()
            .is_some_and(|m| m.phase() == wifi::Phase::Down),
        "{what}: the handshake context closed"
    );
}

#[test]
fn supervisor_after_chip_errors_on_the_fake_ap() {
    if skip("supervisor_after_chip_errors_on_the_fake_ap") {
        return;
    }
    let mut l = Life::new("fault");
    let m0 = l.mark();
    let own = l.own();
    l.go(|b, c, s, e| rtw89_core::sta::add_interface(b, c, s, e, own))
        .unwrap();
    let pmk = wifi::crypto::psk(FAKE_PASS, SSID).unwrap();
    let pmk_bytes = pmk.expose().to_vec();
    let cfg = WlanCfg {
        ssid: SSID.to_vec(),
        pmk,
        band: Band::B2g,
        channels: vec![1, 6, 11],
        dwell_ms: 20,
        timing: JoinTiming::default(),
    };
    let mut w = Wlan::new(cfg);
    let mut seen = Vec::new();
    w.sup.on(l.env.now);
    until_joined(&mut l, &mut w, &mut seen, "the first join");
    // a group rekey while linked
    l.ap("rekey 64646464646464646464646464646464");
    for _ in 0..5 {
        turn(&mut l, &mut w, &mut seen);
    }
    assert_eq!(l.ap_log().last().unwrap(), "group2 ok");
    assert!(l.sta.keys.gtk(2).is_some());

    // the chip stops answering while the link is up: the poll fails, the link ends here
    l.chip_fail = Fail::All;
    let t = l.env.now;
    assert_eq!(
        turn(&mut l, &mut w, &mut seen),
        Step::LinkLost(LinkLoss::ChipFailed)
    );
    assert_link_gone(&l, "after the chip error");
    assert_eq!(w.sup.state(), SvState::Waiting { until: t + 2000 });
    assert!(
        seen.iter()
            .any(|s| s.starts_with("chip.error link poll: chip error")),
        "the error logged (B6): {seen:?}"
    );
    // while the chip stays gone, every join fails and the next comes by the rhythm
    let mut delays = Vec::new();
    for _ in 0..9 {
        let wake = w.next_wake().expect("a join planned");
        l.env.now = wake - 1;
        assert_eq!(turn(&mut l, &mut w, &mut seen), Step::Idle, "not before");
        l.env.now = wake;
        match turn(&mut l, &mut w, &mut seen) {
            Step::JoinFailed(_) => {}
            s => panic!("a join with the chip gone: {s:?}"),
        }
        delays.push(w.next_wake().unwrap() - l.env.now);
    }
    let mut want = vec![10_000; 6];
    want.extend([60_000, 60_000, 60_000]);
    assert_eq!(delays, want, "the rhythm after a loss: 2 s, 10 s x6, 60 s");
    assert!(
        seen.iter()
            .any(|s| s.contains("joining again in 60000 ms (try 9)")),
        "{seen:?}"
    );
    // the chip answers again: the next join by the rhythm goes through
    l.chip_fail = Fail::None;
    l.env.now = w.next_wake().unwrap();
    until_joined(
        &mut l,
        &mut w,
        &mut seen,
        "the join after the chip came back",
    );
    assert!(l.sta.keys.ptk.is_some() && !l.sta.keys.gtk.is_empty());
    for _ in 0..3 {
        assert!(matches!(turn(&mut l, &mut w, &mut seen), Step::Frames(_)));
    }

    // the firmware reports a beacon loss and our leaving frame cannot go out: the link ends
    // here all the same and the supervisor plans the rejoin
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
        hex(&b)
    };
    l.ap(&format!("c2h {loss}"));
    let mut lost = None;
    for _ in 0..5 {
        // the poll reads the report; from then on every write fails
        let s = turn(&mut l, &mut w, &mut seen);
        if l.sta.beacon_loss {
            l.chip_fail = Fail::Writes;
            lost = Some(turn(&mut l, &mut w, &mut seen));
            break;
        }
        if let Step::LinkLost(_) = s {
            lost = Some(s);
            break;
        }
    }
    let lost = lost.expect("the beacon loss seen");
    l.chip_fail = Fail::None;
    assert_eq!(lost, Step::LinkLost(LinkLoss::BeaconLoss));
    for e in l.sta.take_events() {
        seen.push(format!("{} {e} {e:?}", e.name()));
    }
    assert!(
        seen.iter()
            .any(|s| s.starts_with("chip.error teardown: chip error")),
        "the failed leaving logged: {seen:?}"
    );
    assert_link_gone(&l, "after the beacon loss");
    assert!(matches!(w.sup.state(), SvState::Waiting { .. }));
    until_joined(&mut l, &mut w, &mut seen, "the join after the beacon loss");

    // WLAN off with the chip gone: an error, but no key stays and no join follows
    l.chip_fail = Fail::All;
    let off = l.go(|b, c, s, e| w.off(b, c, s, e));
    assert!(off.is_err(), "the leaving frame could not go out");
    seen.push(format!("{off:?}"));
    for e in l.sta.take_events() {
        seen.push(format!("{} {e} {e:?}", e.name()));
    }
    assert_link_gone(&l, "after WLAN off");
    assert_eq!(w.sup.state(), SvState::Off);
    assert_eq!(w.next_wake(), None);

    // R8i: nothing of the passphrase, the PMK or a key the chip got shows anywhere
    let keys = l.sec_keys(m0);
    assert!(keys.len() >= 4, "PTK and GTKs of two joins: {keys:?}");
    seen.push(format!("{:?}", l.sta));
    seen.push(format!("{w:?}"));
    let mut secrets: Vec<Vec<u8>> = keys
        .iter()
        .map(|k| {
            (0..k.len() / 2)
                .map(|i| u8::from_str_radix(&k[2 * i..2 * i + 2], 16).unwrap())
                .collect()
        })
        .collect();
    secrets.push(pmk_bytes);
    for s in &seen {
        assert!(!s.contains("fake-mode"), "the passphrase in {s:?}");
        for k in &secrets {
            for shown in [
                hex(k),
                hex(k).to_uppercase(),
                format!("{:?}", &k[..8]),
                format!("{:x?}", &k[..8]),
                format!("{:02X?}", &k[..8]),
            ] {
                assert!(!s.contains(&shown), "a key ({shown}) in {s:?}");
            }
        }
    }
    let _ = l.fake.finish();
}
