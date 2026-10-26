//! js/test-pcitx.mjs ported (its expected words written by hand from the bit
//! positions in pci.h / txrx.h), with infos built by hand in place of txrx's
//! txInfo, plus the failure paths of the Rust side.

use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

use super::fakechip::{RPQ_INFO, TxChip, data_info, mgmt_info, rpp, rpq_buf};
use super::*;
use crate::bus::{Bus, BusError};
use crate::membus::MemBus;
use crate::pci::fwcmd::{TxDescInfo, fill_txdesc};
use crate::pci::rings::{RingConfig, pci_alloc};
use crate::trace::{Op, OpKind, TraceEntry};

const CH8: u32 = RTW89_TXCH_CH8;
const ACH0: u32 = RTW89_TXCH_ACH0;
const ACH1: u32 = RTW89_TXCH_ACH1;
const ACH2: u32 = RTW89_TXCH_ACH2;
const ACH3: u32 = RTW89_TXCH_ACH3;
const HISR_RPQ: u32 = B_AX_RPQDMA_INT | B_AX_RPQBD_FULL_INT;

fn setup() -> (MemBus, PciRings, PciTx) {
    let mut b = MemBus::new();
    let rings = pci_alloc(&mut b, &RingConfig::js()).unwrap();
    let mut tx = PciTx::new();
    tx.init(&rings);
    (b, rings, tx)
}

/// register accesses since `from` (test-pcitx.mjs `regOps`: writes, reads, rmw; no memory)
fn reg_ops(b: &MemBus, from: usize) -> Vec<(OpKind, u8, u32, u32)> {
    b.trace[from..]
        .iter()
        .filter_map(|e| match e {
            TraceEntry::Op(o) if matches!(o.kind, OpKind::Wr | OpKind::Rd | OpKind::Rmw) => {
                Some((o.kind, o.size, o.a, o.c))
            }
            _ => None,
        })
        .collect()
}
fn wr(n: u8, a: u32, v: u32) -> (OpKind, u8, u32, u32) {
    (OpKind::Wr, n, a, v)
}
fn rd(a: u32) -> (OpKind, u8, u32, u32) {
    (OpKind::Rd, 4, a, 0)
}
fn ops_from(b: &MemBus, from: usize) -> Vec<Op> {
    b.trace[from..]
        .iter()
        .filter_map(|e| match e {
            TraceEntry::Op(o) => Some(*o),
            TraceEntry::Note(_) => None,
        })
        .collect()
}
fn mem(b: &mut MemBus, at: u32, n: usize) -> Vec<u8> {
    let mut v = vec![0u8; n];
    b.mem_read_bytes(at, &mut v).unwrap();
    v
}
fn texts(ev: &[TxEvent]) -> Vec<String> {
    ev.iter().map(ToString::to_string).collect()
}
fn frame(n: usize, fill: u8) -> Vec<u8> {
    (0..n).map(|i| fill.wrapping_add(i as u8)).collect()
}
fn idx(rings: &PciRings, name: &str) -> u32 {
    rings
        .tx(name)
        .map(|r| r.idx)
        .or_else(|| rings.rx(name).map(|r| r.idx))
        .unwrap()
}

