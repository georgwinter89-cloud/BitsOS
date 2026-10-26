// spot-tables.mjs: a fingerprint of every generated table, taken straight from
// the JS driver's values (the same sources the emitter reads, not its output),
// for tests/gen_spot.rs. One line per table:
//   tbl.<module>::<NAME> len n sum wsum first_n first_sum last_n last_sum
// len: rows; n, sum: all integers in the table (booleans 0/1, strings
// skipped); wsum: sum of (row index + 1) * row sum, so a moved row shows;
// first/last: integer count and sum of the first and last row.
import { jsText, wzImport, constOf, literals, literalAfter, snake } from './lib.mjs';

function ints(v, out = []) {
  if (typeof v === 'number') {
    if (!Number.isInteger(v)) throw new Error(`not an integer: ${v}`);
    out.push(BigInt(v));
  } else if (typeof v === 'boolean') out.push(v ? 1n : 0n);
  else if (Array.isArray(v)) for (const x of v) ints(x, out);
  else if (v && typeof v === 'object') for (const x of Object.values(v)) ints(x, out);
  return out;
}
const sum = (a) => a.reduce((s, x) => s + x, 0n);
const lines = [];
function fp(key, rows) {
  if (!Array.isArray(rows)) throw new Error(`${key}: no rows`);
  const per = rows.map((r) => ints(r));
  const all = per.flat();
  const w = per.reduce((s, r, i) => s + BigInt(i + 1) * sum(r), 0n);
  const f = per[0] ?? [], l = per[per.length - 1] ?? [];
  lines.push(`tbl.${key} ${rows.length} ${all.length} ${sum(all)} ${w} ${f.length} ${sum(f)} ${l.length} ${sum(l)}`);
}

const { R } = await wzImport('kit/regs.mjs');

// dmac.rs (js/dmac.mjs module constants, evaluated from its text as the emitter does)
{
  const NAMES = ['WDE_PG_64', 'PLE_PG_128', 'SIZE', 'WDE_QT', 'PLE_QT', 'DLE_MEM', 'HFC_CH', 'HFC_PUB', 'HFC_PREC'];
  const text = jsText('dmac.mjs');
  const decls = [...text.matchAll(/^const ([\s\S]*?);\n/gm)].map((m) => m[1])
    .filter((d) => NAMES.some((n) => new RegExp(`^${n}\\b|,\\s*${n}\\s*=`).test(d)));
  const t = new Function(`const ${decls.join(';\nconst ')};\nreturn { ${NAMES.join(', ')} };`)();
  fp('dmac::DLE_MEM', Object.values(t.DLE_MEM).map((c) =>
    [t.SIZE[c.wde], t.SIZE[c.ple], t.WDE_QT[c.wdeMin], t.WDE_QT[c.wdeMax], t.PLE_QT[c.pleMin], t.PLE_QT[c.pleMax]]));
  fp('dmac::HFC_CH', t.HFC_CH);
  fp('dmac::HFC_PUB', [t.HFC_PUB]);
  fp('dmac::HFC_PREC', [t.HFC_PREC]);
}

// fw_feat.rs: FEAT_8852B rows of js/fw-parse.mjs (code, feature bit)
{
  const { verCode } = await wzImport('js/fw-parse.mjs');
  const body = jsText('fw-parse.mjs').match(/const FEAT_8852B = \[([\s\S]*?)\n\];/)[1];
  fp('fw_feat::FEAT_TBL', [...body.matchAll(/\['(\w+)',\s*(\d+),\s*(\d+),\s*(\d+),\s*(\d+),\s*'(\w+)'\]/g)]
    .map((m) => [verCode(+m[2], +m[3], +m[4], +m[5]), R(`RTW89_FW_FEATURE_${m[6]}`)]));
}

// txrx.rs
{
  const { BITRATES } = await wzImport('js/txrx-ieee.mjs');
  fp('txrx::BITRATES', BITRATES);
  fp('txrx::PHYST_IE_LEN', new Function('VL', `return ${jsText('txrx-rx.mjs').match(/^const PHYST_IE_LEN = (\[[\s\S]*?\]);/m)[1]};`)(R('VAR_LEN')));
}

// btc.rs
{
  const { S_DEF, T_DEF_OFF, MON_REG } = await wzImport('js/btc-h2c.mjs');
  fp('btc::S_DEF', S_DEF);
  fp('btc::T_DEF_OFF', T_DEF_OFF);
  fp('btc::MON_REG', MON_REG);
  for (const n of ['CXTBL', 'OFF_TBL', 'RF_UL', 'RF_DL']) fp(`btc::${n}`, constOf('btc-run.mjs', n));
  fp('btc::WL_RSSI_THRES', constOf('btc-wlsta.mjs', 'WL_RSSI_THRES'));
}

