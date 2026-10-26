//! The address, BSSID and security CAM of the RTL8852B as Linux rtw89 cam.c keeps them (JS
//! js/sta-cam.mjs): driver-side bookkeeping plus the H2C payloads that carry them to the
//! firmware. The chip only learns about an entry through rtw89_fw_h2c_cam (`h2c_addr_cam`) and
//! the SEC_CAM H2C (`sec_key_cmd`). The entry counts are the chip's ([`CamSizes`]; 8852B
//! chip_info: acam_num 128, bcam_num 10, scam_num 128), not BitsOS limits.

use alloc::vec;
use alloc::vec::Vec;

use crate::bus::{lowbit, prep};
use crate::error::{Rtw89Error, Rtw89Result};
use crate::tables::regs::*;
use wifi::secret::Secret;

use super::{ApLink, Vif};

/// The entry counts of a chip's address, BSSID and security CAM (struct rtw89_chip_info
/// acam_num, bcam_num, scam_num): the chip's, so another chip brings its own
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CamSizes {
    pub addr: usize,
    pub bssid: usize,
    pub sec: usize,
}

impl CamSizes {
    /// rtw8852b_chip_info (linux-6.6/drivers/net/wireless/realtek/rtw89/rtw8852b.c:2508; acam_num at :2555)
    pub const RTL8852B: CamSizes = CamSizes {
        addr: 128,
        bssid: 10,
        sec: 128,
    };
}
/// RTW89_SEC_CAM_IN_ADDR_CAM: key slots per address CAM entry
pub const SEC_IN_ADDR: usize = RTW89_SEC_CAM_IN_ADDR_CAM as usize;

/// Linux: struct rtw89_addr_cam_entry (core.h), zeroed as kzalloc leaves it
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AddrCam {
    pub idx: u32,
    pub offset: u32,
    pub len: u32,
    pub valid: bool,
    pub addr_mask: u32,
    pub mask_sel: u32,
    pub sec_ent_mode: u32,
    /// bitmap of the key slots in use (sec_cam_map)
    pub sec_cam_map: u32,
    pub sec_ent_keyid: [u32; SEC_IN_ADDR],
    pub sec_ent: [u32; SEC_IN_ADDR],
    pub bssid_cam_idx: u32,
    pub wapi: u32,
}

/// Linux: struct rtw89_bssid_cam_entry (core.h), zeroed
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BssidCam {
    pub idx: u32,
    pub phy_idx: u32,
    pub len: u32,
    pub offset: u32,
    pub valid: bool,
    pub bssid: [u8; 6],
}

/// Linux: struct rtw89_sec_cam_entry
#[derive(Debug, PartialEq, Eq)]
pub struct SecCam {
    pub sec_cam_idx: u32,
    /// RTW89_SEC_KEY_TYPE_*
    pub typ: u32,
    pub len: u32,
    pub offset: u32,
    pub spp_mode: u32,
    /// the key, padded to 32 bytes; wiped when the entry goes (R8i)
    key: Secret<32>,
    /// the key's index (key->keyidx) and whether it is pairwise
    pub key_idx: u32,
    pub pairwise: bool,
}

/// Linux: struct rtw89_cam_info (rtwdev-wide: it survives remove/add_interface)
#[derive(Debug, PartialEq, Eq)]
pub struct CamInfo {
    addr_cam_map: Vec<bool>,
    bssid_cam_map: Vec<bool>,
    sec_cam_map: Vec<bool>,
    sec_entries: Vec<Option<SecCam>>,
}

impl Default for CamInfo {
    fn default() -> CamInfo {
        CamInfo::new(CamSizes::RTL8852B)
    }
}

fn first_free(map: &[bool]) -> Option<usize> {
    map.iter().position(|&b| !b)
}

