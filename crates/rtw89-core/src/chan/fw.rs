//! The TX power and regulatory tables of the firmware file's elements, taken the way Linux
//! rtw89 takes them (JS js/chan-fw.mjs; Linux fw.c element handlers and
//! rtw89_load_rfe_data_from_fw). Pure parsing: no register access. The BB_GAIN element is phy's
//! (`Phy::bb_gain`, rtw89_phy_init_bb_reg), the TXPWR_TRK element rfk2's
//! (`Rfk2State::load_txpwr_trk`).

use alloc::vec;
use alloc::vec::Vec;

use crate::error::{Rtw89Error, Rtw89Result};
use crate::fw::Firmware;
use crate::tables::chan::TXPWR_CONF;
use crate::tables::regs::*;

/// An s8 array with the dimensions of the C array (`v[a][b][c]...`); an index outside them reads
/// `None` and is never written (the dimensions are Linux's enum sizes, R1)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct S8Table {
    dims: Vec<usize>,
    data: Vec<i8>,
}

impl S8Table {
    pub fn new(dims: &[usize]) -> S8Table {
        S8Table {
            dims: dims.to_vec(),
            data: vec![0; dims.iter().product()],
        }
    }
    pub fn dims(&self) -> &[usize] {
        &self.dims
    }
    /// The values in C order (the last index runs fastest)
    pub fn data(&self) -> &[i8] {
        &self.data
    }
    fn at(&self, idx: &[usize]) -> Option<usize> {
        if idx.len() != self.dims.len() {
            return None;
        }
        let mut o = 0usize;
        for (&i, &d) in idx.iter().zip(&self.dims) {
            if i >= d {
                return None;
            }
            o = o * d + i;
        }
        Some(o)
    }
    pub fn get(&self, idx: &[usize]) -> Option<i8> {
        self.at(idx).and_then(|o| self.data.get(o).copied())
    }
    pub fn set(&mut self, idx: &[usize], v: i8) {
        if let Some(s) = self.at(idx).and_then(|o| self.data.get_mut(o)) {
            *s = v;
        }
    }
}

const CCK: usize = RTW89_RATE_CCK_NUM as usize;
const OFDM: usize = RTW89_RATE_OFDM_NUM as usize;
const OFDMA: usize = RTW89_OFDMA_NUM as usize;
const NSS: usize = RTW89_NSS_NUM as usize;
const MCS: usize = __RTW89_RATE_MCS_NUM as usize;
const NSS_HEDCM: usize = RTW89_NSS_HEDCM_NUM as usize;
const HEDCM: usize = RTW89_RATE_HEDCM_NUM as usize;
const OFFSET: usize = __RTW89_RATE_OFFSET_NUM as usize;
const BANDS: usize = RTW89_BAND_NUM as usize;
const BYR_BWS: usize = RTW89_BYR_BW_NUM as usize;

/// struct rtw89_txpwr_byrate's head of one band and bandwidth (core.h), RF units (1/4 dB)
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ByrHead {
    pub cck: [i8; CCK],
    pub ofdm: [i8; OFDM],
    pub mcs: [[[i8; MCS]; NSS]; OFDMA],
    pub hedcm: [[[i8; HEDCM]; NSS_HEDCM]; OFDMA],
    pub offset: [i8; OFFSET],
}

/// struct rtw89_rate_desc as rtw89_phy_raw_byr_seek reads it (enum rtw89_rate_section `rs`)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RateDesc {
    pub rs: u32,
    pub nss: u32,
    pub ofdma: u32,
    pub idx: u32,
}

impl ByrHead {
    // Linux: rtw89_phy_raw_byr_seek (phy.c:2425): the value of one rate (`None`: no such rate)
    pub fn seek(&self, d: RateDesc) -> Option<i8> {
        let (o, n, i) = (d.ofdma as usize, d.nss as usize, d.idx as usize);
        match d.rs {
            RTW89_RS_CCK => self.cck.get(i).copied(),
            RTW89_RS_OFDM => self.ofdm.get(i).copied(),
            RTW89_RS_MCS => self.mcs.get(o)?.get(n)?.get(i).copied(),
            RTW89_RS_HEDCM => self.hedcm.get(o)?.get(n)?.get(i).copied(),
            RTW89_RS_OFFSET => self.offset.get(i).copied(),
            _ => None,
        }
    }
    fn seek_mut(&mut self, d: RateDesc) -> Option<&mut i8> {
        let (o, n, i) = (d.ofdma as usize, d.nss as usize, d.idx as usize);
        match d.rs {
            RTW89_RS_CCK => self.cck.get_mut(i),
            RTW89_RS_OFDM => self.ofdm.get_mut(i),
            RTW89_RS_MCS => self.mcs.get_mut(o)?.get_mut(n)?.get_mut(i),
            RTW89_RS_HEDCM => self.hedcm.get_mut(o)?.get_mut(n)?.get_mut(i),
            RTW89_RS_OFFSET => self.offset.get_mut(i),
            _ => None,
        }
    }
}

