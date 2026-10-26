// rfk1-js.mjs: the JS driver's own rfk1.mjs (rfk1-util/-dack/-iqk/-iqkstep/-chan/-tables.mjs) on
// test-rfk1.mjs's chip model (test-rfk1-fake.mjs RfFake), with test-rfk1-stubs.mjs as the
// neighbours (btc, macinit, fw, rfk2), scenario by scenario: test-rfk1-seq.mjs's and
// test-rfk1-fail.mjs's, as text: `## <scenario>`, every op as a bringup-golden.txt line, the
// neighbours' calls as `= call ...` lines where they happened, then results and the whole rfk
// state as `= ...` lines. Plus `## c tables`: the RFK tables and arrays read from the Linux C
// files (test-rfk1-fake.mjs cRfkTable/cArray).
// tests/rfk1_js.rs runs the same on the Rust side (rfk1::fake::RfFake, the same chip model)
// and compares the text line by line.
// The clock: test-rfk1.mjs's simulated time, but counting a queued delay at once (as the Rust
// fake does), so both sides give up a timed-out poll after the same reads.
//   node rfk1-js.mjs            (WLAN_TOOLS: the read-only WLAN tree)
// Reads only; writes nothing but stdout.
import { pathToFileURL } from 'node:url';

const WZ = process.env.WLAN_TOOLS ?? '/home/georg/BitsOS/werkzeuge/plattform/wlan';
const imp = (rel) => import(pathToFileURL(`${WZ}/${rel}`).href);
const { useStubs } = await imp('js/test-rfk1-stubs.mjs');
useStubs();
const { R, prep, field } = await imp('kit/rtwdev.mjs');
const { fresh, sim, cRfkTable, cArray } = await imp('js/test-rfk1-fake.mjs');
const rfk1 = await imp('js/rfk1.mjs');
const { iqkOneShot, lokFinetuneCheck, ID } = await imp('js/rfk1-iqkstep.mjs');
const { rfkState } = await imp('js/rfk1-util.mjs');

let cur = null;
const pending = () => (cur ? cur.q.list.filter((op) => op[0] === 5).reduce((s, op) => s + op[3], 0) : 0);
Date.now = () => Math.floor((sim.us + pending()) / 1000);

const KIND = { 1: 'wr', 2: 'rd', 3: 'rmw', 4: 'poll', 5: 'delay', 6: 'mwr', 7: 'mrd', 8: 'mcpy', 9: 'cfgr', 10: 'cfgw', 11: 'mpoll', 12: 'mark', 13: 'now' };
const HEX = (v) => `0x${(v >>> 0).toString(16).toUpperCase().padStart(8, '0')}`;
const line = ([k, s, t, a, b, c]) =>
  `${KIND[k]}${k <= 4 || k === 6 || k === 7 ? s * 8 : ''} ${HEX(a)} ${HEX(b)} ${HEX(c)}${k === 4 || k === 11 ? ` ${t} ms` : ''}`;
const H = (v) => `0x${(v >>> 0).toString(16)}`;
const B = (v) => (v ? '1' : '0');
const L = (a, f = H) => a.map(f).join(',');
const LL = (a, f = H) => a.map((x) => L(x, f)).join(';');
const out = [];

function arg(x) {
  if (typeof x === 'boolean') return B(x);
  if (typeof x === 'number') return H(x);
  if (typeof x === 'string') return x;
  return `${H(x.cat)} ${H(x.cls)} ${H(x.func)} ${B(x.rack)} ${B(x.dack)} ${Buffer.from(x.payload).toString('hex')}`;
}

