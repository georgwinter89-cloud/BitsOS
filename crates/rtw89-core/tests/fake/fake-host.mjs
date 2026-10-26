// fake-host.mjs: the JS fake chip for the Rust trace bus. Runs V1's
// wlan-fake.mjs fakeChip (FakeMiniPc of js/bringup-fake.mjs behind V1's
// 16-byte op protocol) against the Unix socket the Rust test listens on; when
// a line arrives on stdin, prints the fake's trace (traceText, as
// bringup-golden.txt has it with an index in front) and exits.
//   node fake-host.mjs <socket> [--warm] [--sta]
// --warm: the first read of R_AX_IC_PWR_STATE says the MAC is on (a warm boot);
// then, after a line "=== js ===", the trace the JS driver's own busMasterOn
// makes on a fake Mini-PC in the same state, for comparison.
// --sta: what test-bringup.mjs gives the JS bring-up besides the chip, for the
// station stages: the fake AP answers a DHCP discover with an offer (its
// onDhcpSent), and the station side of the WPA2 handshake (the fake's
// eapolFactory, wpa.mjs with the fake network's test PMK) answers on stdout:
//   eapol-open <spa> <aa> <ownIe> <apIe>   -> ok
//   eapol <EAPOL PDU hex>                  -> eapol <answer hex|-> <what it got|->
//   ap-log                                 -> ap-log <the fake AP's log, comma separated>
//   ap <move> ...                          -> ok, or what was asked (apCmd below: rekey, deauth,
//                                             disassoc, c2h, beacon, data, set, gtk, tk, sent, ofld, assoc)
//   dev-set <FakeMiniPc flag> <0|1>        -> ok (rfkillBlocked)
//   lan arp | lan stats                    -> (--lan) the LAN's broadcast ARP request; its counts
//   fault <what> ...                       -> ok: V1 wlan-robust-test.mjs's injections (d.regs,
//                                             d.onRead, d.onPoll, d.kick) for the driver's health:
//     halt <code hex>     the firmware posts a halt (HALT_C2H = code, HALT_C2H_CTRL = 1)
//     notready            the next read of WCPU_FW_CTRL gives 0x02
//     dead <0|1>          every MMIO and config read all ones, every poll fails (on/off)
//     deadlook            the RXQ index once all ones, then SYS_CFG1 once (one dead look)
//     rxq-past <n>        the next n reads of the RXQ index give 0x0FFF0000
//     txfreeze <0|1>      the TX rings (not CH12) take nothing: indices frozen, kicks ignored
//   reg <hex addr>                         -> reg <value hex> (what the chip holds, no trace)
// A line "done" ends the run as before.
// Reads only: V1 (BITSOS_V1) and the WLAN tools (WLAN_TOOLS, wlan-tools.mjs).
import { pathToFileURL } from 'node:url';

const V1 = process.env.BITSOS_V1 ?? '/home/georg/BitsOS/BitsOS-Plattform';
const { fakeChip, file } = await import(pathToFileURL(`${V1}/wlan-fake.mjs`).href);
const { wlanImport } = await import(pathToFileURL(`${V1}/wlan-tools.mjs`).href);
const { FakeMiniPc } = await wlanImport('js/bringup-fake.mjs');
const { busMasterOn, traceText, dhcpFrame } = await wlanImport('js/bringup-steps.mjs');
const { fakeApAnswer } = await wlanImport('js/bringup-fake.mjs');

const IC_PWR_STATE = 0x3f0, MAC_ON = 0x100;          // B_AX_WLMAC_PWR_STE_MASK 0x300, MAC_AX_MAC_ON 1
const [sock, ...flags] = process.argv.slice(2);
const warm = flags.includes('--warm');
const sta = flags.includes('--sta');
// --lan <port>: V1's LAN (net-lan.mjs: DHCP, ARP, ping, DNS, HTTP) behind the fake AP through V1's
// lanBridge (wlan-fake.mjs), on UDP ports port and port + 1 of 127.0.0.1 (wlan-net-test.mjs's setup)
const lanAt = flags.indexOf('--lan');
const lanPort = lanAt >= 0 ? Number(flags[lanAt + 1]) : null;
let net = null, bridge = null;
if (lanPort) {
  const { lan } = await import(pathToFileURL(`${V1}/net-lan.mjs`).href);
  const { lanBridge } = await import(pathToFileURL(`${V1}/wlan-fake.mjs`).href);
  net = lan(lanPort);
  bridge = lanBridge(lanPort);
}

