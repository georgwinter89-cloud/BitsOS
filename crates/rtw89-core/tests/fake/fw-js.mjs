// fw-js.mjs: the JS driver's own fw.mjs (fw-parse.mjs, fw-h2c.mjs) on the kit's
// FakeDev and on the real firmware file, scenario by scenario, as text: `## <scenario>`,
// then every op as a bringup-golden.txt line, `= ...` lines for what the functions
// returned or wrote into DMA memory, `! error <kind>` where one failed.
// tests/fw_js.rs runs the same scenarios on the Rust side (HookBus with the same
// answers) and compares the text. The scenarios are test-fw.mjs's, plus every cut of a
// made-up container and the start-up H2Cs byte by byte.
//   node fw-js.mjs            (WLAN_TOOLS: the read-only WLAN tree)
// Reads only; writes nothing but stdout.
import { readFileSync } from 'node:fs';
import { pathToFileURL } from 'node:url';

const WZ = process.env.WLAN_TOOLS ?? '/home/georg/BitsOS/werkzeuge/plattform/wlan';
const imp = (rel) => import(pathToFileURL(`${WZ}/${rel}`).href);
const { FakeDev, R, prep, RtwError } = await imp('kit/rtwdev.mjs');
const { hciDmaTrx, dmacPreInit } = await imp('js/dmac.mjs');
const { pciAlloc, pciPreInit } = await imp('js/pci.mjs');
const fwm = await imp('js/fw.mjs');
const { mfwRecognize } = await imp('js/fw-parse.mjs');
const { FwError } = fwm;

const KIND = { 1: 'wr', 2: 'rd', 3: 'rmw', 4: 'poll', 5: 'delay', 6: 'mwr', 7: 'mrd', 8: 'mcpy', 9: 'cfgr', 10: 'cfgw', 11: 'mpoll', 12: 'mark', 13: 'now' };
const HEX = (v) => `0x${(v >>> 0).toString(16).toUpperCase().padStart(8, '0')}`;
const hx = (v) => `0x${(v >>> 0).toString(16).padStart(8, '0')}`;
const line = ([k, s, t, a, b, c]) =>
  `${KIND[k]}${k <= 4 || k === 6 || k === 7 ? s * 8 : ''} ${HEX(a)} ${HEX(b)} ${HEX(c)}${k === 4 || k === 11 ? ` ${t} ms` : ''}`;
const fnv = (b) => { let h = 0x811c9dc5; for (const x of b) h = Math.imul(h ^ x, 0x01000193) >>> 0; return h >>> 0; };
const out = [];
const file = readFileSync(`${WZ}/firmware/rtw8852b_fw-2.bin`);
const FWC = 0x1e0, PE = 0x88, IDX = R('R_AX_CH12_TXBD_IDX') >>> 0;

// the kind of an error, as tests/fw_js.rs names the Rust one
function kind(e) {
  if (e instanceof FwError) {
    const m = e.message;
    if (/^H2C path ready/.test(m)) return 'path H2C';
    if (/^FWDL path ready/.test(m)) return 'path FWDL';
    if (/^WCPU already/.test(m)) return 'wcpu';
    if (/CH12 DMA stuck/.test(m)) return `stuck status=${e.status ?? '-'}`;
    if (/link gone/.test(m)) return `gone status=${e.status ?? '-'}`;
    if (e.status !== undefined) return `status ${e.status}`;
    if (/does not process h2c/.test(m)) return 'h2creg busy';
    if (/c2h reg timeout/.test(m)) return 'c2hreg timeout';
    return 'fw';
  }
  return e instanceof RtwError ? 'device' : 'other';
}

