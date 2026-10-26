//! The handshake cases of V1's wlan-eapol-cases.mjs against the Rust supplicant, with the
//! authenticator of wlan-ref.mjs (wifi::peer) as the AP:
//! 1. the hashcat capture (hashcat.net example hash for mode 22000, passphrase "hashcat!", SSID
//!    "TP-LINK_HASHCAT_TEST"): given its ANonce and SNonce, BitsOS must send the very frame the
//!    real client sent, MIC included; then message 3 and a group update of that session;
//! 2. random sessions (GTKs of 16 and 32 bytes, other elements in the key data, a repeated
//!    message 1, beacons with and without an RSN element, padding behind the frames);
//! 3. frames to drop (each must leave the state exactly as it was), the KRACK guard on the GTK,
//!    a group message between messages 1 and 3 of a new handshake, and (changed from V1, as
//!    wpa_supplicant) a retransmitted message 3: answered, the PTK in force kept.
//!
//! Then the callback contract js/test-sta.mjs checks on wpa.mjs.

use wifi::crypto::{self, Pmk};
use wifi::eapol::{self, Accepted, Bad, Got, Random, Supplicant, ki};
use wifi::peer::{self, Authenticator, RSN_CCMP_PSK};
use wifi::secret::Secret;

fn hex(s: &str) -> Vec<u8> {
    let s: String = s.chars().filter(|c| !c.is_whitespace()).collect();
    (0..s.len() / 2)
        .map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).unwrap())
        .collect()
}

/// mulberry32 as wlan-crypto-test.mjs: the same inputs on every run
struct Rnd(u32);
impl Rnd {
    fn next(&mut self, n: u32) -> u32 {
        self.0 = self.0.wrapping_add(0x6D2B79F5);
        let mut t = (self.0 ^ (self.0 >> 15)).wrapping_mul(1 | self.0);
        t = (t.wrapping_add((t ^ (t >> 7)).wrapping_mul(61 | t))) ^ t;
        (((t ^ (t >> 14)) as u64 * u64::from(n)) >> 32) as u32
    }
    fn bytes(&mut self, n: usize) -> Vec<u8> {
        (0..n).map(|_| self.next(256) as u8).collect()
    }
    fn arr<const N: usize>(&mut self) -> [u8; N] {
        self.bytes(N).try_into().unwrap()
    }
}

/// No randomness: every SNonce in these cases is set beforehand
struct NoRandom;
impl Random for NoRandom {
    fn fill(&mut self, _: &mut [u8]) -> bool {
        false
    }
}

fn take(s: &mut Supplicant, f: &[u8], what: &str) -> Accepted {
    s.receive(f, &mut NoRandom)
        .unwrap_or_else(|e| panic!("{what}: dropped ({e})"))
}

fn drop_ok(s: &mut Supplicant, f: &[u8], want: Bad, what: &str) {
    let before = s.fingerprint();
    assert_eq!(s.receive(f, &mut NoRandom).err(), Some(want), "{what}");
    assert_eq!(s.fingerprint(), before, "{what}: the state as it was");
}

/// The frame with `change` applied, its MIC made anew under `kck` (or not)
fn modf(f: &[u8], kck: Option<&Secret<16>>, change: impl FnOnce(&mut Vec<u8>)) -> Vec<u8> {
    let mut g = f.to_vec();
    change(&mut g);
    if let Some(k) = kck {
        g[eapol::OFF_MIC..eapol::OFF_MIC + 16].fill(0);
        let m = crypto::eapol_mic(k, &g);
        g[eapol::OFF_MIC..eapol::OFF_MIC + 16].copy_from_slice(&m);
    }
    g
}

fn info(f: &[u8], kck: &Secret<16>, change: impl FnOnce(u16) -> u16) -> Vec<u8> {
    modf(f, Some(kck), |g| {
        let v = change(u16::from_be_bytes([g[5], g[6]]));
        g[5..7].copy_from_slice(&v.to_be_bytes());
    })
}

