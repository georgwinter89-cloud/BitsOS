// tables-phy.mjs: src/gen/{chan,phy}.rs, the data of
// werkzeuge/plattform/wlan/gen/{chan-tables,phy}.mjs as typed Rust.
import { jsText, wzImport, constOf, literals, header, hex, intSlice, snake } from './lib.mjs';

// gen/chan-tables.mjs
export async function chanRs() {
  const { R } = await wzImport('kit/regs.mjs');
  const { DEFAULT_COUNTRY } = await wzImport('js/chan-regd.mjs');
  const out = [...header('js/chan-regd.mjs, chan-bb.mjs, chan-txpwr.mjs, chan-fw.mjs'),
    'use super::{ChanRule, GainReg, TxFir, TxpwrConf};', '',
    `/// chan-regd.mjs DEFAULT_COUNTRY`, `pub const DEFAULT_COUNTRY: &str = "${DEFAULT_COUNTRY}";`, ''];
  const [rules] = literals(jsText('chan-regd.mjs'), 'const RULES =', 1, '{');
  for (const [country, label] of [['DE', 'DE'], ['00', 'WW']]) {
    for (const band of ['2g', '5g']) {
      out.push(`/// chan-regd.mjs RULES['${country}']['${band}']`, `pub static RULES_${label}_${band.toUpperCase()}: &[ChanRule] = &[`);
      for (const [first, last, noIr, radar] of rules[country][band]) {
        out.push(`    ChanRule { first: ${first}, last: ${last}, no_ir: ${!!noIr}, radar: ${!!radar} },`);
      }
      out.push('];');
    }
  }
  const bb = jsText('chan-bb.mjs');
  const [barker] = literals(bb, 'const SCO_BARKER =', 1);
  const [cck] = literals(bb, 'const SCO_CCK =', 1);
  out.push('', ...intSlice('SCO_BARKER', barker, 'rtw8852bx_ctrl_sco_cck barker thresholds by primary channel - 1', 'u32'),
    ...intSlice('SCO_CCK', cck, 'rtw8852bx_ctrl_sco_cck CCK thresholds by primary channel - 1', 'u32'), '');
  const gains = (name, rows) => {
    out.push(`/// chan-bb.mjs ${name}: 2 GHz registers of paths A, B; 5 GHz ones; the mask`, `pub static ${name}: &[GainReg] = &[`);
    for (const [gg, ga, mask] of rows) out.push(`    GainReg { g2: [${gg.map((v) => hex(v)).join(', ')}], g5: [${ga.map((v) => hex(v)).join(', ')}], mask: ${hex(mask, 8)} },`);
    out.push('];');
  };
  gains('LNA_REGS', literals(bb, 'const LNA_REGS =', 1)[0]);
  gains('TIA_REGS', literals(bb, 'const TIA_REGS =', 1)[0]);
  const [fir14, fir, regs, bits] = literals(bb, 'const fir =', 4);
  out.push('', '/// ctrlCh TX FIR: register, mask, value on channel 14, value elsewhere', 'pub static TXFIR: &[TxFir] = &[');
  regs.forEach((n, i) => out.push(`    TxFir { reg: ${hex(R(`R_TXFIR${n}`))}, mask: ${hex(R(`B_TXFIR_${bits[i]}`), 8)}, ch14: ${hex(fir14[i])}, other: ${hex(fir[i])} },`));
  out.push('];', ...intSlice('CH_BASE', literals(bb, 'const base =', 1)[0], 'chan-bb.mjs base (rtw89_ch_base_table, 2/5 GHz part)', 'u8'), '');
  const [dfir] = literals(jsText('chan-txpwr.mjs'), 'const DFIR =', 1, '{');
  for (const k of ['flat', 'sharp', 'sharp14']) out.push(...intSlice(`DFIR_${k.toUpperCase()}`, dfir[k], `chan-txpwr.mjs DFIR.${k}`, 'u32'));
  const fw = jsText('chan-fw.mjs');
  const conf = fw.slice(fw.indexOf('const TXPWR_CONF'), fw.indexOf('};', fw.indexOf('const TXPWR_CONF')));
  const keys = [...conf.matchAll(/ID\('(\w+)'\)\]: '(\w+)'/g)];
  if (!keys.length) throw new Error('TXPWR_CONF not found');
  out.push('', '/// chan-fw.mjs TXPWR_CONF: firmware element id -> configuration key, in its order', 'pub static TXPWR_CONF: &[TxpwrConf] = &[');
  for (const [, id, key] of keys) out.push(`    TxpwrConf { id: ${R(`RTW89_FW_ELEMENT_ID_${id}`)}, key: "${key}" },`);
  out.push('];', '');
  return out.join('\n');
}

