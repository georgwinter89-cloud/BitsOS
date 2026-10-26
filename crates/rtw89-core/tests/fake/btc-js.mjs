// btc-js.mjs: the JS driver's own btc.mjs (btc-run/-hw/-h2c/-c2h/-wlsta.mjs, with phy-bbx.mjs's
// pre-AGC tables) on test-btc.mjs's device (FakeDev with the CMAC on, rfe 1, cv 1, the firmware
// version of the real file), with fw.mjs's h2cSend as a stand-in (stubs/btc-fw.mjs) that notes
// each coex H2C with its header and payload where it happened. Scenario by scenario, the JS side
// of test-btc.mjs (which checks the same module against sequences written from the Linux C) and
// more, as text: `## <scenario>`, every op as a bringup-golden.txt line, the H2Cs as `= call h2c`
// lines where they happened, then results and the whole coexistence state as `= ...` lines.
// tests/btc_js.rs runs the same on the Rust side (rfk1::fake::RfFake as the FakeDev) and compares
// the text line by line.
//   node btc-js.mjs            (WLAN_TOOLS: the read-only WLAN tree)
// Reads only; writes nothing but stdout.
import { readFileSync } from 'node:fs';
import { pathToFileURL } from 'node:url';

const WZ = process.env.WLAN_TOOLS ?? '/home/georg/BitsOS/werkzeuge/plattform/wlan';
const imp = (rel) => import(pathToFileURL(`${WZ}/${rel}`).href);
const { useStubs } = await imp('js/stubs/hook.mjs');
useStubs(/\/btc(-[a-z0-9]+)?\.mjs$/, { './fw.mjs': new URL('stubs/btc-fw.mjs', import.meta.url).href });
const { FakeDev, R, BB } = await imp('kit/rtwdev.mjs');
const btc = await imp('js/btc.mjs');
const hw = await imp('js/btc-hw.mjs');
const { h2cCxRole } = await imp('js/btc-h2c.mjs');

const KIND = { 1: 'wr', 2: 'rd', 3: 'rmw', 4: 'poll', 5: 'delay', 6: 'mwr', 7: 'mrd', 8: 'mcpy', 9: 'cfgr', 10: 'cfgw', 11: 'mpoll', 12: 'mark', 13: 'now' };
const HEX = (v) => `0x${(v >>> 0).toString(16).toUpperCase().padStart(8, '0')}`;
const line = ([k, s, t, a, b, c]) =>
  `${KIND[k]}${k <= 4 || k === 6 || k === 7 ? s * 8 : ''} ${HEX(a)} ${HEX(b)} ${HEX(c)}${k === 4 || k === 11 ? ` ${t} ms` : ''}`;
const H = (v) => `0x${((v ?? 0) >>> 0).toString(16)}`;
const B = (v) => (v ? '1' : '0');
const N = (v) => `${v ?? 0}`;
const L = (a, f = N) => [...a].map(f).join(',');
const out = [];

// the firmware version of the NORMAL_CE cv 1 entry of the real file (as test-btc.mjs)
const fwFile = readFileSync(`${WZ}/firmware/rtw8852b_fw-2.bin`);
let fwVer = null;
for (let i = 0; i < fwFile[1]; i++) {
  const o = 16 + i * 16;
  if (fwFile[o] === 1 && fwFile[o + 1] === 5) {
    const w1 = fwFile.readUInt32LE(fwFile.readUInt32LE(o + 4) + 4);
    fwVer = [w1 & 0xff, (w1 >> 8) & 0xff, (w1 >> 16) & 0xff, w1 >>> 24];
  }
}

