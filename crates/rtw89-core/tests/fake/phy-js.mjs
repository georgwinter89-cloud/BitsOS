// phy-js.mjs: the JS driver's own phy.mjs (phy-efuse/-tbl/-bbx/-dm/-trk/-env/-dig.mjs, with
// the real fw.mjs and pci.mjs) on test-phy.mjs's chip model (phy-test-expect.mjs ChipDev),
// scenario by scenario (test-phy.mjs's, plus the tracking parts test-track.mjs drives), as
// text: `## <scenario>`, then every op as a bringup-golden.txt line, `= ...` lines for
// results and state, `! error <kind>` where one failed. Plus `## spec ...`: the register
// sequences of the C as phy-test-expect.mjs transcribes them (cResetBbRf, cDmInit, the BB
// helpers, the tables read from rtw8852b_common.c).
// tests/phy_js.rs runs the same scenarios on the Rust side (ChipBus, the same chip model) and
// compares the text line by line.
//   node phy-js.mjs            (WLAN_TOOLS: the read-only WLAN tree)
// Reads only; writes nothing but stdout.
import { readFileSync } from 'node:fs';
import { pathToFileURL } from 'node:url';

const WZ = process.env.WLAN_TOOLS ?? '/home/georg/BitsOS/werkzeuge/plattform/wlan';
const imp = (rel) => import(pathToFileURL(`${WZ}/${rel}`).href);
const { FakeDev, R, BB, prep, field, RtwError } = await imp('kit/rtwdev.mjs');
const { phyMask32, phyR32Mask } = await imp('kit/phyio.mjs');
const fw = await imp('js/fw.mjs');
const { pciAlloc } = await imp('js/pci.mjs');
const phy = await imp('js/phy.mjs');
const pe = await imp('js/phy-efuse.mjs');
const pt = await imp('js/phy-tbl.mjs');
const X = await imp('js/phy-test-expect.mjs');

const KIND = { 1: 'wr', 2: 'rd', 3: 'rmw', 4: 'poll', 5: 'delay', 6: 'mwr', 7: 'mrd', 8: 'mcpy', 9: 'cfgr', 10: 'cfgw', 11: 'mpoll', 12: 'mark', 13: 'now' };
const HEX = (v) => `0x${(v >>> 0).toString(16).toUpperCase().padStart(8, '0')}`;
const hx = (v) => `0x${(v >>> 0).toString(16).padStart(8, '0')}`;
const line = ([k, s, t, a, b, c]) =>
  `${KIND[k]}${k <= 4 || k === 6 || k === 7 ? s * 8 : ''} ${HEX(a)} ${HEX(b)} ${HEX(c)}${k === 4 || k === 11 ? ` ${t} ms` : ''}`;
const out = [];
const kit = (f) => readFileSync(`${WZ}/kit/${f}`);
const FWP = fw.fwParse(readFileSync(`${WZ}/firmware/rtw8852b_fw-2.bin`), { cv: 1 });
const list = (a) => [...a].join(',');
const b01 = (v) => (v ? 1 : 0);

// the kind of an error, as tests/phy_js.rs names the Rust one
function kind(e) {
  if (e instanceof fw.FwError) return 'fw';
  if (e instanceof RtwError) return 'device';
  return 'other';
}
// phy's own log lines (the messages of Linux's warnings)
const PHY_LOG = /^(failed to check efuse autoload|efuse dump \(dav=0\)|invalid PHY package|failed to load CR|RF parameters exceed|rf path \d+ reg h2c config failed|failed to poll AFE|failed to poll nctl|mac_id \d+ is out of range)/;
function logs(d) {
  const l = (d.logs ?? []).filter((m) => PHY_LOG.test(m)).map((m) => `= log ${m}`);
  if (d.logs) d.logs.length = 0;
  return l;
}

async function scenario(name, d, fn) {
  out.push(`## ${name}`);
  await d.flush();
  const t0 = d.trace.length;
  let res = [];
  try {
    res = (await fn()) ?? [];
    await d.flush();
  } catch (e) {
    res = [`! error ${kind(e)}`];
    if (process.env.PHY_JS_VERBOSE) console.error(name, e.stack);
  }
  for (const op of d.trace.slice(t0)) out.push(line(op));
  out.push(...res, ...logs(d));
}
async function spec(name, model, fn) {
  out.push(`## spec ${name}`);
  for (const op of await X.expectOps(model, fn)) out.push(line(op));
}

// test-phy.mjs newDev: the dumps, the firmware attached, the PCIe rings, spies for chan/rfk1's chip ops
const SPIES = ['initTxpwrUnit', 'rfkInit', 'setTxpwrCtrl', 'powerTrim', 'cfgTxrxPath'];
async function newDev(model = {}) {
  const d = new X.ChipDev({ c2h: X.phycapC2h(2, 2, 0, 0), ...model });
  d.logs = [];
  d.log = (m) => d.logs.push(m);
  d.st = { cv: 1, acv: 1, efuseLog: kit('efuse-log.bin'), efuseDav: kit('efuse-dav.bin'), efuseCtrl: 0x2020 };
  fw.fwAttach(d, FWP);
  await pciAlloc(d);
  await d.flush();
  d.trace.length = 0;
  d.calls = [];
  d.phy = { ops: Object.fromEntries(SPIES.map((n) => [n, async (dd) => { await dd.flush(); dd.calls.push(`= call ${n} at ${dd.trace.length}`); }])) };
  return d;
}
const calls = (d) => { const c = d.calls; d.calls = []; return c; };

