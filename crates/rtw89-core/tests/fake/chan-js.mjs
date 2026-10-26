// chan-js.mjs: the JS driver's own chan.mjs (chan-bb/-txpwr/-path/-fw/-efuse/-regd.mjs, with
// phy-bbx.mjs and phy-tbl.mjs's BB gain walk) on test-chan.mjs's device (chan-test-expect.mjs
// dev: FakeDev with the chip facts, the real firmware file and efuse dump), with test-chan.mjs's
// stand-ins of macinit, rfk1 and btc (here as stubs/chan-*.mjs, which also note where each call
// happened). Scenario by scenario, the JS side of test-chan.mjs (which checks the same module
// against sequences written from the Linux C), as text: `## <scenario>`, every op as a
// bringup-golden.txt line, the neighbours' calls as `= call ...` lines where they happened, then
// results and the channel state as `= ...` lines. `## inputs` prints what the module reads from
// the firmware file and the efuse: the TX power configurations taken, the REGD map, the efuse
// gain and trim values, every loaded TX power table and the limits read from them for four
// countries. tests/chan_js.rs runs the same on the Rust side (rfk1::fake::RfFake set up as dev)
// and compares the text line by line.
//   node chan-js.mjs            (WLAN_TOOLS: the read-only WLAN tree)
// Reads only; writes nothing but stdout.
import { pathToFileURL } from 'node:url';

const WZ = process.env.WLAN_TOOLS ?? '/home/georg/BitsOS/werkzeuge/plattform/wlan';
const imp = (rel) => import(pathToFileURL(`${WZ}/${rel}`).href);
const { useStubs } = await imp('js/stubs/hook.mjs');
useStubs(/\/chan(-[a-z0-9-]+)?\.mjs$/, {
  './macinit.mjs': new URL('stubs/chan-macinit.mjs', import.meta.url).href,
  './rfk1.mjs': new URL('stubs/chan-rfk1.mjs', import.meta.url).href,
  './btc.mjs': new URL('stubs/chan-btc.mjs', import.meta.url).href,
});
const { R, BB, prep } = await imp('kit/rtwdev.mjs');
const C = await imp('js/chan.mjs');
const { setChannelBb, setGainOffset } = await imp('js/chan-bb.mjs');
const { readTxpwrLimit, readTxpwrLimitRu } = await imp('js/chan-txpwr.mjs');
const { recognizeTxpwr, loadRegdMap } = await imp('js/chan-fw.mjs');
const { parseGainOffset, parseGainComp, parsePowerTrim } = await imp('js/chan-efuse.mjs');
const { regdApply, chanState } = await imp('js/chan-regd.mjs');
const { dev, FW, EFUSE_LOG } = await imp('js/chan-test-expect.mjs');
const { synthBbGain, synthByrate } = await imp('js/chan-test-raw.mjs');

const KIND = { 1: 'wr', 2: 'rd', 3: 'rmw', 4: 'poll', 5: 'delay', 6: 'mwr', 7: 'mrd', 8: 'mcpy', 9: 'cfgr', 10: 'cfgw', 11: 'mpoll', 12: 'mark', 13: 'now' };
const HEX = (v) => `0x${(v >>> 0).toString(16).toUpperCase().padStart(8, '0')}`;
const line = ([k, s, t, a, b, c]) =>
  `${KIND[k]}${k <= 4 || k === 6 || k === 7 ? s * 8 : ''} ${HEX(a)} ${HEX(b)} ${HEX(c)}${k === 4 || k === 11 ? ` ${t} ms` : ''}`;
const H = (v) => `0x${(v >>> 0).toString(16)}`;
const B = (v) => (v ? '1' : '0');
const L = (a, f = (x) => `${x}`) => [...a].map(f).join(',');
const out = [];