// the whole coexistence state, in a fixed order (tests/btc_js.rs prints the same)
function stateLines(d) {
  const b = d.btc;
  if (!b) return ['= btc none'];
  const { wl, bt0, dm, md, fwinfo, ctrl } = b, s = wl.status, r = wl.role, k = wl.rfkInfo;
  const o = [];
  o.push(`= wl scbd ${H(wl.scbd)} chg ${N(wl.scbdChg)} coex ${N(wl.coexMode)} reqmac ${N(wl.ptaReqMac)} ${B(wl.ptaRegMacChg)} ` +
    `lchg ${N(wl.linkModeChg)} bg ${N(wl.bgMode)} ver ${H(wl.verInfo.fwCoex)} ${H(wl.verInfo.fw)} hi ${N(wl.is5gHiCh)} ` +
    `go ${N(wl.goClientExist)} noa ${N(wl.noaExist)} dbcc ${N(wl.dbccChg)} band ${L(wl.rfBandMap)} scan ${L(wl.scanBand)} ` +
    `${N(wl.scanPhyMap)} rssi ${N(wl.rssiLevel)}`);
  o.push(`= st init ${B(s.initOk)} rf ${N(s.rfOff)} ${N(s.rfOffPre)} lps ${N(s.lps)} ${N(s.lpsPre)} busy ${N(s.busy)} ` +
    `scan ${N(s.scan)} tr ${N(s.transacting)} 4way ${N(s._4way)} dhcp ${N(s.dhcp)} hs ${N(s.handshake)} ` +
    `roam ${N(s.roaming)} dir ${N(s.trafficDir)}`);
  o.push(`= rfk ${N(k.state)} ${N(k.type)} ${N(k.pathMap)} ${N(k.phyMap)} ${N(k.band)} ${B(k.conRfk)}`);
  o.push(`= role mode ${N(r.linkMode)} ${N(r.linkModeHb1)} chg ${N(r.linkModeChg)} p2p ${N(r.p2pExist)} req ` +
    `${N(r.ptaReqBand)} dbcc ${N(r.dbccEn)} ${N(r.dbccChg)} ${N(r.dbcc2gPhy)} map ${H(r.roleMap)} mrole ` +
    `${H(r.mroleType)} ${H(r.mroleNoaDuration)} cnt ${N(r.connectCnt)}`);
  r.rlink.forEach((x, i) => o.push(`= rlink ${i} ${N(x.connected)} ${N(x.pid)} ${N(x.phy)} ${N(x.noa)} ${N(x.rfBand)} ` +
    `${B(x.active)} ${N(x.bw)} ${N(x.role)} ${N(x.ch)} ${H(x.noaDur)} ${N(x.clientCnt)} ${N(x.mode)} ${N(x.macId)} ` +
    `${N(x.clientPs)}`));
  wl.rlinkInfo.forEach((x, i) => {
    if (!x) { o.push(`= info ${i} -`); return; }
    const t = x.stat?.traffic;
    const tr = t ? `${N(t.txTfcLv)} ${N(t.rxTfcLv)} ${N(t.txThroughput)} ${N(t.rxThroughput)} ${N(t.txRate)} ${N(t.rxRate)}` : '-';
    o.push(`= info ${i} ${N(x.mode)} ${N(x.role)} ${N(x.phy)} ${N(x.pid)} ${B(x.active)} ${N(x.band)} ${N(x.ch)} ` +
      `${N(x.bw)} ${N(x.macId)} ${N(x.connected)} tr ${tr} rssi ${x.stat?.rssi ?? '-'} st ` +
      `${x.rssiState ? L(x.rssiState) : '-'} busy ${N(x.busy)} dir ${N(x.dir)} drop ${N(x.rxRateDropCnt)}`);
  });
  o.push(`= bt0 ${N(bt0.enableNow)} ${N(bt0.enableLast)} patch ${N(bt0.runPatchCode)} scbd ${H(bt0.scbdC2h)} ` +
    `${H(bt0.scbd)} ${N(bt0.mbxAvl)} ${N(bt0.whqlTest)} ${N(bt0.btgType)} ${N(bt0.a2dp)} ${N(bt0.pan)} ${N(bt0.hfp)} ` +
    `${N(bt0.lnaConstrain)} ${N(bt0.hiLnaRx)} ${N(bt0.connect)} ${N(bt0.linkCntNow)} ${N(bt0.infoUpdates)} ` +
    `${H(bt0.verFw)} lowpri ${B(bt0.scanRxLowPri)} ext ${N(b.btExtFuncType)}`);
  o.push(`= ctrl ${N(ctrl.manual)} ${N(ctrl.ignoBt)} ${B(ctrl.alwaysFreerun)} ${N(ctrl.traceStep)} ${N(ctrl.fwScan)}`);
  const gnt = dm.gntSet.map((g) => `${N(g.gntBt)}${N(g.gntBtSwEn)}${N(g.gntWl)}${N(g.gntWlSwEn)}`).join(',');
  o.push(`= dm rsn ${N(dm.runReason)} act ${N(dm.runAction)} only ${N(dm.wlOnly)} ${N(dm.btOnly)} inst ` +
    `${N(dm.tdmaInstantExcute)} lps ${B(dm.lpsCtrlScbd)} ${B(dm.lpsCtrlChange)} ant ${H(dm.setAntPath)} gnt ${gnt} ` +
    `agc ${N(dm.wlPreAgc)} ${N(dm.wlPreAgcRb)} btg ${N(dm.wlBtgRx)} ${N(dm.wlBtgRxRb)} stb ${N(dm.wlStbChg)} ` +
    `lna2 ${N(dm.wlLna2)} err ${B(dm.error.tdma)}${B(dm.error.slot)} cnt ${N(dm.cntDm.tdma)} ${N(dm.cntDm.slot)}`);
  const bind = (x) => `${N(x.rfBand)}/${N(x.btSel)}/${N(x.btLinkWeight)}/${N(x.wlHwbSel)}/${N(x.wlLinkMode)}`;
  o.push(`= dm2 txpwr ${L(dm.rfTrxPara.wlTxPower)} rxg ${L(dm.rfTrxPara.wlRxGain)} lvl ${N(dm.trxParaLevel)} oob ` +
    `${B(dm.outOfBand)} tdd ${B(dm.tddEn)} fdd ${B(dm.fddEn)} bind ${bind(dm.tddBind)} ${bind(dm.fddBind)} free ` +
    `${B(dm.freerun)} fddt ${N(dm.fddtTrain)} req ${B(dm.btReqEn)} ${L(dm.btReqLen)} pta ${B(dm.ptaOwnerWarn)} leak ` +
    `${N(dm.leakAp)} sta ${N(dm.cntNotifyWlSta)} ${N(dm.cntDmWlStaLast)} offl ${N(dm.wlFwCxOffload)} txl ` +
    `${N(dm.txLimit.enable)} ${N(dm.txLimit.txTime)} ${N(dm.txLimit.txRetry)}`);
  o.push(`= tdma ${L(dm.tdma)} now ${L(dm.tdmaNow)}`);
  const slots = (a) => a.map(([dur, tbl, type]) => `${dur}/${H(tbl)}/${type}`).join(' ');
  o.push(`= slot ${slots(dm.slot)}`);
  o.push(`= slotnow ${slots(dm.slotNow)}`);
  const ii = dm.initInfo ?? {};
  o.push(`= init ${N(ii.initMode)} ${N(ii.wlInitOk)} ${N(ii.wlOnly)} ${N(ii.btOnly)} ${N(ii.cxOther)} ${N(ii.wlGuardCh)} ` +
    `${N(ii.dbccEn)}`);
  const a = md.ant;
  o.push(`= md ${N(md.rfeType)} ${N(md.ktVer)} ${N(md.btSolo)} ${N(md.bt0SwType)} ${N(md.bt0Pos)} ${N(md.waType)} ` +
    `${N(md.ktVerAdie)} ant ${N(a.type)} ${N(a.num)} ${N(a.isolation)} ${N(a.diversity)} ${N(a.singlePos)} ${N(a.btgPos)} ` +
    `${N(a.streamCnt)} type ${N(b.antType)} btg ${N(b.btgPos)}`);
  o.push(`= fw rpt ${H(fwinfo.rptEnMap)} h2c ${N(fwinfo.cntH2c)} fail ${N(fwinfo.cntH2cFail)} policy ${H(b.policyType)} ` +
    `force ${B(b.updatePolicyForce)}`);
  const t = dm.trxInfo ?? {};
  o.push(`= trx ${N(t.txLvl)} ${N(t.rxLvl)} ${N(t.txRate)} ${N(t.rxRate)} ${N(t.txTp)} ${N(t.rxTp)} ${N(t.wlRssi)}`);
  o.push(`= reports ${b.btReport.map((x) => (x.kind === 'scbd' ? `scbd ${x.bid} ${H(x.scbd)}` : `info ${x.bid} ` +
    `${x.data.toString('hex')}`)).join(' ; ') || '-'}`);
  return o;
}