// ---- state as text ----
const s8list = (a) => list((a ?? []).map((v) => v | 0));
function efuseLines(ef, hal) {
  const mac = [...ef.addr].map((x) => x.toString(16).padStart(2, '0')).join(':');
  const g = ef.efuseGain, t = ef.tssi, p = ef.pwrTrim;
  return [
    `= efuse valid ${b01(ef.valid)} pk ${b01(ef.powerKValid)} mac ${mac} rfe ${ef.rfeType} xtal ${ef.xtalCap} country ${list(ef.countryCode)}`,
    `= tssi thermal ${list(t.thermal)} cck ${s8list(t.tssiCck[0])} / ${s8list(t.tssiCck[1])}`,
    `= tssi mcs ${s8list(t.tssiMcs[0])} / ${s8list(t.tssiMcs[1])}`,
    `= tssi trim ${s8list(t.tssiTrim[0])} / ${s8list(t.tssiTrim[1])}`,
    `= trim thermal ${list(p.thermalTrim)} pg ${b01(p.pgThermalTrim)} pa ${list(p.paBiasTrim)} pg ${b01(p.pgPaBiasTrim)}`,
    `= gain offset ${b01(g.offsetValid)} ${s8list(g.offset[0])} / ${s8list(g.offset[1])}`,
    `= gain comp ${b01(g.compValid)} ${s8list(g.comp[0])} / ${s8list(g.comp[1])} base ${g.offsetBase[0]},${g.rssiBase[0]}`,
    `= dav ${ef.davLog ? [...ef.davLog].map((x) => x.toString(16).padStart(2, '0')).join('') : '-'}`,
    ...(hal ? [`= hal cckpd ${b01(hal.supportCckpd)} igi ${b01(hal.supportIgi)}`] : []),
  ];
}
const halLine = (h) => `= hal tx ${h.txNss} rx ${h.rxNss} ant ${h.antennaTx},${h.antennaRx} div ${b01(h.txPathDiversity)} noeht ${b01(h.noEht)}`;
function sum(pairs) { let s = 0; for (const [a, v] of pairs) s = (s + Math.imul(a, 31) + v) >>> 0; return s; }
const pair = (p) => (p ? `${hx(p[0])},${hx(p[1])}` : '-');
function loadLine(name, r) {
  if (!r) return `= load ${name} -`;
  return `= load ${name} applied ${r.applied} aborted ${b01(r.aborted)} head ${r.headline ? `${r.headline.size}/${r.headline.idx}` : '-'} ` +
    `first ${pair(r.first)} last ${pair(r.last)}${r.h2cValues !== undefined ? ` h2c ${r.h2cValues} ${b01(r.h2cOk)}` : ''}`;
}
function gainLines(g) {
  const o = [];
  for (let gb = 0; gb < 8; gb++) for (let p = 0; p < 2; p++) {
    o.push(`= bbgain ${gb} ${p} lna ${s8list(g.lnaGain[gb][p])} tia ${s8list(g.tiaGain[gb][p])} byp ${s8list(g.lnaGainBypass[gb][p])} ` +
      `op1db ${s8list(g.lnaOp1db[gb][p])} tiaop ${s8list(g.tiaLnaOp1db[gb][p])} r20 ${g.rplOfst20[gb][p]} ` +
      `r40 ${s8list(g.rplOfst40[gb][p])} r80 ${s8list(g.rplOfst80[gb][p])} r160 ${s8list(g.rplOfst160[gb][p])}`);
  }
  return o;
}
function h2cLines(d, from) {
  const bufs = new Set(d.pci.tx.CH12.bufs);
  return d.memWrites.slice(from).filter(([a]) => bufs.has(a)).map(([, b]) => {
    const w0 = b.readUInt32LE(24), w1 = b.readUInt32LE(28);
    const len = field(w1, R('H2C_HDR_TOTAL_LEN')) - R('H2C_HEADER_LEN');
    const words = [...Array(Math.max(0, len) >> 2).keys()].map((k) => b.readUInt32LE(32 + k * 4));
    return `= h2c ${hx(w0)} ${hx(w1)} n ${words.length} first ${hx(words[0] ?? 0)} last ${hx(words.at(-1) ?? 0)} ` +
      `sum ${hx(words.reduce((s, w) => (s + w) >>> 0, 0))}`;
  });
}
function cfoLines(c) {
  return [`= cfo cap ${c.crystalCap} def ${c.defXCap} ofst ${c.xCapOfst} ub ${c.xCapUb} lb ${c.xCapLb} adj ${b01(c.isAdjust)} ` +
    `lock ${b01(c.divergenceLockEn)},${c.lockCnt} status ${c.phyCfoStatus} timer ${b01(c.cfoTrigByTimerEn)},${c.cfoTimerMs} ` +
    `trk ${c.phyCfoTrkCnt} pkt ${c.packetCount},${c.packetCountPre} tail0 ${c.cfoTail[0]} cnt0 ${c.cfoCnt[0]} ` +
    `dcfo ${c.dcfoAvg},${c.dcfoAvgPre} avgpre ${c.cfoAvgPre}`];
}
function envLines(e) {
  return [`= env app ${e.ifsClmApp} mntr ${e.ifsClmMntrTime} period ${e.ccxPeriod} unit ${e.ccxUnitIdx} rac ${e.ccxRacLv} ` +
    `ongoing ${b01(e.ccxOngoing)} wd ${e.ccxWatchdogResult ?? 0} thl ${list(e.ifsClmThL)} thh ${list(e.ifsClmThH)}`,
  `= env rep ${list([e.ifsClmTx, e.ifsClmEdccaExclCca, e.ifsClmCckccaExclFa, e.ifsClmOfdmccaExclFa, e.ifsClmCckfa, e.ifsClmOfdmfa].map((v) => v ?? 0))} ` +
    `his ${list(e.ifsClmHis ?? [0, 0, 0, 0])} avg ${list(e.ifsClmAvg ?? [0, 0, 0, 0])} cca ${list(e.ifsClmCca ?? [0, 0, 0, 0])} total ${e.ifsClmTotalIfs ?? 0}`,
  `= env ratio ${list([e.ifsClmTxRatio, e.ifsClmEdccaExclCcaRatio, e.ifsClmCckFaRatio, e.ifsClmOfdmFaRatio, e.ifsClmCckCcaExclFaRatio,
    e.ifsClmOfdmCcaExclFaRatio, e.ifsClmCckFaPermil, e.ifsClmOfdmFaPermil].map((v) => v ?? 0))} ifsavg ${list(e.ifsClmIfsAvg ?? [0, 0, 0, 0])} ` +
    `ccaavg ${list(e.ifsClmCcaAvg ?? [0, 0, 0, 0])} edcca ${e.edccaClmRatio ?? 0}`];
}
function digLines(g) {
  return [`= dig rssi ${g.igiRssi} fa ${g.igiFaRssi} ofst ${g.faRssiOfst} max ${g.dynIgiMax} min ${g.dynIgiMin} pdmax ${g.dynPdThMax} ` +
    `pdcnt ${g.dynPdMaxCnt} pden ${b01(g.dynPdThEn)} low ${g.pdLowThOfst} linked ${b01(g.isLinkedPre)} g ${b01(g.lnaGainIs === 'g')} ` +
    `fath ${list(g.faTh)} force ${b01(g.forceGaincodeIdxEn)} bak ${g.bakDig} noisy ${g.noisyLv ?? 0} diag ${g.diagPdCnt ?? 0} bypass ${b01(g.bypassDig)}`];
}

