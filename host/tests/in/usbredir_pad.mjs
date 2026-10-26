// IN-2b: a USB gamepad for QEMU over usb-redir, taken from V1's usbredir-pad.mjs (BitsOS-Plattform, 03.10.2026)
// with three pads: Georg's DragonRise 0079:0006 (low speed, 8-byte reports, bInterval 10, an OUT endpoint 0x01),
// the same at high speed with bInterval 7 (8 ms), and a DualShock 3 054C:0268 that reports nothing before
// GET_REPORT of its feature report 0xF2. Unlike V1's model, a string index a pad does not name is answered as the
// real DragonRise answers it (255 bytes of its memory, spec usb.md §2), and every string request is logged, so
// pad_check.py can tell that V2 asks for none.
// Run as a program it reads JSON lines on stdin and answers one JSON line each:
//   {"cmd":"plug","port":P,"pad":"dragonrise"|"highpad"|"ds3"}  {"cmd":"report","port":P,"hex":"7f7f..."}
//   {"cmd":"unplug","port":P}  {"cmd":"state","port":P} -> {"ready":bool,"log":[...]}
// QEMU side: -chardev socket,id=X,host=127.0.0.1,port=P,server=on,wait=off,nodelay=on -device usb-redir,chardev=X
// (QEMU attaches a redirected device to an xHCI only with 64-bit ids, 32-bit bulk lengths and endpoint packet
// sizes announced, hw/usb/redirect.c usbredir_do_attach, so every header after the two hellos is 16 bytes).
import net from 'node:net';
import readline from 'node:readline';
const T = { hello: 0, connect: 1, disconnect: 2, reset: 3, ifInfo: 4, epInfo: 5,
  setConfig: 6, getConfig: 7, configStatus: 8, setAlt: 9, getAlt: 10, altStatus: 11,
  startIntr: 15, stopIntr: 16, intrStatus: 17, cancel: 21, disconnectAck: 24,
  control: 100, interrupt: 103 };
const OK = 0, STALL = 4;
const CAPS = 1 << 1 | 1 << 4 | 1 << 5 | 1 << 6;  // device version, ep packet size, 64-bit ids, 32-bit bulk

const hex = (h) => Buffer.from(h.replace(/\s+/g, ''), 'hex');
const str = (s) => { const b = Buffer.alloc(2 + 2 * s.length); b[0] = b.length; b[1] = 3; b.write(s, 2, 'utf16le'); return b; };

export const DRAGONRISE = {
  speed: 0,                                   // low
  device: hex('12 01 00 01 00 00 00 08 79 00 06 00 07 01 01 02 00 01'),
  report: hex(`05010904a101a10275089505150026ff
    00350046ff0009300931093209320935
    8102750495012507463b016514093981
    4265007501950c250145010509190129
    0c81020600ff75019508250145010901
    8102c0a1027508950746ff0026ff0009
    029102c0c0`),
  strings: ['DragonRise Inc.  ', 'Generic   USB  Joystick  '],
  idle: hex('7F 7F 7F 7F 7F 0F 00 C0'),
  interval: 10,
  out: 0x01,
};

// Georg's descriptor at high speed, polled every 2^(7-1) x 125 us = 8 ms
// as its descriptor asks (V2 wants 1 ms: bInterval 4).
export const HIGHPAD = {
  ...DRAGONRISE,
  speed: 2,
  device: hex('12 01 00 02 00 00 00 40 09 12 09 50 00 01 01 02 00 01'),
  strings: ['BitsOS test', 'High-speed pad'],
  interval: 8,
  bInterval: 7,
};

// A Sony DualShock 3 (054C:0268) with its report descriptor of 148 bytes:
// report ID 1, 49 bytes, 19 buttons from bit 16 of which the first 16
// count (Select, L3, R3, Start, the D-pad up right down left, L2, R2, L1,
// R1, triangle, circle, cross, square), the sticks at bytes 6 to 9. As the
// real one it sends nothing before GET_REPORT of its feature report 0xF2
// (Linux, hid-sony.c). Full speed; it reports every 10 ms (the real one
// every 1 ms).
const ds3Idle = Buffer.alloc(49);
ds3Idle[0] = 1;
ds3Idle.fill(0x80, 6, 10);
export const DUALSHOCK3 = {
  speed: 1,
  device: hex('12 01 00 02 00 00 00 40 4c 05 68 02 00 01 01 02 00 01'),
  report: hex(`05 01 09 04 a1 01 a1 02 85 01 75 08 95 01 15 00 26 ff 00 81 03
    75 01 95 13 15 00 25 01 35 00 45 01 05 09 19 01 29 13 81 02 75 01 95 0d 06 00 ff 81 03
    15 00 26 ff 00 05 01 09 01 a1 00 75 08 95 04 35 00 46 ff 00 09 30 09 31 09 32 09 35 81 02 c0
    05 01 95 13 09 01 81 02 95 0c 81 01 75 10 95 04 26 ff 03 46 ff 03 09 01 81 02 c0
    a1 02 85 02 75 08 95 30 09 01 b1 02 c0 a1 02 85 ee 75 08 95 30 09 01 b1 02 c0
    a1 02 85 ef 75 08 95 30 09 01 b1 02 c0 c0`),
  strings: ['Sony', 'PLAYSTATION(R)3 Controller'],
  idle: ds3Idle,
  mps: 64,
  interval: 10,
  waitF2: true,
};

