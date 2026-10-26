// macinit-js.mjs: the JS driver's own macinit.mjs (macinit-io/-trx/-imr/-port/-h2c.mjs,
// with the real fw.mjs, pci.mjs, dmac.mjs) on the kit's FakeDev, scenario by scenario
// (test-macinit.mjs's and test-macinit-probe.mjs's), as text: `## <scenario>`, then every
// op as a bringup-golden.txt line, `= ...` lines for results and state, `! error <kind>`
// where one failed. Plus `## spec ...`: the register sequences of the C as
// macinit-spec.mjs transcribes them from Linux's mac.c / rtw8852b.c (with the C's names,
// values read from the headers), as golden lines.
// tests/macinit_js.rs runs the same scenarios on the Rust side (HookBus with the same chip
// answers) and compares the text line by line.
//   node macinit-js.mjs            (WLAN_TOOLS: the read-only WLAN tree)
// Reads only; writes nothing but stdout.
import { readFileSync } from 'node:fs';
import { pathToFileURL } from 'node:url';

const WZ = process.env.WLAN_TOOLS ?? '/home/georg/BitsOS/werkzeuge/plattform/wlan';
const imp = (rel) => import(pathToFileURL(`${WZ}/${rel}`).href);
// macinit-spec.mjs imports the fw stand-in's mark constants; only it gets the stand-in
const { useStubs } = await imp('js/stubs/hook.mjs');
useStubs(/\/macinit-spec\.mjs$/, { './fw.mjs': './stubs/macinit-fw.mjs' });
const { FakeDev, R, prep, lowbit, RtwError } = await imp('kit/rtwdev.mjs');
const { hciDmaTrx, dmacPreInit } = await imp('js/dmac.mjs');
const { pciAlloc, pciPreInit } = await imp('js/pci.mjs');
const fwm = await imp('js/fw.mjs');
const mi = await imp('js/macinit.mjs');
const { checkMacEn, MacError } = await imp('js/macinit-io.mjs');
const spec = await imp('js/macinit-spec.mjs');
const { FwError } = fwm;

const KIND = { 1: 'wr', 2: 'rd', 3: 'rmw', 4: 'poll', 5: 'delay', 6: 'mwr', 7: 'mrd', 8: 'mcpy', 9: 'cfgr', 10: 'cfgw', 11: 'mpoll', 12: 'mark', 13: 'now' };
const HEX = (v) => `0x${(v >>> 0).toString(16).toUpperCase().padStart(8, '0')}`;
const hx = (v) => `0x${(v >>> 0).toString(16).padStart(8, '0')}`;
const line = ([k, s, t, a, b, c]) =>
  `${KIND[k]}${k <= 4 || k === 6 || k === 7 ? s * 8 : ''} ${HEX(a)} ${HEX(b)} ${HEX(c)}${k === 4 || k === 11 ? ` ${t} ms` : ''}`;
// a spec op [kind, size, a, b, c, ms] (macinit-spec.mjs `want`) as a golden line
const specLine = ([k, s, a, b, c, ms]) => line([k, s, ms ?? 0, a, b, c]);
const out = [];
const file = readFileSync(`${WZ}/firmware/rtw8852b_fw-2.bin`);
const fw = fwm.fwParse(file, { cv: 1 });

// the kind of an error, as tests/macinit_js.rs names the Rust one
function kind(e) {
  if (e instanceof MacError) return 'mac';
  if (e instanceof FwError) return 'fw';
  if (e instanceof RtwError) return 'device';
  return 'other';
}

