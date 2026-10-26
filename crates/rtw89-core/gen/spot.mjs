// spot.mjs: values straight from the JS driver's tables (the modules and
// kit/regs.mjs, not the emitter), one "key value" line each, for the Rust
// test tests/gen_spot.rs, which compares them with src/gen/*.rs.
//   node gen/spot.mjs NAME...        (register names to look up as well)
import { wzImport, constOf, literals, literalAfter, jsText } from './lib.mjs';

const { R } = await wzImport('kit/regs.mjs');
const out = [];
const put = (k, v) => {
  if (typeof v === 'boolean') v = v ? 1 : 0;
  if (!Number.isInteger(v)) throw new Error(`${k}: ${v}`);
  out.push(`${k} ${v}`);
};

for (const n of process.argv.slice(2)) put(`reg.${n}`, R(n));

const dmac = jsText('dmac.mjs');
const t = new Function(`${dmac.match(/^const SIZE = [\s\S]*?^};/m)[0].replace(/WDE_PG_64/g, '64').replace(/PLE_PG_128/g, '128')}
${dmac.match(/^const PLE_QT = [\s\S]*?^};/m)[0]}
${dmac.match(/^const HFC_CH = [\s\S]*?\]\];/m)[0]}
${dmac.match(/^const HFC_PUB = .*;/m)[0]}
return { SIZE, PLE_QT, HFC_CH, HFC_PUB };`)();
put('dmac.scc.ple_max.4', t.PLE_QT.ple_qt58[4]);
put('dmac.dlfw.wde.unlnk', t.SIZE.wde_size9[2]);
put('dmac.hfc_ch.len', t.HFC_CH.length);
put('dmac.hfc_ch.12.min', t.HFC_CH[12][0]);
put('dmac.hfc_pub.pub_max', t.HFC_PUB.pubMax);

const run = await wzImport('js/btc-run.mjs');
const h2c = await wzImport('js/btc-h2c.mjs');
put('btc.rsn.ACT1_WORK', run.RSN.ACT1_WORK);
put('btc.wscb.ALL', run.WSCB.ALL);
put('btc.cxtbl.3', constOf('btc-run.mjs', 'CXTBL')[3]);
put('btc.cxtbl.len', constOf('btc-run.mjs', 'CXTBL').length);
put('btc.s_def.1.cxtbl', h2c.S_DEF[1][1]);
put('btc.s_def.len', h2c.S_DEF.length);
put('btc.mon_reg.12.offset', h2c.MON_REG[12][2]);
put('btc.rf_dl.5.1', constOf('btc-run.mjs', 'RF_DL')[5][1]);

const bb = jsText('chan-bb.mjs');
put('chan.sco_cck.13', literals(bb, 'const SCO_CCK =', 1)[0][13]);
const [fir14] = literals(bb, 'const fir =', 1);
put('chan.txfir.2.ch14', fir14[2]);
put('chan.lna.6.g5.1', literals(bb, 'const LNA_REGS =', 1)[0][6][1][1]);
const [rules] = literals(jsText('chan-regd.mjs'), 'const RULES =', 1, '{');
put('chan.rules.de.5g.2.last', rules.DE['5g'][2][1]);
put('chan.rules.ww.2g.1.no_ir', rules['00']['2g'][1][2]);

const { featuresFor, verCode } = await wzImport('js/fw-parse.mjs');
put('fw_feat.29_29_18.count', featuresFor(verCode(0, 29, 29, 18)).size);
put('fw_feat.29_29_18.scan_offload', featuresFor(verCode(0, 29, 29, 18)).has('SCAN_OFFLOAD'));

const pmac = constOf('phy-bbx.mjs', 'PMAC_HT20_MCS7');
put('phy.pmac.len', pmac.length);
put('phy.pmac.7.data', pmac[7][2]);
put('phy.delay_us.252', constOf('phy-tbl.mjs', 'DELAY_US')[252]);
put('phy.ef.mac_addr_e', constOf('phy-efuse.mjs', 'EF').macAddrE);

const r1 = await wzImport('js/rfk1-tables.mjs');
put('rfk1.afeInitDefs.len', r1.afeInitDefs.length);
put('rfk1.afeInitDefs.0.data', r1.afeInitDefs[0][3]);
put('rfk1.dackS0_1Defs.len', r1.dackS0_1Defs.length);
put('rfk1.setNondbccPath01.len', r1.setNondbccPath01.length);
put('rfk1.aIdxrxgain.2', r1.aIdxrxgain[2]);
put('rfk1.backupRfRegs.len', r1.backupRfRegs.length);
put('rfk1.checkDadcEnDefsB.6.path', r1.checkDadcEnDefsB[6][1]);
put('rfk1.checkDadcEnDefsB.6.data', r1.checkDadcEnDefsB[6][4]);
put('rfk1.dackS0_1Defs.17.us', r1.dackS0_1Defs[17][1]);

const r2 = await wzImport('js/rfk2-tables.mjs');
put('rfk2.tssi_dac_gain_defs_b.len', r2.rtw8852b_tssi_dac_gain_defs_b.length);
put('rfk2.tssi_sys_b_defs_5g.0.mask', r2.rtw8852b_tssi_sys_b_defs_5g[0][1]);
put('rfk2.dpk_afe_defs.28.addr', r2.rtw8852b_dpk_afe_defs[28][0]);
put('rfk2.ofdm_groups.len', literalAfter('rfk2-tssi.mjs', 'const OFDM_GROUPS =').length);
put('rfk2.kip_reg.2', literalAfter('rfk2-dpkflow.mjs', 'const KIP_REG =')[2]);

const { rtw89_8852b_trk_cfg } = await wzImport('js/rfk2-trkdata.mjs');
const firstTrk = rtw89_8852b_trk_cfg.delta.findIndex(Boolean);
put('trk.first_type', firstTrk);
put('trk.first.0.29', rtw89_8852b_trk_cfg.delta[firstTrk][0][29]);
put('trk.13.0.0', rtw89_8852b_trk_cfg.delta[13]?.[0]?.[0] ?? -999);

const { BITRATES } = await wzImport('js/txrx-ieee.mjs');
put('txrx.bitrates.len', BITRATES.length);
put('txrx.bitrates.last', BITRATES[BITRATES.length - 1]);
put('txrx.physt.11', new Function('VL', `return ${jsText('txrx-rx.mjs').match(/^const PHYST_IE_LEN = (\[[\s\S]*?\]);/m)[1]};`)(R('VAR_LEN'))[11]);

console.log(out.join('\n'));
