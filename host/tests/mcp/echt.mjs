// echt.mjs (V2): the A1 check of BitsOS V2's MCP server against a real target, the idea of V1's
// werkzeuge/plattform/echt/echt.mjs. Runs the MCP protocol, tools/list and every tool that only looks, and
// `run` with the static hello programs and tiny test programs sent along as elf_b64 (nothing is written to the
// target's files), and checks the shape of each answer.
//
//   node echt.mjs --host <ip[:port]> [--token-file <path>] [--quick] [--only <check,...>]
//   BITSOS_TOKEN=... node echt.mjs --host 127.0.0.1:25300
//
// The token comes from --token-file (its first line) or BITSOS_TOKEN; it is never printed. Port 8765 unless
// given. run's 30 s default deadline is checked in parallel with the rest (it adds no time; --quick leaves it
// out; --slow is accepted and changes nothing, it was the old switch for it). Prints "ok <check>" / "FAIL <check>: why" per check and a
// summary; exit 0 when all pass, 1 when one fails, 2 when the target cannot be asked at all.
//
// What it changes on the target: nothing but the programs it runs (each ends by itself or is killed by stop),
// one `ktest` run of the kreg. tests (class Safe), `reboot` only as a dry run, and the MC-4a file checks in a
// folder /tmp/echt-<n> of its own (removed at the end). It never calls reload, send_keys, send_mouse,
// pci_write, mmio_write or pci_rescan. --http <me:port> [--http-bind <addr>] adds the fetch check: echt.mjs
// serves 1 MiB on that port and BitsOS fetches it from http://<me:port>/echt-blob (the address BitsOS reaches
// this machine at; QEMU slirp: 10.0.2.2).
// A2 (MC-4b), when the kernel has `install_kernel` (RB-6): `install.refusals` (bad arguments, R9: refused before
// anything is fetched) and, with --http and --install-bundle <dir> (KERNEL + INITRD, e.g. the running generation's
// files), `install.dry_run`: BitsOS fetches the bundle from http://<me:port>/echt-install/, checks it and plans the
// generation, writes nothing (the stick's SYS listing is the same before and after), and names the local hashes.
// `install.proof_counter`: wrong-token requests move hw_info's "mcp requests=" but not "answered=" (RB-6's proof).
// An install check that cannot run prints "SKIP <check>: why"; with --require-install it fails instead.
import { readFileSync } from 'node:fs';
import { createServer, request } from 'node:http';
import { setTimeout as sleep } from 'node:timers/promises';

const PROTOCOL = '2025-06-18';

function arg(name) {
  const i = process.argv.indexOf(name);
  return i > 0 ? process.argv[i + 1] : undefined;
}
const hostArg = arg('--host') ?? process.env.BITSOS_HOST;
if (!hostArg) {
  console.error('usage: node echt.mjs --host <ip[:port]> [--token-file <path>] [--quick] [--only <check,...>]');
  process.exit(2);
}
const [HOST, PORT] = hostArg.includes(':') ? [hostArg.split(':')[0], Number(hostArg.split(':')[1])] : [hostArg, 8765];
const tokenFile = arg('--token-file');
const TOKEN = (tokenFile ? readFileSync(tokenFile, 'utf8').split(/\r?\n/)[0] : process.env.BITSOS_TOKEN ?? '').trim();
if (!TOKEN) {
  console.error('no token: --token-file <path> or BITSOS_TOKEN');
  process.exit(2);
}
const QUICK = process.argv.includes('--quick');
const HTTP = arg('--http');
const HTTP_BIND = arg('--http-bind') ?? '0.0.0.0';
const ONLY = arg('--only')?.split(',');
const INSTALL_BUNDLE = arg('--install-bundle');
// --require-install: an install check that cannot run (no install_kernel, no bundle arguments) fails instead of
// printing SKIP (the desk's A2 run, so a default run never silently does nothing).
const REQUIRE_INSTALL = process.argv.includes('--require-install');

// ---------------------------------------------------------------------------------------------------------
// Transport: one connection per request (as V1's mcp-call.mjs; no keep-alive surprises).

function post(body, { token = TOKEN, ms = 30000 } = {}) {
  return new Promise((done, failed) => {
    const data = Buffer.from(typeof body === 'string' ? body : JSON.stringify(body));
    const req = request({ host: HOST, port: PORT, path: '/mcp', method: 'POST', agent: false, signal: AbortSignal.timeout(ms), headers: {
      'Content-Type': 'application/json', Accept: 'application/json, text/event-stream', 'MCP-Protocol-Version': PROTOCOL,
      Authorization: `Bearer ${token}`, 'Content-Length': data.length } }, (res) => {
      const parts = [];
      res.on('data', (d) => parts.push(d));
      res.on('end', () => done({ status: res.statusCode, headers: res.headers, text: Buffer.concat(parts).toString('utf8') }));
      res.on('error', failed);
    });
    req.on('error', failed);
    req.end(data);
  });
}

let nextId = 1;
async function rpc(method, params, opts) {
  const id = nextId++;
  const r = await post({ jsonrpc: '2.0', id, method, params: params ?? {} }, opts);
  let j;
  try { j = JSON.parse(r.text); } catch { throw new Error(`${method}: HTTP ${r.status}, no JSON: ${r.text.slice(0, 200)}`); }
  if (j.jsonrpc !== '2.0' || j.id !== id) throw new Error(`${method}: envelope ${JSON.stringify(j).slice(0, 200)}`);
  return { status: r.status, j };
}