// the channel state (struct rtw89_chan, rtw89_chan_rcd, entity, country)
function chanLine(d) {
  const c = chanState(d);
  return `= chan ${c.band} ${c.channel} ${c.primaryChannel} ${c.bandWidth} ${c.freq} ${c.subbandType} ${c.txCompBand} ` +
    `${c.priChIdx} ${c.priSbIdx} ${c.rfsiBand} rcd ${c.prevCh} ${c.prevBand} ${c.prevBandType} ${B(c.bandChanged)} ` +
    `active ${B(c.entityActive)} dm ${B(c.dmInitDone)} regd ${c.regd?.alpha2 ?? '-'}`;
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
    d.q = new d.q.constructor();
    if (process.env.CHAN_JS_VERBOSE) console.error(name, e.stack);
  }
  const ops = d.trace.slice(t0).map(line);
  const calls = d.calls.map(([n, pos, ...a]) => [pos - t0, `= call ${n}${a.map((x) => ` ${x}`).join('')}`]);
  for (let i = 0; i <= ops.length; i++) {
    for (const [pos, c] of calls) if (pos === i) out.push(c);
    if (i < ops.length) out.push(ops[i]);
  }
  out.push(...res, chanLine(d));
}

// test-chan.mjs chanFor: a struct rtw89_chan for the MAC and BB parts alone (d.chan untouched)
const subbandOf = (ch) => (ch <= 14 ? 0 : ch <= 64 ? 1 : ch <= 144 ? 3 : 4);
const chanFor = (x, ch) => ({ ...chanState(x), band: ch > 14 ? '5g' : '2g', ch, channel: ch, primaryChannel: ch,
  bandType: ch > 14 ? 1 : 0, subbandType: subbandOf(ch) });
const bandOf = (ch) => (ch > 14 ? '5g' : '2g');

// ---- inputs: what chan reads from the firmware file and the efuse ----
out.push('## inputs');
{
  const d = dev();
  const confs = recognizeTxpwr(d);
  const keys = ['byrate', 'lmt2g', 'lmt5g', 'lmt6g', 'daLmt2g', 'daLmt5g', 'daLmt6g', 'lmtRu2g', 'lmtRu5g', 'lmtRu6g',
    'daLmtRu2g', 'daLmtRu5g', 'daLmtRu6g', 'txShapeLmt', 'txShapeLmtRu'];
  out.push(`= confs ${keys.filter((k) => confs[k]).map((k) => `${k}=${confs[k].rfe}/${confs[k].entSz}/${confs[k].numEnts}`).join(' ')}`);
  const map = loadRegdMap(d);
  out.push(`= regd map ${map.length}`);
  for (const r of map) out.push(`= regd ${r.alpha2} ${L(r.txpwrRegd)} ${H(r.fmap)}`);
  const gain = (eg) => `${B(eg.offsetValid)} ${B(eg.compValid)} offset ${eg.offset.map((o) => L(o)).join(';')} ` +
    `comp ${eg.comp.map((o) => L(o)).join(';')}`;
  const none = Buffer.alloc(128, 0xff);
  out.push(`= gain ${gain({ ...parseGainOffset(EFUSE_LOG), ...parseGainComp(none) })}`);
  const glog = Buffer.from(EFUSE_LOG), gpc = Buffer.alloc(128, 0xff);
  glog[0x2d4] = 0x1e; glog[0x2d6] = 0x2f; glog[0x2d8] = 0xe1; glog[0x2da] = 0x90; glog[0x2dc] = 0x7f;
  gpc[0x5bb - 0x580] = 0x03; gpc[0x590 - 0x580] = 0x0e; gpc[0x5ba - 0x580] = 0x0d; gpc[0x58f - 0x580] = 0x02;
  gpc[0x5b9 - 0x580] = 0x8f; gpc[0x58d - 0x580] = 0x71;
  out.push(`= gain prog ${gain({ ...parseGainOffset(glog), ...parseGainComp(gpc) })}`);
  const trim = (t) => `${B(t.pgThermalTrim)} ${L(t.thermalTrim, H)} ${B(t.pgPaBiasTrim)} ${L(t.paBiasTrim, H)}`;
  const tpc = Buffer.alloc(128, 0xff);
  tpc[0x5df - 0x580] = 0x13; tpc[0x5dc - 0x580] = 0x0c; tpc[0x5de - 0x580] = 0x5a; tpc[0x5db - 0x580] = 0x21;
  out.push(`= trim ${trim(parsePowerTrim(none))} | ${trim(parsePowerTrim(tpc))}`);

  // every loaded table, one line per leading index tuple (all-zero lines left out)
  await C.setTxpwrCtrl(d);                                 // runs setup: the tables
  const t = d.chan.tables;
  out.push(`= rfe ${Object.entries(t.rfe).sort(([a], [b]) => keys.indexOf(a) - keys.indexOf(b)).map(([k, v]) => `${k}=${v}`).join(' ')} da ${B(t.hasDa)}`);
  for (let band = 0; band < 3; band++) {
    for (let bw = 0; bw < 5; bw++) {
      const h = t.byr[band][bw];
      const s = [L(h.cck), L(h.ofdm), h.mcs.map((o) => o.map((n) => L(n)).join('/')).join('|'),
        h.hedcm.map((o) => o.map((n) => L(n)).join('/')).join('|'), L(h.offset)].join(' ');
      out.push(`= byr ${band} ${bw} ${s}`);
    }
  }
  const dump = (name, tbl) => {
    const last = tbl.dims[tbl.dims.length - 1], rows = tbl.data.length / last;
    for (let r = 0; r < rows; r++) {
      const row = tbl.data.subarray(r * last, (r + 1) * last);
      if (!row.some(Boolean)) continue;
      const idx = [];
      for (let k = tbl.dims.length - 2, q = r; k >= 0; k--) { idx.unshift(q % tbl.dims[k]); q = Math.floor(q / tbl.dims[k]); }
      out.push(`= ${name} ${idx.join(',')}: ${L(row)}`);
    }
  };
  dump('lmt2g', t.lmt2g); dump('lmt5g', t.lmt5g); dump('lmtru2g', t.lmtRu2g); dump('lmtru5g', t.lmtRu5g);
  dump('shape', t.txShapeLmt); dump('shaperu', t.txShapeLmtRu);

  // the limits as rtw89_phy_read_txpwr_limit(_ru) read them, MAC units, for four countries
  const chans = [];
  for (let ch = 1; ch <= 14; ch++) chans.push([0, ch]);
  for (const [a, b] of [[36, 64], [100, 144], [149, 177]]) for (let ch = a; ch <= b; ch++) chans.push([1, ch]);
  for (const cc of ['DE', 'TV', 'GB', 'XX']) {
    regdApply(d, cc);
    out.push(`= limits ${cc} -> ${d.chan.regd.alpha2} ${L(d.chan.regd.txpwrRegd)}`);
    for (const [band, ch] of chans) {
      const v = [];
      for (let bw = 0; bw < 5; bw++) for (let ntx = 0; ntx < 2; ntx++) for (let rs = 0; rs < 3; rs++) for (let bf = 0; bf < 2; bf++) {
        v.push(readTxpwrLimit(d, band, bw, ntx, rs, bf, ch));
      }
      const ru = [];
      for (let r = 0; r < 8; r++) for (let ntx = 0; ntx < 2; ntx++) ru.push(readTxpwrLimitRu(d, band, r, ntx, ch));
      out.push(`= lmt ${band} ${ch}: ${L(v)} ru ${L(ru)}`);
    }
  }
}