// test-macinit `fake`: config space with the PCIe capability at 0x70 and the link at gen 1,
// the BA CAM reset done, all public pages free; `extra(a)` answers first
function fake(extra = () => undefined) {
  const logs = [];
  const d = new FakeDev({ log: (s) => logs.push(s), onRead: (kind, a, s) => {
    if (kind === 'cfg') return { 0x04: 0x00100002, 0x34: 0x70, 0x70: 0x00020010, 0x80: 0x10110000 }[a];
    const v = extra(a);
    if (v !== undefined) return v;
    if (a === R('R_AX_RESPBA_CAM_CTRL')) return 0;
    if (a === R('R_AX_PUB_PAGE_INFO2')) return 446;
    return undefined;
  } });
  d.logs = logs;
  d.st = { cv: 1, acv: 1, rfe: 1, efuse: { valid: true, powerKValid: false } };
  d.mac = { qtaMode: 'SCC', dleInfo: {}, hfcParam: {} };
  return d;
}
async function enabled(d) {
  d.w32(R('R_AX_DMAC_FUNC_EN'), R('B_AX_MAC_FUNC_EN') | R('B_AX_DMAC_FUNC_EN'));
  d.w32(R('R_AX_CMAC_FUNC_EN'), R('B_AX_CMAC_EN'));
  await d.flush();
  d.trace.length = 0;
}
// fw-js.mjs `ready`: the rings and the partial init up to the download, so H2Cs can go on CH12
async function ready(d) {
  d.set32(R('R_AX_DMAC_FUNC_EN'), R('B_AX_MAC_FUNC_EN') | R('B_AX_DMAC_FUNC_EN'));
  hciDmaTrx(d, true); await dmacPreInit(d); await pciAlloc(d); await pciPreInit(d);
  await d.flush();
  d.writes = [];
  const mw = d.memWrite.bind(d);
  d.memWrite = async (a, b) => { d.writes.push([a, Buffer.from(b)]); return mw(a, b); };
  return d;
}

const pw = (v) => (v === true ? '1' : v === false ? '0' : '-');
// what the MAC part logged (its own lines only), up to the first parenthesis
const logs = (d) => d.logs.filter((l) => l.startsWith('macinit:')).map((l) => `= log ${l.split(' (')[0]}`);
const state = (d) => [`= power ${pw(d.mac?.powerOn)} ready ${+!!d.fw?.ready}`, ...logs(d)];

// one scenario: its ops since `d`'s mark, then what fn returned (or the error's kind)
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
    if (process.env.MACINIT_JS_VERBOSE) console.error(name, e.stack);
  }
  for (const op of d.trace.slice(t0)) out.push(line(op));
  out.push(...res);
}
function specScenario(name, specs, vars = {}) {
  out.push(`## spec ${name}`);
  for (const s of specs) out.push(specLine(spec.want(s, vars)));
}

// ---- the C's sequences (macinit-spec.mjs) ----
const NO_LINK = R('RTW89_NET_TYPE_NO_LINK'), INFRA = R('RTW89_NET_TYPE_INFRA');
const TXEN = R('B_AX_CTN_TXEN_ALL_MASK');
specScenario('sys_init', spec.SYS_INIT);
specScenario('enable_bb_rf', spec.ENABLE_BB_RF);
specScenario('dmac_init tail', spec.DMAC_TAIL);
specScenario('cmac_init', spec.CMAC_INIT, { rx_fltr: 'DEFAULT_AX_RX_FLTR', rx_max_len: 22 });
specScenario('trx_init tail', spec.TRX_TAIL);
for (const port of [0, 1, 2, 3]) {
  specScenario(`port_update ${port} no link`, spec.portUpdateSpecs(port, NO_LINK),
    { net_type: NO_LINK, beacon_int: 0, dtim_period: 0, bss_color: 0 });
}
specScenario('port_update 1 infra func_sw', spec.portUpdateSpecs(1, INFRA, { funcSw: true }),
  { net_type: INFRA, beacon_int: 102, dtim_period: 3, bss_color: 5 });
specScenario('vif tables 3', spec.VIF_TABLES, { macid: 3 });
specScenario('core tail', spec.CORE_TAIL);
specScenario('sch tx h2c', spec.SCH_TX_H2C, { tx_en: 0x1234, mask: TXEN, n: 1 });

