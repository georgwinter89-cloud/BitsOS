// reload.mjs (V2): a freshly built kernel onto the running BitsOS V2 over the network, without the stick.
// Builds the bundle (cargo xtask image --name <n>), serves its KERNEL and INITRD over a tiny HTTP server,
// calls the kernel's `reload` tool (RB) with that URL, waits until MCP answers again and checks that a new
// kernel runs: the Reload counter rose by one, `version` names the new start and `reload_info` shows the
// served initramfs (size and FNV-1a 64). --count N does that N times in a row, with the time of each round.
//
//   node reload.mjs <BitsOS host[:port]> [--name <n>] [--no-build] [--kernel <file> --initrd <file>]
//        [--bind <address>] [--port <p>] [--me <address>] [--count N] [--back-s S] [--token-file <path>]
//
// --name: the image name for `cargo xtask image` (default reload); --no-build serves what the last build of
// that name left in target/xtask/esp-<n>/BITSOS2/SYS/<newest>/; --kernel/--initrd serve given files instead.
// --bind: where the HTTP server listens (default 0.0.0.0; 127.0.0.1 for QEMU's slirp); --port: its port (an
// MC lane port, default 25410; the desk uses its own range); --me: the address BitsOS fetches from (default:
// 10.0.2.2 when the host is 127.0.0.1 = QEMU slirp, else this machine's address on the way to BitsOS).
// The server answers only GET /KERNEL and /INITRD, and only while this runs. The token as mcp-call.mjs takes
// it (BITSOS_TOKEN or --token-file), never printed.
// Exit code 0 when every round came back with the new kernel, 1 with the reason otherwise, 2 when BitsOS
// cannot be asked at the start.
import { execFileSync } from 'node:child_process';
import { createSocket } from 'node:dgram';
import { existsSync, readdirSync, readFileSync } from 'node:fs';
import { createServer } from 'node:http';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { setTimeout as sleep } from 'node:timers/promises';
import { connect, fnv1a64, McpError, readToken } from './mcp-call.mjs';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..', '..');

function fail(why, code = 1) {
  console.error(`reload: ${why}`);
  process.exit(code);
}

const opts = {}, rest = [];
const FLAGS = ['--name', '--kernel', '--initrd', '--bind', '--port', '--me', '--count', '--back-s', '--token-file'];
for (let i = 2; i < process.argv.length; i++) {
  const a = process.argv[i];
  if (FLAGS.includes(a)) opts[a.slice(2)] = process.argv[++i] ?? '';
  else if (a === '--no-build') opts['no-build'] = true;
  else rest.push(a);
}
const [host] = rest;
if (!host || rest.length > 1 || Boolean(opts.kernel) !== Boolean(opts.initrd)) {
  fail('usage: node reload.mjs <host[:port]> [--name n] [--no-build] [--kernel F --initrd F] [--bind A] [--port P] [--me A] [--count N] [--back-s S] [--token-file F]', 2);
}
const name = opts.name ?? 'reload';
const port = Number(opts.port ?? 25410);
const count = Number(opts.count ?? 1);
const backMs = Number(opts['back-s'] ?? 120) * 1000;
if (!(port >= 1 && port <= 65535)) fail(`--port wants 1 to 65535, not ${opts.port}`, 2);
if (!(Number.isInteger(count) && count >= 1)) fail(`--count wants a whole number >= 1, not ${opts.count}`, 2);
if (!/^[A-Za-z0-9_-]+$/.test(name)) fail(`--name '${name}': only letters, digits, - and _`, 2);

// The bundle: the given files, or the newest generation of target/xtask/esp-<name>
function bundleFiles() {
  if (opts.kernel) return [opts.kernel, opts.initrd];
  if (!opts['no-build']) {
    console.log(`building: cargo xtask image --name ${name}`);
    try {
      execFileSync('cargo', ['xtask', 'image', '--name', name], { cwd: ROOT, stdio: ['ignore', 'inherit', 'inherit'] });
    } catch { fail('cargo xtask image failed'); }
  }
  const sys = join(ROOT, 'target', 'xtask', `esp-${name}`, 'BITSOS2', 'SYS');
  if (!existsSync(sys)) fail(`${sys} does not exist: build it (without --no-build)`);
  const gens = readdirSync(sys).filter((g) => /^\d+$/.test(g)).sort((a, b) => b - a);
  if (!gens.length) fail(`no generation in ${sys}`);
  return [join(sys, gens[0], 'KERNEL'), join(sys, gens[0], 'INITRD')];
}
const [kernelFile, initrdFile] = bundleFiles();
let kernel, initrd;
try { kernel = readFileSync(kernelFile); initrd = readFileSync(initrdFile); } catch (e) { fail(`cannot read the bundle: ${e.message}`); }
if (kernel.length < 64 || kernel.toString('latin1', 0, 4) !== '\x7fELF') fail(`${kernelFile} is no ELF kernel`);
const initrdFnv = fnv1a64(initrd);

let token;
try { token = readToken(opts['token-file']); } catch (e) { fail(e.message, 2); }
const bitsos = host.replace(/:\d+$/, '');