/// struct rtw89_txpwr_conf of one element as rtw89_fw_recognize_txpwr_from_elm took it; `data`
/// is the element's content (union u from byte 8)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TxpwrConf {
    pub key: &'static str,
    pub id: u32,
    pub rfe: u8,
    pub ent_sz: u8,
    pub num_ents: u32,
    pub data: Vec<u8>,
}

/// Which element `key` (TXPWR_CONF's names) has id `id`
fn key_of(id: u32) -> Option<&'static str> {
    TXPWR_CONF
        .iter()
        .find(|c| u32::from(c.id) == id)
        .map(|c| c.key)
}

// Linux: rtw89_fw_recognize_txpwr_from_elm (fw.c:1176) for all TXPWR elements in file order: an
// element of another analog die is skipped; the efuse's RFE type wins (the last one if several);
// RFE 0 is the default as long as nothing else was taken. Returns the taken configurations in
// TXPWR_CONF's order.
pub fn recognize_txpwr(fw: &Firmware, rfe_type: u8, hal_aid: u16) -> Vec<TxpwrConf> {
    let mut confs: Vec<TxpwrConf> = Vec::new();
    for e in &fw.elements {
        let Some(key) = key_of(e.id) else { continue };
        if e.aid != 0 && e.aid != hal_aid {
            continue;
        }
        let p = fw.payload(e);
        let rfe = p.get(2).copied().unwrap_or(0);
        let have = confs.iter().position(|c| c.key == key);
        let dflt = u32::from(rfe) == RTW89_TXPWR_CONF_DFLT_RFE_TYPE;
        let take = rfe == rfe_type
            || (dflt
                && have.is_none_or(|i| u32::from(confs[i].rfe) == RTW89_TXPWR_CONF_DFLT_RFE_TYPE));
        if !take {
            continue;
        }
        let conf = TxpwrConf {
            key,
            id: e.id,
            rfe,
            ent_sz: p.get(3).copied().unwrap_or(0),
            num_ents: le32(p, 4),
            data: p.get(8..).unwrap_or(&[]).to_vec(),
        };
        match have {
            Some(i) => confs[i] = conf,
            None => confs.push(conf),
        }
    }
    let order = |k: &str| TXPWR_CONF.iter().position(|c| c.key == k).unwrap_or(0);
    confs.sort_by_key(|c| order(c.key));
    confs
}

fn le32(b: &[u8], o: usize) -> u32 {
    match b.get(o..o + 4) {
        Some(x) => u32::from_le_bytes([x[0], x[1], x[2], x[3]]),
        None => 0,
    }
}

// rtw89_for_each_in_txpwr_conf + rtw89_txpwr_entcpy + __fw_txpwr_entry_acceptable (core.h:5375,
// fw.c:12057): each entry copied into a zeroed struct of N bytes; an entry longer than the struct
// is taken only when its extension is zero. Not Linux: entries past the element's content are an
// error (Linux would read past the element).
fn entries<const N: usize>(c: &TxpwrConf) -> Rtw89Result<Vec<[u8; N]>> {
    let sz = usize::from(c.ent_sz);
    let total = u64::from(c.num_ents) * sz as u64;
    if total > c.data.len() as u64 {
        return Err(Rtw89Error::FwFile {
            what: "txpwr element entries past its content",
            value: c.num_ents,
        });
    }
    let mut out = Vec::new();
    if sz == 0 {
        return Ok(out);
    }
    for ent in c.data[..total as usize].chunks_exact(sz) {
        let mut e = [0u8; N];
        let n = N.min(sz);
        e[..n].copy_from_slice(&ent[..n]);
        if sz > N && ent[N..].iter().any(|&b| b != 0) {
            continue;
        }
        out.push(e);
    }
    Ok(out)
}

