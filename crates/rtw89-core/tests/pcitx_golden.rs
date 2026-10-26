//! pcitx against bringup-golden.txt where the golden trace shows it: from stage scan
//! on (probe requests, auth/assoc, EAPOL, DHCP, the stop's last frame), every TX BD
//! of a frame ring, every kick and every release-report pass (rtw89_pci_napi_poll's
//! RPQ half) are pcitx's. The bring-up before them (firmware download, MAC/PHY init,
//! channel) is not ported yet, so the trace cannot run there end to end; this test
//! takes the golden lines that are pcitx's, reads from them only *when* a frame went
//! to which ring and when a reclaim ran, replays that on rtw89-core's pcitx (rings
//! from `pci_alloc` with the JS lengths, so the same DMA addresses) with
//! test-pcitx.mjs's fake chip (every kicked BD taken at once, one release report per
//! frame, as V1's fake chip does), and the ops must be the golden lines, in order:
//! BD addresses and words, WD page addresses (the free list's order), host index
//! values, and each reclaim's RPQ/TX index reads, RPQ buffer clears and index write.
//! Reads only the golden file (no node); it never skips.

use std::path::PathBuf;

use rtw89_core::membus::MemBus;
use rtw89_core::pci::{self, PciRings, RingConfig, TxDescInfo};
use rtw89_core::pcitx::PciTx;
use rtw89_core::pcitx::fakechip::{TxChip, data_info, mgmt_info};
use rtw89_core::tables::regs::*;

fn golden() -> Vec<String> {
    let dir = std::env::var_os("WLAN_TOOLS")
        .map(PathBuf::from)
        .unwrap_or_else(|| "/home/georg/BitsOS/werkzeuge/plattform/wlan".into());
    let p = dir.join("bringup-golden.txt");
    let text = std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()));
    text.lines().map(String::from).collect()
}

fn num(s: &str) -> u32 {
    u32::from_str_radix(s.trim_start_matches("0x"), 16).unwrap()
}

/// What a golden line is to pcitx
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Item {
    /// a TX BD word of the frame ring `ring` (index into rings.tx); `first`: the length word
    Bd {
        ring: usize,
        first: bool,
        len: u32,
    },
    Kick {
        ring: usize,
    },
    ReclaimStart,
    /// a line inside a reclaim pass
    InReclaim,
}

struct Layout {
    rings: PciRings,
}

impl Layout {
    fn new() -> Layout {
        let mut b = MemBus::new();
        Layout {
            rings: pci::pci_alloc(&mut b, &RingConfig::js()).unwrap(),
        }
    }
    fn frame_rings(&self) -> impl Iterator<Item = (usize, &pci::TxRing)> {
        self.rings
            .tx
            .iter()
            .enumerate()
            .filter(|(_, r)| r.name != "CH12")
    }
    fn classify(&self, line: &str, in_reclaim: bool) -> Option<Item> {
        let w: Vec<&str> = line.split(' ').collect();
        if w.len() < 4 || line.starts_with('#') {
            return None;
        }
        let (op, a, v) = (w[0], num(w[1]), num(w[3]));
        let rpq = self.rings.rx("RPQ").unwrap();
        match op {
            "wr32" if a == R_AX_PCIE_HISR00 && v == B_AX_RPQDMA_INT | B_AX_RPQBD_FULL_INT => {
                return Some(Item::ReclaimStart);
            }
            "mwr32" => {
                for (i, r) in self.frame_rings() {
                    if a >= r.bd && a < r.bd + r.len * 8 {
                        return Some(Item::Bd {
                            ring: i,
                            first: (a - r.bd) % 8 == 0,
                            len: v & 0xffff,
                        });
                    }
                }
                if in_reclaim && v == 0 && rpq.bufs.contains(&a) {
                    return Some(Item::InReclaim);
                }
            }
            "wr16" => {
                if in_reclaim && a == rpq.idx {
                    return Some(Item::InReclaim);
                }
                if let Some((i, _)) = self.frame_rings().find(|(_, r)| r.idx == a) {
                    return Some(Item::Kick { ring: i });
                }
            }
            "rd32"
                if in_reclaim && (a == rpq.idx || self.frame_rings().any(|(_, r)| r.idx == a)) =>
            {
                return Some(Item::InReclaim);
            }
            _ => {}
        }
        None
    }
}