async function scenario(name, d, fn) {
  out.push(`## ${name}`);
  await d.flush();
  const t0 = d.trace.length;
  d.calls = [];
  let res = [];
  try {
    const r = await fn();
    res = Array.isArray(r) ? r : [];
    await d.flush();
  } catch (e) {
    res = ['! error'];
    try { await d.flush(); } catch { d.q = new d.q.constructor(); }
    if (process.env.BTC_JS_VERBOSE) console.error(name, e.stack);
  }
  const ops = d.trace.slice(t0).map(line);
  const calls = d.calls.map(([n, pos, ...a]) => [pos - t0, `= call ${n}${a.map((x) => ` ${x}`).join('')}`]);
  for (let i = 0; i <= ops.length; i++) {
    for (const [pos, c] of calls) if (pos === i) out.push(c);
    if (i < ops.length) out.push(ops[i]);
  }
  out.push(...res, ...stateLines(d));
}

const CMAC_ON = (k, a) => (k === 'mmio' && a === R('R_AX_CMAC_FUNC_EN') ? R('B_AX_CMAC_EN') : undefined);
function newDev(onRead = CMAC_ON, { rfe = 1, ver = fwVer } = {}) {
  const d = new FakeDev({ onRead });
  d.st = { cv: 1, rfe };
  d.fw = { suit: { ver } };
  return d;
}
const ret = (v) => [`= ret ${typeof v === 'boolean' ? B(v) : H(v)}`];