// ---- power switch ----
{
  const d = fake();
  await scenario('preinit', d, async () => { await mi.macPreinit(d); return state(d); });
  const v = fake();
  v.st.efuse.valid = false;
  await scenario('preinit efuse invalid', v, async () => { await mi.macPreinit(v); return state(v); });
  let reads = 0;
  const a = fake((x) => (x === R('R_AX_IC_PWR_STATE') && reads++ === 0 ? 0x100 : undefined));
  a.st.efuse.powerKValid = true;
  a.fw = { ready: true };
  await scenario('preinit mac already on', a, async () => { await mi.macPreinit(a); return state(a); });
  const fails = new Set([R('B_AX_APFN_ONMAC'), R('B_AX_APFM_OFFMAC')]);
  const r = fake();
  r.onPoll = (k, x, mask) => !(x === R('R_AX_SYS_PW_CTRL') && fails.delete(mask));
  await scenario('preinit retry, failing power off', r, async () => { await mi.macPreinit(r); return state(r); });
  const p = fake();
  p.fw = { ready: true };
  p.mac.powerOn = true;
  await scenario('power off', p, async () => { await mi.macPwrOff(p); return state(p); });
  const f = fake((x) => (x === R('R_AX_EFUSE_CTRL') ? (R('B_AX_EF_RDY') | 0xff) >>> 0 : undefined));
  f.st = { cv: 1, acv: 1 };
  let once = true;
  f.onPoll = (k, x, mask) => !(x === R('R_AX_SYS_PW_CTRL') && mask === R('B_AX_APFN_ONMAC') && once && !(once = false));
  await scenario('probe power on with a retry', f, async () => { await mi.macPwrOnProbe(f); return [`= secure ${+!!f.st.secureBoot}`, ...state(f)]; });
  const s = fake((x) => (x === R('R_AX_EFUSE_CTRL') ? (R('B_AX_EF_RDY') | 0x01) >>> 0 : undefined));
  s.st = { cv: 1, acv: 1 };
  await scenario('probe power on, secure boot bytes', s, async () => { await mi.macPwrOnProbe(s); return [`= secure ${+!!s.st.secureBoot}`]; });
  const n = fake();
  n.st = { cv: 1, acv: 1 };
  n.onPoll = (k, x, mask) => !(x === R('R_AX_EFUSE_CTRL') && mask === R('B_AX_EF_RDY'));
  await scenario('probe power on, efuse never ready', n, async () => { await mi.macPwrOnProbe(n); return [`= secure ${+!!n.st.secureBoot}`, ...state(n)]; });
}

// ---- sys/dmac/cmac/trx init ----
{
  const d = fake();
  await scenario('sys_init', d, async () => { mi.sysInit(d); });
  await scenario('enable_bb_rf', d, async () => { mi.enableBbRf(d); });
  const m = fake();
  await enabled(m);
  await scenario('dmac_init', m, async () => { await mi.dmacInit(m); });
  const c = fake();
  await enabled(c);
  c.mac.rxFltr = mi.defaultRxFltr();
  c.mac.dleInfo = { plePgSize: 128, c0RxQta: 89 };
  await scenario('cmac_init', c, async () => { await mi.cmacInit(c); return logs(c); });
  const t = fake();
  await enabled(t);
  t.mac.rxFltr = mi.defaultRxFltr();
  await scenario('trx_init', t, async () => { await mi.trxInit(t); });
}

// ---- rtw89_mac_init ----
{
  const d = fake();
  await pciAlloc(d);
  await scenario('mac_init', d, async () => { await mi.macInit(d, { fw }); return [`= seq ${d.fw.h2cSeq}`, ...state(d)]; });
  const f = fake();
  await pciAlloc(f);
  f.onPoll = (k, a) => !(k === 'mmio' && a === R('R_AX_SS_CTRL'));
  await scenario('mac_init, sta sch never ready', f, async () => {
    try { await mi.macInit(f, { fw }); } catch (e) { await f.flush(); return [`! error ${kind(e)}`, ...state(f)]; }
    return ['= no error'];
  });
  const b = fake((a) => (a === R('R_AX_PUB_PAGE_INFO2') ? 445 : undefined));
  await pciAlloc(b);
  await enabled(b);
  await scenario('mac_init, public pages', b, async () => {
    try { await mi.macInit(b, { fw, preinit: false }); } catch (e) { await b.flush(); return [`! error ${kind(e)}`, ...state(b)]; }
    return ['= no error'];
  });
}

