//! txrx beyond js/test-txrx.mjs: the whole TX way through pcitx on the in-memory chip, the
//! refusals touching nothing, the PPDU queue bound, and inputs from the air and from callers
//! that must never panic.

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use super::data::{CcmpKey, DataTx};
use super::ieee::*;
use super::parse::*;
use super::tests::{AP, Lcg, OWN, Ops, Pp, Rd, beacon, cat, chan, ppdu_item, rxdesc};
use super::*;
use crate::bus::Bus;
use crate::error::Rtw89Error;
use crate::membus::MemBus;
use crate::pci::{RingConfig, fill_txdesc, pci_alloc};
use crate::pcitx::PciTx;
use crate::pcitx::fakechip::TxChip;
use crate::phy::Band;

struct Way {
    bus: MemBus,
    rings: crate::pci::PciRings,
    tx: PciTx,
    chip: TxChip,
    t: Txrx,
}

fn way() -> Way {
    let mut bus = MemBus::new();
    let rings = pci_alloc(&mut bus, &RingConfig::js()).unwrap();
    let chip = TxChip::new(&mut bus, &rings);
    let mut tx = PciTx::new();
    tx.init(&rings);
    bus.trace.clear();
    let mut t = Txrx::new();
    t.chan = chan(Band::B2g, 6);
    Way {
        bus,
        rings,
        tx,
        chip,
        t,
    }
}

fn read(bus: &mut MemBus, at: u32, n: usize) -> Vec<u8> {
    let mut v = vec![0u8; n];
    bus.mem_read_bytes(at, &mut v).unwrap();
    v
}

#[test]
fn a_frame_goes_the_whole_way_and_comes_back_as_a_report() {
    let mut w = way();
    let mut p = ProbeReq::new(OWN);
    p.ch = Some(6);
    let mut f = probe_req(&p);
    let sent = tx_frame(
        &mut w.bus,
        &mut w.t,
        &mut w.tx,
        &mut w.rings,
        &mut f,
        &TxOpts::default(),
    )
    .unwrap();
    let q = sent.queued.expect("queued on CH8");
    assert_eq!((q.ch, q.seq, q.wp, q.kicked), ("CH8", 0, 1, true));
    assert_eq!(le16(&f, 22), 0, "mac80211's first sequence number");
    assert_eq!(w.t.tx.seq_ctrl, 0x10);
    // the WD page: body + info as fill_txdesc builds them, then WP info and address info;
    // the frame behind the page
    let ring = w.rings.tx("CH8").unwrap();
    let page = ring.bufs[0];
    let desc = fill_txdesc(&sent.info.desc());
    assert_eq!(read(&mut w.bus, page, desc.len()), desc);
    assert_eq!(read(&mut w.bus, page + crate::pcitx::WD_PAGE, f.len()), f);
    let lines = w.bus.lines();
    assert_eq!(
        lines.last().map(String::as_str),
        Some("wr16 0x00001078 0x00000000 0x00000001"),
        "the CH8 kick"
    );
    w.chip.run(&mut w.bus, &w.rings);
    let reports = w.tx.reclaim(&mut w.bus, &mut w.rings).unwrap();
    assert_eq!(reports.len(), 1);
    assert_eq!(
        (reports[0].ch, reports[0].wlan_seq, reports[0].acked),
        ("CH8", 0, true)
    );
}

