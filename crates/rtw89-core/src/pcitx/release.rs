//! The release reports on RPQ and the TX BD index (JS `pciTxReclaim` and the
//! release path under it): WD pages come back, frames get their TX status.

use alloc::vec;
use alloc::vec::Vec;

use crate::bus::{Bus, field};
use crate::error::{Rtw89Error, Rtw89Result};
use crate::pci::rings::PciRings;
use crate::tables::regs::*;

use super::{PciTx, Place, TxEvent, TxReport};

/// sizeof(struct rtw89_pci_rxbd_info)
const RXBD_INFO: usize = 4;
/// rtw8852b_pci_info.rpp_fmt_size = sizeof(struct rtw89_pci_rpp_fmt)
const RPP_SIZE: usize = 4;
/// The first read of an RPQ buffer: rxbd_info, RX descriptor and some reports (the JS's
/// one round trip over MCP for most buffers; a longer buffer gets a second read)
const RPQ_HEAD: u32 = 64;
/// sizeof(struct rtw89_rxdesc_short), sizeof(struct rtw89_rxdesc_long)
const RXD_SHORT: usize = 16;
const RXD_LONG: usize = 32;

/// rtw8852b_pci_info.tx_dma_ch_mask (rtw8852be.c:54): the channels the 8852BE has no ring for
const TX_DMA_CH_MASK: u32 = (1 << RTW89_TXCH_ACH4)
    | (1 << RTW89_TXCH_ACH5)
    | (1 << RTW89_TXCH_ACH6)
    | (1 << RTW89_TXCH_ACH7)
    | (1 << RTW89_TXCH_CH10)
    | (1 << RTW89_TXCH_CH11);

// Linux: rtw89_core_get_ch_dma (core.c:819), the 8852B's for PCIe: the DMA channel of a
// QSEL; `None` where Linux warns "Cannot map qsel to dma" and falls through to ACH0
pub fn ch_dma(qsel: u32) -> Option<u32> {
    Some(match qsel {
        RTW89_TX_QSEL_BE_0 | RTW89_TX_QSEL_BE_1 | RTW89_TX_QSEL_BE_2 | RTW89_TX_QSEL_BE_3 => {
            RTW89_TXCH_ACH0
        }
        RTW89_TX_QSEL_BK_0 | RTW89_TX_QSEL_BK_1 | RTW89_TX_QSEL_BK_2 | RTW89_TX_QSEL_BK_3 => {
            RTW89_TXCH_ACH1
        }
        RTW89_TX_QSEL_VI_0 | RTW89_TX_QSEL_VI_1 | RTW89_TX_QSEL_VI_2 | RTW89_TX_QSEL_VI_3 => {
            RTW89_TXCH_ACH2
        }
        RTW89_TX_QSEL_VO_0 | RTW89_TX_QSEL_VO_1 | RTW89_TX_QSEL_VO_2 | RTW89_TX_QSEL_VO_3 => {
            RTW89_TXCH_ACH3
        }
        RTW89_TX_QSEL_B0_MGMT => RTW89_TXCH_CH8,
        RTW89_TX_QSEL_B0_HI => RTW89_TXCH_CH9,
        RTW89_TX_QSEL_B1_MGMT => RTW89_TXCH_CH10,
        RTW89_TX_QSEL_B1_HI => RTW89_TXCH_CH11,
        _ => return None,
    })
}

/// rtw89_core_get_ch_dma with Linux's fallthrough (no warning: for host-built descriptors)
pub(super) fn ch_dma_or_ach0(qsel: u32) -> u32 {
    ch_dma(qsel).unwrap_or(RTW89_TXCH_ACH0)
}