/// The station's answer to message 1 as the authenticator checks it, and the PTK it then holds
fn m2(ap: &mut Authenticator, answer: &[u8], snonce: &[u8; 32], own_ie: &[u8]) {
    assert!(
        ap.message2(answer),
        "message 2: right MIC and fields for the AP"
    );
    assert_eq!(answer[0], 1, "version 1 as wpa_supplicant sends");
    assert_eq!(&answer[eapol::OFF_NONCE..eapol::OFF_NONCE + 32], snonce);
    assert_eq!(
        &answer[eapol::KEY_HDR_LEN..],
        own_ie,
        "our RSN element as key data"
    );
    assert_eq!(
        &answer[eapol::OFF_KEY_LEN..eapol::OFF_KEY_LEN + 2],
        &[0, 0],
        "key length 0"
    );
}

fn ctx(pmk: Pmk, spa: [u8; 6], aa: [u8; 6], own: &[u8], ap_ie: Option<&[u8]>) -> Supplicant {
    Supplicant::new(pmk, spa, aa, own, ap_ie).unwrap()
}

#[test]
fn hashcat_capture_gives_the_same_message_2() {
    let ssid = b"TP-LINK_HASHCAT_TEST";
    let aa: [u8; 6] = hex("6466b38ec3fc").try_into().unwrap();
    let spa: [u8; 6] = hex("225edc49b7aa").try_into().unwrap();
    let anonce: [u8; 32] = hex("10e3be3b005a629e89de088d6a2fdc489db83ad4764f2d186b9cde15446e972e")
        .try_into()
        .unwrap();
    let mut message2 = hex(&format!(
        "{}{}{}",
        "0103007502010a0000000000000000000148ce2ccba9c1fda130ff2fbbfb4fd3b063d1a93920b0f7df54a5cbf787b16171",
        "0".repeat(96),
        "001630140100000fac040100000fac040100000fac028000"
    ));
    message2[81..97].copy_from_slice(&hex("024022795224bffca545276c3762686f"));
    let pmk = crypto::psk(b"hashcat!", ssid).unwrap();
    let own_ie = message2[99..].to_vec();
    let mut s = ctx(pmk.duplicate(), spa, aa, &own_ie, None);
    s.set_snonce(message2[17..49].try_into().unwrap());
    let mut r = Rnd(7);
    let gtk = r.bytes(16);
    let mut ap = Authenticator::new(pmk, aa, spa, anonce, &gtk, 1);
    let mut rsc = r.bytes(6);
    rsc.extend_from_slice(&[0, 0]);
    ap.rsc = rsc.try_into().unwrap();
    let a = take(
        &mut s,
        &ap.message1(),
        "message 1 with the ANonce of the capture",
    );
    assert_eq!(a.got, Got::Msg1);
    assert_eq!(
        a.answer, message2,
        "message 2 is the captured frame of a real client byte for byte, its MIC too"
    );
    assert!(ap.message2(&message2));
    let a = take(
        &mut s,
        &ap.message3(&[]),
        "message 3 of the capture's session",
    );
    assert_eq!(a.got, Got::Msg3);
    assert!(ap.message4_ok(&a.answer), "message 4 right for the AP");
    assert!(a.ptk.unwrap().ct_eq(ap.tk().unwrap()), "the TK is the AP's");
    let g = a.gtk.unwrap();
    assert_eq!((g.key.expose(), g.id, g.rsc), (&gtk[..], 1, ap.rsc));
    let new = r.bytes(16);
    let a = take(
        &mut s,
        &ap.group1(&new, 2),
        "a group key update of the capture's session",
    );
    assert!(ap.group2_ok(&a.answer));
    let g = a.gtk.unwrap();
    assert_eq!((g.key.expose(), g.id), (&new[..], 2));
}