// ---- the bring-up's coexistence on one device, as test-btc.mjs ----
const d = newDev();
out.push(`= fw ${fwVer.join('.')}`);
await scenario('power on', d, () => { btc.btcPowerOn(d); });
await scenario('init', d, () => btc.btcInit(d));
const pm = btc.btcPhyMap(d, 0, 0);
await scenario('dack start', d, async () => ret(await btc.btcWlRfk(d, pm, R('BTC_WRFKT_DACK'), R('BTC_WRFK_START'))));
await scenario('dack stop', d, async () => ret(await btc.btcWlRfk(d, pm, R('BTC_WRFKT_DACK'), R('BTC_WRFK_STOP'))));
await scenario('iqk one-shot', d, async () => ret(await btc.btcWlRfk(d, pm, R('BTC_WRFKT_IQK'), R('BTC_WRFK_ONESHOT_START'))));
await scenario('rxdck one-shot', d, async () => ret(await btc.btcWlRfk(d, pm, R('BTC_WRFKT_RXDCK'), R('BTC_WRFK_ONESHOT_START'))));
await scenario('dack start twice', d, async () => {
  await btc.btcWlRfk(d, pm, R('BTC_WRFKT_DACK'), R('BTC_WRFK_START'));
  await btc.btcWlRfk(d, pm, R('BTC_WRFKT_DACK'), R('BTC_WRFK_START'));
  await btc.btcWlRfk(d, pm, R('BTC_WRFKT_DACK'), R('BTC_WRFK_STOP'));
});
await scenario('rfk state 7', d, async () => ret(await btc.btcWlRfk(d, 0x13, R('BTC_WRFKT_DPK'), 7)));
await scenario('conn rfk', d, () => { btc.btcConnRfk(d, true); btc.btcPreserveBtTime(d, 30); btc.btcConnRfk(d, false); });
await scenario('iqk start', d, () => btc.btcWlRfk(d, pm, R('BTC_WRFKT_IQK'), R('BTC_WRFK_START')));
await scenario('band switch inside iqk', d, () => btc.btcSwitchBand(d));
await scenario('iqk stop', d, () => btc.btcWlRfk(d, pm, R('BTC_WRFKT_IQK'), R('BTC_WRFK_STOP')));
await scenario('radio on', d, () => btc.btcRadioOn(d));
await scenario('band switch', d, () => btc.btcSwitchBand(d));
d.chan = { band: '2g', ch: 6, bw: 20 };
await scenario('scan start', d, () => btc.btcScan(d, true));
await scenario('scan finish', d, () => btc.btcScan(d, false));
d.sta = { port: 0, macId: 0 };
await scenario('role start', d, () => btc.btcRole(d, 'start'));
await scenario('connect start', d, () => btc.btcConnect(d, true));
await scenario('connect end', d, () => btc.btcConnect(d, false));
for (const kind of ['arp', 'icmp', 'eapol', 'eapolEnd', 'dhcp']) await scenario(`packet ${kind}`, d, () => btc.btcPacket(d, kind));
await scenario('act1 work', d, () => btc.btcAct1Work(d));
await scenario('act1 work again', d, () => btc.btcAct1Work(d));
// the track work's station notification: RSSI levels, traffic, rates
const stats = (tx, rx, txTp = 0, rxTp = 0) => ({ txTfcLv: tx, rxTfcLv: rx, txThroughput: txTp, rxThroughput: rxTp });
const link = (avgRssi, st, txHwRate = 0, rxHwRate = 0) => ({ port: 0, avgRssi, stats: st, txHwRate, rxHwRate });
await scenario('wl_sta no link', d, () => btc.btcWlSta(d, null));
await scenario('wl_sta rssi 80 idle', d, () => btc.btcWlSta(d, link(160, stats(0, 0))));
await scenario('wl_sta rssi 55 rx', d, () => btc.btcWlSta(d, link(110, stats(0, 2, 0, 900), 0, 0x10b)));
await scenario('wl_sta rssi 55 rx again', d, () => btc.btcWlSta(d, link(111, stats(0, 2, 0, 950), 0, 0x10b)));
await scenario('wl_sta rssi 35 tx', d, () => btc.btcWlSta(d, link(70, stats(3, 1, 4000, 10), 0, 0x10b)));
await scenario('wl_sta rssi 41 tx', d, () => btc.btcWlSta(d, link(83, stats(3, 1, 4000, 10), 0, 0x10b)));
await scenario('wl_sta rssi 42 tx', d, () => btc.btcWlSta(d, link(84, stats(3, 1, 4000, 10), 0, 0x10b)));
await scenario('wl_sta rssi 43 idle', d, () => btc.btcWlSta(d, link(87, stats(0, 0), 0, 0)));
await scenario('packet dhcp after traffic', d, () => btc.btcPacket(d, 'dhcp'));
for (let i = 0; i < 21; i++) await scenario(`wl_sta quiet ${i}`, d, () => btc.btcWlSta(d, link(87, stats(0, 0), 0, 0)));
await scenario('wl_sta other port', d, () => btc.btcWlSta(d, { ...link(87, stats(1, 1)), port: 3 }));
await scenario('role disconnect', d, () => btc.btcRole(d, 'disconnect'));
await scenario('wl_sta after disconnect', d, () => btc.btcWlSta(d, link(87, stats(0, 1), 0, 0)));
await scenario('role stop', d, () => btc.btcRole(d, 'stop'));
await scenario('role port 6', d, async () => { d.sta.port = 6; await btc.btcRole(d, 'connStart'); d.sta.port = 0; });
await scenario('scoreboard read', d, async () => ret(await btc.btcBtScoreboard(d)));
await scenario('radio off', d, () => btc.btcRadioOff(d));
await scenario('power off', d, () => btc.btcPowerOff(d));
await scenario('scan after power off', d, () => btc.btcScan(d, true));
await scenario('packet after power off', d, () => btc.btcPacket(d, 'dhcp'));
await scenario('radio on after power off', d, () => btc.btcRadioOn(d));
await scenario('band switch after radio on', d, () => btc.btcSwitchBand(d));

