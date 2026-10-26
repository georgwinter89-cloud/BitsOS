// txrx-js.mjs: the JS driver's own txrx.mjs (with txrx-desc/-rx/-ieee/-data.mjs, pci.mjs's
// fillTxdesc and phy.mjs's RX chip ops) as the oracle of rtw89-core's txrx module. A seeded
// generator makes the inputs: TX requests (frames of every kind, also torn ones, with every
// option), mac80211 sequence numbers, the station's frames, Ethernet <-> 802.11 data frames,
// the CCMP replay check, the parsers on structured and random frames, and RX buffers (frames,
// PPDU statuses, C2Hs, segments, garbage) under several channel/scan states and chip ops.
// Each input is printed as a `> ...` command line, followed by what txrx.mjs answered as
// `= ...` lines. tests/txrx_js.rs reads the commands, runs them on the Rust side and prints the
// same `= ...` lines; the two texts must be equal.
//   node txrx-js.mjs [seed]     (WLAN_TOOLS: the read-only WLAN tree)
// Reads only; writes nothing but stdout.
import { pathToFileURL } from 'node:url';

const WZ = process.env.WLAN_TOOLS ?? '/home/georg/BitsOS/werkzeuge/plattform/wlan';
const imp = (rel) => import(pathToFileURL(`${WZ}/${rel}`).href);
const T = await imp('js/txrx.mjs');
const { fillTxdesc } = await imp('js/pci.mjs');
const phy = await imp('js/phy.mjs');

const out = [];
const say = (s) => out.push(s);
const hex = (b) => (b && b.length ? Buffer.from(b).toString('hex') : '-');
const B = (v) => (v ? '1' : '0');
const N = (v) => `${v ?? 0}`;
const O = (v) => (v === undefined || v === null ? '-' : `${v}`);

// ---- the generator (an LCG; only this side generates, the Rust side reads the commands) ----
let seed = Number(process.argv[2] ?? 20261015) >>> 0;
const rnd = () => (seed = (Math.imul(seed, 1103515245) + 12345) >>> 0) >>> 8;
const r = (n) => rnd() % n;
const chance = (p) => r(1000) < p * 1000;
const pick = (a) => a[r(a.length)];
const bytes = (n) => Buffer.from(Array.from({ length: n }, () => r(256)));
const u16le = (v) => { const b = Buffer.alloc(2); b.writeUInt16LE(v & 0xffff); return b; };
const u16be = (v) => { const b = Buffer.alloc(2); b.writeUInt16BE(v & 0xffff); return b; };
const le32 = (...v) => { const b = Buffer.alloc(4 * v.length); v.forEach((x, i) => b.writeUInt32LE(x >>> 0, 4 * i)); return b; };

const OWN = Buffer.from('02b175000001', 'hex'), AP = Buffer.from('3ca62f112233', 'hex');
const MACS = [T.BCAST, Buffer.from('01005e000001', 'hex'), OWN, AP];
const mac = () => (chance(0.8) ? pick(MACS) : bytes(6));
const BANDS = ['2g', '5g'];

function ie(id, body) { return Buffer.concat([Buffer.from([id, body.length & 0xff]), body]); }

// an RSN element body: mostly well formed, sometimes cut, with odd counts or versions
function rsnBody() {
  const parts = [u16le(chance(0.9) ? 1 : r(4))];
  const suite = () => { const b = Buffer.alloc(4); b.writeUInt32BE((pick([0x000fac00, 0x0050f200, 0x000fac00]) | pick([1, 2, 4, 6, 8, 9, 12])) >>> 0); return b; };
  if (chance(0.9)) parts.push(suite());
  const list = () => { const n = pick([0, 1, 1, 2, 3, 300]); parts.push(u16le(n)); for (let i = 0; i < Math.min(n, 3); i++) parts.push(suite()); };
  if (chance(0.85)) list();
  if (chance(0.8)) list();
  if (chance(0.8)) parts.push(u16le(pick([0, 0x80, 0xc0, 0x8c, r(0x10000)])));
  if (chance(0.3)) { const n = pick([0, 1, 2, 5]); parts.push(u16le(n)); for (let i = 0; i < Math.min(n, 2); i++) parts.push(bytes(16)); }
  if (chance(0.3)) parts.push(suite());
  let b = Buffer.concat(parts);
  if (chance(0.15)) b = b.subarray(0, r(b.length + 1));
  return b;
}

