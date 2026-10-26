//! The join as a state machine (wifi::mlme) against a scripted AP: WZ:js/join-selftest.mjs's
//! three scenarios (the whole join, a deauthentication after the association request, no
//! answer), then the cases sta-testlink.mjs drives through the JS station (refused
//! authentication, deauthentication during the handshake, handshake timeout, group rekey, the
//! GTK in force again, the AP ending the link), and the supervisor's rhythm.

use wifi::crypto;
use wifi::eapol::{Random, Supplicant};
use wifi::events::{LinkLoss, WifiEvent};
use wifi::mlme::{Action, JoinFailure, JoinTiming, Mgmt, Mlme, Phase, reason};
use wifi::peer::{Authenticator, RSN_CCMP_PSK};
use wifi::secret::Secret;
use wifi::supervisor::{Rhythm, Supervisor, SvState};

const SPA: [u8; 6] = [0x6e, 0x05, 0xd3, 0xd5, 0x30, 0xf1];
const AA: [u8; 6] = [0x74, 0x42, 0x7f, 0xac, 0xf7, 0x34];
const BC: [u8; 6] = [0xff; 6];

struct Fixed(u8);
impl Random for Fixed {
    fn fill(&mut self, out: &mut [u8]) -> bool {
        out.fill(self.0);
        true
    }
}

/// join-selftest.mjs's AP: wlan-ref.mjs's Authenticator with a test PMK, ANonce 0xA7..., GTK
/// 0x61..., key ID 1, counter 1
fn new_ap() -> Authenticator {
    Authenticator::new(Secret::new([0x5a; 32]), AA, SPA, [0xa7; 32], &[0x61; 16], 1)
}

fn start(now: u64) -> (Mlme, Vec<Action>) {
    let sup = Supplicant::new(
        Secret::new([0x5a; 32]),
        SPA,
        AA,
        &RSN_CCMP_PSK,
        Some(&RSN_CCMP_PSK),
    )
    .unwrap();
    Mlme::start(SPA, AA, sup, JoinTiming::default(), now)
}

fn auth_ok() -> Mgmt {
    Mgmt::Auth {
        a1: SPA,
        a2: AA,
        alg: 0,
        seq: 2,
        status: 0,
    }
}
fn assoc_ok() -> Mgmt {
    Mgmt::AssocResp {
        a1: SPA,
        a2: AA,
        status: 0,
        aid: 0xc001,
        capability: 0x0431,
    }
}

fn names(a: &[Action]) -> Vec<String> {
    a.iter()
        .map(|a| match a {
            Action::SendAuth { attempt } => format!("auth {attempt}"),
            Action::SendAssoc { attempt } => format!("assoc {attempt}"),
            Action::Associated { aid, capability } => format!("associated {aid} {capability:#x}"),
            Action::SendEapol(_) => "eapol".into(),
            Action::InstallPtk(_) => "ptk".into(),
            Action::InstallGtk(g) => format!("gtk {}", g.id),
            Action::Connected { aid, group_key } => format!("connected {aid} {group_key}"),
            Action::Failed { why, deauth } => format!("failed: {why} ({deauth:?})"),
            Action::Ended { reason, disassoc } => format!("ended {reason} {disassoc}"),
        })
        .collect()
}

fn eapol_out(a: &[Action]) -> Vec<Vec<u8>> {
    a.iter()
        .filter_map(|a| match a {
            Action::SendEapol(f) => Some(f.clone()),
            _ => None,
        })
        .collect()
}