// ---- C2Hs: the BT side and the firmware's reports ----
const c2h = (dev, func, payload, { cat = 2, cls = 0x12 } = {}) => ret(btc.btcC2h(dev, { cat, cls, func, payload: Buffer.from(payload) }));
await scenario('c2h bt scoreboard off', d, () => c2h(d, 2, [0, 0, 0, 0]));
await scenario('c2h bt scoreboard on (2nd BT)', d, () => c2h(d, 0x82, [2, 0, 0, 0x40]));
await scenario('c2h bt scoreboard short', d, () => c2h(d, 2, [2, 0, 0]));
await scenario('c2h bt info', d, () => c2h(d, 1, [1, 2, 3, 4, 5]));
await scenario('c2h other class', d, () => c2h(d, 2, [2, 0, 0, 0], { cls: 0x10 }));
await scenario('c2h other category', d, () => c2h(d, 2, [2, 0, 0, 0], { cat: 1 }));
await scenario('c2h other event', d, () => c2h(d, 5, [2, 0, 0, 0]));

const RAW = { CTRL: 0, TDMA: 1, SLOT: 2, CYSTA: 3, STEP: 4, MREG: 6, GPIO: 7, TEST: 12, BIG: 40 };
const rptOf = (type, c, len = c.length) => { const b = Buffer.alloc(3 + c.length); b[0] = type; b.writeUInt16LE(len, 1); c.copy(b, 3); return b; };
const ctrlRpt = (en, fwc = 0, fwv = 0) => { const c = Buffer.alloc(80); c[0] = 5; c.writeUInt32LE(fwc, 24); c.writeUInt32LE(fwv, 28); c.writeUInt32LE(en, 32); return c; };
const mregRpt = (v4688, v980, num = 16) => { const c = Buffer.alloc(84); c[0] = 2; c[1] = num; c.writeUInt32LE(v980, 4 + 4 * 12); c.writeUInt32LE(v4688 >>> 0, 4 + 4 * 14); return c; };
const tdmaRpt = (t) => Buffer.concat([Buffer.from([3, 0, 0, 0]), Buffer.from(t)]);
const rpt = (dev, ...reports) => ret(btc.btcC2h(dev, { cat: 2, cls: 0x12, func: 0, payload: Buffer.concat(reports) }));
let agcReg;
const m = newDev((k, a) => CMAC_ON(k, a) ?? (k === 'mmio' && a === BB + 0x4688 ? agcReg : undefined));
await scenario('m power on + init', m, async () => { btc.btcPowerOn(m); await btc.btcInit(m); });
await scenario('m ctrl + mreg', m, () => rpt(m, rptOf(RAW.CTRL, ctrlRpt(4, 0x1d1d0012, 0x001d1d12)), rptOf(RAW.MREG, mregRpt(0x1a000000, 0))));
await scenario('m scan', m, () => btc.btcScan(m, true));
await scenario('m band switch', m, () => btc.btcSwitchBand(m));
await scenario('m mreg on', m, () => rpt(m, rptOf(RAW.MREG, mregRpt(0x80000000, 0))));
await scenario('m eapol', m, () => btc.btcPacket(m, 'eapol'));
await scenario('m mreg on again', m, () => rpt(m, rptOf(RAW.MREG, mregRpt(0x80000000, 0))));
agcReg = 0x80123456;
await scenario('m act1 register on', m, () => btc.btcAct1Work(m));
agcReg = undefined;
await scenario('m ctrl wrong length', m, () => rpt(m, rptOf(RAW.CTRL, ctrlRpt(0x7f), 79), rptOf(RAW.MREG, mregRpt(0x1a000000, 0))));
await scenario('m unknown type', m, () => rpt(m, rptOf(RAW.TEST, Buffer.alloc(4)), rptOf(RAW.MREG, mregRpt(0x1a000000, 0))));
await scenario('m type past the max', m, () => rpt(m, rptOf(RAW.BIG, Buffer.alloc(4))));
await scenario('m ctrl map 0x7f', m, () => rpt(m, rptOf(RAW.CTRL, ctrlRpt(0x7f))));
await scenario('m dhcp after the map', m, () => btc.btcPacket(m, 'dhcp'));
await scenario('m mreg gnt mux', m, () => rpt(m, rptOf(RAW.MREG, mregRpt(0x1a000000, 1 << 17))));
await scenario('m mreg few entries', m, () => rpt(m, rptOf(RAW.MREG, mregRpt(0x80000000, 0, 13))));
await scenario('m mreg none', m, () => rpt(m, rptOf(RAW.MREG, mregRpt(0x80000000, 0, 0))));
await scenario('m header past the end', m, () => rpt(m, rptOf(RAW.MREG, mregRpt(0x1a000000, 0)), Buffer.from([6, 0x54])));
await scenario('m length past the end', m, () => rpt(m, rptOf(RAW.MREG, mregRpt(0x1a000000, 0)).subarray(0, 50)));
await scenario('m other sizes', m, () => rpt(m, rptOf(RAW.CYSTA, Buffer.alloc(1384)), rptOf(RAW.STEP, Buffer.alloc(208)),
  rptOf(RAW.GPIO, Buffer.alloc(16)), rptOf(RAW.MREG, mregRpt(0x1a000000, 0))));
