//! Payloads of the H2C commands a station sends (JS js/sta-h2c.mjs; Linux fw.c). Each builder
//! returns an [`H2cMsg`] (category, class, function, acks and the payload) for fw's `h2c_send`,
//! which adds the 8-byte header (rtw89_h2c_pkt_set_hdr) and the sequence number. Payloads start
//! zeroed (rtw89_fw_h2c_alloc_skb).

use alloc::vec;
use alloc::vec::Vec;

use crate::bus::{lowbit, prep};
use crate::fw::H2c;
use crate::tables::regs::*;

use super::cam::{fill_addr_cam_info, fill_bssid_cam_info};
use super::ra::RaInfo;
use super::{ApLink, Vif};

/// One H2C as sta builds it
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct H2cMsg {
    pub h: H2c,
    pub payload: Vec<u8>,
}

fn msg(cat: u32, cls: u32, func: u32, rack: bool, dack: bool, payload: Vec<u8>) -> H2cMsg {
    H2cMsg {
        h: H2c {
            cat,
            cls,
            func,
            rack,
            dack,
        },
        payload,
    }
}

fn mac_msg(cls: u32, func: u32, rack: bool, dack: bool, payload: Vec<u8>) -> H2cMsg {
    msg(H2C_CAT_MAC, cls, func, rack, dack, payload)
}

fn rd(b: &[u8], w: usize) -> u32 {
    u32::from_le_bytes([b[w * 4], b[w * 4 + 1], b[w * 4 + 2], b[w * 4 + 3]])
}
fn wr(b: &mut [u8], w: usize, v: u32) {
    b[w * 4..w * 4 + 4].copy_from_slice(&v.to_le_bytes());
}

// Linux: le32p_replace_bits (include/linux/bitfield.h) on dword `w` of a payload
pub fn set_bits(b: &mut [u8], w: usize, mask: u32, v: u32) {
    let old = rd(b, w);
    wr(b, w, (old & !mask) | prep(mask, v));
}

/// `w` dwords zeroed
fn dwords(w: usize) -> Vec<u8> {
    vec![0u8; w * 4]
}

// Linux: rtw89_fw_h2c_cam (fw.c:2290); AX chip: struct rtw89_h2c_addr_cam_v0 (60 bytes, no
// UPD_MODE word). `sta`: the AP's link for its entry, `scan_addr` the scan's source address.
pub fn h2c_addr_cam(vif: &Vif, sta: Option<&ApLink>, scan_addr: Option<&[u8; 6]>) -> H2cMsg {
    let mut p = dwords(15);
    fill_addr_cam_info(vif, sta, scan_addr, &mut p);
    fill_bssid_cam_info(vif, &mut p);
    mac_msg(
        H2C_CL_MAC_ADDR_CAM_UPDATE,
        H2C_FUNC_MAC_ADDR_CAM_UPD,
        false,
        true,
        p,
    )
}

// Linux: rtw89_fw_h2c_role_maintain (fw.c:4949); station: self_role from the vif
pub fn h2c_role_maintain(vif: &Vif, sta: Option<&ApLink>, upd_mode: u32) -> H2cMsg {
    let v = prep(
        RTW89_H2C_ROLE_MAINTAIN_W0_MACID,
        sta.map_or(vif.mac_id, |s| s.mac_id),
    ) | prep(RTW89_H2C_ROLE_MAINTAIN_W0_SELF_ROLE, vif.self_role)
        | prep(RTW89_H2C_ROLE_MAINTAIN_W0_UPD_MODE, upd_mode)
        | prep(RTW89_H2C_ROLE_MAINTAIN_W0_WIFI_ROLE, vif.wifi_role)
        | prep(RTW89_H2C_ROLE_MAINTAIN_W0_BAND, vif.mac_idx)
        | prep(RTW89_H2C_ROLE_MAINTAIN_W0_PORT, vif.port);
    mac_msg(
        H2C_CL_MAC_MEDIA_RPT,
        H2C_FUNC_MAC_FWROLE_MAINTAIN,
        false,
        true,
        v.to_le_bytes().to_vec(),
    )
}