/// The whole join with the authenticator: returns the MLME connected and the AP with its keys
fn joined() -> (Mlme, Authenticator, u64) {
    let mut ap = new_ap();
    let (mut m, a) = start(0);
    assert_eq!(names(&a), ["auth 1"]);
    let mut t = 5;
    assert_eq!(names(&m.frame(&auth_ok(), t)), ["assoc 1"]);
    t += 6;
    assert_eq!(names(&m.frame(&assoc_ok(), t)), ["associated 1 0x431"]);
    assert_eq!((m.phase(), m.aid()), (Phase::Handshake, 1));
    t += 13;
    let a = m.eapol(&ap.message1(), &mut Fixed(0x3c));
    assert_eq!(names(&a), ["eapol"]);
    assert!(
        ap.message2(&eapol_out(&a)[0]),
        "message 2: right MIC for the AP"
    );
    assert!(m.poll_done(t).is_empty(), "no keys yet");
    t += 16;
    let a = m.eapol(&ap.message3(&[]), &mut Fixed(0));
    // wpa_supplicant's order: 4/4 out, then the PTK, then the GTK
    assert_eq!(names(&a), ["eapol", "ptk", "gtk 1"]);
    assert!(
        ap.message4_ok(&eapol_out(&a)[0]),
        "message 4: right for the AP"
    );
    match &a[1] {
        Action::InstallPtk(tk) => assert!(tk.ct_eq(ap.tk().unwrap()), "the TK is the AP's"),
        _ => unreachable!(),
    }
    assert_eq!(names(&m.poll_done(t)), ["connected 1 true"]);
    assert!(m.authorized());
    (m, ap, t)
}

#[test]
fn whole_join() {
    let (mut m, _, _) = joined();
    let ev: Vec<&str> = m.take_events().iter().map(|e| e.name()).collect();
    assert_eq!(
        ev,
        [
            "auth.sent",
            "auth.ok",
            "assoc.sent",
            "assoc.ok",
            "eapol.taken",
            "eapol.taken",
            "key.ptk",
            "key.gtk",
            "link.up"
        ]
    );
}

#[test]
fn deauth_after_association_request() {
    let (mut m, _) = start(0);
    m.frame(&auth_ok(), 5);
    // join-selftest: the AP deauthenticates (reason 2), then again (reason 9)
    let a = m.frame(
        &Mgmt::Deauth {
            a1: SPA,
            a2: AA,
            reason: 2,
        },
        8,
    );
    assert_eq!(
        names(&a),
        ["failed: deauthenticated by the AP during association, reason 2 (None)"]
    );
    assert_eq!(m.phase(), Phase::Down);
    assert!(
        m.frame(
            &Mgmt::Deauth {
                a1: SPA,
                a2: AA,
                reason: 9
            },
            9
        )
        .is_empty(),
        "nothing after the end"
    );
}

#[test]
fn no_answer() {
    let (mut m, _) = start(0);
    let mut sent = 1;
    let mut t = 0;
    let fail = loop {
        t += 10;
        let a = m.poll_done(t);
        if a.is_empty() {
            continue;
        }
        match &a[0] {
            Action::SendAuth { attempt } => {
                sent += 1;
                assert_eq!(*attempt, sent);
            }
            Action::Failed { why, deauth } => break (*why, *deauth),
            _ => unreachable!(),
        }
    };
    assert_eq!(fail, (JoinFailure::AuthTimeout, None));
    assert_eq!(sent, 3, "IEEE80211_AUTH_MAX_TRIES");
    assert!(t >= 600, "three waits of 200 ms: {t}");
    // at least three polls per try even when the clock jumps (an MCP round trip is slow)
    let (mut m, _) = start(0);
    assert!(m.poll_done(1000).is_empty() && m.poll_done(1000).is_empty());
    assert_eq!(names(&m.poll_done(1000)), ["auth 2"]);
}