// ---- vif init and the port update ----
const HOOK_MARK = 0x700;
function hooks(d) {
  const calls = [];
  const hook = (name) => async (...args) => { d.w32(HOOK_MARK + 4 * calls.length, 1); calls.push([name, ...args].join(' ')); };
  return { calls, hooks: Object.fromEntries(['macidPause', 'roleMaintain', 'joinInfo', 'camInit', 'addrCam',
    'defaultCmacTbl'].map((n) => [n, hook(n)])) };
}
for (const [port, macId] of [[0, 0], [1, 3]]) {
  const d = fake();
  await enabled(d);
  const { calls, hooks: h } = hooks(d);
  await scenario(`vif_init port ${port} macid ${macId}`, d, async () => {
    await mi.vifInit(d, { macId, port, addr: Buffer.from('02b175000001', 'hex') }, h);
    return calls.map((c) => `= hook ${c}`);
  });
}
{
  const cfg1 = R('R_AX_PORT_CFG_P0') + 0x40;
  const a = fake((r) => (r === cfg1 ? R('B_AX_PORT_FUNC_EN') : undefined));
  await scenario('port_update infra port 1 running', a, async () => {
    await mi.portUpdate(a, { macId: 3, port: 1, netType: INFRA, beaconInt: 102, dtimPeriod: 3, bssColor: 5 });
  });
  for (const port of [2, 3, 4]) {
    const p = fake();
    await scenario(`port_update ad hoc port ${port}`, p, async () => {
      await mi.portUpdate(p, { macId: 1, port, netType: R('RTW89_NET_TYPE_AD_HOC'), beaconInt: 200, dtimPeriod: 1, bssColor: 9,
        nontransmitted: true, bssidIndex: 2 });
    });
  }
  const x = fake();
  await scenario('port_update port 5', x, () => mi.portUpdate(x, { port: 5 }));
  const ap = fake((r) => (r === R('R_AX_PORT_CFG_P0') ? R('B_AX_PORT_FUNC_EN') : undefined));
  await scenario('port_update running AP port', ap, () => mi.portUpdate(ap, { port: 0, netType: R('RTW89_NET_TYPE_AP_MODE') }));
}

// ---- the MACID pause H2C, the core start tail ----
const h2cBytes = (d, from) => d.writes.slice(from).map(([, b]) => `= h2c ${b.subarray(24).toString('hex')}`);
{
  const d = await ready(fake());
  await enabled(d);
  await scenario('macid pause 37', d, async () => { await mi.macidPause(d, 37, true); return h2cBytes(d, 0); });
  let from = d.writes.length;
  await scenario('macid unpause 2', d, async () => { await mi.macidPause(d, 2, false); return h2cBytes(d, from); });
  from = d.writes.length;
  const order = [];
  await scenario('core start tail', d, async () => {
    await mi.coreStartTail(d, { radioOn: async (x) => { order.push('radio'); x.w32(0xB7C, 1); },
      trackStart: () => order.push('track') });
    return [`= order ${order.join(' ')}`, ...h2cBytes(d, from), `= seq ${d.fw.h2cSeq}`];
  });
  await scenario('ppdu status off', d, () => mi.cfgPpduStatus(d, false));
}

// ---- check_mac_en ----
for (const [sel, v] of [['dmac', 0xEAEAEAEA], ['dmac', R('B_AX_MAC_FUNC_EN')], ['dmac', R('B_AX_MAC_FUNC_EN') | R('B_AX_DMAC_FUNC_EN')],
  ['cmac', 0xDEADBEEF], ['cmac', 0], ['cmac', R('B_AX_CMAC_EN')]]) {
  const d = fake((a) => (a === R('R_AX_DMAC_FUNC_EN') || a === R('R_AX_CMAC_FUNC_EN') ? v : undefined));
  await scenario(`check_mac_en ${sel} ${hx(v)}`, d, async () => { await checkMacEn(d, sel); return ['= ok']; });
}