// Linux: rtw89_fw_h2c_join_info (fw.c:5045); AX chip: struct rtw89_h2c_join (one dword)
pub fn h2c_join_info(vif: &Vif, sta: Option<&ApLink>, dis_conn: bool) -> H2cMsg {
    let v = prep(
        RTW89_H2C_JOININFO_W0_MACID,
        sta.map_or(vif.mac_id, |s| s.mac_id),
    ) | prep(RTW89_H2C_JOININFO_W0_OP, u32::from(dis_conn))
        | prep(RTW89_H2C_JOININFO_W0_BAND, vif.mac_idx)
        | prep(RTW89_H2C_JOININFO_W0_WMM, vif.wmm)
        | prep(RTW89_H2C_JOININFO_W0_TGR, vif.trigger)
        | prep(RTW89_H2C_JOININFO_W0_PORT_ID, vif.port)
        | prep(RTW89_H2C_JOININFO_W0_NET_TYPE, vif.net_type)
        | prep(RTW89_H2C_JOININFO_W0_WIFI_ROLE, vif.wifi_role)
        | prep(RTW89_H2C_JOININFO_W0_SELF_ROLE, vif.self_role);
    mac_msg(
        H2C_CL_MAC_MEDIA_RPT,
        H2C_FUNC_MAC_JOININFO,
        false,
        true,
        v.to_le_bytes().to_vec(),
    )
}

// Linux: rtw89_fw_h2c_macid_pause (fw.c:5180); fw 0.29.29.18 has no MACID_PAUSE_SLEEP: struct
// rtw89_fw_macid_pause_grp, pause_grp[4] then mask_grp[4]
pub fn h2c_macid_pause(sh: u32, grp: u32, pause: bool) -> H2cMsg {
    let mut p = dwords(8);
    let g = (grp & 3) as usize;
    let bit = 1u32.wrapping_shl(sh);
    wr(&mut p, 4 + g, bit);
    if pause {
        wr(&mut p, g, bit);
    }
    mac_msg(H2C_CL_MAC_FW_OFLD, H2C_FUNC_MAC_MACID_PAUSE, true, false, p)
}

// Linux: rtw89_fw_h2c_set_edca (fw.c:5242), H2C_EDCA_LEN 12; `ac` the firmware's AC
pub fn h2c_set_edca(vif: &Vif, ac: u32, val: u32) -> H2cMsg {
    let mut p = dwords(3);
    set_bits(&mut p, 0, 0x3, 0); // RTW89_SET_EDCA_SEL GENMASK(1, 0)
    set_bits(&mut p, 0, 0x8, vif.mac_idx); // _BAND BIT(3)
    set_bits(&mut p, 0, 0x10, 0); // _WMM BIT(4)
    set_bits(&mut p, 0, 0x60, ac); // _AC GENMASK(6, 5)
    wr(&mut p, 1, val); // _PARAM, dword 1
    mac_msg(H2C_CL_MAC_FW_OFLD, H2C_FUNC_USR_EDCA, false, true, p)
}

/// SET_CTRL_INFO_* / SET_CMC_TBL_* of fw.h: (value dword, field mask); the same mask goes into
/// dword + 8 (the firmware's write mask) for every CMC field
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cmc {
    MacId,
    Operation,
    MgqRptEn,
    DisRtsFb,
    DisDataFb,
    RtsTxcntLmtSel,
    RtsRtyLowestRate,
    DataTxcntLmtSel,
    MultiPortId,
    TxpwrMode,
    DataDcm,
    BsrQueueSizeFormat,
    NtxPathEn,
    PathMapA,
    PathMapB,
    PathMapC,
    PathMapD,
    AntselA,
    AntselB,
    AntselC,
    AntselD,
    Uldl,
    DopplerCtrl,
    NominalPktPadding,
    NominalPktPadding40,
    TxpwrTolerence,
    NominalPktPadding80,
    NominalPktPadding160,
}