for (let i = 0; i < 3; i++) await scenario(`m tdma differs ${i}`, m, () => rpt(m, rptOf(RAW.TDMA, tdmaRpt([1, 0, 0, 0, 0, 0, 0, 0]))));
await scenario('m dhcp forced', m, () => btc.btcPacket(m, 'dhcp'));
await scenario('m tdma in sync', m, () => rpt(m, rptOf(RAW.TDMA, tdmaRpt(m.btc.dm.tdmaNow))));
const slots = Buffer.alloc(8 + 8 * 18); slots[0] = 1; slots[1] = 18;
const slot = (dur, tbl, type) => { const b = Buffer.alloc(8); b.writeUInt16LE(dur, 0); b.writeUInt32LE(tbl >>> 0, 2); b.writeUInt16LE(type, 6); return b; };
m.btc.dm.slotNow.forEach((s, i) => slot(...s).copy(slots, 8 + 8 * i));
for (let i = 0; i < 3; i++) await scenario(`m slot in sync ${i}`, m, () => rpt(m, rptOf(RAW.SLOT, slots)));
slots[8] ^= 1;
for (let i = 0; i < 3; i++) await scenario(`m slot differs ${i}`, m, () => rpt(m, rptOf(RAW.SLOT, slots)));
await scenario('m two reports', m, () => rpt(m, rptOf(RAW.SLOT, slots), rptOf(RAW.TDMA, tdmaRpt([0, 0, 0, 1, 0, 0, 0, 0]))));
await scenario('m act1 forced', m, () => btc.btcAct1Work(m));