// test-fw.mjs `ready`: scaffold steps as rtw89_mac_partial_init; memWrites recorded
async function ready(onRead = null, onPoll = null) {
  const d = new FakeDev({ onRead: (k, a, sz) => (k === 'cfg' && a === 0x80) ? 0x10110000 : (k === 'mmio' ? onRead?.(a) : undefined),
    onPoll: onPoll ? (k, a, m) => onPoll(a, m) : null });
  d.set32(R('R_AX_DMAC_FUNC_EN'), R('B_AX_MAC_FUNC_EN') | R('B_AX_DMAC_FUNC_EN'));
  hciDmaTrx(d, true); await dmacPreInit(d); await pciAlloc(d); await pciPreInit(d);
  await d.flush();
  d.writes = [];
  const mw = d.memWrite.bind(d);
  d.memWrite = async (a, b) => { d.writes.push([a, Buffer.from(b)]); return mw(a, b); };
  return d;
}

// one scenario: its ops since `d`'s mark, then what fn returned (or the error's kind)
async function scenario(name, d, fn) {
  out.push(`## ${name}`);
  if (d) await d.flush();
  const t0 = d ? d.trace.length : 0;
  let res = [];
  try {
    res = (await fn()) ?? [];
    if (d) await d.flush();
  } catch (e) {
    res = [`! error ${kind(e)}`];
    if (process.env.FW_JS_VERBOSE) console.error(name, e.stack);
  }
  if (d) for (const op of d.trace.slice(t0)) out.push(line(op));
  out.push(...res);
}

const memw = (d) => d.writes.map(([a, b]) => `= memw ${hx(a)} ${b.length} ${hx(fnv(b))}`);
const h2cBytes = (d, from) => d.writes.slice(from).map(([, b]) => `= h2c ${b.subarray(24).toString('hex')}`);
const fwShow = (f) => [`= fw ready=${+!!f.ready} tries=${f.tries ?? '-'} pieces=${f.pieces ?? '-'} seq=${f.h2cSeq}`,
  ...(f.failDump ? [`= dump ${hx(f.failDump.fwCtrl)} ${hx(f.failDump.bootDbg)} ${f.failDump.pc.map(hx).join(' ')}`] : [])];

// ---- parsing: the real file ----
function parseShow(fw) {
  const s = fw.suit, i = s.info;
  const r = [`= ${fwm.fwVersionText(s)}`,
    `= entry ${s.entry ? `${s.entry.cv} ${s.entry.type} ${s.entry.mp} ${s.entry.shift} ${s.entry.size}` : '-'} type ${s.type} hdr ${s.hdrVer} ${i.hdrLen} ${i.dynamicHdrLen} ${i.sectionNum} ${i.partSize} ${+!!i.dynamicHdrEn} ${i.idmemShareMode}`];
  for (const x of i.sections) {
    const p = fwm.sectionPieces(s, x, i.partSize);
    r.push(`= sec ${x.type} ${x.len} ${x.offset} ${hx(x.dlAddr)} ${+x.checksum} ${+x.redl} ${x.mssc} ${x.msscLen} ${+x.ignore} pieces ${p.length} ${p.length ? p[p.length - 1].length : 0}`);
  }
  for (const e of fw.elements) {
    r.push(`= elm ${e.id} ${e.name} ${e.size} ${e.aid} ${e.ver.toString('hex')} ${e.offset} ${e.payload.length} ${hx(fnv(e.payload))}`);
  }
  r.push(`= feat ${[...fw.feat].sort().join(' ')}`, `= missing ${fw.missingElements.join(' ')}`);
  return r;
}
await scenario('parse real cv 1', null, () => parseShow(fwm.fwParse(file, { cv: 1 })));
await scenario('parse real cv 2', null, () => parseShow(fwm.fwParse(file, { cv: 2 })));
await scenario('parse real cv 0', null, () => fwm.fwParse(file, { cv: 0 }));
await scenario('parse secure boot', null, () => fwm.fwParse(file, { cv: 1, secureBoot: true }));
const legacy = file.subarray(53072, 53072 + 326728);
await scenario('legacy', null, () => [`= normal ${mfwRecognize(legacy, R('RTW89_FW_NORMAL'), 1).size}`]);
await scenario('legacy normal ce', null, () => mfwRecognize(legacy, R('RTW89_FW_NORMAL_CE'), 1));
await scenario('legacy parse', null, () => fwm.fwParse(legacy, { cv: 1 }));
{
  const last = fwm.fwParse(file, { cv: 1 }).elements.at(-1);
  await scenario('cut at the last element', null, () => [`= elements ${fwm.fwParse(file.subarray(0, last.offset + last.size), { cv: 1 }).elements.length}`]);
}
const vc = (a, b, c, e) => fwm.verCode(a, b, c, e);
await scenario('features', null, () => [[0, 29, 29, 6], [0, 29, 29, 7], [0, 29, 29, 255], [0, 29, 30, 0], [0, 29, 128, 0], [0, 29, 130, 0], [1, 0, 0, 0]]
  .map((v) => `= feat ${v.join('.')}: ${[...fwm.featuresFor(vc(...v))].sort().join(' ')}`));