impl Cmc {
    /// (dword, mask) as fw.h's inline setter writes it
    pub const fn place(self) -> (usize, u32) {
        match self {
            Cmc::MacId => (0, 0x7f),
            Cmc::Operation => (0, 0x80),
            Cmc::MgqRptEn => (1, 1 << 21),
            Cmc::DisRtsFb => (1, 1 << 25),
            Cmc::DisDataFb => (1, 1 << 26),
            Cmc::RtsTxcntLmtSel => (2, 1 << 11),
            Cmc::RtsRtyLowestRate => (2, 0xf000_0000),
            Cmc::DataTxcntLmtSel => (3, 1 << 6),
            Cmc::MultiPortId => (5, 0x7),
            Cmc::TxpwrMode => (5, 0xe00),
            Cmc::DataDcm => (5, 1 << 12),
            Cmc::BsrQueueSizeFormat => (6, 1 << 13),
            Cmc::NtxPathEn => (6, 0xf0000),
            Cmc::PathMapA => (6, 0x30_0000),
            Cmc::PathMapB => (6, 0xc0_0000),
            Cmc::PathMapC => (6, 0x300_0000),
            Cmc::PathMapD => (6, 0xc00_0000),
            Cmc::AntselA => (6, 1 << 28),
            Cmc::AntselB => (6, 1 << 29),
            Cmc::AntselC => (6, 1 << 30),
            Cmc::AntselD => (6, 0x8000_0000),
            Cmc::Uldl => (7, 1 << 17),
            Cmc::DopplerCtrl => (7, 0xc_0000),
            Cmc::NominalPktPadding => (7, 0x30_0000),
            Cmc::NominalPktPadding40 => (7, 0xc0_0000),
            Cmc::TxpwrTolerence => (7, 0xf00_0000),
            Cmc::NominalPktPadding80 => (7, 0xc000_0000),
            Cmc::NominalPktPadding160 => (8, 0x3000_0000),
        }
    }
    /// The fw.h setter's name (SET_CTRL_INFO_* for MAC ID and OPERATION, SET_CMC_TBL_* else)
    pub const fn name(self) -> &'static str {
        match self {
            Cmc::MacId => "SET_CTRL_INFO_MACID",
            Cmc::Operation => "SET_CTRL_INFO_OPERATION",
            Cmc::MgqRptEn => "SET_CMC_TBL_MGQ_RPT_EN",
            Cmc::DisRtsFb => "SET_CMC_TBL_DISRTSFB",
            Cmc::DisDataFb => "SET_CMC_TBL_DISDATAFB",
            Cmc::RtsTxcntLmtSel => "SET_CMC_TBL_RTS_TXCNT_LMT_SEL",
            Cmc::RtsRtyLowestRate => "SET_CMC_TBL_RTS_RTY_LOWEST_RATE",
            Cmc::DataTxcntLmtSel => "SET_CMC_TBL_DATA_TXCNT_LMT_SEL",
            Cmc::MultiPortId => "SET_CMC_TBL_MULTI_PORT_ID",
            Cmc::TxpwrMode => "SET_CMC_TBL_TXPWR_MODE",
            Cmc::DataDcm => "SET_CMC_TBL_DATA_DCM",
            Cmc::BsrQueueSizeFormat => "SET_CMC_TBL_BSR_QUEUE_SIZE_FORMAT",
            Cmc::NtxPathEn => "SET_CMC_TBL_NTX_PATH_EN",
            Cmc::PathMapA => "SET_CMC_TBL_PATH_MAP_A",
            Cmc::PathMapB => "SET_CMC_TBL_PATH_MAP_B",
            Cmc::PathMapC => "SET_CMC_TBL_PATH_MAP_C",
            Cmc::PathMapD => "SET_CMC_TBL_PATH_MAP_D",
            Cmc::AntselA => "SET_CMC_TBL_ANTSEL_A",
            Cmc::AntselB => "SET_CMC_TBL_ANTSEL_B",
            Cmc::AntselC => "SET_CMC_TBL_ANTSEL_C",
            Cmc::AntselD => "SET_CMC_TBL_ANTSEL_D",
            Cmc::Uldl => "SET_CMC_TBL_ULDL",
            Cmc::DopplerCtrl => "SET_CMC_TBL_DOPPLER_CTRL",
            Cmc::NominalPktPadding => "SET_CMC_TBL_NOMINAL_PKT_PADDING",
            Cmc::NominalPktPadding40 => "SET_CMC_TBL_NOMINAL_PKT_PADDING40",
            Cmc::TxpwrTolerence => "SET_CMC_TBL_TXPWR_TOLERENCE",
            Cmc::NominalPktPadding80 => "SET_CMC_TBL_NOMINAL_PKT_PADDING80",
            Cmc::NominalPktPadding160 => "SET_CMC_TBL_NOMINAL_PKT_PADDING160",
        }
    }
    pub const ALL: [Cmc; 28] = [
        Cmc::MacId,
        Cmc::Operation,
        Cmc::MgqRptEn,
        Cmc::DisRtsFb,
        Cmc::DisDataFb,
        Cmc::RtsTxcntLmtSel,
        Cmc::RtsRtyLowestRate,
        Cmc::DataTxcntLmtSel,
        Cmc::MultiPortId,
        Cmc::TxpwrMode,
        Cmc::DataDcm,
        Cmc::BsrQueueSizeFormat,
        Cmc::NtxPathEn,
        Cmc::PathMapA,
        Cmc::PathMapB,
        Cmc::PathMapC,
        Cmc::PathMapD,
        Cmc::AntselA,
        Cmc::AntselB,
        Cmc::AntselC,
        Cmc::AntselD,
        Cmc::Uldl,
        Cmc::DopplerCtrl,
        Cmc::NominalPktPadding,
        Cmc::NominalPktPadding40,
        Cmc::TxpwrTolerence,
        Cmc::NominalPktPadding80,
        Cmc::NominalPktPadding160,
    ];
}

