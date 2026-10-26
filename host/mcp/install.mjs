// install.mjs (V2, MC-4b): a kernel bundle onto BitsOS V2's boot stick over the network, as a new system
// generation (RB-6's `install_kernel`), without plugging the stick over. Builds nothing: it serves the given
// KERNEL and INITRD on a small HTTP server bound to this machine's LAN address, calls `install_kernel` with
// that URL, prints the answer and checks that the hashes BitsOS names are those of the local files.
//
//   node install.mjs <BitsOS host[:port]> (--bundle <dir> | --kernel <file> --initrd <file>) --bind <LAN address>
//        [--me <address>] [--port <8770-8799>] [--dry-run] [--config <file>] [--unpin] [--allow-no-fallback]
//        [--timeout-ms N] [--upload] [--reboot [--back-s S] [--confirm-s S]] [--token-file <path>]
//
// --bundle: a directory holding KERNEL and INITRD (e.g. target/xtask/esp-<name>/BITSOS2/SYS/1);
// --bind: the address of this machine the server listens on (the Gaming-PC's LAN address; 127.0.0.1 for QEMU's
// slirp); never 0.0.0.0. --me: the address BitsOS fetches from (default: --bind, or 10.0.2.2 for 127.0.0.1).
// --port: the server's port, in 8770-8799 (default: the first free one there). The server answers only GET (and
// HEAD) of <random dir>/KERNEL and <random dir>/INITRD, only while this runs.
// --dry-run: BitsOS fetches and checks everything and plans the generation, and writes nothing.
// --config: a whole new \BITSOS2\CONFIG.TXT (BitsOS refuses one that adds or changes nvme_write,
// stick_write or usb_msd_write, or drops the token). --unpin, --allow-no-fallback: as install_kernel takes them.
// --upload: no HTTP server; the files go up with write_file into /tmp/install-<pid>/ (hash checked) and
// install_kernel reads them there (kernel_path/initrd_path; needs a kernel whose install_kernel has them), the
// uploads are deleted afterwards. Slower (about 1,000 requests for a 120 MB kernel), for when no port is open.
// --reboot: after a real install, `reboot`, wait until MCP answers from a new start (--back-s, default 180), check
// that the new generation runs as its one try, then call reload_info every 2 s (the proof needs answers with no
// pause over boot_confirm_s/4, at least 6 s) until it says the generation is confirmed (--confirm-s, default 300).
// The token as mcp-call.mjs takes it (BITSOS_TOKEN or --token-file), never printed. BITSOS_INSTALL_PORTS=<lo>-<hi>
// replaces the port range (the lane's tests only, which keep to their own port range).
// Exit code 0: installed (or the dry run passed, or with --reboot: confirmed), with the hashes equal; 1: refused,
// failed, a hash differs, or the new generation did not come up or confirm; 2: bad arguments, or BitsOS cannot
// be asked.
import { randomBytes } from 'node:crypto';
import { existsSync, readFileSync, statSync } from 'node:fs';
import { createServer } from 'node:http';
import { join } from 'node:path';
import { setTimeout as sleep } from 'node:timers/promises';
import { connect, fnv1a64, McpError, readToken, upload } from './mcp-call.mjs';

// The install server's ports (the desk's range, open in the Gaming-PC's firewall). BITSOS_INSTALL_PORTS=<lo>-<hi>
// moves it, for the lane's own tests only (host/tests convention: scenarios use their lane's port range).
let PORT_LO = 8770, PORT_HI = 8799;
if (process.env.BITSOS_INSTALL_PORTS) {
  const m = /^(\d+)-(\d+)$/.exec(process.env.BITSOS_INSTALL_PORTS);
  if (!m || Number(m[1]) < 1024 || Number(m[2]) > 65535 || Number(m[1]) > Number(m[2])) fail(`BITSOS_INSTALL_PORTS=${process.env.BITSOS_INSTALL_PORTS}: want <lo>-<hi> within 1024-65535`, 2);
  [PORT_LO, PORT_HI] = [Number(m[1]), Number(m[2])];
}

