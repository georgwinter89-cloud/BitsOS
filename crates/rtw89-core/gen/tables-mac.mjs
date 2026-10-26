// tables-mac.mjs: src/gen/{dmac,fw_feat,txrx,btc}.rs, the data of
// werkzeuge/plattform/wlan/gen/{dmac,fw-feat,txrx,btc}.mjs as typed Rust.
import { jsText, wzImport, constOf, header, hex, intSlice, lit } from './lib.mjs';

// gen/dmac.mjs: the module constants of js/dmac.mjs, evaluated from its text
export function dmacRs() {
  const NAMES = ['WDE_PG_64', 'PLE_PG_128', 'SIZE', 'WDE_QT', 'PLE_QT', 'DLE_MEM', 'HFC_CH', 'HFC_PUB', 'HFC_PREC',
    'FIFO', 'SCC_RSVD', 'DMA_CH_MASK', 'DMA_H2C'];
  const text = jsText('dmac.mjs');
  const decls = [...text.matchAll(/^const ([\s\S]*?);\n/gm)].map((m) => m[1])
    .filter((d) => NAMES.some((n) => new RegExp(`^${n}\\b|,\\s*${n}\\s*=`).test(d)));
  const t = new Function(`const ${decls.join(';\nconst ')};\nreturn { ${NAMES.join(', ')} };`)();
  for (const n of NAMES) if (t[n] === undefined) throw new Error(`dmac.mjs: ${n} not found`);
  if (Object.keys(t.HFC_PUB).join() !== 'grp0,grp1,pubMax,wpThrd') throw new Error('HFC_PUB fields changed');
  const precKeys = 'ch011Prec,h2cPrec,wpCh07Prec,wpCh811Prec,ch011Full,h2cFull,wpCh07Full,wpCh811Full';
  if (Object.keys(t.HFC_PREC).join() !== precKeys) throw new Error('HFC_PREC fields changed');
  const size = (k) => `DleSize { pge_size: ${t.SIZE[k][0]}, lnk_pge_num: ${t.SIZE[k][1]}, unlnk_pge_num: ${t.SIZE[k][2]} }`;
  const list = (a) => `&[${a.join(', ')}]`;
  const out = [...header('js/dmac.mjs: rtw89_mac_size, rtw8852b_dle_mem_pcie, rtw8852b_hfc_chcfg_pcie'),
    'use super::{DleMem, DleSize, HfcCh, HfcPrec, HfcPub};', '',
    `pub const WDE_PG_64: u16 = ${t.WDE_PG_64};`, `pub const PLE_PG_128: u16 = ${t.PLE_PG_128};`,
    `pub const FIFO: u32 = ${t.FIFO};`, `pub const SCC_RSVD: u32 = ${t.SCC_RSVD};`,
    `pub const DMA_CH_MASK: u32 = ${hex(t.DMA_CH_MASK)};`, `pub const DMA_H2C: u32 = ${t.DMA_H2C};`, '',
    '/// rtw8852b_dle_mem_pcie, one row per quota mode', 'pub static DLE_MEM: &[DleMem] = &['];
  for (const [mode, c] of Object.entries(t.DLE_MEM)) {
    out.push(`    DleMem { mode: "${mode}", wde_size: ${size(c.wde)}, ple_size: ${size(c.ple)},`,
      `        wde_min: ${list(t.WDE_QT[c.wdeMin])}, wde_max: ${list(t.WDE_QT[c.wdeMax])},`,
      `        ple_min: ${list(t.PLE_QT[c.pleMin])}, ple_max: ${list(t.PLE_QT[c.pleMax])} },`);
  }
  out.push('];', '', '/// rtw8852b_hfc_chcfg_pcie: per DMA channel ACH0..B1HI, then FWCMDQ', 'pub static HFC_CH: &[HfcCh] = &[');
  for (const [min, max, grp] of t.HFC_CH) out.push(`    HfcCh { min: ${min}, max: ${max}, grp: ${grp} },`);
  const p = t.HFC_PUB, q = t.HFC_PREC;
  out.push('];', '', `pub static HFC_PUB: HfcPub = HfcPub { grp0: ${p.grp0}, grp1: ${p.grp1}, pub_max: ${p.pubMax}, wp_thrd: ${p.wpThrd} };`,
    `pub static HFC_PREC: HfcPrec = HfcPrec { ch011_prec: ${q.ch011Prec}, h2c_prec: ${q.h2cPrec}, wp_ch07_prec: ${q.wpCh07Prec},`,
    `    wp_ch811_prec: ${q.wpCh811Prec}, ch011_full: ${q.ch011Full}, h2c_full: ${q.h2cFull}, wp_ch07_full: ${q.wpCh07Full},`,
    `    wp_ch811_full: ${q.wpCh811Full} };`, '');
  return out.join('\n');
}