// ---- the PHY init parts (test-chan.mjs) ----
const CHS = [1, 6, 13, 36, 44, 100, 140, 149, 165];
{
  const d = dev();
  await scenario('init_txpwr_unit', d, () => C.initTxpwrUnit(d));
  await scenario('set_txpwr_ctrl', d, () => C.setTxpwrCtrl(d));
  await scenario('power_trim without phycap', d, () => C.powerTrim(d));
  await scenario('cfg_txrx_path', d, () => C.cfgTxrxPath(d));
  // ---- the MAC and BB parts alone, channel after channel ----
  for (const ch of CHS) {
    await scenario(`mac ch ${ch}`, d, () => C.setChannelMac(d, chanFor(d, ch)));
    await scenario(`bb ch ${ch}`, d, () => setChannelBb(d, chanFor(d, ch)));
  }
}
{
  const phycap = Buffer.alloc(128, 0xff);
  phycap[0x5df - 0x580] = 0x13; phycap[0x5dc - 0x580] = 0x0c; phycap[0x5de - 0x580] = 0x5a; phycap[0x5db - 0x580] = 0x21;
  const d = dev({ phycap });
  await scenario('power_trim with phycap', d, () => C.powerTrim(d));
  const p2 = Buffer.alloc(128, 0xff);
  p2[0x5dc - 0x580] = 0x1f; p2[0x5db - 0x580] = 0xf0;      // only path B programmed
  const d2 = dev({ phycap: p2 });
  await scenario('power_trim path B only', d2, () => C.powerTrim(d2));
}
{
  // set_gain_offset with programmed efuse offsets and phycap comp (test-chan.mjs), all subbands
  const glog = Buffer.from(EFUSE_LOG), gpc = Buffer.alloc(128, 0xff);
  glog[0x2d4] = 0x1e; glog[0x2d6] = 0x2f; glog[0x2d8] = 0xe1; glog[0x2da] = 0x90; glog[0x2dc] = 0x7f;
  gpc[0x5bb - 0x580] = 0x03; gpc[0x590 - 0x580] = 0x0e; gpc[0x5ba - 0x580] = 0x0d; gpc[0x58f - 0x580] = 0x02;
  gpc[0x5b9 - 0x580] = 0x8f; gpc[0x58d - 0x580] = 0x71;
  const g = dev({ efuseLog: glog, phycap: gpc });
  for (const antB of [false, true]) {
    g.st.hal = antB ? { antennaRx: R('RF_B') } : undefined;
    for (const sb of [0, 1, 3, 4]) {
      await scenario(`set_gain_offset subband ${sb}${antB ? ' antenna B' : ''}`, g, () => setGainOffset(g, sb));
    }
  }
  g.st.hal = undefined;
  await scenario('cfg_txrx_path programmed gains', g, () => C.cfgTxrxPath(g));
  for (const ch of [1, 36, 100, 149]) await scenario(`bb ch ${ch} programmed gains`, g, () => setChannelBb(g, chanFor(g, ch)));
}
for (const [ant, name] of [['RF_A', 'A'], ['RF_B', 'B']]) {
  const d = dev();
  d.st.hal = { antennaRx: R(ant), rxNss: 1 };
  await scenario(`cfg_txrx_path antenna ${name}`, d, () => C.cfgTxrxPath(d));
  const base = d.onRead;
  d.onRead = (kind, a, sz) => (kind === 'mmio' && a === BB + R('R_CHBW_MOD_V1') ? prep(R('B_ANT_RX_SEG0'), name === 'A' ? 1 : 2) : base(kind, a, sz));
  await scenario(`bb ch 1 rx path ${name}`, d, () => setChannelBb(d, chanFor(d, 1)));
  await scenario(`bb ch 36 rx path ${name}`, d, () => setChannelBb(d, chanFor(d, 36)));
}
{
  // a BB_GAIN element whose bytes all differ (the real one repeats values)
  const s = dev({ fw: synthBbGain(FW) });
  await scenario('cfg_txrx_path synthetic bb gain', s, () => C.cfgTxrxPath(s));
  for (const ch of [1, 36, 100, 149]) await scenario(`bb ch ${ch} synthetic bb gain`, s, () => setChannelBb(s, chanFor(s, ch)));
}