#[test]
fn refusals_and_timeouts() {
    let (mut m, _) = start(0);
    let a = m.frame(
        &Mgmt::Auth {
            a1: SPA,
            a2: AA,
            alg: 0,
            seq: 2,
            status: 1,
        },
        1,
    );
    assert_eq!(
        names(&a),
        ["failed: authentication refused, status 1 (None)"]
    );
    // frames not for this join change nothing: another BSS, another station, seq 1
    let (mut m, _) = start(0);
    for f in [
        Mgmt::Auth {
            a1: SPA,
            a2: [2; 6],
            alg: 0,
            seq: 2,
            status: 0,
        },
        Mgmt::Auth {
            a1: [2; 6],
            a2: AA,
            alg: 0,
            seq: 2,
            status: 0,
        },
        Mgmt::Auth {
            a1: SPA,
            a2: AA,
            alg: 0,
            seq: 1,
            status: 0,
        },
        Mgmt::AssocResp {
            a1: SPA,
            a2: AA,
            status: 0,
            aid: 1,
            capability: 0,
        },
        Mgmt::Deauth {
            a1: SPA,
            a2: [2; 6],
            reason: 1,
        },
    ] {
        assert!(m.frame(&f, 1).is_empty(), "{f:?}");
    }
    assert_eq!(m.phase(), Phase::Auth);
    m.frame(&auth_ok(), 2);
    let a = m.frame(
        &Mgmt::AssocResp {
            a1: SPA,
            a2: AA,
            status: 17,
            aid: 0,
            capability: 0,
        },
        3,
    );
    assert_eq!(names(&a), ["failed: association refused, status 17 (None)"]);
    // no message 1 within 8 s: 4-way handshake timeout, our deauthentication with reason 15
    let (mut m, _) = start(0);
    m.frame(&auth_ok(), 1);
    m.frame(&assoc_ok(), 2);
    assert!(m.poll_done(8001).is_empty());
    let a = m.poll_done(8002);
    assert_eq!(
        names(&a),
        [format!(
            "failed: 4-way handshake timed out (Some({}))",
            reason::FOURWAY_TIMEOUT
        )]
    );
    // the AP deauthenticates during the handshake: the link ends at once, no frame of ours
    let mut ap = new_ap();
    let (mut m, _) = start(0);
    m.frame(&auth_ok(), 1);
    m.frame(&assoc_ok(), 2);
    let a = m.eapol(&ap.message1(), &mut Fixed(1));
    assert!(ap.message2(&eapol_out(&a)[0]));
    let a = m.frame(
        &Mgmt::Deauth {
            a1: SPA,
            a2: AA,
            reason: 2,
        },
        3,
    );
    assert_eq!(
        names(&a),
        ["failed: deauthenticated by the AP during the handshake, reason 2 (None)"]
    );
    // the context is closed with it: a later message 3 is refused
    assert!(m.eapol(&ap.message3(&[]), &mut Fixed(1)).is_empty());
    // a message 3 the station drops (a GTK KDE of 33 bytes): no keys, the same timeout
    let mut ap = new_ap();
    ap.gtk = vec![0x61; 33];
    let (mut m, _) = start(0);
    m.frame(&auth_ok(), 1);
    m.frame(&assoc_ok(), 2);
    let a = m.eapol(&ap.message1(), &mut Fixed(2));
    assert!(ap.message2(&eapol_out(&a)[0]));
    assert!(m.eapol(&ap.message3(&[]), &mut Fixed(2)).is_empty());
    assert!(!m.ptk_in());
    assert_eq!(
        names(&m.poll_done(9000)),
        ["failed: 4-way handshake timed out (Some(15))"]
    );
}