// ---- 1. efuse ----
{
  let d = await newDev();
  await scenario('efuse parse', d, async () => { await phy.efuseParse(d); return efuseLines(d.st.efuse, d.st.hal); });
  d = await newDev({ rdyFails: 4 });
  await scenario('efuse parse, four failed phycap dumps', d, async () => { await phy.efuseParse(d); return efuseLines(d.st.efuse); });
  d = await newDev({ rdyFails: 5 });
  await scenario('efuse parse, five failed phycap dumps', d, async () => { await phy.efuseParse(d); return efuseLines(d.st.efuse); });
  d = await newDev();
  delete d.st.efuseCtrl;
  await scenario('efuse parse, autoload register read', d, async () => { await phy.efuseParse(d); return efuseLines(d.st.efuse); });
  d = await newDev();
  const pc = Buffer.alloc(128, 0xff);
  const at = (a, v) => { pc[a - pe.PHYCAP_ADDR] = v; };
  at(0x5e9, 0xaa); at(0x5d6, 0x05); at(0x5cf, 0xfe); at(0x5ab, 0x81); at(0x5df, 0x12); at(0x5dc, 0x13);
  at(0x5de, 0x21); at(0x5db, 0x43); at(0x5bb, 0x0f); at(0x58d, 0x07); at(0x5ba, 0xf8); at(0x590, 0x31);
  d.st.efusePhycap = pc;
  d.st.efuseLog = kit('efuse-log.bin').subarray(0, 2048);
  await scenario('efuse parse, phycap given, 2048-byte log', d, async () => { await phy.efuseParse(d); return efuseLines(d.st.efuse); });
  // _decode_efuse_gain: high nibble -> path A, low nibble -> path B, signed; 0xff everywhere -> not valid
  const g2 = Buffer.from(kit('efuse-log.bin'));
  g2[X.OF.rx_gain_2g_cck] = 0x3e;
  g2[X.OF.rx_gain_5g_high] = 0xf1;
  const e2 = pe.newEfuse();
  await scenario('read_efuse gain nibbles', d, async () => { pe.readEfuse8852bx(g2, e2); return efuseLines(e2).slice(5, 6); });
  for (const [n] of X.GAIN) g2[X.OF[n]] = 0xff;
  await scenario('read_efuse gain all 0xff', d, async () => { pe.readEfuse8852bx(g2, e2); return efuseLines(e2).slice(5, 6); });
  // the logical map from the physical dump and the DAV map
  await scenario('logical maps', d, async () => {
    const log = pe.logicalEfuseMap(kit('efuse-phy.bin'), Buffer.alloc(2048, 0xff));
    const dav = pe.logicalEfuseMap(kit('efuse-dav.bin'), Buffer.alloc(16, 0xff));
    return [`= log ${b01(log.equals(kit('efuse-log.bin').subarray(0, 2048)))} dav ${dav.toString('hex')}`];
  });
}

