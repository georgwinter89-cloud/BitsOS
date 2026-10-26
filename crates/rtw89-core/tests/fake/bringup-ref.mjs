// bringup-ref.mjs: the JS driver's whole bring-up on the fake Mini-PC, exactly as
// test-bringup.mjs runs it for bringup-golden.txt (fresh efuse, channels 1/6/11, 20 ms dwell,
// the fake AP's network, its eapol factory and DHCP answer), with a look at every buffer the
// driver kicks on a TX ring: the firmware commands on CH12 (WD body and H2C) and the frames
// (WD page and frame). Those bytes go to DMA memory untraced, so the golden file cannot hold
// them; tests/fake_golden.rs needs them where it replays sta's part of the trace (sta is not
// ported: its H2Cs must carry the JS driver's bytes, so V1's fake chip answers them as it did)
// and compares txrx's frames with them. The run's trace is checked against
// bringup-golden.txt first (as test-bringup.mjs does).
// Output: `ok <lines>`, then one line per kicked buffer:
//   rec <golden line of the BD's address word> <ring> <slot> <buffer address> <bytes hex> <frame hex|->
//   node bringup-ref.mjs      (WLAN_TOOLS: the read-only WLAN tree)
// Reads only; writes nothing but stdout.
import { readFileSync } from 'node:fs';
import { pathToFileURL } from 'node:url';

const WZ = process.env.WLAN_TOOLS ?? '/home/georg/BitsOS/werkzeuge/plattform/wlan';
const imp = (rel) => import(pathToFileURL(`${WZ}/${rel}`).href);
const file = (rel) => readFileSync(`${WZ}/${rel}`);
const { R } = await imp('kit/rtwdev.mjs');
const { bringup } = await imp('js/bringup.mjs');
const { FakeMiniPc, FAKE_AP, fakeApAnswer } = await imp('js/bringup-fake.mjs');
const { dhcpFrame, traceText } = await imp('js/bringup-steps.mjs');

const WD_BODY = 24, WD_INFO = 24, WP_INFO = 8;

// the fake Mini-PC, noting each kicked buffer before the chip takes it
class RefChip extends FakeMiniPc {
  constructor(o) { super(o); this.records = []; }
  async kick(addr, wp) {
    const ring = Object.values(this.pci?.tx ?? {}).find((r) => r.idx === addr);
    if (ring) {
      for (let i = this.seen[ring.name] ?? 0; i !== wp; i = (i + 1) % ring.len) {
        const len = this.u32(ring.bd + i * 8) & 0xffff, page = this.u32(ring.bd + i * 8 + 4);
        const rec = { at: this.trace.length, ring: ring.name, bd: ring.bd + i * 8, slot: i, page, bytes: this.bytes(page, len), frame: null };
        if (ring.name !== 'CH12') {
          const info = this.u32(page) & R('RTW89_TXWD_BODY0_WD_INFO_EN') ? WD_INFO : 0;
          const a = page + WD_BODY + info + WP_INFO;
          rec.frame = this.bytes(this.u32(a + 4), this.u32(a) & 0xffff);
        }
        this.records.push(rec);
      }
    }
    return super.kick(addr, wp);
  }
}

const d = new RefChip({ efuse: { phy: file('kit/efuse-phy.bin'), dav: file('kit/efuse-dav.bin') } });
const r = await bringup(d, { until: 'dhcp', fresh: true, fwFile: file('firmware/rtw8852b_fw-2.bin'),
  efuse: { log: file('kit/efuse-log.bin'), dav: file('kit/efuse-dav.bin') }, ssid: FAKE_AP.ssid, channels: [1, 6, 11],
  dwellMs: 20, eapol: d.eapolFactory(), log: () => {},
  onDhcpSent: (dev, frame) => fakeApAnswer(dev, dhcpFrame({ op: 2, src: dev.ap.bssid, dst: dev.sta.addr, mac: dev.sta.addr,
    xid: 0xB1750001, yiaddr: [192, 168, 0, 23] }).subarray(14)) });
if (!r.ok || !r.frames.some((f) => f.includes('DHCP offer'))) {
  console.log(`FAILED: the JS bring-up did not reach the DHCP offer (${r.error?.message})`);
  process.exit(1);
}

// test-bringup.mjs's normalisation (empty RX polls left out), keeping each line's op index
const EMPTY_POLL = [/^wr32 0x000010B4 0x00000000 0x00100004$/, /^rd32 0x00001054 /, /^wr32 0x000010B4 0x00000000 0x00080003$/,
  /^rd32 0x00001050 /];
const raw = traceText(d.trace, r.marks).split('\n').filter(Boolean)
  .map((l) => (l.startsWith('#') ? { op: null, text: l } : { op: Number(l.slice(0, l.indexOf(' '))), text: l.slice(l.indexOf(' ') + 1) }));
const norm = [];
for (let i = 0; i < raw.length; i++) {
  if (EMPTY_POLL.every((re, k) => re.test(raw[i + k]?.text ?? '')) && !/^mwr32/.test(raw[i + 4]?.text ?? '')) { i += 3; continue; }
  norm.push(raw[i]);
}
const golden = file('bringup-golden.txt').toString('latin1').split('\n').filter(Boolean);
const diff = norm.findIndex((l, i) => l.text !== golden[i]);
if (diff !== -1 || norm.length !== golden.length) {
  console.log(`FAILED: the JS run's trace differs from bringup-golden.txt at line ${diff + 1}`);
  process.exit(1);
}
console.log(`ok ${norm.length}`);
const h = (v) => `0x${(v >>> 0).toString(16).toUpperCase().padStart(8, '0')}`;
for (const rec of d.records) {
  // the last write of this BD's address word before the chip took it
  const want = `mwr32 ${h(rec.bd + 4)} 0x00000000 ${h(rec.page)}`;
  let line = -1;
  for (let i = 0; i < norm.length; i++) {
    if (norm[i].op !== null && norm[i].op >= rec.at) break;
    if (norm[i].text === want) line = i + 1;
  }
  console.log(`rec ${line} ${rec.ring} ${rec.slot} ${h(rec.page)} ${rec.bytes.toString('hex') || '-'} ${rec.frame ? rec.frame.toString('hex') || '-' : '-'}`);
}
