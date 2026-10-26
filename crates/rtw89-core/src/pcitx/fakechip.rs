//! Host tests only: js/test-pcitx.mjs's fake chip on [`MemBus`] (it takes every
//! kicked BD and answers with one release report per frame on RPQ), its RPQ
//! buffer layout, and TX descriptor infos built by hand in place of txrx's
//! `txInfo` (the TX descriptor builder is the txrx module's).

use alloc::collections::VecDeque;
use alloc::vec::Vec;

use crate::bus::{Bus, field, prep};
use crate::membus::MemBus;
use crate::pci::fwcmd::TxDescInfo;
use crate::pci::rings::PciRings;
use crate::tables::regs::*;

use super::release::ch_dma_or_ach0;

/// rxbd_info of an RPQ buffer as the test's chip writes it: FS | LS | TAG 1
pub const RPQ_INFO: u32 = 0x0001_c000;

/// One release report: SEQ << 16 | TX_STATUS << 13 | QSEL << 8
pub fn rpp(seq: u32, qsel: u32, status: u32) -> u32 {
    prep(RTW89_PCI_RPP_SEQ, seq)
        | prep(RTW89_PCI_RPP_TX_STATUS, status)
        | prep(RTW89_PCI_RPP_QSEL, qsel)
}

/// One RPQ buffer (test-pcitx.mjs `rpqBuf`): rxbd_info `info` | WRITE_SIZE (4 + 16 + 4 per
/// report), a short RX descriptor RPKT_TYPE 7 (TX_REL_HOST) << 24 | RPKT_LEN, the reports
pub fn rpq_buf(rpps: &[u32], info: u32) -> Vec<u8> {
    let n = rpps.len() as u32;
    let mut words = alloc::vec![info | (20 + 4 * n), 0x0700_0000 | (4 * n), 0, 0, 0];
    words.extend_from_slice(rpps);
    words.iter().flat_map(|w| w.to_le_bytes()).collect()
}

/// A management frame's info as txrx's txInfo gives it on CH8 (B0_MGMT, HW SSN, fixed rate)
pub fn mgmt_info(len: usize, mac_id: u32, seq: u32) -> TxDescInfo {
    TxDescInfo {
        ch_dma: RTW89_TXCH_CH8,
        wd_page: true,
        hw_ssn_sel: RTW89_MGMT_HW_SSN_SEL,
        hw_seq_mode: RTW89_MGMT_HW_SEQ_MODE,
        qsel: RTW89_TX_QSEL_B0_MGMT,
        pkt_size: len as u32,
        mac_id,
        seq,
        wd_info: Some([
            RTW89_TXWD_INFO0_USE_RATE | RTW89_TXWD_INFO0_DISDATAFB,
            0,
            0,
            0,
            RTW89_TXWD_INFO4_HW_RTS_EN,
            0,
        ]),
        ..TxDescInfo::default()
    }
}

/// rtw89_core_get_qsel (txrx.h:833)
pub fn qsel_of_tid(tid: u32) -> u32 {
    match tid {
        1 | 2 => RTW89_TX_QSEL_BK_0,
        4 | 5 => RTW89_TX_QSEL_VI_0,
        6 | 7 => RTW89_TX_QSEL_VO_0,
        _ => RTW89_TX_QSEL_BE_0,
    }
}

/// A data frame's info as txInfo gives it for `tid` (24-byte header: HDR_LLC_LEN 12)
pub fn data_info(tid: u32, len: usize, mac_id: u32, seq: u32) -> TxDescInfo {
    let qsel = qsel_of_tid(tid);
    TxDescInfo {
        ch_dma: ch_dma_or_ach0(qsel),
        hdr_llc_len: 12,
        wd_page: true,
        tid_indicate: matches!(tid, 2 | 3 | 5 | 7),
        qsel,
        pkt_size: len as u32,
        mac_id,
        seq,
        wd_info: Some([
            0,
            0,
            0,
            0,
            RTW89_TXWD_INFO4_RTS_EN | RTW89_TXWD_INFO4_HW_RTS_EN,
            0,
        ]),
        ..TxDescInfo::default()
    }
}

/// test-pcitx.mjs `chipRun`: every BD the host kicked is taken (its WP info seq and
/// WD body QSEL make a report), then the reports go into RPQ buffers, one buffer
/// each, while one RPQ buffer stays free
#[derive(Debug, Default)]
pub struct TxChip {
    pub rpq_hw: u32,
    pub pending: VecDeque<u32>,
}

impl TxChip {
    /// The chip's indexes all start at 0 (forced reads of every TX ring and RPQ)
    pub fn new(bus: &mut MemBus, rings: &PciRings) -> TxChip {
        for r in &rings.tx {
            bus.forced.insert(r.idx, 0);
        }
        if let Some(rpq) = rings.rx("RPQ") {
            bus.forced.insert(rpq.idx, 0);
        }
        TxChip::default()
    }

    pub fn run(&mut self, bus: &mut MemBus, rings: &PciRings) {
        for rg in rings.tx.iter().filter(|r| r.name != "CH12") {
            let host = bus.regs.get(&rg.idx).copied().unwrap_or(0) & 0xffff;
            let mut cur = bus.forced.get(&rg.idx).copied().unwrap_or(0) >> 16;
            while cur != host && cur < rg.len {
                let mut bd = [0u8; 8];
                bus.mem_read_bytes(rg.bd + cur * 8, &mut bd).unwrap();
                let len = u16::from_le_bytes([bd[0], bd[1]]) as usize;
                let at = u32::from_le_bytes([bd[4], bd[5], bd[6], bd[7]]);
                let mut wd = alloc::vec![0u8; len];
                bus.mem_read_bytes(at, &mut wd).unwrap();
                let seq = u32::from(u16::from_le_bytes([wd[len - 16], wd[len - 15]]) & 0x7fff);
                let body2 = u32::from_le_bytes([wd[8], wd[9], wd[10], wd[11]]);
                self.pending
                    .push_back(rpp(seq, field(body2, RTW89_TXWD_BODY2_QSEL), 0));
                cur = (cur + 1) % rg.len;
            }
            bus.forced.insert(rg.idx, cur << 16);
        }
        let rpq = rings.rx("RPQ").unwrap();
        while !self.pending.is_empty() && (self.rpq_hw + 1) % rpq.len != rpq.wp {
            let r = self.pending.pop_front().unwrap();
            bus.mem_write_bytes(rpq.bufs[self.rpq_hw as usize], &rpq_buf(&[r], RPQ_INFO))
                .unwrap();
            self.rpq_hw = (self.rpq_hw + 1) % rpq.len;
        }
        bus.forced.insert(rpq.idx, self.rpq_hw << 16);
    }
}