// Linux: fw_txpwr_byrate_entry_valid (fw.c:12069)
fn byrate_valid(e: &[u8; 11]) -> bool {
    let (band, nss, rs, shf, len, bw, ofdma) = (
        u32::from(e[0]),
        u32::from(e[1]),
        u32::from(e[2]),
        u32::from(e[3]),
        u32::from(e[4]),
        u32::from(e[9]),
        u32::from(e[10]),
    );
    if band >= RTW89_BAND_NUM || bw >= RTW89_BYR_BW_NUM {
        return false;
    }
    let end = shf + len;
    match rs {
        RTW89_RS_CCK => end <= RTW89_RATE_CCK_NUM,
        RTW89_RS_OFDM => end <= RTW89_RATE_OFDM_NUM,
        RTW89_RS_MCS => {
            end <= __RTW89_RATE_MCS_NUM && nss < RTW89_NSS_NUM && ofdma < RTW89_OFDMA_NUM
        }
        RTW89_RS_HEDCM => {
            end <= RTW89_RATE_HEDCM_NUM && nss < RTW89_NSS_HEDCM_NUM && ofdma < RTW89_OFDMA_NUM
        }
        RTW89_RS_OFFSET => end <= __RTW89_RATE_OFFSET_NUM,
        _ => false,
    }
}

// Linux: rtw89_fw_load_txpwr_byrate (fw.c:12113): byr[band][bw] from struct
// rtw89_fw_txpwr_byrate_entry (11 bytes: band, nss, rs, shf, len, le32 data, bw, ofdma)
fn load_byrate(c: &TxpwrConf) -> Rtw89Result<Vec<ByrHead>> {
    let mut byr = vec![ByrHead::default(); BANDS * BYR_BWS];
    for e in entries::<11>(c)? {
        if !byrate_valid(&e) {
            continue;
        }
        let head = &mut byr[usize::from(e[0]) * BYR_BWS + usize::from(e[9])];
        let mut data = u32::from_le_bytes([e[5], e[6], e[7], e[8]]);
        for i in 0..u32::from(e[4]) {
            let d = RateDesc {
                rs: e[2].into(),
                nss: e[1].into(),
                ofdma: e[10].into(),
                idx: u32::from(e[3]) + i,
            };
            if let Some(s) = head.seek_mut(d) {
                *s = data as u8 as i8;
            }
            data >>= 8;
        }
    }
    Ok(byr)
}

// Linux: rtw89_fw_load_txpwr_lmt_2ghz / _5ghz (fw.c:12168, 12208) with their entry checks:
// v[bw][ntx][rs][bf][regd][ch_idx] from (bw, nt, rs, bf, regd, ch_idx, v)
fn load_lmt(c: &TxpwrConf, bws: u32, chs: u32) -> Rtw89Result<S8Table> {
    let dims = [
        bws as usize,
        RTW89_NTX_NUM as usize,
        RTW89_RS_LMT_NUM as usize,
        RTW89_BF_NUM as usize,
        RTW89_REGD_NUM as usize,
        chs as usize,
    ];
    let mut t = S8Table::new(&dims);
    for e in entries::<7>(c)? {
        let idx = [e[0], e[1], e[2], e[3], e[4], e[5]].map(usize::from);
        if idx.iter().zip(&dims).all(|(i, d)| i < d) {
            t.set(&idx, e[6] as i8);
        }
    }
    Ok(t)
}

// Linux: rtw89_fw_load_txpwr_lmt_ru_2ghz / _5ghz (fw.c:12286, 12321): v[ru][ntx][regd][ch_idx]
fn load_lmt_ru(c: &TxpwrConf, chs: u32) -> Rtw89Result<S8Table> {
    let dims = [
        RTW89_RU_NUM as usize,
        RTW89_NTX_NUM as usize,
        RTW89_REGD_NUM as usize,
        chs as usize,
    ];
    let mut t = S8Table::new(&dims);
    for e in entries::<5>(c)? {
        let idx = [e[0], e[1], e[2], e[3]].map(usize::from);
        if idx.iter().zip(&dims).all(|(i, d)| i < d) {
            t.set(&idx, e[4] as i8);
        }
    }
    Ok(t)
}