function fail(why, code = 1) {
  console.error(`install: ${why}`);
  process.exit(code);
}

const VALUE = ['--bundle', '--kernel', '--initrd', '--bind', '--me', '--port', '--config', '--timeout-ms', '--back-s', '--confirm-s', '--token-file'];
const SWITCH = ['--dry-run', '--unpin', '--allow-no-fallback', '--upload', '--reboot'];
const opts = {}, rest = [];
for (let i = 2; i < process.argv.length; i++) {
  const a = process.argv[i];
  if (VALUE.includes(a)) {
    const v = process.argv[++i];
    if (v === undefined) fail(`${a} needs a value`, 2);
    opts[a.slice(2)] = v;
  } else if (SWITCH.includes(a)) opts[a.slice(2)] = true;
  else if (a.startsWith('--')) fail(`unknown option ${a}`, 2);
  else rest.push(a);
}
const USAGE = 'usage: node install.mjs <host[:port]> (--bundle DIR | --kernel F --initrd F) --bind A [--me A] [--port 8770-8799] [--dry-run] [--config F] [--unpin] [--allow-no-fallback] [--timeout-ms N] [--upload] [--reboot [--back-s S] [--confirm-s S]] [--token-file F]';
const [host] = rest;
if (!host || rest.length > 1) fail(USAGE, 2);
if (opts.bundle && (opts.kernel || opts.initrd)) fail('--bundle or --kernel/--initrd, not both', 2);
if (!opts.bundle && !(opts.kernel && opts.initrd)) fail(`the bundle is missing: --bundle DIR or --kernel F --initrd F\n${USAGE}`, 2);
if (opts.reboot && opts['dry-run']) fail('--reboot after a --dry-run would start nothing new', 2);

const IPV4 = /^(\d{1,3})\.(\d{1,3})\.(\d{1,3})\.(\d{1,3})$/;
// The networks BitsOS fetches from (its URL rule, R9): 10/8, 172.16/12, 192.168/16, 127/8, 169.254/16.
function privateIpv4(a) {
  const m = IPV4.exec(a);
  if (!m || m.slice(1).some((x) => Number(x) > 255)) return false;
  const [x, y] = [Number(m[1]), Number(m[2])];
  return x === 10 || x === 127 || (x === 172 && y >= 16 && y <= 31) || (x === 192 && y === 168) || (x === 169 && y === 254);
}
const bind = opts.bind;
if (!bind) fail(`--bind <this machine's LAN address> is missing (the server listens only there)\n${USAGE}`, 2);
if (bind === '0.0.0.0') fail('--bind 0.0.0.0 would serve the kernel on every network of this machine; give its LAN address', 2);
if (!privateIpv4(bind)) fail(`--bind ${bind}: an IPv4 address of this machine on a private network (BitsOS fetches only from those)`, 2);
const me = opts.me ?? (bind.startsWith('127.') ? '10.0.2.2' : bind);
if (!privateIpv4(me)) fail(`--me ${me}: BitsOS fetches only from private IPv4 addresses`, 2);
let port = null;
if (opts.port !== undefined) {
  port = Number(opts.port);
  if (!(Number.isInteger(port) && port >= PORT_LO && port <= PORT_HI)) fail(`--port ${opts.port}: the install server uses ${PORT_LO}-${PORT_HI}`, 2);
}
const timeoutMs = Number(opts['timeout-ms'] ?? 300000);
if (!(Number.isInteger(timeoutMs) && timeoutMs >= 1000)) fail(`--timeout-ms ${opts['timeout-ms']}: a whole number of milliseconds >= 1000`, 2);
const backMs = Number(opts['back-s'] ?? 180) * 1000, confirmMs = Number(opts['confirm-s'] ?? 300) * 1000;
if (!(backMs > 0) || !(confirmMs > 0)) fail('--back-s and --confirm-s want seconds > 0', 2);