fn cmc(t: &mut [u8], f: Cmc, v: u32) {
    let (w, mask) = f.place();
    set_bits(t, w, mask, v);
    if !matches!(f, Cmc::MacId | Cmc::Operation) {
        set_bits(t, w + 8, mask, mask >> lowbit(mask));
    }
}

// Linux: __rtw89_fw_h2c_set_tx_path (fw.c:3589); 2 RF paths
fn set_tx_path(t: &mut [u8], antenna_tx: u32) {
    let ntx = if antenna_tx != 0 { antenna_tx } else { RF_AB };
    cmc(t, Cmc::NtxPathEn, ntx);
    cmc(t, Cmc::PathMapA, 0);
    cmc(t, Cmc::PathMapB, u32::from(ntx == RF_AB));
    cmc(t, Cmc::PathMapC, 0);
    cmc(t, Cmc::PathMapD, 0);
}

/// H2C_CMC_TBL_LEN
pub const H2C_CMC_TBL_LEN: usize = 68;

// Linux: rtw89_fw_h2c_default_cmac_tbl (fw.c:3613); 8852B: h2c_cctl_func_id
// H2C_FUNC_MAC_CCTLINFO_UD, PCIe: tx_rpt_enabled false. `antenna_tx`: hal->antenna_tx
pub fn h2c_default_cmac_tbl(vif: &Vif, sta: Option<&ApLink>, antenna_tx: u32) -> H2cMsg {
    let mut t = vec![0u8; H2C_CMC_TBL_LEN];
    cmc(&mut t, Cmc::MacId, sta.map_or(vif.mac_id, |s| s.mac_id));
    cmc(&mut t, Cmc::Operation, 1);
    cmc(&mut t, Cmc::TxpwrMode, 0);
    set_tx_path(&mut t, antenna_tx);
    for f in [
        Cmc::AntselA,
        Cmc::AntselB,
        Cmc::AntselC,
        Cmc::AntselD,
        Cmc::MgqRptEn,
        Cmc::DopplerCtrl,
        Cmc::TxpwrTolerence,
    ] {
        cmc(&mut t, f, 0);
    }
    mac_msg(
        H2C_CL_MAC_FR_EXCHG,
        H2C_FUNC_MAC_CCTLINFO_UD,
        false,
        true,
        t,
    )
}