#[test]
fn test_pcitx_mjs() {
    // ---- pciTx: WD page, address info, TX BD, kick off ----
    let (mut d, mut rings, mut tx) = setup();
    let auth = frame(30, 0xb0);
    let info = mgmt_info(30, 0, 0x123);
    let r = tx
        .tx(&mut d, &mut rings, CH8, &info, &auth, true)
        .unwrap()
        .unwrap();
    let ring = rings.tx("CH8").unwrap().clone();
    let rpq = rings.rx("RPQ").unwrap().clone();
    let page = ring.bufs[0];
    assert_eq!(
        (r.ch, r.seq, rings.tx("CH8").unwrap().wp),
        ("CH8", 0, 1),
        "WD page 0, BD 0"
    );
    let n = d.trace.len();
    assert_eq!(
        ops_from(&d, n - 3),
        vec![
            Op::new(OpKind::Mwr, 4, 0, ring.bd, 0, 0x4000_0040),
            Op::new(OpKind::Mwr, 4, 0, ring.bd + 4, 0, page),
            Op::new(OpKind::Wr, 2, 0, ring.idx, 0, 1),
        ],
        "TX BD (length 64 | LS b14 << 16, WD page) then the host index write"
    );
    let pg = mem(&mut d, page, 64);
    assert_eq!(
        pg[..48],
        fill_txdesc(&info)[..],
        "WD body + info in the page"
    );
    let mut want = vec![0x00, 0x80, 0, 0, 0, 0, 0, 0, 0x1e, 0x00, 0x01, 0x80];
    want.extend_from_slice(&(page + 128).to_le_bytes());
    assert_eq!(
        pg[48..],
        want[..],
        "WP info seq0 = VALID | 0; address info: 30, MSDU_LS | NUM 1, frame"
    );
    assert_eq!(
        mem(&mut d, page + 128, 30),
        auth,
        "frame behind the WD page"
    );

    // ---- a release report on RPQ gives the page back (register order as rtw89_pci_napi_poll) ----
    d.forced.insert(rpq.idx, 1 << 16);
    d.forced.insert(ring.idx, 1 << 16);
    d.mem_write_bytes(rpq.bufs[0], &rpq_buf(&[rpp(0, 0x12, 0)], RPQ_INFO))
        .unwrap();
    let mark = d.trace.len();
    let done = tx.reclaim(&mut d, &mut rings).unwrap();
    assert_eq!(
        done,
        vec![TxReport {
            ch: "CH8",
            seq: 0,
            tx_status: 0,
            acked: true,
            len: 30,
            qsel: 0x12,
            mac_id: 0,
            wlan_seq: 0x123
        }]
    );
    assert_eq!(
        reg_ops(&d, mark),
        vec![
            wr(4, R_AX_PCIE_HISR00, HISR_RPQ),
            rd(rpq.idx),
            rd(ring.idx),
            wr(2, rpq.idx, 1)
        ],
        "HISR00 RPQ clear, RPQ idx read, TX ring idx read, RPQ idx write"
    );
    let w8 = tx.wd_ring("CH8").unwrap();
    assert_eq!(
        (
            w8.busy.len(),
            w8.free.back().copied(),
            rings.tx("CH8").unwrap().rp,
            rings.rx("RPQ").unwrap().wp
        ),
        (0, Some(0), 1, 1),
        "page 0 free again"
    );
    assert_eq!(
        mem(&mut d, rpq.bufs[0], 4),
        vec![0; 4],
        "RPQ rxbd_info cleared after use"
    );

    // ---- refused frames: nothing taken, nothing written ----
    let free_before = tx.wd_ring("CH8").unwrap().free.len();
    let mark = d.trace.len();
    let e = tx.tx(&mut d, &mut rings, ACH0, &mgmt_info(30, 0, 0), &auth, true);
    assert!(
        matches!(
            e,
            Err(Rtw89Error::TxRefused {
                ch: 0,
                value: 8,
                ..
            })
        ),
        "management info on ACH0: {e:?}"
    );
    let mut long = auth.clone();
    long.extend_from_slice(&[0; 100]);
    let e = tx.tx(&mut d, &mut rings, CH8, &mgmt_info(30, 0, 0), &long, true);
    assert!(
        matches!(e, Err(Rtw89Error::TxRefused { value: 30, .. })),
        "TXPKT_SIZE != frame length: {e:?}"
    );
    let wrong_qsel = TxDescInfo {
        qsel: RTW89_TX_QSEL_VO_0,
        ..data_info(0, 60, 0, 0)
    };
    let e = tx.tx(&mut d, &mut rings, ACH0, &wrong_qsel, &frame(60, 1), true);
    assert!(
        matches!(e, Err(Rtw89Error::TxRefused { value: 3, .. })),
        "QSEL VO_0 on ACH0: {e:?}"
    );
    let w8 = tx.wd_ring("CH8").unwrap();
    assert_eq!(
        (
            w8.free.len(),
            w8.busy.len(),
            d.trace.len(),
            rings.tx("CH8").unwrap().wp
        ),
        (free_before, 0, mark, 1),
        "refusals keep every WD page and write nothing"
    );

    // ---- resource check on a full ring (ACH1, 4 BDs) ----
    let ach1 = rings.tx("ACH1").unwrap().clone();
    assert_eq!(
        tx.resource(&mut d, &mut rings, ACH1).unwrap(),
        3,
        "one BD always kept"
    );
    for i in 0..3 {
        tx.tx(
            &mut d,
            &mut rings,
            ACH1,
            &data_info(1, 60, 0, i),
            &frame(60, 2),
            i == 2,
        )
        .unwrap()
        .unwrap();
    }
    let mark = d.trace.len();
    assert_eq!(
        tx.tx(
            &mut d,
            &mut rings,
            ACH1,
            &data_info(1, 60, 0, 3),
            &frame(60, 2),
            true
        )
        .unwrap(),
        None
    );
    assert_eq!(
        d.trace.len(),
        mark,
        "full ring: refused (Linux -ENOSPC) without touching the chip"
    );
    assert_eq!(texts(&tx.take_events()), ["no available TXBD on ACH1"]);
    d.forced.insert(ach1.idx, 2 << 16);
    let mark = d.trace.len();
    assert_eq!(
        tx.resource(&mut d, &mut rings, ACH1).unwrap(),
        1,
        "BDs reclaimed, one WD page left"
    );
    assert_eq!(
        reg_ops(&d, mark),
        vec![rd(rpq.idx), rd(ach1.idx)],
        "RPQ idx, then the ring idx"
    );
    d.mem_write_bytes(
        rpq.bufs[1],
        &rpq_buf(&[rpp(0, 1, 0), rpp(1, 1, RTW89_TX_RETRY_LIMIT)], RPQ_INFO),
    )
    .unwrap();
    d.forced.insert(rpq.idx, 2 << 16);
    let done = tx.reclaim(&mut d, &mut rings).unwrap();
    let got: Vec<_> = done
        .iter()
        .map(|x| (x.ch, x.seq, x.tx_status, x.acked))
        .collect();
    assert_eq!(
        got,
        [("ACH1", 0, 0, true), ("ACH1", 1, 1, false)],
        "the second not acked (retry limit)"
    );
    assert_eq!(
        tx.resource(&mut d, &mut rings, ACH1).unwrap(),
        2,
        "after the reports: 2 BDs, 3 pages"
    );

    // ---- a report that comes before the TX BD index moved (rtw89_pci_release_txwd_skb) ----
    let ach2 = rings.tx("ACH2").unwrap().clone();
    tx.tx(
        &mut d,
        &mut rings,
        ACH2,
        &data_info(4, 60, 0, 0),
        &frame(60, 3),
        true,
    )
    .unwrap();
    d.mem_write_bytes(
        rpq.bufs[2],
        &rpq_buf(&[rpp(0, RTW89_TX_QSEL_VI_0, 0)], RPQ_INFO),
    )
    .unwrap();
    d.forced.insert(rpq.idx, 3 << 16);
    d.forced.insert(ach2.idx, 0);
    let done = tx.reclaim(&mut d, &mut rings).unwrap();
    let w2 = tx.wd_ring("ACH2").unwrap();
    assert_eq!(
        (done.len(), w2.busy_seqs(), w2.free_seqs().contains(&0)),
        (1, vec![0], false),
        "report first: frame reported, page stays on the busy list"
    );
    assert_eq!(texts(&tx.take_events()), ["queue 2 txwd 0 is not idle"]);
    for i in 0..2 {
        tx.tx(
            &mut d,
            &mut rings,
            ACH2,
            &data_info(5, 60, 0, i),
            &frame(60, 3),
            true,
        )
        .unwrap();
    }
    d.forced.insert(ach2.idx, 3 << 16);
    assert_eq!(
        tx.resource(&mut d, &mut rings, ACH2).unwrap(),
        2,
        "reclaim_txbd frees page 0"
    );
    let w2 = tx.wd_ring("ACH2").unwrap();
    let mut free = w2.free_seqs();
    free.sort_unstable();
    assert_eq!(
        (free, w2.busy.len()),
        (vec![0, 3], 0),
        "pages 1 and 2 wait for their reports"
    );

    // ---- RPQ buffers the release path refuses ----
    let mark = d.trace.len();
    let rpq_wp = rings.rx("RPQ").unwrap().wp;
    let len = rpq.len;
    d.mem_write_bytes(
        rpq.bufs[rpq_wp as usize],
        &rpq_buf(&[rpp(1, RTW89_TX_QSEL_VI_0, 0)], 0x0001_8000),
    )
    .unwrap();
    d.forced.insert(rpq.idx, ((rpq_wp + 2) % len) << 16);
    let done = tx.reclaim(&mut d, &mut rings).unwrap();
    assert_eq!(done.len(), 0);
    assert_eq!(rings.rx("RPQ").unwrap().wp, (rpq_wp + 2) % len);
    assert_eq!(
        reg_ops(&d, mark).last(),
        Some(&wr(2, rpq.idx, (rpq_wp + 2) % len))
    );
    assert_eq!(
        texts(&tx.take_events()),
        [
            "cannot process RP frame not set FS/LS",
            "failed to release TX skbs"
        ],
        "RPQ buffer without LS: nothing released, the rest skipped"
    );
    let wp = rings.rx("RPQ").unwrap().wp;
    d.mem_write_bytes(
        rpq.bufs[wp as usize],
        &rpq_buf(&[rpp(1, RTW89_TX_QSEL_B1_MGMT, 0)], RPQ_INFO),
    )
    .unwrap();
    d.forced.insert(rpq.idx, ((wp + 1) % len) << 16);
    assert_eq!(tx.reclaim(&mut d, &mut rings).unwrap().len(), 0);
    assert_eq!(
        texts(&tx.take_events()),
        ["should no release report on txch 10"]
    );

    // ---- no WD page and no report: the resource check returns at once (ACH3) ----
    let ach3 = rings.tx("ACH3").unwrap().clone();
    for i in 0..3 {
        tx.tx(
            &mut d,
            &mut rings,
            ACH3,
            &data_info(6, 60, 0, i),
            &frame(60, 4),
            true,
        )
        .unwrap();
    }
    d.forced.insert(ach3.idx, 1 << 16);
    assert_eq!(
        tx.resource(&mut d, &mut rings, ACH3).unwrap(),
        1,
        "page 0 waits for its report"
    );
    let r = tx
        .tx(
            &mut d,
            &mut rings,
            ACH3,
            &data_info(7, 60, 0, 3),
            &frame(60, 4),
            true,
        )
        .unwrap()
        .unwrap();
    let n = d.trace.len();
    assert_eq!(
        (r.seq, r.wp),
        (3, 0),
        "fourth frame: page 3, host index wraps to 0"
    );
    assert_eq!(
        ops_from(&d, n - 1),
        vec![Op::new(OpKind::Wr, 2, 0, ach3.idx, 0, 0)]
    );
    let mark = d.trace.len();
    assert_eq!(tx.resource(&mut d, &mut rings, ACH3).unwrap(), 0);
    assert_eq!(
        reg_ops(&d, mark),
        vec![rd(rpq.idx)],
        "no page, no report: only the RPQ index is read"
    );
}