// the whole rfk state (rfk2's parts left out)
function stateLines(d) {
  const r = rfkState(d), k = r.dack, q = r.iqk, m = r.mcc;
  return [
    `= tssi ${L(r.isTssiMode, B)}`,
    `= rck ${[0, 1].map((p) => (r.rck?.[p] ? `${H(r.rck[p].rckVal)}/${B(r.rck[p].timeout)}` : '-')).join(',')}`,
    `= dack done ${B(k.dackDone)} cnt ${k.dackCnt} addck ${LL(k.addckD)} bias ${LL(k.biaskD)} dadck ${LL(k.dadckD)} ` +
      `to ${L(k.addckTimeout, B)} ${L(k.msbkTimeout, B)} ${L(k.dadckTimeout, B)}`,
    `= msbk ${k.msbkD.map((x) => LL(x)).join('|')}`,
    `= addc ${k.addc.map((a) => `${a.path}:${a.dcRe}:${a.dcIm}`).join(',')}`,
    `= iqk init ${B(q.isIqkInit)} fail ${q.iqkFailCnt} times ${q.iqkTimes} ver ${H(q.version)} syn ${q.syn1to2} ` +
      `en ${B(q.iqkFftEn)}${B(q.iqkSramEn)}${B(q.iqkCfirEn)}${B(q.iqkXymEn)} nb ${B(q.isNbiqk)}`,
    `= iqk fails ${LL(q.lokCorFail, B)} ${LL(q.lokFinFail, B)} ${LL(q.iqkTxFail, B)} ${LL(q.iqkRxFail, B)} ` +
      `lok ${L(q.lokFail ?? [false, false], B)} restore ${L(q.restoreFail ?? [false, false], B)}`,
    `= iqk chan ${L(q.iqkChannel)} band ${L(q.iqkBand)} ch ${L(q.iqkCh)} bw ${L(q.iqkBw)} mcc ${LL(q.iqkMccCh)} tidx ${L(q.iqkTableIdx)}`,
    `= iqk cfir ${L(q.nbTxcfir)} ${L(q.nbRxcfir)} bp ${L(q.bpTxkresult)} ${L(q.bpRxkresult)} ${L(q.bpIqkenable)} ` +
      `wb ${L(q.isWbTxiqk, B)} ${L(q.isWbRxiqk, B)}`,
    `= iqk lok ${LL(q.lokIdac)} ${LL(q.lokVbuf)}`,
    `= mcc ${L(m.ch)} ${L(m.band)} ${L(m.bw)} ${L(m.rf18)} ${m.tableIdx}`,
    `= lck ${(r.lck ?? []).join(',')}`,
  ];
}

async function scenario(name, d, fn) {
  out.push(`## ${name}`);
  await d.flush();
  cur = d;
  sim.us = 0;
  const t0 = d.trace.length;
  d.calls = [];
  let res = [];
  try {
    res = (await fn()) ?? [];
    await d.flush();
  } catch (e) {
    res = ['! error'];
    if (process.env.RFK1_JS_VERBOSE) console.error(name, e.stack);
  }
  const ops = d.trace.slice(t0).map(line);
  const calls = d.calls.map(([n, pos, ...a]) => [pos - t0, `= call ${n}${a.map((x) => ` ${arg(x)}`).join('')}`]);
  for (let i = 0; i <= ops.length; i++) {
    for (const [pos, c] of calls) if (pos === i) out.push(c);
    if (i < ops.length) out.push(ops[i]);
  }
  out.push(...res, ...stateLines(d));
  cur = null;
}

const rfKey = (p, reg) => `${p}:${R(reg)}`;
const notBit = (reg) => (v) => v & ~R(reg);
function chanFake(o = {}) {
  const d = fresh(o);
  d.rfSet(0, 0x10018, 0x00c01);
  d.rfSet(1, 0x10018, 0x00c01);
  d.rf.set(rfKey(0, 'RR_LDO'), 0x123);
  return d;
}

// ---- the C tables ----
const TABLES = ['afe_init_defs', 'check_addc_defs_a', 'check_addc_defs_b', 'check_dadc_en_defs_a', 'check_dadc_en_defs_b',
  'check_dadc_dis_defs_a', 'check_dadc_dis_defs_b', 'dack_s0_1_defs', 'dack_s0_2_defs', 'dack_s0_3_defs', 'dack_s1_1_defs',
  'dack_s1_2_defs', 'dack_s1_3_defs'];