// ---- role v2 layout: ports 16 bytes apart, port 3 under the mrole words, port 4 past the end ----
const r5 = newDev();
await scenario('r5 init', r5, async () => { btc.btcPowerOn(r5); await btc.btcInit(r5); });
{
  const rr = r5.btc.wl.role;
  rr.rlink[1] = { connected: 1, pid: 1, phy: 0, noa: 1, rfBand: 1, clientPs: 1, bw: 2, role: 1, ch: 40, noaDur: 0x11223344 };
  rr.rlink[3] = { connected: 1, pid: 3, phy: 1, noa: 0, rfBand: 2, clientPs: 0, bw: 1, role: 2, ch: 0x33, noaDur: 0x55667788 };
  rr.rlink[4] = { connected: 1, pid: 4, phy: 1, noa: 1, rfBand: 1, clientPs: 1, bw: 3, role: 3, ch: 0x44, noaDur: 0x99aabbcc };
  rr.mroleType = 0x0a0b0c0d; rr.mroleNoaDuration = 0x01020304;
}
await scenario('r5 role layout', r5, async () => { await h2cCxRole(r5); });

// ---- a 5 GHz association ----
const f5 = newDev();
await scenario('f5 init', f5, async () => { btc.btcPowerOn(f5); await btc.btcInit(f5); });
f5.chan = { band: '5g', ch: 36, bw: 20 };
f5.sta = { port: 0, macId: 3 };
await scenario('f5 role start', f5, () => btc.btcRole(f5, 'start'));
await scenario('f5 connect', f5, async () => { await btc.btcConnect(f5, true); await btc.btcConnect(f5, false); });
await scenario('f5 phy map', f5, () => ret(btc.btcPhyMap(f5, 0, 3)));
await scenario('f5 scan', f5, async () => { await btc.btcScan(f5, true); await btc.btcScan(f5, false); });
f5.chan = { band: '5g', ch: 149, bw: 20 };
await scenario('f5 connect 149', f5, async () => { await btc.btcConnect(f5, true); await btc.btcConnect(f5, false); });
f5.chan = { band: '5g', ch: 100, bw: 20 };
await scenario('f5 connect 100', f5, async () => { await btc.btcConnect(f5, true); await btc.btcConnect(f5, false); });
f5.chan = { band: '2g', ch: 11, bw: 20 };
f5.sta = { port: 1, macId: 4 };
await scenario('f5 second port on 2.4 GHz', f5, async () => { await btc.btcConnect(f5, true); await btc.btcConnect(f5, false); });