/// txrx's txFrame as far as pcitx sees it: data frames check the resource first
fn tx_frame(
    d: &mut MemBus,
    rings: &mut PciRings,
    tx: &mut PciTx,
    info: &TxDescInfo,
    f: &[u8],
) -> bool {
    let data = info.qsel != RTW89_TX_QSEL_B0_MGMT;
    if data && tx.resource(d, rings, info.ch_dma).unwrap() == 0 {
        return false;
    }
    tx.tx(d, rings, info.ch_dma, info, f, true)
        .unwrap()
        .is_some()
}

#[test]
fn a_fake_chip_around_the_rings() {
    let (mut d, mut rings, mut tx) = setup();
    let mut chip = TxChip::new(&mut d, &rings);
    let (mut sent, mut refused, mut reports) = (0, 0, 0);
    fn count(q: bool, sent: &mut u32, refused: &mut u32) {
        if q { *sent += 1 } else { *refused += 1 }
    }
    let probe = frame(40, 0x40);
    let data = frame(60, 0x60);
    let drain = |d: &mut MemBus, rings: &mut PciRings, tx: &mut PciTx, chip: &mut TxChip| {
        let mut n = 0;
        for _ in 0..4 {
            chip.run(d, rings);
            n += tx.reclaim(d, rings).unwrap().len();
        }
        n
    };
    for i in 0..40 {
        count(
            tx_frame(&mut d, &mut rings, &mut tx, &mgmt_info(40, 0, 0), &probe),
            &mut sent,
            &mut refused,
        );
        count(
            tx_frame(&mut d, &mut rings, &mut tx, &data_info(0, 60, 0, i), &data),
            &mut sent,
            &mut refused,
        );
        chip.run(&mut d, &rings);
        if i % 3 == 2 {
            reports += tx.reclaim(&mut d, &mut rings).unwrap().len();
        }
    }
    reports += drain(&mut d, &mut rings, &mut tx, &mut chip);
    let kicks = |d: &MemBus, rings: &PciRings, name: &str| -> Vec<u32> {
        let a = idx(rings, name);
        ops_from(d, 0)
            .into_iter()
            .filter(|o| o.kind == OpKind::Wr && o.size == 2 && o.a == a)
            .map(|o| o.c)
            .collect()
    };
    let k8 = kicks(&d, &rings, "CH8");
    let k0 = kicks(&d, &rings, "ACH0");
    assert_eq!(
        (
            k8.len(),
            k8.iter().max().copied(),
            k0.iter().max().copied(),
            rings.tx("CH8").unwrap().wp
        ),
        (40, Some(15), Some(15), 8),
        "40 frames per ring: around a 16-BD ring twice, every host index below 16"
    );
    let free = |tx: &PciTx, n: &str| tx.wd_ring(n).unwrap().free.len();
    assert_eq!(
        (sent, reports, free(&tx, "CH8"), free(&tx, "ACH0")),
        (80, 80, 16, 16)
    );
    for i in 0..15 {
        count(
            tx_frame(&mut d, &mut rings, &mut tx, &data_info(0, 60, 0, i), &data),
            &mut sent,
            &mut refused,
        );
    }
    chip.run(&mut d, &rings);
    assert!(
        tx_frame(&mut d, &mut rings, &mut tx, &data_info(0, 60, 0, 99), &data),
        "ACH0 full but sent: the resource check pulls the reports in"
    );
    for _ in 0..20 {
        count(
            tx_frame(&mut d, &mut rings, &mut tx, &mgmt_info(40, 0, 0), &probe),
            &mut sent,
            &mut refused,
        );
    }
    assert_eq!(
        (sent, refused),
        (110, 5),
        "20 probes at once on CH8: 15 queued, 5 refused"
    );
    reports += drain(&mut d, &mut rings, &mut tx, &mut chip);
    let busy = |tx: &PciTx, n: &str| tx.wd_ring(n).unwrap().busy.len();
    assert_eq!(
        (
            reports,
            free(&tx, "CH8"),
            free(&tx, "ACH0"),
            busy(&tx, "CH8"),
            busy(&tx, "ACH0")
        ),
        (111, 16, 16, 0, 0),
        "all 111 frames reported, every WD page free again"
    );
}