// chan.rs
{
  const [rules] = literals(jsText('chan-regd.mjs'), 'const RULES =', 1, '{');
  for (const [country, label] of [['DE', 'DE'], ['00', 'WW']]) {
    for (const band of ['2g', '5g']) {
      fp(`chan::RULES_${label}_${band.toUpperCase()}`, rules[country][band].map(([a, b, noIr, radar]) => [a, b, !!noIr, !!radar]));
    }
  }
  const bb = jsText('chan-bb.mjs');
  fp('chan::SCO_BARKER', literals(bb, 'const SCO_BARKER =', 1)[0]);
  fp('chan::SCO_CCK', literals(bb, 'const SCO_CCK =', 1)[0]);
  fp('chan::LNA_REGS', literals(bb, 'const LNA_REGS =', 1)[0]);
  fp('chan::TIA_REGS', literals(bb, 'const TIA_REGS =', 1)[0]);
  const [fir14, fir, regs, bits] = literals(bb, 'const fir =', 4);
  fp('chan::TXFIR', regs.map((n, i) => [R(`R_TXFIR${n}`), R(`B_TXFIR_${bits[i]}`), fir14[i], fir[i]]));
  fp('chan::CH_BASE', literals(bb, 'const base =', 1)[0]);
  const [dfir] = literals(jsText('chan-txpwr.mjs'), 'const DFIR =', 1, '{');
  for (const k of ['flat', 'sharp', 'sharp14']) fp(`chan::DFIR_${k.toUpperCase()}`, dfir[k]);
  const fw = jsText('chan-fw.mjs');
  const conf = fw.slice(fw.indexOf('const TXPWR_CONF'), fw.indexOf('};', fw.indexOf('const TXPWR_CONF')));
  fp('chan::TXPWR_CONF', [...conf.matchAll(/ID\('(\w+)'\)\]: '(\w+)'/g)].map((m) => [R(`RTW89_FW_ELEMENT_ID_${m[1]}`)]));
}

// phy.rs
{
  for (const n of ['PMAC_HT20_MCS7', 'PREAGC_EN', 'PREAGC_DIS']) fp(`phy::${n}`, constOf('phy-bbx.mjs', n));
  fp('phy::DELAY_US', Object.entries(constOf('phy-tbl.mjs', 'DELAY_US')).map(([a, us]) => [Number(a), us]));
  fp('phy::CH_BASE', constOf('phy.mjs', 'CH_BASE'));
  for (const n of ['IGI_RSSI_TH', 'FA_TH_2G', 'FA_TH_5G', 'FA_TH_NOLINK']) fp(`phy::${n}`, constOf('phy-dm.mjs', n));
  fp('phy::TRIM_ADDR', constOf('phy-efuse.mjs', 'trimAddr'));
  fp('phy::COMP_ADDRS', constOf('phy-efuse.mjs', 'compAddrs'));
}

// rfk1.rs: every export of js/rfk1-tables.mjs
for (const [name, v] of Object.entries(await wzImport('js/rfk1-tables.mjs'))) fp(`rfk1::${snake(name)}`, v);

// rfk2.rs
{
  const T = await wzImport('js/rfk2-tables.mjs');
  const label = (n) => n.replace(/^rtw8852b_/, '').toUpperCase();
  for (const [name, v] of Object.entries(T)) if (name !== 'rtw8852bx_pmac_ht20_mcs7_tbl') fp(`rfk2::${label(name)}`, v);
  const ab = (base) => ['a_defs_2g', 'a_defs_5g', 'b_defs_2g', 'b_defs_5g'].map((s) => T[`rtw8852b_${base}_${s}`]);
  fp('rfk2::PICK_TSSI_SYS_PATH', ab('tssi_sys'));
  fp('rfk2::PICK_TSSI_SLOPE_ORG', ab('tssi_slope'));
  for (const [lbl, base] of [['PICK_TSSI_INIT_TXPWR', 'tssi_init_txpwr_defs'], ['PICK_TSSI_INIT_TXPWR_HE_TB', 'tssi_init_txpwr_he_tb_defs'],
    ['PICK_TSSI_DCK', 'tssi_dck_defs'], ['PICK_TSSI_DAC_GAIN', 'tssi_dac_gain_defs'], ['PICK_TSSI_SLOPE', 'tssi_slope_defs']]) {
    fp(`rfk2::${lbl}`, [T[`rtw8852b_${base}_a`], T[`rtw8852b_${base}_b`]]);
  }
  const align = [];
  for (const p of ['a', 'b']) for (const g of ['2g', '5g1', '5g2', '5g3']) for (const k of ['all', 'part']) align.push(T[`rtw8852b_tssi_align_${p}_${g}_${k}_defs`]);
  fp('rfk2::PICK_TSSI_ALIGN', align);
  for (const [k, v] of Object.entries(literalAfter('rfk2-tssi.mjs', 'const DE ='))) fp(`rfk2::DE_${snake(k)}`, v);
  const groups = (rows) => rows.map((r) => [r[0], r[1], r[2], !!r[3]]);
  fp('rfk2::CCK_GROUPS', groups(literalAfter('rfk2-tssi.mjs', 'function tssiGetCckGroup', ' of ')));
  fp('rfk2::OFDM_GROUPS', groups(literalAfter('rfk2-tssi.mjs', 'const OFDM_GROUPS =')));
  fp('rfk2::TRIM_GROUPS', groups(literalAfter('rfk2-tssi.mjs', 'function tssiGetTrimGroup', ' of ')));
  for (const n of ['TSSI_TRIGGER', 'TSSI_CW_RPT_ADDR', 'TSSI_CW_DEFAULT_ADDR', 'TSSI_CW_DEFAULT_MASK', 'ALIM_BB_REG']) {
    fp(`rfk2::${n}`, literalAfter('rfk2-alimk.mjs', `const ${n} =`));
  }
  fp('rfk2::POWER', literalAfter('rfk2-alimk.mjs', 'const POWER_2G ='));
  fp('rfk2::KIP_REG', literalAfter('rfk2-dpkflow.mjs', 'const KIP_REG ='));
}

// trk.rs: by enum rtw89_fw_txpwr_trk_type, missing types as no rows
{
  const { rtw89_8852b_trk_cfg } = await wzImport('js/rfk2-trkdata.mjs');
  fp('trk::TRK_CFG', Array.from({ length: R('RTW89_FW_TXPWR_TRK_TYPE_NR') }, (_, i) => rtw89_8852b_trk_cfg.delta[i] ?? null));
}

console.log(lines.join('\n'));
