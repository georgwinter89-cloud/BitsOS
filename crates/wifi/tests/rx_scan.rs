//! The receive checks (PN replay per TID, duplicates, the control port), the RSN element and the
//! scan list, against the behaviour of sta-link.mjs / sta-mlme.mjs / sta-scan.mjs and the JS test's
//! expectations (sta-testlink.mjs's RX checks, test-sta.mjs's own RSN element).

use wifi::replay::{
    DataDrop, DataInfo, DupCheck, DupInfo, Keys, MGMT_IDX, NUM_TIDS, RxPn, data_allowed,
};
use wifi::rsn::{self, Unusable, akm, cipher};
use wifi::scan::{Band, Bss, ScanList};

fn hex(s: &str) -> Vec<u8> {
    (0..s.len() / 2)
        .map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).unwrap())
        .collect()
}

#[test]
fn pn_replay_per_tid() {
    // starts at the handshake's RSC: a PN at or below it is a replay (sta-testlink: PN 5 = RSC)
    let mut k = RxPn::new(5);
    assert!(k.replay(0, 5), "PN at the RSC");
    assert!(!k.replay(0, 6));
    assert!(k.replay(0, 6), "the same PN again");
    assert!(!k.replay(3, 6), "another TID has its own counter");
    assert!(!k.replay(0, 8) && k.replay(0, 7), "lower after higher");
    assert!(
        !k.replay(MGMT_IDX, 6),
        "the management counter (PMF) is the 17th"
    );
    assert!(
        k.replay(NUM_TIDS + 1, 100),
        "past the counters: dropped, never a panic"
    );
    assert_eq!(k.replays, 4);
    assert_eq!(
        RxPn::rsc_of(&[1, 2, 3, 4, 5, 6, 0xff, 0xff]),
        0x0605_0403_0201,
        "6 bytes of PN"
    );
}

#[test]
fn duplicates() {
    let mut d = DupCheck::new();
    let f = |retry, seq_ctrl, tid| DupInfo {
        control: false,
        group: false,
        nullfunc: false,
        retry,
        tid,
        seq_ctrl,
    };
    assert!(!d.duplicate(&f(false, 0x10, None)));
    assert!(
        d.duplicate(&f(true, 0x10, None)),
        "retry bit, same sequence control"
    );
    assert!(
        !d.duplicate(&f(false, 0x10, None)),
        "without the retry bit it is new"
    );
    assert!(
        !d.duplicate(&f(true, 0x10, Some(3))),
        "QoS data counts per TID"
    );
    assert!(d.duplicate(&f(true, 0x10, Some(3))));
    let mut g = f(true, 0x10, None);
    g.group = true;
    assert!(
        !d.duplicate(&g),
        "a group-addressed frame is never a duplicate"
    );
    g.group = false;
    g.nullfunc = true;
    assert!(!d.duplicate(&g), "null functions never count");
    assert_eq!(d.dups, 2);
}

struct K {
    ptk: Option<RxPn>,
    gtk: Option<(u8, RxPn)>,
}
impl Keys for K {
    fn rx_pn(&mut self, id: u8) -> Option<&mut RxPn> {
        if id == 0 {
            return self.ptk.as_mut();
        }
        self.gtk.as_mut().filter(|g| g.0 == id).map(|g| &mut g.1)
    }
    fn any(&self) -> bool {
        self.ptk.is_some() || self.gtk.is_some()
    }
}