// ---- whole switches: help, MAC, BB, RF, TX power, help, band change ----
const e = dev({ txEn: 0x00ff });
await scenario('switch before cfg_txrx_path', e, () => C.setChannel(e, { band: '2g', ch: 1 }));
await scenario('cfg_txrx_path before switching', e, () => C.cfgTxrxPath(e));
for (const ch of CHS) await scenario(`switch ${bandOf(ch)} ${ch}`, e, () => C.setChannel(e, { band: bandOf(ch), ch }));
await scenario('set_txpwr after 165', e, () => C.setTxpwr(e));
await scenario('switch 2g 14 refused for DE', e, () => C.setChannel(e, { band: '2g', ch: 14 }));
await scenario('switch 40 MHz refused', e, () => C.setChannel(e, { band: '5g', ch: 36, bw: 40 }));
await scenario('switch 6g refused', e, () => C.setChannel(e, { band: '6g', ch: 37 }));
await scenario('switch 5g 38 refused (off grid)', e, () => C.setChannel(e, { band: '5g', ch: 38 }));
await scenario('switch 2g 6 with rfk_channel', e, () => C.setChannel(e, { band: '2g', ch: 6, rfk: true }));
await scenario('switch 2g 11 same band', e, () => C.setChannel(e, { band: '2g', ch: 11 }));

// ---- a step of set_channel fails: TX power and help-leave still run (core.c:582-588) ----
e.failRf = true;
e.onPoll = (kind, a) => a !== BB + R('R_SWSI_V1');
await scenario('switch 5g 36 with the RF step failing', e, () => C.setChannel(e, { band: '5g', ch: 36 }));
e.failRf = false;
e.onPoll = null;
await scenario('switch 5g 44 after the failure', e, () => C.setChannel(e, { band: '5g', ch: 44 }));