// The part of rtw89_core_query_rxdesc (core.c:4107) the release path needs, on a fresh
// desc_info: where the reports start, counted from the buffer's start. Linux's AX
// reader never fills `shift`, so it is 0; drv_info in 8-byte units; then the short or
// long descriptor. The rest of the descriptor is the RX parser's (txrx).
fn rpp_start(buf: &[u8]) -> usize {
    let d0 = buf
        .get(RXBD_INFO..RXBD_INFO + 4)
        .map_or(0, |b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]));
    let drv_info_len = (field(d0, AX_RXD_DRV_INFO_SIZE_MASK) as usize) << 3;
    let rxd_len = if d0 & AX_RXD_LONG_RXD != 0 {
        RXD_LONG
    } else {
        RXD_SHORT
    };
    RXBD_INFO + drv_info_len + rxd_len
}

fn le32(buf: &[u8], at: usize) -> Option<u32> {
    let b = buf.get(at..at.checked_add(4)?)?;
    Some(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

// Linux: rtw89_pci_dma_recalc (pci.c:61) on an index the chip reported: how many
// entries it moved past `from`. Not Linux: an index past the ring is a fault
// (spec/wlan.md §2.3), with the register and its value.
fn moved(reg: u32, v: u32, len: u32, from: u32) -> Rtw89Result<(u32, u32)> {
    let cur = field(v, TXBD_HW_IDX_MASK);
    if cur >= len || from >= len {
        return Err(Rtw89Error::RingIndex { reg, value: v, len });
    }
    let cnt = if cur >= from {
        cur - from
    } else {
        len - (from - cur)
    };
    Ok((cur, cnt))
}

impl PciTx {
    // Linux: rtw89_pci_poll_rpq_dma (pci.c:720) as rtw89_pci_napi_poll (pci.c:4517) runs
    // it, before the RXQ poll (`pci::rx_poll`): release reports in, WD pages back.
    // Returns the TX reports collected since the last call (also those a `resource`
    // check pulled in). isr_clear_rpq of rtw89_pci_isr_ax (rtw8852be.c:14): status
    // bits only, BitsOS never enables the interrupt, but napi_poll writes them first.
    //
    // Only a reclaim that took no RPQ buffer returns an error (nothing was lost then);
    // one that took some returns them and leaves the failure to the next call (see
    // `release_tx`).
    pub fn reclaim<B: Bus + ?Sized>(
        &mut self,
        bus: &mut B,
        rings: &mut PciRings,
    ) -> Rtw89Result<Vec<TxReport>> {
        bus.w32(R_AX_PCIE_HISR00, B_AX_RPQDMA_INT | B_AX_RPQBD_FULL_INT)?;
        let cnt = self.rpq_recalc(bus, rings)?;
        if cnt > 0 {
            self.release_tx(bus, rings, cnt)?;
        }
        Ok(core::mem::take(&mut self.done))
    }

    // Linux: rtw89_pci_rxbd_recalc + rtw89_pci_dma_recalc for RX (pci.c:146, 61); the
    // 8852BE's rx_ring_eq_is_full is false. A host index write that did not reach the
    // chip last time is repeated first.
    pub(super) fn rpq_recalc<B: Bus + ?Sized>(
        &mut self,
        bus: &mut B,
        rings: &mut PciRings,
    ) -> Rtw89Result<u32> {
        let rx = rings.rx_mut("RPQ")?;
        if rx.idx_stale {
            bus.w16(rx.idx, rx.wp)?;
            rx.idx_stale = false;
        }
        let v = bus.r32(rx.idx)?;
        let (cur, cnt) = moved(rx.idx, v, rx.len, rx.wp)?;
        rx.rp = cur;
        Ok(cnt)
    }

    // Linux: rtw89_pci_reclaim_txbd (pci.c:501) with rtw89_pci_txbd_recalc (pci.c:87)
    pub(super) fn reclaim_txbd<B: Bus + ?Sized>(
        &mut self,
        bus: &mut B,
        rings: &mut PciRings,
        ri: usize,
    ) -> Rtw89Result<()> {
        let ring = &mut rings.tx[ri];
        let v = bus.r32(ring.idx)?;
        let (cur, mut cnt) = moved(ring.idx, v, ring.len, ring.rp)?;
        ring.rp = cur;
        let wi = self.wd_index(ring);
        let w = &mut self.wd[wi];
        while cnt > 0 {
            cnt -= 1;
            let Some(seq) = w.busy.pop_front() else {
                self.events.push(TxEvent::NoBusyTxwd { ring: w.name });
                break;
            };
            let Some(p) = w.pages.get_mut(seq as usize) else {
                continue;
            };
            p.on = Place::None;
            // this skb has been freed by RPP
            if p.skb.is_none() {
                w.enqueue(seq);
            }
        }
        Ok(())
    }

    // Linux: rtw89_pci_release_tx (pci.c:697). Not Linux: a bus error while a buffer is
    // read stops there; the buffers taken before it go back to the chip (index write)
    // and their reports are kept, the failing buffer is the next call's first, so a
    // lasting fault comes back there as the error (only a call that took nothing
    // returns it). When the index write fails after buffers were taken, the RPQ is
    // marked `idx_stale` and the next recalc writes it first.
    pub(super) fn release_tx<B: Bus + ?Sized>(
        &mut self,
        bus: &mut B,
        rings: &mut PciRings,
        cnt: u32,
    ) -> Rtw89Result<()> {
        let mut cnt = cnt;
        let mut taken = 0u32;
        let mut failed = None;
        while cnt > 0 {
            match self.release_tx_skbs(bus, rings) {
                Ok(0) => {
                    self.events.push(TxEvent::ReleaseFailed { skipped: cnt });
                    // skip the rest RXBD bufs (rtw89_pci_rxbd_increase)
                    let rx = rings.rx_mut("RPQ")?;
                    rx.wp = (rx.wp + cnt) % rx.len;
                    taken += cnt;
                    break;
                }
                Ok(n) => {
                    cnt = cnt.saturating_sub(n);
                    taken += n;
                }
                Err(e) => {
                    failed = Some(e);
                    break;
                }
            }
        }
        if taken == 0
            && let Some(e) = failed
        {
            return Err(e);
        }
        let rx = rings.rx_mut("RPQ")?;
        if bus.w16(rx.idx, rx.wp).is_err() || (failed.is_none() && bus.flush().is_err()) {
            rx.idx_stale = true;
        }
        if let Some(e) = failed {
            self.events.push(TxEvent::Fault(e));
        }
        Ok(())
    }

    // Linux: rtw89_pci_release_tx_skbs (pci.c:644): one RPQ buffer. The buffer counts
    // as taken once it is read: its reports are handed on even if clearing its
    // rxbd_info fails afterwards (that failure becomes a `Fault` event), so a report is
    // never processed twice.
    fn release_tx_skbs<B: Bus + ?Sized>(
        &mut self,
        bus: &mut B,
        rings: &mut PciRings,
    ) -> Rtw89Result<u32> {
        let rx = rings.rx_mut("RPQ")?;
        // rtw89_pci_get_rx_skb_idx
        let Some(&buf) = rx.bufs.get(rx.wp as usize) else {
            return Err(Rtw89Error::Invalid {
                what: "RPQ write index past the ring",
                value: rx.wp,
            });
        };
        let size = rx.size;
        let mut mem = vec![0u8; RPQ_HEAD.min(size) as usize];
        bus.mem_read_bytes(buf, &mut mem)?;
        // rtw89_pci_rxbd_info_update; rtw89_pci_validate_rx_tag and target_rx_tag:
        // check_rx_tag is false for the 8852BE
        let info = le32(&mem, 0).unwrap_or(0);
        let fs = info & RTW89_PCI_RXBD_FS != 0;
        let ls = info & RTW89_PCI_RXBD_LS != 0;
        // the chip's length is cut to the buffer: never a read past it
        let len = field(info, RTW89_PCI_RXBD_WRITE_SIZE).min(size) as usize;
        if !fs || !ls {
            self.events.push(TxEvent::NotFsLs { info });
            return Ok(0);
        }
        if len > mem.len() {
            mem = vec![0u8; len];
            bus.mem_read_bytes(buf, &mut mem)?;
        }
        // first segment has RX desc
        let mut off = rpp_start(&mem);
        while off + RPP_SIZE <= len {
            let Some(rpp) = le32(&mem, off) else { break };
            self.release_rpp(bus, rings, rpp);
            off += RPP_SIZE;
        }
        // not Linux (as pci::rx_poll): a stale rxbd_info never looks new
        if let Err(e) = bus.mw32(buf, 0) {
            self.events.push(TxEvent::Fault(e.into()));
        }
        // rtw89_pci_rxbd_increase
        let rx = rings.rx_mut("RPQ")?;
        rx.wp = if rx.wp + 1 >= rx.len {
            rx.wp + 1 - rx.len
        } else {
            rx.wp + 1
        };
        Ok(1)
    }

    // Linux: rtw89_pci_release_rpp (pci.c:596) with rtw89_pci_parse_rpp (pci.c:572).
    // Linux's seq check is against its 512 pages (RTW89_PCI_TXWD_NUM_MAX); here a ring
    // has `len` pages (R1: configurable), so the check is against the ring's pages, and a
    // seq Linux would take but this ring has no page for is "no WD page".
    fn release_rpp<B: Bus + ?Sized>(&mut self, bus: &mut B, rings: &mut PciRings, rpp: u32) {
        let seq = field(rpp, RTW89_PCI_RPP_SEQ);
        let qsel = field(rpp, RTW89_PCI_RPP_QSEL);
        let tx_status = field(rpp, RTW89_PCI_RPP_TX_STATUS);
        // rtw89_chip_get_ch_dma
        let txch = match ch_dma(qsel) {
            Some(c) => c,
            None => {
                self.events.push(TxEvent::QselUnmapped { qsel });
                RTW89_TXCH_ACH0
            }
        };
        if txch >= RTW89_TXCH_NUM || TX_DMA_CH_MASK & (1 << txch) != 0 {
            self.events.push(TxEvent::MaskedTxch { txch });
            return;
        }
        let Some(ri) = rings.tx.iter().position(|r| u32::from(r.ch) == txch) else {
            self.events.push(TxEvent::NoWdPage { txch, seq });
            return;
        };
        let wi = self.wd_index(&rings.tx[ri]);
        let pages = self.wd[wi].pages.len() as u32;
        if seq >= pages {
            self.events
                .push(if seq >= RTW89_PCI_TXWD_NUM_MAX.max(pages) {
                    TxEvent::InvalidSeq { seq }
                } else {
                    TxEvent::NoWdPage { txch, seq }
                });
            return;
        }
        self.release_txwd_skb(bus, rings, ri, wi, seq, tx_status);
    }

    // Linux: rtw89_pci_release_txwd_skb (pci.c:538) with rtw89_pci_tx_status (pci.c:460).
    // A TX BD index that cannot be read (or lies past the ring) is taken as "not moved":
    // the page stays busy until a later reclaim; the report is still delivered.
    fn release_txwd_skb<B: Bus + ?Sized>(
        &mut self,
        bus: &mut B,
        rings: &mut PciRings,
        ri: usize,
        wi: usize,
        seq: u32,
        tx_status: u32,
    ) {
        let si = seq as usize;
        if self.wd[wi].pages[si].on != Place::None {
            if let Err(e) = self.reclaim_txbd(bus, rings, ri) {
                self.events.push(TxEvent::Fault(e));
            }
            // In low power mode, RPP can receive before updating of TX BD.
            // BitsOS has no low power mode: always a warning.
            if self.wd[wi].pages[si].on != Place::None {
                self.events.push(TxEvent::NotIdle {
                    ch: rings.tx[ri].ch,
                    seq,
                });
            }
        }
        let w = &mut self.wd[wi];
        if let Some(skb) = w.pages[si].skb.take() {
            self.done.push(TxReport {
                ch: w.name,
                seq,
                tx_status,
                acked: tx_status == RTW89_TX_DONE,
                len: skb.len,
                qsel: skb.qsel,
                mac_id: skb.mac_id,
                wlan_seq: skb.wlan_seq,
            });
        }
        if w.pages[si].on == Place::None {
            w.enqueue(seq);
        }
    }
}