#[test]
fn data_checks_of_a_station() {
    let mut keys = K {
        ptk: Some(RxPn::new(0)),
        gtk: Some((1, RxPn::new(5))),
    };
    let prot = |key_id, pn, decrypted| DataInfo {
        protected: true,
        decrypted,
        key_id,
        pn,
        sec_idx: 0,
        eapol_to_us: false,
    };
    let plain = DataInfo {
        protected: false,
        decrypted: false,
        key_id: 0,
        pn: 0,
        sec_idx: 0,
        eapol_to_us: false,
    };
    // sta-testlink.mjs: old PN, new, replay, plain, not decrypted
    assert_eq!(
        data_allowed(&prot(1, 5, true), true, &mut keys),
        Err(DataDrop::Replay)
    );
    assert_eq!(data_allowed(&prot(1, 6, true), true, &mut keys), Ok(()));
    assert_eq!(
        data_allowed(&prot(1, 6, true), true, &mut keys),
        Err(DataDrop::Replay)
    );
    assert_eq!(
        data_allowed(&plain, true, &mut keys),
        Err(DataDrop::Unencrypted)
    );
    assert_eq!(
        data_allowed(&prot(1, 7, false), true, &mut keys),
        Err(DataDrop::NotDecrypted)
    );
    assert_eq!(
        data_allowed(&prot(2, 9, true), true, &mut keys),
        Err(DataDrop::NoKey)
    );
    // EAPOL to us passes before the port is open; anything else waits for it
    let eapol = DataInfo {
        eapol_to_us: true,
        ..plain
    };
    let mut none = K {
        ptk: None,
        gtk: None,
    };
    assert_eq!(data_allowed(&eapol, false, &mut none), Ok(()));
    assert_eq!(
        data_allowed(&plain, false, &mut none),
        Err(DataDrop::PortClosed)
    );
    assert_eq!(
        data_allowed(&plain, true, &mut none),
        Ok(()),
        "an open network would pass"
    );
    assert_eq!(DataDrop::PortClosed.as_str(), "port not authorized");
}

#[test]
fn rsn_element() {
    // js/test-sta.mjs: our own RSN element: CCMP, PSK, capabilities 0
    let own = rsn::own_ie(cipher::CCMP);
    assert_eq!(own, hex("30140100000fac040100000fac040100000fac020000"));
    let r = rsn::parse(rsn::element_body(&own).unwrap()).unwrap();
    assert_eq!(
        (r.group, r.pairwise.as_slice(), r.akms.as_slice(), r.caps),
        (cipher::CCMP, &[cipher::CCMP][..], &[akm::PSK][..], 0)
    );
    assert_eq!(rsn::usable(Some(&r)), Ok(()));
    // the fake AP's (capabilities 0x000c: replay counters, no PMF)
    let ap = rsn::parse(&hex("0100000fac040100000fac040100000fac020c00")).unwrap();
    assert!(rsn::usable(Some(&ap)).is_ok() && !ap.mfpr);
    let pmf = rsn::parse(&hex("0100000fac040100000fac040100000fac02c000")).unwrap();
    assert_eq!(rsn::usable(Some(&pmf)), Err(Unusable::PmfRequired));
    let tkip = rsn::parse(&hex("0100000fac020100000fac040100000fac020000")).unwrap();
    assert_eq!(rsn::usable(Some(&tkip)), Err(Unusable::GroupNotCcmp));
    let sae = rsn::parse(&hex("0100000fac040100000fac040100000fac080000")).unwrap();
    assert_eq!(rsn::usable(Some(&sae)), Err(Unusable::NoPsk));
    let gcmp = rsn::parse(&hex("0100000fac040100000fac080100000fac020000")).unwrap();
    assert_eq!(rsn::usable(Some(&gcmp)), Err(Unusable::NoCcmp));
    assert_eq!(rsn::usable(None), Err(Unusable::NoRsn));
    // torn and odd elements never panic: defaults, or "truncated"
    assert_eq!(rsn::parse(&[1]), None);
    assert_eq!(rsn::parse(&[2, 0]).unwrap().bad, Some("version"));
    let torn = rsn::parse(&hex("0100000fac040500000fac04")).unwrap();
    assert_eq!(torn.bad, Some("RSN list truncated"));
    assert_eq!(rsn::usable(Some(&torn)), Err(Unusable::NoRsn));
    let short = rsn::parse(&hex("0100")).unwrap();
    assert_eq!(
        (short.group, short.akms.as_slice()),
        (cipher::CCMP, &[akm::IEEE8021X][..])
    );
    for n in 0..64 {
        let b: Vec<u8> = (0..n).map(|i| (i * 37 + 1) as u8).collect();
        let _ = rsn::parse(&b);
        let _ = rsn::element_body(&b);
    }
    assert!(
        rsn::element_body(&hex("3002")).is_none(),
        "length past the end"
    );
    assert!(
        rsn::element_body(&hex("dd020100")).is_none(),
        "not an RSN element"
    );
}