// the elements of a beacon or probe response body
function ies() {
  const list = [];
  const ssid = chance(0.1) ? bytes(r(40)) : Buffer.from(pick(['BitsOS-Fake', 'FRITZ!Box 7590 XY', '', 'äöü']));
  if (chance(0.9)) list.push(ie(0, ssid));
  if (chance(0.9)) list.push(ie(1, Buffer.from(Array.from({ length: 1 + r(8) }, () => pick([0x82, 0x84, 0x8b, 0x96, 0x0c, 0x12, 0x18, 0x24, 0x8c, 0x98, 0xb0, r(256)])))));
  if (chance(0.8)) list.push(ie(3, chance(0.9) ? Buffer.from([pick([1, 6, 11, 36, 149, 0, 14, r(256)])]) : bytes(2)));
  if (chance(0.5)) list.push(ie(5, bytes(pick([1, 4, 6]))));
  if (chance(0.4)) list.push(ie(7, bytes(pick([1, 3, 6]))));
  if (chance(0.5)) list.push(ie(50, Buffer.from([0x30, 0x48, 0x60, 0x6c])));
  if (chance(0.6)) list.push(ie(48, rsnBody()));
  if (chance(0.3)) list.push(ie(45, bytes(pick([1, 26]))));
  if (chance(0.4)) list.push(ie(61, chance(0.8) ? Buffer.concat([Buffer.from([pick([1, 6, 11, 40, 0])]), bytes(21)]) : bytes(10)));
  if (chance(0.2)) list.push(ie(221, Buffer.concat([Buffer.from(pick(['0050f201', '0050f202', '0050f2']), 'hex'), bytes(r(6))])));
  if (chance(0.2)) list.push(ie(r(256), bytes(r(10))));
  for (let i = list.length - 1; i > 0; i--) if (chance(0.2)) [list[i], list[i - 1]] = [list[i - 1], list[i]];
  let b = Buffer.concat(list);
  if (chance(0.1)) b = Buffer.concat([b, Buffer.from([r(256), 40]), bytes(r(10))]);   // a truncated one
  return b;
}

function beaconFrame() {
  const h = Buffer.concat([u16le(T.fcOf(0, pick([8, 8, 5, 4]), chance(0.1) ? 0x8000 : 0)), u16le(r(0x10000)), mac(), mac(), mac(), u16le(r(0x10000))]);
  const fixed = Buffer.concat([bytes(8), u16le(pick([100, 102, r(0x10000)])), u16le(pick([0x0411, 0x0401, 0x0011, r(0x10000)]))]);
  return Buffer.concat([h, fixed, ies()]);
}

const LLC = Buffer.from('aaaa03000000', 'hex'), TUNNEL = Buffer.from('aaaa030000f8', 'hex');
// a payload after the 802.11 header: LLC/SNAP with an EtherType of interest, or bytes
function payload() {
  const k = r(8);
  if (k === 0) return bytes(r(30));
  const ip = (proto, sp, dp, pad) => {
    const h = Buffer.alloc(20); h[0] = pick([0x45, 0x46, 0x4f]); h[9] = proto;
    return Buffer.concat([h, u16be(sp), u16be(dp), Buffer.alloc(pad)]);
  };
  const type = pick([0x888e, 0x0806, 0x0800, 0x0800, 0x0800, 0x8137, 0x80f3, r(0x10000)]);
  let body;
  if (type === 0x0800) body = chance(0.5) ? ip(17, pick([67, 68, 53]), pick([68, 67, 53]), pick([0, 100, 226, 227, 218, 219, 240, 300])) : ip(pick([1, 6, 17]), r(0x10000), r(0x10000), r(40));
  else body = bytes(r(40));
  return Buffer.concat([chance(0.85) ? LLC : (chance(0.5) ? TUNNEL : bytes(6)), u16be(type), body]);
}

function dataFrame() {
  const stype = pick([0, 0, 8, 8, 4, 12, 1, r(16)]);
  let flags = 0;
  for (const f of [0x100, 0x200, 0x400, 0x800, 0x1000, 0x4000, 0x8000]) if (chance(f === 0x100 || f === 0x200 ? 0.5 : 0.15)) flags |= f;
  const fc = T.fcOf(2, stype, flags);
  const parts = [u16le(fc), u16le(r(0x10000)), mac(), mac(), mac(), u16le(r(0x10000))];
  if ((flags & 0x300) === 0x300) parts.push(mac());
  if (stype & 8) { parts.push(u16le(r(0x10000))); if (flags & 0x8000) parts.push(bytes(4)); }
  if (flags & 0x4000) { const c = bytes(8); if (chance(0.85)) c[3] |= 0x20; else c[3] &= ~0x20; parts.push(c); }
  parts.push(payload());
  if (flags & 0x4000) parts.push(bytes(8));
  return Buffer.concat(parts);
}

