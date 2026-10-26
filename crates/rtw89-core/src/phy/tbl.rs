//! The BB/RF tables of the firmware file's elements and the table walker of Linux
//! phy.c (rtw89_phy_init_reg with its headline and if/elif/else/end blocks), plus the
//! per-entry config functions and the BB gain table (JS js/phy-tbl.mjs).

use alloc::vec::Vec;

use crate::bus::Bus;
use crate::error::Rtw89Result;
use crate::fw::{Firmware, RfRegInfo};
use crate::tables::regs::*;

use super::io::{RFREG_MASK, phy_w32, rf_w};

/// One table of struct rtw89_reg2_def rows (struct rtw89_phy_table)
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RegTable {
    pub regs: Vec<(u32, u32)>,
    /// the RADIO element's path (RADIO_A = 0 ...); 0 for the others
    pub rf_path: u32,
    pub ver: [u8; 4],
}

/// The tables rtw89_fw_recognize_elements takes from the firmware file (struct
/// rtw89_fw_elm_info's bb_tbl, bb_gain, rf_radio[], rf_nctl, afe)
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PhyTables {
    pub bb: Option<RegTable>,
    pub bb_gain: Option<RegTable>,
    /// rf_radio[idx] (RF_PATH_MAX slots)
    pub rf: [Option<RegTable>; 4],
    pub nctl: Option<RegTable>,
    /// the AFE_PWR_SEQ element: its size and union u (8 header bytes, then 24-byte rows)
    pub afe: Option<(u32, Vec<u8>)>,
}

fn le32(b: &[u8], o: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        b.get(o..o.checked_add(4)?)?.try_into().ok()?,
    ))
}

impl PhyTables {
    // Linux: rtw89_build_phy_tbl_from_elm (fw.c:1099), for every element as
    // rtw89_fw_recognize_elements (fw.c:1560) walks them: the first one of a kind wins, one
    // with another aid is ignored. Payload = union u: reg2 { u8 idx; u8 rsvd[7]; regs[] }.
    // The AFE element: rtw89_build_afe_pwr_seq_from_elm (fw.c:1415), the last one wins.
    // Rows past the file's end are not taken; a RADIO idx past rf_radio[] is ignored (Linux
    // would index past its array).
    pub fn from_firmware(fw: &Firmware, aid: u16) -> PhyTables {
        let mut t = PhyTables::default();
        for e in &fw.elements {
            let p = fw.payload(e);
            if e.id == RTW89_FW_ELEMENT_ID_AFE_PWR_SEQ {
                t.afe = Some((e.size, p.to_vec()));
                continue;
            }
            if e.aid != 0 && e.aid != aid {
                continue;
            }
            let (slot, rf_path) = match e.id {
                RTW89_FW_ELEMENT_ID_BB_REG => (&mut t.bb, 0),
                RTW89_FW_ELEMENT_ID_BB_GAIN => (&mut t.bb_gain, 0),
                RTW89_FW_ELEMENT_ID_RADIO_A..=RTW89_FW_ELEMENT_ID_RADIO_D => {
                    let Some(s) = p.first().and_then(|&i| t.rf.get_mut(usize::from(i))) else {
                        continue;
                    };
                    (s, e.id - RTW89_FW_ELEMENT_ID_RADIO_A)
                }
                RTW89_FW_ELEMENT_ID_RF_NCTL => (&mut t.nctl, 0),
                _ => continue,
            };
            if slot.is_some() {
                continue;
            }
            let n = (e.size / 8) as usize;
            let mut regs = Vec::with_capacity(n.min(p.len() / 8));
            for i in 0..n {
                let (Some(a), Some(d)) = (le32(p, 8 + i * 8), le32(p, 12 + i * 8)) else {
                    break;
                };
                regs.push((a, d));
            }
            *slot = Some(RegTable {
                regs,
                rf_path,
                ver: e.ver,
            });
        }
        t
    }
}

// get_phy_headline / get_phy_cond / get_phy_target / ... (phy.h)
const fn cond_of(addr: u32) -> u32 {
    addr >> 28
}
const fn target_of(addr: u32) -> u32 {
    addr & 0x0fff_ffff
}
const fn compare_of(rfe: u32, cv: u32) -> u32 {
    ((rfe & 0xff) << 16) | (cv & 0xff)
}
const fn cond_rfe(addr: u32) -> u32 {
    (addr >> 16) & 0xff
}
const fn cond_cv(addr: u32) -> u32 {
    addr & 0xff
}

