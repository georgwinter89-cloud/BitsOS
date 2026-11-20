// remote_desk_check.mjs (GP-R2): the remote desk's tools against a stand-in of the Gaming-PC's kernel MCP server on
// this host, no QEMU. Checks:
//   1. every client tool given --gaming-pc finds the default token file as selftest/collect do (host/mcp/token.mjs):
//      below USERPROFILE (where the Windows session of 15.11. put it), below HOME, BITSOS_TOKEN_FILE; paths with
//      spaces; none -> exit 2 with the place named; without --gaming-pc no default file is read and nothing is sent;
//   2. step 28's split parts hold for the round's real lines (fields in the middle) and refuse a line without them;
//   3. stage 8 and step 30: the card runs on its interrupt (no CONFIG line keeps it polled): a polled card, a failed
//      stage or no vector is red; a reload_info error does not stop collect;
//   4. the heartbeat: a ktest call that answers only after 3.5 s leaves no pause over the stand-in's 1.5 s gap, and
//      collect waits until the stand-in's proof (3 s of answers, the kernel's rule in small) says confirmed.
// Usage: node remote_desk_check.mjs <run dir> <port>; prints "ok: ..." lines, exit 1 on the first red.
import { spawn } from 'node:child_process';
import { createServer } from 'node:http';
import { mkdirSync, rmSync, writeFileSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { holds, judge, list, readFact, shape } from '../../remote-desk/collect.mjs';
import * as ph from '../../remote-desk/phase.mjs';
import { defaultTokenFile, tokenFileCandidates, tokenFileFor } from '../../mcp/token.mjs';

const HERE = dirname(fileURLToPath(import.meta.url));
const ROOT = resolve(HERE, '..', '..', '..');
const [runDir, portText] = process.argv.slice(2);
const port = Number(portText);
if (!runDir || !port) { console.error('usage: node remote_desk_check.mjs <run dir> <port>'); process.exit(2); }
const red = (m) => { console.log(`RED: ${m}`); process.exit(1); };
const ok = (m) => console.log(`ok: ${m}`);
const need = (c, m) => (c ? ok(m) : red(m));
const TOKEN = 'gp-r2-remote-desk-token';

// The round's lines (round 1, step 28), typed from the result; no MAC of the card.
const RTL = [
  '#100 [ 20.1] I net rtl8126.probe function=83:00.0 device=0x8126 subsystem=0x10ec8126',
  '#101 [ 20.2] I net rtl8126.stage ifname=rtl0 stage=probe verdict=ok Karte erkannt',
  '#102 [ 20.3] I net rtl8126.fw_file ifname=rtl0 path=/usr/lib/firmware/rtl_nic/rtl8126a-3.fw bytes=13232 version="rtl8126a-3_0.0.5 08/30/24" actions=3279 as_measured=true',
  '#103 [ 20.3] I net rtl8126.stage ifname=rtl0 stage=firmware verdict=ok Firmware geladen',
  '#104 [ 20.4] I net rtl8126.fw_played ifname=rtl0 steps=3279 writes=3000 delay_ms=0 max_wait_us=0 slow_waits=0',
  '#105 [ 20.4] I net rtl8126.stage ifname=rtl0 stage=phy verdict=ok Leitungsteil erkannt',
  '#106 [ 21.0] I net rtl8126.up ifname=rtl0 chip=RTL8126A xid=0x64a mac=00:00:00:00:00:00 firmware=true modes=0x3f irq=polled phy_max_wait_us=0',
  ...['link', 'rings', 'lease', 'gateway'].map((s, i) => `#${107 + i} [ 22.0] I net rtl8126.stage ifname=rtl0 stage=${s} verdict=ok`),
  '#111 [ 40.0] I net rtl8126.stage ifname=rtl0 stage=irq verdict=ok Die Karte meldet sich von selbst (Unterbrechung)',
].join('\n');
// The same card when it stayed polled (no vector to be had): stage 8 only a note.
const POLLED = RTL.replace('I net rtl8126.stage ifname=rtl0 stage=irq verdict=ok Die Karte meldet sich von selbst (Unterbrechung)',
  'W net rtl8126.stage ifname=rtl0 stage=irq verdict=note Die Karte meldet sich nicht von selbst: sie wird weiter regelmäßig abgefragt');
const CONFIG = (extra = '') => `#7 [ 0.3] I kernel config values="gp_net=on gp_stick=on nvme_bind=off nvme_write=off profile=gaming-pc screen=firmware stick_write=off token=*** usb_msd_write=off${extra}"`;

// 1. Token places.
const tmp = join(runDir, 'token places');
rmSync(tmp, { recursive: true, force: true });
const prof = join(tmp, 'C Users', 'Georg Win'), home = join(tmp, 'linux home');
for (const h of [prof, home]) mkdirSync(join(h, '.config', 'bitsos-gaming-pc'), { recursive: true });
const inProf = join(prof, '.config', 'bitsos-gaming-pc', 'token.txt'), inHome = join(home, '.config', 'bitsos-gaming-pc', 'token.txt');
need(tokenFileCandidates({ USERPROFILE: prof, HOME: home })[0] === inProf && tokenFileCandidates({ USERPROFILE: prof, HOME: home })[1] === inHome,
  'the token is looked for below USERPROFILE first, then below HOME');
need(tokenFileFor(undefined, true, { USERPROFILE: prof, HOME: home }) === undefined, 'no file anywhere: no token file (readToken says where to put it)');
writeFileSync(inHome, `${TOKEN}\r\n`);
need(tokenFileFor(undefined, true, { USERPROFILE: prof, HOME: home }) === inHome, 'a file below HOME only is found');
writeFileSync(inProf, `${TOKEN}\r\n`);
need(defaultTokenFile({ USERPROFILE: prof, HOME: home }) === inProf, 'with both, the one below USERPROFILE wins');
need(tokenFileFor(undefined, false, { USERPROFILE: prof, HOME: home }) === undefined, 'without --gaming-pc the Gaming-PC\'s file is not read');
need(tokenFileFor(undefined, true, { USERPROFILE: prof, HOME: home, BITSOS_TOKEN: 'x' }) === undefined, 'BITSOS_TOKEN carries the token: no file read');
need(tokenFileFor('given.txt', false, { USERPROFILE: prof }) === 'given.txt', '--token-file wins');
need(defaultTokenFile({ BITSOS_TOKEN_FILE: '/elsewhere/t.txt', USERPROFILE: prof }) === '/elsewhere/t.txt', 'BITSOS_TOKEN_FILE moves the place');

// 2. Split parts against the real lines.
const s28 = list().steps.find((s) => s.id === '28-rtl-log');
need(holds(['rtl8126.fw_file', 'bytes=13232', 'as_measured=true'], RTL) && !RTL.includes('rtl8126.fw_file path=/usr/lib/firmware/rtl_nic/rtl8126a-3.fw bytes=13232 as_measured=true'),
  'the round\'s fw_file line has the parts, not the old whole text');
need(!holds(['rtl8126.up', 'firmware=true', 'xid=0x64a'], RTL), 'parts count in their order');
need(!holds(['rtl8126.fw_file', 'chip=RTL8126A'], RTL), 'parts must stand on one line');

// 3. Stage 8 and step 30: the card runs on its interrupt.
need(judge(shape(s28, {}), false, RTL).length === 0, 'step 28 is green on the round\'s lines with stage 8 ok');
const polled = judge(shape(s28, {}), false, POLLED);
need(polled.length === 1 && polled[0].includes('stage=irq ... verdict=ok'), `a card that stayed polled is red at stage 8: ${polled.join('; ')}`);
const failed = RTL.replace('stage=rings verdict=ok', 'stage=rings verdict=failed');
need(judge(shape(s28, {}), false, failed).some((b) => b.includes('verdict=failed')), 'a failed stage is red');
need(judge(shape(s28, {}), false, RTL.split('\n').filter((l) => !l.includes('rtl8126.probe')).join('\n')).some((b) => b.includes('missing: rtl8126.probe')),
  'a missing probe line is red');
const s30 = list().steps.find((s) => s.id === '30-msi');
need(shape(s30, {}).judge !== false, 'step 30 is always judged');
need(judge(shape(s30, {}), false, 'msi: 0 sets').length === 1, 'no vector is red');
need(judge(shape(s30, {}), false, 'msi: 1 sets\n83:00.0 rtl8126 msix #0: cpu 3 vector 0x41 count 150 suppressed 0 storms 0').length === 0, 'the card\'s vector is green');

// 3b. GP-DISK: with the Lexar named (gp_nvme= kept by the kernel) steps 04, 05, 19, 19b, 20 and 21 expect its driver;
// without the line (round 1's stick) they expect what they did.
const nvFact = list().facts.find((f) => f.name === 'gp_nvme_named');
const LEXAR_CONFIG = CONFIG().replace('nvme_bind=off nvme_write=off', 'gp_nvme=81:00.0/1d97:5236 gp_nvme_loader=on nvme_bind=off nvme_boot=S:G:3099625472:807403520 nvme_write=on');
need(readFact(nvFact, false, LEXAR_CONFIG) === true && readFact(nvFact, false, CONFIG()) === false && readFact(nvFact, false, '(nothing)') === null,
  'gp_nvme_named is read from the kernel config line: named, not named, unknown');
need(readFact(nvFact, false, CONFIG(' gp_nvme=')) === false && readFact(nvFact, false, CONFIG(' gp_nvme_loader=on')) === false,
  'an emptied gp_nvme= or the loader word alone names nothing');
const step = (id) => list().steps.find((s) => s.id === id);
const both = (id, without, withIt, what) => {
  const a = judge(shape(step(id), { gp_nvme_named: false }), false, without), b = judge(shape(step(id), { gp_nvme_named: true }), false, withIt);
  need(a.length === 0 && b.length === 0, `${id}: ${what} (without: ${a.join('; ') || 'green'}; with: ${b.join('; ') || 'green'})`);
  need(judge(shape(step(id), { gp_nvme_named: true }), false, without).length > 0, `${id}: the old answer is red once the Lexar is named`);
  need(judge(shape(step(id), { gp_nvme_named: false }), false, withIt).length > 0, `${id}: the Lexar's answer is red without the line`);
};
both('05-config', CONFIG(), LEXAR_CONFIG, 'the forced lines, and with gp_nvme= the stick\'s nvme lines and the loader word');
const LOADER = '#3 [ 0.0] I boot loader.line bitsboot profile.chosen name=gaming-pc reason=dmi\n#4 [ 0.0] I boot loader.line bitsboot profile.tiers net=on stick=on';
both('04-loader', LOADER, `${LOADER} nvme=on\n#5 [ 0.0] W boot loader.line bitsboot safe mode: only the NVMe controller gp_nvme=81:00.0/1d97:5236 may get a driver`, 'the third tier and the safe-mode line');
const UNTOUCHED = ['01:00.0', '81:00.0', '86:00.0', '82:00.0'].map((f) => `I pci untouched function=${f} key=nvme_bind`).join('\n') + '\nI pci untouched function=85:00.0 key=gp_wlan';
both('19-pci-untouched', UNTOUCHED, UNTOUCHED.replace('I pci untouched function=81:00.0 key=nvme_bind\n', ''), 'the Lexar leaves the untouched list');
both('19b-pci-gp-nvme', '(no lines)', 'I pci gp_nvme.released function=81:00.0 vendor=0x1d97 device=0x5236', 'one released line, only with the line');
need(judge(shape(step('19b-pci-gp-nvme'), { gp_nvme_named: true }), false, 'W pci gp_nvme.refused function=81:00.0 wanted=81:00.0/1d97:5236 vendor=0x1d97 device=0x5237 class=0x10802').length > 0, '19b: a refused line is red');
both('20-nvme', 'no NVMe controller', 'nvme0 at 81:00.0 1d97:5236 state=ready model="Lexar SSD NQ790 2TB" serial="X" firmware="Y"', 'no controller, or exactly the Lexar');
need(judge(shape(step('20-nvme'), { gp_nvme_named: true }), false, 'nvme0 at 81:00.0 1d97:5236 model="Lexar SSD NQ790 2TB"\nnvme1 at 86:00.0 144d:a80a').length > 0, '20: a second controller is red');
const BLOCK = (one, w) => `I block configured stick_block0=true stick_usb=090c:1000 stick_id=true stick_write=false usb_msd_write=false nvme_write=${w}\nW block ${one ? 'internal_disks.one' : 'internal_disks.off'}`;
both('21-block', BLOCK(false, false), BLOCK(true, true), 'internal_disks.off/.one and the NVMe write switch');
need(['26b-latency', '37-data-window', '38-data-moved'].every((id) => step(id)?.kind === 'read' && ['latency', 'read_file'].includes(step(id).tool)), 'the new steps only read');
need(judge(step('26b-latency'), false, 'input-to-screen latency per frame (...)\nconsole: frames=12 min_us=1600').length === 0, '26b keeps the console path');

// 4. A stand-in kernel MCP server: the token, the answers of a few steps, a slow ktest, and the proof in small.
const NEED_MS = 3000, GAP_MS = 1500;
const proof = { first: null, last: null, gaps: 0, done: false, answers: [] };
let configGone = false, rtlLog = RTL, reloadFails = false, calls = 0, played = false;
const asked = [];
let msiSet = true, msiCount = 100, msiStep = 50, keysAll = true, typed = true, guardHash = '0x00000000deadbeef';
// Georg's keys as the kernel keeps them (seq 40..): none until he typed; 33 only injected unless keysAll.
const keyEvents = () => (typed ? [30, 31, 32].map((c, i) => `ev ${40 + i} t=1.000000 dev=3 KEY code=${c} value=1`).concat('ev 43 t=1.100000 dev=0xffffff00 KEY code=33 value=1 injected', keysAll ? ['ev 44 t=1.200000 dev=3 KEY code=33 value=1'] : []) : []);
const eventsFrom = (since) => keyEvents().filter((l) => Number(l.split(' ')[1]) >= since);
// The real stages' answers on a cold card (livestage.rs, stage.rs, stage_ro.rs).
const GX = {
  state: 'Grafikkarte 1: Stand zeigen: fertig.\ngx_bringup card0 state: PASS\nwrites: 0 register writes (0 failed), 0 refused by the stage\'s list\nsession: nothing asked yet',
  alive: 'Grafikkarte 1: Sicherheitsprozessor prüfen: fertig.\ngx_bringup card0 alive: PASS\nwrites: 0 register writes (0 failed), 0 refused by the stage\'s list\nC2PMSG_81 0x00000000, C2PMSG_64 0x00000000, C2PMSG_35 0x80000000, C2PMSG_33 0x80000000\nsecure OS: not running (a cold card); the stage sos loads it',
  bootloader: 'Grafikkarte 1: Startlader prüfen: fertig.\ngx_bringup card0 bootloader: PASS\nwrites: 0 register writes (0 failed), 0 refused by the stage\'s list\nC2PMSG_35 after the wait: Ok(80000000)',
  smu: 'Grafikkarte 1: Energieverwaltung befragen: fertig.\ngx_bringup card0 smu: PASS\nwrites: 37 register writes (0 failed), 0 refused by the stage\'s list\nsmu version 104.80.0 (program 0), driver interface 0x33 (Linux 7.3.1 is built for 0x33)',
};
const MOUNTS = '/dev/nvme0n1p2 /boot vfat rw,relatime 0 0\n/dev/mapper/data /data ext4 rw,relatime 0 0';
const answered = () => {
  const now = Date.now();
  proof.answers.push(now);
  if (proof.last !== null && now - proof.last > GAP_MS) { proof.first = null; proof.gaps++; }
  proof.first ??= now;
  proof.last = now;
  if (now - proof.first >= NEED_MS) proof.done = true;
};
const text = (name, args) => {
  // As the kernel: proven, then one answer while OK goes onto the stick, then confirmed.
  if (name === 'reload_info' && proof.done && !proof.wrote) { proof.wrote = true; return 'boot confirmation (RB-6): generation 9: writing OK to the stick'; }
  if (name === 'reload_info') return proof.done ? 'boot confirmation (RB-6): generation 9 confirmed (OK) in this boot' : 'boot confirmation (RB-6): generation 9 waits for its confirmation: MCP answers seen over 0 s of 3 s';
  if (name === 'log' && args.name === 'config') return configGone ? '(no lines)' : CONFIG();
  if (name === 'log' && args.name === 'rtl8126.') return rtlLog;
  if (name === 'hw_info' && args.section === 'msi') return msiSet ? `msi: 1 sets\n83:00.0 rtl8126 msix #0: cpu 3 vector 0x41 count ${msiCount += msiStep} suppressed 0 storms 0` : 'msi: 0 sets';
  if (name === 'snd') return 'outputs n=5\ntarget=auto (write: auto|jack|hdmi|usb)';
  if (name === 'snd_ton') return 'Tontest USB-Headset: gefunden; das Gerät nennt seine Rate nicht (bei Headsets üblich) | Rate laut Gerät 48000 Hz; Strom gestartet; 192000 Bytes in ~1000 ms gesendet; 0 Pakete verpasst, 0 Fehler; gestoppt, fertig.';
  if (name === 'log' && args.name === 'uac') return '#300 [ 9.0] I usb uac.bound vendor=0x26ce product=0xa09 control=0 playback=3 capture=0';
  if (name === 'log' && args.subsystem === 'usb_audio' && args.name === 'uac.tone_check') return played ? '#410 [ 60.0] I usb_audio uac.tone_check output=headset line="Tontest USB-Headset: gefunden"' : '(no lines)';
  if (name === 'gx_bringup') return GX[args.stage] ?? `(stand-in: no stage ${args.stage})`;
  if (name === 'run' && (args.args ?? []).join(' ').includes('/proc/mounts')) return `exit 0\n${MOUNTS}`;
  if (name === 'input' && args.since === undefined) return 'input devices n=2\ndevice 3 name="USB HID 10f5:501b" phys=xhci1-5/input0 id=10f5:501b keyboard=true pointer=false keys=105 rel=[] abs=[] leds=[0,1,2] repeat=true events=0 held=[]\ndevice 4 name="MCP keyboard" phys=mcp id=ffff:ff00 keyboard=true pointer=false keys=105 rel=[] abs=[] leds=[] repeat=false events=0 held=[]\nlocks caps=false num=false scroll=false held=[] physical_held=[] dropped=0 next_seq=' + (40 + keyEvents().length);
  if (name === 'input') return [text('input', {}), `events n=${eventsFrom(args.since).length} shown=${eventsFrom(args.since).length}`, ...eventsFrom(args.since)].join('\n');
  if (name === 'nvme') return `nvme0 at 81:00.0 1d97:5236 state=ready commands admin: ${calls}\n  guard_hash ns1 ${guardHash}`;
  if (name === 'ktest') return 'KTEST done passed=6 failed=0 skipped=0\npassed=6 failed=0 skipped=0 refused=0';
  return `(stand-in: no answer for ${name})`;
};
const server = createServer((req, res) => {
  const parts = [];
  req.on('data', (d) => parts.push(d));
  req.on('end', () => {
    calls++;
    if (req.headers.authorization !== `Bearer ${TOKEN}`) { res.writeHead(401).end(); return; }
    const m = JSON.parse(Buffer.concat(parts).toString('utf8'));
    if (m.method === 'notifications/initialized') { res.writeHead(202).end(); return; }
    const reply = (result) => { res.writeHead(200, { 'Content-Type': 'application/json' }).end(JSON.stringify({ jsonrpc: '2.0', id: m.id, result })); answered(); };
    if (m.method === 'initialize') { reply({ protocolVersion: '2025-06-18', serverInfo: { name: 'stand-in', version: '1' } }); return; }
    const { name, arguments: args } = m.params;
    asked.push(name);
    if (name === 'reload_info' && reloadFails) { res.writeHead(200, { 'Content-Type': 'application/json' }).end(JSON.stringify({ jsonrpc: '2.0', id: m.id, error: { code: -32000, message: 'stand-in: busy' } })); return; }
    // MCP's read_file runs on a kernel thread: /proc/mounts is self/mounts, and a kernel thread has no self.
    if (name === 'read_file' && /^\/proc\/(mounts|self\/)/.test(args?.path ?? '')) { reply({ content: [{ type: 'text', text: 'read_file: /proc/mounts: no such file or directory' }], isError: true }); return; }
    if (name === 'snd_ton') played = true;
    const body = { content: [{ type: 'text', text: text(name, args ?? {}) }] };
    if (name === 'ktest') setTimeout(() => reply(body), 3500);
    else if (name === 'input' && args?.since !== undefined && args.wait_ms > 0 && !eventsFrom(args.since).length) setTimeout(() => reply(body), Math.min(args.wait_ms, 1000));
    else reply(body);
  });
});
await new Promise((r) => server.listen(port, '127.0.0.1', r));
const host = `127.0.0.1:${port}`;
const node = (args, env) => new Promise((done) => {
  const c = spawn(process.execPath, args, { cwd: ROOT, env: { PATH: process.env.PATH, ...env } });
  let out = '';
  c.stdout.on('data', (d) => { out += d; });
  c.stderr.on('data', (d) => { out += d; });
  c.on('close', (code) => done({ code, out }));
});
const mcpCall = join(ROOT, 'host', 'mcp', 'mcp-call.mjs');
let r = await node([mcpCall, host, 'reload_info', '--gaming-pc'], { USERPROFILE: prof, HOME: join(tmp, 'nowhere') });
need(r.code === 0 && r.out.includes('generation 9'), `mcp-call.mjs --gaming-pc finds the token below USERPROFILE without --token-file (exit ${r.code}: ${r.out.trim()})`);
r = await node([mcpCall, host, 'reload_info', '--gaming-pc'], { HOME: home });
need(r.code === 0, `mcp-call.mjs --gaming-pc finds it below HOME (exit ${r.code})`);
calls = 0;
r = await node([mcpCall, host, 'reload_info'], { USERPROFILE: prof, HOME: home });
need(r.code === 2 && r.out.includes('no token') && r.out.includes('--gaming-pc') && calls === 0,
  `without --gaming-pc (a call to the Mini-PC that forgot its token): exit 2 "no token", nothing sent (exit ${r.code}, ${calls} requests)`);
for (const tool of ['push.mjs', 'install.mjs', 'reload.mjs', 'echt.mjs', 'access.mjs', 'pair.mjs']) {
  const src = (await import('node:fs')).readFileSync(join(ROOT, 'host', 'mcp', tool), 'utf8');
  need(src.includes("tokenFileFor(opts['token-file'], opts['gaming-pc'])") && src.includes('--gaming-pc'), `${tool} reads the default file only with --gaming-pc`);
}
r = await node([mcpCall, host, 'reload_info', '--gaming-pc'], { HOME: join(tmp, 'nowhere'), USERPROFILE: join(tmp, 'nowhere either') });
need(r.code === 2 && r.out.includes(join(tmp, 'nowhere either', '.config', 'bitsos-gaming-pc', 'token.txt')) && !r.out.includes(TOKEN), `no token: exit 2 naming the place (${r.out.trim()})`);
writeFileSync(join(tmp, 'wrong.txt'), 'not-the-token-at-all\n');
r = await node([mcpCall, host, 'reload_info', '--gaming-pc'], { BITSOS_TOKEN_FILE: join(tmp, 'wrong.txt'), USERPROFILE: prof });
need(r.code === 2 && r.out.includes('401'), 'BITSOS_TOKEN_FILE is the one file read');

const collect = join(ROOT, 'host', 'remote-desk', 'collect.mjs');
proof.first = proof.last = null; proof.done = false; proof.wrote = false; proof.gaps = 0; proof.answers = [];
const out1 = join(runDir, 'collect out 1');
r = await node([collect, host, '--only', '05,28,30,50', '--ktest', '--heartbeat-s', '1', '--confirm-wait-s', '20', '--out', out1], { USERPROFILE: prof });
writeFileSync(join(runDir, 'collect-1.txt'), r.out);
need(r.code === 0, `collect is green on the card's interrupt (exit ${r.code}):\n${r.out}`);
need(/^ok {3}05-config/m.test(r.out) && /^ok {3}28-rtl-log/m.test(r.out) && /^ok {3}30-msi/m.test(r.out), 'steps 05, 28 and 30 are green');
need(/^ok {3}50-ktest-rtl/m.test(r.out) && /^ok {3}confirm: generation 9 confirmed/m.test(r.out) && proof.wrote, 'the slow ktest ran; collect waited through the stick write until the generation confirmed itself');
const pauses = proof.answers.slice(1).map((t, i) => t - proof.answers[i]);
need(proof.gaps === 0 && Math.max(...pauses) <= GAP_MS, `no pause over ${GAP_MS} ms between answers while a call took 3.5 s (longest ${Math.max(...pauses)} ms)`);

rtlLog = POLLED; msiSet = false;
proof.first = proof.last = null; proof.done = false;
r = await node([collect, host, '--only', '28,30', '--heartbeat-s', '1', '--confirm-wait-s', '0', '--out', join(runDir, 'collect out 2')], { USERPROFILE: prof });
writeFileSync(join(runDir, 'collect-2.txt'), r.out);
need(r.code === 1 && /^RED  28-rtl-log .*stage=irq \.\.\. verdict=ok/m.test(r.out) && /^RED  30-msi/m.test(r.out),
  'a card that stayed polled: stage 8 and step 30 are red');

configGone = true; rtlLog = failed; reloadFails = true;
r = await node([collect, host, '--only', '28,30', '--heartbeat-s', '1', '--confirm-wait-s', '0', '--out', join(runDir, 'collect out 3')], { USERPROFILE: prof });
writeFileSync(join(runDir, 'collect-3.txt'), r.out);
need(/^note confirm: reload_info did not answer/m.test(r.out), `a reload_info error is a note, not a crash:\n${r.out}`);
need(r.code === 1 && /^RED  28-rtl-log .*must not appear: rtl8126\.stage \.\.\. verdict=failed/m.test(r.out) && !/^RED {2}confirm/m.test(r.out),
  'a failed stage keeps step 28 red, no confirm red from an error answer');

// 5. GP-R3: round 2's phases. First the list itself, then collect.mjs --phase against the stand-in.
const checks = list();
const ids = ph.phaseIds(checks);
need(ids.join(',') === 'A,B1,B2,B3,C,D,E', `the phases in their order: ${ids.join(',')}`);
const allIds = new Set(checks.steps.map((x) => x.id));
for (const id of ids) {
  const p = ph.phase(checks, id);
  for (const st of p.steps) {
    need(!allIds.has(st.id), `${id}: step ${st.id} has its own id`);
    allIds.add(st.id);
    need(ph.KINDS.includes(st.kind), `${st.id}: kind ${st.kind} is known`);
    if (!['install', 'confirm', 'keys'].includes(st.kind)) need(typeof st.tool === 'string' && typeof st.args === 'object', `${st.id}: a tool and its arguments`);
    if (st.kind === 'sound' || st.kind === 'keys') need(typeof st.ask === 'string' && st.ask.length > 20, `${st.id}: asks Georg first`);
    if (st.kind === 'install') need(['A', 'B3'].includes(id), `${st.id}: installs only in A and B3`);
    if (st.tool === 'nvme') need(!('speed_mb' in st.args) && Object.keys(st.args).every((k) => k === 'guard_hash'), `${st.id}: nvme only reads ({} or guard_hash)`);
    need(!['data_move', 'reboot', 'poweroff', 'ktest_picture'].includes(st.tool), `${st.id}: no ${st.tool}`);
    if (st.tool === 'gx_bringup') need(['state', 'alive', 'bootloader', 'smu'].includes(st.args.stage) && st.args.device === 'card0' && !st.args.pmfw, `${st.id}: only the read-only stages, on card0`);
  }
  need((p.never ?? []).length > 0 && (p.before ?? []).length > 0, `${id}: says what comes before and what never`);
}
// A kept answer is compared only within its own phase (one boot, one window state): B1's hash, taken before
// bitsdisk armed the window, covers the BitsOS partitions too, B2's does not.
const keeper = new Map();
for (const id of ids) {
  const seen = new Set();
  for (const st of ph.phase(checks, id).steps) {
    if (st.same) need(seen.has(st.same.name), `${id}: ${st.id} compares with ${st.same.name}, kept earlier in this phase`);
    if (st.keep) {
      need(!keeper.has(st.keep.name) || keeper.get(st.keep.name) === id, `${st.id}: ${st.keep.name} is kept in ${keeper.get(st.keep.name)} too`);
      keeper.set(st.keep.name, id);
      seen.add(st.keep.name);
    }
  }
}
// MCP's read_file runs on a kernel thread, which has no /proc/self (and /proc/mounts is self/mounts).
const everyStep = [...checks.steps, ...ids.flatMap((i) => ph.phase(checks, i).steps)];
need(!everyStep.some((x) => x.tool === 'read_file' && /^\/proc\/(mounts|self(\/|$)|thread-self)/.test(x.args.path)), 'no read_file of /proc/mounts or /proc/self (a kernel thread cannot)');
need(ph.phase(checks, 'B1').steps.find((x) => x.id === 'B1-14-window').want?.includes('window=none why=no_nvme_data'), 'B1-14 wants the window as the kernel says it before bitsdisk');
need(ph.phase(checks, 'C').steps.find((x) => x.id === 'C03-no-sound-yet').refuse?.includes('uac.tone_check'), 'C03 refuses the event snd_ton writes');
need(ph.phase(checks, 'C').steps.filter((x) => x.kind === 'sound').length === 1 && ph.phase(checks, 'C').steps.find((x) => x.kind === 'sound').tool === 'snd_ton',
  'the one sound of the round is phase C\'s snd_ton');
need(!ids.filter((i) => i !== 'C').some((i) => ph.phase(checks, i).steps.some((x) => x.kind === 'sound' || x.tool === 'snd_ton')), 'no other phase plays');
need(ph.needs(ph.phase(checks, 'A')).bundle && !ph.needs(ph.phase(checks, 'B3')).bundle && ph.needs(ph.phase(checks, 'C')).sound, 'A needs the bundle, B3 takes it optionally, C has the tone');
const a06 = ph.installArgv(ph.phase(checks, 'A').steps.find((x) => x.id === 'A06-install'), 'h:1', 'b dir');
need(a06.includes('--upload') && a06.includes('--reboot') && !a06.includes('--bind') && !a06.includes('--dry-run') && a06[a06.indexOf('--bundle') + 1] === 'b dir',
  `A06 runs install.mjs --upload --reboot (no --bind: Windows opens no port): ${a06.slice(1).join(' ')}`);
need(ph.installArgv(ph.phase(checks, 'A').steps.find((x) => x.id === 'A05-install-dry'), 'h', 'b').includes('--dry-run'), 'A05 is the dry run');
// KL: GP-CPU2's steps at phase A's end, judged on lines as the kernel writes them (platform smp.rs, mcp msr.rs,
// sched cstate.rs, sched-mcp's idle line, MCP ktest), and the fact that turns them (cpu_policy=performance).
const aStep = (id, facts) => ph.withPlus(shape(ph.phase(checks, 'A').steps.find((x) => x.id === id), facts));
const plain = { cpu_perf: false };
const HWP = (epp, pol, n = 20) => Array.from({ length: n }, (_, c) => `#${20 + c} [ 0.5] I cpu hwp.on cpu=${c} was_on=false lowest=1 highest=${c < 8 ? 55 : 46} request=${epp}004601 mode=auto epp=${epp} policy=${pol}`).join('\n');
need(judge(aStep('A17-hwp-on', plain), false, HWP('0x40', 'balanced')).length === 0, 'A17: 20 hwp.on lines with epp=0x40 policy=balanced are green');
need(judge(aStep('A17-hwp-on', plain), false, HWP('0x40', 'balanced', 19)).some((b) => b.includes('cpu=19')), 'A17: a core without its hwp.on line is red');
need(judge(aStep('A17-hwp-on', plain), false, HWP('0x80', 'balanced')).length === 20, 'A17: round 1\'s EPP 0x80 is red on every core');
need(judge(aStep('A17-hwp-on', { cpu_perf: true }), false, HWP('0x10', 'performance')).length === 0
  && judge(aStep('A17-hwp-on', plain), false, HWP('0x10', 'performance')).length > 0, 'A17: epp=0x10 policy=performance only with cpu_policy=performance');
need(judge(aStep('A17-hwp-on', plain), false, `${HWP('0x40', 'balanced')}\n#9 [ 0.4] W cpu config.bad key=cpu_policy value=fast`).length === 1, 'A17: an unknown cpu_policy value is red');
const MSR = (v) => `msr 0x774 = ${v} (cpu 6, the CPU this call ran on)`;
need(judge(aStep('A18-msr-774', plain), false, MSR('0x0000000040005701')).length === 0 && judge(aStep('A18-msr-774', plain), false, MSR('0x0000000080005701')).length === 1,
  'A18: bits 31:24 = 0x40 green, round 1\'s 0x80005701 red');
need(judge(aStep('A18-msr-774', { cpu_perf: true }), false, MSR('0x0000000010005701')).length === 0 && judge(aStep('A18-msr-774', { cpu_perf: true }), false, MSR('0x0000000040005701')).length === 1,
  'A18: 0x10 with cpu_policy=performance');
const IDLE = [
  '#30 [ 0.2] I sched idle.state_mwait kind=2 hint=0x21 latency_us=127',
  '#31 [ 0.2] I sched idle.state_mwait kind=3 hint=0x60 latency_us=1048',
  '#32 [ 0.2] I sched idle.mwait source=model states=2 check=pending MWAIT states as Linux\'s intel_idle takes them; used after the self-check',
  '#33 [ 0.2] I sched idle.deep states=2 mode=auto dummy_read=false',
  '#60 [ 3.1] I sched idle.check_state cpu=0 hint=0x21 wake_us=4 residency_pct=97',
  '#61 [ 3.4] I sched idle.check result=ok states=2 samples=4',
].join('\n');
need(judge(aStep('A19-idle-events', plain), false, IDLE).length === 0, 'A19: the MWAIT plan and idle.check result=ok are green');
need(judge(aStep('A19-idle-events', plain), false, IDLE.replace('I sched idle.check result=ok states=2 samples=4', 'W sched idle.check result=failed cpu=8 hint=0x60 reason=slow_wake')).length === 2,
  'A19: a failed self-check is red (no ok line, a failed one)');
need(judge(aStep('A19-idle-events', plain), false, IDLE.split('\n').slice(0, 4).join('\n')).length === 1, 'A19: no idle.check line yet is red');
const LINE = (m, c) => `cpu 0 online ...\nidle mode=${m} factor=4 states=C1,C2@mwait:0x21/127us,C3@mwait:0x60/1048us source=model check=${c} bm_demotions=0`;
need(judge(aStep('A20-idle-line', plain), false, LINE('auto', 'passed')).length === 0 && judge(aStep('A20-idle-line', plain), false, `${LINE('auto', 'failed')} check_failed=cpu8/0x60/slow_wake`).length === 2,
  'A20: the idle line with check=passed is green, a failed check red');
need(judge(aStep('A20-idle-line', plain), false, LINE('c1', 'pending')).length === 1, 'A20: a line in C1 without a passed check is red');
const KT = (p, f) => `ktest "gp.cpu.": passed=${p} failed=${f} skipped=0 refused=0`;
need(aStep('A21-ktest-cpu', plain).kind === 'ktest' && judge(aStep('A21-ktest-cpu', plain), false, KT(5, 0)).length === 0 && judge(aStep('A21-ktest-cpu', plain), false, KT(4, 1)).length === 1
  , 'A21: the five gp.cpu. tests');
const fPerf = checks.facts.find((f) => f.name === 'cpu_perf');
need(readFact(fPerf, false, CONFIG(' cpu_policy=performance')) === true && readFact(fPerf, false, CONFIG(' cpu_policy=balanced')) === false && readFact(fPerf, false, '(nothing)') === null,
  'cpu_perf is read from the kernel config line');
need(ph.requireProblems(ph.phase(checks, 'B1'), { gp_nvme_named: false })[0]?.includes('old bitsboot') && ph.requireProblems(ph.phase(checks, 'B1'), { gp_nvme_named: true }).length === 0,
  'B1 needs the Lexar named and says what to do without it');
need(ph.requireProblems(ph.phase(checks, 'B3'), { gp_nvme_named: true }).length === 1 && ph.requireProblems(ph.phase(checks, 'D'), {}).length === 0,
  'B3 needs the start from the SSD (an unknown fact is red too); D needs nothing');

// Both CONFIG states and the start from the SSD (GP-DISK's list against 5bcba1c4).
const ssdFact = checks.facts.find((f) => f.name === 'booted_from_ssd');
const SSD_LOADER = `${LOADER} nvme=on\n#5 [ 0.0] W boot loader.line bitsboot safe mode: only the NVMe controller gp_nvme=81:00.0/1d97:5236 may get a driver\n#6 [ 0.0] I boot loader.line stick: device path PciRoot(0x0)/Pci(0x1C,0x0)/Pci(0x0,0x0)/NVMe(0x1,00-00)/HD(2,GPT)\n#7 [ 0.0] I boot loader.line stick: the boot volume is an NVMe partition`;
need(readFact(ssdFact, false, SSD_LOADER) === true && readFact(ssdFact, false, LOADER) === false && readFact(ssdFact, false, '(none)') === null, 'booted_from_ssd is read from the loader lines');
const ssd = { gp_nvme_named: true, booted_from_ssd: true };
need(judge(shape(step('04-loader'), ssd), false, SSD_LOADER).length === 0 && judge(shape(step('04-loader'), ssd), false, SSD_LOADER.split('\n').slice(0, 3).join('\n')).length > 0,
  '04 from the SSD wants the NVMe device path and the NVMe boot volume');
need(judge(shape(step('05-config'), { gp_nvme_named: true }), false, LEXAR_CONFIG.replace('gp_nvme=81:00.0/1d97:5236 ', 'gp_nvme=81:00.0/1d97:5237 ')).length > 0, '05 wants the Lexar\'s own gp_nvme value');
need(judge(shape(step('25-stick'), ssd), false, 'I fat stick.mounted\nI fat stick.decided device=nvme0n1p2 nvme=true rw=true').length === 0
  && judge(shape(step('25-stick'), ssd), false, 'I fat stick.mounted\nI fat stick.decided device=sda1 rw=true').length > 0, '25 from the SSD wants the BITSOS partition as /boot');
const PLATTEN = 'BitsOS-Bericht für den Gaming-PC\nPlatten\n81:00.0 Lexar SSD NQ790 2TB: Treiber an (in CONFIG.TXT mit gp_nvme freigegeben)\n01:00.0 Samsung: kein Treiber, nicht gelesen (Georgs Windows- oder CachyOS-Platte, nie freigebbar)';
need(judge(shape(step('03-report'), { gp_nvme_named: true }), false, PLATTEN).every((b) => !b.includes('Treiber an') && !b.includes('nie freigebbar')), '03 with gp_nvme wants the Lexar\'s and the Samsungs\' rows');
need(['37-data-window', '38-data-moved'].every((id) => step(id).args.encoding === 'text'), '37 and 38 read text (read_file answers base64 by default)');
const b201 = ph.phase(checks, 'B2').steps.find((x) => x.id === 'B2-01-config');
const b201s = ph.withPlus(shape(b201, { gp_nvme_named: true }));
need(b201s.want.includes('nvme_data=') && b201s.want.includes('gp_nvme=81:00.0/1d97:5236') && judge(b201s, false, LEXAR_CONFIG).some((b) => b.includes('nvme_data=')),
  'a used step\'s own additions stay when a fact replaces its list (B2-01 wants nvme_data= with gp_nvme=)');

// keys, lines, keep.
const KEYS = text('input', { since: 40 });
need(JSON.stringify(ph.keysSeen(KEYS, '10f5:501b')) === JSON.stringify({ devices: ['3'], codes: [30, 31, 32, 33] }), `keys of the real keyboard only, the injected one not: ${JSON.stringify(ph.keysSeen(KEYS, '10f5:501b'))}`);
need(ph.nextSeq(text('input', {})) === 45 && ph.lastSeq(KEYS) === 44 && ph.lastSeq('events n=0 shown=0') === -1, 'next_seq and the last event read');
process.env.BITSOS_DESK_KEEP = join(runDir, 'keep dir');
ph.keep('lexar-guard', ph.lines('x\n  guard_hash ns1 0x1\ny', 'guard_hash '));
need(JSON.stringify(ph.kept('lexar-guard')) === JSON.stringify(['guard_hash ns1 0x1']) && ph.kept('nothing-kept') === null, 'kept lines come back; nothing kept is null');

// collect --phase against the stand-in.
const keepDir = join(runDir, 'collect keep');
rmSync(keepDir, { recursive: true, force: true });
const phaseRun = async (args, what, env = {}) => {
  asked.length = 0;
  const res = await node([collect, host, '--heartbeat-s', '1', '--confirm-wait-s', '0', '--out', join(runDir, `phase ${what}`), ...args], { USERPROFILE: prof, BITSOS_DESK_KEEP: keepDir, ...env });
  writeFileSync(join(runDir, `phase-${what}.txt`), res.out);
  return res;
};
configGone = false; rtlLog = RTL; reloadFails = false; msiSet = true; proof.done = true; proof.wrote = true;
calls = 0;
r = await phaseRun(['--phase', 'A'], 'a-nobundle');
need(r.code === 2 && r.out.includes('--bundle') && calls === 0, `phase A without --bundle: exit 2 before any call (exit ${r.code}, ${calls} requests)`);
r = await phaseRun(['--phase', 'C'], 'c-ask');
need(r.code === 3 && /^ok {3}C01-snd/m.test(r.out) && /^ASK {2}C04-tone: Frag Georg/m.test(r.out) && /--sound --from C04-tone/.test(r.out) && !asked.includes('snd_ton'),
  `phase C runs up to the tone and asks Georg first, nothing played (exit ${r.code})`);
r = await phaseRun(['--phase', 'C', '--from', 'C04-tone'], 'c-nosound');
need(r.code === 3 && !asked.includes('snd_ton'), `--from the tone without --sound still asks (exit ${r.code})`);
r = await phaseRun(['--phase', 'C', '--from', 'C04-tone', '--sound'], 'c-tone');
need(r.code === 0 && asked.filter((n) => n === 'snd_ton').length === 1 && /^ok {3}C04-tone snd_ton/m.test(r.out) && /^ok {3}C05-snd-after/m.test(r.out) && /after: {2}ask Georg/m.test(r.out),
  `with Georg's yes the tone plays once and the phase ends with the question to Georg (exit ${r.code})`);
r = await phaseRun(['--phase', 'C'], 'c-again');
need(r.code === 1 && /^RED {2}C03-no-sound-yet log: must not appear: uac\.tone_check/m.test(r.out) && !asked.includes('snd_ton'),
  `phase C again after a tone in this boot: red at C03, nothing played (exit ${r.code})`);
r = await phaseRun(['--phase', 'E', '--only', 'E03,E04,E05,E06,E07'], 'e-stages');
need(r.code === 0 && ['E03', 'E04', 'E05', 'E06', 'E07'].every((x) => new RegExp(`^ok {3}${x}-`, 'm').test(r.out)),
  `phase E's read-only stages are green on the real stages' answers (a cold card: no secure OS, the bootloader ready, nothing written) (exit ${r.code})`);
rtlLog = POLLED;
r = await phaseRun(['--phase', 'D'], 'd-stop');
need(r.code === 1 && /^RED {2}D01-rtl-log/m.test(r.out) && /^STOP phase D at D01-rtl-log/m.test(r.out) && !asked.includes('hw_info') && /never: {2}never restart the card/m.test(r.out),
  `phase D stops at its first red (stage 8 still a note) and calls nothing after it (exit ${r.code})`);
rtlLog = RTL + '\n#112 [ 41.0] I net rtl8126.irq_armed ifname=rtl0 kind=msix';
r = await phaseRun(['--phase', 'D', '--only', 'D01,D02'], 'd-msi');
need(r.code === 0 && /^ok {3}D01-rtl-log/m.test(r.out) && /^ok {3}D02-msi hw_info/m.test(r.out) && asked.filter((n) => n === 'hw_info').length === 2,
  `the interrupt armed: stage 8 ok, the vector's count rose between two calls (exit ${r.code})`);
// A phase of its own file (--checks): the guard hash kept and compared, Georg's keys, a count that stands still.
const mini = { facts: [], steps: [], phases: [{ id: 'T', title: 't', before: ['b'], never: ['n'], steps: [
  { id: 'T1-keep', kind: 'read', tool: 'nvme', args: { guard_hash: true }, keep: { name: 'lexar-guard', lines: 'guard_hash ' } },
  { id: 'T2-same', kind: 'read', tool: 'nvme', args: { guard_hash: true }, same: { name: 'lexar-guard', lines: 'guard_hash ' } },
  { id: 'T3-keys', kind: 'keys', ask: 'Georg: bitte a s d f an der echten Tastatur tippen', keys: { id: '10f5:501b', codes: [30, 31, 32, 33] }, wait_s_keys: 5 },
  { id: 'T4-still', kind: 'read', tool: 'hw_info', args: { section: 'msi' }, twice: { after_s: 1, rises: ph.phase(checks, 'D').steps.find((x) => x.id === 'D02-msi').twice.rises } },
] }] };
const mounts = { facts: [], steps: [], phases: [{ id: 'M', title: 'm', before: ['b'], never: ['n'], steps: [
  ph.phase(checks, 'B2').steps.find((x) => x.id === 'B2-04-mounts'), ph.phase(checks, 'B3').steps.find((x) => x.id === 'B3-04-mounts'),
  { id: 'M3-old', kind: 'read', tool: 'read_file', args: { path: '/proc/mounts', encoding: 'text' }, want: [' /data ext4 rw'] },
] }] };
writeFileSync(join(runDir, 'mounts checks.json'), JSON.stringify(mounts));
r = await phaseRun(['--checks', join(runDir, 'mounts checks.json'), '--phase', 'M'], 'm-mounts');
need(r.code === 1 && /^ok {3}B2-04-mounts run/m.test(r.out) && /^ok {3}B3-04-mounts run/m.test(r.out) && /^RED {2}M3-old read_file: the tool answered with an error/m.test(r.out),
  `the mounts through /bin/sh are green, read_file of /proc/mounts is an error as on the kernel (exit ${r.code})`);
const miniFile = join(runDir, 'mini checks.json');
writeFileSync(miniFile, JSON.stringify(mini));
typed = false;
r = await phaseRun(['--checks', miniFile, '--phase', 'T'], 't-ask');
need(r.code === 3 && /^ok {3}T1-keep/m.test(r.out) && /^ok {3}T2-same/m.test(r.out) && /^ASK {2}T3-keys/m.test(r.out) && !/--sound/.test(r.out.split('ASK')[1] ?? '')
  && /do not wait for his answer/.test(r.out) && asked.includes('input') && JSON.stringify(ph.kept('keys-t3-keys', keepDir)) === '["40"]',
  `a kept guard hash compares equal, the keys wait for Georg, the held run kept where the events stood (exit ${r.code})`);
// Georg types right after the session told him, before the resumed run reads anything: his keys still count.
typed = true;
r = await phaseRun(['--checks', miniFile, '--phase', 'T', '--from', 'T3-keys'], 't-keys');
need(r.code === 0 && /^ok {3}T3-keys/m.test(r.out), `the real keyboard's a s d f, typed before the resumed run, arrived (exit ${r.code})`);
keysAll = false;
r = await phaseRun(['--checks', miniFile, '--phase', 'T', '--from', 'T3-keys'], 't-keys-missing');
const inputCalls = asked.filter((n) => n === 'input').length;
need(r.code === 1 && /^RED {2}T3-keys keys: no key press of 10f5:501b for code\(s\) 33/m.test(r.out) && inputCalls <= 10,
  `an f only from MCP's injected device is red, and each call waited for new keys (exit ${r.code}, ${inputCalls} input calls in 5 s)`);
guardHash = '0x00000000feedface';
r = await phaseRun(['--checks', miniFile, '--phase', 'T', '--from', 'T2-same'], 't-differs');
need(r.code === 1 && /^RED {2}T2-same nvme: differs from what lexar-guard kept/m.test(r.out), `a changed guard hash is red (exit ${r.code})`);
keysAll = true; msiStep = 0;
r = await phaseRun(['--checks', miniFile, '--phase', 'T', '--from', 'T4-still'], 't-still');
need(r.code === 1 && /^RED {2}T4-still hw_info: the count did not rise in 1 s \(\d+ -> \d+\)/m.test(r.out), `a vector whose count stands still is red (exit ${r.code})`);
server.close();
console.log('remote desk check: green');
