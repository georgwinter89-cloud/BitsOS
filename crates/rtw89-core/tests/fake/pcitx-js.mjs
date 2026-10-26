// pcitx-js.mjs: the JS driver's own pcitx.mjs (with pci.mjs's pciAlloc) on the kit's
// FakeDev, driven by tests/fake/pcitx-scenarios.txt (its header says the commands).
// Prints, per scenario `## <name>`, every op as a bringup-golden.txt line, `= ...` for
// what the functions returned or the state asked for, `= log ...` for what pcitx.mjs
// logged and `! error` where a call threw. tests/pcitx_js.rs interprets the same file on
// the Rust side (MemBus) and compares the text. The TX infos are built by hand as
// txrx's txInfo gives them (src/pcitx/fakechip.rs has the same).
//   node pcitx-js.mjs [scenarios.txt]   (WLAN_TOOLS: the read-only WLAN tree)
// Reads only; writes nothing but stdout.
import { readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

const WZ = process.env.WLAN_TOOLS ?? '/home/georg/BitsOS/werkzeuge/plattform/wlan';
const imp = (rel) => import(pathToFileURL(`${WZ}/${rel}`).href);
const { FakeDev, R, field, prep } = await imp('kit/rtwdev.mjs');
const { pciAlloc } = await imp('js/pci.mjs');
const { pciTx, pciTxKick, pciTxReclaim, pciTxResource, pciTxInit } = await imp('js/pcitx.mjs');
const { getChDma } = await imp('js/txrx-desc.mjs');

const file = process.argv[2] ?? join(dirname(fileURLToPath(import.meta.url)), 'pcitx-scenarios.txt');
const KIND = { 1: 'wr', 2: 'rd', 3: 'rmw', 4: 'poll', 5: 'delay', 6: 'mwr', 7: 'mrd', 8: 'mcpy', 9: 'cfgr', 10: 'cfgw', 11: 'mpoll', 12: 'mark', 13: 'now' };
const hex = (v) => `0x${(v >>> 0).toString(16).toUpperCase().padStart(8, '0')}`;
const line = ([k, s, t, a, b, c]) =>
  `${KIND[k]}${k <= 4 || k === 6 || k === 7 ? s * 8 : ''} ${hex(a)} ${hex(b)} ${hex(c)}${k === 4 || k === 11 ? ` ${t} ms` : ''}`;
const num = (s) => Number(s);
const out = [];

// TX infos as txrx's txInfo gives them (mgmt on CH8, data by TID)
const qselOfTid = (tid) => R([1, 2].includes(tid) ? 'RTW89_TX_QSEL_BK_0' : [4, 5].includes(tid) ? 'RTW89_TX_QSEL_VI_0'
  : [6, 7].includes(tid) ? 'RTW89_TX_QSEL_VO_0' : 'RTW89_TX_QSEL_BE_0');
function info(kind, len, macId, seq) {
  const [k, a, b] = kind.split(':');
  if (k === 'h2c') return { chDma: R('RTW89_DMA_H2C'), pktSize: len };
  if (k === 'mgmt') {
    return { chDma: R('RTW89_TXCH_CH8'), wdPage: true, hwSsnSel: R('RTW89_MGMT_HW_SSN_SEL'), hwSeqMode: R('RTW89_MGMT_HW_SEQ_MODE'),
      qsel: R('RTW89_TX_QSEL_B0_MGMT'), pktSize: len, macId, seq, enWdInfo: true,
      wdInfo: [(R('RTW89_TXWD_INFO0_USE_RATE') | R('RTW89_TXWD_INFO0_DISDATAFB')) >>> 0, 0, 0, 0, R('RTW89_TXWD_INFO4_HW_RTS_EN') >>> 0, 0] };
  }
  const tid = num(a), qsel = qselOfTid(tid);
  const i = { chDma: getChDma(qsel), hdrLlcLen: 12, wdPage: true, tidIndicate: [2, 3, 5, 7].includes(tid), qsel, pktSize: len, macId, seq,
    enWdInfo: true, wdInfo: [0, 0, 0, 0, (R('RTW89_TXWD_INFO4_RTS_EN') | R('RTW89_TXWD_INFO4_HW_RTS_EN')) >>> 0, 0] };
  if (k === 'nowd') { i.enWdInfo = false; delete i.wdInfo; }
  if (k === 'qsel') i.qsel = num(b);
  return i;
}
const frame = (n, seq) => Buffer.from(Array.from({ length: n }, (_, i) => ((seq * 7) + i) & 0xff));
const rpp = (seq, qsel, st) => (prep(R('RTW89_PCI_RPP_SEQ'), seq) | prep(R('RTW89_PCI_RPP_TX_STATUS'), st) | prep(R('RTW89_PCI_RPP_QSEL'), qsel)) >>> 0;

let d, chip, logSeen = 0, mark = 0;
const ring = (name) => d.pci.tx[name] ?? d.pci.rx[name];
const showWd = (name) => {
  const w = d.txrx.wd[name], r = d.pci.tx[name];
  out.push(`= wd ${name} free=${w.free.map((p) => p.seq).join(',')} busy=${w.busy.map((p) => p.seq).join(',')} wp=${r.wp} rp=${r.rp}`);
};
const value = (v) => {
  if (!v.startsWith('hw:')) return num(v) >>> 0;
  const e = v.slice(3), rpq = d.pci.rx.RPQ;
  const n = e.startsWith('rpq+') ? (rpq.wp + num(e.slice(4))) % rpq.len : num(e);
  return (n << 16) >>> 0;
};

// test-pcitx.mjs chipRun
async function chipRun() {
  for (const rg of Object.values(d.pci.tx)) {
    if (rg.name === 'CH12') continue;
    const host = d._get(d.regs, rg.idx, 2);
    let cur = d.idx.get(rg.idx) >>> 16;
    while (cur !== host && cur < rg.len) {
      const bd = await d.memRead(rg.bd + cur * 8, 8);
      const wd = await d.memRead(bd.readUInt32LE(4), bd.readUInt16LE(0));
      const seq = wd.readUInt16LE(bd.readUInt16LE(0) - 16) & 0x7fff;
      chip.pending.push(rpp(seq, field(wd.readUInt32LE(8), R('RTW89_TXWD_BODY2_QSEL')), 0));
      cur = (cur + 1) % rg.len;
    }
    d.idx.set(rg.idx, (cur << 16) >>> 0);
  }
  const rpq = d.pci.rx.RPQ;
  while (chip.pending.length && (chip.hw + 1) % rpq.len !== rpq.wp) {
    await d.memWrite(rpq.bufs[chip.hw], rpqBuf([chip.pending.shift()], 0x1c000, {}));
    chip.hw = (chip.hw + 1) % rpq.len;
  }
  d.idx.set(rpq.idx, (chip.hw << 16) >>> 0);
}

function rpqBuf(rpps, flags, { long = false, drv = 0, size, pad = 0 }) {
  const n = rpps.length;
  const d0 = (0x07000000 | (4 * n) | (long ? R('AX_RXD_LONG_RXD') : 0) | prep(R('AX_RXD_DRV_INFO_SIZE_MASK'), drv)) >>> 0;
  const desc = Buffer.alloc(long ? 32 : 16);
  desc.writeUInt32LE(d0, 0);
  const body = Buffer.concat([desc, Buffer.alloc(drv * 8), ...rpps.map((v) => { const b = Buffer.alloc(4); b.writeUInt32LE(v, 0); return b; })]);
  const head = Buffer.alloc(4);
  head.writeUInt32LE(((flags | prep(R('RTW89_PCI_RXBD_WRITE_SIZE'), size ?? 4 + body.length)) >>> 0), 0);
  const all = Buffer.concat([head, body]);
  return all.length < pad ? Buffer.concat([all, Buffer.alloc(pad - all.length)]) : all;
}

async function run(cmd, i) {
  const t = cmd.trim().split(/\s+/).map((x) => x.replaceAll('$i', String(i)));
  const opt = (k) => t.find((x) => x.startsWith(`${k}=`))?.slice(k.length + 1);
  switch (t[0]) {
    case 'every': if (i % num(t[1]) === num(t[2])) await run(t.slice(3).join(' '), i); return;
    case 'new':
      d = new FakeDev();
      await pciAlloc(d);
      pciTxInit(d);
      d.idx = new Map();
      d.onRead = (kind, a) => (kind === 'mmio' && d.idx.has(a) ? d.idx.get(a) : undefined);
      d.logs = [];
      d.log = (m) => d.logs.push(m);
      await d.flush();
      mark = d.trace.length; logSeen = 0;
      return;
    case 'chipnew':
      chip = { hw: 0, pending: [] };
      for (const rg of Object.values(d.pci.tx)) d.idx.set(rg.idx, 0);
      d.idx.set(d.pci.rx.RPQ.idx, 0);
      return;
    case 'chip': await chipRun(); return;
    case 'tx': {
      const len = num(t[3]), seq = num(t[5]);
      const r = await pciTx(d, t[1], info(t[2], len, num(t[4]), seq), frame(len + num(opt('extra') ?? 0), seq), { kick: !t.includes('nokick') });
      out.push(r ? `= tx ${r.ch} ${r.seq} ${r.wp}` : '= tx null');
      return;
    }
    case 'txframe': {
      const len = num(t[2]), seq = num(t[3]), inf = info(t[1], len, 0, seq);
      let q = false;
      if (t[1] === 'mgmt' || await pciTxResource(d, inf.chDma)) q = !!(await pciTx(d, inf.chDma, inf, frame(len, seq)));
      out.push(`= queued ${+q}`);
      return;
    }
    case 'kick': await pciTxKick(d, t[1]); return;
    case 'resource': out.push(`= res ${await pciTxResource(d, t[1])}`); return;
    case 'reclaim':
      for (const x of await pciTxReclaim(d)) {
        out.push(`= rep ${x.ch} ${x.seq} ${x.txStatus} ${+x.acked} ${x.len} ${x.qsel} ${x.macId} ${x.wlanSeq}`);
      }
      return;
    case 'init': pciTxInit(d); return;
    case 'force': d.idx.set(ring(t[1]).idx, value(t[2])); return;
    case 'rpqbuf': {
      const rpq = d.pci.rx.RPQ;
      const slot = t[1].startsWith('rpq+') ? (rpq.wp + num(t[1].slice(4))) % rpq.len : num(t[1]);
      const rpps = t[3] === '-' ? [] : t[3].split(',').map((x) => { const [s, q, st] = x.split(':').map(num); return rpp(s, q, st); });
      const sz = opt('size');
      await d.memWrite(rpq.bufs[slot], rpqBuf(rpps, num(t[2]), { long: t.includes('long'), drv: num(opt('drv') ?? 0),
        size: sz === undefined ? undefined : num(sz), pad: num(opt('pad') ?? 0) }));
      return;
    }
    case 'page': {
      const w = d.txrx.wd[t[1]], p = w.pages[num(t[2])];
      out.push(`= page ${(await d.memRead(p.addr, p.len)).toString('hex')}`);
      out.push(`= frame ${(await d.memRead(p.addr + 128, 16)).toString('hex')}`);
      return;
    }
    case 'wd': showWd(t[1]); return;
    case 'rpq': out.push(`= rpq ${d.pci.rx.RPQ.wp} ${d.pci.rx.RPQ.rp}`); return;
    case 'kicks': {
      const ks = d.trace.filter((o) => o[0] === 1 && o[1] === 2 && o[3] === d.pci.tx[t[1]].idx).map((o) => o[5]);
      out.push(`= kicks ${t[1]} ${ks.length} ${Math.max(...ks)}`);
      return;
    }
    default: throw new Error(`unknown command ${t[0]}`);
  }
}

// one command: its ops, then its results, then what it logged
async function step(cmd, i) {
  const at = out.length;
  try {
    await run(cmd, i);
  } catch (e) {
    if (/unknown command/.test(e.message)) throw e;
    out.push('! error');
    if (process.env.PCITX_JS_VERBOSE) console.error(cmd, e.message);
  }
  await d.flush();
  const ops = d.trace.slice(mark).map(line);
  mark = d.trace.length;
  out.splice(at, 0, ...ops);
  for (; logSeen < d.logs.length; logSeen++) out.push(`= log ${d.logs[logSeen]}`);
}

for (const raw of readFileSync(file, 'utf8').split('\n')) {
  const l = raw.trim();
  if (!l || (l.startsWith('#') && !l.startsWith('## '))) continue;
  if (l.startsWith('## ')) { out.push(l); continue; }
  const rep = l.match(/^repeat (\d+) (.*)$/);
  const n = rep ? num(rep[1]) : 1, body = rep ? rep[2] : l;
  for (let i = 0; i < n; i++) for (const c of body.split(';')) await step(c, i);
}
process.stdout.write(out.join('\n') + '\n');