// Linux: rtw89_fw_load_tx_shape_lmt (fw.c:12398): v[band][tx_shape_rs][regd][reg6_pwr] from
// (band, tx_shape_rs, regd, v, reg6_pwr)
fn load_tx_shape(c: &TxpwrConf) -> Rtw89Result<S8Table> {
    let dims = [
        BANDS,
        RTW89_RS_TX_SHAPE_NUM as usize,
        RTW89_REGD_NUM as usize,
        NUM_OF_RTW89_REG_6GHZ_POWER as usize,
    ];
    let mut t = S8Table::new(&dims);
    for e in entries::<5>(c)? {
        let idx = [e[0], e[1], e[2], e[4]].map(usize::from);
        if idx.iter().zip(&dims).all(|(i, d)| i < d) {
            t.set(&idx, e[3] as i8);
        }
    }
    Ok(t)
}

// Linux: rtw89_fw_load_tx_shape_lmt_ru (fw.c:12435): v[band][regd][reg6_pwr] from (band, regd, v,
// reg6_pwr)
fn load_tx_shape_ru(c: &TxpwrConf) -> Rtw89Result<S8Table> {
    let dims = [
        BANDS,
        RTW89_REGD_NUM as usize,
        NUM_OF_RTW89_REG_6GHZ_POWER as usize,
    ];
    let mut t = S8Table::new(&dims);
    for e in entries::<4>(c)? {
        let idx = [e[0], e[1], e[3]].map(usize::from);
        if idx.iter().zip(&dims).all(|(i, d)| i < d) {
            t.set(&idx, e[2] as i8);
        }
    }
    Ok(t)
}

