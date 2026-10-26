//! The descriptor level of rtw89 for an AX chip (JS js/txrx-desc.mjs): queue and DMA
//! channel choice, the WD info dwords, and the RX descriptor reader.

use crate::bus::{field, prep};
use crate::tables::regs::*;

use super::TxInfo;

/// 8852B txwd_body_size = sizeof(struct rtw89_txwd_body)
pub const WD_BODY_SIZE: usize = 24;
/// 8852B txwd_info_size = sizeof(struct rtw89_txwd_info)
pub const WD_INFO_SIZE: usize = 24;
/// sizeof(struct rtw89_rxdesc_short)
pub const RXD_SHORT: usize = 16;
/// sizeof(struct rtw89_rxdesc_long)
pub const RXD_LONG: usize = 32;

// Linux: rtw89_core_get_qsel (txrx.h:833)
pub fn get_qsel(tid: u32) -> u32 {
    match tid {
        1 | 2 => RTW89_TX_QSEL_BK_0,
        4 | 5 => RTW89_TX_QSEL_VI_0,
        6 | 7 => RTW89_TX_QSEL_VO_0,
        _ => RTW89_TX_QSEL_BE_0, // 0, 3 and anything else
    }
}

// Linux: rtw89_core_get_tid_indicate (txrx.h:873)
pub fn get_tid_indicate(tid: u32) -> bool {
    matches!(tid, 2 | 3 | 5 | 7)
}

// Linux: rtw89_core_get_ch_dma (core.c:819); the 8852B uses this one for PCIe. A QSEL that maps
// to no channel takes ACH0 as Linux's fallthrough (pcitx's release path logs that case).
pub fn get_ch_dma(qsel: u32) -> u32 {
    crate::pcitx::ch_dma(qsel).unwrap_or(RTW89_TXCH_ACH0)
}

// Linux: rtw89_build_txwd_info0..4 (core.c:1562-1635), the part of rtw89_core_fill_txdesc
// that pci's fill_txdesc takes from the caller. Dword 5 is never written by Linux and stays
// zero (enqueue_txwd's memset). Every value is cut to its field.
pub fn wd_info_dwords(i: &TxInfo) -> [u32; 6] {
    let b = |v: bool| u32::from(v);
    let info0 = prep(RTW89_TXWD_INFO0_USE_RATE, b(i.use_rate))
        | prep(RTW89_TXWD_INFO0_DATA_BW, i.data_bw)
        | prep(RTW89_TXWD_INFO0_GI_LTF, i.gi_ltf)
        | prep(RTW89_TXWD_INFO0_DATA_RATE, i.data_rate)
        | prep(RTW89_TXWD_INFO0_DATA_STBC, b(i.stbc))
        | prep(RTW89_TXWD_INFO0_DATA_LDPC, b(i.ldpc))
        | prep(RTW89_TXWD_INFO0_DISDATAFB, b(i.dis_data_fb))
        | prep(RTW89_TXWD_INFO0_MULTIPORT_ID, i.port);
    let info1 = prep(RTW89_TXWD_INFO1_MAX_AGGNUM, i.ampdu_num)
        | prep(RTW89_TXWD_INFO1_A_CTRL_BSR, b(i.a_ctrl_bsr))
        | prep(
            RTW89_TXWD_INFO1_DATA_RTY_LOWEST_RATE,
            i.data_retry_lowest_rate,
        )
        | prep(RTW89_TXWD_INFO1_DATA_TXCNT_LMT_SEL, b(i.tx_cnt_lmt_en))
        | prep(RTW89_TXWD_INFO1_DATA_TXCNT_LMT, i.tx_cnt_lmt);
    let info2 = prep(RTW89_TXWD_INFO2_AMPDU_DENSITY, i.ampdu_density)
        | prep(RTW89_TXWD_INFO2_SEC_TYPE, i.sec_type)
        | prep(RTW89_TXWD_INFO2_SEC_HW_ENC, b(i.sec_en))
        | prep(RTW89_TXWD_INFO2_SEC_CAM_IDX, i.sec_cam_idx);
    let info3 = prep(RTW89_TXWD_INFO3_SPE_RPT, b(i.report));
    let info4 = prep(RTW89_TXWD_INFO4_RTS_EN, b(!i.is_bmc))
        | prep(RTW89_TXWD_INFO4_HW_RTS_EN, 1)
        | prep(RTW89_TXWD_INFO4_SW_DEFINE, i.sn);
    [info0, info1, info2, info3, info4, 0]
}

