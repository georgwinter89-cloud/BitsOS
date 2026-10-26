//! Frames from the air never make the station panic (WL-3 fixer): every prefix and single-byte
//! changes of a beacon, an authentication and an association response, a deauthentication and a
//! disassociation go through the station's whole intake (txrx's parser, the scan list entry, the
//! choice of the BSS, the handshake context, wifi's MLME in each join phase). A frame not from
//! our AP to us leaves the MLME as it was.

use alloc::vec;
use alloc::vec::Vec;

use super::link::mgmt_of;
use super::scan::bss_from_frame;
use crate::phy::Band;
use crate::txrx::ieee;

use wifi::eapol::Supplicant;
use wifi::mlme::{JoinTiming, Mgmt, Mlme};
use wifi::scan::ScanList;
use wifi::secret::Secret;
use wifi::{Phase, rsn};

const OWN: [u8; 6] = [0x02, 0xb1, 0x75, 0x00, 0x00, 0x01];
const AP: [u8; 6] = [0x3c, 0xa6, 0x2f, 0x11, 0x22, 0x33];
const SSID: &[u8] = b"FRITZ!Box 7590 XY";

fn hx(s: &str) -> Vec<u8> {
    let s: Vec<u8> = s.bytes().filter(|c| c.is_ascii_hexdigit()).collect();
    s.chunks(2)
        .map(|c| u8::from_str_radix(core::str::from_utf8(c).unwrap(), 16).unwrap())
        .collect()
}

fn cat(parts: &[&[u8]]) -> Vec<u8> {
    parts.concat()
}

/// test-txrx.mjs's FritzBox-like beacon (WPA2/WPA3 transition, PMF capable), without FCS
fn beacon() -> Vec<u8> {
    let mut ssid = vec![0x00, SSID.len() as u8];
    ssid.extend_from_slice(SSID);
    cat(&[
        &ieee::hdr(0x0080, &[0xff; 6], &AP, &AP, 0x10a),
        &hx("0011223344550000 6400 3104"),
        &ssid,
        &hx(
            "01 08 82 84 8b 96 0c 12 18 24   03 01 06   05 04 00 01 00 00
             07 06 44 45 20 01 0d 14   2a 01 00   32 04 30 48 60 6c
             30 1e 0100 000fac04 0100 000fac04 0200 000fac02 000fac08 8c00 0000 000fac06
             2d 1a ad01 17 ffff 0000000000000000000000000000 0000 00000000 00
             3d 16 06 0000000000 00000000000000000000000000000000
             dd 18 0050f2020101 80 00 03a40000 27a40000 42435e00 62322f00",
        ),
    ])
}

fn auth_resp() -> Vec<u8> {
    cat(&[&ieee::hdr(0x00b0, &OWN, &AP, &AP, 1), &hx("0000 0200 0000")])
}

fn assoc_resp() -> Vec<u8> {
    cat(&[
        &ieee::hdr(0x0010, &OWN, &AP, &AP, 2),
        &hx("3104 0000 01c0 01 08 82 84 8b 96 0c 12 18 24 32 04 30 48 60 6c"),
    ])
}

fn deauth() -> Vec<u8> {
    ieee::deauth(&AP, &OWN, 2, 3)
}

fn disassoc() -> Vec<u8> {
    cat(&[&ieee::hdr(0x00a0, &OWN, &AP, &AP, 4), &hx("0800")])
}

/// The frame, every prefix of it, and it with each byte changed in five ways
fn variants(f: &[u8]) -> Vec<Vec<u8>> {
    let mut out: Vec<Vec<u8>> = (0..=f.len()).map(|n| f[..n].to_vec()).collect();
    for i in 0..f.len() {
        for v in [0, 0xff, f[i] ^ 1, f[i] ^ 0x80, f[i].wrapping_add(1)] {
            if v != f[i] {
                let mut g = f.to_vec();
                g[i] = v;
                out.push(g);
            }
        }
    }
    out
}

fn sup(ap_ie: Option<&[u8]>) -> Option<Supplicant> {
    let own = rsn::own_ie(rsn::cipher::CCMP);
    Supplicant::new(Secret::new([7; 32]), OWN, AP, &own, ap_ie).ok()
}

/// An MLME in `phase`, brought there by the AP's good answers
fn mlme_in(phase: Phase) -> Mlme {
    let (mut m, _) = Mlme::start(OWN, AP, sup(None).unwrap(), JoinTiming::default(), 0);
    for (f, p) in [
        (auth_resp(), Phase::Assoc),
        (assoc_resp(), Phase::Handshake),
    ] {
        if m.phase() == phase {
            break;
        }
        let a = m.frame(&mgmt_of(&f).unwrap(), 1);
        assert!(!a.is_empty() && m.phase() == p, "{p:?}");
    }
    assert_eq!(m.phase(), phase);
    m.take_events();
    m
}

fn ours(m: &Mgmt) -> bool {
    let (a1, a2) = match m {
        Mgmt::Auth { a1, a2, .. }
        | Mgmt::AssocResp { a1, a2, .. }
        | Mgmt::Deauth { a1, a2, .. }
        | Mgmt::Disassoc { a1, a2, .. } => (*a1, *a2),
    };
    a2 == AP && (a1 == OWN || a1 == [0xff; 6])
}

#[test]
fn frames_from_the_air_never_panic_the_station() {
    let mut list = ScanList::new();
    let mut seen = 0usize;
    let frames = [beacon(), auth_resp(), assoc_resp(), deauth(), disassoc()];
    for f in &frames {
        for g in variants(f) {
            // the scan's intake: the list entry, its RSN element, the choice, the context
            if let Some(b) = bss_from_frame(&g, 6, Some(-50), Band::B2g) {
                let _ = (b.usable(), b.rsn(), b.group_cipher());
                if let Some(Ok(c)) = list.choose(&b.ssid) {
                    let own = rsn::own_ie(c.group_cipher());
                    let _ = Supplicant::new(
                        Secret::new([1; 32]),
                        OWN,
                        c.bssid,
                        &own,
                        c.rsn_ie.as_deref(),
                    );
                }
                list.add(b);
                seen += 1;
            }
            // the MLME in every phase of a join and on a link
            let Some(mg) = mgmt_of(&g) else { continue };
            for phase in [Phase::Auth, Phase::Assoc, Phase::Handshake] {
                let mut m = mlme_in(phase);
                let a = m.frame(&mg, 2);
                if !ours(&mg) {
                    assert!(
                        a.is_empty() && m.phase() == phase && m.take_events().is_empty(),
                        "a frame not of our AP to us changed the MLME: {mg:?}"
                    );
                }
            }
        }
    }
    assert!(seen > 100, "beacons taken: {seen}");
    assert!(!list.is_empty());
    // the good beacon is in the list and joinable; the list holds every BSSID once
    let good = bss_from_frame(&beacon(), 6, Some(-40), Band::B2g).unwrap();
    assert_eq!(good.usable(), Ok(()));
    let all = list.sorted();
    let mut ids: Vec<_> = all.iter().map(|b| b.bssid).collect();
    ids.sort();
    ids.dedup();
    assert_eq!(ids.len(), all.len(), "one entry per BSSID");
}