#[test]
fn group_rekey_and_the_ap_ending_the_link() {
    let (mut m, mut ap, mut t) = joined();
    m.take_events();
    // group 1/2: the new GTK, then 2/2 out (wpa_supplicant_process_1_of_2)
    t += 1000;
    let g1 = ap.group1(&[0x62; 16], 2);
    let a = m.eapol(&g1, &mut Fixed(0));
    assert_eq!(names(&a), ["gtk 2", "eapol"]);
    assert!(ap.group2_ok(&eapol_out(&a)[0]));
    // the same group 1/2 again: dropped (replay), nothing sent
    assert!(m.eapol(&g1, &mut Fixed(0)).is_empty());
    // the GTK in force with a new counter: answered, not installed again (KRACK guard)
    let a = m.eapol(&ap.group1(&[0x62; 16], 2), &mut Fixed(0));
    assert_eq!(names(&a), ["eapol"]);
    let ev: Vec<String> = m.take_events().iter().map(|e| e.to_string()).collect();
    assert_eq!(
        ev,
        [
            "EAPOL group message 1 taken",
            "group key 2 in",
            "EAPOL frame dropped: replay",
            "EAPOL group message 1 taken",
            "group key 2 in force came again: kept"
        ]
    );
    // a broadcast deauthentication of our AP ends the link: no frame of ours
    let a = m.frame(
        &Mgmt::Deauth {
            a1: BC,
            a2: AA,
            reason: 7,
        },
        t,
    );
    assert_eq!(names(&a), ["ended 7 false"]);
    assert_eq!(m.phase(), Phase::Down);
    assert!(
        m.eapol(&ap.group1(&[0x63; 16], 1), &mut Fixed(0))
            .is_empty(),
        "context closed"
    );
    // a disassociation ends it the same way
    let (mut m, _, t) = joined();
    let a = m.frame(
        &Mgmt::Disassoc {
            a1: SPA,
            a2: AA,
            reason: 8,
        },
        t,
    );
    assert_eq!(names(&a), ["ended 8 true"]);
}

#[test]
fn no_key_byte_in_events_or_debug() {
    let (mut m, _, _) = joined();
    let shown = format!("{m:?} {:?}", m.take_events());
    for k in [[0x5a; 4], [0x61; 4], [0xa7; 4]] {
        assert!(!shown.contains(&format!("{k:?}")[1..12]), "{shown}");
    }
    let pmk = crypto::psk(b"fake-mode passphrase", b"BitsOS-Fake").unwrap();
    assert_eq!(format!("{pmk:?}"), "<secret 32 bytes>");
}

#[test]
fn supervisor_rhythm() {
    let r = Rhythm::default();
    assert_eq!(
        (1..=10).map(|n| r.delay(n)).collect::<Vec<_>>(),
        [
            2000, 10000, 10000, 10000, 10000, 10000, 10000, 60000, 60000, 60000
        ]
    );
    let mut s = Supervisor::default();
    assert!(!s.tick(0), "off: no join");
    s.on(100);
    assert!(s.tick(100), "switched on: the first join at once");
    assert_eq!(s.state(), SvState::Joining);
    s.joined();
    assert_eq!(s.state(), SvState::Up);
    s.link_lost(1000, LinkLoss::BeaconLoss);
    assert_eq!(s.next_wake(), Some(3000), "2 s after the loss");
    assert!(!s.tick(2999) && s.tick(3000));
    let mut now = 3000;
    let mut waits = Vec::new();
    for _ in 0..9 {
        s.join_failed(now);
        let w = s.next_wake().unwrap();
        waits.push(w - now);
        now = w;
        assert!(s.tick(now));
    }
    assert_eq!(
        waits,
        [
            10000, 10000, 10000, 10000, 10000, 10000, 60000, 60000, 60000
        ]
    );
    s.joined();
    s.link_lost(now, LinkLoss::EndedByAp { reason: 2 });
    assert_eq!(
        s.next_wake(),
        Some(now + 2000),
        "a new loss counts from the start"
    );
    s.off();
    assert_eq!((s.state(), s.next_wake()), (SvState::Off, None));
    assert!(!s.tick(now + 1_000_000));
    s.link_lost(now, LinkLoss::ChipFailed);
    assert_eq!(s.state(), SvState::Off, "off stays off");
    let ev: Vec<String> = s.take_events().iter().map(|e| e.to_string()).collect();
    assert_eq!(ev[0], "joining again (try 1)");
    assert!(ev.contains(&"joining again in 2000 ms (try 1): no beacons from the AP".to_string()));
    assert!(ev.contains(&"joining again in 60000 ms (try 8): the join failed".to_string()));
    assert_eq!(ev.last().unwrap(), "WLAN off");
    assert!(matches!(WifiEvent::Off, WifiEvent::Off));
}