// A tools/call answer, checked for MCP's shape: { isError, text (text items joined), images, rpcError }.
async function call(name, args = {}, ms) {
  const wait = (args.timeout_ms ?? (name === 'run' ? 30000 : 10000)) + 15000;
  const { status, j } = await rpc('tools/call', { name, arguments: args }, { ms: ms ?? wait });
  if (j.error) {
    if (typeof j.error.code !== 'number' || typeof j.error.message !== 'string') throw new Error(`${name}: bad error shape`);
    return { rpcError: j.error.code, text: j.error.message, isError: true, images: [] };
  }
  const r = j.result;
  if (status !== 200 || !r || !Array.isArray(r.content) || typeof r.isError !== 'boolean') throw new Error(`${name}: bad result shape ${JSON.stringify(j).slice(0, 200)}`);
  const images = [];
  for (const c of r.content) {
    if (c.type === 'text') {
      if (typeof c.text !== 'string') throw new Error(`${name}: text item without text`);
    } else if (c.type === 'image') {
      if (c.mimeType !== 'image/png' || typeof c.data !== 'string' || !/^[A-Za-z0-9+/]*={0,2}$/.test(c.data)) throw new Error(`${name}: bad image item`);
      images.push(Buffer.from(c.data, 'base64'));
    } else {
      throw new Error(`${name}: unknown content type ${c.type}`);
    }
  }
  return { isError: r.isError, text: r.content.filter((c) => c.type === 'text').map((c) => c.text).join('\n'), images };
}

// ---------------------------------------------------------------------------------------------------------
// Tiny static programs (the same bytes as crates/proc/src/mcp_run/progs.rs; nasm -f bin, org 0x400078).

function elf(code) {
  const base = 0x400000n, at = 120n, size = at + BigInt(code.length);
  const b = Buffer.alloc(Number(size));
  Buffer.from([0x7f, 0x45, 0x4c, 0x46, 2, 1, 1]).copy(b, 0);
  b.writeUInt16LE(2, 16); b.writeUInt16LE(0x3e, 18); b.writeUInt32LE(1, 20);
  b.writeBigUInt64LE(base + at, 24); b.writeBigUInt64LE(64n, 32);
  [64, 56, 1, 64, 0, 0].forEach((v, i) => b.writeUInt16LE(v, 52 + 2 * i));
  b.writeUInt32LE(1, 64); b.writeUInt32LE(5, 68);
  [0n, base, base, size, size, 0x1000n].forEach((v, i) => b.writeBigUInt64LE(v, 72 + 8 * i));
  Buffer.from(code).copy(b, Number(at));
  return b.toString('base64');
}
const hex = (s) => s.match(/../g).map((h) => parseInt(h, 16));
// FNV-1a 64 as "0x%016x" (stat's hash:true, fetch's answer)
function fnv1a64(buf) {
  let h = 0xcbf29ce484222325n;
  for (const b of buf) h = ((h ^ BigInt(b)) * 0x100000001b3n) & 0xffffffffffffffffn;
  return `0x${h.toString(16).padStart(16, '0')}`;
}
const PROGS = {
  spin: elf(hex('f390ebfc')),
  sleeploop: elf(hex('b801000000bf01000000488d3529000000ba050000000f05b823000000488d3d0600000031f60f05ebd6000000000000000000e1f505000000007469636b0a')),
  stderr: elf(hex('b801000000bf01000000488d352b000000ba0a0000000f05b801000000bf02000000488d351d000000ba0a0000000f05b8e7000000bf070000000f05746f207374646f75740a746f207374646572720a')),
  fault: elf(hex('b801000000bf01000000488d350f000000ba110000000f05c6042500000000016265666f726520746865206661756c740a')),
  echo: elf(hex('4881ec0010000031c031ff4889e6ba001000000f054885c07e144889c2b801000000bf010000004889e60f05ebd9b8e700000031ff0f05')),
};

// ---------------------------------------------------------------------------------------------------------
// Checks.

const results = [];
async function check(name, f) {
  if (ONLY && !ONLY.some((o) => name.startsWith(o))) return;
  const t0 = Date.now();
  try {
    await f();
    results.push({ name, ok: true });
    console.log(`ok ${name} (${Date.now() - t0} ms)`);
  } catch (e) {
    results.push({ name, ok: false });
    console.log(`FAIL ${name}: ${String(e.message ?? e).slice(0, 2000)}`);
  }
}
function skipped(name, why) {
  if (ONLY && !ONLY.some((o) => name.startsWith(o))) return;
  if (REQUIRE_INSTALL) {
    results.push({ name, ok: false });
    console.log(`FAIL ${name}: ${why} (--require-install)`);
  } else console.log(`SKIP ${name}: ${why}`);
}
function want(cond, what, text) {
  if (!cond) throw new Error(`${what}${text === undefined ? '' : `\n  answer: ${String(text).slice(0, 1500)}`}`);
}
const callNumber = (t) => Number((/\(call (\d+)\)/.exec(t) ?? [])[1]);

// `output` of call n until it finished (or `ms` passed): the final text.
async function resultOf(n, ms = 15000) {
  const end = Date.now() + ms;
  for (;;) {
    const o = await call('output', { call: n });
    if (o.text.startsWith(`call ${n} run: finished`) || /^ERROR: call \d+ run: finished/.test(o.text)) return o.text;
    want(Date.now() < end, `call ${n} did not finish within ${ms} ms`, o.text);
    await sleep(250);
  }
}