// test-bringup.mjs's onDhcpSent: the AP's DHCP offer for a discover (UDP 68 -> 67) of the station
function dhcpAnswer(d, f) {
  const fc = f.readUInt16LE(0);
  if (((fc >> 2) & 3) !== 2 || f.length < 24) return;
  const o = 24 + (fc & 0x4000 ? 8 : 0);
  if (f.length < o + 8 + 24 || f.readUInt16BE(o + 6) !== 0x0800) return;
  const ip = o + 8, udp = ip + (f[ip] & 0xf) * 4;
  if (f[ip + 9] !== 17 || f.readUInt16BE(udp) !== 68 || f.readUInt16BE(udp + 2) !== 67) return;
  const addr = Buffer.from(f.subarray(10, 16));
  fakeApAnswer(d, dhcpFrame({ op: 2, src: d.ap.bssid, dst: addr, mac: addr, xid: 0xB1750001, yiaddr: [192, 168, 0, 23] }).subarray(14));
}

// the first IC_PWR_STATE read says "MAC on"; later ones the fake's own answer
function warmUp(d) {
  let first = true;
  const answer = d.onRead;
  d.onRead = (kind, a, size) => {
    if (kind === 'mmio' && a === IC_PWR_STATE && first) { first = false; return MAC_ON; }
    return answer(kind, a, size);
  };
}

const chip = await fakeChip(sock, sta ? { onTx: dhcpAnswer } : bridge ? { onTx: bridge.onTx } : {});
if (chip.gone) {
  console.error(`fake-host: no connection to ${sock}`);
  process.exit(2);
}
if (warm) warmUp(chip.d);

// the fake AP driven by a Rust test (sta's life cycle): sta-testlink.mjs's moves, one per line
const hx = (x) => Buffer.from(x === '-' ? '' : x, 'hex');
const le16 = (v) => { const b = Buffer.alloc(2); b.writeUInt16LE(v, 0); return b; };
async function apCmd([what, ...b]) {
  const ap = chip.d.ap;
  const to = (x) => (x === 'sta' ? ap.sta : x === 'bc' ? Buffer.alloc(6, 0xff) : hx(x));
  switch (what) {
    case 'rekey': ap.rekey(b[0] === '-' ? null : hx(b[0])); break;
    case 'deauth': ap.inject(Buffer.concat([ap.mgmt(12, to(b[1]), Buffer.alloc(0)), le16(Number(b[0]))])); break;
    case 'disassoc': ap.inject(Buffer.concat([ap.mgmt(10, to(b[1]), Buffer.alloc(0)), le16(Number(b[0]))])); break;
    case 'c2h': ap.inject(hx(b[0]), { c2h: true }); break;
    case 'beacon': {
      const old = ap.bssid;
      if (b[0]) ap.bssid = hx(b[0]);
      ap.beacon();
      ap.bssid = old;
      break;
    }
    case 'data': {
      const [da, et, payload, protect, keyId, pn, hwDec, retry, seq] = b;
      ap.dataTo(to(da), parseInt(et, 16), hx(payload), { protect: protect === '1', keyId: Number(keyId), pn: BigInt(pn),
        hwDec: hwDec === '1', retry: retry === '1', seq: seq === '-' ? null : seq === 'last' ? ap.lastSeq : Number(seq) });
      break;
    }
    case 'set': ap[b[0]] = b[1] === '1'; break;
    case 'gtk': return `ap gtk ${ap.gtk.toString('hex')} ${ap.gtkId}`;
    case 'tk': return `ap tk ${ap.ptk ? ap.ptk.subarray(32, 48).toString('hex') : '-'}`;
    case 'sent': return `ap sent ${ap.data.map((x) => `${x.prot ? 'p' : 'c'}:${x.et.toString(16)}:${x.eth.length}`).join(',') || '-'} ` +
      `${ap.lastPn ? `${ap.lastPn.pn}:${ap.lastPn.keyId}:${ap.lastPn.ext ? 1 : 0}` : '-'}`;
    case 'ofld': return `ap ofld ${[...ap.ofld.entries()].map(([k, v]) => `${k}:${v.toString('hex')}`).join(',') || '-'} ${ap.errors.length}`;
    case 'assoc': return `ap assoc ${(ap.assocRates ?? []).join(',')} ${ap.staIe ? ap.staIe.toString('hex') : '-'}`;
    default: return `unknown ap ${what}`;
  }
  return 'ok';
}