async function localAddress(to) {  // the address this machine sends from to `to`
  const s = createSocket('udp4');
  try {
    await new Promise((done, failed) => { s.once('error', failed); s.connect(8765, to, done); });
    return s.address().address;
  } finally { s.close(); }
}
const me = opts.me || (bitsos === '127.0.0.1' || bitsos === 'localhost' ? '10.0.2.2' : await localAddress(bitsos).catch(() => fail(`no way from here to ${bitsos}`)));
const url = `http://${me}:${port}/`;

let fetches = [];
const server = createServer((req, res) => {
  const file = { '/KERNEL': kernel, '/INITRD': initrd }[req.url];
  if (req.method !== 'GET' || !file) return res.writeHead(404).end();
  const t0 = Date.now();
  res.on('finish', () => fetches.push(`${req.url.slice(1)} ${file.length} B in ${Date.now() - t0} ms`));
  res.writeHead(200, { 'Content-Type': 'application/octet-stream', 'Content-Length': file.length }).end(file);
});
await new Promise((done) => {
  server.once('error', (e) => fail(e.code === 'EADDRINUSE' ? `port ${port} is taken here: --port <another>` : `cannot serve on port ${port}: ${e.message}`));
  server.listen(port, opts.bind || '0.0.0.0', done);
});
console.log(`serving KERNEL (${kernel.length} B) and INITRD (${initrd.length} B, fnv ${initrdFnv}) as ${url} (listening on ${opts.bind || '0.0.0.0'}:${port})`);

// What the running kernel says about its start: { version, counter, initrdBytes, initrdFnv }
async function startFacts(session) {
  const v = await session.call('version', {});
  const info = await session.call('reload_info', {});
  const m = info.text.match(/reload counter (\d+)/);
  const ir = info.text.match(/initramfs (\d+) bytes at \S+, fnv (0x[0-9a-f]+)/);
  return { version: v.text.trim(), counter: m ? Number(m[1]) : null, initrdBytes: ir ? Number(ir[1]) : null, initrdFnv: ir ? ir[2] : null };
}

let session;
try {
  session = await connect(host, { token });
} catch (e) {
  if (!(e instanceof McpError)) throw e;
  server.close();
  fail(`${e.message}: is BitsOS running with its MCP server (CONFIG.TXT token=...)?`, 2);
}
let before = await startFacts(session);
console.log(`BitsOS answers at ${session.url}: ${before.version.replace('\n', ' | ')}`);

const rounds = [];
for (let round = 1; round <= count; round++) {
  fetches = [];
  const t0 = Date.now();
  let answer;
  try {
    answer = await session.call('reload', { url: `${url}KERNEL`, initrd: `${url}INITRD`, timeout_ms: 60000 });
  } catch (e) {
    if (!(e instanceof McpError)) throw e;
    // The answer may be lost when the old kernel goes down right after sending it.
    if (!fetches.length) { server.close(); fail(`round ${round}: ${e.message}, before BitsOS fetched anything`); }
    console.log(`round ${round}: ${e.message}; BitsOS had fetched the bundle, so it may start anyway`);
  }
  const tAnswer = Date.now() - t0;
  if (answer?.isError) { server.close(); fail(`round ${round}: reload refused: ${answer.text}${fetches.length ? '' : `\nBitsOS never fetched from ${url}: is --me right, and does the firewall let node take connections there?`}`); }
  if (answer) console.log(`round ${round}: ${answer.text.trim()}`);
  let back = null;
  const tWait = Date.now();
  while (!back && Date.now() - tWait < backMs) {
    await sleep(500);
    try {
      const s = await connect(host, { token, ms: 3000 });
      const f = await startFacts(s);
      // INT-W7: `version` and `reload_info` are two requests; the old kernel may still answer the first while it
      // writes back before its jump (seen once: version from the old start, counter from the new). Once the
      // counter moved the old kernel is gone, so ask both again.
      if (f.counter !== before.counter) back = { s, f: await startFacts(s) };
    } catch (e) {
      if (e instanceof McpError && e.kind === 'token') { server.close(); fail(`round ${round}: the new kernel refuses the token: ${e.message}`); }
    }
  }
  if (!back) { server.close(); fail(`round ${round}: BitsOS did not answer MCP with a new start within ${backMs / 1000} s (fetched: ${fetches.join(', ') || 'nothing'})`); }
  const total = Date.now() - t0;
  const f = back.f;
  const problems = [];
  if (before.counter !== null && f.counter !== before.counter + 1) problems.push(`Reload counter ${f.counter}, not ${before.counter + 1}`);
  if (f.version === before.version) problems.push('version names the same start');
  if (f.initrdBytes !== initrd.length || f.initrdFnv !== initrdFnv) problems.push(`initramfs ${f.initrdBytes} B fnv ${f.initrdFnv}, served ${initrd.length} B fnv ${initrdFnv}`);
  console.log(`round ${round}: back after ${total} ms (answer ${tAnswer} ms, MCP again ${total - tAnswer} ms later); ${f.version.split('\n')[1] ?? ''}; fetched ${fetches.join(', ')}`);
  if (problems.length) { server.close(); fail(`round ${round}: ${problems.join('; ')}`); }
  rounds.push(total);
  session = back.s;
  before = f;
}
server.close();
const ms = rounds.map((r) => `${(r / 1000).toFixed(1)} s`).join(', ');
console.log(`reload: ${count} of ${count} rounds came back with the new kernel (${ms}; slowest ${(Math.max(...rounds) / 1000).toFixed(1)} s)`);
process.exit(0);