// Stops call n; its result (stop waits 0.5 s, then output until it ended).
async function stopped(n) {
  const s = await call('stop', { call: n });
  return /still running/.test(s.text.split('\n')[0]) ? resultOf(n) : s.text;
}

const A1_TOOLS = ['version', 'hw_info', 'net_info', 'cpus', 'ktest', 'reboot', 'msr_read', 'read_phys', 'screen', 'send_keys',
  'send_mouse', 'output', 'stop', 'run', 'log', 'reload', 'reload_info', 'pci_read', 'mmio_read',
  'read_file', 'write_file', 'list_dir', 'stat', 'delete', 'fetch',
  // MC-5b (A2)
  'ps', 'kill'];

async function main() {
  try {
    await rpc('ping', {}, { ms: 8000 });
  } catch (e) {
    console.log(`FAIL reach: ${HOST}:${PORT} does not answer MCP (${e.message})`);
    process.exit(2);
  }

  // run's own 30 s default deadline (no timeout_ms): the brief's key requirement for run, so always checked,
  // started first and awaited last, so it runs beside the other checks instead of adding 30 s.
  const defaultDeadline = QUICK ? null : check('run.default_deadline_30s', async () => {
    const t0 = Date.now();
    const r = await call('run', { elf_b64: PROGS.sleeploop, path: 'sleeploop-30s' }, 60000);
    const ms = Date.now() - t0;
    want(r.isError && /^Still running/.test(r.text) && ms >= 29000 && ms < 45000, `answered after ${ms} ms`, r.text);
    want(/--- stdout bytes 0\.\.\d+ ---\ntick\n/.test(r.text), 'the output so far', r.text);
    const t = await stopped(callNumber(r.text));
    want(/ended: signal 9 \(SIGKILL\), code 137\nstopped by the stop tool/.test(t), 'stop kills it', t);
  });

  await check('protocol.initialize', async () => {
    const { j } = await rpc('initialize', { protocolVersion: PROTOCOL, capabilities: {}, clientInfo: { name: 'echt-v2', version: '1' } });
    const r = j.result;
    want(r?.protocolVersion === PROTOCOL && r.serverInfo?.name === 'BitsOS' && r.capabilities?.tools, 'initialize result', JSON.stringify(j));
    want(/no poweroff tool/i.test(r.instructions ?? ''), 'instructions name the missing poweroff', r.instructions);
    const n = await post({ jsonrpc: '2.0', method: 'notifications/initialized' });
    want(n.status === 202, `notification: HTTP ${n.status}`);
  });
  await check('protocol.errors', async () => {
    const bad = await post({ jsonrpc: '2.0', id: 1, method: 'ping' }, { token: 'wrong-token-1234' });
    want(bad.status === 401, `wrong token: HTTP ${bad.status}`);
    const { j } = await rpc('no/such/method');
    want(j.error?.code === -32601, 'unknown method -32601', JSON.stringify(j));
    const u = await call('no_such_tool');
    want(u.rpcError === -32602 && /Unknown tool/.test(u.text), 'unknown tool -32602', u.text);
    const m = await call('msr_read', {});
    want(m.rpcError === -32602, 'missing argument -32602', m.text);
  });

  let tools = [];
  await check('tools.list', async () => {
    const { j } = await rpc('tools/list');
    tools = j.result?.tools ?? [];
    want(tools.length >= A1_TOOLS.length, `${tools.length} tools`);
    for (const t of tools) {
      const a = t.annotations ?? {};
      want(/^[a-z][a-z0-9_]*$/.test(t.name) && typeof t.description === 'string' && t.description.length > 5, `tool ${t.name}: name/description`);
      want(t.inputSchema?.type === 'object', `tool ${t.name}: inputSchema`);
      want(typeof a.owner === 'string' && a.owner && ['read', 'change', 'delete_overwrite'].includes(a.safety), `tool ${t.name}: B5 owner/safety`, JSON.stringify(a));
      // MC-6: a service of a uid other than 0 only claims its safety: cautious hints, verified:false.
      const hintsOk = a.verified === false ? a.readOnlyHint === false && a.destructiveHint === true : a.readOnlyHint === (a.safety === 'read') && a.destructiveHint === (a.safety === 'delete_overwrite');
      want(hintsOk, `tool ${t.name}: hints`, JSON.stringify(a));
      want(!/poweroff|power_off|shutdown/.test(t.name), `R8f: ${t.name}`);
    }
    const names = tools.map((t) => t.name);
    const missing = A1_TOOLS.filter((n) => !names.includes(n));
    want(missing.length === 0, `missing tools: ${missing.join(', ')}`);
    want(JSON.stringify(names) === JSON.stringify([...names].sort()), 'sorted by name');
    const run = tools.find((t) => t.name === 'run');
    want(run.inputSchema.properties?.timeout_ms?.default === 30000 && run.inputSchema.properties?.elf_b64, 'run: 30 s default deadline, elf_b64');
  });

  // Tools that only look.
  await check('tool.version', async () => {
    const r = await call('version');
    want(!r.isError && /^BitsOS V2 kernel \d+\.\d+/.test(r.text), 'version text', r.text);
  });
  await check('tool.hw_info', async () => {
    const r = await call('hw_info');
    for (const s of ['kernel', 'cpu', 'memory', 'firmware', 'interrupts', 'pci']) want(r.text.includes(`== ${s} ==`), `section ${s}`, r.text);
    want(/next start: /.test(r.text), 'next start', r.text);
    const c = await call('hw_info', { section: 'cpu' });
    want(!c.isError && /cpu vendor=\S/.test(c.text) && /avx2/.test(c.text), 'cpu section (x86-64-v3)', c.text);
  });
  await check('tool.cpus', async () => {
    const r = await call('cpus');
    const m = /cpus online=(\d+)/.exec(r.text);
    want(!r.isError && m && Number(m[1]) >= 1, 'cpus online=', r.text);
    console.log(`  cpus online=${m[1]}`);
  });
  await check('tool.net_info', async () => {
    const r = await call('net_info');
    want(!r.isError && /== interfaces ==/.test(r.text) && /link=up (?:\d+ [MG]bit\/s )?addr=\d+\.\d+\.\d+\.\d+\/\d+/.test(r.text), 'an interface with an address', r.text);
  });
  await check('tool.log', async () => {
    const r = await call('log', { max: 5 });
    want(!r.isError && /^log next=\d+/.test(r.text), 'log head', r.text);
    const e = await call('log', { level: 'error', max: 20 });
    want(!e.isError, 'log level=error', e.text);
    console.log(`  errors in the log: ${(e.text.match(/^#\d+ /gm) ?? []).length}`);
  });
  await check('tool.sched_locks', async () => {
    for (const [n, a, re] of [['sched', { max: 3 }, /^sched cpus=\d+/], ['locks', { max: 3 }, /^locks classes=\d+/]]) {
      if (!tools.some((t) => t.name === n)) continue;
      const r = await call(n, a);
      want(!r.isError && re.test(r.text), n, r.text);
    }
  });
  await check('tool.reload_info', async () => {
    const r = await call('reload_info');
    want(!r.isError && /^reload counter \d+/.test(r.text), 'reload_info', r.text);
  });
  await check('tool.msr_read', async () => {
    const r = await call('msr_read', { msr: '0xC0000080' });
    want(!r.isError && /^msr 0xc0000080 = 0x[0-9a-f]{16}/.test(r.text), 'EFER', r.text);
  });
  await check('tool.read_phys', async () => {
    const r = await call('read_phys', { addr: '0x100000', len: 16 });
    want(!r.isError && /^phys 0x100000 len=16\n0x0+100000: ([0-9a-f]{2} ){16}/.test(r.text), 'read_phys', r.text);
  });
  await check('tool.pci_read', async () => {
    const r = await call('pci_read', { function: '00:00.0', offset: 0 });
    want(!r.isError && /^pci 00:00\.0 \[[0-9a-f]{4}:[0-9a-f]{4}\]/.test(r.text) && /0x0000: [0-9a-f]{8}/.test(r.text), 'pci_read 00:00.0', r.text);
  });
  await check('tool.screen', async () => {
    const t = await call('screen', { format: 'text' });
    want(!t.isError && /^console \d+x\d+ cells/.test(t.text), 'screen text', t.text);
    const p = await call('screen', { scale: 8 });
    want(!p.isError && p.images.length === 1 && /^screen png \d+x\d+/.test(p.text), 'screen png', p.text);
    want(p.images[0].subarray(0, 8).equals(Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a])), 'PNG signature');
  });
  await check('tool.ktest', async () => {
    const r = await call('ktest', { filter: 'kreg.' });
    want(!r.isError && /passed=\d+ failed=0/.test(r.text), 'kreg. ktests', r.text);
  });
  await check('tool.reboot_dry_run', async () => {
    const r = await call('reboot', { dry_run: true });
    want(!r.isError && /^reboot \(dry run, nothing happens\)/.test(r.text) && /next start: /.test(r.text), 'reboot dry run', r.text);
  });
  await check('tool.output_stop_idle', async () => {
    const o = await call('output', { call: 999999999 });
    want(o.isError && /no call 999999999/.test(o.text), 'output of an unknown call', o.text);
  });

  // run: the static hello programs in the image, then the tiny programs sent along.
  await check('run.hello', async () => {
    const r = await call('run', { path: 'hello' });
    want(!r.isError && /^pid \d+ ended: exit 0\n--- stdout \(31 bytes\) ---\nhello from musl C on BitsOS V2\n--- stderr \(0 bytes\) ---\n$/.test(r.text), 'hello', r.text);
  });
  await check('run.hello_rs', async () => {
    const r = await call('run', { path: '/bin/hello-rs', args: ['a', 'b'], env: { X: '1' } });
    want(!r.isError && /ended: exit 0\n/.test(r.text) && /hello from Rust std on BitsOS V2, sum=10/.test(r.text), 'hello-rs', r.text);
  });
  await check('run.stderr_exit', async () => {
    const r = await call('run', { elf_b64: PROGS.stderr, path: 'two-streams' });
    want(!r.isError && /ended: exit 7\n--- stdout \(10 bytes\) ---\nto stdout\n--- stderr \(10 bytes\) ---\nto stderr\n$/.test(r.text), 'stdout and stderr apart, exit 7', r.text);
  });
  await check('run.fault', async () => {
    const r = await call('run', { elf_b64: PROGS.fault, path: 'faulty' });
    want(!r.isError && /ended: signal 11 \(SIGSEGV\), code 139\nProgram faulty \(pid \d+/.test(r.text) && /Page fault at 0x0/.test(r.text), 'fault message', r.text);
    want(/--- stdout \(17 bytes\) ---\nbefore the fault\n/.test(r.text), 'output before the fault', r.text);
  });
  await check('run.stdin_b64', async () => {
    const r = await call('run', { elf_b64: PROGS.echo, path: 'echo', stdin_b64: Buffer.from('hi\0\xffthere', 'latin1').toString('base64') });
    want(!r.isError && r.text.includes('ended: exit 0\n--- stdout (9 bytes) ---\nhi\u0000�there\n'), 'stdin bytes back on stdout', JSON.stringify(r.text));
  });
  await check('run.missing', async () => {
    const r = await call('run', { path: '/no/such/program' });
    want(r.isError && /no such file/i.test(r.text), 'a missing program', r.text);
  });
  await check('run.sleeploop_partial', async () => {
    const r = await call('run', { elf_b64: PROGS.sleeploop, path: 'sleeploop', timeout_ms: 700 });
    want(r.isError && /^Still running: run \(call \d+\)/.test(r.text) && /pid \d+ \/bin\/sleeploop is running/.test(r.text), 'deadline answer', r.text);
    want(/--- stdout bytes 0\.\.\d+ ---\ntick\ntick\n/.test(r.text), 'the output so far', r.text);
    const n = callNumber(r.text);
    await sleep(600);
    const o = await call('output', { call: n });
    want(!o.isError && o.text.startsWith(`call ${n} run: running after `) && /--- stdout bytes [1-9]\d*\.\.\d+ ---\ntick\n/.test(o.text), 'only new output', o.text);
    const t = await stopped(n);
    want(/ended: signal 9 \(SIGKILL\), code 137\nstopped by the stop tool/.test(t), 'stop kills it', t);
    want(((t.split('--- stdout (')[1] ?? '').match(/tick\n/g) ?? []).length >= 10, 'every tick in the result', t);
  });
  await check('run.spin_deadline_kill', async () => {
    const t0 = Date.now();
    const r = await call('run', { elf_b64: PROGS.spin, path: 'spin', timeout_ms: 500 });
    want(r.isError && /^Still running: run \(call \d+\)/.test(r.text) && /pid \d+ \/bin\/spin is running/.test(r.text) && Date.now() - t0 >= 450, 'deadline answer', r.text);
    const n = callNumber(r.text);
    await sleep(300);
    const o = await call('output', { call: n });
    want(o.text.startsWith(`call ${n} run: running`), 'still running after the answer', o.text);
    const t = await stopped(n);
    want(/ended: signal 9 \(SIGKILL\), code 137\nstopped by the stop tool/.test(t), 'stop kills a program that never leaves user mode', t);
  });

  // MC-5b: ps, kill, output/stop by pid (and the rest of V1's mcp2-test check 3: other tools answer while a
  // program runs).
  const psRow = (text, pid) => (text.split('\n').find((l) => l.trim().split(/\s+/)[0] === String(pid)) ?? '').trim().split(/\s+/);
  await check('procs.ps_kill_stop_by_pid', async () => {
    const r = await call('run', { elf_b64: PROGS.sleeploop, path: 'ps-sleeploop', args: ['a b'], timeout_ms: 500 });
    want(r.isError && /^Still running/.test(r.text), 'deadline answer', r.text);
    const pid = Number((/pid (\d+) /.exec(r.text) ?? [])[1]);
    const n = callNumber(r.text);
    const meanwhile = [];
    for (const [tool, args] of [['version', {}], ['list_dir', { path: '/' }], ['cpus', {}]]) {
      const t0 = Date.now();
      const a = await call(tool, args);
      meanwhile.push(Date.now() - t0);
      want(!a.isError, `${tool} answers while the program runs`, a.text);
    }
    want(meanwhile.every((ms) => ms < 4000), `answers while it runs: ${meanwhile.join(' ')} ms`);
    const all = await call('ps');
    want(!all.isError && /^processes \d+ \(zombie \d+, stopped \d+\), their threads \d+, kernel threads \d+; committed \d+ KiB of the limit \d+ KiB\n/.test(all.text), 'ps summary line', all.text);
    const row = psRow(all.text, pid);
    want(['S', 'R'].includes(row[5]) && row[6] === '1' && all.text.includes('ps-sleeploop a b'), `ps row of ${pid}`, all.text);
    const one = await call('ps', { pid });
    want(one.text.includes(`tid=${pid} state=`) && /class=\w+/.test(one.text), 'ps of one pid with its thread', one.text);
    const o = await call('output', { pid });
    want(!o.isError && o.text.startsWith(`call ${n} run: running after `), 'output by pid', o.text);
    let k = await call('kill', { pid, signal: 'SIGSTOP' });
    want(!k.isError && /sent SIGSTOP.*now stopped \(T\)$/.test(k.text), 'SIGSTOP stops it', k.text);
    want(psRow((await call('ps', { pid })).text, pid)[5] === 'T', 'ps shows T');
    k = await call('kill', { pid, signal: 'CONT' });
    want(!k.isError && /sent SIGCONT.*now running$/.test(k.text), 'SIGCONT continues it', k.text);
    const s = await call('stop', { pid, grace_ms: 2000 });
    const t = /still running/.test(s.text.split('\n')[0]) ? await resultOf(n) : s.text;
    want(/ended: signal 2 \(SIGINT\), code 130\nstopped by the stop tool \(SIGINT\)/.test(t), 'stop by pid: SIGINT ends it', t);
  });
  await check('procs.spin_cpu_time_and_kill', async () => {
    const r = await call('run', { elf_b64: PROGS.spin, path: 'ps-spin', timeout_ms: 500 });
    const pid = Number((/pid (\d+) /.exec(r.text) ?? [])[1]);
    const n = callNumber(r.text);
    await sleep(300);
    const row = psRow((await call('ps', { pid })).text, pid);
    want(row[5] === 'R' && Number(row[9]) > 0.1, `R with user CPU time: ${row.join(' ')}`);
    const k = await call('kill', { pid });
    want(!k.isError && k.text.includes('sent SIGTERM: its default action ends it; now ended: signal 15 (SIGTERM)'), 'kill (SIGTERM) ends it', k.text);
    const t = await resultOf(n);
    want(t.includes('ended: signal 15 (SIGTERM), code 143'), 'its run call hands over the end', t);
  });
  await check('procs.refusals', async () => {
    const all = await call('ps', { kernel: true, threads: true });
    if (psRow(all.text, 1).length > 1 && psRow(all.text, 1)[5] !== 'Z') {
      let k = await call('kill', { pid: 1, signal: 'SIGTERM' });
      want(k.isError && k.text.includes('PID 1 (init) gets SIGTERM only with confirm: true'), 'PID 1 without confirm', k.text);
      k = await call('stop', { pid: 1 });
      want(k.isError && k.text.includes('only with confirm'), 'stop of PID 1 without confirm', k.text);
      k = await call('kill', { pid: 1, signal: 0 });
      want(!k.isError && k.text.includes('exists'), 'signal 0 looks at PID 1', k.text);
    }
    // A kernel thread's id that is no pid or tid (pids and tids share one space; sched ids are another): it
    // names no process, so ESRCH, and the kernel thread is never signalled.
    const ids = new Set(all.text.split('\n').flatMap((l) => [l.trim().split(/\s+/)[0], /tid=(\d+) /.exec(l)?.[1]]));
    const kt = all.text.split('\n').map((l) => /^\s*kthread=(\d+) state=/.exec(l)?.[1]).find((id) => id && !ids.has(id) && Number(id) > 0);
    if (kt) {
      const k = await call('kill', { pid: Number(kt), signal: 9 });
      want(k.isError && k.text.includes(`no process ${kt} (ESRCH)`), `kernel thread ${kt}`, k.text);
    }
    const k = await call('kill', { pid: 4000000000 });
    want(k.isError && k.text.includes('(ESRCH)'), 'no such pid', k.text);
    const b = await call('kill', { pid: 2, signal: 65 });
    want(b.rpcError === -32602, 'bad signal -32602', b.text);
  });

  // MC-4a: the file tools in a folder of their own below /tmp (removed at the end).
  const dir = `/tmp/echt-${process.pid}`;
  const ok1 = (r, what) => { want(!r.isError, what, r.text); return r.text; };
  // MC-6: a user-space service (mcp-fakesvc, user/lib/mcpreg) registers tools through /run/bitsos/mcp.sock; they
  // are listed with owner, safety and pid, calls are forwarded, and the tools vanish when the program ends.
  await check('svc.register_call_vanish', async () => {
    const name = `echt${Date.now() % 100000}`;
    const listed = async () => (await rpc('tools/list')).j.result.tools;
    const r = await call('run', { path: '/bin/mcp-fakesvc', args: ['echo', name], timeout_ms: 1000 });
    want(r.isError && /^Still running/.test(r.text), 'the service runs on', r.text);
    const pid = Number((/pid (\d+) /.exec(r.text) ?? [])[1]);
    const n = callNumber(r.text);
    let t;
    for (let i = 0; i < 50 && !t; i++) {
      t = (await listed()).find((x) => x.name === `${name}_echo`);
      if (!t) await sleep(100);
    }
    want(t && t.annotations.owner === name && t.annotations.safety === 'read' && t.annotations.service_pid === pid && t.annotations.verified === false && t.annotations.readOnlyHint === false, 'listed with owner, safety, pid and verified:false (uid 1000)', JSON.stringify(t));
    const a = await call(`${name}_echo`, { n: 5 });
    want(!a.isError && a.text === 'echo {"n":5}', 'the call is forwarded and answered', a.text);
    const b = await call(`${name}_echo`, { n: 'five' });
    want(b.rpcError === -32602 && /Wrong type for argument: n/.test(b.text), 'the kernel checks the registered schema', b.text);
    want(!(await call('version')).isError, 'version answers');
    const s = await call('stop', { pid, grace_ms: 1000 });
    if (/still running/.test(s.text.split('\n')[0])) await resultOf(n);
    let gone = false;
    for (let i = 0; i < 50 && !gone; i++) {
      gone = !(await listed()).some((x) => x.name.startsWith(`${name}_`));
      if (!gone) await sleep(100);
    }
    want(gone, 'its tools vanish when the program ends');
    const g = await call(`${name}_echo`, {});
    want(g.rpcError === -32602 && /Unknown tool/.test(g.text), 'a vanished tool is unknown', g.text);
  });
  await check('files.roundtrip', async () => {
    ok1(await call('write_file', { path: `${dir}/a.txt`, text: 'hello\n', parents: true }), 'write_file new');
    ok1(await call('write_file', { path: `${dir}/a.txt`, text: 'J', offset: 0 }), 'write_file offset');
    ok1(await call('write_file', { path: `${dir}/a.txt`, data: Buffer.from('!\n').toString('base64'), append: true }), 'write_file append');
    const r = JSON.parse(ok1(await call('read_file', { path: `${dir}/a.txt`, encoding: 'text' }), 'read_file'));
    want(r.text === 'Jello\n!\n' && r.size === 8 && r.eof === true, 'content after offset+append', JSON.stringify(r));
    const big = Buffer.alloc(300 * 1024, 0).map((_, i) => (i * 13 + (i >> 10)) & 0xff);
    ok1(await call('write_file', { path: `${dir}/big`, data: big.subarray(0, 150 * 1024).toString('base64') }), 'big 1');
    ok1(await call('write_file', { path: `${dir}/big`, data: big.subarray(150 * 1024).toString('base64'), offset: 150 * 1024 }), 'big 2');
    const st = JSON.parse(ok1(await call('stat', { path: `${dir}/big`, hash: true }), 'stat'));
    want(st.size === big.length && st.fnv1a64 === fnv1a64(big) && st.type === 'file' && st.fs === 'tmpfs', 'stat size+hash', JSON.stringify(st));
    const back = JSON.parse(ok1(await call('read_file', { path: `${dir}/big`, offset: 1000, length: 5000 }), 'read range'));
    want(Buffer.from(back.data, 'base64').equals(big.subarray(1000, 6000)) && back.next_offset === 6000, 'read range', JSON.stringify(back).slice(0, 200));
    const l = ok1(await call('list_dir', { path: dir }), 'list_dir');
    want(l.startsWith(`${dir}: 2 entries (tmpfs)`) && / 8 a\.txt/.test(l) && new RegExp(` ${big.length} big`).test(l), 'list_dir', l);
    ok1(await call('delete', { path: `${dir}/a.txt` }), 'delete');
    const gone = await call('stat', { path: `${dir}/a.txt` });
    want(gone.isError && /ENOENT/.test(gone.text), 'deleted', gone.text);
  });
  await check('files.refusals', async () => {
    for (const [tool, args, re] of [
      ['write_file', { path: '/dev/console', text: 'x' }, /^Refused: .*\/dev holds/],
      ['read_file', { path: '/dev/console' }, /^Refused: .*device node/],
      // /tmp is its own tmpfs since INT-A1b: refused as a mount point before the top-level rule is reached.
      ['delete', { path: '/tmp' }, /^Refused: .*(top-level|mount point)/],
      ['write_file', { path: `${dir}/CONFIG.TXT`, text: 'nvme_write=on\n' }, /^Refused: .*R8a/],
      ['fetch', { url: 'http://8.8.8.8/x', path: `${dir}/x` }, /^Refused: .*R9/],
    ]) {
      const r = await call(tool, args);
      want(r.isError && re.test(r.text), `${tool} ${JSON.stringify(args)}`, r.text);
    }
    const r = await call('write_file', { path: 'tmp/relative', text: 'x' });
    want(r.rpcError === -32602, 'relative path: -32602', r.text);
  });
  await check('files.upload_run', async () => {
    const elfBytes = Buffer.from(PROGS.stderr, 'base64');
    ok1(await call('write_file', { path: `${dir}/bin/stderr`, data: PROGS.stderr, mode: '0755', parents: true }), 'upload');
    const st = JSON.parse(ok1(await call('stat', { path: `${dir}/bin/stderr`, hash: true }), 'stat'));
    want(st.mode === '0755' && st.fnv1a64 === fnv1a64(elfBytes), 'uploaded program', JSON.stringify(st));
    const r = await call('run', { path: `${dir}/bin/stderr` });
    want(!r.isError && /ended: exit 7\n--- stdout \(10 bytes\) ---\nto stdout\n--- stderr \(10 bytes\) ---\nto stderr\n/.test(r.text), 'run the uploaded program', r.text);
  });
  if (HTTP) {
    await check('files.fetch', async () => {
      const blob = Buffer.alloc(1 << 20).map((_, i) => (i * 7 + (i >> 12)) & 0xff);
      const [, port] = HTTP.split(':');
      const srv = createServer((req, res) => {
        if (req.url !== '/echt-blob') return res.writeHead(404).end();
        res.writeHead(200, { 'Content-Length': blob.length }).end(blob);
      });
      await new Promise((done, failed) => { srv.once('error', failed); srv.listen(Number(port), HTTP_BIND, done); });
      try {
        const r = await call('fetch', { url: `http://${HTTP}/echt-blob`, path: `${dir}/blob`, timeout_ms: 60000 });
        want(!r.isError && r.text.includes(`${blob.length} bytes`) && r.text.includes(fnv1a64(blob)), 'fetch', r.text);
        const st = JSON.parse(ok1(await call('stat', { path: `${dir}/blob`, hash: true }), 'stat'));
        want(st.size === blob.length && st.fnv1a64 === fnv1a64(blob), 'fetched file', JSON.stringify(st));
        const nf = await call('fetch', { url: `http://${HTTP}/nothing`, path: `${dir}/none` });
        want(nf.isError && /HTTP 404/.test(nf.text), '404', nf.text);
      } finally { srv.close(); }
    });
  }
  // A2 (MC-4b): install_kernel, RB-6's tool, only checks that write nothing.
  if (!tools.length) tools = (await rpc('tools/list')).j.result?.tools ?? [];  // --only without tools.list
  // RB-6's proof counter: a peer without the token moves mcp.requests, never mcp.answered (MC-4b: the boot
  // confirmation counts mcp.answered, so wrong-token traffic must not confirm a new generation).
  await check('install.proof_counter', async () => {
    const read = async () => {
      const t = ok1(await call('hw_info', { section: 'kernel' }), 'hw_info kernel');
      const m = /mcp requests=(\d+) answered=(\d+)/.exec(t);
      want(m, 'hw_info kernel names "mcp requests=N answered=N"', t);
      return { req: Number(m[1]), ans: Number(m[2]) };
    };
    const a = await read();
    const WRONG = 3;
    for (let i = 0; i < WRONG; i++) {
      const r = await post({ jsonrpc: '2.0', id: 1, method: 'tools/call', params: { name: 'version', arguments: {} } }, { token: `wrong-token-echt-${i}` });
      want(r.status === 401, `wrong token: HTTP ${r.status}`);
    }
    const b = await read();
    // Every request counts in requests, only authenticated answers in answered (other clients add to both).
    const dReq = b.req - a.req, dAns = b.ans - a.ans;
    want(dReq >= WRONG + 1 && dAns >= 1 && dAns <= dReq - WRONG, `requests +${dReq}, answered +${dAns}: the ${WRONG} wrong-token requests must count in requests only`);
  });
  if (!tools.some((t) => t.name === 'install_kernel')) {
    skipped('install.refusals', 'this kernel has no install_kernel (before RB-6)');
    skipped('install.dry_run', 'this kernel has no install_kernel (before RB-6)');
  } else {
    await check('install.refusals', async () => {
      const t = tools.find((x) => x.name === 'install_kernel');
      want(t.annotations?.owner === 'RB' && t.annotations?.safety === 'change', 'install_kernel: owner RB, safety change', JSON.stringify(t.annotations));
      const bad = await call('install_kernel', {});
      want(bad.rpcError === -32602, 'no url: invalid params (-32602)', bad.text);
      for (const [args, re] of [
        [{ url: 'http://8.8.8.8/b/', dry_run: true }, /^Refused: .*private/],
        [{ url: 'https://192.168.1.1/b/', dry_run: true }, /^Refused: /],
      ]) {
        const r = await call('install_kernel', args);
        want(r.isError && re.test(r.text), `install_kernel ${JSON.stringify(args)}`, r.text);
      }
      const typed = await call('install_kernel', { url: 'http://10.0.2.2/b/', dry_run: 'yes' });
      want(typed.rpcError === -32602, 'dry_run "yes": invalid params (-32602)', typed.text);
    });
    if (HTTP && INSTALL_BUNDLE) {
      await check('install.dry_run', async () => {
        const files = { '/echt-install/KERNEL': readFileSync(`${INSTALL_BUNDLE}/KERNEL`), '/echt-install/INITRD': readFileSync(`${INSTALL_BUNDLE}/INITRD`) };
        const [, port] = HTTP.split(':');
        const srv = createServer((req, res) => {
          const f = files[req.url];
          if (!f || req.method !== 'GET') return res.writeHead(404).end();
          res.writeHead(200, { 'Content-Length': f.length }).end(f);
        });
        await new Promise((done, failed) => { srv.once('error', failed); srv.listen(Number(port), HTTP_BIND, done); });
        try {
          const sys = async () => ok1(await call('list_dir', { path: '/boot/BITSOS2/SYS' }), 'list the stick\'s SYS');
          const before = await sys();
          const r = await call('install_kernel', { url: `http://${HTTP}/echt-install/`, dry_run: true, timeout_ms: 120000 });
          want(!r.isError && /^install_kernel \(dry run, nothing written\): would install generation \d+ \(NEW\)/.test(r.text), 'dry run', r.text);
          const k = fnv1a64(files['/echt-install/KERNEL']), i = fnv1a64(files['/echt-install/INITRD']);
          want(new RegExp(`KERNEL [^\n]*fnv ${k}`).test(r.text) && new RegExp(`INITRD [^\n]*fnv ${i}`).test(r.text), `hashes ${k} ${i}`, r.text);
          want((await sys()) === before, 'the stick\'s SYS listing changed', before);
        } finally { srv.close(); }
      });
    } else skipped('install.dry_run', 'needs --http <me:port> and --install-bundle <dir> (KERNEL + INITRD)');
  }
  await check('files.cleanup', async () => {
    ok1(await call('delete', { path: dir, recursive: true }), 'delete the folder');
    const l = ok1(await call('list_dir', { path: '/tmp' }), 'list /tmp');
    want(!l.includes(`echt-${process.pid}`), 'folder gone', l);
  });
  if (defaultDeadline) await defaultDeadline;

  const failed = results.filter((r) => !r.ok);
  console.log(`echt ${HOST}:${PORT}: ${results.length - failed.length} of ${results.length} ok${failed.length ? `; failed: ${failed.map((f) => f.name).join(', ')}` : ''}`);
  process.exit(failed.length ? 1 : 0);
}

main().catch((e) => {
  console.log(`FAIL echt: ${e.stack ?? e}`);
  process.exit(2);
});