function anyFrame() {
  const k = r(10);
  let f;
  if (k < 3) f = beaconFrame();
  else if (k < 6) f = dataFrame();
  else if (k < 8) f = Buffer.concat([u16le(T.fcOf(0, r(16), chance(0.1) ? 0x8000 : (chance(0.2) ? 0x4000 : 0))), u16le(0), mac(), mac(), mac(), u16le(r(0x10000)), bytes(r(40))]);
  else if (k < 9) f = Buffer.concat([u16le(T.fcOf(1, r(16), 0)), bytes(r(20))]);
  else f = bytes(r(60));
  if (chance(0.12)) f = f.subarray(0, r(f.length + 1));
  return f;
}

// ---- printing (tests/txrx_js.rs prints the same) ----
const TXF = ['pktSize', 'isBmc', 'wdPage', 'chDma', 'seq', 'report', 'sn', 'txCntLmtEn', 'txCntLmt', 'hiq', 'updWlanHdr', 'qsel',
  'swMld', 'port', 'macId', 'hwSsnSel', 'hwSeqMode', 'enWdInfo', 'useRate', 'disDataFb', 'dataRate', 'tidIndicate', 'erCap',
  'stbc', 'ldpc', 'secEn', 'secKeyid', 'secType', 'secCamIdx', 'dataRetryLowestRate', 'aCtrlBsr', 'bk', 'hdrLlcLen'];
function txInfoLines(info) {
  say(`= info ${O(info.txType)} ${TXF.map((k) => N(Number(info[k] ?? 0))).join(' ')} btc ${O(info.btcPkt)}`);
  say(`= wd ${info.wdInfo ? info.wdInfo.map((v) => (v >>> 0).toString(16)).join(',') : '-'} desc ${hex(fillTxdesc(info))}`);
}
const BAND = (b) => (b ?? '-');
function hdrLine(h) {
  if (!h) return say('= hdr -');
  const a = (m) => (m && m.length === 6 ? hex(m) : '-');
  say(`= hdr ${h.fc} ${h.type} ${h.stype} ${B(h.toDs)}${B(h.fromDs)}${B(h.retry)}${B(h.pm)}${B(h.protected)} ${h.hdrLen} ${h.dur} ` +
    `${a(h.addr1)} ${a(h.addr2)} ${a(h.addr3)} ${O(h.seq)} ${O(h.frag)} ${B(h.short)} ${a(h.addr4)} ${O(h.qos)} ${O(h.tid)}`);
}
function rsnLine(x) {
  if (!x) return say('= rsn -');
  const s = (v) => (v >>> 0).toString(16);
  say(`= rsn ${x.version} ${s(x.group)} ${x.pairwise.map(s).join(',')} ${x.akms.map(s).join(',')} ${x.caps} ` +
    `${x.pmkids.map(hex).join(',') || '-'} ${x.groupMgmt === null ? '-' : s(x.groupMgmt)} ${O(x.bad)}` +
    (x.bad === 'version' ? '' : ` ${B(x.mfpc)}${B(x.mfpr)}${B(x.psk)}${B(x.ccmp)}`));
}
function iesLine(l) { say(`= ies ${l.map((e) => `${e.id}:${hex(e.data)}`).join(' ') || '-'} ${B(l.truncated)}`); }
function beaconLines(b) {
  if (!b) return say('= beacon -');
  say(`= beacon ${b.kind} ${hex(b.bssid)} ${hex(b.sa)} ${b.tsf} ${b.beaconInt} ${b.capability} ${B(b.privacy)} ${hex(b.ssidRaw)} ` +
    `${hex(Buffer.from(b.ssid, 'utf8'))} ${O(b.ch)} ${hex(Buffer.from(b.rateBytes))} ${b.rates.map((x) => x * 2).join(',') || '-'} ` +
    `${b.basicRates.map((x) => x * 2).join(',') || '-'} ${B(b.wpa)} ` +
    `${b.ht ? `${O(b.ht.capInfo)},${O(b.ht.primaryCh)},${O(b.ht.secondaryOffset)}` : '-'} ${O(b.dtimPeriod)} ` +
    `${b.country === null ? '-' : hex(Buffer.from(b.country, 'latin1'))}`);
  rsnLine(b.rsn);
  iesLine(b.ies);
}
function mgmtLine(m) {
  if (!m) return say('= mgmt -');
  hdrLine(m);
  const k = m.kind;
  if (k === 'auth') say(`= mgmt auth ${m.alg} ${m.authSeq} ${m.status}`);
  else if (k === 'assocResp') { say(`= mgmt assocResp ${m.capability} ${m.status} ${m.aid}`); iesLine(m.ies); }
  else if (k === 'deauth' || k === 'disassoc') say(`= mgmt ${k} ${m.reason}`);
  else say('= mgmt other');
}
function ethLine(e) {
  if (!e) return say('= eth -');
  say(`= eth ${hex(e.eth)} ${hex(e.dst)} ${hex(e.src)} ${hex(e.bssid)} ${e.type} ${B(e.eapol)} ${B(e.protected)} ${O(e.pn)} ` +
    `${O(e.keyId)} ${e.seq} ${B(e.retry)} ${B(e.qos)} ${e.tid} ${e.secIdx} ${B(e.multicast)}`);
}
const DESC = ['pktSize', 'drvInfoSize', 'longRxdesc', 'pktType', 'macInfoValid', 'bw', 'dataRate', 'giLtf', 'userId', 'srEn',
  'ppduCnt', 'ppduType', 'freeRunCnt', 'icvErr', 'crc32Err', 'hwDec', 'swDec', 'addr1Match', 'ampdu', 'shift', 'bbSel',
  'shiftField', 'offset', 'rxdLen', 'ready', 'frameType', 'addrCamValid', 'addrCamId', 'secCamId', 'macId', 'rxPlId', 'secType',
  'copyLen'];