/// The frame's info as txrx would give it on this ring (the WD info only when the BD
/// length says the page has one: 24 + 24 + 8 + 8)
fn info_for(ring: &pci::TxRing, bd_len: u32) -> TxDescInfo {
    let i = match ring.name {
        "CH8" => mgmt_info(30, 0, 0),
        "ACH1" => data_info(1, 30, 0, 0),
        "ACH2" => data_info(4, 30, 0, 0),
        "ACH3" => data_info(6, 30, 0, 0),
        "CH9" => TxDescInfo {
            qsel: RTW89_TX_QSEL_B0_HI,
            ch_dma: RTW89_TXCH_CH9,
            ..data_info(0, 30, 0, 0)
        },
        _ => data_info(0, 30, 0, 0),
    };
    if bd_len == 0x40 {
        i
    } else {
        TxDescInfo { wd_info: None, ..i }
    }
}

#[test]
fn every_pcitx_line_of_the_golden_trace_from_stage_scan_on() {
    let g = golden();
    let start = g
        .iter()
        .position(|l| l == "# stage scan")
        .expect("golden has stage scan");
    let lay = Layout::new();

    // the golden's pcitx lines, and what happened when
    let mut want = Vec::new();
    let mut items = Vec::new();
    let mut in_reclaim = false;
    for l in &g[start..] {
        let it = lay.classify(l, in_reclaim);
        in_reclaim = matches!(it, Some(Item::ReclaimStart | Item::InReclaim));
        if let Some(it) = it {
            want.push(l.clone());
            items.push(it);
        }
    }
    let frames = items
        .iter()
        .filter(|i| matches!(i, Item::Bd { first: true, .. }))
        .count();
    let reclaims = items.iter().filter(|i| **i == Item::ReclaimStart).count();
    assert!(
        frames >= 9 && reclaims >= 14,
        "{frames} frames, {reclaims} reclaims in the golden trace"
    );

    // replay on the Rust side
    let mut d = MemBus::new();
    let mut rings = pci::pci_alloc(&mut d, &RingConfig::js()).unwrap();
    let mut tx = PciTx::new();
    tx.init(&rings);
    let mut chip = TxChip::new(&mut d, &rings);
    let t0 = d.trace.len();
    for it in &items {
        match *it {
            Item::Bd {
                ring,
                first: true,
                len,
            } => {
                let r = &rings.tx[ring];
                let (ch, inf) = (u32::from(r.ch), info_for(r, len));
                let q = tx
                    .tx(&mut d, &mut rings, ch, &inf, &[0x5a; 30], false)
                    .unwrap();
                assert!(q.is_some(), "a golden frame the ring takes");
            }
            Item::Kick { ring } => {
                let ch = u32::from(rings.tx[ring].ch);
                tx.kick(&mut d, &rings, ch).unwrap();
                chip.run(&mut d, &rings);
            }
            Item::ReclaimStart => {
                tx.reclaim(&mut d, &mut rings).unwrap();
            }
            _ => {}
        }
    }
    let ours: Vec<String> = d.lines()[t0..].to_vec();
    for (i, w) in want.iter().enumerate() {
        assert_eq!(
            ours.get(i),
            Some(w),
            "pcitx line {} of the golden trace",
            i + 1
        );
    }
    assert_eq!(ours.len(), want.len(), "as many lines");
    assert!(tx.take_events().is_empty(), "no warnings on the golden run");
    eprintln!(
        "pcitx golden: {} lines ({frames} frames, {reclaims} reclaims) from line {}",
        want.len(),
        start + 1
    );
}