// gen/phy.mjs
export function phyRs() {
  const out = [...header('js/phy-bbx.mjs, phy-tbl.mjs, phy.mjs, phy-dm.mjs, phy-efuse.mjs'), 'use super::MaskWrite;', ''];
  const reg3 = (name, file, cname, doc) => {
    out.push(`/// ${doc} (${file} ${cname}): addr, mask, data`, `pub static ${name}: &[MaskWrite] = &[`);
    for (const [addr, mask, data] of constOf(file, cname)) out.push(`    MaskWrite { addr: ${hex(addr)}, mask: ${hex(mask, 8)}, data: ${hex(data)} },`);
    out.push('];', '');
  };
  reg3('PMAC_HT20_MCS7', 'phy-bbx.mjs', 'PMAC_HT20_MCS7', 'rtw8852bx_pmac_ht20_mcs7_tbl');
  reg3('PREAGC_EN', 'phy-bbx.mjs', 'PREAGC_EN', 'rtw8852bx_btc_preagc_en_defs');
  reg3('PREAGC_DIS', 'phy-bbx.mjs', 'PREAGC_DIS', 'rtw8852bx_btc_preagc_dis_defs');
  const delays = Object.entries(constOf('phy-tbl.mjs', 'DELAY_US')).map(([a, us]) => [Number(a), us]);
  out.push('/// phy-tbl.mjs DELAY_US: rtw89_io_mdelay/udelay pseudo address -> microseconds', 'pub static DELAY_US: &[(u32, u32)] = &[');
  for (const [a, us] of delays) out.push(`    (${hex(a)}, ${us}),`);
  out.push('];', '',
    ...intSlice('CH_BASE', constOf('phy.mjs', 'CH_BASE'), 'rtw89_ch_base_table (phy.mjs CH_BASE)', 'u8'),
    ...intSlice('IGI_RSSI_TH', constOf('phy-dm.mjs', 'IGI_RSSI_TH'), 'phy-dm.mjs IGI_RSSI_TH', 'u8'),
    ...intSlice('FA_TH_2G', constOf('phy-dm.mjs', 'FA_TH_2G'), 'phy-dm.mjs FA_TH_2G', 'u16'),
    ...intSlice('FA_TH_5G', constOf('phy-dm.mjs', 'FA_TH_5G'), 'phy-dm.mjs FA_TH_5G', 'u16'),
    ...intSlice('FA_TH_NOLINK', constOf('phy-dm.mjs', 'FA_TH_NOLINK'), 'phy-dm.mjs FA_TH_NOLINK', 'u16'),
    ...intSlice('TRIM_ADDR', constOf('phy-efuse.mjs', 'trimAddr'), 'readPhycap8852bx trimAddr: TSSI trim per path, decreasing', 'u16'));
  const comp = constOf('phy-efuse.mjs', 'compAddrs');
  out.push('/// readPhycap8852bx compAddrs: gain comp [path][subband], 0 = none',
    `pub static COMP_ADDRS: [[u16; ${comp[0].length}]; ${comp.length}] = [`, ...comp.map((r) => `    [${r.map((v) => hex(v)).join(', ')}],`), '];', '',
    '/// struct rtw8852bx_efuse offsets (phy-efuse.mjs EF)');
  for (const [name, off] of Object.entries(constOf('phy-efuse.mjs', 'EF'))) out.push(`pub const EF_${snake(name)}: usize = ${hex(off)};`);
  out.push('');
  return out.join('\n');
}
