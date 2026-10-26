//! The efuse maps of the RTL8852B parsed as Linux does (JS js/phy-efuse.mjs and
//! bringup-steps.mjs `freshEfuse`; Linux efuse.c's logical map, DAV dump and phycap dump,
//! rtw8852b_common.c `__rtw8852bx_read_efuse` / `__rtw8852bx_read_phycap`).

use alloc::vec;
use alloc::vec::Vec;

use crate::bringup::efuse::dump_physical_tries;
use crate::bringup::power::{xtal_read, xtal_write};
use crate::bus::Bus;
use crate::error::{Rtw89Error, Rtw89Result};
use crate::tables::phy::*;
use crate::tables::regs::*;

// chip constants of rtw8852b_chip_info (the chip's efuse layout, not counts of things)
pub const PHYS_SIZE: u32 = 1216;
pub const LOG_SIZE: usize = 2048;
pub const SEC_CTRL: u32 = 4;
pub const DAV_PHYS_SIZE: u32 = 96;
pub const DAV_LOG_SIZE: usize = 16;
pub const PHYCAP_ADDR: u32 = 0x580;
pub const PHYCAP_SIZE: u32 = 128;
/// rf_path_num of the 8852B
pub const RF_PATHS: usize = 2;

const TSSI_CCK: usize = TSSI_CCK_CH_GROUP_NUM as usize;
const TSSI_2G: usize = TSSI_MCS_2G_CH_GROUP_NUM as usize;
const TSSI_5G: usize = TSSI_MCS_5G_CH_GROUP_NUM as usize;
const TSSI_TRIM: usize = TSSI_TRIM_CH_GROUP_NUM as usize;
const GAIN_NR: usize = RTW89_GAIN_OFFSET_NR as usize;
const SUBBAND_NR: usize = RTW89_SUBBAND_NR as usize;

/// What the efuse dumps of the probe left (JS `d.st.efuseCtrl`, `efuseLog`, `efuseDav`,
/// `efusePhycap`): a dump that is `None` is read from the chip when it is needed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EfuseMaps {
    /// R_AX_SYS_WL_EFUSE_CTRL as the dump read it (the autoload bit)
    pub ctrl: Option<u16>,
    /// the logical map (2048 bytes; 2064 with the DAV part is taken too)
    pub log: Vec<u8>,
    /// the DAV physical map (96 bytes)
    pub dav: Option<Vec<u8>>,
    /// physical 0x580..0x5FF
    pub phycap: Option<Vec<u8>>,
}

/// Linux: struct rtw89_tssi_info (core.h), the efuse part, paths A and B
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TssiInfo {
    pub thermal: [u8; RF_PATHS],
    pub tssi_cck: [[i8; TSSI_CCK]; RF_PATHS],
    /// bw40_tssi (2 GHz groups), then bw40_1s_tssi_5g
    pub tssi_mcs: [[i8; TSSI_2G + TSSI_5G]; RF_PATHS],
    pub tssi_trim: [[i8; TSSI_TRIM]; RF_PATHS],
}

/// Linux: struct rtw89_phy_efuse_gain, paths A and B
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EfuseGain {
    pub offset_valid: bool,
    pub comp_valid: bool,
    pub offset: [[i8; GAIN_NR]; RF_PATHS],
    pub offset2: [[i8; GAIN_NR]; RF_PATHS],
    pub offset_base: [i8; RF_PATHS],
    pub rssi_base: [i8; RF_PATHS],
    pub comp: [[i8; SUBBAND_NR]; RF_PATHS],
}

/// Linux: struct rtw89_power_trim_info
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PwrTrim {
    pub pg_thermal_trim: bool,
    pub thermal_trim: [u8; RF_PATHS],
    pub pg_pa_bias_trim: bool,
    pub pa_bias_trim: [u8; RF_PATHS],
}

/// Linux: struct rtw89_efuse (core.h:4651) with ->tssi, ->efuse_gain and ->pwr_trim in one
/// place (JS `d.st.efuse`)
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Efuse {
    /// the autoload bit was set
    pub valid: bool,
    pub power_k_valid: bool,
    pub addr: [u8; 6],
    pub rfe_type: u8,
    pub xtal_cap: u8,
    pub country_code: [u8; 2],
    pub tssi: TssiInfo,
    pub gain: EfuseGain,
    pub pwr_trim: PwrTrim,
    /// the DAV logical map (16 bytes)
    pub dav_log: Vec<u8>,
}

impl Efuse {
    /// The MAC address as text (`02:b1:75:00:00:01`)
    pub fn mac_text(&self) -> alloc::string::String {
        let a = self.addr;
        alloc::format!(
            "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
            a[0],
            a[1],
            a[2],
            a[3],
            a[4],
            a[5]
        )
    }
}