/// The headline rtw89_phy_sel_headline picked: how many headlines, and which
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Headline {
    pub size: usize,
    pub idx: usize,
}

// Linux: rtw89_phy_sel_headline (phy.c:1733): `None` when no headline fits (rfe, cv)
pub fn sel_headline(table: &RegTable, rfe: u32, cv: u32) -> Option<Headline> {
    let addr = |i: usize| table.regs[i].0;
    let size = table
        .regs
        .iter()
        .take_while(|(a, _)| cond_of(*a) == PHY_HEADLINE_VALID)
        .count();
    if size == 0 {
        return Some(Headline { size: 0, idx: 0 });
    }
    // case 1: RFE match, CV match; case 2: RFE match, CV don't care
    for compare in [compare_of(rfe, cv), compare_of(rfe, PHY_COND_DONT_CARE)] {
        if let Some(i) = (0..size).find(|&i| target_of(addr(i)) == compare) {
            return Some(Headline { size, idx: i });
        }
    }
    // case 3: RFE match, CV max in table; case 4: RFE don't care, CV max in table
    let mut cv_max = 0;
    let mut idx = None;
    for want in [rfe, PHY_COND_DONT_CARE] {
        for i in 0..size {
            if cond_rfe(addr(i)) == want && cond_cv(addr(i)) >= cv_max {
                cv_max = cond_cv(addr(i));
                idx = Some(i);
            }
        }
        if let Some(i) = idx {
            return Some(Headline { size, idx: i });
        }
    }
    None
}

/// What one table walk applied (JS initReg's result)
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RegLoad {
    pub applied: u32,
    pub first: Option<(u32, u32)>,
    pub last: Option<(u32, u32)>,
    /// Linux's walk stopped: no headline for (rfe, cv), or an ELSE without a matched branch
    pub aborted: bool,
    pub headline: Option<Headline>,
    /// RADIO tables: the values stored for the firmware and whether their H2C went out
    pub h2c_values: u32,
    pub h2c_ok: bool,
}

/// Why a walk stopped (Linux's two messages of rtw89_phy_init_reg)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WalkStop {
    /// "invalid PHY package: rfe/cv"
    InvalidPackage { rfe: u32, cv: u32 },
    /// "failed to load CR addr/data"
    LoadCr { addr: u32, data: u32 },
}

// Linux: rtw89_phy_init_reg (phy.c:1815). `config(addr, data)` runs for every plain entry of
// the selected branches, in table order. Returns what was applied and, when Linux stops the
// walk (aborted), why; an error of `config` ends the walk.
pub fn init_reg(
    table: &RegTable,
    rfe: u32,
    cv: u32,
    mut config: impl FnMut(u32, u32) -> Rtw89Result<()>,
) -> Rtw89Result<(RegLoad, Option<WalkStop>)> {
    let mut res = RegLoad::default();
    let Some(head) = sel_headline(table, rfe, cv) else {
        res.aborted = true;
        return Ok((res, Some(WalkStop::InvalidPackage { rfe, cv })));
    };
    res.headline = Some(head);
    let cfg_target = target_of(table.regs.get(head.idx).map_or(0, |r| r.0));
    let (mut target, mut is_matched, mut target_found) = (0, true, false);
    for &(addr, data) in table.regs.iter().skip(head.size) {
        match cond_of(addr) {
            PHY_COND_BRANCH_IF | PHY_COND_BRANCH_ELIF => target = target_of(addr),
            PHY_COND_BRANCH_ELSE => {
                is_matched = false;
                if !target_found {
                    res.aborted = true;
                    return Ok((res, Some(WalkStop::LoadCr { addr, data })));
                }
            }
            PHY_COND_BRANCH_END => {
                is_matched = true;
                target_found = false;
            }
            PHY_COND_CHECK => {
                if target_found {
                    is_matched = false;
                } else if target == cfg_target {
                    is_matched = true;
                    target_found = true;
                } else {
                    is_matched = false;
                    target_found = false;
                }
            }
            _ if is_matched => {
                config(addr, data)?;
                res.applied += 1;
                res.first.get_or_insert((addr, data));
                res.last = Some((addr, data));
            }
            _ => {}
        }
    }
    Ok((res, None))
}