// Georg's descriptor with the IDs of Nintendo's Switch Pro controller
// (057E:2009), full speed: BitsOS leaves that one to raw HID by its IDs,

const interfacesOf = (pad) => (pad.interfaces
  ?? [{ number: 0, report: pad.report, ep: 0x81, out: pad.out, idle: pad.idle, mps: pad.mps, interval: pad.interval, bInterval: pad.bInterval }])
  .map((f) => ({ cls: 3, sub: 0, proto: 0, ...f, mps: f.mps ?? f.idle?.length, bInterval: f.bInterval ?? f.interval }));

function config(ifaces) {
  const endpoint = (f, at) => Buffer.from([7, 5, at, 3, f.mps & 0xFF, f.mps >> 8, f.bInterval]);
  const body = Buffer.concat(ifaces.flatMap((f) => [
    Buffer.from([9, 4, f.number, 0, (f.ep ? 1 : 0) + (f.out ? 1 : 0), f.cls, f.sub, f.proto, 0]),
    ...(f.report ? [Buffer.from([9, 0x21, 0x10, 0x01, 0x21, 0x01, 0x22, f.report.length & 0xFF, f.report.length >> 8])] : []),
    ...(f.ep ? [endpoint(f, f.ep)] : []),
    ...(f.out ? [endpoint(f, f.out)] : []),
  ]));
  const n = 9 + body.length;
  return Buffer.concat([Buffer.from([9, 2, n & 0xFF, n >> 8, ifaces.length, 1, 0, 0x80, 0xFA]), body]);
}

// QEMU arguments for a pad slot: plugPad(port) plugs a pad in there; bus
// null leaves the choice of the USB bus to QEMU (OVMF's unnamed xHCI), at
// names QEMU's USB port (1.1: port 1 of a hub on root port 1)
export const padArgs = (port, id = `pad${port}`, bus = 'xhci.0', at = null) => ['-chardev',
  `socket,id=${id},host=127.0.0.1,port=${port},server=on,wait=off,nodelay=on`, '-device',
  `usb-redir,chardev=${id}${bus ? `,bus=${bus}` : ''}${at ? `,port=${at}` : ''}`];