// ---- the chip ops not reached on this path ----
const t = newDev();
btc.btcPowerOn(t);
await scenario('txpwr ctrl', t, () => { for (const v of [0xffffffff, 0xffff0010, 0x0010ffff, 0x01ff01ff, 0x0100fe00]) hw.btcSetWlTxpwrCtrl(t, v); });
await scenario('rx gain', t, () => { for (const v of [2, 0, 1, 3]) hw.btcSetWlRxGain(t, v); });
await scenario('s1 standby', t, () => { hw.btcWlS1Standby(t, 1); hw.btcWlS1Standby(t, 0); });
await scenario('bt rssi', t, () => [`= rssi ${[-10, 0, -120, -106, -105, 120, 127, 200].map(hw.btcGetBtRssi).join(',')}`]);
await scenario('wl pri', t, () => { for (const m of [1, 2, 3, 0, 4]) { hw.btcSetWlPri(t, m, true); hw.btcSetWlPri(t, m, false); } });
await scenario('lte read', t, async () => ret(await hw.readLte(t, 0x38)));
await scenario('preserve with a2dp', t, () => { t.btc.bt0.a2dp = 1; btc.btcPreserveBtTime(t, 30); t.btc.bt0.a2dp = 0; });

// ---- guards ----
const c = newDev(() => undefined);
await scenario('cmac off init', c, async () => { btc.btcPowerOn(c); await btc.btcInit(c); });
const w = newDev((k, a, s) => CMAC_ON(k, a) ?? (k === 'mmio' && a === 0x73 && s === 1 ? 0x04 : undefined));
await scenario('pta owner warning', w, async () => { btc.btcPowerOn(w); await btc.btcInit(w); });
const b = newDev();
btc.btcPowerOn(b);
b.btc.bt0.scbdC2h = 1 << 1;
await scenario('bt on stops the init', b, () => btc.btcInit(b));
const b2 = newDev();
btc.btcPowerOn(b2);
b2.btc.bt0.scbdC2h = 0xffffffff;
await scenario('scoreboard all ones', b2, () => btc.btcInit(b2));
await scenario('scoreboard all ones band switch', b2, () => btc.btcSwitchBand(b2));
const wo = newDev();
btc.btcPowerOn(wo);
await scenario('wl only init', wo, () => btc.btcInit(wo, R('BTC_MODE_WL')));
await scenario('wl only band switch', wo, () => btc.btcSwitchBand(wo));
await scenario('wl only radio off', wo, () => btc.btcRadioOff(wo));
for (const mode of [2, 3, 4]) {
  const x = newDev();
  btc.btcPowerOn(x);
  await scenario(`mode ${mode} refused`, x, () => btc.btcInit(x, mode));
}
const ded = newDev(CMAC_ON, { rfe: 2 });
btc.btcPowerOn(ded);
await scenario('dedicated antenna refused', ded, () => btc.btcInit(ded));
const r0 = newDev(CMAC_ON, { rfe: 0 });
btc.btcPowerOn(r0);
await scenario('rfe 0 init', r0, () => btc.btcInit(r0));
for (const ver of ['0.29.28.255', '0.29.29.0', '0.29.121.255', '0.29.122.0']) {
  const o = newDev(CMAC_ON, { ver });
  await scenario(`firmware ${ver}`, o, () => { btc.btcPowerOn(o); });
}
const po = newDev();
await scenario('po init', po, async () => { btc.btcPowerOn(po); await btc.btcInit(po); });
await scenario('po power off without radio off', po, () => btc.btcPowerOff(po));
await scenario('po radio on', po, () => btc.btcRadioOn(po));
await scenario('po band switch', po, () => btc.btcSwitchBand(po));
const early = newDev();
btc.btcPowerOn(early);
await scenario('scan before init', early, () => btc.btcScan(early, true));
await scenario('dack before init', early, () => btc.btcWlRfk(early, 0x10, R('BTC_WRFKT_DACK'), R('BTC_WRFK_START')));

process.stdout.write(out.join('\n') + '\n');