// Linux: rtw89_fw_h2c_assoc_cmac_tbl (fw.c:3889); no HE: all paddings 0, station: ULDL 1
pub fn h2c_assoc_cmac_tbl(vif: &Vif, sta: Option<&ApLink>, band_2g: bool) -> H2cMsg {
    let mut t = vec![0u8; H2C_CMC_TBL_LEN];
    let lowest = if band_2g {
        RTW89_HW_RATE_CCK1
    } else {
        RTW89_HW_RATE_OFDM6
    };
    cmc(&mut t, Cmc::MacId, sta.map_or(vif.mac_id, |s| s.mac_id));
    cmc(&mut t, Cmc::Operation, 1);
    cmc(&mut t, Cmc::DisRtsFb, 1);
    cmc(&mut t, Cmc::DisDataFb, 1);
    cmc(&mut t, Cmc::RtsRtyLowestRate, lowest);
    cmc(&mut t, Cmc::RtsTxcntLmtSel, 0);
    cmc(&mut t, Cmc::DataTxcntLmtSel, 0);
    cmc(&mut t, Cmc::Uldl, 1);
    cmc(&mut t, Cmc::MultiPortId, vif.port);
    for f in [
        Cmc::NominalPktPadding,
        Cmc::NominalPktPadding40,
        Cmc::NominalPktPadding80,
        Cmc::NominalPktPadding160,
    ] {
        cmc(&mut t, f, 0);
    }
    if sta.is_some() {
        cmc(&mut t, Cmc::BsrQueueSizeFormat, 0); // link_sta->he_cap.has_he: never for us
    }
    mac_msg(
        H2C_CL_MAC_FR_EXCHG,
        H2C_FUNC_MAC_CCTLINFO_UD,
        false,
        true,
        t,
    )
}

// Linux: rtw89_fw_h2c_txpath_cmac_tbl (fw.c:4660): the TX paths of a station's CMAC table entry
// after rtw89_phy_tx_path_div_track picked a path (hal->antenna_tx)
pub fn h2c_txpath_cmac_tbl(sta: &ApLink, antenna_tx: u32) -> H2cMsg {
    let mut t = vec![0u8; H2C_CMC_TBL_LEN];
    cmc(&mut t, Cmc::MacId, sta.mac_id);
    cmc(&mut t, Cmc::Operation, 1);
    set_tx_path(&mut t, antenna_tx);
    mac_msg(
        H2C_CL_MAC_FR_EXCHG,
        H2C_FUNC_MAC_CCTLINFO_UD,
        false,
        true,
        t,
    )
}

// Linux: rtw89_fw_h2c_tp_offload (fw.c:5518): struct rtw89_h2c_ofld, the vif's throughput in Mbps
pub fn h2c_tp_offload(mac_id: u32, tx_tp: u32, rx_tp: u32) -> H2cMsg {
    let v = prep(RTW89_H2C_OFLD_W0_MAC_ID, mac_id)
        | prep(RTW89_H2C_OFLD_W0_TX_TP, tx_tp)
        | prep(RTW89_H2C_OFLD_W0_RX_TP, rx_tp);
    mac_msg(
        H2C_CL_MAC_FW_OFLD,
        H2C_FUNC_OFLD_TP,
        false,
        true,
        v.to_le_bytes().to_vec(),
    )
}

// Linux: rtw89_fw_h2c_ra (fw.c:5560); AX: struct rtw89_h2c_ra (v0), no CSI (no beamformer)
pub fn h2c_ra(ra: &RaInfo) -> H2cMsg {
    let mut p = dwords(4);
    let w0 = prep(RTW89_H2C_RA_W0_MODE, ra.mode_ctrl)
        | prep(RTW89_H2C_RA_W0_BW_CAP, ra.bw_cap)
        | prep(RTW89_H2C_RA_W0_MACID, ra.macid)
        | prep(RTW89_H2C_RA_W0_DCM, ra.dcm_cap)
        | prep(RTW89_H2C_RA_W0_ER, ra.er_cap)
        | prep(RTW89_H2C_RA_W0_INIT_RATE_LV, ra.init_rate_lv)
        | prep(RTW89_H2C_RA_W0_UPD_ALL, ra.upd_all)
        | prep(RTW89_H2C_RA_W0_SGI, ra.en_sgi)
        | prep(RTW89_H2C_RA_W0_LDPC, ra.ldpc_cap)
        | prep(RTW89_H2C_RA_W0_STBC, ra.stbc_cap)
        | prep(RTW89_H2C_RA_W0_SS_NUM, ra.ss_num)
        | prep(RTW89_H2C_RA_W0_GILTF, ra.giltf)
        | prep(RTW89_H2C_RA_W0_UPD_BW_NSS_MASK, ra.upd_bw_nss_mask)
        | prep(RTW89_H2C_RA_W0_UPD_MASK, ra.upd_mask);
    wr(&mut p, 0, w0);
    wr(&mut p, 1, ra.ra_mask as u32);
    wr(
        &mut p,
        2,
        ((ra.ra_mask >> 32) as u32) & RTW89_H2C_RA_W2_RAMASK_HI32,
    );
    wr(
        &mut p,
        3,
        prep(RTW89_H2C_RA_W3_FIX_GILTF_EN, ra.fix_giltf_en)
            | prep(RTW89_H2C_RA_W3_FIX_GILTF, ra.fix_giltf),
    );
    msg(
        H2C_CAT_OUTSRC,
        H2C_CL_OUTSRC_RA,
        H2C_FUNC_OUTSRC_RA_MACIDCFG,
        false,
        false,
        p,
    )
}