const descStr = (d) => (d ? `${DESC.map((k) => N(Number(d[k] ?? 0))).join(' ')} ${O(d.writeSize)}` : '-');
function ppduStr(p) {
  if (!p) return '-';
  return `${p.start} ${p.len} ${B(p.valid)} ${B(p.toSelf)} ${p.rate} ${p.macId} ${p.phyIdx} ${p.rssi.join(',')} ${p.rssiAvg} ` +
    `${p.rplAvg} ${p.chanIdx} ${p.ie} ${p.rxPathEn} ${B(p.ofdm.has)},${p.ofdm.avgSnr},${p.ofdm.evmMax},${p.ofdm.evmMin} ` +
    `${B(p.hasData)}${B(p.hasBcn)} ${O(p.ie09)} ${O(p.ie10)} ${'hdr2En' in p ? B(p.hdr2En) : '-'} ${B(p.ldpc)}${B(p.stbc)}${B(p.bf)}${B(p.su)} ` +
    `${O(p.cfo)}`;
}
function statusStr(s) {
  return `${BAND(s.band)} ${O(s.ch)} ${O(s.freq)} ${B(s.failedFcs)}${B(s.decrypted)}${B(s.ampdu)} ${O(s.ampduRef)} ${s.bw} ` +
    `${s.signal} ${s.chains} ${s.chains ? s.chainSignal.join(',') : '-'} ${s.encoding} ${s.rateIdx} ${O(s.nss)} ${B(s.shortGi)} ` +
    `${s.heGi} ${s.mactime} ${B(s.noSignal)}`;
}
function c2hStr(c) {
  if (!c) return '-';
  const a = c.ack ? `${c.ack.kind},${c.ack.cat},${c.ack.cls},${c.ack.func},${c.ack.seq},${O(c.ack.ret)}` : '-';
  return `${c.cat} ${c.cls} ${c.func} ${c.len} ${O(c.seq)} ${hex(c.payload)} ${a}`;
}
function deliverLines(list) {
  for (const e of list) {
    say(`= d ${descStr(e.desc)}`);
    say(`= d frame ${hex(e.frame)} match ${B(e.ppduMatch)} pdesc ${e.ppduDesc ? `${e.ppduDesc.pktType},${e.ppduDesc.ppduCnt}` : '-'}`);
    say(`= d ppdu ${ppduStr(e.ppdu)}`);
    say(`= d status ${statusStr(e.status)}`);
  }
}
function rxLines(res, stats) {
  say(`= rx ${res.type} ${O(res.reason)} ${res.queued ? res.pending.length : '-'} ${res.frame ? hex(res.frame) : '-'}`);
  say(`= rx desc ${descStr(res.desc)}`);
  say(`= rx ppdu ${ppduStr(res.ppdu)}`);
  say(`= rx c2h ${c2hStr(res.c2h)}`);
  deliverLines(res.deliver ?? []);
  for (const s of stats.splice(0)) say(`= stats ${s}`);
}

