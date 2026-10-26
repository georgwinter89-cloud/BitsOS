// rfk2-js.mjs: the JS driver's own rfk2.mjs (rfk2-common/-dpk/-dpkflow/-tssi/-alimk/-trk/
// -tables/-trkdata.mjs, with phy-bbx.mjs and chan-path.mjs's RX path) on test-rfk2.mjs's chip
// model (rfk2-tchip.mjs makeChip: the real efuse dump and firmware file, RF registers behind SWSI,
// KIP and TSSI reports), with test-rfk2.mjs's stand-ins of btc and macinit (here as
// stubs/rfk2-*.mjs, which also note where each call happened). Scenario by scenario, the JS side
// of test-rfk2.mjs (which checks the same module against the Linux C itself), as text:
// `## <scenario>`, every op as a bringup-golden.txt line, the neighbours' calls as `= call ...`
// lines where they happened, then results and the whole rfk2 state as `= ...` lines. `## inputs`
// prints the efuse values and the TXPWR_TRK tables (the firmware's element and Linux's own).
// tests/rfk2_js.rs runs the same on the Rust side (rfk1::fake::RfFake set up as makeChip) and
// compares the text line by line.
// The clock: the delays of the trace so far, counting a queued delay at once (as the Rust fake).
//   node rfk2-js.mjs            (WLAN_TOOLS: the read-only WLAN tree)
// Reads only; writes nothing but stdout.
import { pathToFileURL } from 'node:url';

const WZ = process.env.WLAN_TOOLS ?? '/home/georg/BitsOS/werkzeuge/plattform/wlan';
const imp = (rel) => import(pathToFileURL(`${WZ}/${rel}`).href);
const { useStubs } = await imp('js/stubs/hook.mjs');
useStubs(/\/(rfk1-util|rfk2(-[a-z0-9]+)?)\.mjs$/, {
  './macinit.mjs': new URL('stubs/rfk2-macinit.mjs', import.meta.url).href,
  './btc.mjs': new URL('stubs/rfk2-btc.mjs', import.meta.url).href,
});
const { R, BB, prep, field, FakeDev } = await imp('kit/rtwdev.mjs');
const { rfW, RFREG_MASK } = await imp('kit/phyio.mjs');
const rfk2 = await imp('js/rfk2.mjs');
const DP = await imp('js/rfk2-dpk.mjs');
const { dpkRun } = await imp('js/rfk2-dpkflow.mjs');
const TS = await imp('js/rfk2-tssi.mjs');
const PB = await imp('js/phy-bbx.mjs');
const { ctrlRxPath } = await imp('js/chan-path.mjs');
const { chanGet, rfkState } = await imp('js/rfk1-util.mjs');
const { pollSoft } = await imp('js/rfk2-common.mjs');
const { rtw89BuildTxpwrTrkTblFromElm } = await imp('js/rfk2-trk.mjs');
const { rtw89_8852b_trk_cfg } = await imp('js/rfk2-trkdata.mjs');
const { efTssi, efGain, fwElements, makeChip } = await imp('js/rfk2-tchip.mjs');

let cur = null, t0 = 0;
const sumDelays = (ops) => ops.filter((op) => op[0] === 5).reduce((s, op) => s + op[3], 0);
Date.now = () => (cur ? Math.floor((sumDelays(cur.trace.slice(t0)) + sumDelays(cur.q.list)) / 1000) : 0);

const KIND = { 1: 'wr', 2: 'rd', 3: 'rmw', 4: 'poll', 5: 'delay', 6: 'mwr', 7: 'mrd', 8: 'mcpy', 9: 'cfgr', 10: 'cfgw', 11: 'mpoll', 12: 'mark', 13: 'now' };
const HEX = (v) => `0x${(v >>> 0).toString(16).toUpperCase().padStart(8, '0')}`;
const line = ([k, s, t, a, b, c]) =>
  `${KIND[k]}${k <= 4 || k === 6 || k === 7 ? s * 8 : ''} ${HEX(a)} ${HEX(b)} ${HEX(c)}${k === 4 || k === 11 ? ` ${t} ms` : ''}`;
const H = (v) => `0x${(v >>> 0).toString(16)}`;
const D = (v) => `${v}`;
const B = (v) => (v ? '1' : '0');
const L = (a, f = H) => [...a].map(f).join(',');
const LL = (a, f = H) => a.map((x) => L(x, f)).join(';');
const out = [];