// ---- a made-up container (test-fw.mjs `synth`) ----
function synth({ mssc = 1, formatted = false, truncate = 0, part = 2020, dyn = 0, dynLen = dyn } = {}) {
  const hdr = Buffer.alloc(64 + dyn);
  hdr.writeUInt32LE(0x1d1d0000, 4);
  hdr.writeUInt32LE(prep(R('FW_HDR_W3_LEN'), 64 + dyn), 12);
  hdr.writeUInt32LE(2 << 8, 24);
  hdr.writeUInt32LE(((0x5a << 24) | (dyn ? R('FW_HDR_W7_DYN_HDR') : 0) | part) >>> 0, 28);
  if (dyn) hdr.writeUInt32LE(dynLen, 64);
  hdr.writeUInt32LE(0x18900000, 32); hdr.writeUInt32LE((2 << 24) | (1 << 28) | 3000, 36);
  hdr.writeUInt32LE(0x18e00000, 48); hdr.writeUInt32LE((9 << 24) | 100, 52); hdr.writeUInt32LE(formatted ? 0xff : mssc, 56);
  const sec1 = Buffer.alloc(3008, 0x11), sec2 = Buffer.alloc(100, 0x22);
  let sig = Buffer.alloc(mssc * 512, 0x33);
  if (formatted) {
    sig = Buffer.alloc(32 + 1 + 2 * 512, 0x44);
    Buffer.from('MSSKPOOL').copy(sig, 0);
    sig.writeUInt32LE(33, 12); sig[16] = 0; sig[20] = 0; sig[21] = 1;
    sig.writeUInt16LE(2, 22); sig.writeUInt16LE(1, 24); sig.writeUInt16LE(8, 26);
    sec2.writeUInt16LE(0, 60);
  }
  let suit = Buffer.concat([hdr, sec1, sec2, sig]);
  if (truncate) suit = suit.subarray(0, suit.length - truncate);
  const mfw = Buffer.alloc(16 + 2 * 16);
  mfw[0] = 0xff; mfw[1] = 2;
  for (const [i, mp] of [[0, 1], [1, 0]]) {
    const o = 16 + i * 16;
    mfw[o] = 1; mfw[o + 1] = 5; mfw[o + 2] = mp; mfw.writeUInt32LE(48, o + 4); mfw.writeUInt32LE(suit.length, o + 8);
  }
  return Buffer.concat([mfw, suit]);
}
const SYNTH = [['default', {}], ['formatted', { formatted: true }], ['mssc 3', { mssc: 3 }], ['truncate 4', { truncate: 4 }],
  ['dynamic header 16/12', { dyn: 16, dynLen: 12 }], ['part 0x100 dynamic 16', { part: 0x100, dyn: 16 }]];