// The bundle (read once: what is served is what is hashed)
const kernelFile = opts.kernel ?? join(opts.bundle, 'KERNEL');
const initrdFile = opts.initrd ?? join(opts.bundle, 'INITRD');
function readBundleFile(f) {
  if (!existsSync(f) || !statSync(f).isFile()) fail(`${f} is no file`, 2);
  try { return readFileSync(f); } catch (e) { fail(`cannot read ${f}: ${e.message}`, 2); }
}
const kernel = readBundleFile(kernelFile), initrd = readBundleFile(initrdFile);
if (kernel.length < 64 || kernel.toString('latin1', 0, 4) !== '\x7fELF') fail(`${kernelFile} is no ELF kernel`, 2);
if (!['070701', '070702'].includes(initrd.toString('latin1', 0, 6))) fail(`${initrdFile} is no newc cpio archive (an initramfs)`, 2);
let config;
if (opts.config) config = readBundleFile(opts.config).toString('utf8');
const fnv = { KERNEL: fnv1a64(kernel), INITRD: fnv1a64(initrd) };
console.log(`bundle: KERNEL ${kernel.length} bytes (fnv ${fnv.KERNEL}) from ${kernelFile}; INITRD ${initrd.length} bytes (fnv ${fnv.INITRD}) from ${initrdFile}`);

let token;
try { token = readToken(opts['token-file']); } catch (e) { fail(e.message, 2); }

// ---- the HTTP server (not with --upload) ----
const dir = `/install-${randomBytes(6).toString('hex')}/`;
const served = { KERNEL: 0, INITRD: 0 };
let server = null;
async function serve() {
  const files = { [`${dir}KERNEL`]: ['KERNEL', kernel], [`${dir}INITRD`]: ['INITRD', initrd] };
  server = createServer((req, res) => {
    const f = files[req.url];
    if (!f || (req.method !== 'GET' && req.method !== 'HEAD')) {
      console.log(`server: ${req.method} ${req.url} from ${req.socket.remoteAddress}: 404`);
      return res.writeHead(404, { 'Content-Length': 0 }).end();
    }
    const [name, body] = f;
    const t0 = Date.now();
    res.on('finish', () => {
      if (req.method === 'GET') served[name] += body.length;
      console.log(`server: ${name} ${req.method === 'GET' ? `${body.length} bytes` : 'head'} to ${req.socket.remoteAddress} in ${Date.now() - t0} ms`);
    });
    res.writeHead(200, { 'Content-Type': 'application/octet-stream', 'Content-Length': body.length });
    res.end(req.method === 'GET' ? body : undefined);
  });
  const listen = (p) => new Promise((done) => {
    const onError = (e) => done(e);
    server.once('error', onError);
    server.listen(p, bind, () => { server.off('error', onError); done(null); });
  });
  for (const p of port !== null ? [port] : Array.from({ length: PORT_HI - PORT_LO + 1 }, (_, i) => PORT_LO + i)) {
    const e = await listen(p);
    if (!e) { port = p; return; }
    if (e.code === 'EADDRNOTAVAIL') fail(`--bind ${bind} is no address of this machine`, 2);
    if (e.code !== 'EADDRINUSE') fail(`cannot serve on ${bind}:${p}: ${e.message}`, 2);
    if (port !== null) fail(`port ${p} is taken on ${bind}: another --port in ${PORT_LO}-${PORT_HI}, or leave it out`, 2);
  }
  fail(`every port ${PORT_LO}-${PORT_HI} is taken on ${bind}`, 2);
}
function done(code) {
  if (server) server.close();
  process.exit(code);
}

// What RB's install_kernel answer says (pure; the protocol test checks it against the kernel's text).
function parseAnswer(text) {
  const g = /(?:installed|would install) generation (\d+) \(NEW\)/.exec(text);
  const k = /KERNEL [^\n]*?fnv (0x[0-9a-f]{16})/.exec(text);
  const i = /INITRD [^\n]*?fnv (0x[0-9a-f]{16})/.exec(text);
  const next = /next boot starts: ([^\n]*)/.exec(text);
  return { generation: g ? Number(g[1]) : null, kernelFnv: k?.[1] ?? null, initrdFnv: i?.[1] ?? null, next: next?.[1] ?? null,
    dry: text.startsWith('install_kernel (dry run'), readBack: text.includes('both read back from the stick and equal') };
}