// Linux: rtw89_dump_logical_efuse_map (efuse.c:238) into `log`. The chip's sizes are used
// for the DAV map too, as in Linux; bytes past the physical buffer read as 0xff, and bytes
// past `log` are not stored (the JS driver's rules). An entry past the map is Linux's -EINVAL.
pub fn logical_efuse_map(phy: &[u8], log: &mut [u8]) -> Rtw89Result<()> {
    let at = |i: u32| phy.get(i as usize).copied().unwrap_or(0xff);
    let mut p = SEC_CTRL;
    while p < PHYS_SIZE - SEC_CTRL {
        let (h1, h2) = (at(p), at(p + 1));
        if h1 == 0xff || h2 == 0xff {
            break;
        }
        let blk = (u32::from(h2 & 0xf0) >> 4) | (u32::from(h1 & 0x0f) << 4);
        let word_en = h2 & 0xf;
        p += 2;
        for i in 0..4u32 {
            if word_en & (1 << i) != 0 {
                continue;
            }
            let l = (blk << 3) + (i << 1);
            if p + 1 > PHYS_SIZE - SEC_CTRL - 1 || l as usize + 1 > LOG_SIZE {
                return Err(Rtw89Error::Efuse {
                    what: "efuse logical map out of range",
                    addr: p,
                    value: l,
                });
            }
            if (l as usize + 1) < log.len() {
                log[l as usize] = at(p);
                log[l as usize + 1] = at(p + 1);
            }
            p += 2;
        }
    }
    Ok(())
}

// Linux: rtw89_parse_efuse_map_ax (efuse.c:281), the map part: full_log_size = logical +
// DAV logical; a log that already carries the DAV part gets it rebuilt from the DAV dump
pub fn logical_maps(maps: &EfuseMaps) -> Rtw89Result<Vec<u8>> {
    let mut log = vec![0xffu8; LOG_SIZE + DAV_LOG_SIZE];
    let n = maps.log.len().min(LOG_SIZE);
    log[..n].copy_from_slice(&maps.log[..n]);
    if let Some(dav) = &maps.dav {
        logical_efuse_map(dav, &mut log[LOG_SIZE..])?;
    }
    Ok(log)
}

// Linux: rtw89_dump_physical_efuse_map_dav (efuse.c:145) as the JS driver runs it
// (kit/rtwpwr.mjs davDump): per byte control and address through XTAL SI, 20 us, then the
// ready bit and the value; the ready bits are checked after all bytes (one round trip on
// the MCP kit). Linux polls the ready bit per byte; a byte without it is an error here too.
pub fn dump_dav<B: Bus + ?Sized>(bus: &mut B, addr: u32, size: u32) -> Rtw89Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut not_ready = 0u32;
    let mut first_bad = None;
    for a in addr..addr.saturating_add(size) {
        xtal_write(bus, XTAL_SI_CTRL, 0x40, FULL_BIT_MASK)?;
        xtal_write(bus, XTAL_SI_LOW_ADDR, a & 0xff, XTAL_SI_LOW_ADDR_MASK)?;
        xtal_write(bus, XTAL_SI_CTRL, a >> 8, XTAL_SI_HIGH_ADDR_MASK)?;
        xtal_write(bus, XTAL_SI_CTRL, 0, XTAL_SI_MODE_SEL_MASK)?;
        bus.delay_us(20)?;
        let ctl = xtal_read(bus, XTAL_SI_CTRL)?;
        let val = xtal_read(bus, XTAL_SI_READ_VAL)?;
        if ctl & XTAL_SI_RDY == 0 {
            not_ready += 1;
            first_bad.get_or_insert((a, ctl));
        }
        out.push(val as u8);
    }
    if let Some((a, ctl)) = first_bad {
        return Err(Rtw89Error::Efuse {
            what: "DAV efuse byte without RDY",
            addr: a,
            value: ctl | (not_ready << 8),
        });
    }
    Ok(out)
}

// Linux: rtw89_parse_efuse_map_ax (efuse.c:281), the dumps (JS bringup-steps.mjs
// `freshEfuse`): the autoload register, the physical map (five tries), the DAV map, the
// logical map. `on_retry` hears of each failed physical try (Linux's warning).
pub fn dump_maps<B: Bus + ?Sized>(
    bus: &mut B,
    cv: u32,
    on_retry: &mut dyn FnMut(u32, &Rtw89Error),
) -> Rtw89Result<EfuseMaps> {
    let ctrl = bus.r16(R_AX_SYS_WL_EFUSE_CTRL)?;
    let phy = dump_physical_tries(bus, 0, PHYS_SIZE, cv, on_retry)?;
    let dav = dump_dav(bus, 0, DAV_PHYS_SIZE)?;
    let mut log = vec![0xffu8; LOG_SIZE];
    logical_efuse_map(&phy, &mut log)?;
    Ok(EfuseMaps {
        ctrl: Some(ctrl),
        log,
        dav: Some(dav),
        phycap: None,
    })
}

// Linux: rtw89_parse_phycap_map_ax (efuse.c:350), the dump part: physical 0x580..0x5FF
pub fn phycap_read<B: Bus + ?Sized>(
    bus: &mut B,
    cv: u32,
    on_retry: &mut dyn FnMut(u32, &Rtw89Error),
) -> Rtw89Result<Vec<u8>> {
    dump_physical_tries(bus, PHYCAP_ADDR, PHYCAP_SIZE, cv, on_retry)
}

