//! The TX descriptor's WD body (and WD info) and the firmware command
//! channel CH12 (JS js/pci.mjs `fillTxdesc`, `pciFwcmd`).

use alloc::vec::Vec;

use crate::bus::{Bus, field, prep};
use crate::error::{Rtw89Error, Rtw89Result};
use crate::tables::regs::*;

use super::rings::{CMD_BUF, PciRings};

/// struct rtw89_txwd_body: 6 dwords
pub const WD_BODY: usize = 24;
/// struct rtw89_txwd_info: 6 dwords
pub const WD_INFO: usize = 24;

/// The fields of struct rtw89_tx_desc_info that the AX WD body carries, and
/// the WD info dwords (built by the caller, txrx). `wd_info` given = en_wd_info.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TxDescInfo {
    pub wp_offset: u32,
    pub ch_dma: u32,
    pub hdr_llc_len: u32,
    pub wd_page: bool,
    pub fw_dl: bool,
    pub hw_ssn_sel: u32,
    pub hw_seq_mode: u32,
    pub tid_indicate: bool,
    pub qsel: u32,
    pub pkt_size: u32,
    pub mac_id: u32,
    pub seq: u32,
    pub agg_en: bool,
    pub bk: bool,
    pub wd_info: Option<[u32; WD_INFO / 4]>,
}

// Linux: rtw89_core_fill_txdesc (core.c:1637) for AX: rtw89_build_txwd_body0/2/3,
// then the WD info when en_wd_info. Every value is cut to its field.
pub fn fill_txdesc(info: &TxDescInfo) -> Vec<u8> {
    let b = |v: bool| u32::from(v);
    let body0 = prep(RTW89_TXWD_BODY0_WP_OFFSET, info.wp_offset)
        | prep(RTW89_TXWD_BODY0_WD_INFO_EN, b(info.wd_info.is_some()))
        | prep(RTW89_TXWD_BODY0_CHANNEL_DMA, info.ch_dma)
        | prep(RTW89_TXWD_BODY0_HDR_LLC_LEN, info.hdr_llc_len)
        | prep(RTW89_TXWD_BODY0_WD_PAGE, b(info.wd_page))
        | prep(RTW89_TXWD_BODY0_FW_DL, b(info.fw_dl))
        | prep(RTW89_TXWD_BODY0_HW_SSN_SEL, info.hw_ssn_sel)
        | prep(RTW89_TXWD_BODY0_HW_SSN_MODE, info.hw_seq_mode);
    let body2 = prep(RTW89_TXWD_BODY2_TID_INDICATE, b(info.tid_indicate))
        | prep(RTW89_TXWD_BODY2_QSEL, info.qsel)
        | prep(RTW89_TXWD_BODY2_TXPKT_SIZE, info.pkt_size)
        | prep(RTW89_TXWD_BODY2_MACID, info.mac_id);
    let body3 = prep(RTW89_TXWD_BODY3_SW_SEQ, info.seq)
        | prep(RTW89_TXWD_BODY3_AGG_EN, b(info.agg_en))
        | prep(RTW89_TXWD_BODY3_BK, b(info.bk));
    let mut out = Vec::with_capacity(WD_BODY + WD_INFO);
    for dw in [body0, 0, body2, body3, 0, 0] {
        out.extend_from_slice(&dw.to_le_bytes());
    }
    if let Some(wd) = info.wd_info {
        for dw in wd {
            out.extend_from_slice(&dw.to_le_bytes());
        }
    }
    out
}