// gen/fw-feat.mjs: the FEAT_8852B rows of js/fw-parse.mjs, checked against featuresFor
export async function fwFeatRs() {
  const { R } = await wzImport('kit/regs.mjs');
  const { verCode, featuresFor } = await wzImport('js/fw-parse.mjs');
  const table = jsText('fw-parse.mjs').match(/const FEAT_8852B = \[([\s\S]*?)\n\];/);
  if (!table) throw new Error('FEAT_8852B not found in fw-parse.mjs');
  const rows = [...table[1].matchAll(/\['(\w+)',\s*(\d+),\s*(\d+),\s*(\d+),\s*(\d+),\s*'(\w+)'\]/g)]
    .map((m) => ({ cond: m[1], code: verCode(+m[2], +m[3], +m[4], +m[5]), name: m[6] }));
  if (!rows.length) throw new Error('no rows in FEAT_8852B');
  for (const { code } of rows) for (const c of [code - 1, code, code + 1]) {
    const want = [...featuresFor(c)].sort().join(' ');
    const got = rows.filter((r) => ({ ge: c >= r.code, le: c <= r.code, lt: c < r.code })[r.cond]).map((r) => r.name)
      .filter((n, i, a) => a.indexOf(n) === i).sort().join(' ');
    if (want !== got) throw new Error(`rows read wrong at code ${c}: ${got} vs ${want}`);
  }
  const cond = { ge: 'Ge', le: 'Le', lt: 'Lt' };
  const out = [...header('js/fw-parse.mjs FEAT_8852B, Linux fw_feat_tbl fw.c:871-883'), 'use super::{FeatCond, FwFeat};', ''];
  for (const n of [...new Set(rows.map((r) => r.name))]) out.push(`pub const RTW89_FW_FEATURE_${n}: u32 = ${R(`RTW89_FW_FEATURE_${n}`)};`);
  out.push('', '/// per row: version code (fw-parse.mjs verCode), condition, feature bit', 'pub static FEAT_TBL: &[FwFeat] = &[');
  for (const r of rows) {
    out.push(`    FwFeat { cond: FeatCond::${cond[r.cond]}, code: ${hex(r.code, 8)}, feature: RTW89_FW_FEATURE_${r.name}, name: "${r.name}" },`);
  }
  out.push('];', '');
  return out.join('\n');
}

// gen/txrx.mjs: rtw89_bitrates and the PHY status IE lengths
export async function txrxRs() {
  const { R } = await wzImport('kit/regs.mjs');
  const { BITRATES, bandRates } = await wzImport('js/txrx-ieee.mjs');
  const m = jsText('txrx-rx.mjs').match(/^const PHYST_IE_LEN = (\[[\s\S]*?\]);/m);
  if (!m) throw new Error('txrx-rx.mjs: PHYST_IE_LEN not found');
  const VL = R('VAR_LEN');
  const ieLen = new Function('VL', `return ${m[1]};`)(VL);
  if (ieLen.length !== 32) throw new Error('PHYST_IE_LEN: 32 entries expected');
  const first5g = BITRATES.length - bandRates('5g').length;
  if (bandRates('5g').join() !== BITRATES.slice(first5g).join()) throw new Error('the 5 GHz rates are no tail of BITRATES');
  return [...header('js/txrx-ieee.mjs BITRATES, js/txrx-rx.mjs PHYST_IE_LEN'),
    ...intSlice('BITRATES', BITRATES, 'rtw89_bitrates (core.c:153) in 100 kbit/s, as js/txrx-ieee.mjs BITRATES'),
    `/// index of the first 5 GHz rate in BITRATES`, `pub const RATES_5G: usize = ${first5g};`,
    `/// VAR_LEN in PHYST_IE_LEN: the length comes from the IE header`, `pub const VAR_LEN: u8 = ${VL};`,
    `/// rtw89_phy_gen_ax.physt_ie_len (phy.c:9045) by IE type`,
    `pub static PHYST_IE_LEN: [u8; 32] = [${ieLen.map(lit).join(', ')}];`, ''].join('\n');
}