impl CamInfo {
    /// Linux: rtw89_core_init's zeroed rtw89_cam_info, sized by the chip's chip_info
    pub fn new(sizes: CamSizes) -> CamInfo {
        CamInfo {
            addr_cam_map: vec![false; sizes.addr],
            bssid_cam_map: vec![false; sizes.bssid],
            sec_cam_map: vec![false; sizes.sec],
            sec_entries: (0..sizes.sec).map(|_| None).collect(),
        }
    }
    /// Security CAM entries in use
    pub fn sec_in_use(&self) -> usize {
        self.sec_cam_map.iter().filter(|&&b| b).count()
    }
    /// Linux: rtw89_cam_info.sec_entries lookup of rtw89_cam_sec_key_del (cam.c:537)
    pub fn sec_entry(&self, idx: u32) -> Option<&SecCam> {
        self.sec_entries.get(idx as usize).and_then(Option::as_ref)
    }
}

// Linux: rtw89_cam_init_bssid_cam (cam.c:704)
fn init_bssid_cam(cam: &mut CamInfo, vif: &mut Vif) -> Rtw89Result<()> {
    if vif.bssid_cam.valid {
        return Ok(());
    }
    let idx = first_free(&cam.bssid_cam_map).ok_or(Rtw89Error::Invalid {
        what: "no free BSSID CAM",
        value: cam.bssid_cam_map.len() as u32,
    })?;
    cam.bssid_cam_map[idx] = true;
    vif.bssid_cam = BssidCam {
        idx: idx as u32,
        phy_idx: vif.phy_idx,
        len: BSSID_CAM_ENT_SIZE,
        offset: 0,
        valid: true,
        bssid: vif.bssid,
    };
    Ok(())
}

// Linux: rtw89_cam_init_addr_cam (cam.c:646); 8852B: rtw89_get_addr_cam_entry_size (cam.c:631)
// gives ADDR_CAM_ENT_SIZE
fn init_addr_cam(cam: &mut CamInfo, vif: &mut Vif) -> Rtw89Result<()> {
    if vif.addr_cam.valid {
        return Ok(());
    }
    let idx = first_free(&cam.addr_cam_map).ok_or(Rtw89Error::Invalid {
        what: "no free address CAM",
        value: cam.addr_cam_map.len() as u32,
    })?;
    cam.addr_cam_map[idx] = true;
    vif.addr_cam = AddrCam {
        idx: idx as u32,
        len: ADDR_CAM_ENT_SIZE,
        offset: 0,
        valid: true,
        addr_mask: 0,
        mask_sel: RTW89_NO_MSK,
        sec_ent_mode: RTW89_ADDR_CAM_SEC_NORMAL,
        sec_cam_map: 0,
        sec_ent_keyid: [0; SEC_IN_ADDR],
        sec_ent: [0; SEC_IN_ADDR],
        bssid_cam_idx: vif.bssid_cam.idx,
        wapi: 0,
    };
    Ok(())
}

// Linux: rtw89_cam_init (cam.c:741)
pub fn cam_init(cam: &mut CamInfo, vif: &mut Vif) -> Rtw89Result<()> {
    init_bssid_cam(cam, vif)?;
    init_addr_cam(cam, vif)
}

// Linux: rtw89_cam_deinit (cam.c:597) with deinit_addr_cam / deinit_bssid_cam
pub fn cam_deinit(cam: &mut CamInfo, vif: &mut Vif) {
    vif.addr_cam.valid = false;
    if let Some(b) = cam.addr_cam_map.get_mut(vif.addr_cam.idx as usize) {
        *b = false;
    }
    vif.bssid_cam.valid = false;
    if let Some(b) = cam.bssid_cam_map.get_mut(vif.bssid_cam.idx as usize) {
        *b = false;
    }
}

// Linux: rtw89_cam_bssid_changed (cam.c:734)
pub fn cam_bssid_changed(vif: &mut Vif) {
    vif.bssid_cam.bssid = vif.bssid;
}

fn put(h2c: &mut [u8], w: usize, mask: u32, v: u32) {
    super::h2c::set_bits(h2c, w, mask, v);
}

fn hash(start: u32, a: &[u8; 6]) -> u32 {
    a.iter()
        .skip(start as usize)
        .fold(0u32, |h, &b| h ^ u32::from(b))
}