// ---- beyond test-pcitx.mjs: Linux's checks, R1, and the failure paths ----

#[test]
fn fwcmd_channel_and_long_frames_are_refused() {
    let (mut d, mut rings, mut tx) = setup();
    let fw = TxDescInfo {
        ch_dma: RTW89_DMA_H2C,
        pkt_size: 10,
        ..TxDescInfo::default()
    };
    let e = tx.tx(&mut d, &mut rings, RTW89_TXCH_CH12, &fw, &[0; 10], true);
    assert!(matches!(
        e,
        Err(Rtw89Error::TxRefused {
            what: "only fw cmd uses dma channel 12",
            ..
        })
    ));
    let e = tx.tx(
        &mut d,
        &mut rings,
        CH8,
        &TxDescInfo {
            ch_dma: RTW89_DMA_H2C,
            ..mgmt_info(10, 0, 0)
        },
        &[0; 10],
        true,
    );
    assert!(matches!(
        e,
        Err(Rtw89Error::TxRefused {
            what: "only fw cmd uses dma channel 12",
            ..
        })
    ));
    let e = tx.resource(&mut d, &mut rings, RTW89_TXCH_CH12);
    assert!(matches!(e, Err(Rtw89Error::TxRefused { .. })));
    let e = tx.tx(
        &mut d,
        &mut rings,
        5,
        &data_info(0, 10, 0, 0),
        &[0; 10],
        true,
    );
    assert!(matches!(
        e,
        Err(Rtw89Error::TxRefused {
            what: "no TX ring for this DMA channel",
            value: 5,
            ..
        })
    ));
    let big = vec![0u8; FRAME_ROOM + 1];
    let e = tx.tx(
        &mut d,
        &mut rings,
        CH8,
        &mgmt_info(big.len(), 0, 0),
        &big,
        true,
    );
    assert!(
        matches!(e, Err(Rtw89Error::TxRefused { value: 2049, .. })),
        "{e:?}"
    );
    let ok = vec![0u8; FRAME_ROOM];
    assert!(
        tx.tx(
            &mut d,
            &mut rings,
            CH8,
            &mgmt_info(ok.len(), 0, 0),
            &ok,
            true
        )
        .unwrap()
        .is_some()
    );
}