// gen/btc.mjs: the coex enums, slot defaults, monitor registers and policy tables
export async function btcRs() {
  const { R } = await wzImport('kit/regs.mjs');
  const run = await wzImport('js/btc-run.mjs');
  const { S_DEF, T_DEF_OFF, MON_REG } = await wzImport('js/btc-h2c.mjs');
  const out = [...header('js/btc-run.mjs, js/btc-h2c.mjs, js/btc-wlsta.mjs (coex.c, rtw8852b.c)'),
    '#![allow(dead_code)]', 'use super::{BtcSlot, MonReg};', ''];
  for (const e of ['RSN', 'ACT', 'WSCB', 'BSCB', 'WLINK', 'MLME']) {
    out.push(`/// coex.c enum (btc-run.mjs ${e})`, `pub mod ${e.toLowerCase()} {`);
    for (const [k, v] of Object.entries(run[e])) out.push(`    pub const ${k}: u32 = ${lit(v)};`);
    out.push('}', '');
  }
  out.push('/// Linux names btc.mjs and btc-hw.mjs reach through a variable');
  for (const n of ['BTC_ROLE_START', 'BTC_ROLE_STOP', 'BTC_ROLE_MSTS_STA_CONN_START', 'BTC_ROLE_MSTS_STA_CONN_END',
    'BTC_ROLE_MSTS_STA_DIS_CONN', 'PACKET_DHCP', 'PACKET_ARP', 'PACKET_EAPOL', 'PACKET_EAPOL_END', 'PACKET_ICMP',
    'RTW89_MAC_AX_PLT_LTE_RX', 'RTW89_MAC_AX_PLT_GNT_BT_TX', 'RTW89_MAC_AX_PLT_GNT_BT_RX', 'RTW89_MAC_AX_PLT_GNT_WL',
    'B_AX_TX_PLT_GNT_LTE_RX', 'B_AX_TX_PLT_GNT_BT_TX', 'B_AX_TX_PLT_GNT_BT_RX', 'B_AX_TX_PLT_GNT_WL',
    'B_AX_RX_PLT_GNT_LTE_RX', 'B_AX_RX_PLT_GNT_BT_TX', 'B_AX_RX_PLT_GNT_BT_RX', 'B_AX_RX_PLT_GNT_WL']) {
    out.push(`pub const ${n}: u32 = ${lit(R(n))};`);
  }
  if (S_DEF.length !== R('CXST_MAX')) throw new Error('s_def is not CXST_MAX slots');
  out.push('', '/// s_def (coex.c:81): dur, cxtbl, cxtype per slot (CXST_MAX)', 'pub static S_DEF: &[BtcSlot] = &[');
  for (const [dur, tbl, type] of S_DEF) out.push(`    BtcSlot { dur: ${dur}, cxtbl: ${hex(tbl, 8)}, cxtype: ${type} },`);
  out.push('];', '', ...intSlice('T_DEF_OFF', T_DEF_OFF, 't_def[CXTD_OFF] (coex.c:65)', 'u8'), '',
    '/// rtw89_btc_8852b_mon_reg (rtw8852b.c:334)', 'pub static MON_REG: &[MonReg] = &[');
  for (const [type, bytes, offset] of MON_REG) out.push(`    MonReg { kind: ${type}, bytes: ${bytes}, offset: ${hex(offset)} },`);
  out.push('];', '', ...intSlice('CXTBL', constOf('btc-run.mjs', 'CXTBL'), 'cxtbl (coex.c:101)', 'u32'), '',
    ...intSlice('OFF_TBL', constOf('btc-run.mjs', 'OFF_TBL'), 'TDMA-off policy -> cxtbl index (coex.c:5168)', 'u8'), '');
  for (const [name, doc] of [['RF_UL', 'rtw89_btc_8852b_rf_ul_v0 (rtw8852b.c:310)'], ['RF_DL', 'rtw89_btc_8852b_rf_dl_v0 (rtw8852b.c:322)']]) {
    out.push(`/// ${doc}: wl_tx_power, wl_rx_gain, bt_tx_power, bt_rx_gain`, `pub static ${name}: &[[u8; 4]] = &[`);
    for (const r of constOf('btc-run.mjs', name)) {
      if (r.length !== 4) throw new Error(`${name}: a row of ${r.length}`);
      out.push(`    [${r.map(lit).join(', ')}],`);
    }
    out.push('];', '');
  }
  out.push(...intSlice('WL_RSSI_THRES', constOf('btc-wlsta.mjs', 'WL_RSSI_THRES'), 'rtw89_btc_8852b_wl_rssi_thres (rtw8852b.c:353)', 'u8'), '');
  return out.join('\n');
}