fn bss(last: u8, rssi: Option<i32>, dtim: Option<u8>, ssid: &[u8], rsn_ie: Option<Vec<u8>>) -> Bss {
    Bss {
        bssid: [2, 0, 0, 0, 0, last],
        ssid: ssid.to_vec(),
        band: Band::B2g,
        ch: 6,
        beacon_int: 100,
        capability: 0x0411,
        rate_bytes: vec![0x82, 0x84, 0x8b, 0x96],
        rsn_ie,
        dtim_period: dtim,
        rssi,
    }
}

#[test]
fn scan_list() {
    let wpa2 = Some(rsn::own_ie(cipher::CCMP));
    let mut l = ScanList::new();
    l.add(bss(1, Some(-60), Some(3), b"home", wpa2.clone()));
    // a probe response of the same BSS: newest wins, but the better RSSI and the beacon's DTIM stay
    let mut pr = bss(1, Some(-70), None, b"home", wpa2.clone());
    pr.ch = 6;
    pr.capability = 0x0431;
    l.add(pr);
    l.add(bss(
        2,
        Some(-40),
        Some(1),
        b"home",
        Some(hex("30140100000fac020100000fac040100000fac020000")),
    ));
    l.add(bss(3, None, None, b"other", None));
    l.add(bss(4, Some(-50), Some(1), b"open", None));
    assert_eq!(l.len(), 4);
    let s = l.sorted();
    assert_eq!(
        s.iter().map(|b| b.bssid[5]).collect::<Vec<_>>(),
        [2, 4, 1, 3],
        "strongest first, no RSSI last"
    );
    let one = s.iter().find(|b| b.bssid[5] == 1).unwrap();
    assert_eq!(
        (one.rssi, one.dtim_period, one.capability),
        (Some(-60), Some(3), 0x0431)
    );
    // choosing: the strongest usable one of that name; the strongest TKIP one is passed over
    assert_eq!(l.choose(b"home").unwrap().unwrap().bssid[5], 1);
    assert_eq!(l.choose(b"open"), Some(Err(Unusable::NoRsn)));
    assert_eq!(l.choose(b"nowhere"), None);
    assert_eq!(one.group_cipher(), cipher::CCMP);
}

/// Never a panic on an RSN element from the air (WL-3 fixer): every prefix and single-byte
/// changes of real elements go through the element check, the parser, the joinability check and
/// the handshake context; what parses keeps its lists inside the element
#[test]
fn hostile_rsn_elements_never_panic() {
    let elements = [
        wifi::peer::RSN_CCMP_PSK.to_vec(),
        rsn::own_ie(cipher::CCMP),
        // FritzBox transition mode (PSK + SAE, MFPC, group management cipher)
        hex("301e0100000fac040100000fac040200000fac02000fac088c000000000fac06"),
        // CCMP/GCMP-256 pairwise, four AKMs, two PMKIDs
        {
            let mut v = hex(
                "30460100000fac040200000fac04000fac090400000fac02000fac08000fac04000fac0680000200",
            );
            v.extend([0x5a; 32]);
            v
        },
    ];
    let mut parsed = 0;
    for e in &elements {
        let mut vs: Vec<Vec<u8>> = (0..=e.len()).map(|n| e[..n].to_vec()).collect();
        for i in 0..e.len() {
            for v in [0, 0xff, e[i] ^ 1, e[i] ^ 0x80, e[i].wrapping_add(1)] {
                let mut g = e.clone();
                g[i] = v;
                vs.push(g);
            }
        }
        for g in vs {
            let body = rsn::element_body(&g);
            // the body alone, as a beacon parser hands it on, and every tail of it
            for b in body.into_iter().chain((0..g.len()).map(|k| &g[k..])) {
                if let Some(r) = rsn::parse(b) {
                    parsed += 1;
                    let _ = rsn::usable(Some(&r));
                    assert!(4 * (r.pairwise.len() + r.akms.len()) <= b.len() + 8);
                }
            }
            let own = rsn::own_ie(cipher::CCMP);
            let s = wifi::eapol::Supplicant::new(
                wifi::secret::Secret::new([3; 32]),
                [2; 6],
                [4; 6],
                &own,
                Some(&g),
            );
            assert!(s.is_ok(), "an element of {} bytes is kept", g.len());
        }
    }
    assert!(parsed > 1000, "{parsed}");
}