#[test]
fn a_frame_without_wd_info_has_a_40_byte_page() {
    let (mut d, mut rings, mut tx) = setup();
    let info = TxDescInfo {
        wd_info: None,
        ..data_info(0, 60, 3, 7)
    };
    let r = tx
        .tx(&mut d, &mut rings, ACH0, &info, &frame(60, 9), false)
        .unwrap()
        .unwrap();
    let ring = rings.tx("ACH0").unwrap();
    let page = ring.bufs[r.seq as usize];
    let n = d.trace.len();
    assert_eq!(
        ops_from(&d, n - 2)[0],
        Op::new(OpKind::Mwr, 4, 0, ring.bd, 0, 0x4000_0028)
    );
    let pg = mem(&mut d, page, 40);
    assert_eq!(pg[..24], fill_txdesc(&info)[..]);
    assert_eq!(pg[24..26], [0x00, 0x80], "seq0 right after the WD body");
    assert_eq!(pg[32..34], [60, 0]);
    assert_eq!(tx.wd_ring("ACH0").unwrap().pages[0].len, 40);
    // no kick yet: the host index register was never written
    assert!(
        !ops_from(&d, 0)
            .iter()
            .any(|o| o.kind == OpKind::Wr && o.a == ring.idx)
    );
    tx.kick(&mut d, &rings, ACH0).unwrap();
    assert_eq!(
        ops_from(&d, n).last(),
        Some(&Op::new(OpKind::Wr, 2, 0, ring.idx, 0, 1))
    );
}