/// rtw89_io_mdelay / rtw89_io_udelay pseudo addresses of the tables -> microseconds
pub fn delay_us_of(addr: u32) -> Option<u32> {
    crate::tables::phy::DELAY_US
        .iter()
        .find(|(a, _)| *a == addr)
        .map(|(_, us)| *us)
}

// Linux: rtw89_phy_config_bb_reg (phy.c:1347), PHY 0 only (dbcc off)
pub fn config_bb_reg<B: Bus + ?Sized>(bus: &mut B, addr: u32, data: u32) -> Rtw89Result<()> {
    if let Some(us) = delay_us_of(addr) {
        bus.delay_us(us)?;
    } else if data != BYPASS_CR_DATA {
        phy_w32(bus, addr, data)?;
    }
    Ok(())
}

// Linux: rtw89_phy_config_rf_reg_v1 (phy.c:1713): only 0xfe is a delay here, every other
// address (0xf9..0xfd too) goes to the RF; from 0x100 up the value is also stored for the
// firmware (rtw89_phy_cofig_rf_reg_store, phy.c:1624). Returns false when the store was full
// (Linux's "RF parameters exceed size"; the value is dropped).
pub fn config_rf_reg_v1<B: Bus + ?Sized>(
    bus: &mut B,
    addr: u32,
    data: u32,
    rf_path: u32,
    info: &mut RfRegInfo,
) -> Rtw89Result<bool> {
    if addr == 0xfe {
        bus.delay_us(50_000)?;
        return Ok(true);
    }
    rf_w(bus, rf_path, addr, RFREG_MASK, data)?;
    if addr < 0x100 {
        return Ok(true);
    }
    Ok(info.store(addr, data))
}

// ---- BB gain table: values for the driver, no register writes ----

const GBANDS: usize = RTW89_BB_GAIN_BAND_NR as usize;
const PATHS: usize = super::efuse::RF_PATHS;
const LNA: usize = LNA_GAIN_NUM as usize;
const TIA: usize = TIA_GAIN_NUM as usize;
const RXSC40: usize = RTW89_BB_RXSC_NUM_40 as usize;
const RXSC80: usize = RTW89_BB_RXSC_NUM_80 as usize;
const RXSC160: usize = RTW89_BB_RXSC_NUM_160 as usize;

// enum rtw89_phy_bb_rxsc_start_idx (phy.c)
const RXSC_START_IDX_FULL: u32 = 0;
const RXSC_START_IDX_20: u32 = 1;
const RXSC_START_IDX_20_1: u32 = 5;
const RXSC_START_IDX_40: u32 = 9;
const RXSC_START_IDX_80: u32 = 13;

/// Linux: struct rtw89_phy_bb_gain_info (core.h:7258), indexed [gain_band][path][i]
/// (the 8852B's two paths)
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BbGain {
    pub lna_gain: [[[i8; LNA]; PATHS]; GBANDS],
    pub tia_gain: [[[i8; TIA]; PATHS]; GBANDS],
    pub lna_gain_bypass: [[[i8; LNA]; PATHS]; GBANDS],
    pub lna_op1db: [[[i8; LNA]; PATHS]; GBANDS],
    pub tia_lna_op1db: [[[i8; LNA + 1]; PATHS]; GBANDS],
    pub rpl_ofst_20: [[i8; PATHS]; GBANDS],
    pub rpl_ofst_40: [[[i8; RXSC40]; PATHS]; GBANDS],
    pub rpl_ofst_80: [[[i8; RXSC80]; PATHS]; GBANDS],
    pub rpl_ofst_160: [[[i8; RXSC160]; PATHS]; GBANDS],
}

// bytes of `data` from the low byte up into arr[from..from+n) (the part inside arr)
fn fill(arr: &mut [i8], from: usize, n: usize, data: u32) {
    for i in 0..n {
        if let Some(s) = arr.get_mut(from + i) {
            *s = (data >> (8 * i)) as u8 as i8;
        }
    }
}

