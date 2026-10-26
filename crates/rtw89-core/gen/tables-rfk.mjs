// tables-rfk.mjs: src/gen/{rfk1,rfk2,trk}.rs, the data of
// werkzeuge/plattform/wlan/gen/{rfk1-tables,rfk2-tables,rfk2-trkdata}.mjs as
// typed Rust: sequence tables as `Rfk` entries (the original packs them into
// bytes for the NASM parser; Rust keeps the entries).
import { wzImport, literalAfter, header, hex, intSlice, snake } from './lib.mjs';

// one rfk1-tables.mjs / rfk2-tables.mjs entry -> a Rust `Rfk` value, checked as rfkRecord does
function rfk(entry) {
  const [flag, ...a] = entry;
  const int = (v) => { if (!Number.isInteger(v) || v < 0 || v > 0xffffffff) throw new Error(`rfk: ${JSON.stringify(entry)}`); return v; };
  switch (flag) {
    case 'WM': return `Rfk::Wm { addr: ${hex(int(a[0]))}, mask: ${hex(int(a[1]), 8)}, data: ${hex(int(a[2]))} }`;
    case 'WS': return `Rfk::Ws { addr: ${hex(int(a[0]))}, bits: ${hex(int(a[1]), 8)} }`;
    case 'WC': return `Rfk::Wc { addr: ${hex(int(a[0]))}, bits: ${hex(int(a[1]), 8)} }`;
    case 'DELAY': return `Rfk::Delay { us: ${int(a[0])} }`;
    case 'WRF':
      if (a[0] !== 0 && a[0] !== 1) throw new Error(`rfk: path ${a[0]}`);
      return `Rfk::Wrf { path: ${a[0]}, addr: ${hex(int(a[1]))}, mask: ${hex(int(a[2]), 8)}, data: ${hex(int(a[3]))} }`;
    default: throw new Error(`rfk: unknown flag ${flag}`);
  }
}
const table = (name, doc, entries) => [`/// ${doc}`, `pub static ${name}: &[Rfk] = &[`, ...entries.map((e) => `    ${rfk(e)},`), '];', ''];

// gen/rfk1-tables.mjs: every export of js/rfk1-tables.mjs
export async function rfk1Rs() {
  const T = await wzImport('js/rfk1-tables.mjs');
  const out = [...header('js/rfk1-tables.mjs: rtw8852b_rfk_table.c, rtw8852b_rfk.c'), 'use super::Rfk;', ''];
  for (const [name, v] of Object.entries(T)) {
    if (!Array.isArray(v)) throw new Error(`rfk1-tables.mjs: ${name} is no array`);
    const rname = snake(name);
    if (Array.isArray(v[0])) {
      const tbl = typeof v[0][0] === 'string' ? v : v.map(([addr, mask, data]) => ['WM', addr, mask, data]);
      out.push(...table(rname, `rfk1-tables.mjs ${name}`, tbl));
    } else {
      out.push(...intSlice(rname, v, `rfk1-tables.mjs ${name}`), '');
    }
  }
  return out.join('\n');
}