// ---- the country: rtw89_regd_notifier sets the TX power again (regd.c:921) ----
const flags = (d, band, list) => list.map((ch) => {
  const f = C.channelFlags(d, band, ch);
  return f.allowed ? `${f.noIr ? 'N' : ''}${f.radar ? 'R' : ''}` || '+' : '-';
}).join(' ');
const C2 = [0, 1, 11, 12, 13, 14, 15], C5 = [32, 36, 38, 40, 48, 52, 64, 68, 100, 140, 144, 148, 149, 153, 165, 169, 173, 177, 181];
const flagLines = (d) => [`= flags 2g ${flags(d, '2g', C2)}`, `= flags 5g ${flags(d, '5g', C5)}`, `= flags 6g ${flags(d, '6g', [1, 5])}`];
await scenario('flags DE', e, () => flagLines(e));
await scenario('regd TV', e, async () => [`= regd ${L((await C.regdSet(e, 'TV')).txpwrRegd)}`]);
await scenario('regd DE', e, async () => [`= regd ${L((await C.regdSet(e, 'DE')).txpwrRegd)}`]);
await scenario('switch 5g 149', e, () => C.setChannel(e, { band: '5g', ch: 149 }));
await scenario('regd XX (world)', e, async () => [`= regd ${(await C.regdSet(e, 'XX')).alpha2}`, ...flagLines(e)]);
await scenario('switch 2g 14 in the world', e, () => C.setChannel(e, { band: '2g', ch: 14 }));
await scenario('switch 2g 12 in the world', e, () => C.setChannel(e, { band: '2g', ch: 12 }));
await scenario('regd GB', e, async () => [`= regd ${L((await C.regdSet(e, 'GB')).txpwrRegd)}`]);
await scenario('regd DE again', e, async () => [`= regd ${L((await C.regdSet(e, 'DE')).txpwrRegd)}`, ...flagLines(e)]);

// ---- CMAC reads: 0xDEADBEEF -> R_AX_CK_EN all on, read again (pci.c:1998) ----
for (const n of [2, 11]) {
  const k = dev(), base = k.onRead, RFMOD = R('R_AX_WMAC_RFMOD');
  let dead = n;
  k.onRead = (kind, a, sz) => (kind === 'mmio' && a === RFMOD && dead-- > 0 ? 0xdeadbeef : base(kind, a, sz));
  await scenario(`mac ch 1 with ${n} dead reads`, k, () => C.setChannelMac(k, chanFor(k, 1)));
}
{
  // the CMAC is off: the PPDU status is left alone, the stand-in's sch TX writes nothing
  const k = dev(), base = k.onRead;
  k.onRead = (kind, a, sz) => (kind === 'mmio' && a === R('R_AX_CMAC_FUNC_EN') ? 0 : base(kind, a, sz));
  await scenario('help with the CMAC off', k, async () => {
    const bak = await C.setChannelHelp(k, true, chanFor(k, 36));
    await C.setChannelHelp(k, false, chanFor(k, 36), bak);
  });
}

// ---- order of the bring-up: no switch before phyDmInit, none after power off ----
{
  const f = dev();
  await scenario('order cfg_txrx_path', f, () => C.cfgTxrxPath(f));
  await scenario('order switch 2g 1', f, () => C.setChannel(f, { band: '2g', ch: 1 }));
  C.chanPowerOff(f);
  await scenario('order set_txpwr after power off', f, () => C.setTxpwr(f));
  await scenario('order switch after power off', f, () => C.setChannel(f, { band: '2g', ch: 6 }));
  await scenario('order cfg_txrx_path again', f, () => C.cfgTxrxPath(f));
  await scenario('order switch 2g 6 after power on', f, () => C.setChannel(f, { band: '2g', ch: 6 }));
}

// ---- the TX power with a TXPWR_BYRATE element whose bytes all differ ----
{
  const y = dev({ fw: synthByrate(FW), txEn: 0x00ff });
  await scenario('synthetic byrate cfg_txrx_path', y, () => C.cfgTxrxPath(y));
  for (const ch of [1, 36, 149]) {
    await scenario(`synthetic byrate switch ${ch}`, y, () => C.setChannel(y, { band: bandOf(ch), ch }));
    await scenario(`synthetic byrate set_txpwr ${ch}`, y, () => C.setTxpwr(y));
  }
}

process.stdout.write(out.join('\n') + '\n');