#[test]
fn chip_indexes_past_the_ring_are_faults() {
    let (mut d, mut rings, mut tx) = setup();
    let rpq_idx = idx(&rings, "RPQ");
    let ach0_idx = idx(&rings, "ACH0");
    // RPQ index past the ring (16): an error with the register's value, nothing written back
    d.forced.insert(rpq_idx, 0xFFFF_FFFF);
    let mark = d.trace.len();
    let e = tx.reclaim(&mut d, &mut rings);
    assert_eq!(
        e,
        Err(Rtw89Error::RingIndex {
            reg: rpq_idx,
            value: 0xFFFF_FFFF,
            len: 16
        })
    );
    assert_eq!(
        reg_ops(&d, mark),
        vec![wr(4, R_AX_PCIE_HISR00, HISR_RPQ), rd(rpq_idx)]
    );
    assert_eq!(rings.rx("RPQ").unwrap().wp, 0);
    // a TX index past the ring in the resource check: the error, the ring's rp kept
    d.forced.insert(rpq_idx, 0);
    for i in 0..15 {
        tx.tx(
            &mut d,
            &mut rings,
            ACH0,
            &data_info(0, 60, 0, i),
            &frame(60, 1),
            true,
        )
        .unwrap()
        .unwrap();
    }
    d.forced.insert(ach0_idx, 16 << 16);
    let e = tx.resource(&mut d, &mut rings, ACH0);
    assert_eq!(
        e,
        Err(Rtw89Error::RingIndex {
            reg: ach0_idx,
            value: 16 << 16,
            len: 16
        })
    );
    assert_eq!(rings.tx("ACH0").unwrap().rp, 0);
    // the same inside the release path: the report is delivered, the page stays busy, a Fault event
    let rpq = rings.rx("RPQ").unwrap().clone();
    d.mem_write_bytes(
        rpq.bufs[0],
        &rpq_buf(&[rpp(0, RTW89_TX_QSEL_BE_0, 0)], RPQ_INFO),
    )
    .unwrap();
    d.forced.insert(rpq_idx, 1 << 16);
    let done = tx.reclaim(&mut d, &mut rings).unwrap();
    assert_eq!(done.len(), 1);
    let ev = tx.take_events();
    assert!(
        matches!(ev[0], TxEvent::Fault(Rtw89Error::RingIndex { .. })),
        "{ev:?}"
    );
    assert_eq!(texts(&ev[1..]), ["queue 0 txwd 0 is not idle"]);
    assert_eq!(rings.rx("RPQ").unwrap().wp, 1, "the RPQ buffer was taken");
    // once the index reads right, the page comes back through reclaim_txbd
    d.forced.insert(ach0_idx, 1 << 16);
    assert_eq!(tx.resource(&mut d, &mut rings, ACH0).unwrap(), 1);
    assert_eq!(tx.wd_ring("ACH0").unwrap().free_seqs(), vec![15, 0]);
}

#[test]
fn hostile_reports_never_free_a_page_twice() {
    let (mut d, mut rings, mut tx) = setup();
    let rpq = rings.rx("RPQ").unwrap().clone();
    let ch8_idx = idx(&rings, "CH8");
    tx.tx(
        &mut d,
        &mut rings,
        CH8,
        &mgmt_info(30, 0, 1),
        &frame(30, 1),
        true,
    )
    .unwrap();
    d.forced.insert(ch8_idx, 1 << 16);
    // the right report, then the same one again, a seq past the 16 pages, one past
    // Linux's 512, an unmapped QSEL (Linux: ACH0), all in one buffer
    let rpps = [
        rpp(0, RTW89_TX_QSEL_B0_MGMT, 0),
        rpp(0, RTW89_TX_QSEL_B0_MGMT, 0),
        rpp(20, RTW89_TX_QSEL_B0_MGMT, 0),
        rpp(600, RTW89_TX_QSEL_B0_MGMT, 0),
        rpp(1, RTW89_TX_QSEL_B0_BCN, 0),
    ];
    d.mem_write_bytes(rpq.bufs[0], &rpq_buf(&rpps, RPQ_INFO))
        .unwrap();
    d.forced.insert(rpq.idx, 1 << 16);
    let done = tx.reclaim(&mut d, &mut rings).unwrap();
    assert_eq!(done.len(), 1);
    assert_eq!(
        texts(&tx.take_events()),
        [
            "queue 8 txwd 0 is not idle",
            "no WD page 20 on txch 8",
            "invalid seq 600",
            "Cannot map qsel to dma: 16",
            "queue 0 txwd 1 is not idle",
        ]
    );
    let w = tx.wd_ring("CH8").unwrap();
    assert_eq!(w.free.len(), 16, "page 0 once on the free list");
    let mut seqs = w.free_seqs();
    seqs.sort_unstable();
    seqs.dedup();
    assert_eq!(seqs.len(), 16);
    // the unmapped QSEL went to ACH0's page 1, which was free: not idle, nothing released
    assert_eq!(tx.wd_ring("ACH0").unwrap().free.len(), 16);
}