/// The masks of fw.h's inline setters RTW89_SET_FWCMD_PACKET_OFLD_PKT_IDX/_OP/_LENGTH
pub mod pkt_ofld {
    pub const IDX: u32 = 0xff;
    pub const OP: u32 = 0x700;
    pub const LENGTH: u32 = 0xffff_0000;
}
/// fw.c:7308 H2C_LEN_PKT_OFLD
const H2C_LEN_PKT_OFLD: usize = 4;

// Linux: __rtw89_fw_h2c_add_pkt_offload (fw.c:7347): H2C_LEN_PKT_OFLD + the frame
pub fn h2c_pkt_ofld_add(id: u32, frame: &[u8]) -> H2cMsg {
    let mut p = vec![0u8; H2C_LEN_PKT_OFLD + frame.len()];
    let w = prep(pkt_ofld::IDX, id)
        | prep(pkt_ofld::OP, RTW89_PKT_OFLD_OP_ADD)
        | prep(pkt_ofld::LENGTH, frame.len() as u32);
    wr(&mut p, 0, w);
    p[H2C_LEN_PKT_OFLD..].copy_from_slice(frame);
    mac_msg(H2C_CL_MAC_FW_OFLD, H2C_FUNC_PACKET_OFLD, true, true, p)
}

// Linux: rtw89_fw_h2c_del_pkt_offload (fw.c:7309)
pub fn h2c_pkt_ofld_del(id: u32) -> H2cMsg {
    let w = prep(pkt_ofld::IDX, id) | prep(pkt_ofld::OP, RTW89_PKT_OFLD_OP_DEL);
    mac_msg(
        H2C_CL_MAC_FW_OFLD,
        H2C_FUNC_PACKET_OFLD,
        true,
        true,
        w.to_le_bytes().to_vec(),
    )
}

/// fw.c:3112-3113: H2C_GENERAL_PKT_LEN, H2C_GENERAL_PKT_ID_UND
const H2C_GENERAL_PKT_LEN: usize = 6;
pub const H2C_GENERAL_PKT_ID_UND: u32 = 0xff;

/// The packet offload ids of rtw89_fw_h2c_general_pkt (`None`: the firmware did not take it)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct GeneralPkts {
    pub ps_poll: Option<u32>,
    pub null_data: Option<u32>,
    pub qos_null: Option<u32>,
}

// Linux: rtw89_fw_h2c_general_pkt (fw.c:3114); SET_GENERAL_PKT_* of fw.h
pub fn h2c_general_pkt(macid: u32, ids: &GeneralPkts) -> H2cMsg {
    let mut p = dwords(2);
    let und = |v: Option<u32>| v.unwrap_or(H2C_GENERAL_PKT_ID_UND);
    set_bits(&mut p, 0, 0xff, macid);
    set_bits(&mut p, 0, 0xff00, H2C_GENERAL_PKT_ID_UND); // PROBRSP_ID
    set_bits(&mut p, 0, 0xff_0000, und(ids.ps_poll));
    set_bits(&mut p, 0, 0xff00_0000, und(ids.null_data));
    set_bits(&mut p, 1, 0xff, und(ids.qos_null));
    set_bits(&mut p, 1, 0xff00, H2C_GENERAL_PKT_ID_UND); // CTS2SELF_ID
    p.truncate(H2C_GENERAL_PKT_LEN);
    msg(
        H2C_CAT_MAC,
        H2C_CL_FW_INFO,
        H2C_FUNC_MAC_GENERAL_PKT,
        false,
        true,
        p,
    )
}

/// fw.c:10620 H2C_PKT_DROP_LEN
const H2C_PKT_DROP_LEN: usize = 24;