const arg = (x) => (typeof x === 'string' ? x : H(x));

// the whole rfk2 state (rfk1's parts left out)
function stateLines(d) {
  const r = rfkState(d), k = r.dpk, t = r.tssi;
  const bych = [];
  for (let p = 0; p < 2; p++) {
    t.checkBackupAligmk[p].forEach((c, i) => {
      const v = t.alignmentBackupByCh[p][i].slice(0, 4);
      if (c || v.some(Boolean)) bych.push(`${p}:${i}:${B(c)}:${L(v)}`);
    });
  }
  return [
    `= tssi mode ${L(r.isTssiMode, B)} timeouts ${r.pollTimeouts}`,
    `= dpk en ${B(k.isDpkEnable)} reload ${B(k.isDpkReloadEn)} gs ${L(k.dpkGs)} cur ${L(k.curIdx)} ` +
      `corr ${LL(k.corrVal)} idx ${LL(k.corrIdx)} dc ${LL(k.dcI)} ${LL(k.dcQ)}`,
    `= dpk bp ${k.bp.map((p) => p.map((b) => [b.band, b.bw, b.ch, +b.pathOk, b.txagcDpk, b.therDpk, b.gs, b.pwsf].map(H).join('/')).join(',')).join(';')}`,
    `= tssi done ${t.alignmentDone.slice(0, 2).map((x) => L(x, B)).join(';')}`,
    `= tssi value ${t.alignmentValue.slice(0, 2).map((x) => x.map((v) => L(v.slice(0, 4))).join('|')).join(';')}`,
    `= tssi bych ${bych.join(' ')}`,
  ];
}