for (const [name, o] of SYNTH) await scenario(`synth ${name}`, null, () => parseShow(fwm.fwParse(synth(o), { cv: 1 })));
{
  const sd = fwm.fwParse(synth({ part: 0x100, dyn: 16 }), { cv: 1 }).suit;
  sd.info.sections[1].lenOverride = 60;
  await scenario('header H2C with len_override 60', null, () => [`= hdr ${fwm.fwHdrH2c(new FakeDev(), sd).toString('hex')}`,
    `= pieces ${fwm.sectionPieces(sd, sd.info.sections[1], 2020).map((p) => p.length).join(' ')}`]);
  sd.info.sections[1].lenOverride = 200;
  await scenario('len_override above len', null, () => [`= pieces ${fwm.sectionPieces(sd, sd.info.sections[1], 2020).map((p) => p.length).join(' ')}`]);
}
// every cut of the formatted container: where the JS and the Rust stop must agree
{
  const s = synth({ formatted: true });
  const res = [];
  for (let n = 0; n <= s.length; n += 7) {
    try { const f = fwm.fwParse(s.subarray(0, n), { cv: 1 }); res.push(`${n}:${f.elements.length}`); } catch { res.push(`${n}:e`); }
  }
  await scenario('cuts of the formatted container', null, () => [`= ${res.join(' ')}`]);
}

// ---- the download on FakeDev ----
const fw = fwm.fwParse(file, { cv: 1 });
{
  const d = await ready();
  await scenario('download', d, async () => { await fwm.fwDownload(d, fwm.fwParse(file, { cv: 1 })); return [...fwShow(d.fw), ...memw(d)]; });
}
const failing = async (name, onRead, onPoll) => {
  const d = await ready(onRead, onPoll);
  await scenario(name, d, async () => {
    let err = null;
    try { await fwm.fwDownload(d, fwm.fwParse(file, { cv: 1 })); } catch (e) { err = e; }
    await d.flush();
    return [...fwShow(d.fw), ...(err ? [`! error ${kind(err)}`] : [])];
  });
};
await failing('status 2 every try', (a) => (a === FWC ? 0x40 : undefined), (a, m) => !(a === FWC && m === 0xe0));
await failing('h2c path never ready', null, (a, m) => !(a === FWC && m === 0x2));
await failing('wcpu already on', (a) => (a === PE ? 0x3 : undefined), null);
await failing('lost connection', null, (a, m) => { if (a === FWC && m === 0x2) throw new Error('connection lost'); return true; });
{
  let rdy = 0, pc = 0;
  await failing('one bad try', (a) => (a === FWC ? 0x12340040 : a === R('R_AX_BOOT_DBG') ? 0xb0070000 : a === R('R_AX_DBG_PORT_SEL') ? 0x1000 + pc++ : undefined),
    (a, m) => !(a === FWC && m === 0xe0 && rdy++ === 0));
}
{
  let n = 0;
  await failing('ch12 timeout at poll 30', null, (a) => !(a === IDX && ++n === 30));
}
{
  let n = 0;
  await failing('ch12 stuck', null, (a) => !(a === IDX && ++n >= 30));
}
await failing('ch12 all ones', (a) => (a === IDX ? 0xffffffff : undefined), (a, m) => !(a === FWC && m === 0x2));
{
  // the first status poll fails and the read after it says 4 (cut mismatch); the next try passes
  let k = 0, f = 0;
  await failing('status 4 (cut mismatch), then fixed', (a) => (a === FWC && f ? (f = 0, 0x80) : undefined),
    (a, m) => { if (a === FWC && m === 0xe0 && k++ < 1) { f = 1; return false; } return true; });
}