/// struct rtw89_pkt_drop_params as a station fills it
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PktDrop {
    pub sel: u32,
    pub macid: u32,
    pub band: u32,
    pub port: u32,
    pub mbssid: u32,
    pub tf_trs: u32,
}

// Linux: rtw89_fw_h2c_pkt_drop (fw.c:10621); macid_band_sel[0..3] stay 0 (dwords 2-5)
pub fn h2c_pkt_drop(d: &PktDrop) -> H2cMsg {
    let mut p = vec![0u8; H2C_PKT_DROP_LEN];
    set_bits(&mut p, 0, 0xff, d.sel);
    set_bits(&mut p, 0, 0xff00, d.macid);
    set_bits(&mut p, 0, 0xff_0000, d.band);
    set_bits(&mut p, 0, 0xff00_0000, d.port);
    set_bits(&mut p, 1, 0xff, d.mbssid);
    set_bits(&mut p, 1, 0xff00, d.tf_trs);
    mac_msg(H2C_CL_MAC_FW_OFLD, H2C_FUNC_PKT_DROP, false, false, p)
}

// Linux: rtw89_fw_h2c_set_bcn_fltr_cfg (fw.c:5397); fw has BEACON_LOSS_COUNT_V1 (7-bit count),
// no CQM threshold configured: RTW89_DEFAULT_CQM_THOLD/_HYST
pub fn h2c_bcn_fltr_cfg(vif: &Vif, connect: bool) -> H2cMsg {
    let max_cnt = (1u32 << 7) - 1;
    let cnt = RTW89_BCN_LOSS_CNT.min(max_cnt);
    let thold = RTW89_DEFAULT_CQM_THOLD + MAX_RSSI as i32;
    let c = u32::from(connect);
    let v = prep(RTW89_H2C_BCNFLTR_W0_MON_RSSI, c)
        | prep(RTW89_H2C_BCNFLTR_W0_MON_BCN, c)
        | prep(RTW89_H2C_BCNFLTR_W0_MON_EN, c)
        | prep(
            RTW89_H2C_BCNFLTR_W0_MODE,
            RTW89_BCN_FLTR_OFFLOAD_MODE_DEFAULT,
        )
        | prep(RTW89_H2C_BCNFLTR_W0_BCN_LOSS_CNT_H3, cnt >> 4)
        | prep(RTW89_H2C_BCNFLTR_W0_BCN_LOSS_CNT_L4, cnt & 0xf)
        | prep(RTW89_H2C_BCNFLTR_W0_RSSI_HYST, RTW89_DEFAULT_CQM_HYST)
        | prep(RTW89_H2C_BCNFLTR_W0_RSSI_THRESHOLD, thold as u32)
        | prep(RTW89_H2C_BCNFLTR_W0_MAC_ID, vif.mac_id);
    mac_msg(
        H2C_CL_MAC_FW_OFLD,
        H2C_FUNC_CFG_BCNFLTR,
        false,
        true,
        v.to_le_bytes().to_vec(),
    )
}

// Linux: rtw89_fw_h2c_rssi_offload (fw.c:5472); `rssi_avg` is the PPDU status' (raw, x2)
pub fn h2c_rssi_offload(mac_id: u32, rssi_avg: u32) -> H2cMsg {
    let mut p = dwords(2);
    wr(
        &mut p,
        0,
        prep(RTW89_H2C_OFLD_RSSI_W0_MACID, mac_id) | prep(RTW89_H2C_OFLD_RSSI_W0_NUM, 1),
    );
    wr(
        &mut p,
        1,
        prep(RTW89_H2C_OFLD_RSSI_W1_VAL, (rssi_avg >> RSSI_FACTOR) & 0xff),
    );
    mac_msg(H2C_CL_MAC_FW_OFLD, H2C_FUNC_OFLD_RSSI, false, true, p)
}

// Linux: rtw89_cam_send_sec_key_cmd (cam.c:52): SEC_CAM update, rack 1, dack 0
pub fn h2c_sec_cam(cmd: Vec<u8>) -> H2cMsg {
    mac_msg(H2C_CL_MAC_SEC_CAM, H2C_FUNC_MAC_SEC_UPD, true, false, cmd)
}