#[test]
fn data_frames_wait_for_ring_room_and_refusals_touch_nothing() {
    let mut w = way();
    // ACH1 (TID 1) has 4 BDs, one always free: three frames, then the resource check says 0
    let eth = cat(&[&AP, &OWN, &[0x08, 0x06], &[0; 28]]);
    let o = TxOpts {
        tid: 1,
        ..TxOpts::default()
    };
    for n in 0..3 {
        let mut f = eth_to_wlan(
            &eth,
            DataTx {
                bssid: AP,
                key: None,
                qos: None,
                seq: 0,
            },
        )
        .unwrap();
        let s = tx_frame(&mut w.bus, &mut w.t, &mut w.tx, &mut w.rings, &mut f, &o).unwrap();
        assert_eq!(s.queued.map(|q| q.ch), Some("ACH1"), "frame {n}");
    }
    let mut f = eth_to_wlan(
        &eth,
        DataTx {
            bssid: AP,
            key: None,
            qos: None,
            seq: 0,
        },
    )
    .unwrap();
    let s = tx_frame(&mut w.bus, &mut w.t, &mut w.tx, &mut w.rings, &mut f, &o).unwrap();
    assert!(
        s.queued.is_none(),
        "ring full: not queued (mac80211 keeps it)"
    );
    // a protected frame without key: refused before any op
    let mut ptk = CcmpKey::new(0, Some(3));
    let mut f = eth_to_wlan(
        &eth,
        DataTx {
            bssid: AP,
            key: Some(&mut ptk),
            qos: None,
            seq: 0,
        },
    )
    .unwrap();
    let before = w.bus.trace.len();
    let e = tx_frame(
        &mut w.bus,
        &mut w.t,
        &mut w.tx,
        &mut w.rings,
        &mut f,
        &TxOpts::default(),
    )
    .unwrap_err();
    assert!(matches!(e, Rtw89Error::Frame { .. }), "{e:?}");
    assert_eq!(w.bus.trace.len(), before, "nothing reached the chip");
    // a key without CAM entry, of another cipher, a 6 GHz band, a torn frame
    let no_cam = TxKey {
        key_idx: 0,
        sec_cam_idx: None,
        cipher: cipher::CCMP,
    };
    let tkip = TxKey {
        sec_cam_idx: Some(1),
        cipher: cipher::TKIP,
        ..no_cam
    };
    for (key, band) in [
        (Some(no_cam), None),
        (Some(tkip), None),
        (Some(TxKey::from(&ptk)), Some(Band::B6g)),
    ] {
        let o = TxOpts {
            key,
            band,
            ..TxOpts::default()
        };
        assert!(tx_info(&mut w.t, None, &f, &o).is_err());
    }
    let mut torn = vec![0x08, 0x00, 0, 0];
    assert!(
        tx_frame(
            &mut w.bus,
            &mut w.t,
            &mut w.tx,
            &mut w.rings,
            &mut torn,
            &TxOpts::default()
        )
        .is_err()
    );
    assert_eq!(w.bus.trace.len(), before, "still nothing");
    // no channel and no band: refused with what is missing
    let mut t = Txrx::new();
    let e = tx_info(&mut t, None, &deauth(&OWN, &AP, 3, 0), &TxOpts::default()).unwrap_err();
    assert!(matches!(e, Rtw89Error::Frame { value: 0, .. }), "{e:?}");
}

#[test]
fn the_ppdu_queue_has_a_bound_against_a_broken_chip() {
    let mut t = Txrx::new();
    t.rx.ppdu_queue_bound = 4;
    let b = beacon();
    let item = |cnt| {
        let d = rxdesc(Rd {
            long: true,
            len: b.len() as u32,
            cnt,
            ..Rd::default()
        });
        cat(&[&d, &b])
    };
    let mut delivered = 0;
    for _ in 0..9 {
        let one = item(1);
        let r = rx_parse(&mut t, RxIn::packet(&one), &mut ());
        assert_eq!(r.pending, Some(b.len()));
        delivered += r.deliver.len();
    }
    assert_eq!((delivered, t.rx.queue.len()), (8, 1), "two flushes of 4");
    assert_eq!(
        t.take_events(),
        [
            RxEvent::PpduQueueFull { flushed: 4 },
            RxEvent::PpduQueueFull { flushed: 4 }
        ]
    );
    // the PPDU status that finally comes takes the one waiting
    let pp = ppdu_item(Pp {
        cnt: 1,
        ..Pp::default()
    });
    let r = rx_parse(&mut t, pp.rx(), &mut Ops::default());
    assert_eq!(r.deliver.len(), 1);
    assert!(t.rx.queue.is_empty());
}

#[test]
fn odd_rx_buffers_are_dropped_with_a_reason() {
    let mut t = Txrx::new();
    let empty = RxIn {
        data: &[],
        fs: true,
        ls: true,
        len: Some(u32::MAX),
    };
    assert_eq!(rx_parse(&mut t, empty, &mut ()).typ, RxType::Dropped);
    // a WRITE_SIZE below the rxbd_info, and one far past the buffer: cut, never read past
    let b = beacon();
    let d = rxdesc(Rd {
        len: b.len() as u32,
        ..Rd::default()
    });
    let whole = cat(&[&d, &b]);
    for len in [0, 3, 4, 20, u32::MAX] {
        let r = rx_parse(
            &mut t,
            RxIn {
                data: &whole,
                fs: true,
                ls: true,
                len: Some(len),
            },
            &mut (),
        );
        assert!(matches!(r.typ, RxType::Frame), "len {len}: {:?}", r.typ);
        let got = r.deliver[0].frame.len();
        assert!(got <= b.len(), "len {len}: {got}");
    }
    // RPKT_LEN 0x3fff with a short buffer: the packet is what is there
    let d = rxdesc(Rd {
        len: 0x3fff,
        ..Rd::default()
    });
    let r = rx_parse(&mut t, RxIn::packet(&cat(&[&d, &b])), &mut ());
    assert_eq!(r.deliver[0].frame.len(), b.len());
    // a long descriptor with drv info past the buffer: an empty packet, no panic
    let mut d = rxdesc(Rd {
        long: true,
        len: 100,
        ..Rd::default()
    });
    d[3] |= 0x70; // DRV_INFO_SIZE 7: 56 bytes of drv info that are not there
    let r = rx_parse(&mut t, RxIn::packet(&d), &mut ());
    assert_eq!(r.pending, Some(0));
}