// ---- RX chip ops: none, the test's stand-ins, or phy.mjs's own ----
function makeOps(kind, stats) {
  const rxStats = (ent, p, dsc) => stats.push(`${p ? 1 : 0} ${dsc ? dsc.pktType : '-'} ${hex(ent.frame).slice(0, 16)}`);
  if (kind === 'none') return {};
  if (kind === 'standin') {
    return {
      convertRplToRssi: (p) => { const delta = (p.rplAvg - p.rssiAvg) & 0xff; p.rssi = p.rssi.map((v, i) => (i < 2 ? (v + delta) & 0xff : v)); p.rssiAvg = p.rplAvg; },
      queryPpdu: (p, st) => { st.signal = (Math.max(p.rssi[0], p.rssi[1]) >> 1) - 110; st.chains = 3; st.chainSignal = [(p.rssi[0] >> 1) - 110, (p.rssi[1] >> 1) - 110]; },
      rxStats,
    };
  }
  return { convertRplToRssi: phy.convertRplToRssi, queryPpdu: phy.queryPpdu, rxStats };
}

// ---- RX buffers ----
const RATES = () => pick([r(12), r(12), 0x80 | r(32), 0x100 | r(0x80), 0x180 | r(0x80), r(0x200)]);
// pair: { bw, gi, rate, cnt } shared by a frame and its PPDU status (so they can match)
function desc({ long, type, len, macInfo = false, drv = 0, pair = null, frameType = null }) {
  const p = pair ?? { bw: r(4), gi: r(8), rate: RATES(), cnt: r(8) };
  const w0 = (long ? 0x80000000 : 0) | (type << 24) | (macInfo ? 1 << 23 : 0) | (drv << 28) | (chance(0.1) ? r(4) << 14 : 0) | (len & 0x3fff);
  const w1 = (p.bw << 30) | (p.gi << 25) | (p.rate << 16) | (chance(0.1) ? r(256) << 8 : 0) | (chance(0.1) ? 0x80 : 0) | (p.cnt << 4) | r(16);
  const w3 = [1, 2, 4, 8, 0x200, 0x400].reduce((a, b) => a | (chance(0.25) ? b : 0), 0);
  const w = [w0, w1, r(0x1000000), w3];
  if (long) w.push(frameType ?? r(4), r(0x20000000), r(0x10000), r(0x1000000));
  return Buffer.concat([le32(...w), bytes(drv * 8)]);
}
// a PPDU status packet: MAC info (users, RX counters, PLCP), PHY status header, IEs
function ppduPkt() {
  const usr = pick([0, 1, 1, 2, 3, 4, 5]), rxCnt = chance(0.2), plcp = pick([0, 0, 1, 2]);
  const mac = [le32((rxCnt ? 1 << 29 : 0) | usr | (chance(0.1) ? r(0x10000) << 8 : 0), (plcp << 16) | r(0x10000))];
  for (let i = 0; i < usr; i++) mac.push(le32((chance(0.7) ? 1 : 0) | (chance(0.5) ? 0x10 : 0) | (chance(0.5) ? 0x40 : 0) | (r(256) << 8)));
  if (usr & 1) mac.push(le32(0));
  if (rxCnt) mac.push(bytes(0x60));
  if (plcp) mac.push(bytes(plcp * 8));
  const hdr2 = chance(0.2);
  const iesl = [];
  for (let n = r(4); n > 0; n--) {
    const type = pick([0, 0, 1, 1, 9, 10, 4, 2, 8, 11, 23, r(32)]);
    let body;
    const fixed = [16, 32, 24, 24, 8, 8, 8, 8, 0xff, 8, 0xff, 176, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 16, 24, 0xff, 0xff, 0xff, 0, 24, 24, 24, 24, 32, 32, 32, 32][type];
    const vl = fixed === 0xff ? 1 + r(3) : 0;
    const len = fixed === 0xff ? vl * 8 : fixed;
    body = bytes(Math.max(len, 4));
    const w0 = body.readUInt32LE(0);
    body.writeUInt32LE(((w0 & ~0xfff) | type | (vl << 5)) >>> 0, 0);
    if (chance(0.1)) body = body.subarray(0, r(body.length));
    iesl.push(body);
  }
  if (chance(0.1)) iesl.push(Buffer.alloc(8));
  const iesb = Buffer.concat(iesl);
  const phyLen = 8 + (hdr2 ? 8 : 0) + iesb.length;
  const units = chance(0.85) ? phyLen >> 3 : r(64);
  const w0 = (pick([11, 12, 13, 14, 15, 0, r(16)])) | (chance(0.9) ? 0x80 : 0) | (hdr2 ? 0x40 : 0) | ((units & 0xff) << 8) | (r(256) << 24) | (r(4) << 4);
  const phyh = Buffer.concat([le32(w0, r(0x100000000)), hdr2 ? le32(r(0x100000000), r(0x100000000)) : Buffer.alloc(0)]);
  return Buffer.concat([...mac, phyh, iesb]);
}
function c2hPkt() {
  const n = r(16);
  return Buffer.concat([le32(r(4) | (r(64) << 2) | (r(256) << 8), (8 + n + (chance(0.2) ? r(8) : 0)) & 0x3fff, r(0x100000000)), bytes(n)]).subarray(0, chance(0.1) ? r(12) : 8 + 4 + n);
}
// one or more ring buffers (data after the rxbd_info) for one packet
function rxBuffers() {
  const k = r(20);
  if (k === 19 || k === 18) {                                   // a frame and its PPDU status
    const pair = { bw: r(2), gi: pick([0, 0, 1, 7]), rate: pick([0, 1, 4, 11, 0x80 | r(8), r(0x200)]), cnt: r(8) };
    const f = Buffer.concat([anyFrame(), bytes(4)]), pp = ppduPkt();
    const a = Buffer.concat([desc({ long: true, type: 0, len: f.length, pair, frameType: pick([0, 2, 2, 1]) }), f]);
    const b = Buffer.concat([desc({ long: chance(0.5), type: 1, len: pp.length, macInfo: chance(0.9), pair }), pp]);
    return [a, b].map((data) => ({ data, fs: true, ls: true, len: data.length + 4 }));
  }
  if (k === 0) return [{ data: bytes(r(80)), fs: chance(0.7), ls: chance(0.7), len: chance(0.9) ? null : r(100) }];
  let type, body, long = chance(0.6), macInfo = false;
  if (k < 10) { type = 0; body = Buffer.concat([anyFrame(), bytes(4)]); }
  else if (k < 15) { type = 1; body = ppduPkt(); macInfo = chance(0.9); }
  else if (k < 18) { type = 10; body = c2hPkt(); }
  else { type = pick([2, 3, 7, r(16)]); body = bytes(r(30)); }
  const drv = chance(0.15) ? r(4) : 0;
  const len = chance(0.9) ? body.length : r(0x4000);
  const pkt = Buffer.concat([desc({ long, type, len, macInfo, drv }), body]);
  const total = (b) => (chance(0.92) ? b.length + 4 : r(b.length + 40));
  if (chance(0.15) && pkt.length > 20) {                        // segments
    const cut = 4 + r(pkt.length - 8), a = pkt.subarray(0, cut), b = pkt.subarray(cut);
    const segs = [{ data: a, fs: true, ls: false }, { data: b, fs: false, ls: true }];
    // a first segment longer than its RPKT_LEN, or a continuation past it
    if (chance(0.2)) segs[0].data = Buffer.concat([pkt.subarray(0, Math.min(pkt.length, 40)), bytes(r(60))]);
    if (chance(0.2)) segs[r(2)][pick(['fs', 'ls'])] ^= true;
    return segs.map((s) => ({ ...s, len: total(s.data) }));
  }
  return [{ data: pkt, fs: true, ls: true, len: chance(0.1) ? null : total(pkt) }];
}