// Linux: rtw89_h2c_tx + rtw89_pci_fwcmd_submit (pci.c:1551) + the kick off
// (__rtw89_pci_tx_kick_off, pci.c:1332): one buffer on CH12. `payload`: an H2C
// (header + content) or a raw firmware download piece (`fw_dl`). BitsOS then
// waits until the chip's read index passed it, so the slot is free again (not
// Linux: it reclaims on the interrupt). The host index moves before the wait,
// so after a timeout it is ahead of the chip: the firmware download resyncs CH12
// before a retry (spec/wlan.md §2.2 "CH12 resync").
pub fn fwcmd<B: Bus + ?Sized>(
    bus: &mut B,
    rings: &mut PciRings,
    payload: &[u8],
    fw_dl: bool,
    wait_ms: u16,
) -> Rtw89Result<()> {
    let ring = rings.tx_mut("CH12")?;
    let len = WD_BODY + payload.len();
    if len > CMD_BUF {
        return Err(Rtw89Error::Invalid {
            what: "fwcmd longer than a CH12 buffer",
            value: u32::try_from(payload.len()).unwrap_or(u32::MAX),
        });
    }
    let wp = ring.wp as usize;
    let (Some(&buf), true) = (ring.bufs.get(wp), ring.wp < ring.len) else {
        return Err(Rtw89Error::Invalid {
            what: "CH12 write index past the ring",
            value: ring.wp,
        });
    };
    // Linux: rtw89_pci_get_avail_txbd_num (pci.c:1222), checked in
    // rtw89_pci_tx_write (pci.c:1658, -ENOSPC): one BD stays free. After
    // timeouts without a resync the host index runs into buffers the chip has
    // not fetched; they are never overwritten.
    if avail_txbd(ring.len, ring.wp, ring.rp) == 0 {
        return Err(Rtw89Error::RingFull {
            reg: ring.idx,
            wp: ring.wp,
            rp: ring.rp,
        });
    }
    let mut wd = fill_txdesc(&TxDescInfo {
        ch_dma: RTW89_DMA_H2C,
        fw_dl,
        pkt_size: payload.len() as u32,
        ..TxDescInfo::default()
    });
    wd.extend_from_slice(payload);
    // the copy is wiped at once: an H2C can carry a key (SEC_CAM, R8i); DMA memory is the chip's
    let written = bus.mem_write_bytes(buf, &wd);
    wifi::secret::wipe(&mut wd);
    written?;
    let bad = Rtw89Error::BadDmaAddress { addr: ring.bd };
    let bd = ring.bd.checked_add(ring.wp * 8).ok_or(bad.clone())?;
    let bd4 = bd.checked_add(4).ok_or(bad)?;
    // length, opt (LS, DMA high bits 0), dma
    bus.mw32(bd, len as u32 | (RTW89_PCI_TXBD_OPT_LS << 16))?;
    bus.mw32(bd4, buf)?;
    ring.wp = (ring.wp + 1) % ring.len;
    bus.w16(ring.idx, ring.wp)?;
    bus.poll32(
        ring.idx,
        TXBD_HW_IDX_MASK,
        prep(TXBD_HW_IDX_MASK, ring.wp),
        wait_ms,
    )?;
    ring.rp = ring.wp;
    Ok(())
}

// Linux: rtw89_pci_get_avail_txbd_num (pci.c:1222): free BDs, one kept back
// so a full ring never looks empty. An `rp` past the ring counts as full.
pub(crate) fn avail_txbd(len: u32, wp: u32, rp: u32) -> u32 {
    if rp >= len {
        return 0;
    }
    if rp > wp {
        rp - wp - 1
    } else {
        (len - (wp - rp)).saturating_sub(1)
    }
}

// Not Linux (JS js/fw.mjs `ch12Resync`; spec/wlan.md §2.2 "CH12 resync"):
// after a CH12 index poll timed out, the host index is ahead of the chip.
// Before the next try the ring goes back to the host index the chip saw (the
// register's host field) and every kicked buffer must have been fetched within
// `wait_ms` (the JS waits 50 ms). A host field past the ring (all ones: the
// link is gone) is `RingIndex` with the register's value; a chip that does not
// catch up is the poll timeout. Returns the index the ring now stands at.
pub fn fwcmd_resync<B: Bus + ?Sized>(
    bus: &mut B,
    rings: &mut PciRings,
    wait_ms: u16,
) -> Rtw89Result<u32> {
    let ring = rings.tx_mut("CH12")?;
    let v = bus.r32(ring.idx)?;
    let host = field(v, TXBD_HOST_IDX_MASK);
    if host >= ring.len {
        return Err(Rtw89Error::RingIndex {
            reg: ring.idx,
            value: v,
            len: ring.len,
        });
    }
    ring.wp = host;
    ring.rp = host;
    bus.poll32(
        ring.idx,
        TXBD_HW_IDX_MASK,
        prep(TXBD_HW_IDX_MASK, host),
        wait_ms,
    )?;
    bus.flush()?;
    Ok(host)
}