const ARRAYS = ['_a_idxrxgain', '_a_idxattc2', '_a_idxattc1', '_g_idxrxgain', '_g_idxattc2', '_g_idxattc1', '_a_power_range',
  '_a_track_range', '_a_gain_bb', '_a_itqt', '_g_power_range', '_g_track_range', '_g_gain_bb', '_g_itqt',
  'rtw8852b_backup_bb_regs', 'rtw8852b_backup_rf_regs', 'rtw8852b_set_nondbcc_path01', 'rtw8852b_restore_nondbcc_path01'];
out.push('## c tables');
for (const t of TABLES) for (const e of cRfkTable(`rtw8852b_${t}`)) out.push(`= tbl ${t} ${e[0]} ${e.slice(1).map(H).join(' ')}`);
for (const a of ARRAYS) out.push(`= arr ${a} ${cArray(a).map((e) => (Array.isArray(e) ? e.map(H).join('/') : H(e))).join(',')}`);

// ---- part 1 (test-rfk1-seq.mjs) ----
{
  const d = fresh();
  await scenario('parser', d, async () => {
    rfk1.rfkParser(d, [['WS', 0x1234, 0x10], ['WC', 0x1234, 0x4], ['WM', 0x1238, 0xf0, 0x5], ['WRF', 1, 0x55, 0xff, 0x12], ['DELAY', 7]]);
    const c = rfk1.chanGet(d);
    return [`= chan ${H(c.channel)} ${H(c.bandType)} ${H(c.bandWidth)} ${H(c.subbandType)}`];
  });
}
{
  const d = fresh();
  await scenario('rck', d, () => rfk1.rck(d));
}
{
  let sample = 0;
  const d = fresh({ answers: { [R('R_DBG32_D')]: () => (sample++ % 2 ? 0xfff003 : 0xffe003) } });
  await scenario('dack', d, () => rfk1.dack(d));
}
for (const tssi of [false, true]) {
  const d = fresh();
  rfkState(d).isTssiMode[1] = tssi;
  await scenario(`rx dck tssi ${B(tssi)}`, d, () => rfk1.rxDck(d, 0));
}
for (const [band, ch, lok, mhz] of [['2g', 6, false, 20], ['5g', 36, false, 20], ['2g', 1, true, 20], ['2g', 6, false, 40], ['5g', 42, false, 80]]) {
  const d = fresh(lok ? { rf: { [rfKey(0, 'RR_TXMO')]: 0 } } : {});
  d.chan = { band, ch, bw: mhz };
  await scenario(`iqk ${band} ${ch} ${mhz}${lok ? ' lok' : ''}`, d, () => rfk1.iqk(d, 0));
}
for (const [band, ch, bw] of [['2g', 1, 20], ['2g', 14, 20], ['5g', 36, 20], ['2g', 6, 40], ['2g', 6, 80]]) {
  const d = chanFake();
  await scenario(`channel ${band} ${ch} ${bw}`, d, async () => {
    await rfk1.setChannelRf(d, { band, ch, bw });
    return [`= rf18 ${H(d.peek(0, R('RR_CFGCH')))} ${H(d.peek(1, R('RR_CFGCH')))} ${H(d.peek(0, 0x10018))} ${H(d.peek(1, 0x10018))}`];
  });
}
{
  const d = fresh();
  await scenario('mcc slot', d, () => {
    d.chan = { band: '2g', ch: 6, bw: 20 };
    rfk1.mccGetChInfo(d, 0);
    d.chan = { band: '5g', ch: 36, bw: 20 };
    rfk1.mccGetChInfo(d, 0);
  });
}
{
  const d = fresh();
  d.chan = { band: '2g', ch: 11, bw: 20 };
  await scenario('rfk channel', d, () => rfk1.rfkChannel(d));
}
{
  const d = fresh();
  Object.assign(rfkState(d).mcc, { ch: [6, 36, 0], tableIdx: 1 });
  await scenario('mccch slot 1', d, () => rfk1.h2cRfNtfyMcc(d));
}
{
  const d = fresh();
  d.chan = { band: '2g', ch: 6, bw: 20 };
  rfk1.mccGetChInfo(d, 0);
  rfkState(d).isTssiMode = [true, true];
  await scenario('rfk init', d, () => rfk1.rfkInit(d));
}
{
  const d = fresh();
  await scenario('rfk2 entries', d, async () => {
    await rfk1.rfkBandChanged(d);
    await rfk1.rfkScan(d, true);
    await rfk1.rfkScan(d, false);
    await rfk1.rfkTrack(d);
  });
}