/// Linux's warning of rtw89_phy_cfg_bb_rpl_ofst for a bandwidth it does not know
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnknownBw {
    pub addr: u32,
    pub bw: u32,
}

// Linux: rtw89_phy_cfg_bb_rpl_ofst (phy.c:1434)
fn cfg_rpl_ofst(g: &mut BbGain, addr: u32, data: u32) -> Option<UnknownBw> {
    let (rx, bw) = (addr & 0xf, (addr >> 4) & 0xf);
    let path = ((addr >> 8) & 0xff) as usize;
    let gband = ((addr >> 16) & 0xff) as usize;
    let s = data as u8 as i8;
    if bw == RTW89_CHANNEL_WIDTH_20 {
        g.rpl_ofst_20[gband][path] = s;
    } else if bw == RTW89_CHANNEL_WIDTH_40 {
        let t = &mut g.rpl_ofst_40[gband][path];
        match rx {
            RXSC_START_IDX_FULL => t[0] = s,
            RXSC_START_IDX_20 => fill(t, RXSC_START_IDX_20 as usize, 2, data),
            _ => {}
        }
    } else if bw == RTW89_CHANNEL_WIDTH_80 {
        let t = &mut g.rpl_ofst_80[gband][path];
        match rx {
            RXSC_START_IDX_FULL => t[0] = s,
            RXSC_START_IDX_20 => fill(t, RXSC_START_IDX_20 as usize, 4, data),
            RXSC_START_IDX_40 => fill(t, RXSC_START_IDX_40 as usize, 2, data),
            _ => {}
        }
    } else if bw == RTW89_CHANNEL_WIDTH_160 {
        let t = &mut g.rpl_ofst_160[gband][path];
        match rx {
            RXSC_START_IDX_FULL => t[0] = s,
            RXSC_START_IDX_20 => fill(t, RXSC_START_IDX_20 as usize, 4, data),
            RXSC_START_IDX_20_1 => fill(t, RXSC_START_IDX_20_1 as usize, 4, data),
            RXSC_START_IDX_40 => fill(t, RXSC_START_IDX_40 as usize, 4, data),
            RXSC_START_IDX_80 => fill(t, RXSC_START_IDX_80 as usize, 2, data),
            _ => {}
        }
    } else {
        return Some(UnknownBw { addr, bw });
    }
    None
}

// Linux: rtw89_phy_config_bb_gain_ax (phy.c:1577) with cfg_bb_gain_error, cfg_bb_rpl_ofst,
// cfg_bb_gain_bypass, cfg_bb_gain_op1db. union rtw89_phy_bb_gain_arg: byte 0 type (or
// rxsc_start:4, bw:4), path, gain_band, cfg_type. cfg_type 4 is for rfe_type >= 50 (eFEM)
// only; other unknown entries are Linux warnings, nothing is stored.
pub fn config_bb_gain_ax(g: &mut BbGain, addr: u32, data: u32) -> Option<UnknownBw> {
    let typ = addr & 0xff;
    let path = ((addr >> 8) & 0xff) as usize;
    let gband = ((addr >> 16) & 0xff) as usize;
    let cfg_type = addr >> 24;
    if gband >= GBANDS || path >= PATHS || (0xf9..=0xfe).contains(&addr) {
        return None;
    }
    match (cfg_type, typ) {
        (0, 0) => fill(&mut g.lna_gain[gband][path], 0, 4, data),
        (0, 1) => fill(&mut g.lna_gain[gband][path], 4, 3, data),
        (0, 2) => fill(&mut g.tia_gain[gband][path], 0, 2, data),
        (1, _) => return cfg_rpl_ofst(g, addr, data),
        (2, 0) => fill(&mut g.lna_gain_bypass[gband][path], 0, 4, data),
        (2, 1) => fill(&mut g.lna_gain_bypass[gband][path], 4, 3, data),
        (3, 0) => fill(&mut g.lna_op1db[gband][path], 0, 4, data),
        (3, 1) => fill(&mut g.lna_op1db[gband][path], 4, 3, data),
        (3, 2) => fill(&mut g.tia_lna_op1db[gband][path], 0, 4, data),
        (3, 3) => fill(&mut g.tia_lna_op1db[gband][path], 4, 4, data),
        _ => {}
    }
    None
}