// ---- 2. firmware tables and the walker ----
const t = phy.phyTablesFromElements(FWP.elements);
{
  const d = new FakeDev();
  await scenario('tables', d, async () => [`= tables ${[t.bb, t.bbGain, t.rf[0], t.rf[1], t.nctl].map((x) => x.nRegs).join(',')} ` +
    `paths ${t.rf[0].rfPath},${t.rf[1].rfPath} afe ${t.afe ? 1 : 0} rf2 ${t.rf[2] ? 1 : 0}`]);
  for (const [name, tbl] of [['BB_REG', t.bb], ['BB_GAIN', t.bbGain], ['RADIO_A', t.rf[0]], ['RADIO_B', t.rf[1]], ['RF_NCTL', t.nctl]]) {
    for (const [rfe, cv] of [[1, 1], [1, 0], [2, 1], [0x29, 1], [1, 2], [5, 1], [0, 0]]) {
      const seen = [];
      const dd = new FakeDev();
      dd.logs = [];
      dd.log = (m) => dd.logs.push(m);
      await scenario(`walk ${name} ${rfe}/${cv}`, dd, async () => {
        const r = pt.initReg(dd, tbl, { rfe, cv }, (x, a, v) => seen.push([a, v]));
        return [loadLine(name, r), `= sum ${seen.length} ${hx(sum(seen))}`];
      });
    }
  }
  const mk = (pairs, rfPath = 0) => ({ nRegs: pairs.length, regs: Uint32Array.from(pairs.flat()), rfPath });
  const H = (rfe, cv) => (0xf0000000 | (rfe << 16) | cv) >>> 0, IF = (x) => (0x80000000 | x) >>> 0, ELIF = (x) => (0x90000000 | x) >>> 0;
  const CHK = 0x40000000, ELSE = 0xa0000000, END = 0xb0000000;
  const small = mk([[H(1, 0), 0], [H(1, 3), 0], [H(0xff, 2), 0], [0x10, 1], [IF(0x10003), 0], [CHK, 0], [0x11, 2], [ELIF(0x10000), 0], [CHK, 0],
    [0x12, 3], [ELSE, 0], [0x13, 4], [END, 0], [0x14, 5]]);
  const smalls = [['small', small], ['case2', mk([[H(1, 3), 0], [H(1, 0xff), 0], [H(1, 0xff), 0], [0x10, 1]])], ['nohead', mk([[H(2, 3), 0], [0x10, 1]])],
    ['flat', mk([[0x20, 1], [0x21, 2]])], ['double check', mk([[IF(0x10001), 0], [CHK, 0], [0x30, 1], [ELIF(0x10001), 0], [CHK, 0], [0x31, 2], [END, 0], [0x32, 3]])]];
  for (const [name, tbl] of smalls) {
    for (const [rfe, cv] of [[1, 1], [1, 0], [7, 0], [0xff, 2]]) {
      const seen = [];
      const dd = new FakeDev();
      dd.logs = [];
      dd.log = (m) => dd.logs.push(m);
      await scenario(`walk ${name} ${rfe}/${cv}`, dd, async () => {
        const r = pt.initReg(dd, tbl, { rfe, cv }, (x, a, v) => seen.push([a, v]));
        return [loadLine(name, r), `= seen ${seen.map(pair).join(' ')}`];
      });
    }
  }
}

// ---- 3. config functions, the RF H2C store ----
{
  let f = new FakeDev();
  await scenario('config_bb_reg', f, async () => {
    [[0xfe, 0], [0xfd, 0], [0xfc, 0], [0xfb, 0], [0xfa, 0], [0xf9, 0], [0x900, R('BYPASS_CR_DATA')], [0x904, 7]].forEach(([a, v]) => pt.configBbReg(f, a, v));
  });
  f = new FakeDev();
  const rfInfo = fw.rfRegInfo(R('RF_PATH_A'));
  await scenario('config_rf_reg_v1', f, async () => {
    [[0xfe, 0], [0x10005, 3], [0x05, 7], [0x105, 9], [0xfd, 1]].forEach(([a, v]) => pt.configRfRegV1(f, a, v, 0, rfInfo));
    return [`= idx ${rfInfo.currIdx}`];
  });
  f = new FakeDev();
  const rfB = fw.rfRegInfo(R('RF_PATH_B'));
  await scenario('config_rf_reg_v1 path B direct', f, async () => {
    [[0x10018, 0x12345], [0x10042, 0x1], [0x18, 0xfffff]].forEach(([a, v]) => pt.configRfRegV1(f, a, v, 1, rfB));
    return [`= idx ${rfB.currIdx}`];
  });
  const d = await newDev();
  const m0 = d.memWrites.length;
  await scenario('rf reg upload', d, async () => { await fw.rfRegUpload(d, rfInfo); return h2cLines(d, m0); });
}

// ---- 4. PHY capabilities from the firmware ----
for (const [name, c2h] of [['1,2,2,2', X.phycapC2h(1, 2, 2, 2)], ['0,3,1,2 11be', X.phycapC2h(0, 3, 1, 2, R('RTW89_C2HREG_PHYCAP_W1_PROT_11BE'))],
  ['2,2,2,1', X.phycapC2h(2, 2, 2, 1)], ['wrong id', X.phycapC2h(2, 2, 0, 0, 3, R('RTW89_FWCMD_C2HREG_FUNC_NULL'))]]) {
  const d = await newDev({ c2h });
  await phy.efuseParse(d);
  if (name === '1,2,2,2') await scenario('dm init before phycap', d, async () => { await phy.phyDmInit(d); });
  await scenario(`mac_setup_phycap ${name}`, d, async () => {
    const c = await phy.macSetupPhycap(d);
    return [`= cap ${c.txNss},${c.rxNss},${c.txAnt},${c.rxAnt}`, halLine(d.st.hal)];
  });
}