// gen/rfk2-tables.mjs: the TSSI and DPK sequences, the pickers, the module-private arrays
export async function rfk2Rs() {
  const T = await wzImport('js/rfk2-tables.mjs');
  const SKIP = new Set(['rtw8852bx_pmac_ht20_mcs7_tbl']);                       // phy-bbx's (phy.rs PMAC_HT20_MCS7)
  const label = (n) => n.replace(/^rtw8852b_/, '').toUpperCase();
  const out = [...header('js/rfk2-tables.mjs (rtw8852b_rfk_table.c), rfk2-tssi/-alimk/-dpkflow.mjs'),
    'use super::{Rfk, TssiGroup};', ''];
  for (const [name, tbl] of Object.entries(T)) {
    if (SKIP.has(name)) continue;
    out.push(...table(label(name), name, tbl.map((e) => ['WM', ...e])));
  }
  const pick = (lbl, doc, names) => {
    for (const n of names) if (!T[n]) throw new Error(`no table ${n}`);
    out.push(`/// ${doc}`, `pub static ${lbl}: [&[Rfk]; ${names.length}] = [${names.map(label).join(', ')}];`);
  };
  const ab = (base) => [`${base}_a_defs_2g`, `${base}_a_defs_5g`, `${base}_b_defs_2g`, `${base}_b_defs_5g`].map((s) => `rtw8852b_${s}`);
  pick('PICK_TSSI_SYS_PATH', '[A 2g, A 5g, B 2g, B 5g]', ab('tssi_sys'));
  pick('PICK_TSSI_SLOPE_ORG', '[A 2g, A 5g, B 2g, B 5g]', ab('tssi_slope'));
  for (const [lbl, base] of [['PICK_TSSI_INIT_TXPWR', 'tssi_init_txpwr_defs'], ['PICK_TSSI_INIT_TXPWR_HE_TB', 'tssi_init_txpwr_he_tb_defs'],
    ['PICK_TSSI_DCK', 'tssi_dck_defs'], ['PICK_TSSI_DAC_GAIN', 'tssi_dac_gain_defs'], ['PICK_TSSI_SLOPE', 'tssi_slope_defs']]) {
    pick(lbl, '[path A, path B]', [`rtw8852b_${base}_a`, `rtw8852b_${base}_b`]);
  }
  const align = [];
  for (const p of ['a', 'b']) for (const g of ['2g', '5g1', '5g2', '5g3']) for (const k of ['all', 'part']) align.push(`rtw8852b_tssi_align_${p}_${g}_${k}_defs`);
  pick('PICK_TSSI_ALIGN', '_tssi_alignment_default: [path][2g, 5g1, 5g2, 5g3][all, part], flattened', align);
  out.push('');
  const de = literalAfter('rfk2-tssi.mjs', 'const DE =');
  out.push('/// rfk2-tssi.mjs DE (_tssi_de_*): [path A, path B] per field');
  for (const [k, v] of Object.entries(de)) out.push(`pub const DE_${snake(k)}: [u32; 2] = [${v.map((x) => hex(x)).join(', ')}];`);
  const groups = (lbl, doc, rows) => {
    out.push(`/// ${doc}: first channel, last channel, group, extra`, `pub static ${lbl}: &[TssiGroup] = &[`);
    for (const r of rows) out.push(`    TssiGroup { first: ${r[0]}, last: ${r[1]}, group: ${r[2]}, extra: ${!!r[3]} },`);
    out.push('];');
  };
  groups('CCK_GROUPS', '_tssi_get_cck_group', literalAfter('rfk2-tssi.mjs', 'function tssiGetCckGroup', ' of '));
  groups('OFDM_GROUPS', '_tssi_get_ofdm_group', literalAfter('rfk2-tssi.mjs', 'const OFDM_GROUPS ='));
  groups('TRIM_GROUPS', '_tssi_get_trim_group', literalAfter('rfk2-tssi.mjs', 'function tssiGetTrimGroup', ' of '));
  const alimk = (n) => literalAfter('rfk2-alimk.mjs', `const ${n} =`);
  const power = alimk('POWER_2G'), power5 = literalAfter('rfk2-alimk.mjs', 'POWER_5G =');
  if (power.join() !== power5.join()) throw new Error('POWER_2G and POWER_5G differ');
  const cw = alimk('TSSI_CW_DEFAULT_ADDR');
  out.push('', ...intSlice('TSSI_TRIGGER', alimk('TSSI_TRIGGER'), 'rfk2-alimk.mjs TSSI_TRIGGER by path', 'u32'),
    ...intSlice('TSSI_CW_RPT_ADDR', alimk('TSSI_CW_RPT_ADDR'), 'rfk2-alimk.mjs TSSI_CW_RPT_ADDR by path', 'u32'),
    '/// rfk2-alimk.mjs TSSI_CW_DEFAULT_ADDR [path][n]',
    `pub static TSSI_CW_DEFAULT_ADDR: [[u32; ${cw[0].length}]; ${cw.length}] = [`, ...cw.map((r) => `    [${r.map((v) => hex(v)).join(', ')}],`), '];',
    ...intSlice('TSSI_CW_DEFAULT_MASK', alimk('TSSI_CW_DEFAULT_MASK'), 'rfk2-alimk.mjs TSSI_CW_DEFAULT_MASK', 'u32'),
    ...intSlice('ALIM_BB_REG', alimk('ALIM_BB_REG'), 'rfk2-alimk.mjs ALIM_BB_REG', 'u32'),
    ...intSlice('POWER', power, 'rfk2-alimk.mjs POWER_2G (= POWER_5G)', 'u32'),
    ...intSlice('KIP_REG', literalAfter('rfk2-dpkflow.mjs', 'const KIP_REG ='), 'rfk2-dpkflow.mjs KIP_REG (_dpk_cal_select kip_reg[])', 'u32'), '');
  return out.join('\n');
}

// gen/rfk2-trkdata.mjs: rtw89_8852b_trk_cfg, by enum rtw89_fw_txpwr_trk_type
export async function trkRs() {
  const { R } = await wzImport('kit/regs.mjs');
  const { rtw89_8852b_trk_cfg } = await wzImport('js/rfk2-trkdata.mjs');
  const SIZE = 30, NR = R('RTW89_FW_TXPWR_TRK_TYPE_NR');
  const out = [...header('js/rfk2-trkdata.mjs: Linux rtw89_8852b_trk_cfg, rtw8852b_table.c'), '',
    `/// DELTA_SWINGIDX_SIZE`, `pub const DELTA_SWINGIDX_SIZE: usize = ${SIZE};`, ''];
  const ptr = [];
  for (let type = 0; type < NR; type++) {
    const rows = rtw89_8852b_trk_cfg.delta[type];
    if (!rows) { ptr.push('None'); continue; }
    ptr.push(`Some(TRK_CFG_${type})`);
    out.push(`static TRK_CFG_${type}: &[[i8; ${SIZE}]] = &[`);
    for (const row of rows) {
      if (row.length !== SIZE || row.some((v) => !Number.isInteger(v) || v < -128 || v > 127)) throw new Error(`type ${type}: bad row`);
      out.push(`    [${row.join(', ')}],`);
    }
    out.push('];');
  }
  out.push('', '/// per enum rtw89_fw_txpwr_trk_type: the s8 delta rows, or None',
    `pub static TRK_CFG: [Option<&[[i8; ${SIZE}]]>; ${NR}] = [`, ...ptr.map((p) => `    ${p},`), '];', '');
  return out.join('\n');
}