// ---- BitsOS ----
let session;
try {
  session = await connect(host, { token });
} catch (e) {
  if (!(e instanceof McpError)) throw e;
  fail(`${e.message}: is BitsOS running with its MCP server (CONFIG.TXT token=...)?`, 2);
}
const startLine = async (s) => (await s.call('version', {})).text.split('\n')[1] ?? '';
const before = await startLine(session);
console.log(`BitsOS answers at ${session.url}: ${before}`);

const args = { timeout_ms: timeoutMs };
let remote = null;
if (opts.upload) {
  remote = `/tmp/install-${process.pid}`;
  try {
    for (const [name, body] of [['KERNEL', kernel], ['INITRD', initrd]]) {
      const u = await upload(session, body, `${remote}/${name}`, { parents: true });
      console.log(`upload: ${name} ${u.bytes} bytes to ${remote}/${name} in ${u.ms} ms, hash ${u.fnv} checked`);
    }
  } catch (e) {
    if (!(e instanceof McpError)) throw e;
    await session.call('delete', { path: remote, recursive: true }).catch(() => {});
    fail(`upload: ${e.message}`);
  }
  Object.assign(args, { kernel_path: `${remote}/KERNEL`, initrd_path: `${remote}/INITRD` });
} else {
  await serve();
  args.url = `http://${me}:${port}${dir}`;
  console.log(`serving the bundle as ${args.url} (listening on ${bind}:${port} only)`);
}
if (opts['dry-run']) args.dry_run = true;
if (config !== undefined) args.config = config;
if (opts.unpin) args.unpin = true;
if (opts['allow-no-fallback']) args.allow_no_fallback = true;

const t0 = Date.now();
let answer;
try {
  answer = await session.call('install_kernel', args);
} catch (e) {
  if (!(e instanceof McpError)) throw e;
  const hint = /unknown tool|not found|-32601|-32602/i.test(e.message)
    ? ' (a kernel before RB-6 has no install_kernel, and one before MC-4b change request 1 no kernel_path for --upload: Reload into a newer kernel first, host/mcp/reload.mjs)'
    : '';
  if (remote) await session.call('delete', { path: remote, recursive: true }).catch(() => {});
  fail(`install_kernel: ${e.message}${hint}`, e.kind === 'answer' ? 1 : 2);
}
const ms = Date.now() - t0;
if (remote) {
  const d = await session.call('delete', { path: remote, recursive: true });
  if (d.isError) console.log(`note: the uploads in ${remote} stay: ${d.text}`);
}
console.log(answer.text.trim().split('\n').map((l) => `| ${l}`).join('\n'));
if (answer.isError) {
  const fetched = served.KERNEL || served.INITRD;
  const hint = remote || fetched || /nothing fetched|R9|private|https|refused before/i.test(answer.text)
    ? '' : `\nBitsOS never fetched from ${args.url}: is --me right, and does the firewall let connections to port ${port} through?`;
  console.error(`install: install_kernel refused or failed after ${ms} ms${hint}`);
  done(1);
}
const a = parseAnswer(answer.text);
const problems = [];
if (a.generation === null) problems.push('the answer names no generation');
if (a.kernelFnv !== fnv.KERNEL) problems.push(`KERNEL hash ${a.kernelFnv} on BitsOS, ${fnv.KERNEL} here`);
if (a.initrdFnv !== fnv.INITRD) problems.push(`INITRD hash ${a.initrdFnv} on BitsOS, ${fnv.INITRD} here`);
if (!opts['dry-run'] && (a.dry || !a.readBack)) problems.push('the answer does not say both files were read back from the stick');
if (opts['dry-run'] && !a.dry) problems.push('a dry run was asked, the answer is no dry run');
if (problems.length) {
  console.error(`install: ${problems.join('; ')}`);
  done(1);
}
const how = remote ? 'read from the uploads' : `fetched ${served.KERNEL} + ${served.INITRD} bytes`;
if (opts['dry-run']) {
  console.log(`install: dry run ok after ${ms} ms: generation ${a.generation} would be installed (${how}; both hashes equal the local files); nothing was written`);
  done(0);
}
console.log(`install: generation ${a.generation} installed (NEW) after ${ms} ms (${how}; KERNEL ${kernel.length} bytes, INITRD ${initrd.length} bytes, both hashes equal the local files, read back from the stick); next boot starts: ${a.next ?? '?'}`);
if (!opts.reboot) done(0);
if (server) { server.close(); server = null; }