#[test]
fn random_sessions() {
    let mut r = Rnd(1);
    let ies: [Option<Vec<u8>>; 3] = [
        Some(RSN_CCMP_PSK.to_vec()),
        Some(hex("30180100000fac040100000fac040200000fac02000fac088c00")),
        None,
    ];
    for i in 0..12 {
        let ap_ie = ies[i % 3].clone();
        let pmk: [u8; 32] = r.arr();
        let (spa, aa): ([u8; 6], [u8; 6]) = (r.arr(), r.arr());
        let gtk_len = if r.next(3) != 0 { 16 } else { 32 };
        let gtk = r.bytes(gtk_len);
        let gtk_id = 1 + r.next(3) as u8;
        let anonce: [u8; 32] = r.arr();
        let mut ap = Authenticator::new(Secret::new(pmk), aa, spa, anonce, &gtk, gtk_id);
        ap.ap_ie = ap_ie.clone().unwrap_or_else(|| RSN_CCMP_PSK.to_vec());
        ap.tx = i % 4 == 1;
        ap.rsc = r.arr();
        ap.replay = 1 + u64::from(r.next(1000));
        let own = wifi::rsn::own_ie(wifi::rsn::cipher::CCMP);
        let mut s = ctx(Secret::new(pmk), spa, aa, &own, ap_ie.as_deref());
        let snonce: [u8; 32] = r.arr();
        s.set_snonce(snonce);
        let pad = |r: &mut Rnd, f: Vec<u8>| -> Vec<u8> {
            let n = if r.next(3) != 0 {
                0
            } else {
                1 + r.next(20) as usize
            };
            let mut f = f;
            f.extend(r.bytes(n));
            f
        };
        let a = take(
            &mut s,
            &pad(&mut r, ap.message1()),
            "message 1 of random sessions",
        );
        if i % 2 == 1 {
            // the AP repeats message 1 with a new ANonce: same SNonce, new keys
            ap.replay += 1 + u64::from(r.next(5));
            ap.anonce = r.arr();
            let b = take(
                &mut s,
                &ap.message1(),
                "a repeated message 1 with a new ANonce",
            );
            assert_ne!(a.answer, b.answer);
            m2(&mut ap, &b.answer, &snonce, &own);
        } else {
            m2(&mut ap, &a.answer, &snonce, &own);
        }
        let ie_len = ap.ap_ie.len();
        let full = 256 - ie_len - 8 - gtk_len - 2;
        let mut big = vec![0xdd, full as u8, 0x00, 0x50, 0xf2];
        big.extend(r.bytes(full - 3));
        let extras = [
            Vec::new(),
            hex(&format!(
                "dd1c000fac090400{}{}",
                "00".repeat(6),
                "aa".repeat(16)
            )),
            hex("dd050050f20401"),
            hex("7f0400000001"),
            big,
        ];
        let extra = &extras[if i % 6 == 5 { 4 } else { i % 4 }];
        let m3 = pad(&mut r, ap.message3(extra));
        let a = take(
            &mut s,
            &m3,
            "message 3 of random sessions (IGTK KDE, vendor, unknown, 256 bytes of key data)",
        );
        assert_eq!(a.got, Got::Msg3);
        assert!(ap.message4_ok(&a.answer));
        assert!(a.ptk.unwrap().ct_eq(ap.tk().unwrap()));
        let g = a.gtk.unwrap();
        assert_eq!(
            (g.key.expose(), g.id, g.tx, g.rsc),
            (&gtk[..], gtk_id, i % 4 == 1, ap.rsc)
        );
        for _ in 0..1 + (i % 3) {
            let n = if r.next(3) != 0 { 16 } else { 32 };
            let new = r.bytes(n);
            let id = r.next(4) as u8;
            let a = take(
                &mut s,
                &pad(&mut r, ap.group1(&new, id)),
                "group key updates of random sessions",
            );
            assert_eq!(a.got, Got::Group);
            assert!(ap.group2_ok(&a.answer));
            let g = a.gtk.unwrap();
            assert_eq!((g.key.expose(), g.id), (&new[..], id));
        }
    }
}