// Linux: rtw89_cam_fill_addr_cam_info (cam.c:813), addrcam_ver 0 (8852B)
pub fn fill_addr_cam_info(
    vif: &Vif,
    sta: Option<&ApLink>,
    scan_addr: Option<&[u8; 6]>,
    h2c: &mut [u8],
) {
    let ac = &vif.addr_cam;
    let sma = scan_addr.unwrap_or(&vif.mac_addr);
    let tma = sta.map_or(&vif.bssid, |s| &s.addr);
    let (mut sma_start, mut tma_start) = (0, 0);
    if ac.addr_mask != 0 {
        let start = lowbit(ac.addr_mask);
        if ac.mask_sel == RTW89_SMA {
            sma_start = start;
        } else if ac.mask_sel == RTW89_TMA {
            tma_start = start;
        }
    }
    let mac_id = sta.map_or(vif.mac_id, |s| s.mac_id);
    put(h2c, 1, ADDR_CAM_W1_IDX, ac.idx);
    put(h2c, 1, ADDR_CAM_W1_OFFSET, ac.offset);
    put(h2c, 1, ADDR_CAM_W1_LEN, ac.len);
    put(h2c, 2, ADDR_CAM_W2_VALID, u32::from(ac.valid));
    put(h2c, 2, ADDR_CAM_W2_NET_TYPE, vif.net_type);
    put(h2c, 2, ADDR_CAM_W2_BCN_HIT_COND, vif.bcn_hit_cond);
    put(h2c, 2, ADDR_CAM_W2_HIT_RULE, vif.hit_rule);
    put(h2c, 2, ADDR_CAM_W2_BB_SEL, vif.phy_idx);
    put(h2c, 2, ADDR_CAM_W2_ADDR_MASK, ac.addr_mask);
    put(h2c, 2, ADDR_CAM_W2_MASK_SEL, ac.mask_sel);
    put(h2c, 2, ADDR_CAM_W2_SMA_HASH, hash(sma_start, sma));
    put(h2c, 2, ADDR_CAM_W2_TMA_HASH, hash(tma_start, tma));
    put(h2c, 3, ADDR_CAM_W3_BSSID_CAM_IDX, ac.bssid_cam_idx);
    put(h2c, 4, ADDR_CAM_W4_SMA0, sma[0].into());
    put(h2c, 4, ADDR_CAM_W4_SMA1, sma[1].into());
    put(h2c, 4, ADDR_CAM_W4_SMA2, sma[2].into());
    put(h2c, 4, ADDR_CAM_W4_SMA3, sma[3].into());
    put(h2c, 5, ADDR_CAM_W5_SMA4, sma[4].into());
    put(h2c, 5, ADDR_CAM_W5_SMA5, sma[5].into());
    put(h2c, 5, ADDR_CAM_W5_TMA0, tma[0].into());
    put(h2c, 5, ADDR_CAM_W5_TMA1, tma[1].into());
    put(h2c, 6, ADDR_CAM_W6_TMA2, tma[2].into());
    put(h2c, 6, ADDR_CAM_W6_TMA3, tma[3].into());
    put(h2c, 6, ADDR_CAM_W6_TMA4, tma[4].into());
    put(h2c, 6, ADDR_CAM_W6_TMA5, tma[5].into());
    put(h2c, 8, ADDR_CAM_W8_PORT_INT, vif.port);
    put(h2c, 8, ADDR_CAM_W8_TSF_SYNC, vif.port);
    put(h2c, 8, ADDR_CAM_W8_TF_TRS, vif.trigger);
    put(h2c, 8, ADDR_CAM_W8_LSIG_TXOP, vif.lsig_txop);
    put(h2c, 8, ADDR_CAM_W8_TGT_IND, vif.tgt_ind);
    put(h2c, 8, ADDR_CAM_W8_FRM_TGT_IND, vif.frm_tgt_ind);
    put(h2c, 8, ADDR_CAM_W8_MACID, mac_id);
    // station mode: only INFRA carries the AID (AP mode is left out)
    if vif.net_type == RTW89_NET_TYPE_INFRA {
        put(h2c, 9, ADDR_CAM_W9_AID12, u32::from(vif.aid) & 0xfff);
    }
    put(h2c, 9, ADDR_CAM_W9_WOL_PATTERN, vif.wowlan_pattern);
    put(h2c, 9, ADDR_CAM_W9_WOL_UC, vif.wowlan_uc);
    put(h2c, 9, ADDR_CAM_W9_WOL_MAGIC, vif.wowlan_magic);
    put(h2c, 9, ADDR_CAM_W9_WAPI, ac.wapi);
    put(h2c, 9, ADDR_CAM_W9_SEC_ENT_MODE, ac.sec_ent_mode);
    let keyid = [
        ADDR_CAM_W9_SEC_ENT0_KEYID,
        ADDR_CAM_W9_SEC_ENT1_KEYID,
        ADDR_CAM_W9_SEC_ENT2_KEYID,
        ADDR_CAM_W9_SEC_ENT3_KEYID,
        ADDR_CAM_W9_SEC_ENT4_KEYID,
        ADDR_CAM_W9_SEC_ENT5_KEYID,
        ADDR_CAM_W9_SEC_ENT6_KEYID,
    ];
    for (m, v) in keyid.into_iter().zip(ac.sec_ent_keyid) {
        put(h2c, 9, m, v);
    }
    put(h2c, 10, ADDR_CAM_W10_SEC_ENT_VALID, ac.sec_cam_map & 0xff);
    let ent: [(usize, u32); SEC_IN_ADDR] = [
        (10, ADDR_CAM_W10_SEC_ENT0),
        (10, ADDR_CAM_W10_SEC_ENT1),
        (10, ADDR_CAM_W10_SEC_ENT2),
        (11, ADDR_CAM_W11_SEC_ENT3),
        (11, ADDR_CAM_W11_SEC_ENT4),
        (11, ADDR_CAM_W11_SEC_ENT5),
        (11, ADDR_CAM_W11_SEC_ENT6),
    ];
    for ((w, m), v) in ent.into_iter().zip(ac.sec_ent) {
        put(h2c, w, m, v);
    }
}