#[test]
fn a_write_size_past_the_buffer_or_a_long_descriptor() {
    let (mut d, mut rings, mut tx) = setup();
    let rpq = rings.rx("RPQ").unwrap().clone();
    for i in 0..2 {
        tx.tx(
            &mut d,
            &mut rings,
            ACH0,
            &data_info(0, 60, 0, i),
            &frame(60, 1),
            true,
        )
        .unwrap();
    }
    d.forced.insert(idx(&rings, "ACH0"), 2 << 16);
    // long RX descriptor and 2 drv_info units: the reports start at 4 + 16 + 32
    let mut b = rpq_buf(&[], RPQ_INFO);
    let d0 = AX_RXD_LONG_RXD | (2 << 28) | 0x0700_0000;
    b[4..8].copy_from_slice(&d0.to_le_bytes());
    b.resize(52, 0xEE);
    b.extend_from_slice(&rpp(1, 0, 0).to_le_bytes());
    let info = RTW89_PCI_RXBD_FS | RTW89_PCI_RXBD_LS | (b.len() as u32);
    b[0..4].copy_from_slice(&info.to_le_bytes());
    d.mem_write_bytes(rpq.bufs[0], &b).unwrap();
    // WRITE_SIZE 0x3FFF, larger than the 2048-byte buffer: cut to it; zero words are
    // reports of seq 0 on ACH0
    let mut b = rpq_buf(&[rpp(0, 0, 0)], RPQ_INFO | 0x3fff);
    b.resize(2048, 0);
    d.mem_write_bytes(rpq.bufs[1], &b).unwrap();
    d.forced.insert(rpq.idx, 2 << 16);
    let done = tx.reclaim(&mut d, &mut rings).unwrap();
    let got: Vec<_> = done.iter().map(|x| (x.ch, x.seq)).collect();
    assert_eq!(got, [("ACH0", 1), ("ACH0", 0)]);
    assert_eq!(tx.wd_ring("ACH0").unwrap().free.len(), 16);
}

#[test]
fn a_bd_address_past_4_gib_is_refused_without_taking_a_page() {
    // the BD address is ring.bd + wp * 8, both checked (no wrap, no panic in debug)
    let (mut d, mut rings, mut tx) = setup();
    let mark = d.trace.len();
    {
        let r = rings.tx.iter_mut().find(|r| r.name == "CH8").unwrap();
        r.bd = u32::MAX - 8;
        r.wp = 2;
    }
    let e = tx.tx(
        &mut d,
        &mut rings,
        CH8,
        &mgmt_info(30, 0, 0),
        &frame(30, 1),
        true,
    );
    assert_eq!(e, Err(Rtw89Error::BadDmaAddress { addr: u32::MAX - 8 }));
    assert_eq!(tx.wd_ring("CH8").unwrap().free.len(), 16);
    assert_eq!(d.trace.len(), mark);
}