#[test]
fn frames_to_drop_and_the_krack_guard() {
    let mut r = Rnd(3);
    let pmk: [u8; 32] = r.arr();
    let (spa, aa): ([u8; 6], [u8; 6]) = (r.arr(), r.arr());
    let gtk0 = r.bytes(16);
    let anonce: [u8; 32] = r.arr();
    let mut ap = Authenticator::new(Secret::new(pmk), aa, spa, anonce, &gtk0, 1);
    ap.rsc = r.arr();
    ap.replay = 5;
    let own = RSN_CCMP_PSK.to_vec();
    let mut s = ctx(Secret::new(pmk), spa, aa, &own, Some(&RSN_CCMP_PSK));
    let snonce: [u8; 32] = r.arr();
    s.set_snonce(snonce);
    let junk_kck = Secret::new(r.arr::<16>());
    let loose = |info: u16, key_len: u16, data: &[u8]| {
        eapol::build_frame(
            &eapol::KeyFields {
                version: 2,
                info,
                key_len,
                replay: 9u64.to_be_bytes(),
                nonce: None,
                rsc: [0; 8],
                data,
            },
            Some(&junk_kck),
        )
    };
    let d24 = r.bytes(24);
    drop_ok(
        &mut s,
        &loose(
            ki::VER_AES | ki::ACK | ki::MIC | ki::SECURE | ki::ENCRYPTED,
            0,
            &d24,
        ),
        Bad::State,
        "a group message before any handshake",
    );
    drop_ok(
        &mut s,
        &loose(0x13ca, 16, &d24),
        Bad::State,
        "message 3 before message 1",
    );
    let a = take(
        &mut s,
        &ap.message1(),
        "message 1 before the frames to drop",
    );
    m2(&mut ap, &a.answer, &snonce, &own);
    let good = ap.message3(&[]);
    let kck = ap.ptk.as_ref().unwrap().kck.duplicate();
    let kek = ap.ptk.as_ref().unwrap().kek.duplicate();
    let k = Some(&kck);
    let m3 = |ap: &Authenticator,
              plain: Option<&[u8]>,
              key: Option<&Secret<16>>,
              raw: Option<Vec<u8>>| {
        let data = raw.unwrap_or_else(|| {
            let p = plain.unwrap();
            let mut out = vec![0u8; p.len() + 8];
            assert!(crypto::aes_wrap(key.unwrap_or(&kek), p, &mut out));
            out
        });
        eapol::build_frame(
            &eapol::KeyFields {
                version: 2,
                info: 0x13ca,
                key_len: 16,
                replay: ap.replay.to_be_bytes(),
                nonce: Some(&ap.anonce),
                rsc: ap.rsc,
                data: &data,
            },
            Some(&kck),
        )
    };
    let kd = |parts: &[&[u8]]| eapol::pad_key_data(parts.concat());
    let gtk16 = r.bytes(16);
    let kde = eapol::gtk_kde(&gtk16, 1, false);
    let other_kek = Secret::new(r.arr::<16>());
    let bit = |r: &mut Rnd, lo: usize, n: usize| (lo + r.next(n as u32) as usize, 1u8 << r.next(8));
    let (mi, mb) = bit(&mut r, 81, 16);
    let (ri, rb) = bit(&mut r, 65, 8);
    let (di, db) = bit(&mut r, 99, good.len() - 99);
    let many = vec![0x7fu8; 240];
    // V1 dropped 272 bytes of key data here (its 256-byte bound); the key data is now bounded
    // by the frame only (long_key_data_and_rsn_elements_are_taken); the draw stays so the later
    // random values are V1's
    let _ = r.bytes(216);
    let mut stray = Vec::new();
    stray.extend_from_slice(&RSN_CCMP_PSK);
    stray.extend_from_slice(&kde);
    stray.extend(hex("7f07000000000000007f"));
    let tkip_ie = hex("30140100000fac020100000fac040100000fac020000");
    let new_anonce = r.bytes(32);
    let drops: Vec<(Vec<u8>, Bad, &str)> = vec![
        (good[..98].to_vec(), Bad::Frame, "a frame of 98 bytes"),
        (
            modf(&good, None, |g| g[1] = 0),
            Bad::Frame,
            "an EAPOL frame that is no key frame",
        ),
        (
            good[..good.len() - 1].to_vec(),
            Bad::Frame,
            "a frame shorter than its body length says",
        ),
        (
            modf(&good, k, |g| g[2..4].copy_from_slice(&94u16.to_be_bytes())),
            Bad::Frame,
            "a body length below 95",
        ),
        (
            modf(&good, k, |g| {
                let v = u16::from_be_bytes([g[97], g[98]]) + 1;
                g[97..99].copy_from_slice(&v.to_be_bytes())
            }),
            Bad::Frame,
            "key data past the body",
        ),
        (
            modf(&good, k, |g| g[4] = 254),
            Bad::Kind,
            "descriptor 254 (WPA)",
        ),
        (
            info(&good, &kck, |v| v ^ 3),
            Bad::Kind,
            "key descriptor version 1 (HMAC-MD5, TKIP)",
        ),
        (
            info(&good, &kck, |v| v ^ 1),
            Bad::Kind,
            "key descriptor version 3 (AES-CMAC, AKM 6)",
        ),
        (info(&good, &kck, |v| v ^ ki::ACK), Bad::Kind, "no Key Ack"),
        (
            info(&good, &kck, |v| v | ki::REQUEST),
            Bad::Kind,
            "a Request bit",
        ),
        (
            info(&good, &kck, |v| v | ki::ERROR),
            Bad::Kind,
            "an Error bit",
        ),
        (info(&good, &kck, |v| v | ki::SMK), Bad::Kind, "an SMK bit"),
        (
            info(&good, &kck, |v| v | 0x10),
            Bad::Kind,
            "a pairwise frame with a key index",
        ),
        (
            info(&good, &kck, |v| v ^ ki::INSTALL),
            Bad::Kind,
            "message 3 without Install",
        ),
        (
            info(&good, &kck, |v| v ^ ki::SECURE),
            Bad::Kind,
            "message 3 without Secure",
        ),
        (
            info(&good, &kck, |v| v ^ ki::ENCRYPTED),
            Bad::Kind,
            "message 3 without Encrypted Key Data",
        ),
        (
            modf(&good, k, |g| g[7..9].copy_from_slice(&32u16.to_be_bytes())),
            Bad::Kind,
            "message 3 with a key length of 32 (TKIP)",
        ),
        (
            modf(&good, k, |g| g[17..49].copy_from_slice(&new_anonce)),
            Bad::Nonce,
            "message 3 with another ANonce than message 1",
        ),
        (
            modf(&good, None, |g| g[mi] ^= mb),
            Bad::Mic,
            "message 3 with a flipped bit in its MIC",
        ),
        (
            modf(&good, None, |g| g[ri] ^= rb),
            Bad::Mic,
            "message 3 with a flipped bit in its RSC",
        ),
        (
            modf(&good, None, |g| g[di] ^= db),
            Bad::Mic,
            "message 3 with a flipped bit in its key data",
        ),
        (
            modf(&good, None, |g| g[0] ^= 3),
            Bad::Mic,
            "message 3 with another EAPOL version byte",
        ),
        (
            m3(
                &ap,
                Some(&kd(&[&RSN_CCMP_PSK, &kde])),
                Some(&other_kek),
                None,
            ),
            Bad::KeyData,
            "message 3 wrapped with another KEK",
        ),
        (
            m3(&ap, None, None, Some(r.bytes(16))),
            Bad::KeyData,
            "message 3 with 16 bytes of key data",
        ),
        (
            m3(&ap, None, None, Some(r.bytes(36))),
            Bad::KeyData,
            "message 3 with 36 bytes of key data (no multiple of 8)",
        ),
        (
            m3(&ap, Some(&kd(&[&RSN_CCMP_PSK, &kde, &many])), None, None),
            Bad::KeyData,
            "message 3 with more than 264 bytes of key data, an element past the end",
        ),
        (
            m3(&ap, Some(&kd(&[&RSN_CCMP_PSK])), None, None),
            Bad::KeyData,
            "message 3 without a GTK",
        ),
        (
            m3(
                &ap,
                Some(&kd(&[
                    &RSN_CCMP_PSK,
                    &eapol::gtk_kde(&r.bytes(33), 1, false),
                ])),
                None,
                None,
            ),
            Bad::KeyData,
            "message 3 with a GTK of 33 bytes",
        ),
        (
            m3(
                &ap,
                Some(&kd(&[&RSN_CCMP_PSK, &kde, &hex("7f3000")])),
                None,
                None,
            ),
            Bad::KeyData,
            "message 3 with an element running past the end",
        ),
        (
            m3(&ap, Some(&stray), None, None),
            Bad::KeyData,
            "message 3 whose key data ends in one stray byte (not 0xDD)",
        ),
        (
            m3(&ap, Some(&kd(&[&tkip_ie, &kde])), None, None),
            Bad::Ie,
            "message 3 with another RSN element than the beacon (TKIP group)",
        ),
        (
            m3(&ap, Some(&kd(&[&kde])), None, None),
            Bad::Ie,
            "message 3 without an RSN element",
        ),
    ];
    for (f, why, what) in &drops {
        drop_ok(&mut s, f, *why, what);
    }
    let a = take(&mut s, &good, "message 3 after all those dropped");
    assert!(ap.message4_ok(&a.answer));
    assert!(a.ptk.is_some());
    assert_eq!(a.gtk.as_ref().unwrap().key.expose(), &gtk0[..]);
    let g1_gtk = r.bytes(16);
    let g1 = ap.group1(&g1_gtk, 1);
    let later: Vec<(Vec<u8>, Bad, &str)> = vec![
        (
            good.clone(),
            Bad::Replay,
            "message 3 again (its counter did not go up)",
        ),
        (
            modf(&g1, k, |g| {
                let c = u64::from_be_bytes(g[9..17].try_into().unwrap()) - 1;
                g[9..17].copy_from_slice(&c.to_be_bytes())
            }),
            Bad::Replay,
            "a group message with the counter of message 3",
        ),
        (
            modf(&g1, None, |g| g[81] ^= 0x80),
            Bad::Mic,
            "a group message with a flipped bit in its MIC",
        ),
        (
            info(&g1, &kck, |v| v ^ ki::SECURE),
            Bad::Kind,
            "a group message without Secure",
        ),
        (
            info(&g1, &kck, |v| v ^ ki::MIC),
            Bad::Kind,
            "a group message without MIC",
        ),
        (
            info(&g1, &kck, |v| v | ki::INSTALL),
            Bad::Kind,
            "a group message with Install",
        ),
    ];
    for (f, why, what) in &later {
        drop_ok(&mut s, f, *why, what);
    }
    let no_gtk = {
        let p = kd(&[&RSN_CCMP_PSK]);
        let mut out = vec![0u8; p.len() + 8];
        crypto::aes_wrap(&kek, &p, &mut out);
        eapol::build_frame(
            &eapol::KeyFields {
                version: 2,
                info: 0x1382,
                key_len: 0,
                replay: ap.replay.to_be_bytes(),
                nonce: None,
                rsc: [0; 8],
                data: &out,
            },
            Some(&kck),
        )
    };
    drop_ok(
        &mut s,
        &no_gtk,
        Bad::KeyData,
        "a group message without a GTK",
    );
    let a = take(&mut s, &g1, "a group message after those dropped");
    assert!(ap.group2_ok(&a.answer) && a.gtk.is_some());
    // the AP repeats the GTK in force (its group message 2 got lost; KRACK holds such a repeat
    // back): answered, nothing new to install, its counter stays
    ap.rsc = [0; 8];
    let in_force = ap.gtk.clone();
    let a = take(
        &mut s,
        &ap.group1(&in_force, ap.gtk_id),
        "the GTK in force again with a lower RSC",
    );
    assert!(ap.group2_ok(&a.answer));
    assert!(a.gtk.is_none(), "the GTK in force is not installed again");
    ap.rsc = r.arr();
    let new = r.bytes(16);
    let a = take(
        &mut s,
        &ap.group1(&new, ap.gtk_id),
        "a new GTK under the same key ID",
    );
    assert!(a.gtk.is_some());
    let mut near = ap.gtk.clone();
    near[15] ^= 1;
    assert!(
        take(
            &mut s,
            &ap.group1(&near, ap.gtk_id),
            "a GTK differing in its last byte"
        )
        .gtk
        .is_some()
    );
    let mut longer = ap.gtk.clone();
    longer.push(0);
    assert!(
        take(
            &mut s,
            &ap.group1(&longer, ap.gtk_id),
            "the GTK in force and a zero byte more"
        )
        .gtk
        .is_some()
    );
    let other_id = (ap.gtk_id + 1) & 3;
    let a = take(
        &mut s,
        &ap.group1(&longer, other_id),
        "the GTK in force under another key ID",
    );
    assert!(
        a.gtk.is_none(),
        "as hostap: key and length decide, the ID is kept"
    );
    assert_eq!(s.gtk_id(), Some(1));
    // a new 4-way handshake under the keys in force; a group message between its messages 1 and 3
    // is taken under the PTK in force, not the tried one
    let snonce2: [u8; 32] = r.arr();
    s.set_snonce(snonce2);
    ap.replay += 1;
    ap.anonce = r.arr();
    let a = take(
        &mut s,
        &ap.message1(),
        "message 1 of a new 4-way handshake under the keys in force",
    );
    let between = r.bytes(16);
    let m1_counter = ap.replay;
    let g = take(
        &mut s,
        &ap.group1(&between, 1),
        "a group message between messages 1 and 3",
    );
    assert!(ap.group2_ok(&g.answer) && g.gtk.is_some());
    let group_counter = ap.replay;
    ap.replay = m1_counter; // message 2 answers message 1's counter
    m2(&mut ap, &a.answer, &snonce2, &own);
    ap.replay = group_counter;
    let m3b = ap.message3(&[]);
    let a = take(&mut s, &m3b, "its message 3 brings the GTK in force");
    assert!(ap.message4_ok(&a.answer));
    assert!(a.ptk.unwrap().ct_eq(ap.tk().unwrap()), "a new PTK");
    assert!(a.gtk.is_none(), "the GTK in force kept");
    // Changed from V1 (BAD_STATE there), as wpa_supplicant: the AP sends message 3 again with a
    // higher counter (our message 4 got lost): message 4 again, the PTK in force kept (KRACK)
    let kck2 = ap.ptk.as_ref().unwrap().kck.duplicate();
    let again = modf(&m3b, Some(&kck2), |g| {
        let c = u64::from_be_bytes(g[9..17].try_into().unwrap()) + 1;
        g[9..17].copy_from_slice(&c.to_be_bytes())
    });
    ap.replay += 1;
    let a = take(
        &mut s,
        &again,
        "message 3 retransmitted with a higher counter",
    );
    assert_eq!(a.got, Got::Msg3);
    assert!(
        ap.message4_ok(&a.answer),
        "message 4 again, for the new counter"
    );
    assert!(a.ptk.is_none() && a.gtk.is_none(), "no key installed again");
    drop_ok(
        &mut s,
        &again,
        Bad::Replay,
        "the same retransmission once more",
    );
}