// ---- the run ----
function txNew() {
  const chan = chance(0.8) ? { band: pick(['2g', '2g', '5g', '6g']), ch: pick([1, 6, 11, 36]) } : null;
  const macId = chance(0.5) ? r(4) : null;
  const rpt = chance(0.3);
  say(`> txnew ${chan ? `${chan.band},${chan.ch}` : '-'} ${O(macId)} ${B(rpt)}`);
  const d = { txrx: { txRptEnabled: rpt }, chan };
  if (macId !== null) d.sta = { macId };
  return d;
}
function txOpts() {
  const o = {};
  const s = [];
  if (chance(0.5)) { o.macId = r(8); s.push(`mac_id=${o.macId}`); }
  if (chance(0.5)) { o.sta = true; s.push('sta=1'); }
  if (chance(0.5)) { o.basicRates = pick([0xf, 0xff0, 0x3, 0x10, r(0x1000)]); s.push(`basic=${o.basicRates}`); }
  if (chance(0.5)) { o.suppRates = pick([0xfff, 0xff0, 0x1, r(0x1000)]); s.push(`supp=${o.suppRates}`); }
  if (chance(0.4)) { o.band = pick(['2g', '5g', '6g']); s.push(`band=${o.band}`); }
  if (chance(0.35)) {
    o.key = { keyIdx: r(4), cipher: chance(0.9) ? 'CCMP' : 'TKIP' };
    if (chance(0.9)) o.key.secCamIdx = r(16);
    s.push(`key=${o.key.keyIdx},${O(o.key.secCamIdx)},${o.key.cipher === 'CCMP' ? 0x000fac04 : 0x000fac02}`);
  }
  if (chance(0.4)) { o.tid = r(16); s.push(`tid=${o.tid}`); }
  if (chance(0.2)) { o.noCck = true; s.push('nocck=1'); }
  if (chance(0.3)) { o.reqTxStatus = true; s.push('rpt=1'); }
  if (chance(0.1)) { o.erCap = true; o.stbc = true; o.ldpc = true; s.push('ersl=1'); }
  return [o, s.join(' ')];
}