fn sign4(v: u8) -> i8 {
    let n = (v & 0xf) as i8;
    if v & 0x8 != 0 { n - 16 } else { n }
}

// _decode_efuse_gain (rtw8852b_common.c:206): high nibble, low nibble, valid
fn decode_gain(data: u8) -> (i8, i8, bool) {
    (sign4(data >> 4), sign4(data), data != 0xff)
}

// Linux: __rtw8852bx_read_efuse (rtw8852b_common.c:241), PCIe, with
// rtw8852bx_efuse_parsing_tssi and _parsing_gain_offset. Bytes past `map` read as 0xff.
pub fn read_efuse_8852bx(map: &[u8], ef: &mut Efuse) {
    let at = |o: usize| map.get(o).copied().unwrap_or(0xff);
    ef.country_code = [at(EF_COUNTRY_CODE), at(EF_COUNTRY_CODE + 1)];
    ef.tssi.thermal = [at(EF_PATH_A_THERM), at(EF_PATH_B_THERM)];
    for (i, o) in [EF_PATH_A_TSSI, EF_PATH_B_TSSI].into_iter().enumerate() {
        for (j, v) in ef.tssi.tssi_cck[i].iter_mut().enumerate() {
            *v = at(o + j) as i8;
        }
        // struct rtw8852bx_tssi_offset: cck_tssi[6], bw40_tssi[5], rsvd[7], bw40_1s_tssi_5g[14]
        for (j, v) in ef.tssi.tssi_mcs[i].iter_mut().enumerate() {
            let skip = if j < TSSI_2G { 0 } else { 7 };
            *v = at(o + TSSI_CCK + j + skip) as i8;
        }
    }
    let mut valid = false;
    for (o, idx) in [
        (EF_RX_GAIN2G_CCK, RTW89_GAIN_OFFSET_2G_CCK),
        (EF_RX_GAIN2G_OFDM, RTW89_GAIN_OFFSET_2G_OFDM),
        (EF_RX_GAIN5G_LOW, RTW89_GAIN_OFFSET_5G_LOW),
        (EF_RX_GAIN5G_MID, RTW89_GAIN_OFFSET_5G_MID),
        (EF_RX_GAIN5G_HIGH, RTW89_GAIN_OFFSET_5G_HIGH),
    ] {
        let (high, low, v) = decode_gain(at(o));
        ef.gain.offset[0][idx as usize] = high;
        ef.gain.offset[1][idx as usize] = low;
        valid |= v;
    }
    ef.gain.offset_valid = valid;
    for (i, b) in ef.addr.iter_mut().enumerate() {
        *b = at(EF_MAC_ADDR_E + i);
    }
    ef.rfe_type = at(EF_RFE_TYPE);
    ef.xtal_cap = at(EF_XTAL_K);
}

// Linux: __rtw8852bx_read_phycap (rtw8852b_common.c:434): power_cal, tssi, thermal_trim,
// pa_bias_trim, gain_comp on the map of physical 0x580..0x5FF (bytes past it read as 0xff)
pub fn read_phycap_8852bx(pc: &[u8], ef: &mut Efuse) {
    let at = |a: u16| {
        pc.get(usize::from(a).wrapping_sub(PHYCAP_ADDR as usize))
            .copied()
            .unwrap_or(0xff)
    };
    if at(0x5e9) == 0xaa {
        ef.power_k_valid = true; // PWR_K_CHK_OFFSET, PWR_K_CHK_VALUE
    }
    let mut pg = false;
    for (i, &base) in TRIM_ADDR.iter().enumerate().take(RF_PATHS) {
        for (j, t) in ef.tssi.tssi_trim[i].iter_mut().enumerate() {
            // the addresses are in decreasing order
            let v = at(base.wrapping_sub(j as u16));
            *t = v as i8;
            if v != 0xff {
                pg = true;
            }
        }
    }
    if !pg {
        ef.tssi.tssi_trim = [[0; TSSI_TRIM]; RF_PATHS];
    }
    let p = &mut ef.pwr_trim;
    for (i, a) in [0x5df, 0x5dc].into_iter().enumerate() {
        p.thermal_trim[i] = at(a);
        if p.thermal_trim[i] != 0xff {
            p.pg_thermal_trim = true;
        }
    }
    for (i, a) in [0x5de, 0x5db].into_iter().enumerate() {
        p.pa_bias_trim[i] = at(a);
        if p.pa_bias_trim[i] != 0xff {
            p.pg_pa_bias_trim = true;
        }
    }
    let mut valid = false;
    for (path, addrs) in COMP_ADDRS.iter().enumerate().take(RF_PATHS) {
        for (i, &a) in addrs
            .iter()
            .enumerate()
            .take(RTW89_SUBBAND_2GHZ_5GHZ_NR as usize)
        {
            if a == 0 {
                continue;
            }
            let (_, low, v) = decode_gain(at(a));
            ef.gain.comp[path][i] = low;
            valid |= v;
        }
    }
    ef.gain.comp_valid = valid;
}
