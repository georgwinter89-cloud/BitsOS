//! The TX power tracking deltas the TSSI thermal meter table needs (rtwdev->fw.elm_info.
//! txpwr_trk), from the firmware file's TXPWR_TRK element as Linux builds them at probe, else
//! Linux's own rtw89_8852b_trk_cfg (JS js/rfk2-trk.mjs, rfk2-trkdata.mjs).

use alloc::vec::Vec;

use crate::error::{Rtw89Error, Rtw89Result};
use crate::fw::Firmware;
use crate::tables::regs::*;
use crate::tables::trk::{DELTA_SWINGIDX_SIZE, TRK_CFG};

use super::{Rfk2Event, Rfk2State};

/// enum rtw89_fw_txpwr_trk_type's RTW89_FW_TXPWR_TRK_TYPE_NR
const TYPE_NR: usize = 16;

/// RTW89_DEFAULT_NEEDED_FW_TXPWR_TRK_5GHZ | _2GHZ (fw.h:4531): the 8852B has 2 and 5 GHz
pub const NEEDED: u32 = (1 << RTW89_FW_TXPWR_TRK_TYPE_5GB_N)
    | (1 << RTW89_FW_TXPWR_TRK_TYPE_5GB_P)
    | (1 << RTW89_FW_TXPWR_TRK_TYPE_5GA_N)
    | (1 << RTW89_FW_TXPWR_TRK_TYPE_5GA_P)
    | (1 << RTW89_FW_TXPWR_TRK_TYPE_2GB_N)
    | (1 << RTW89_FW_TXPWR_TRK_TYPE_2GB_P)
    | (1 << RTW89_FW_TXPWR_TRK_TYPE_2GA_N)
    | (1 << RTW89_FW_TXPWR_TRK_TYPE_2GA_P)
    | (1 << RTW89_FW_TXPWR_TRK_TYPE_2G_CCK_B_N)
    | (1 << RTW89_FW_TXPWR_TRK_TYPE_2G_CCK_B_P)
    | (1 << RTW89_FW_TXPWR_TRK_TYPE_2G_CCK_A_N)
    | (1 << RTW89_FW_TXPWR_TRK_TYPE_2G_CCK_A_P);

/// struct rtw89_fw_txpwr_track_cfg: per enum rtw89_fw_txpwr_trk_type the rows of s8 deltas
/// (one per subband: 4 for 6 GHz, 3 for 5 GHz, 1 for 2 GHz; empty when the element lacks the
/// type)
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TxpwrTrk {
    pub delta: [Vec<[i8; DELTA_SWINGIDX_SIZE]>; TYPE_NR],
}