// ---- 5. BB/RF reset, BB table and RF tables ----
{
  const d = await newDev();
  await phy.efuseParse(d);
  await phy.macSetupPhycap(d);
  await scenario('reset_bb_rf', d, async () => { await phy.resetBbRf(d); });
  await scenario('init_bb_reg', d, async () => {
    await phy.initBbReg(d);
    return [...calls(d), loadLine('bb', d.phy.loads.bb), loadLine('bbgain', d.phy.loads.bbGain), ...gainLines(d.phy.bbGain)];
  });
  let m0 = d.memWrites.length;
  await scenario('init_rf_reg', d, async () => {
    await phy.initRfReg(d);
    return [...d.phy.loads.rf.map((l, i) => loadLine(`rf${i}`, l)), ...h2cLines(d, m0)];
  });
  const mk = (pairs, rfPath) => ({ nRegs: pairs.length, regs: Uint32Array.from(pairs.flat()), rfPath });
  const big = (n, path) => mk(Array.from({ length: n }, (_, i) => [0x10000 | (i & 0xff), i]), path);
  d.phy.tables.rf = [big(1100, 0), big(1600, 1), null, null];
  m0 = d.memWrites.length;
  await scenario('init_rf_reg 1100 and 1600 values', d, async () => [...d.phy.loads.rf.length ? [] : [],
    ...(await phy.initRfReg(d), d.phy.loads.rf.map((l, i) => loadLine(`rf${i}`, l))), ...h2cLines(d, m0)]);
  d.phy.tables.rf = [big(3, 0), big(3, 1), null, null];
  d.onPoll = (k, a) => a !== d.pci.tx.CH12.idx;
  await scenario('init_rf_reg, CH12 stuck', d, async () => { await phy.initRfReg(d); return d.phy.loads.rf.map((l, i) => loadLine(`rf${i}`, l)); });
  d.phy.tables.rf = [big(3, 0), null, null, null];
  await scenario('init_rf_reg, no RADIO_B', d, async () => { await phy.initRfReg(d); });
}
{
  const d = await newDev();
  await phy.efuseParse(d);
  d.phy.tables = phy.phyTablesFromElements(FWP.elements);
  d.phy.tables.bb = null;
  await scenario('init_bb_reg, no BB_REG', d, async () => { await phy.initBbReg(d); });
}

// ---- 6. dm init ----
{
  const d = await newDev();
  await phy.efuseParse(d);
  await phy.macSetupPhycap(d);
  await scenario('dm init', d, async () => {
    await phy.phyDmInit(d);
    const p = d.phy, g = d.st.efuse.efuseGain;
    return [...calls(d), `= thermal ${list(p.avgThermal)} max ${p.stat.lastThermalMax} cap ${p.cfo.crystalCap} ofst ${p.cfo.xCapOfst} ` +
      `bandedge ${p.ulTb.defIfBandedge} nctl ${b01(p.nctlReady)} base ${g.offsetBase[0]},${g.rssiBase[0]}`, loadLine('nctl', p.loads.nctl),
    ...envLines(p.env), ...digLines(p.dig), ...cfoLines(p.cfo)];
  });
  d.model.nctlReads = 0;
  d.model.nctl = (n) => (n >= 2 ? 4 : 0);
  await scenario('init_rf_nctl 3 rounds', d, async () => { await phy.initRfNctl(d); return [`= nctl ${b01(d.phy.nctlReady)}`]; });
  d.model.nctl = () => 0;
  await scenario('init_rf_nctl timeout', d, async () => { await phy.initRfNctl(d); return [`= nctl ${b01(d.phy.nctlReady)}`]; });
  // 5 GHz channel 36 at 80 MHz, already linked: DIG's thresholds of the link
  const e = await newDev();
  await phy.efuseParse(e);
  await phy.macSetupPhycap(e);
  e.chan = { band: '5g', ch: 36, bw: 80 };
  e.sta = { totalStaAssoc: 1 };
  await scenario('dm init 5g 80 linked', e, async () => { await phy.phyDmInit(e); return [...calls(e), ...digLines(e.phy.dig)]; });
}

// ---- 7. tracking: thermal, CFO ----
{
  const d = await newDev();
  await phy.efuseParse(d);
  await phy.macSetupPhycap(d);
  await phy.phyDmInit(d);
  calls(d);
  const th = () => [`= thermal ${list(d.phy.avgThermal)} max ${d.phy.stat.lastThermalMax} last ${list(d.phy.stat.thermal)}`];
  d.model.thermal = 0x20;
  await scenario('thermal 0x20', d, async () => { await phy.statThermalUpdate(d); return th(); });
  d.model.thermal = 0;
  await scenario('thermal 0', d, async () => { await phy.statThermalUpdate(d); return th(); });
  d.model.thermal = 0x3f;
  d.rfk = { isTssiMode: [false, true] };
  d.regs.set(BB + 0x1c10 + (1 << 13), 0x2a000000);
  await scenario('thermal path B in TSSI mode', d, async () => { await phy.statThermalUpdate(d); return th(); });
  delete d.rfk;
  d.sta = { totalStaAssoc: 1 };
  await scenario('cfo parse', d, async () => {
    for (let i = 0; i < 10; i++) phy.cfoParse(d, 200, { macId: 0 });
    phy.cfoParse(d, 200, { macId: 64 });
    phy.cfoParse(d, -7, { macId: 63 });
    return cfoLines(d.phy.cfo);
  });
  await scenario('cfo track', d, async () => { await phy.cfoTrack(d, { txThroughput: 0 }); return cfoLines(d.phy.cfo); });
  await scenario('cfo track 150 Mbps', d, async () => { await phy.cfoTrack(d, { txThroughput: 150 }); return cfoLines(d.phy.cfo); });
  for (let i = 0; i < 4; i++) phy.cfoParse(d, -50, { macId: 0 });
  d.model.xtal = {};
  await scenario('cfo track work', d, async () => [`= ran ${b01(await phy.cfoTrackWork(d))}`, ...cfoLines(d.phy.cfo)]);
  for (const [tp, tf] of [[120, 400], [120, 400], [10, 0]]) {
    await scenario(`cfo track ${tp} Mbps ${tf} tf`, d, async () => { await phy.cfoTrack(d, { txThroughput: tp, rxTfPeriodic: tf }); return cfoLines(d.phy.cfo); });
  }
  await scenario('cfo track work idle', d, async () => [`= ran ${b01(await phy.cfoTrackWork(d))}`]);
  d.sta.totalStaAssoc = 0;
  await scenario('cfo track unassociated', d, async () => { await phy.cfoTrack(d); return cfoLines(d.phy.cfo); });
  // a large CFO: the cap walks up to the bound, the divergence lock, then back
  d.sta.totalStaAssoc = 1;
  for (let round = 0; round < 24; round++) {
    for (let i = 0; i < 3; i++) phy.cfoParse(d, 900, { macId: 5 });
    await scenario(`cfo walk ${round}`, d, async () => { await phy.cfoTrack(d, { txThroughput: 1 }); return cfoLines(d.phy.cfo); });
  }
  d.sta.totalStaAssoc = 2;
  phy.cfoParse(d, 300, { macId: 1 });
  await scenario('cfo two stations', d, async () => { await phy.cfoTrack(d); return cfoLines(d.phy.cfo); });
}