// V1 wlan-robust-test.mjs's fault injections, on top of the fake's own answers
const R = { HALT_C2H_CTRL: 0x164, HALT_C2H: 0x16C, WCPU_FW_CTRL: 0x1E0, SYS_CFG1: 0xF0 };
const own = { read: chip.d.onRead, kick: null };
let faults = { dead: false, notReady: 0, rx: 0, cfg: 0, past: 0, frozen: null };
chip.d.onRead = (kind, a, size) => {
  const f = faults;
  if (f.dead && (kind === 'mmio' || kind === 'cfg')) return 0xffffffff;
  if (kind === 'mmio') {
    const rxq = chip.d.pci?.rx?.RXQ?.idx;
    if (a === R.WCPU_FW_CTRL && f.notReady > 0) { f.notReady--; return 0x02; }
    if (a === rxq && f.rx > 0) { f.rx--; f.cfg = 1; return 0xffffffff; }
    if (a === R.SYS_CFG1 && f.cfg > 0) { f.cfg--; return 0xffffffff; }
    if (a === rxq && f.past > 0) { f.past--; return 0x0FFF0000; }
    if (f.frozen?.has(a)) { const r = f.frozen.get(a); return ((r.hw << 16) | r.ring.wp) >>> 0; }
  }
  return own.read(kind, a, size);
};
function fault([what, x]) {
  const on = x === '1';
  switch (what) {
    case 'halt':
      chip.d._set(chip.d.regs, R.HALT_C2H, 4, parseInt(x, 16) >>> 0);
      chip.d._set(chip.d.regs, R.HALT_C2H_CTRL, 4, 1);
      return 'ok';
    case 'notready': faults.notReady = 1; return 'ok';
    case 'dead':
      faults.dead = on;
      chip.d.onPoll = on ? () => false : null;
      return 'ok';
    case 'deadlook': faults.rx = 1; return 'ok';
    case 'rxq-past': faults.past = Number(x); return 'ok';
    case 'txfreeze': {
      if (!on) { faults.frozen = null; delete chip.d.kick; return 'ok'; }
      const rings = Object.values(chip.d.pci.tx).filter((r) => r.name !== 'CH12');
      faults.frozen = new Map(rings.map((r) => [r.idx, { ring: r, hw: r.wp }]));
      const proto = Object.getPrototypeOf(chip.d);
      chip.d.kick = async (addr, wp) => (faults.frozen?.has(addr) ? undefined : proto.kick.call(chip.d, addr, wp));
      return 'ok';
    }
    default: return `unknown fault ${what}`;
  }
}

let input = '';
let eapol = null;
let work = Promise.resolve();
const reply = (l) => new Promise((done) => process.stdout.write(`${l}\n`, done));
async function line(l) {
  const [cmd, ...a] = l.trim().split(' ');
  if (cmd === 'eapol-open') {
    const [spa, aa, ownIe, apIe] = a.map((x) => Buffer.from(x === '-' ? '' : x, 'hex'));
    eapol = await chip.d.eapolFactory()({ spa, aa, ownIe, apIe });
    return reply('ok');
  }
  if (cmd === 'eapol') {
    const res = await eapol(Buffer.from(a[0], 'hex'));
    return reply(`eapol ${res.answer ? res.answer.toString('hex') : '-'} ${res.got ? res.got.replace(/ /g, '_') : '-'}`);
  }
  if (cmd === 'ap-log') return reply(`ap-log ${chip.d.ap.log.join(',')}`);
  if (cmd === 'ap') return reply(await apCmd(a));
  if (cmd === 'lan') {
    if (!net) return reply('no lan');
    if (a[0] === 'arp') { net.arpGuest(); return reply('ok'); }
    if (a[0] === 'stats') return reply(`lan ${net.arpReplies.length} ${net.replies.length} ${net.queries.join(',') || '-'} ${bridge.up} ${bridge.down}`);
    return reply(`unknown lan ${a[0]}`);
  }
  if (cmd === 'fault') return reply(fault(a));
  if (cmd === 'reg') return reply(`reg ${(chip.d._get(chip.d.regs, parseInt(a[0], 16), 4) >>> 0).toString(16)}`);
  if (cmd === 'dev-set') {
    chip.d[a[0]] = a[1] === '1';
    return reply('ok');
  }
  if (cmd !== 'done') return reply(`unknown ${cmd}`);
  process.stdin.pause();
  await chip.idle();
  let out = chip.trace();
  if (warm) {
    const d = new FakeMiniPc({ efuse: { phy: file('kit/efuse-phy.bin'), dav: file('kit/efuse-dav.bin') } });
    warmUp(d);
    d.log = () => {};
    await busMasterOn(d, () => {});
    await d.flush();
    out += '=== js ===\n' + traceText(d.trace, []);
  }
  net?.close();
  bridge?.close();
  process.stdout.write(out, () => { chip.close(); process.exit(0); });
}
process.stdin.setEncoding('latin1');
process.stdin.on('data', (s) => {
  input += s;
  let i;
  while ((i = input.indexOf('\n')) >= 0) {
    const l = input.slice(0, i);
    input = input.slice(i + 1);
    work = work.then(() => line(l));
  }
});
