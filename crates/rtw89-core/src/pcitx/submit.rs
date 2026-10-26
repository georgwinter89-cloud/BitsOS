//! Putting a frame on its TX ring (JS `pciTx`, `pciTxKick`) and the resource
//! check before it (JS `pciTxResource`).

use alloc::vec;

use crate::bus::{Bus, prep};
use crate::error::{Rtw89Error, Rtw89Result};
use crate::pci::fwcmd::{TxDescInfo, WD_BODY, WD_INFO, avail_txbd, fill_txdesc};
use crate::pci::rings::PciRings;
use crate::tables::regs::*;

use super::{ADDR_INFO, FRAME_ROOM, PciTx, Place, Queued, TxEvent, TxSkb, WD_PAGE, WP_INFO};
use super::{release::ch_dma_or_ach0, tx_ring_index};

fn refuse(what: &'static str, ch: u32, value: u32) -> Rtw89Error {
    Rtw89Error::TxRefused { what, ch, value }
}

impl PciTx {
    // Linux: rtw89_pci_tx_write (pci.c:1637) -> rtw89_pci_txbd_submit (pci.c:1590) ->
    // rtw89_pci_txwd_submit (pci.c:1494), then, when `kick`, rtw89_pci_ops_tx_kick_off
    // (pci.c:1360). `info` comes from the TX descriptor builder (txrx). `Ok(None)` is
    // Linux's -ENOSPC (no BD or no WD page free): the frame is not queued, mac80211
    // would drop it; nothing on the chip changed (a `TxEvent` says which).
    //
    // Refused with `TxRefused` before anything is taken or written: the firmware
    // command channel (only `pci::fwcmd` uses CH12), a frame for another ring, and the
    // host checks Linux has by construction (on the chip either could stall TX):
    // QSEL must pick this channel's flow-control quota and TXPKT_SIZE must equal the
    // frame (= the address info's length); a frame longer than the buffer's room.
    //
    // A bus error after a WD page was taken and before the BD is complete puts the
    // page back (Linux's err_enqueue_wd) and leaves the host index where it was, so
    // the chip never sees the half-written BD. A failing kick still returns
    // `Ok(Some(q))` with `q.kicked == false` and the bus error as a `TxEvent::Fault`:
    // the frame is on the ring (BD written, index moved), so the caller retries
    // `kick` for this ring and must not call `tx` again for the frame.
    pub fn tx<B: Bus + ?Sized>(
        &mut self,
        bus: &mut B,
        rings: &mut PciRings,
        ch: u32,
        info: &TxDescInfo,
        frame: &[u8],
        kick: bool,
    ) -> Rtw89Result<Option<Queued>> {
        let ri = tx_ring_index(rings, ch)?;
        let ring = &rings.tx[ri];
        let ring_ch = u32::from(ring.ch);
        if ring_ch == RTW89_TXCH_CH12 || info.ch_dma == RTW89_DMA_H2C {
            return Err(refuse("only fw cmd uses dma channel 12", ch, info.ch_dma));
        }
        if info.ch_dma != ring_ch {
            return Err(refuse("frame for another DMA channel", ch, info.ch_dma));
        }
        if ch_dma_or_ach0(info.qsel) != info.ch_dma {
            return Err(refuse("QSEL of another DMA channel", ch, info.qsel));
        }
        let flen = u32::try_from(frame.len()).unwrap_or(u32::MAX);
        if info.pkt_size != flen {
            return Err(refuse(
                "TXPKT_SIZE is not the frame's length",
                ch,
                info.pkt_size,
            ));
        }
        if frame.len() > FRAME_ROOM {
            return Err(refuse("frame longer than a TX buffer", ch, flen));
        }
        if ring.len == 0 || ring.wp >= ring.len || ring.bufs.len() != ring.len as usize {
            return Err(Rtw89Error::Invalid {
                what: "TX ring write index past the ring",
                value: ring.wp,
            });
        }
        // rtw89_chip_fill_txdesc of rtw89_pci_txwd_submit, built before a WD page is
        // taken: host work only (addr_info_nr = 1 is not in the AX WD body)
        let txdesc = fill_txdesc(info);
        if avail_txbd(ring.len, ring.wp, ring.rp) == 0 {
            self.events.push(TxEvent::NoTxbd { ring: ring.name });
            return Ok(None);
        }
        // rtw89_pci_get_next_txbd
        let bd = ring
            .wp
            .checked_mul(8)
            .and_then(|o| ring.bd.checked_add(o))
            .ok_or(Rtw89Error::BadDmaAddress { addr: ring.bd })?;
        let wi = self.wd_index(ring);
        let w = &mut self.wd[wi];
        // rtw89_pci_dequeue_txwd
        let Some(seq) = w.free.pop_front() else {
            self.events.push(TxEvent::NoTxwd { ring: ring.name });
            return Ok(None);
        };
        let page_addr = match w.pages.get_mut(seq as usize) {
            Some(p) => {
                p.on = Place::None;
                p.len = 0;
                p.addr
            }
            None => {
                return Err(Rtw89Error::Invalid {
                    what: "WD page seq past the pages",
                    value: seq,
                });
            }
        };
        let page_len = match submit(bus, info, frame, seq, page_addr, &txdesc, bd) {
            Ok(n) => n,
            Err(e) => {
                // Linux: err_enqueue_wd
                w.enqueue(seq);
                return Err(e);
            }
        };
        let p = &mut w.pages[seq as usize];
        p.len = page_len;
        // skb_queue_tail(&txwd->queue, skb), list_add_tail(&txwd->list, &tx_ring->busy_pages)
        p.skb = Some(TxSkb {
            len: flen,
            qsel: info.qsel,
            mac_id: info.mac_id,
            wlan_seq: info.seq,
        });
        p.on = Place::Busy;
        w.busy.push_back(seq);
        let ring = &mut rings.tx[ri];
        // rtw89_pci_tx_bd_ring_update (pci.c:1347)
        ring.wp = if ring.wp + 1 < ring.len {
            ring.wp + 1
        } else {
            ring.wp + 1 - ring.len
        };
        let mut q = Queued {
            ch: ring.name,
            seq,
            wp: ring.wp,
            kicked: false,
        };
        if kick {
            match self.kick(bus, rings, ch) {
                Ok(()) => q.kicked = true,
                Err(e) => self.events.push(TxEvent::Fault(e)),
            }
        }
        Ok(Some(q))
    }