// Linux: rtw89_cam_fill_bssid_cam_info (cam.c:763); not a nontransmitted BSS, no HE colour
pub fn fill_bssid_cam_info(vif: &Vif, h2c: &mut [u8]) {
    let bc = &vif.bssid_cam;
    put(h2c, 12, ADDR_CAM_W12_BSSID_IDX, bc.idx);
    put(h2c, 12, ADDR_CAM_W12_BSSID_OFFSET, bc.offset);
    put(h2c, 12, ADDR_CAM_W12_BSSID_LEN, bc.len);
    put(h2c, 13, ADDR_CAM_W13_BSSID_VALID, u32::from(bc.valid));
    put(h2c, 13, ADDR_CAM_W13_BSSID_MASK, RTW89_BSSID_MATCH_ALL);
    put(h2c, 13, ADDR_CAM_W13_BSSID_BB_SEL, bc.phy_idx);
    put(h2c, 13, ADDR_CAM_W13_BSSID_BSS_COLOR, 0);
    put(h2c, 13, ADDR_CAM_W13_BSSID_BSSID0, bc.bssid[0].into());
    put(h2c, 13, ADDR_CAM_W13_BSSID_BSSID1, bc.bssid[1].into());
    put(h2c, 14, ADDR_CAM_W14_BSSID_BSSID2, bc.bssid[2].into());
    put(h2c, 14, ADDR_CAM_W14_BSSID_BSSID3, bc.bssid[3].into());
    put(h2c, 14, ADDR_CAM_W14_BSSID_BSSID4, bc.bssid[4].into());
    put(h2c, 14, ADDR_CAM_W14_BSSID_BSSID5, bc.bssid[5].into());
}

// Linux: rtw89_cam_get_sec_key_cmd (cam.c:11): the 24-byte SEC_CAM payload (no ext key)
pub fn sec_key_cmd(sc: &SecCam) -> Vec<u8> {
    let mut cmd = vec![0u8; H2C_SEC_CAM_LEN as usize];
    let w0 = prep(0xff, sc.sec_cam_idx) | prep(0xff00, sc.offset) | prep(0xff_0000, sc.len);
    let w1 = prep(0xf, sc.typ) | prep(0x20, sc.spp_mode); // _TYPE, _EXT_KEY 0, _SPP_MODE
    cmd[0..4].copy_from_slice(&w0.to_le_bytes());
    cmd[4..8].copy_from_slice(&w1.to_le_bytes());
    cmd[8..24].copy_from_slice(&sc.key.expose()[..16]);
    cmd
}