/// Management frames with any field values: the MLME takes only what is of our AP to us, and a
/// stray frame changes nothing (state, events, actions)
#[test]
fn hostile_management_fields_leave_the_mlme() {
    use wifi::Phase;
    use wifi::mlme::{JoinTiming, Mgmt, Mlme};
    let own = [2u8; 6];
    let ap = [4u8; 6];
    let mk = || {
        let s = wifi::eapol::Supplicant::new(
            wifi::secret::Secret::new([3; 32]),
            own,
            ap,
            &rsn::own_ie(cipher::CCMP),
            None,
        )
        .unwrap();
        Mlme::start(own, ap, s, JoinTiming::default(), 0).0
    };
    let addrs = [own, ap, [0xff; 6], [0; 6], [2, 2, 2, 2, 2, 3]];
    let vals = [0u16, 1, 2, 3, 0x3fff, 0xc001, 0xffff];
    let mut frames = Vec::new();
    for &a1 in &addrs {
        for &a2 in &addrs {
            for &x in &vals {
                for &y in &vals {
                    frames.push(Mgmt::Auth {
                        a1,
                        a2,
                        alg: x,
                        seq: y,
                        status: x ^ y,
                    });
                    frames.push(Mgmt::AssocResp {
                        a1,
                        a2,
                        status: x,
                        aid: y,
                        capability: x,
                    });
                }
                frames.push(Mgmt::Deauth { a1, a2, reason: x });
                frames.push(Mgmt::Disassoc { a1, a2, reason: x });
            }
        }
    }
    let good_auth = Mgmt::Auth {
        a1: own,
        a2: ap,
        alg: 0,
        seq: 2,
        status: 0,
    };
    let good_assoc = Mgmt::AssocResp {
        a1: own,
        a2: ap,
        status: 0,
        aid: 1,
        capability: 0x431,
    };
    for f in &frames {
        let (a1, a2) = match f {
            Mgmt::Auth { a1, a2, .. }
            | Mgmt::AssocResp { a1, a2, .. }
            | Mgmt::Deauth { a1, a2, .. }
            | Mgmt::Disassoc { a1, a2, .. } => (*a1, *a2),
        };
        let ours = a2 == ap && (a1 == own || a1 == [0xff; 6]);
        for depth in 0..3 {
            let mut m = mk();
            if depth > 0 {
                assert!(!m.frame(&good_auth, 1).is_empty());
            }
            if depth > 1 {
                assert!(!m.frame(&good_assoc, 1).is_empty());
            }
            m.take_events();
            let phase = m.phase();
            let a = m.frame(f, 2);
            if !ours {
                assert!(
                    a.is_empty() && m.phase() == phase && m.events().is_empty(),
                    "{f:?} in {phase:?}"
                );
            }
            if m.phase() == Phase::Handshake {
                assert!(m.aid() <= 0x3fff, "the AID's flag bits are cut");
            }
        }
    }
}