/// struct rtw89_rx_desc_info as rtw89_core_query_rxdesc fills it for an AX chip, plus where the
/// packet starts and what the PCIe delivery saw (`copy_len`, `write_size`)
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RxDesc {
    pub pkt_size: u32,
    pub drv_info_size: u32,
    pub long_rxdesc: bool,
    pub pkt_type: u32,
    pub mac_info_valid: bool,
    pub bw: u32,
    pub data_rate: u32,
    pub gi_ltf: u32,
    pub user_id: u32,
    pub sr_en: bool,
    pub ppdu_cnt: u32,
    pub ppdu_type: u32,
    pub free_run_cnt: u32,
    pub icv_err: bool,
    pub crc32_err: bool,
    pub hw_dec: bool,
    pub sw_dec: bool,
    pub addr1_match: bool,
    pub ampdu: bool,
    /// Linux's AX reader never fills shift or bb_sel: they stay 0 from the zeroed desc_info
    pub shift: u32,
    pub bb_sel: bool,
    /// The raw AX_RXD_SHIFT field, kept only to watch on the chip
    pub shift_field: u32,
    /// where the RX descriptor's packet info starts: data_offset + shift + drv info
    pub offset: usize,
    pub rxd_len: usize,
    pub ready: bool,
    // long descriptor only: they keep their last values after a short one (one desc_info per ring)
    pub frame_type: u32,
    pub addr_cam_valid: bool,
    pub addr_cam_id: u32,
    pub sec_cam_id: u32,
    pub mac_id: u32,
    pub rx_pl_id: u32,
    pub sec_type: u32,
    /// the bytes the delivery put into the packet (rtw89_skb_put_rx_data)
    pub copy_len: usize,
    /// RTW89_PCI_RXBD_WRITE_SIZE of the last segment (`None`: a whole packet given directly)
    pub write_size: Option<u32>,
}

fn le32(buf: &[u8], at: usize) -> u32 {
    buf.get(at..at + 4)
        .map_or(0, |b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

// Linux: rtw89_core_query_rxdesc (core.c:4107). `off` is Linux's data_offset: where the RX
// descriptor starts in `buf`. Offsets in the result are in `buf`'s coordinates; the payload
// starts at offset + rxd_len. Like the C it fills `desc` in place: the RXQ path reuses one
// desc_info per ring, so the long-only fields keep their last values after a short
// descriptor. The caller checks that the descriptor is in `buf`; dwords past its end read 0.
pub fn query_rxdesc(buf: &[u8], off: usize, desc: &mut RxDesc) {
    let dw = |i: usize| le32(buf, off + i * 4);
    let (d0, d1, d2, d3) = (dw(0), dw(1), dw(2), dw(3));
    let bit = |v: u32, m: u32| v & m != 0;
    desc.pkt_size = field(d0, AX_RXD_RPKT_LEN_MASK);
    desc.drv_info_size = field(d0, AX_RXD_DRV_INFO_SIZE_MASK);
    desc.long_rxdesc = bit(d0, AX_RXD_LONG_RXD);
    desc.pkt_type = field(d0, AX_RXD_RPKT_TYPE_MASK);
    desc.mac_info_valid = bit(d0, AX_RXD_MAC_INFO_VLD);
    desc.bw = field(d1, AX_RXD_BW_MASK); // 8852B: not RTL8852C, so not the v1 mask
    desc.data_rate = field(d1, AX_RXD_RX_DATARATE_MASK);
    desc.gi_ltf = field(d1, AX_RXD_RX_GI_LTF_MASK);
    desc.user_id = field(d1, AX_RXD_USER_ID_MASK);
    desc.sr_en = bit(d1, AX_RXD_SR_EN);
    desc.ppdu_cnt = field(d1, AX_RXD_PPDU_CNT_MASK);
    desc.ppdu_type = field(d1, AX_RXD_PPDU_TYPE_MASK);
    desc.free_run_cnt = field(d2, AX_RXD_FREERUN_CNT_MASK);
    desc.icv_err = bit(d3, AX_RXD_ICV_ERR);
    desc.crc32_err = bit(d3, AX_RXD_CRC32_ERR);
    desc.hw_dec = bit(d3, AX_RXD_HW_DEC);
    desc.sw_dec = bit(d3, AX_RXD_SW_DEC);
    desc.addr1_match = bit(d3, AX_RXD_A1_MATCH);
    desc.ampdu = bit(d3, AX_RXD_AMPDU);
    desc.shift_field = field(d0, AX_RXD_SHIFT_MASK);
    let shift_len = (desc.shift as usize) << 1; // 2-byte unit
    let drv_info_len = (desc.drv_info_size as usize) << 3; // 8-byte unit
    desc.offset = off + shift_len + drv_info_len;
    desc.rxd_len = if desc.long_rxdesc {
        RXD_LONG
    } else {
        RXD_SHORT
    };
    desc.ready = true;
    if !desc.long_rxdesc {
        return;
    }
    let (d4, d5, d7) = (dw(4), dw(5), dw(7));
    desc.frame_type = field(d4, AX_RXD_TYPE_MASK);
    desc.addr_cam_valid = bit(d5, AX_RXD_ADDR_CAM_VLD);
    desc.addr_cam_id = field(d5, AX_RXD_ADDR_CAM_MASK);
    desc.sec_cam_id = field(d5, AX_RXD_SEC_CAM_IDX_MASK);
    desc.mac_id = field(d5, AX_RXD_MAC_ID_MASK);
    desc.rx_pl_id = field(d5, AX_RXD_RX_PL_ID_MASK);
    desc.sec_type = field(d7, AX_RXD_SEC_TYPE_MASK);
}