// Linux: rtw89_cam_get_addr_cam_key_idx (cam.c:138), mode NORMAL: 0-1 unicast, 2-4 group, 5-6 BIP
fn addr_cam_key_idx(ac: &AddrCam, sc: &SecCam) -> Rtw89Result<usize> {
    if ac.sec_ent_mode != RTW89_ADDR_CAM_SEC_NORMAL {
        return Err(Rtw89Error::Invalid {
            what: "addr CAM key mode not NORMAL",
            value: ac.sec_ent_mode,
        });
    }
    let (from, last) = if sc.typ == RTW89_SEC_KEY_TYPE_BIP_CCMP128 {
        (5, 6)
    } else if sc.pairwise {
        (0, 1)
    } else {
        (2, 4)
    };
    let idx = (from..SEC_IN_ADDR)
        .find(|&i| ac.sec_cam_map & (1 << i) == 0)
        .unwrap_or(SEC_IN_ADDR);
    if idx > last {
        return Err(Rtw89Error::Invalid {
            what: "no free key slot in the address CAM",
            value: ac.sec_cam_map,
        });
    }
    Ok(idx)
}

// Linux: __rtw89_cam_attach_sec_cam (cam.c:248), the CAM part (the H2Cs are the caller's)
pub fn attach_sec_cam(vif: &mut Vif, cam: &CamInfo, sec_cam_idx: u32) -> Rtw89Result<usize> {
    let sc = cam.sec_entry(sec_cam_idx).ok_or(Rtw89Error::Invalid {
        what: "security CAM entry not in use",
        value: sec_cam_idx,
    })?;
    let ac = &mut vif.addr_cam;
    let k = addr_cam_key_idx(ac, sc)?;
    ac.sec_ent_keyid[k] = sc.key_idx;
    ac.sec_ent[k] = sc.sec_cam_idx;
    ac.sec_cam_map |= 1 << k;
    Ok(k)
}

// Linux: __rtw89_cam_detach_sec_cam (cam.c:215), the CAM part
pub fn detach_sec_cam(vif: &mut Vif, sec_cam_idx: u32) {
    let ac = &mut vif.addr_cam;
    for i in 0..SEC_IN_ADDR {
        if ac.sec_cam_map & (1 << i) != 0 && ac.sec_ent[i] == sec_cam_idx {
            ac.sec_cam_map &= !(1 << i);
        }
    }
}

// Linux: rtw89_cam_sec_key_install (cam.c:399) with rtw89_cam_get_avail_sec_cam (cam.c:102),
// the bookkeeping before the H2C; 128-bit keys only (no ext_key)
pub fn new_sec_entry(
    cam: &mut CamInfo,
    key: &[u8],
    key_idx: u32,
    pairwise: bool,
    typ: u32,
) -> Rtw89Result<u32> {
    if key.len() > 32 {
        return Err(Rtw89Error::Invalid {
            what: "key too long",
            value: key.len() as u32,
        });
    }
    let idx = first_free(&cam.sec_cam_map).ok_or(Rtw89Error::Invalid {
        what: "no free security CAM",
        value: cam.sec_cam_map.len() as u32,
    })?;
    cam.sec_cam_map[idx] = true;
    let mut k = Secret::new([0u8; 32]);
    k.expose_mut()[..key.len()].copy_from_slice(key);
    cam.sec_entries[idx] = Some(SecCam {
        sec_cam_idx: idx as u32,
        typ,
        len: RTW89_SEC_CAM_LEN,
        offset: 0,
        spp_mode: 0,
        key: k,
        key_idx,
        pairwise,
    });
    Ok(idx as u32)
}

// Linux: rtw89_cam_sec_key_del (cam.c:537), the bookkeeping after the detach (the key is wiped)
pub fn free_sec_entry(cam: &mut CamInfo, idx: u32) {
    if let Some(e) = cam.sec_entries.get_mut(idx as usize) {
        *e = None;
    }
    if let Some(b) = cam.sec_cam_map.get_mut(idx as usize) {
        *b = false;
    }
}