for (let round = 0; round < 6; round++) {
  // TX infos and sequence numbers on one state
  let d = txNew();
  for (let i = 0; i < 70; i++) {
    if (i % 25 === 24) d = txNew();
    // mostly any frame; sometimes a DHCP-like one around rtw89's 282-byte limit for coex
    const f = chance(0.85) ? anyFrame() : Buffer.concat([u16le(T.fcOf(2, pick([0, 8]), 0x100)), u16le(0), mac(), mac(), mac(), u16le(0),
      LLC, u16be(0x0800), Buffer.from([0x45, 0, 0, 0, 0, 0, 0, 0, 0, 17, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
      u16be(pick([67, 68])), u16be(pick([68, 67])), Buffer.alloc(pick([223, 224, 225, 226, 227]))]);
    const [o, os] = txOpts();
    const kind = pick(['-', '-', '-', 'mgmt', 'data', 'fwcmd']);
    if (chance(0.25)) {
      if (chance(0.3)) { d.txrx.lastActrl = true; say('> actrl'); }
      say(`> txseq ${hex(f)}`);
      const g = Buffer.from(f);
      try { T.txSeq(d, g); say(`= seq ${hex(g)} ${d.txrx.seqCtrl} ${d.txrx.tidSeq.join(',')}`); } catch { say('= error'); }
      continue;
    }
    say(`> txinfo ${kind} ${hex(f)} ${os}`);
    try { txInfoLines(T.txInfo(d, kind === '-' ? null : kind, f, o)); say(`= txstate ${N(d.txrx.txRptSn)} ${B(d.txrx.lastActrl)}`); } catch { say('= error'); }
  }
  // the station's frames
  for (let i = 0; i < 25; i++) {
    const own = mac(), bssid = mac(), seq = r(0x1000), band = pick(BANDS);
    const k = r(6);
    if (k === 0) {
      const ssid = chance(0.2) ? bytes(r(40)) : Buffer.from(pick(['', 'AB', 'BitsOS-Fake'])), ch = pick([0, 1, 6, 36]), dst = mac();
      say(`> probe ${hex(own)} ${hex(ssid)} ${band} ${ch} ${hex(dst)} ${hex(bssid)} ${seq}`);
      say(`= ${hex(T.probeReq({ own, ssid, band, ch, dst, bssid, seq }))}`);
    } else if (k === 1) {
      say(`> auth ${hex(own)} ${hex(bssid)} ${seq}`);
      say(`= ${hex(T.authOpen({ own, bssid, seq }))}`);
    } else if (k === 2) {
      const ssid = Buffer.from(pick(['x', 'FRITZ!Box 7590 XY', ''])), rates = chance(0.8) ? Buffer.from(Array.from({ length: r(12) }, () => pick([0x82, 0x84, 0x8b, 0x96, 0x0c, 0x12, 0x18, 0x24, 0x30, 0x48, 0x60, 0x6c, 0x8c, 0x98, 0xb0, r(256)]))) : null;
      const cap = pick([0, 0x10, 0x431, r(0x10000)]), rsn = chance(0.7) ? T.rsnIe() : null, li = pick([5, 10, r(0x10000)]);
      say(`> assoc ${hex(own)} ${hex(bssid)} ${hex(ssid)} ${band} ${rates ? hex(rates) : '*'} ${cap} ${rsn ? hex(rsn) : '-'} ${li} ${seq}`);
      say(`= ${hex(T.assocReq({ own, bssid, ssid, band, apRateBytes: rates ? [...rates] : null, apCapability: cap, rsn, listenInterval: li, seq }))}`);
    } else if (k === 3) {
      const reason = r(0x10000);
      say(`> deauth ${hex(own)} ${hex(bssid)} ${reason} ${seq}`);
      say(`= ${hex(T.deauth({ own, bssid, reason, seq }))}`);
    } else if (k === 4) {
      const qos = chance(0.5), tid = r(16);
      say(`> null ${hex(own)} ${hex(bssid)} ${seq} ${qos ? tid : '-'}`);
      say(`= ${hex(T.nullFunc({ own, bssid, seq, qos, tid }))}`);
    } else {
      const suites = () => Array.from({ length: r(3) }, () => (0x000fac00 | r(16)) >>> 0);
      const group = (0x000fac00 | r(16)) >>> 0, pw = suites(), ak = suites(), caps = r(0x10000);
      const s = (a) => a.map((x) => x.toString(16)).join(',') || '-';
      say(`> rsnie ${group.toString(16)} ${s(pw)} ${s(ak)} ${caps}`);
      say(`= ${hex(T.rsnIe({ group, pairwise: pw, akms: ak, caps }))}`);
    }
  }
  // data frames both ways, replays
  let key = { keyIdx: r(4), txPn: BigInt(r(1000)) };
  say(`> pnkey ${key.keyIdx} ${key.txPn}`);
  for (let i = 0; i < 20; i++) {
    const eth = chance(0.1) ? bytes(r(14)) : Buffer.concat([mac(), mac(), u16be(pick([0x0800, 0x0806, 0x888e, 0x8137, 0x80f3, 0x05dc, r(0x10000)])), bytes(r(50))]);
    const useKey = chance(0.5), qos = chance(0.3) ? r(16) : null, seq = r(0x1000), bssid = mac();
    say(`> e2w ${hex(eth)} ${hex(bssid)} ${B(useKey)} ${O(qos)} ${seq}`);
    try {
      const w = T.ethToWlan(eth, { bssid, key: useKey ? key : null, qos: qos !== null, tid: qos ?? 0, seq });
      say(`= ${hex(w)} ${key.txPn}`);
    } catch { say('= error'); }
  }
  for (let i = 0; i < 40; i++) {
    const f = chance(0.7) ? dataFrame() : anyFrame();
    say(`> w2e ${hex(f)}`);
    ethLine(T.wlanToEth(f));
  }
  const rk = { rsc: BigInt(r(50)) };
  say(`> rkey ${rk.rsc}`);
  for (let i = 0; i < 30; i++) {
    const sec = r(17), pn = BigInt(r(100));
    say(`> replay ${sec} ${pn}`);
    say(`= ${B(T.ccmpReplay(rk, sec, pn))} ${rk.replays ?? 0}`);
  }
  // parsers
  for (let i = 0; i < 40; i++) {
    const f = r(3) ? beaconFrame() : anyFrame();
    say(`> parse ${hex(f)}`);
    beaconLines(T.beaconParse(f));
    mgmtLine(T.mgmtParse(f));
    hdrLine(T.hdrParse(f));
    const b = T.parseIes(f, 36, f.length);
    iesLine(b);
    say(`= chan ${T.iesChannel(b)}`);
  }
  for (let i = 0; i < 20; i++) {
    const b = rsnBody();
    say(`> rsn ${hex(b)}`);
    rsnLine(T.rsnParse(b));
  }
  for (let i = 0; i < 10; i++) {
    const rb = bytes(r(14)), band = pick(BANDS), basic = chance(0.5);
    say(`> bitmap ${hex(rb)} ${band} ${B(basic)}`);
    say(`= ${T.rateBitmap([...rb], band, basic)}`);
  }
  // RX: several states, chip ops
  for (let st = 0; st < 3; st++) {
    const chan = chance(0.85) ? { band: pick(['2g', '5g', '2g']), ch: pick([1, 6, 36, 149, 14]) } : null;
    if (chan && chance(0.4)) { chan.prevBand = pick(BANDS); chan.prevCh = pick([0, 6, 11]); }
    const scanning = chance(0.5), opsKind = pick(['none', 'standin', 'phy']);
    say(`> rxnew ${chan ? `${chan.band},${chan.ch},${chan.prevBand ?? '-'},${chan.prevCh ?? '-'}` : '-'} ${B(scanning)} ${opsKind}`);
    const dev = { chan, sta: { scanning } };
    const stats = [];
    const ops = makeOps(opsKind, stats);
    for (let i = 0; i < 70; i++) {
      for (const b of rxBuffers()) {
        say(`> rx ${hex(b.data)} ${B(b.fs)} ${B(b.ls)} ${O(b.len)}`);
        const res = T.rxParse(dev, b.len === null ? b.data : { data: b.data, fs: b.fs, ls: b.ls, len: b.len }, { ops });
        rxLines(res, stats);
      }
      if (chance(0.02)) {
        say('> rxflush');
        deliverLines(T.rxFlush(dev, { ops }));
        for (const s of stats.splice(0)) say(`= stats ${s}`);
      }
      if (chance(0.02)) {                                         // pciTxInit's RX half (rtw89_pci_reset_trx_rings)
        say('> rxreset');
        dev.txrx ??= {};
        dev.txrx.rxSeg = null;
        if (dev.txrx.rxDesc) dev.txrx.rxDesc.ready = false;
      }
    }
  }
}
console.log(out.join('\n'));