// ---- 8. environment monitor, DIG, EDCCA, UL TB ----
{
  const d = await newDev();
  await phy.efuseParse(d);
  await phy.macSetupPhycap(d);
  await phy.phyDmInit(d);
  calls(d);
  const set = (reg, mask, v) => d.regs.set(BB + R(reg), ((d.regs.get(BB + R(reg)) ?? 0) & ~R(mask) | prep(R(mask), v)) >>> 0);
  await scenario('env track, nothing ready', d, async () => { await phy.envMonitorTrack(d); return envLines(d.phy.env); });
  set('R_IFSCNT', 'B_IFSCNT_DONE_MSK', 1);
  set('R_IFSCNT', 'B_IFSCNT_TOTAL_CNT_MSK', 900);
  set('R_IFS_CLM_TX_CNT', 'B_IFS_CLM_TX_CNT_MSK', 3000);
  set('R_IFS_CLM_TX_CNT', 'B_IFS_CLM_EDCCA_EXCLUDE_CCA_FA_MSK', 1200);
  set('R_IFS_CLM_CCA', 'B_IFS_CLM_CCKCCA_EXCLUDE_FA_MSK', 40000);
  set('R_IFS_CLM_CCA', 'B_IFS_CLM_OFDMCCA_EXCLUDE_FA_MSK', 77);
  set('R_IFS_CLM_FA', 'B_IFS_CLM_CCK_FA_MSK', 500);
  set('R_IFS_CLM_FA', 'B_IFS_CLM_OFDM_FA_MSK', 9000);
  [['B_IFS_T1_HIS_MSK', 3], ['B_IFS_T2_HIS_MSK', 0], ['B_IFS_T3_HIS_MSK', 200], ['B_IFS_T4_HIS_MSK', 9]].forEach(([m, v]) => set('R_IFS_HIS', m, v));
  [['R_IFS_AVG_L', 'B_IFS_T1_AVG_MSK', 7], ['R_IFS_AVG_L', 'B_IFS_T2_AVG_MSK', 100], ['R_IFS_AVG_H', 'B_IFS_T3_AVG_MSK', 0xffff],
    ['R_IFS_AVG_H', 'B_IFS_T4_AVG_MSK', 1], ['R_IFS_CCA_L', 'B_IFS_T1_CCA_MSK', 30], ['R_IFS_CCA_L', 'B_IFS_T2_CCA_MSK', 0],
    ['R_IFS_CCA_H', 'B_IFS_T3_CCA_MSK', 60000], ['R_IFS_CCA_H', 'B_IFS_T4_CCA_MSK', 77]].forEach(([r, m, v]) => set(r, m, v));
  set('R_CLM_EDCCA_RDY', 'B_CLM_EDCCA_RDY', 1);
  set('R_CLM_EDCCA_RESULT', 'B_CLM_EDCCA_RESULT', 45000);
  await scenario('env track, reports ready', d, async () => { await phy.envMonitorTrack(d); return envLines(d.phy.env); });
  await scenario('env track again', d, async () => { await phy.envMonitorTrack(d); return envLines(d.phy.env); });
  d.phy.env.ccxManualCtrl = true;
  await scenario('env track, manual', d, async () => { await phy.envMonitorTrack(d); return envLines(d.phy.env); });
  d.phy.env.ccxManualCtrl = false;
  d.phy.env.ccxOngoing = true;
  d.phy.env.ccxRacLv = 3;
  await scenario('env track, racing', d, async () => { await phy.envMonitorTrack(d); return envLines(d.phy.env); });

  // DIG, EDCCA: unlinked, then linked with the AP's RSSI
  d.chan = { band: '2g', ch: 6, bw: 20 };
  const ch = () => [`= ch rssi ${d.phy.chInfo?.rssiMin ?? '-'} macid ${d.phy.chInfo?.rssiMinMacid ?? '-'}`];
  await scenario('rssi update, no AP', d, async () => [`= changed ${b01(phy.statRssiUpdate(d))}`, ...ch()]);
  await scenario('dig unlinked', d, async () => { await phy.phyDig(d); return digLines(d.phy.dig); });
  await scenario('edcca unlinked', d, async () => { await phy.edccaTrack(d, 0); return [`= edcca ${d.phy.edccaBak.thOld ?? 0}`]; });
  d.sta = { totalStaAssoc: 1, ap: { macId: 3, avgRssi: { internal: 80 << 10 } } };
  for (const [i, rssi, tp] of [[0, 80, 0], [1, 82, 20], [2, 100, 20], [3, 20, 0], [4, 160, 30], [5, 160, 30], [6, 160, 30]]) {
    d.sta.ap.avgRssi.internal = rssi << 10;
    await scenario(`rssi update ${i}`, d, async () => [`= changed ${b01(phy.statRssiUpdate(d))} prev ${d.sta.ap.prevRssi}`, ...ch()]);
    await scenario(`dig linked ${i}`, d, async () => { await phy.phyDig(d); return digLines(d.phy.dig); });
    await scenario(`edcca linked ${i}`, d, async () => { await phy.edccaTrack(d, tp); return [`= edcca ${d.phy.edccaBak.thOld} diag ${d.phy.diagEdccaCnt ?? 0}`]; });
  }
  d.phy.dig.bypassDig = true;
  await scenario('dig bypass', d, async () => { await phy.phyDig(d); return digLines(d.phy.dig); });
  d.phy.dig.pauseDig = true;
  await scenario('dig paused', d, async () => { await phy.phyDig(d); return digLines(d.phy.dig); });
  d.phy.dig.pauseDig = false;
  d.chan = { band: '5g', ch: 100, bw: 40 };
  d.sta.totalStaAssoc = 0;
  await scenario('dig 5g unlinked again', d, async () => { await phy.phyDig(d); return digLines(d.phy.dig); });

  // UL TB
  const sta = (o) => { d.sta = { totalStaAssoc: 1, wifiRole: R('RTW89_WIFI_ROLE_STATION'), assoc: true, dynTbBedgeEn: true, defTriIdx: 2, ...o }; };
  for (const [name, o, tf] of [['high', {}, 200], ['low', {}, 10], ['between', {}, 80], ['no bedge', { dynTbBedgeEn: false }, 10],
    ['not assoc', { assoc: false }, 200], ['two stations', { totalStaAssoc: 2 }, 200]]) {
    sta(o);
    await scenario(`ul tb ${name}`, d, async () => { await phy.ulTbCtrlTrack(d, tf); });
  }
  d.phy.ulTb.dynTbTriEn = false;
  sta({});
  await scenario('ul tb, triangle off', d, async () => { await phy.ulTbCtrlTrack(d, 200); });
}