async function scenario(name, d, fn) {
  out.push(`## ${name}`);
  await d.flush();
  cur = d;
  t0 = d.trace.length;
  d.calls = [];
  let res = [];
  try {
    res = (await fn()) ?? [];
    await d.flush();
  } catch (e) {
    res = ['! error'];
    d.q = new d.q.constructor();
    if (process.env.RFK2_JS_VERBOSE) console.error(name, e.stack);
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

// a register value as the chip may hold it (no trace op)
function poke(d, a, m, v) {
  d._set(d.regs, BB + a, 4, ((d._get(d.regs, BB + a, 4) & ~m) | prep(m, v)) >>> 0);
}

// ---- inputs: the efuse values and the TXPWR_TRK tables ----
out.push('## inputs');
out.push(`= efuse thermal ${L(efTssi.thermal)} cck ${LL(efTssi.tssiCck, D)} mcs ${LL(efTssi.tssiMcs, D)} trim ${LL(efTssi.tssiTrim, D)}`);
out.push(`= gain ${B(efGain.offsetValid)} ${B(efGain.compValid)} offset ${LL(efGain.offset, D)} comp ${LL(efGain.comp, D)} ` +
  `base ${L(efGain.offsetBase, D)} rssi ${L(efGain.rssiBase, D)}`);
const trkElm = fwElements.find((e) => e.id === R('RTW89_FW_ELEMENT_ID_TXPWR_TRK'));
out.push(`= trk elm ${trkElm.size} ${trkElm.aid}`);
const fromElm = rtw89BuildTxpwrTrkTblFromElm(trkElm);
for (let t = 0; t < 16; t++) out.push(`= trk ${t} ${(fromElm.delta[t] ?? []).map((r) => L(r, D)).join(';')}`);
for (let t = 0; t < 16; t++) out.push(`= trkcfg ${t} ${(rtw89_8852b_trk_cfg.delta[t] ?? []).map((r) => L(r, D)).join(';')}`);
out.push(`= trk other aid ${rtw89BuildTxpwrTrkTblFromElm({ ...trkElm, aid: 0x1234 }, 0) === null ? 'ignored' : 'taken'}`);
{
  let r = 'taken';
  try { rtw89BuildTxpwrTrkTblFromElm({ ...trkElm, payload: Buffer.alloc(8 + 600) }); } catch { r = 'refused'; }
  out.push(`= trk no bitmap ${r}`);
}

// ---- single functions (test-rfk2.mjs sections 3 and 4) ----
await scenario('one-shot', makeChip(), () => DP.dpkOneShot(cur, 0, 1, 0x10));
{
  const v = [];
  for (let g = 0; g < 0x1000; g++) v.push(DP.dpkDgainMapping(g));
  await scenario('dgain mapping', makeChip(), () => [`= dgain ${v.join(',')}`]);
}
const tssiMcs = [Array.from({ length: 19 }, (_, i) => i * 5 - 40), Array.from({ length: 19 }, (_, i) => 37 - i * 4)];
const tssiCck = [[1, 2, 3, 4, 5, 6], [-1, -2, -3, -4, -5, -6]];
const tssiTrim = [[1, -2, 3, -4, 5, -6, 7, -8], [-9, 10, -11, 12, -13, 14, -15, 16]];
{
  const d = makeChip({ efuse: { tssi: { ...efTssi, tssiMcs, tssiCck, tssiTrim }, efuseGain: efGain } });
  await scenario('efuse to de', d, () => {
    for (const [band, lo, hi] of [['2g', 1, 14], ['5g', 36, 177]]) {
      for (let ch = lo; ch <= hi; ch++) {
        d.chan = { band, ch, bw: 20 };
        TS.tssiSetEfuseToDe(d, 0, chanGet(d));
      }
    }
  });
}
for (const thermal of [efTssi.thermal, [0xff, 0xff]]) {
  const d = makeChip({ efuse: { tssi: { ...efTssi, thermal }, efuseGain: efGain } });
  await scenario(`tssi setup thermal ${L(thermal)}`, d, () => {
    for (const [band, ch] of [['2g', 1], ['2g', 14], ['5g', 36], ['5g', 64], ['5g', 100], ['5g', 144], ['5g', 149], ['5g', 177]]) {
      d.chan = { band, ch, bw: 20 };
      for (const path of [0, 1]) {
        TS.tssiRfSetting(d, 0, path, chanGet(d));
        TS.tssiSetSys(d, 0, path, chanGet(d));
        TS.tssiSetTmeterTbl(d, 0, path, chanGet(d));
        for (const all of [true, false]) TS.tssiAlignmentDefault(d, 0, path, all, chanGet(d));
      }
    }
  });
}
{
  const gain = { offsetValid: true, offset: [[1, -2, 3, -4, 5], [-3, 4, -5, 6, -7]], compValid: true,
    comp: [[1, -2, 0, 3, -4], [2, 0, -1, 1, 5]], offsetBase: [-12, 0], rssiBase: [16, 0] };
  const d = makeChip({ efuse: { tssi: efTssi, efuseGain: gain } });
  await scenario('dpk steps and bb helpers', d, async () => {
    for (const gs of [0x7f, 0x5b]) {
      rfkState(d).dpk.dpkGs[0] = gs;
      for (const [path, kidx, g] of [[0, 0, 1], [1, 1, 1], [1, 0, 0]]) await DP.dpkFillResult(d, 0, path, kidx, g, 0x32);
    }
    for (const [band, ch, bw] of [['2g', 6, 20], ['5g', 36, 40], ['5g', 42, 80]]) {
      d.chan = { band, ch, bw };
      for (const path of [0, 1]) {
        Object.assign(rfkState(d).dpk.bp[path][1], { band: chanGet(d).bandType, bw: chanGet(d).bandWidth });
        DP.dpkRfSetting(d, 1, path, 1);
        DP.dpkKipPwrClkOn(d, path);
        DP.dpkKipRestore(d, path);
      }
      DP.dpkBbAfeSetting(d, chanGet(d));
      DP.dpkBbAfeRestore(d, chanGet(d));
      for (const enable of [1, 0]) PB.bbSetPmacPktTx(d, enable, 100, 5000, 20, 0, chanGet(d));
      for (const antennaRx of [0, R('RF_B')]) {
        d.st.hal.antennaRx = antennaRx;
        for (const rx of ['RF_A', 'RF_B', 'RF_AB', 'RF_ABCD']) ctrlRxPath(d, R(rx), chanGet(d));
      }
    }
    for (const p of ['RF_PATH_A', 'RF_PATH_B', 'RF_PATH_AB', 'RF_PATH_ABCD']) PB.bbCfgTxPath(d, R(p));
    for (const mode of [0, 1]) PB.bbTxModeSwitch(d, 0, mode);
    PB.bbSetPower(d, 48, 0);
    for (const txPath of [R('RF_AB'), R('RF_A')]) {
      PB.bbRestoreTssi(d, 0, { txPath, rxPath: 2, p0Rfmode: 0x1234, p0RfmodeFtm: 0x5678, p1Rfmode: 0x9abc, p1RfmodeFtm: 0xdef0, txPwr: -3 });
    }
    TS.tssiEnable(d, 0);
    TS.tssiDisable(d, 0);
  });
}

// ---- whole calibrations, step by step (test-rfk2.mjs section 5) ----
const moveTo = (d, chan) => { d.chan = { ...chan }; };
for (const [chan, next] of [[{ band: '2g', ch: 1, bw: 20 }, { band: '5g', ch: 36, bw: 20 }],
  [{ band: '5g', ch: 42, bw: 80 }, { band: '5g', ch: 151, bw: 40 }], [{ band: '2g', ch: 13, bw: 40 }, { band: '5g', ch: 100, bw: 20 }]]) {
  const d = makeChip({ chan, txagcBb: 0x05fe0021 });
  const tag = `${chan.band} ch ${chan.ch} bw ${chan.bw}`, ntag = `${next.band} ch ${next.ch}`;
  await scenario(`dpkInit ${tag}`, d, () => rfk2.dpkInit(d));
  await scenario(`tssi ${tag}`, d, () => rfk2.tssi(d, 0, true));
  await scenario(`dpk ${tag}`, d, () => rfk2.dpk(d, 0));
  for (const [th, txdpd, comOf, dis] of [[[0x1a, 0x1a], 0, 1, 0], [[0x24, 0x10], 1, 1, 0], [[0x24, 0x10], 1, 0, 0],
    [[0x10, 0x24], 0, 0, 0], [[0, 0], 0, 0, 0], [[0x30, 0x08], 0, 1, 1]]) {
    d.phy = { avgThermal: [...th] };
    for (const path of [0, 1]) {
      poke(d, R('R_P0_TXDPD') + (path << 13), R('B_P0_TXDPD'), txdpd);
      poke(d, R('R_DPD_COM') + (path << 8), R('B_DPD_COM_OF'), comOf);
    }
    poke(d, R('R_DPK_TRK'), R('B_DPK_TRK_DIS'), dis);
    await scenario(`dpkTrack ${tag} thermal ${L(th)} txdpd ${txdpd} com_of ${comOf} dis ${dis}`, d, () => rfk2.dpkTrack(d));
  }
  await scenario(`scan start ${tag}`, d, () => rfk2.wifiScanNotify(d, true, 0));
  moveTo(d, next);
  await scenario(`tssiScan to ${ntag}`, d, () => rfk2.tssiScan(d, 0));
  moveTo(d, chan);
  await scenario(`tssiScan back to ${tag}`, d, () => rfk2.tssiScan(d, 0));
  await scenario(`scan end ${tag}`, d, () => rfk2.wifiScanNotify(d, false, 0));
  moveTo(d, next);
  await scenario(`tssi on ${ntag}`, d, () => rfk2.tssi(d, 0, true));
  await scenario(`dpk on ${ntag}`, d, () => rfk2.dpk(d, 0));
  await scenario(`track on ${ntag}`, d, () => rfk2.dpkTrack(d));
  moveTo(d, chan);
  await scenario(`tssi again on ${tag}`, d, () => rfk2.tssi(d, 0, true));
  await scenario(`dpk again on ${tag}`, d, () => rfk2.dpk(d, 0));
}

// ---- the paths Linux only logs (test-rfk2.mjs section 5, second part) ----
{
  const d = makeChip({ cwReady: false });
  await scenario('tssi without a TSSI report', d, () => rfk2.tssi(d, 0, true));
}
{
  const d = makeChip({ failPoll: 0xbff8 });
  await scenario('dpkInit (one-shots fail)', d, () => rfk2.dpkInit(d));
  await scenario('dpk with one-shot timeouts', d, () => rfk2.dpk(d, 0));
}
{
  const d = makeChip({ chan: { band: '5g', ch: 36, bw: 20 } });
  await scenario('tssi without hw tx', d, () => rfk2.tssi(d, 0, false));
}
for (const [corr, dgain] of [[150, 0x600], [200, 0x7d0], [200, 0x100], [180, 0xbf0]]) {
  const d = makeChip({ corr, dgain });
  await scenario(`dpk with corr ${corr} dgain ${H(dgain)}`, d, () => rfk2.dpk(d, 0));
}
{
  const d = makeChip({ chan: { band: '5g', ch: 149, bw: 20 } });
  await scenario('dpkInit, no TSSI', d, () => rfk2.dpkInit(d));
  await scenario('dpk without TSSI', d, () => rfk2.dpk(d, 0));
  for (const th of [[0x30, 0x08], [0x08, 0x30]]) {
    d.phy = { avgThermal: [...th] };
    await scenario(`dpkTrack without TSSI, thermal ${L(th)}`, d, () => rfk2.dpkTrack(d));
  }
}
{
  const d = makeChip();
  await scenario('scan start with TSSI off (runs tssi)', d, () => rfk2.wifiScanNotify(d, true, 0));
}

// the TX AGC search at its ends: the gain loss report always high (down to 0x2e) or always 0
// with the PAS check failing (up to 0x3f)
for (const [name, glHigh] of [['dpk with gain loss high', true], ['dpk with gain loss low', false]]) {
  const d = makeChip();
  const orig = d.onRead;
  const reg = (a) => d._get(d.regs, BB + a, 4);
  d.onRead = (kind, a, s) => {
    const v = orig(kind, a, s);
    if (kind !== 'mmio' || a !== BB + R('R_RPT_COM') || field(reg(R('R_KIP_RPT1')), R('B_KIP_RPT1_SEL')) !== 6) return v;
    if (field(reg(R('R_DPK_CFG2')), R('B_DPK_CFG2_ST'))) return glHigh ? 0x70 : 0x00;
    return field(reg(R('R_DPK_CFG3')), R('MASKBYTE3')) ? 0x01000100 : 0x00400040;
  };
  await scenario(name, d, () => rfk2.dpk(d, 0));
}

// the boundaries of the AGC search and the tracking (not in test-rfk2.mjs): the digital gain
// at the RXBB step and window edges on 80 MHz (the loopback RX IQK and its attenuation), the
// sync correlation at its threshold, the PAS check at equality, tracking before a DPK and with
// the TX AGC RF field 0, the scan start with TSSI on one path only, extreme gain offsets
for (const dgain of [0x555, 0x556, 0x4c1, 0x262, 0x220, 342, 341, 1922, 1923]) {
  const d = makeChip({ chan: { band: '5g', ch: 42, bw: 80 }, dgain });
  await scenario(`dpk 80 MHz dgain ${H(dgain)}`, d, () => rfk2.dpk(d, 0));
}
{
  const d = makeChip({ corr: 170 });
  await scenario('dpk with corr 170', d, () => rfk2.dpk(d, 0));
}
{
  const d = makeChip();
  const orig = d.onRead;
  const reg = (a) => d._get(d.regs, BB + a, 4);
  d.onRead = (kind, a, s) => {
    const v = orig(kind, a, s);
    if (kind !== 'mmio' || a !== BB + R('R_RPT_COM') || field(reg(R('R_KIP_RPT1')), R('B_KIP_RPT1_SEL')) !== 6) return v;
    if (field(reg(R('R_DPK_CFG2')), R('B_DPK_CFG2_ST'))) return 0x00;
    return field(reg(R('R_DPK_CFG3')), R('MASKBYTE3')) ? 0x00050000 : 0x00060002;   // 6^2 + 2^2 == 5^2 * 8 / 5
  };
  await scenario('dpk with the PAS check at equality', d, () => rfk2.dpk(d, 0));
}
{
  const d = makeChip();
  d.phy = { avgThermal: [0x1a, 0x1a] };
  await scenario('dpkTrack before any DPK', d, () => rfk2.dpkTrack(d));
}
{
  const d = makeChip({ txagcBb: 0x05fe0040 });
  await scenario('dpkInit, TX AGC RF 0', d, () => rfk2.dpkInit(d));
  await scenario('tssi, TX AGC RF 0', d, () => rfk2.tssi(d, 0, true));
  await scenario('dpk, TX AGC RF 0', d, () => rfk2.dpk(d, 0));
  d.phy = { avgThermal: [0x24, 0x10] };
  await scenario('dpkTrack, TX AGC RF 0', d, () => rfk2.dpkTrack(d));
  const orig = d.onRead;                                 // path B's TX AGC RF field back on
  d.onRead = (kind, a, s) => (kind === 'mmio' && a === BB + R('R_TXAGC_BB') + 0x2000 ? 0x05fe0021 : orig(kind, a, s));
  d.phy = { avgThermal: [0x24, 0] };                    // path B uses path A's ini_diff
  await scenario('dpkTrack, TX AGC RF 0 on path A only', d, () => rfk2.dpkTrack(d));
}
{
  const d = makeChip();
  rfkState(d).isTssiMode = [false, true];
  await scenario('scan start with TSSI on path B only', d, () => rfk2.wifiScanNotify(d, true, 0));
}
{
  const gain = { offsetValid: true, offset: [[-8, 7, -8, 7, -8], [7, -8, 7, -8, 7]], compValid: true,
    comp: [[7, -8, 7, -8, 7], [-8, 7, -8, 7, -8]], offsetBase: [20, 0], rssiBase: [-20, 0] };
  const d = makeChip({ efuse: { tssi: efTssi, efuseGain: gain } });
  await scenario('rx path with extreme gain offsets', d, () => {
    for (const [band, ch] of [['2g', 6], ['5g', 36], ['5g', 100], ['5g', 149]]) {
      d.chan = { band, ch, bw: 20 };
      for (const antennaRx of [0, R('RF_B')]) {
        d.st.hal.antennaRx = antennaRx;
        for (const rx of ['RF_A', 'RF_B', 'RF_AB', 'RF_ABCD']) ctrlRxPath(d, R(rx), chanGet(d));
      }
    }
  });
}

// ---- the good chip run (test-rfk2.mjs section 6) ----
{
  const d = makeChip();
  await scenario('good run dpkInit', d, () => rfk2.dpkInit(d));
  await scenario('good run tssi', d, async () => {
    await rfk2.tssi(d, 0, true);
    return [`= txen at pmac ${L(d.model.log.txenAtPmac)}`];
  });
  await scenario('good run dpk', d, () => rfk2.dpk(d, 0));
  d.phy = { avgThermal: [0x1a, 0x1a] };
  await scenario('good run track', d, () => rfk2.dpkTrack(d));
}

// ---- more of what only a chip shows: RF mode stuck in TX, an external PA, the reload branch ----
{
  const d = makeChip();
  d.model.rf[0].set(R('RR_MOD'), prep(R('RR_MOD_MASK'), 2));
  await scenario('dpk with RF mode stuck in TX', d, () => rfk2.dpk(d, 0));
}
{
  const d = makeChip();
  d.st.fem = { epa2g: true };
  await scenario('dpk with an external 2 GHz PA', d, () => rfk2.dpk(d, 0));
}
{
  const d = makeChip();
  await scenario('dpk before reload', d, () => rfk2.dpk(d, 0));
  rfkState(d).dpk.isDpkReloadEn = true;
  await scenario('dpk reload same channel', d, () => dpkRun(d, 0, false, chanGet(d)));
  d.chan = { band: '2g', ch: 6, bw: 20 };
  await scenario('dpk reload other channel', d, () => dpkRun(d, 0, false, chanGet(d)));
}

// ---- errors (test-rfk2.mjs section 7) ----
{
  const d = makeChip();
  d.chan = { band: '6g', ch: 1 };
  await scenario('dpk on 6 GHz', d, async () => {
    d.w32(0x1234, 1);                                    // the caller's op is not rfk2's to drop
    await rfk2.dpk(d, 0);
  });
}
{
  const p = new FakeDev({ onPoll: (kind, a) => a !== BB + 0xbff8 && a !== BB + R('R_SWSI_V1') });
  await scenario('soft poll timeout', p, async () => [`= soft ${B(await pollSoft(p, 0xbff8, 0xff, 0x55, 20000))}`]);
  await scenario('hard poll before a soft one', p, async () => {
    rfW(p, 0, R('RR_MOD'), RFREG_MASK, 0x50220);
    await pollSoft(p, 0x80fc, 0xffff, 0x8000, 2000);
  });
}

process.stdout.write(out.join('\n') + '\n');