// Linux: rtw89_build_txpwr_trk_tbl_from_elm (fw.c:1224). `payload` is the element's union u
// (le32 bitmap, le32 reserved, then rows of 30 s8), `size` the header's size, `aid` its aid.
// Ok(None): an element of another analog die (Linux's "ignore this element"). The errors are
// where Linux fails the firmware: a bitmap without the needed 2/5 GHz types (-ENOENT), rows past
// the element's size (-EFAULT); a payload shorter than its rows (the file ends) too.
pub fn build_from_elm(
    payload: &[u8],
    size: u32,
    aid: u16,
    hal_aid: u16,
) -> Rtw89Result<Option<TxpwrTrk>> {
    if aid != 0 && aid != hal_aid {
        return Ok(None);
    }
    let bitmap = payload
        .get(..4)
        .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .ok_or(Rtw89Error::FwFile {
            what: "txpwr trk element without its bitmap",
            value: payload.len() as u32,
        })?;
    if bitmap & NEEDED != NEEDED {
        return Err(Rtw89Error::FwFile {
            what: "needed txpwr trk bitmap missing",
            value: bitmap,
        });
    }
    let contents = payload.get(8..).unwrap_or(&[]);
    let mut t = TxpwrTrk::default();
    let mut offset: u32 = 0;
    let mut bits = bitmap;
    let mut typ: u32 = 0;
    while bits != 0 {
        if bits & 1 != 0 {
            let subband = if typ <= __RTW89_FW_TXPWR_TRK_TYPE_6GHZ_MAX {
                4
            } else if typ <= __RTW89_FW_TXPWR_TRK_TYPE_5GHZ_MAX {
                3
            } else if typ <= __RTW89_FW_TXPWR_TRK_TYPE_2GHZ_MAX {
                1
            } else {
                break;
            };
            let mut rows = Vec::new();
            for k in 0..subband {
                let at = (offset as usize + k) * DELTA_SWINGIDX_SIZE;
                let b = contents
                    .get(at..at + DELTA_SWINGIDX_SIZE)
                    .ok_or(Rtw89Error::FwFile {
                        what: "txpwr trk row past the file's end",
                        value: at as u32,
                    })?;
                let mut row = [0i8; DELTA_SWINGIDX_SIZE];
                for (r, &x) in row.iter_mut().zip(b) {
                    *r = x as i8;
                }
                rows.push(row);
            }
            t.delta[typ as usize] = rows;
            offset += subband as u32;
            if offset as usize * DELTA_SWINGIDX_SIZE > size as usize {
                return Err(Rtw89Error::FwFile {
                    what: "unexpected txpwr trk offset over size",
                    value: offset,
                });
            }
        }
        typ += 1;
        bits >>= 1;
    }
    Ok(Some(t))
}

impl Rfk2State {
    // Linux: rtw89_fw_recognize_elements' TXPWR_TRK handler (fw.c:1541) at probe: every
    // TXPWR_TRK element in the file's order, the last one built wins; `hal_aid` is hal.aid (0
    // on AX chips). An error leaves Linux's own table in place and is returned (Linux fails the
    // firmware there). Without an element Linux's table is used (event).
    pub fn load_txpwr_trk(&mut self, fw: &Firmware, hal_aid: u16) -> Rtw89Result<()> {
        self.load_txpwr_trk_from(
            fw.elements
                .iter()
                .filter(|e| e.id == RTW89_FW_ELEMENT_ID_TXPWR_TRK)
                .map(|e| (fw.payload(e), e.size, e.aid)),
            hal_aid,
        )
    }

    /// `load_txpwr_trk` on the TXPWR_TRK elements as (union u, size, aid)
    pub fn load_txpwr_trk_from<'a>(
        &mut self,
        elems: impl IntoIterator<Item = (&'a [u8], u32, u16)>,
        hal_aid: u16,
    ) -> Rtw89Result<()> {
        self.txpwr_trk = None;
        let mut found = None;
        for (payload, size, aid) in elems {
            if let Some(t) = build_from_elm(payload, size, aid, hal_aid)? {
                found = Some(t);
            }
        }
        if found.is_none() {
            self.push(Rfk2Event::TxpwrTrkDefault);
        }
        self.txpwr_trk = found;
        Ok(())
    }
}

// The `trk->delta[type][row]` of _tssi_set_tmeter_tbl (rtw8852b_rfk.c:2787): the element's, or
// rtw89_8852b_trk_cfg when there is none (`if (!trk) trk = &rtw89_8852b_trk_cfg`)
pub fn row(st: &Rfk2State, typ: u32, idx: usize) -> Rtw89Result<&[i8; DELTA_SWINGIDX_SIZE]> {
    let rows: Option<&[[i8; DELTA_SWINGIDX_SIZE]]> = match &st.txpwr_trk {
        Some(t) => t.delta.get(typ as usize).map(Vec::as_slice),
        None => TRK_CFG.get(typ as usize).copied().flatten(),
    };
    rows.and_then(|r| r.get(idx)).ok_or(Rtw89Error::Invalid {
        what: "rfk2: txpwr trk table without this type/row",
        value: typ,
    })
}