/// R1: no BitsOS bound on the key data or the RSN elements beyond what the frame and an element
/// can hold (V1 refused RSN elements over 64 bytes and key data over 256 bytes)
#[test]
fn long_key_data_and_rsn_elements_are_taken() {
    let mut r = Rnd(7);
    // a transition-mode AP: CCMP and GCMP-256 pairwise, PSK, SAE, FT-PSK and PSK-SHA256, MFPC,
    // two PMKIDs: 72 bytes
    let mut ap_ie = hex(
        "3046 0100 000fac04 0200 000fac04 000fac09 0400 000fac02 000fac08 000fac04 000fac06 8000 0200",
    );
    ap_ie.extend(r.bytes(32));
    assert_eq!(ap_ie.len(), 72);
    assert_eq!(usize::from(ap_ie[1]) + 2, ap_ie.len());
    let rsn = wifi::rsn::parse(wifi::rsn::element_body(&ap_ie).unwrap()).unwrap();
    assert_eq!(wifi::rsn::usable(Some(&rsn)), Ok(()), "joinable");
    let pmk: [u8; 32] = r.arr();
    let (spa, aa): ([u8; 6], [u8; 6]) = (r.arr(), r.arr());
    let gtk = r.bytes(16);
    let mut ap = Authenticator::new(Secret::new(pmk), aa, spa, r.arr(), &gtk, 1);
    ap.ap_ie = ap_ie.clone();
    let own = wifi::rsn::own_ie(wifi::rsn::cipher::CCMP);
    let mut s = ctx(Secret::new(pmk), spa, aa, &own, Some(&ap_ie));
    let snonce: [u8; 32] = r.arr();
    s.set_snonce(snonce);
    let a = take(&mut s, &ap.message1(), "message 1");
    m2(&mut ap, &a.answer, &snonce, &own);
    // a vendor element of 400 bytes behind the GTK: 528 bytes of key data
    let mut extra = Vec::new();
    for _ in 0..2 {
        extra.extend_from_slice(&[0xdd, 198, 0x00, 0x50, 0xf2, 0x7f]);
        extra.extend(r.bytes(194));
    }
    let m3 = ap.message3(&extra);
    assert!(m3.len() - eapol::KEY_HDR_LEN > 500);
    let a = take(
        &mut s,
        &m3,
        "message 3 with long key data and a long RSN element",
    );
    assert!(ap.message4_ok(&a.answer));
    assert!(a.ptk.is_some());
    assert_eq!(a.gtk.unwrap().key.expose(), &gtk[..]);
    // an element longer than an element can be is no RSN element
    let too_long = vec![0x30u8; eapol::IE_MAX + 1];
    assert!(Supplicant::new(Secret::new(pmk), spa, aa, &own, Some(&too_long)).is_err());
}