#[test]
fn bus_errors_never_lose_a_page_or_a_report() {
    // the BD write fails: the page goes back, the host index stays
    let (mut d, mut rings, mut tx) = setup();
    d.fail_at_op = Some(d.trace.len());
    let e = tx.tx(
        &mut d,
        &mut rings,
        CH8,
        &mgmt_info(30, 0, 0),
        &frame(30, 1),
        true,
    );
    assert_eq!(e, Err(Rtw89Error::Bus(BusError::Gone)));
    let w = tx.wd_ring("CH8").unwrap();
    assert_eq!(
        (w.free.len(), w.busy.len(), rings.tx("CH8").unwrap().wp),
        (16, 0, 0)
    );
    // the kick fails: the frame is on the ring (Ok, not kicked, a Fault event), the
    // next kick sends it
    d.fail_at_op = Some(d.trace.len() + 2);
    let q = tx
        .tx(
            &mut d,
            &mut rings,
            CH8,
            &mgmt_info(30, 0, 0),
            &frame(30, 1),
            true,
        )
        .unwrap()
        .unwrap();
    assert_eq!((q.seq, q.wp, q.kicked), (1, 1, false));
    assert_eq!(
        tx.take_events(),
        [TxEvent::Fault(Rtw89Error::Bus(BusError::Gone))]
    );
    assert_eq!(
        (
            rings.tx("CH8").unwrap().wp,
            tx.wd_ring("CH8").unwrap().busy_seqs()
        ),
        (1, vec![1])
    );
    tx.kick(&mut d, &rings, CH8).unwrap();
    // the RPQ index write fails after a buffer was taken: the report is returned, the
    // next reclaim writes the index first
    let rpq = rings.rx("RPQ").unwrap().clone();
    d.forced.insert(idx(&rings, "CH8"), 1 << 16);
    d.mem_write_bytes(
        rpq.bufs[0],
        &rpq_buf(&[rpp(1, RTW89_TX_QSEL_B0_MGMT, 0)], RPQ_INFO),
    )
    .unwrap();
    d.forced.insert(rpq.idx, 1 << 16);
    let mark = d.trace.len();
    // HISR, RPQ idx, CH8 idx, rxbd_info clear, then the RPQ idx write fails
    d.fail_at_op = Some(mark + 4);
    let done = tx.reclaim(&mut d, &mut rings).unwrap();
    assert_eq!(done.len(), 1);
    assert!(rings.rx("RPQ").unwrap().idx_stale);
    let mark = d.trace.len();
    assert!(tx.reclaim(&mut d, &mut rings).unwrap().is_empty());
    assert_eq!(
        reg_ops(&d, mark),
        vec![
            wr(4, R_AX_PCIE_HISR00, HISR_RPQ),
            wr(2, rpq.idx, 1),
            rd(rpq.idx)
        ]
    );
    assert!(!rings.rx("RPQ").unwrap().idx_stale);
    // the rxbd_info clear fails: the buffer still counts as taken (its report delivered once)
    tx.tx(
        &mut d,
        &mut rings,
        CH8,
        &mgmt_info(30, 0, 0),
        &frame(30, 1),
        true,
    )
    .unwrap();
    d.forced.insert(idx(&rings, "CH8"), 2 << 16);
    d.mem_write_bytes(
        rpq.bufs[1],
        &rpq_buf(&[rpp(2, RTW89_TX_QSEL_B0_MGMT, 0)], RPQ_INFO),
    )
    .unwrap();
    d.forced.insert(rpq.idx, 2 << 16);
    let mark = d.trace.len();
    d.fail_at_op = Some(mark + 3);
    let done = tx.reclaim(&mut d, &mut rings).unwrap();
    assert_eq!(done.len(), 1);
    assert!(matches!(
        tx.take_events()[..],
        [TxEvent::Fault(Rtw89Error::Bus(BusError::Gone))]
    ));
    assert_eq!(rings.rx("RPQ").unwrap().wp, 2);
    assert!(tx.reclaim(&mut d, &mut rings).unwrap().is_empty());
}

#[test]
fn init_frees_every_page_and_a_new_alloc_gets_new_pages() {
    let (mut d, mut rings, mut tx) = setup();
    for i in 0..3 {
        tx.tx(
            &mut d,
            &mut rings,
            ACH0,
            &data_info(0, 60, 0, i),
            &frame(60, 1),
            true,
        )
        .unwrap();
    }
    assert_eq!(tx.wd_ring("ACH0").unwrap().busy.len(), 3);
    tx.init(&rings);
    assert_eq!(tx.wd_ring("ACH0").unwrap().free.len(), 16);
    assert!(tx.wd_ring("CH12").is_none(), "CH12 has no WD pages");
    assert_eq!(tx.wd.len(), 6);
    // a new pci_alloc: the WD ring is built again from the new buffers on first use
    let mut rings2 = pci_alloc(&mut d, &RingConfig::js()).unwrap();
    let r = tx
        .tx(
            &mut d,
            &mut rings2,
            ACH0,
            &data_info(0, 60, 0, 0),
            &frame(60, 1),
            true,
        )
        .unwrap()
        .unwrap();
    assert_eq!(r.seq, 0);
    assert_eq!(
        tx.wd_ring("ACH0").unwrap().pages[0].addr,
        rings2.tx("ACH0").unwrap().bufs[0]
    );
}

#[test]
fn rings_longer_than_linux_512_pages_work_r1() {
    let mut d = MemBus::new();
    d.dma_next = 0x10_0000;
    let cfg = RingConfig {
        ach0: 600,
        ..RingConfig::js()
    };
    let mut rings = pci_alloc(&mut d, &cfg).unwrap();
    let mut tx = PciTx::new();
    tx.init(&rings);
    let mut chip = TxChip::new(&mut d, &rings);
    let mut reports = 0;
    for round in 0..3 {
        for i in 0..590 {
            assert!(tx_frame(
                &mut d,
                &mut rings,
                &mut tx,
                &data_info(0, 60, 0, i),
                &frame(60, round)
            ));
            if i % 10 == 9 {
                chip.run(&mut d, &rings);
                reports += tx.reclaim(&mut d, &mut rings).unwrap().len();
            }
        }
        for _ in 0..4 {
            chip.run(&mut d, &rings);
            reports += tx.reclaim(&mut d, &mut rings).unwrap().len();
        }
    }
    assert_eq!(reports, 3 * 590);
    assert!(tx.take_events().is_empty());
    assert_eq!(tx.wd_ring("ACH0").unwrap().free.len(), 600);
}