// ---- 9. AFE, RX status, the BB helpers ----
{
  const d = await newDev();
  await phy.efuseParse(d);
  d.phy.tables = phy.phyTablesFromElements(FWP.elements);
  await scenario('bb afe, none', d, async () => [`= rows ${await phy.bbAfe(d)}`]);
  const afe = Buffer.alloc(8 + 7 * 24);
  [[0, 2, 5, 0x1234, 0xff00, 0x12], [0, 0, 0, 0x4000, 0xf, 3], [1, 0, 5, 50, 0, 0], [2, 2, 0, 0x88, 0x1, 0],
    [2, 2, 1, 0x8c, 0xff, 0x5a], [0, 1, 3, 0x4004, 0xffffffff, 7], [0, 4, 1, 0x2000, 0xffffffff, 9]].forEach((r, i) =>
    r.forEach((v, j) => afe.writeUInt32LE(v >>> 0, 8 + i * 24 + j * 4)));
  d.phy.tables.afe = { size: 7 * 24, payload: afe };
  await scenario('bb afe, seven rows', d, async () => [`= rows ${await phy.bbAfe(d)}`]);
  const rxs = [[100, 110, 100, 90, 0x06, true], [0, 0, 80, 60, 0x24, true], [120, 100, 130, 110, 0, true], [50, 60, 40, 255, 0x0e, true],
    [50, 50, 70, 70, 0x5c, true], [50, 50, 70, 70, 0x36, true], [50, 50, 70, 70, 0x71, true], [50, 50, 70, 70, 0x81, true],
    [50, 50, 70, 70, 0xf3, true], [50, 50, 70, 70, 0x16, false]];
  await scenario('rx status', d, async () => rxs.map(([rssiAvg, rplAvg, a, b, chanIdx, valid]) => {
    const s = phy.rxStatus({ rssiAvg, rplAvg, rssi: [a, b], chanIdx, valid });
    return `= rx ${s.rssi} ${list(s.chainSignal)} ${s.chains} ${s.band ?? '-'} ${s.ch ?? '-'} ${s.freq ?? '-'}`;
  }));

  const prime = (dd) => {
    dd.regs.set(BB + R('R_TXPATH_SEL'), prep(R('B_TXPATH_SEL_MSK'), 3));
    dd.regs.set(BB + R('R_CHBW_MOD_V1'), prep(R('B_ANT_RX_SEG0'), 2));
    ['R_P0_RFMODE', 'R_P0_RFMODE_FTM_RX', 'R_P1_RFMODE', 'R_P1_RFMODE_FTM_RX'].forEach((r, i) => dd.regs.set(BB + R(r), 0x1233312 + i));
    dd.regs.set(BB + R('R_TXPWR'), prep(R('B_TXPWR_MSK'), 0x1f0));
  };
  async function helper(name, fn) {
    const dd = new X.ChipDev();
    prime(dd);
    await scenario(name, dd, async () => fn(dd));
  }
  const chan2g = { channel: 1, bandType: R('RTW89_BAND_2G') }, chan5g = { channel: 36, bandType: R('RTW89_BAND_5G') };
  await helper('set_plcp_tx', (dd) => { phy.bbSetPlcpTx(dd); });
  for (const en of [false, true]) {
    await helper(`nbtg_bt_tx ${b01(en)}`, (dd) => { phy.ctrlNbtgBtTx(dd, en, 0); });
    await helper(`btg_bt_rx ${b01(en)}`, (dd) => { phy.ctrlBtgBtRx(dd, en, 0); });
    await helper(`reset_en 2g ${b01(en)}`, (dd) => { phy.bbResetEn(dd, R('RTW89_BAND_2G'), 0, en); });
    await helper(`reset_en 5g ${b01(en)}`, (dd) => { phy.bbResetEn(dd, R('RTW89_BAND_5G'), 0, en); });
  }
  await helper('pmac start', (dd) => { phy.bbSetPmacPktTx(dd, 1, 10, 20, 20, 0, chan2g); });
  await helper('pmac stop 2g', (dd) => { phy.bbSetPmacPktTx(dd, 0, 10, 20, 20, 0, chan2g); });
  await helper('pmac stop 5g', (dd) => { phy.bbSetPmacPktTx(dd, 0, 10, 20, 20, 0, chan5g); });
  await helper('tx_mode_switch 0', (dd) => { phy.bbTxModeSwitch(dd, 0, 0); });
  await helper('tx_mode_switch 1', (dd) => { phy.bbTxModeSwitch(dd, 0, 1); });
  await helper('set_power -5', (dd) => { phy.bbSetPower(dd, -5, 0); });
  for (const p of ['RF_PATH_A', 'RF_PATH_B', 'RF_PATH_AB']) await helper(`cfg_tx_path ${p}`, (dd) => { phy.bbCfgTxPath(dd, R(p)); });
  await helper('backup/restore tssi', async (dd) => {
    const bak = await phy.bbBackupTssi(dd, 0);
    phy.bbRestoreTssi(dd, 0, bak);
    return [`= bak ${bak.txPath} ${bak.rxPath} ${bak.txPwr} ${hx(bak.p0Rfmode)} ${hx(bak.p1RfmodeFtm)}`];
  });
  for (const p of ['RF_A', 'RF_B', 'RF_AB']) await helper(`rf_mode_rx_path ${p}`, (dd) => { phy.bbCtrlRfModeRxPath(dd, R(p)); });
  await helper('rf_mode_rx_path 0', (dd) => { phy.bbCtrlRfModeRxPath(dd, 0); });
  await helper('bb reset', (dd) => { phy.bbReset(dd, 0); });
  await helper('bb sethw', async (dd) => { dd.st = { efuse: pe.newEfuse() }; await phy.bbSethw(dd); const g = dd.st.efuse.efuseGain; return [`= base ${g.offsetBase[0]},${g.rssiBase[0]}`]; });
  await helper('get_thermal A', async (dd) => [`= thermal ${await phy.getThermal(dd, 0)}`]);
  await helper('get_thermal B tssi', async (dd) => { dd.rfk = { isTssiMode: [false, true] }; dd.regs.set(BB + 0x3c10, 0x2a000000); return [`= thermal ${await phy.getThermal(dd, 1)}`]; });
  await helper('disable_bb_rf', async (dd) => { await phy.disableBbRf(dd); });
  await helper('physts parsing init', async (dd) => { await phy.physStsParsingInit(dd); });
}

