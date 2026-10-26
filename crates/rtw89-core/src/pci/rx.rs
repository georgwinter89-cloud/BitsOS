//! Taking new buffers off an RX ring (JS js/pci.mjs `pciRxPoll`). The RPQ
//! ring (release reports) belongs to the TX path (pcitx); this takes RXQ.

use alloc::vec;
use alloc::vec::Vec;

use crate::bus::{Bus, field};
use crate::error::{Rtw89Error, Rtw89Result};
use crate::tables::regs::*;

use super::rings::PciRings;

/// struct rtw89_pci_rxbd_info: the dword the chip writes in front of a buffer's data
pub const RXBD_INFO: usize = 4;

/// One buffer the chip filled
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RxBuf {
    /// what follows the 4-byte rxbd_info: `len - 4` bytes (none when `len` < 4)
    pub data: Vec<u8>,
    /// first / last segment of a frame (RTW89_PCI_RXBD_FS / _LS)
    pub fs: bool,
    pub ls: bool,
    pub tag: u32,
    /// RTW89_PCI_RXBD_WRITE_SIZE cut to the buffer: counts the rxbd_info too
    pub len: u32,
}

// Linux: rtw89_pci_napi_poll's RXQ part (pci.c:4533: isr_clear_rxq of
// rtw89_pci_isr_ax), rtw89_pci_poll_rxq_dma (pci.c:436) with
// rtw89_pci_rxbd_recalc (pci.c:146) / rtw89_pci_dma_recalc (pci.c:61), and per
// buffer rtw89_pci_rxbd_deliver_skbs (pci.c:321: rxbd_info, FS/LS), then the
// index write back (pci.c:413). At most `max` buffers (the NAPI budget). Not
// Linux: an index past the ring is an error (spec/wlan.md §2.3), where the JS
// and Linux would count it; the buffer's rxbd_info is cleared after the copy,
// so a stale one never looks new.
//
// A bus error never drops a buffer already taken: a buffer counts as taken
// once it is copied and its rxbd_info cleared. When a later buffer fails, the
// poll stops there, gives the taken ones back to the chip (index write) and
// returns them; the failing buffer is the first one of the next poll, so a
// lasting fault comes back there as the error. When the index write itself
// fails after buffers were taken, they are still returned and the ring is
// marked `idx_stale`: the next poll writes the index first (and returns that
// error if it fails again), so the chip always gets its slots back. Only a
// poll that took nothing returns an error.
pub fn rx_poll<B: Bus + ?Sized>(
    bus: &mut B,
    rings: &mut PciRings,
    name: &str,
    max: usize,
) -> Rtw89Result<Vec<RxBuf>> {
    let ring = rings.rx_mut(name)?;
    if ring.len == 0 || ring.bufs.len() != ring.len as usize {
        return Err(Rtw89Error::Invalid {
            what: "RX ring without its buffers",
            value: ring.len,
        });
    }
    if ring.idx_stale {
        bus.w16(ring.idx, ring.wp)?;
        ring.idx_stale = false;
    }
    if name == "RXQ" {
        bus.w32(
            R_AX_PCIE_HISR00,
            B_AX_RXP1DMA_INT | B_AX_RXDMA_INT | B_AX_RDU_INT,
        )?;
    }
    let idx = bus.r32(ring.idx)?;
    let cur = field(idx, TXBD_HW_IDX_MASK);
    if cur >= ring.len || ring.wp >= ring.len {
        return Err(Rtw89Error::RingIndex {
            reg: ring.idx,
            value: idx,
            len: ring.len,
        });
    }
    let cnt = if cur >= ring.wp {
        cur - ring.wp
    } else {
        ring.len - (ring.wp - cur)
    };
    ring.rp = cur;
    let cnt = (cnt as usize).min(max);
    let mut out = Vec::with_capacity(cnt);
    let mut failed = None;
    for _ in 0..cnt {
        match take(bus, ring.bufs[ring.wp as usize], ring.size) {
            Ok(b) => out.push(b),
            Err(e) => {
                failed = Some(e);
                break;
            }
        }
        ring.wp = (ring.wp + 1) % ring.len;
    }
    if out.is_empty() {
        if let Some(e) = failed {
            return Err(e);
        }
        bus.flush()?;
        return Ok(out);
    }
    // the taken buffers go back to the chip (a failing one is the next poll's
    // first); when that does not reach the chip, the next poll repeats it
    if bus.w16(ring.idx, ring.wp).is_err() || (failed.is_none() && bus.flush().is_err()) {
        ring.idx_stale = true;
    }
    Ok(out)
}

// rtw89_pci_rxbd_deliver_skbs (pci.c:321) for one buffer: copy, then clear its rxbd_info
fn take<B: Bus + ?Sized>(bus: &mut B, buf: u32, size: u32) -> Rtw89Result<RxBuf> {
    let mut head = [0u8; RXBD_INFO];
    bus.mem_read_bytes(buf, &mut head)?;
    let info = u32::from_le_bytes(head);
    // the chip's length is cut to the buffer: never a read past it
    let len = field(info, RTW89_PCI_RXBD_WRITE_SIZE).min(size);
    let mut data = vec![0u8; (len as usize).saturating_sub(RXBD_INFO)];
    if !data.is_empty() {
        let at = buf
            .checked_add(RXBD_INFO as u32)
            .ok_or(Rtw89Error::BadDmaAddress { addr: buf })?;
        bus.mem_read_bytes(at, &mut data)?;
    }
    bus.mw32(buf, 0)?;
    Ok(RxBuf {
        data,
        fs: info & RTW89_PCI_RXBD_FS != 0,
        ls: info & RTW89_PCI_RXBD_LS != 0,
        tag: field(info, RTW89_PCI_RXBD_TAG),
        len,
    })
}