#[test]
fn random_tx_requests_never_panic() {
    let mut r = Lcg(77);
    let mut t = Txrx::new();
    let bands = [None, Some(Band::B2g), Some(Band::B5g), Some(Band::B6g)];
    for i in 0..5000u32 {
        let n = (r.next() % 80) as usize;
        let mut f: Vec<u8> = (0..n).map(|_| r.next() as u8).collect();
        if n > 2 && i % 3 == 0 {
            f[0] = [0x08, 0x88, 0x48, 0xc8, 0x00, 0x40, 0xb0, 0xd4][(r.next() % 8) as usize];
        }
        let o = TxOpts {
            mac_id: (r.next() & 1 == 1).then(|| r.next()),
            sta: r.next() & 1 == 1,
            basic_rates: r.next() << 8 | r.next(),
            supp_rates: r.next(),
            band: bands[(r.next() % 4) as usize],
            key: (r.next() & 1 == 1).then(|| TxKey {
                key_idx: r.next() as u8,
                sec_cam_idx: (r.next() & 1 == 1).then(|| r.next()),
                cipher: if r.next() & 1 == 1 {
                    cipher::CCMP
                } else {
                    r.next()
                },
            }),
            tid: r.next(),
            no_cck: r.next() & 1 == 1,
            req_tx_status: r.next() & 1 == 1,
            ..TxOpts::default()
        };
        t.chan = (r.next() & 1 == 1).then_some(TxrxChan {
            band: Band::B2g,
            ch: 1,
            prev: None,
        });
        t.tx.tx_rpt_enabled = r.next() & 1 == 1;
        let kind = [
            None,
            Some(TxKind::Mgmt),
            Some(TxKind::Data),
            Some(TxKind::Fwcmd),
        ][(r.next() % 4) as usize];
        if let Ok(info) = tx_info(&mut t, kind, &f, &o) {
            let _ = fill_txdesc(&info.desc());
        }
        let _ = tx_seq(&mut t, &mut f);
        let mut key = CcmpKey::new(r.next() as u8, None);
        key.tx_pn = u64::MAX - u64::from(r.next() % 2);
        let _ = eth_to_wlan(
            &f,
            DataTx {
                bssid: AP,
                key: (r.next() & 1 == 1).then_some(&mut key),
                qos: (r.next() & 1 == 1).then(|| r.next() as u16),
                seq: r.next() as u16,
            },
        );
        let _ = ccmp_replay(&mut key, r.next() as u16, u64::from(r.next()));
        let _ = rsn_parse(&f);
        let _ = parse_ies(&f, (r.next() % 90) as usize, (r.next() % 90) as usize);
        let _ = rate_bitmap(&f, Band::B5g, r.next() & 1 == 1);
    }
}

#[test]
fn phy_ops_give_the_signal_and_the_cfo_reaches_phy() {
    // phy's own 8852B ops: RSSI from RPL, the signal of the stronger path, the channel of the
    // PPDU status; IE01's CFO into phy's CFO tracking for the PPDU's MAC ID
    let mut phy = crate::phy::Phy::default();
    phy.cfo.cfo_tail = vec![0; 4];
    phy.cfo.cfo_cnt = vec![0; 4];
    let mut t = Txrx::new();
    t.chan = chan(Band::B2g, 1);
    let b = beacon();
    let d = rxdesc(Rd {
        long: true,
        len: b.len() as u32,
        cnt: 3,
        rate: 4,
        ..Rd::default()
    });
    rx_parse(&mut t, RxIn::packet(&cat(&[&d, &b])), &mut PhyOps(&mut phy));
    let pp = ppdu_item(Pp {
        cnt: 3,
        rate: 4,
        ie01: true,
        ..Pp::default()
    });
    let r = rx_parse(&mut t, pp.rx(), &mut PhyOps(&mut phy));
    let e = &r.deliver[0];
    assert!(e.ppdu_match);
    // RSSI_A 0x4c, RSSI_B 0x48 raised by RPL 0x50 - RSSI_AVG 0x4c: 0x50 and 0x4c; the signal
    // is RTW89_RSSI_RAW_TO_DBM of the larger one
    assert_eq!(e.ppdu.as_ref().unwrap().rssi[..2], [0x50, 0x4c]);
    assert_eq!(e.status.signal, (0x50 >> 1) - 110);
    assert_eq!(e.status.chains, 3);
    assert_eq!(
        (e.status.band, e.status.ch),
        (Some(Band::B2g), Some(6)),
        "CH_IDX 6"
    );
    assert_eq!((phy.cfo.cfo_cnt[0], phy.cfo.packet_count), (1, 1));
}