// ---- sch TX ----
function schFake({ id = R('RTW89_FWCMD_C2HREG_FUNC_TX_PAUSE_RPT'), cmac = R('B_AX_CMAC_EN') } = {}) {
  const d = fake((a) => {
    if (a === R('R_AX_CTN_TXEN')) return 0x1234;
    if (a === R('R_AX_CMAC_FUNC_EN')) return cmac;
    if (a === R('R_AX_C2HREG_CTRL')) return 1;
    if (a === R('R_AX_C2HREG_DATA0')) return (id | (2 << 8)) >>> 0;
    return undefined;
  });
  d.fw = { ready: true, h2cCounter: 0, c2hCounter: 0 };
  return d;
}
const schErr = (d) => [`= err ${d.mac.schTxErr ? kind(d.mac.schTxErr) : '-'}`, ...logs(d)];
{
  const d = schFake();
  await scenario('stop sch tx all', d, async () => [`= tx_en ${hx(await mi.stopSchTx(d))}`, ...schErr(d)]);
  await scenario('resume sch tx', d, async () => { await mi.resumeSchTx(d, 0x1234); return schErr(d); });
  for (const sel of ['hiq', 'mg0', 'macid']) {
    const s = schFake();
    await scenario(`stop sch tx ${sel}`, s, async () => [`= tx_en ${hx(await mi.stopSchTx(s, sel))}`, ...schErr(s)]);
  }
  const w = schFake({ id: R('RTW89_FWCMD_C2HREG_FUNC_NULL') });
  await scenario('stop sch tx, wrong c2h', w, async () => [`= tx_en ${hx(await mi.stopSchTx(w))}`, ...schErr(w)]);
  const b = schFake();
  b.onPoll = (k, a) => !(k === 'mmio' && a === R('R_AX_H2CREG_CTRL'));
  await scenario('stop sch tx, h2c register busy', b, async () => [`= tx_en ${hx(await mi.stopSchTx(b))}`, ...schErr(b)]);
  const c = schFake({ cmac: 0xDEADBEEF });
  await scenario('resume sch tx, cmac off', c, async () => { await mi.resumeSchTx(c, 0x1234); return schErr(c); });
  const n = schFake();
  await mi.macPwrOff(n);
  await scenario('resume sch tx before fw', n, async () => { await mi.resumeSchTx(n, 0x1234); return schErr(n); });
  await scenario('stop sch tx hiq before fw', n, async () => [`= tx_en ${hx(await mi.stopSchTx(n, 'hiq'))}`, ...schErr(n)]);
}

// ---- configure_filter ----
{
  const d = fake();
  d.mac.rxFltr = (mi.defaultRxFltr() | R('B_AX_SNIFFER_MODE')) >>> 0;
  const steps = [[{ allmulti: true }, false], [{}, false], [{ fcsfail: true }, false], [{ otherBss: true }, false],
    [{ probeReq: true }, false], [{ bcnPrbrespPromisc: true }, true], [{}, true], [{}, false],
    [{ allmulti: true, fcsfail: true, otherBss: true, bcnPrbrespPromisc: true, probeReq: true }, true],
    [{ allmulti: true, probeReq: true }, false]];
  for (const [flags, scanning] of steps) {
    const name = Object.keys(flags).sort().join(',') || 'none';
    await scenario(`rx filter ${name}${scanning ? ' scanning' : ''}`, d, async () => {
      const rx = await mi.rxFilter(d, flags, { scanning });
      return [`= rx ${hx(rx)} hal ${hx(d.mac.rxFltr)}`];
    });
  }
}

process.stdout.write(out.join('\n') + '\n');