/// struct rtw89_rfe_parms as rtw89_load_rfe_data_from_fw leaves it for the 8852B with this
/// firmware: every table from the file
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TxpwrTables {
    /// the RFE type of each configuration taken (key, rfe), TXPWR_CONF's order
    pub rfe: Vec<(&'static str, u8)>,
    /// byr[band * RTW89_BYR_BW_NUM + bw]
    pub byr: Vec<ByrHead>,
    pub lmt_2g: S8Table,
    pub lmt_5g: S8Table,
    pub lmt_ru_2g: S8Table,
    pub lmt_ru_5g: S8Table,
    pub tx_shape_lmt: S8Table,
    pub tx_shape_lmt_ru: S8Table,
    /// rtw89_fw_has_da_txpwr_table (fw.c:12449): antenna gain tables; none in this firmware
    pub has_da: bool,
}

impl TxpwrTables {
    /// The by-rate head of band `band` (enum rtw89_band) and by-rate bandwidth `bw`
    pub fn byr_head(&self, band: u32, bw: u32) -> Option<&ByrHead> {
        if band >= RTW89_BAND_NUM || bw >= RTW89_BYR_BW_NUM {
            return None;
        }
        self.byr.get(band as usize * BYR_BWS + bw as usize)
    }
}

/// The configurations the 8852B needs from the file (rtw8852b_table.c's built-in tables are not
/// ported: this firmware carries all of them)
const NEEDED: [&str; 7] = [
    "byrate",
    "lmt2g",
    "lmt5g",
    "lmtRu2g",
    "lmtRu5g",
    "txShapeLmt",
    "txShapeLmtRu",
];

// Linux: rtw89_load_rfe_data_from_fw (fw.c:12469) and the byrate load of
// rtw89_core_setup_rfe_parms (core.c:7208-7209). 8852B: no rfe_parms_conf, the default parms of
// rtw8852b_table.c:22914 start the copy; every table below comes from the firmware, so none of
// rtw8852b_table.c stays in use. A missing element is an error (JS: "firmware lacks TX power
// elements"), with the element id of the first one missing.
pub fn load_rfe_data(fw: &Firmware, rfe_type: u8, hal_aid: u16) -> Rtw89Result<TxpwrTables> {
    load_rfe_data_from(&recognize_txpwr(fw, rfe_type, hal_aid))
}

/// `load_rfe_data` on configurations already taken (`recognize_txpwr`'s)
pub fn load_rfe_data_from(confs: &[TxpwrConf]) -> Rtw89Result<TxpwrTables> {
    let get = |k: &str| confs.iter().find(|c| c.key == k);
    if let Some(&k) = NEEDED.iter().find(|&&k| get(k).is_none()) {
        let id = TXPWR_CONF
            .iter()
            .find(|c| c.key == k)
            .map_or(0, |c| u32::from(c.id));
        return Err(Rtw89Error::FwFile {
            what: "firmware lacks a TX power element",
            value: id,
        });
    }
    let need = |k: &str| {
        get(k).ok_or(Rtw89Error::FwFile {
            what: "firmware lacks a TX power element",
            value: 0,
        })
    };
    Ok(TxpwrTables {
        rfe: confs.iter().map(|c| (c.key, c.rfe)).collect(),
        byr: load_byrate(need("byrate")?)?,
        lmt_2g: load_lmt(need("lmt2g")?, RTW89_2G_BW_NUM, RTW89_2G_CH_NUM)?,
        lmt_5g: load_lmt(need("lmt5g")?, RTW89_5G_BW_NUM, RTW89_5G_CH_NUM)?,
        lmt_ru_2g: load_lmt_ru(need("lmtRu2g")?, RTW89_2G_CH_NUM)?,
        lmt_ru_5g: load_lmt_ru(need("lmtRu5g")?, RTW89_5G_CH_NUM)?,
        tx_shape_lmt: load_tx_shape(need("txShapeLmt")?)?,
        tx_shape_lmt_ru: load_tx_shape_ru(need("txShapeLmtRu")?)?,
        has_da: ["daLmt2g", "daLmtRu2g", "daLmt5g", "daLmtRu5g"]
            .iter()
            .all(|k| get(k).is_some()),
    })
}

/// struct rtw89_regd: a country's TX power regulation per band (enum rtw89_regulation_type,
/// RTW89_NA where the country has none) and its function bitmap
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Regd {
    pub alpha2: [u8; 2],
    /// [2 GHz, 5 GHz, 6 GHz]
    pub txpwr_regd: [u32; 3],
    pub fmap: u32,
}

impl Regd {
    /// Worldwide (rtw89_regd_find_reg_by_name's fallback, "00")
    pub const WW: Regd = Regd {
        alpha2: *b"00",
        txpwr_regd: [RTW89_WW; 3],
        fmap: 0,
    };
}

/// What the REGD elements of a file gave: the map of the last one taken, and the country counts
/// of the ones ignored (more than RTW89_REGD_MAX_COUNTRY_NUM)
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RegdMap {
    pub map: Option<Vec<Regd>>,
    pub ignored: Vec<u32>,
}

/// RTW89_REGD_MAX_COUNTRY_NUM (core.h:6990): U8_MAX
const REGD_MAX_COUNTRY_NUM: u32 = 255;

// Linux: rtw89_recognize_regd_from_elm (fw.c:1368) with rtw89_regd_entcpy (fw.c:1319): the
// country map (alpha2 -> TX power regulation per band); the last element wins, one with more
// than 255 countries is ignored (Linux warns). Not Linux: entries past the element's content
// are an error (Linux would read past the element).
pub fn load_regd_map(fw: &Firmware) -> Rtw89Result<RegdMap> {
    load_regd_map_from(
        fw.elements
            .iter()
            .filter(|e| e.id == RTW89_FW_ELEMENT_ID_REGD)
            .map(|e| fw.payload(e)),
    )
}

/// `load_regd_map` on the REGD elements' union u, in file order
pub fn load_regd_map_from<'a>(elems: impl IntoIterator<Item = &'a [u8]>) -> Rtw89Result<RegdMap> {
    let mut out = RegdMap::default();
    for p in elems {
        let ent_sz = usize::from(p.get(3).copied().unwrap_or(0));
        let n = le32(p, 4);
        if n > REGD_MAX_COUNTRY_NUM {
            out.ignored.push(n);
            continue;
        }
        let content = p.get(8..).unwrap_or(&[]);
        if n as usize * ent_sz > content.len() {
            return Err(Rtw89Error::FwFile {
                what: "regd element entries past its content",
                value: n,
            });
        }
        let rule = |r: u8| {
            if u32::from(r) < RTW89_REGD_NUM {
                u32::from(r)
            } else {
                RTW89_NA
            }
        };
        let mut map = Vec::new();
        for i in 0..n as usize {
            // struct rtw89_fw_regd_entry defaults for a short entry
            let mut ent = [
                0,
                0,
                RTW89_NA as u8,
                RTW89_NA as u8,
                RTW89_NA as u8,
                0,
                0,
                0,
                0,
            ];
            let k = ent_sz.min(ent.len());
            ent[..k].copy_from_slice(&content[i * ent_sz..i * ent_sz + k]);
            map.push(Regd {
                alpha2: [ent[0], ent[1]],
                txpwr_regd: [rule(ent[2]), rule(ent[3]), rule(ent[4])],
                fmap: u32::from_le_bytes([ent[5], ent[6], ent[7], ent[8]]),
            });
        }
        out.map = Some(map);
    }
    Ok(out)
}