// ---- the C's sequences (phy-test-expect.mjs) ----
const sel = (tbl) => X.blockSelect(tbl, 1, 1).out;
await spec('reset_bb_rf', {}, X.cResetBbRf);
await spec('bb_reg + bb_reset', {}, (e) => { sel(t.bb).forEach(([a, v]) => X.cConfigBbReg(e, a, v)); X.cBbReset(e); });
for (const path of [0, 1]) await spec(`radio ${path}`, {}, (e) => sel(t.rf[path]).forEach(([a, v]) => X.cConfigRfRegV1(e, path, a, v)));
await spec('dm_init', {}, (e) => X.cDmInit(e, sel(t.nctl)));
await spec('preinit nctl 3', { nctl: (n) => (n >= 2 ? 4 : 0) }, (e) => X.cPreinitNctl(e, 3));
await spec('preinit nctl 101', { nctl: () => 0 }, (e) => X.cPreinitNctl(e, 101));
await spec('set cap 0x3f', {}, (e) => X.cSetCap(e, 0x3f));
const table3 = (e, rows) => rows.forEach(([a, m, v]) => phyMask32(e, a, m, v));
await spec('pmac_ht20_mcs7_tbl', {}, (e) => table3(e, X.cTable('rtw8852b_common.c', 'rtw8852bx_pmac_ht20_mcs7_tbl')));
await spec('preagc dis', {}, (e) => table3(e, X.cTable('rtw8852b_common.c', 'rtw8852bx_btc_preagc_dis_defs')));
await spec('preagc en', {}, (e) => table3(e, X.cTable('rtw8852b_common.c', 'rtw8852bx_btc_preagc_en_defs')));
for (const en of [false, true]) {
  await spec(`btg_bt_rx ${b01(en)}`, {}, (e) => X.cBtgBtRx(e, en));
  await spec(`reset_en 2g ${b01(en)}`, {}, (e) => X.cResetEn(e, true, en));
}
await spec('pmac start', {}, (e) => X.cPmacPktTx(e, 1, 10, 20, true));
await spec('pmac stop 5g', {}, (e) => X.cPmacPktTx(e, 0, 10, 20, false));
await spec('tx_mode_switch 0', {}, X.cTxModeSwitch);
await spec('set_power -5', {}, (e) => X.cSetPower(e, -5));
await spec('cfg_tx_path AB', {}, X.cCfgTxPathAB);
await spec('rf_mode_rx_path AB', {}, (e) => X.cRfModeRxPath(e, R('RF_AB')));
await spec('cfo dm', {}, async (e) => { await X.cSetCap(e, 0x3f); await phyR32Mask(e, R('R_DCFO'), R('B_DCFO')); phyMask32(e, R('R_DCFO_COMP_S0'), R('B_DCFO_COMP_S0_MSK'), 319); });

process.stdout.write(out.join('\n') + '\n');