#[test]
fn callback_contract_of_test_sta() {
    // js/test-sta.mjs's checks of the eapol callback: not a key frame, 3/4 before 1/4, 3/4 of
    // another SNonce, an old 3/4 at a connected context, a closed context refuses
    let mut r = Rnd(9);
    let pmk: [u8; 32] = r.arr();
    let (spa, aa): ([u8; 6], [u8; 6]) = (
        [0x6c, 5, 0xd3, 0xd5, 0x30, 0xf1],
        [2, 0xaa, 0xbb, 0xcc, 0xdd, 6],
    );
    let own = wifi::rsn::own_ie(wifi::rsn::cipher::CCMP);
    let ap_ie = hex("30140100000fac040100000fac040100000fac020c00");
    let mut ap = Authenticator::new(Secret::new(pmk), aa, spa, r.arr(), &r.bytes(16), 1);
    ap.ap_ie = ap_ie.clone();
    let mut a = ctx(Secret::new(pmk), spa, aa, &own, Some(&ap_ie));
    a.set_snonce(r.arr());
    let msg1 = ap.message1();
    let first = take(&mut a, &msg1, "1/4");
    assert!(ap.message2(&first.answer));
    let msg3 = ap.message3(&[]);
    take(&mut a, &msg3, "3/4");
    let mut n3 = ctx(Secret::new(pmk), spa, aa, &own, Some(&ap_ie));
    n3.set_snonce(r.arr());
    assert_eq!(
        n3.receive(&[0u8; 10], &mut NoRandom).err(),
        Some(Bad::Frame),
        "not an EAPOL-Key frame"
    );
    assert_eq!(
        n3.receive(&msg3, &mut NoRandom).err(),
        Some(Bad::State),
        "3/4 before 1/4"
    );
    let m = take(&mut n3, &msg1, "1/4 answered");
    assert!(m.got == Got::Msg1 && m.ptk.is_none());
    assert_eq!(
        n3.receive(&msg3, &mut NoRandom).err(),
        Some(Bad::Mic),
        "3/4 of another SNonce"
    );
    assert_eq!(
        a.receive(&msg3, &mut NoRandom).err(),
        Some(Bad::Replay),
        "an old 3/4 at the connected context"
    );
    n3.close();
    assert_eq!(
        n3.receive(&msg1, &mut NoRandom).err(),
        Some(Bad::State),
        "a closed context refuses"
    );
    assert!(!n3.has_ptk());
    // without randomness there is no handshake (never a fixed SNonce)
    let mut fresh = ctx(Secret::new(pmk), spa, aa, &own, None);
    assert_eq!(fresh.receive(&msg1, &mut NoRandom).err(), Some(Bad::State));
    // an RSN element longer than any element is refused when the context is made (V1: 65)
    assert!(Supplicant::new(Secret::new(pmk), spa, aa, &[0x30; eapol::IE_MAX + 1], None).is_err());
    assert!(Supplicant::new(Secret::new(pmk), spa, aa, &[0x30; 65], None).is_ok());
    // nothing of a key shows in Debug
    let shown = format!("{a:?}");
    assert!(!shown.contains(&format!("{:?}", &pmk[..4])), "{shown}");
    let _ = peer::mic_ok(&msg3, &Secret::new([0; 16]));
}