    // Linux: __rtw89_pci_tx_kick_off (pci.c:1332): the ring's host index
    pub fn kick<B: Bus + ?Sized>(
        &mut self,
        bus: &mut B,
        rings: &PciRings,
        ch: u32,
    ) -> Rtw89Result<()> {
        let ring = &rings.tx[tx_ring_index(rings, ch)?];
        bus.w16(ring.idx, ring.wp)?;
        bus.flush()?;
        Ok(())
    }

    // Linux: __rtw89_pci_check_and_reclaim_tx_resource (pci.c:1266): how many frames
    // the ring can take now, after pulling in release reports if needed. hci.paused is
    // never set (BitsOS has no low power mode). CH12 is refused: its resource is
    // `pci::fwcmd`'s own check.
    pub fn resource<B: Bus + ?Sized>(
        &mut self,
        bus: &mut B,
        rings: &mut PciRings,
        ch: u32,
    ) -> Rtw89Result<u32> {
        let ri = tx_ring_index(rings, ch)?;
        if u32::from(rings.tx[ri].ch) == RTW89_TXCH_CH12 {
            return Err(refuse("CH12 takes firmware commands only", ch, ch));
        }
        let wi = self.wd_index(&rings.tx[ri]);
        let avail = |r: &crate::pci::rings::TxRing| avail_txbd(r.len, r.wp, r.rp);
        let mut bd_cnt = avail(&rings.tx[ri]);
        let wd_cnt = self.wd[wi].free.len() as u32;
        if wd_cnt == 0 || bd_cnt == 0 {
            let cnt = self.rpq_recalc(bus, rings)?;
            if cnt > 0 {
                self.release_tx(bus, rings, cnt)?;
            } else if wd_cnt == 0 {
                return Ok(0);
            }
            bd_cnt = avail(&rings.tx[ri]);
            if bd_cnt == 0 {
                self.reclaim_txbd(bus, rings, ri)?;
            }
        }
        let bd_cnt = avail(&rings.tx[ri]);
        let wd_cnt = self.wd[wi].free.len() as u32;
        Ok(bd_cnt.min(wd_cnt))
    }
}

// rtw89_pci_txwd_submit (pci.c:1494) and the BD of rtw89_pci_txbd_submit (pci.c:1590):
// BitsOS has no skb to map, the frame is copied into the buffer behind the WD page
// and that address is the DMA one. Returns the page length the BD carries.
fn submit<B: Bus + ?Sized>(
    bus: &mut B,
    info: &TxDescInfo,
    frame: &[u8],
    seq: u32,
    page_addr: u32,
    txdesc: &[u8],
    bd: u32,
) -> Rtw89Result<u32> {
    let bad = Rtw89Error::BadDmaAddress { addr: page_addr };
    let dma = page_addr.checked_add(WD_PAGE).ok_or(bad.clone())?;
    dma.checked_add(frame.len() as u32).ok_or(bad)?;
    bus.mem_write_bytes(dma, frame)?;
    let txwd_len = WD_BODY + if info.wd_info.is_some() { WD_INFO } else { 0 };
    let mut page = vec![0u8; txwd_len + WP_INFO + ADDR_INFO];
    // WP info: seq0 = seq | VALID, seq1..3 = 0
    let seq0 = ((seq | RTW89_PCI_TXWP_VALID) & 0xffff) as u16;
    page[txwd_len..txwd_len + 2].copy_from_slice(&seq0.to_le_bytes());
    // Linux: rtw89_pci_fill_txaddr_info (pci.c:1438), the 8852BE's fill_txaddr_info:
    // length, option = MSDU_LS | NUM(1) | DMA high bits (0: below 4 GB), dma
    let a = txwd_len + WP_INFO;
    let option = (RTW89_PCI_ADDR_MSDU_LS | 1 | prep(RTW89_PCI_ADDR_HIGH_MASK, 0)) & 0xffff;
    page[a..a + 2].copy_from_slice(&(frame.len() as u16).to_le_bytes());
    page[a + 2..a + 4].copy_from_slice(&(option as u16).to_le_bytes());
    page[a + 4..a + 8].copy_from_slice(&dma.to_le_bytes());
    // rtw89_chip_fill_txdesc
    let n = txdesc.len().min(txwd_len);
    page[..n].copy_from_slice(&txdesc[..n]);
    bus.mem_write_bytes(page_addr, &page)?;
    let len = page.len() as u32;
    // TX BD: length, opt = LS | DMA_HI (0), dma
    let opt = RTW89_PCI_TXBD_OPT_LS | prep(RTW89_PCI_TXBD_OPT_DMA_HI, 0);
    let bd4 = bd
        .checked_add(4)
        .ok_or(Rtw89Error::BadDmaAddress { addr: bd })?;
    bus.mw32(bd, len | (opt << 16))?;
    bus.mw32(bd4, page_addr)?;
    Ok(len)
}