// ---- --reboot: the new generation's one try and its confirmation ----
const r = await session.call('reboot', {}).catch((e) => { if (e instanceof McpError) return { isError: false, text: `(the answer was lost: ${e.message})` }; throw e; });
console.log(r.text.trim().split('\n').map((l) => `| ${l}`).join('\n'));
if (r.isError) fail('reboot refused');
const tReboot = Date.now();
let back = null;
while (!back && Date.now() - tReboot < backMs) {
  await sleep(1000);
  try {
    const s = await connect(host, { token, ms: 3000 });
    const line = await startLine(s);
    if (line !== before) back = { s, line };
  } catch (e) {
    if (!(e instanceof McpError)) throw e;
    if (e.kind === 'token') fail(`the new start refuses the token: ${e.message}`);
  }
}
if (!back) fail(`BitsOS did not answer MCP from a new start within ${backMs / 1000} s after the reboot (the new kernel may hang: the boot after its one try starts the fallback)`);
console.log(`reboot: MCP back after ${((Date.now() - tReboot) / 1000).toFixed(1)} s: ${back.line}`);
const gm = /generation (\d+)/.exec(back.line);
if (!gm || Number(gm[1]) !== a.generation) fail(`the new start runs generation ${gm?.[1] ?? '?'}, not ${a.generation}: bitsboot started another one (see log '{"filter":"bitsboot"}')`);
// The proof: MCP answers over boot_confirm_s with no pause over boot_confirm_s/4 (at least 6 s, at most 30 s;
// the kernel samples its answer counter every 2 s): a reload_info every 2 s keeps every pause well below that.
// reload_info answers that lack the confirmation line in a row before giving up: RB's text changed (or the kernel
// has no boot confirmation), and waiting the whole --confirm-s would only hide that.
const NO_LINE_MAX = 5;
const tProof = Date.now();
let last = '', lastAt = 0, noLine = 0;
for (;;) {
  let info;
  try {
    info = await back.s.call('reload_info', {});
  } catch (e) {
    if (!(e instanceof McpError)) throw e;
    back.s = await connect(host, { token, ms: 3000 }).catch(() => back.s);
    info = null;
  }
  const m = info && /boot confirmation \(RB-6\): ([^\n]*)/.exec(info.text);
  noLine = info && !m ? noLine + 1 : 0;
  if (noLine >= NO_LINE_MAX) {
    console.log(info.text.trim().split('\n').map((l) => `| ${l}`).join('\n'));
    fail(`reload_info has no "boot confirmation (RB-6): ..." line in ${NO_LINE_MAX} answers in a row (RB's text changed, or this kernel has no boot confirmation): cannot tell whether generation ${a.generation} confirms`);
  }
  // One line per state, and while waiting one every 10 s.
  const kind = m ? m[1].replace(/\d+ s of/, 'N s of') : '';
  if (m && (kind !== last || Date.now() - lastAt >= 10000)) { console.log(`confirm: ${m[1]}`); last = kind; lastAt = Date.now(); }
  if (m && m[1].startsWith(`generation ${a.generation} confirmed (OK)`)) break;
  if (m && /could not be confirmed|was replaced|nothing to confirm/.test(m[1])) fail(`generation ${a.generation} was not confirmed: ${m[1]}`);
  if (Date.now() - tProof > confirmMs) fail(`generation ${a.generation} not confirmed within ${confirmMs / 1000} s`);
  await sleep(2000);
}
console.log(`install: generation ${a.generation} runs and confirmed itself (OK) ${((Date.now() - tProof) / 1000).toFixed(0)} s after MCP came back`);
process.exit(0);