// ---- part 2 (test-rfk1-fail.mjs) ----
{
  const d = fresh({ slow: { [rfKey(0, 'RR_RCKS')]: notBit('RR_RCKS'), [rfKey(1, 'RR_RCKS')]: notBit('RR_RCKS') } });
  await scenario('slow rck', d, () => rfk1.rck(d));
}
{
  const ok = (reg, bit) => [R(reg), notBit(bit)];
  const d = fresh({ slow: Object.fromEntries([ok('R_DRCK_RS', 'B_DRCK_RS_DONE'), [R('R_ADDCKR0'), (v) => v & ~1],
    [R('R_ADDCKR1'), (v) => v & ~1], ok('R_DACK_S0P0', 'B_DACK_S0P0_OK'), ok('R_DACK_S0P2', 'B_DACK_S0P2_OK'),
    ok('R_DACK_S1P0', 'B_DACK_S1P0_OK'), ok('R_DACK10S', 'B_DACK_S1P2_OK')]) });
  await scenario('slow dack', d, () => rfk1.dack(d));
}
{
  const d = fresh({ slow: { [rfKey(0, 'RR_MOD')]: (v) => (v & ~R('RR_MOD_MASK')) | prep(R('RR_MOD_MASK'), 2) } });
  await scenario('slow rx dck', d, () => rfk1.rxDck(d, 0));
}
{
  const d = fresh({ slow: { 0xbff8: () => 0 } });
  d.chan = { band: '2g', ch: 6, bw: 20 };
  await scenario('slow iqk', d, () => rfk1.iqk(d, 0));
}
{
  const d = chanFake({ slow: { [rfKey(0, 'RR_LPF')]: (v) => v | R('RR_LPF_BUSY') } });
  await scenario('slow channel', d, () => rfk1.setChannelRf(d, { band: '2g', ch: 1, bw: 20 }));
}
{
  const d = fresh({ rf: { [rfKey(0, 'RR_RCKS')]: 0, [rfKey(1, 'RR_RCKS')]: 0 } });
  await scenario('rck timeout', d, () => rfk1.rck(d));
}
{
  const d = fresh({ answers: { [R('R_DRCK_RS')]: prep(R('B_DRCK_RS_LPS'), 0x15),
    [R('R_ADDCKR0')]: prep(R('B_ADDCKR0_A0'), 0x123) | prep(R('B_ADDCKR0_A1'), 0x2a4),
    [R('R_DACK_S0P0')]: 0, [R('R_DACK_S1P0')]: 0, [R('R_DACK_S1P1')]: 0,
    [R('R_DACK10S')]: (dv) => prep(R('B_DACK10S'), 0x60 + field(dv.bb(R('R_DACK10')), R('B_DACK10'))),
    [R('R_DACK11S')]: (dv) => prep(R('B_DACK11S'), 0x70 + field(dv.bb(R('R_DACK11')), R('B_DACK11'))) } });
  await scenario('dack timeouts', d, () => rfk1.dack(d));
}
{
  const d = fresh({ rf: { [rfKey(0, 'RR_MOD')]: prep(R('RR_MOD_MASK'), 2) } });
  await scenario('rx mode timeout', d, () => rfk1.rxDck(d, 0));
}
{
  const d = fresh({ answers: { 0xbff8: 0 } });
  d.chan = { band: '2g', ch: 1, bw: 20 };
  rfkState(d);
  await scenario('one-shot timeout', d, async () => [`= fail ${B(await iqkOneShot(d, 0, 0, ID.TXK))}`]);
}
{
  const d = chanFake({ rf: { [rfKey(0, 'RR_LPF')]: R('RR_LPF_BUSY') } });
  await scenario('lpf timeout', d, () => rfk1.setChannelRf(d, { band: '2g', ch: 1, bw: 20 }));
}
const FAIL_RUNS = [
  { cor: [true, false], fin: [false, true], tx: [[2], []], rx: [[], [0, 3]], restore: [false, true] },
  { cor: [false, false], fin: [false, false], tx: [[], []], rx: [[], [1]], restore: [false, false] },
];
for (const [i, fails] of FAIL_RUNS.entries()) {
  const rpt = (dv) => {
    const cmd = dv.bb(R('R_NCTL_CFG')) - 1, p = (cmd >> 5) & 1, kind = (cmd >> 8) & 0xf;
    const lut = dv.bb(R('R_CFIR_LUT') + (p << 8));
    const hit = (kind === 1 && fails.cor[p]) || (kind === 2 && fails.fin[p]) ||
      (kind === 8 && fails.tx[p].includes(lut & R('B_CFIR_LUT_GP'))) || (kind === 0xb && fails.rx[p].includes(lut & R('B_CFIR_LUT_GP_V1'))) ||
      (kind === 0xe && fails.restore[p]);
    return hit ? R('B_NCTL_RPT_FLG') : 0;
  };
  const d = fresh({ answers: { [R('R_NCTL_RPT')]: rpt } });
  d.chan = { band: '2g', ch: 6, bw: 20 };
  await scenario(`iqk fails ${i}`, d, () => rfk1.iqk(d, 0));
}
for (const [ci, cq, vi, vq] of [[0x10, 0x10, 0x20, 0x20], [0x2, 0x2, 0x2, 0x2], [0x1d, 0x1d, 0x3d, 0x3d],
  [0x1, 0x10, 0x20, 0x20], [0x10, 0x1, 0x20, 0x20], [0x1e, 0x10, 0x20, 0x20], [0x10, 0x1e, 0x20, 0x20],
  [0x10, 0x10, 0x1, 0x20], [0x10, 0x10, 0x20, 0x1], [0x10, 0x10, 0x3e, 0x20], [0x10, 0x10, 0x20, 0x3e]]) {
  const txmo = prep(R('RR_TXMO_COI'), ci) | prep(R('RR_TXMO_COQ'), cq), lokvb = prep(R('RR_LOKVB_COI'), vi) | prep(R('RR_LOKVB_COQ'), vq);
  const d = fresh({ rf: { [rfKey(1, 'RR_TXMO')]: txmo, [rfKey(1, 'RR_LOKVB')]: lokvb } });
  await scenario(`lok check ${L([ci, cq, vi, vq])}`, d, async () => [`= fail ${B(await lokFinetuneCheck(d, 1))}`]);
}
for (const after of [1, 2, 3]) {
  const d = chanFake({ rf: { [rfKey(0, 'RR_POW')]: 0x3456c, [rfKey(0, 'RR_SX')]: 0x789ab },
    rfAns: { [rfKey(0, 'RR_SYNFB')]: (dv, k) => (k < after ? 0 : R('RR_SYNFB_LK')) } });
  await scenario(`lo lock after ${after}`, d, () => rfk1.setChannelRf(d, { band: '2g', ch: 11, bw: 20 }));
}
{
  const d = fresh();
  d.chan = { band: '2g', ch: 6, bw: 20 };
  await scenario('iqk again', d, async () => {
    await rfk1.iqk(d, 0);
    await rfk1.iqk(d, 0);
    d.rfk.iqk.iqkTimes = 0xfe;
    await rfk1.iqk(d, 0);
  });
}

process.stdout.write(out.join('\n') + '\n');