// ---- H2Cs: after the download, early H2C, offload config, fw log, RF registers ----
{
  const d = await ready();
  await fwm.fwDownload(d, fw);
  await d.flush();
  let from = d.writes.length;
  await scenario('start-up h2cs', d, async () => {
    d.fw.earlyH2c.push(Buffer.from('0100000108000000', 'hex'));
    await fwm.sendAllEarlyH2c(d);
    await fwm.h2cSetOfldCfg(d);
    await fwm.h2cFwLog(d, false);
    await fwm.h2cFwLog(d, true);
    const info = fwm.rfRegInfo(R('RF_PATH_B'));
    fwm.rfRegStore(d, info, 0x10000, 0x12345); fwm.rfRegStore(d, info, 0x1001f, 0xfffff);
    const up = await fwm.rfRegUpload(d, info);
    const a = fwm.rfRegInfo(R('RF_PATH_A'));
    for (let i = 0; i < 700; i++) fwm.rfRegStore(d, a, 0x100 + (i % 0x400), i * 37);
    const up2 = await fwm.rfRegUpload(d, a);
    return [`= up ${up} ${up2} ${info.currIdx} ${a.currIdx} seq ${d.fw.h2cSeq}`, ...h2cBytes(d, from)];
  });
  from = d.writes.length;
  // 30 more: the sequence numbers and the receive-ack rule through a wrap of CH12
  await scenario('thirty h2cs', d, async () => {
    for (let i = 0; i < 30; i++) {
      await fwm.h2cSend(d, { cat: 1, cls: 0x10 + (i % 3), func: i, payload: Buffer.alloc((i * 13) % 40, i), rack: i % 5 === 0, dack: i % 7 === 0 });
    }
    return [`= seq ${d.fw.h2cSeq}`, ...h2cBytes(d, from)];
  });
}

// ---- register path ----
{
  const m = new FakeDev({ onRead: (k, a) => k !== 'mmio' ? undefined : a === 0x8164 ? 1 : a === 0x8150 ? 0x00110403 : a === 0x8154 ? 0xabcd : undefined });
  const show = (c) => (c ? [`= c2hreg ${c.id} ${c.contentLen} ${c.w.map(hx).join(' ')}`] : ['= c2hreg none']);
  await scenario('msg reg', m, async () => show(await fwm.fwMsgReg(m, { id: 3, contentLen: 0, w: [2 << 16] })));
  await scenario('msg reg 9 bytes', m, async () => show(await fwm.fwMsgReg(m, { id: 0x45, contentLen: 9, w: [0xffffffff, 1, 2, 3] })));
  await scenario('msg reg without answer', m, async () => show(await fwm.fwMsgReg(m, { id: 5, contentLen: 2, w: [0x10000] }, null)));
  await scenario('msg reg counters', m, async () => [`= counters ${m.fw.h2cCounter} ${m.fw.c2hCounter}`]);
  const busy = new FakeDev({ onPoll: (k, a) => a !== 0x8160 });
  await scenario('h2creg busy', busy, () => fwm.fwMsgReg(busy, { id: 3, contentLen: 0, w: [] }));
}

// ---- C2H ----
{
  const c2 = Buffer.alloc(12); c2.writeUInt32LE((1 << 8) | 1, 0); c2.writeUInt32LE(12, 4); c2.writeUInt32LE((5 << 24) | (0 << 16) | (0x14 << 8) | (9 << 2) | 1, 8);
  const show = (b) => {
    const p = fwm.c2hParse(b);
    const a = p.ack ? `${p.ack.kind} ${p.ack.cat} ${p.ack.cls} ${p.ack.func} ${p.ack.seq} ${p.ack.ret ?? '-'}` : '-';
    return [`= c2h ${p.cat} ${p.cls} ${p.func} ${p.len} ${p.seq ?? '-'} ${p.payload.toString('hex')} ack ${a}`];
  };
  await scenario('c2h done ack', null, () => show(c2));
  const r = Buffer.from(c2); r.writeUInt32LE(1, 0); r.writeUInt32LE((7 << 16) | (0x14 << 8) | (9 << 2) | 1, 8);
  await scenario('c2h receive ack', null, () => show(r));
  await scenario('c2h longer buffer', null, () => show(Buffer.concat([r, Buffer.alloc(8, 0xee)])));
  const o = Buffer.from(r); o.writeUInt32LE((1 << 2) | 1, 0);
  await scenario('c2h other class', null, () => show(o));
  await scenario('c2h 9 bytes', null, () => show(o.subarray(0, 9)));
  await scenario('c2h 7 bytes', null, () => show(o.subarray(0, 7)));
  const big = Buffer.alloc(40, 0x5a); big.writeUInt32LE((3 << 8) | (2 << 2) | 2, 0); big.writeUInt32LE(30, 4);
  await scenario('c2h outsrc', null, () => show(big));
}

process.stdout.write(out.join('\n') + '\n');