// port: QEMU's chardev -> { report(bytes, ep), unplug(), log, config }:
// each interrupt IN endpoint sends its current report every `interval` ms
// once the guest reads it, as the real pad does; report() without ep
// changes the first pad's (the first interface with a report descriptor
// and an endpoint); what the guest writes to an OUT endpoint is logged
// and acknowledged; config is the configuration descriptor it serves.
// retry: ms to keep trying while QEMU is not listening yet (a pad plugged
// in before the guest boots).
export function plugPad(port, pad = DRAGONRISE, { retry = 0 } = {}) {
  const log = [];
  const ifaces = interfacesOf(pad), cfg = config(ifaces);
  const current = Object.fromEntries(ifaces.filter((f) => f.ep).map((f) => [f.ep, Buffer.from(f.idle)]));
  const firstEp = (ifaces.find((f) => f.ep && f.report) ?? ifaces.find((f) => f.ep))?.ep;
  const timers = {};
  let id = 1, rx = Buffer.alloc(0), helloSeen = false, gone = false, awake = false;
  let sock;
  let wide = false;                            // 16-byte headers once both hellos are through
  const stopAll = () => { for (const ep of Object.keys(timers)) { clearInterval(timers[ep]); delete timers[ep]; } };
  const send = (type, body, pid = id++) => {
    const h = Buffer.alloc(wide ? 16 : 12);
    h.writeUInt32LE(type, 0); h.writeUInt32LE(body.length, 4);
    if (wide) h.writeBigUInt64LE(BigInt(pid), 8); else h.writeUInt32LE(Number(pid) >>> 0, 8);
    sock.write(Buffer.concat([h, body]));
  };
  function plugIn() {
    const caps = Buffer.alloc(4); caps.writeUInt32LE(CAPS);
    const ver = Buffer.alloc(64); ver.write('BitsOS fake pad', 'latin1');
    send(T.hello, Buffer.concat([ver, caps]), 0);
    wide = true;                               // QEMU's hello had all four
    const ifi = Buffer.alloc(4 + 4 * 32);
    ifi.writeUInt32LE(ifaces.length, 0);
    ifaces.forEach((f, i) => { ifi[4 + i] = f.number; ifi[36 + i] = f.cls; ifi[68 + i] = f.sub; ifi[100 + i] = f.proto; });
    send(T.ifInfo, ifi);
    const ep = Buffer.alloc(32 + 32 + 32 + 64);
    ep.fill(255, 0, 32);                       // type invalid, but:
    ep[0] = 0; ep[16] = 0;                     // control out and in
    ep.writeUInt16LE(pad.device[7], 96); ep.writeUInt16LE(pad.device[7], 96 + 32);
    for (const f of ifaces) {
      for (const i of [f.ep && 16 + (f.ep & 15), f.out && (f.out & 15)].filter(Boolean)) {  // IN endpoints from 16
        ep[i] = 3; ep[32 + i] = f.bInterval; ep[64 + i] = f.number; ep.writeUInt16LE(f.mps, 96 + 2 * i);
      }
    }
    send(T.epInfo, ep);
    const c = Buffer.alloc(10);
    c[0] = pad.speed; c[1] = pad.device[4]; c[2] = pad.device[5]; c[3] = pad.device[6];
    pad.device.copy(c, 4, 8, 14);              // vendor, product, bcdDevice
    send(T.connect, c);
  }
  function descriptor(value, index) {
    const type = value >> 8, n = value & 0xFF;
    if (type === 1) return pad.device;
    if (type === 2) return cfg;
    if (type === 3) {
      log.push(`string ${n}`);
      if (n === 0) return hex('04 03 09 04');
      return pad.strings[n - 1] ? str(pad.strings[n - 1]) : Buffer.alloc(255, 0xA5);  // the DragonRise trap
    }
    if (type === 0x22) return ifaces.find((f) => f.number === index)?.report ?? null;
    return null;
  }
  function control(pid, b) {
    const req = b[1], rt = b[2], value = b.readUInt16LE(4), index = b.readUInt16LE(6), len = b.readUInt16LE(8);
    const hid = ifaces.find((f) => f.number === index && f.report);  // a HID interface with its report descriptor
    let data = null, status = OK;
    if (rt === 0x80 && req === 6 || rt === 0x81 && req === 6) data = descriptor(value, index);
    else if (rt === 0x80 && req === 0 || rt === 0x81 && req === 0) data = Buffer.alloc(2);
    else if (rt === 0x21 && req === 0x0A) data = hid && !pad.stallIdle ? Buffer.alloc(0) : null;  // SET_IDLE
    else if (rt === 0x21 && (req === 0x09 || req === 0x0B)) data = hid ? Buffer.alloc(0) : null;  // SET_REPORT, SET_PROTOCOL
    else if (rt === 0xA1 && req === 0x01 && hid) {                  // GET_REPORT
      if (pad.waitF2 && value === 0x03F2) { awake = true; data = Buffer.concat([Buffer.from([0xF2]), Buffer.alloc(16)]); }
      else data = current[hid.ep] ?? null;
    }
    log.push(`control ${rt.toString(16)} ${req} ${value.toString(16)} ${index} ${len}${data ? '' : ' stall'}`);
    if (!data) status = STALL, data = Buffer.alloc(0);
    if (!(rt & 0x80)) data = Buffer.alloc(0);
    else data = data.subarray(0, len);
    const h = Buffer.from(b.subarray(0, 10));
    h[3] = status; h.writeUInt16LE(rt & 0x80 ? data.length : len, 8);
    send(T.control, Buffer.concat([h, data]), pid);
  }
  function tick(ep) {
    if (pad.waitF2 && !awake) return;          // a DualShock 3 before its 0xF2
    const h = Buffer.alloc(4);
    h[0] = ep; h[1] = OK; h.writeUInt16LE(current[ep].length, 2);
    send(T.interrupt, Buffer.concat([h, current[ep]]));
  }
  function packet(type, pid, b) {
    switch (type) {
      case T.hello: helloSeen = true; log.push(`hello ${b.subarray(0, 64).toString('latin1').replace(/\0+$/, '')}`); plugIn(); break;
      case T.reset: log.push('reset'); break;
      case T.setConfig: log.push(`set config ${b[0]}`); send(T.configStatus, Buffer.from([OK, b[0]]), pid); break;
      case T.getConfig: send(T.configStatus, Buffer.from([OK, 1]), pid); break;
      case T.setAlt: send(T.altStatus, Buffer.from([OK, b[0], b[1]]), pid); break;
      case T.getAlt: send(T.altStatus, Buffer.from([OK, b[0], 0]), pid); break;
      case T.startIntr: {
        const ep = b[0], f = ifaces.find((g) => g.ep === ep);
        log.push(`start interrupt ${ep.toString(16)}`);
        send(T.intrStatus, Buffer.from([OK, ep]), pid);
        clearInterval(timers[ep]);
        if (f) { timers[ep] = setInterval(() => tick(ep), f.interval); timers[ep].unref(); }
        break;
      }
      case T.stopIntr:
        log.push(`stop interrupt ${b[0].toString(16)}`);
        clearInterval(timers[b[0]]); delete timers[b[0]];
        send(T.intrStatus, Buffer.from([OK, b[0]]), pid); break;
      case T.control: control(pid, b); break;
      case T.interrupt: {                      // data for an OUT endpoint: taken, its length acknowledged
        const ep = b[0], data = b.subarray(4, 4 + b.readUInt16LE(2));
        const ours = ifaces.some((f) => f.out === ep);
        log.push(`interrupt out ${ep.toString(16)} ${data.toString('hex')}${ours ? '' : ' stall'}`);
        const h = Buffer.alloc(4);
        h[0] = ep; h[1] = ours ? OK : STALL; h.writeUInt16LE(ours ? data.length : 0, 2);
        send(T.interrupt, h, pid);
        break;
      }
      case T.cancel: case T.disconnectAck: break;
      default: log.push(`type ${type} ignored`);
    }
  }
  const until = Date.now() + retry;
  function open() {
    sock = net.connect(port, '127.0.0.1');
    sock.on('data', (d) => {
      rx = Buffer.concat([rx, d]);
      for (;;) {
        const hl = helloSeen ? 16 : 12;
        if (rx.length < hl) break;
        const type = rx.readUInt32LE(0), len = rx.readUInt32LE(4);
        const pid = hl === 16 ? rx.readBigUInt64LE(8) : BigInt(rx.readUInt32LE(8));
        if (rx.length < hl + len) break;
        packet(type, pid, rx.subarray(hl, hl + len));
        rx = rx.subarray(hl + len);
      }
    });
    sock.on('close', () => { if (helloSeen) stopAll(); });  // QEMU gone: no more ticks
    sock.on('error', (e) => {
      if (e.code === 'ECONNREFUSED' && !helloSeen && !gone && Date.now() < until) setTimeout(open, 50).unref();
      else log.push(`error ${e.message}`);
    });
  }
  open();
  return {
    log,
    config: cfg,
    report(bytes, ep = firstEp) { current[ep] = Buffer.from(bytes); },
    unplug() {
      if (gone) return;
      gone = true;
      stopAll();
      if (helloSeen && !sock.destroyed) send(T.disconnect, Buffer.alloc(0));
      sock.end();
    },
    get ready() { return helloSeen; },
  };
}

const PADS = { dragonrise: DRAGONRISE, highpad: HIGHPAD, ds3: DUALSHOCK3 };
if (process.argv[1] && import.meta.url.endsWith(process.argv[1].split('/').pop())) {
  const plugged = {};
  const rl = readline.createInterface({ input: process.stdin });
  rl.on('line', (line) => {
    let answer;
    try {
      const m = JSON.parse(line), p = plugged[m.port];
      if (m.cmd === 'plug') {
        p?.unplug();
        plugged[m.port] = plugPad(m.port, PADS[m.pad], { retry: 30000 });
        answer = { ok: true };
      } else if (m.cmd === 'report') { p.report(Buffer.from(m.hex, 'hex')); answer = { ok: true }; }
      else if (m.cmd === 'unplug') { p?.unplug(); delete plugged[m.port]; answer = { ok: true }; }
      else if (m.cmd === 'state') answer = { ready: !!p?.ready, log: p ? p.log : [] };
      else answer = { error: `unknown ${m.cmd}` };
    } catch (e) { answer = { error: String(e) }; }
    process.stdout.write(JSON.stringify(answer) + '\n');
  });
  rl.on('close', () => { for (const p of Object.values(plugged)) p.unplug(); setTimeout(() => process.exit(0), 200); });
}
