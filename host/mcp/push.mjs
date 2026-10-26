// push.mjs (V2, MC-push): a package tree (\BITSOS2\<name>\, e.g. cobalt or bin) onto the running BitsOS's boot
// stick over MCP, without plugging the stick over. Only changed files travel; a half-pushed package never
// replaces a working one; an interrupted push is finished by running the same command again.
//
//   node push.mjs <BitsOS host[:port]> --tree <dir> --bind <LAN address> [--name <package>] [--home] [--me <address>]
//        [--port <8770-8799>] [--via fetch|write] [--dry-run] [--timeout-ms N] [--wait-ms N] [--token-file <path>]
//   node push.mjs <BitsOS host[:port]> --file <local file> --to /boot/BITSOS2/<package>/<path> --bind ... (the same options)
//
// --tree: the package as `cargo xtask image` or the YT stage script leaves it, e.g.
// target/xtask/esp-ktest/BITSOS2/cobalt; --name: the folder below \BITSOS2\ (default: the tree's own name; never
// SYS, CONFIG.TXT, STICK.ID, home, EFI or a hidden name). --bind/--me/--port: the HTTP server the kernel fetches
// from, as install.mjs has them (GET/HEAD of <random dir>/<n> only, only while this runs; 127.0.0.1 + 10.0.2.2
// for QEMU's slirp). --via write: no HTTP server, the files go up with write_file in 128 KiB pieces (slower, for
// when no port is open). --dry-run: compare and print the plan, change nothing.
// --home (MC-push2): the tree goes into the user's home \BITSOS2\home\ (--name home, e.g. Georg's Cobalt profile
// unpacked from cobalt_files.tar): files are added and replaced, never removed (the home holds more than one
// tree); BitsOS gives them the user's owner (uid/gid 1000, 0700/0600) as they move in.
// --file/--to (MC-push2): one file to one place (e.g. --to /boot/BITSOS2/bin/cobalt), staged and committed the same
// way through \BITSOS2\.push-<package>\; nothing else in that package changes. Below home only with --home.
//
// The steps, each checked on the BitsOS side:
//  1. an interrupted commit (\BITSOS2\.push-<name>\PLAN.json present) is finished first (move_tree is idempotent);
//  2. `manifest` of \BITSOS2\<name>\ and of the staging tree \BITSOS2\.push-<name>\files\ (SHA-256 per file,
//     computed by BitsOS) against the local tree: unchanged files stay, staged files that are right are kept
//     (resume), everything else in the staging tree is deleted (an older source, a cut fetch's temp file);
//  3. the changed files go into the staging tree (fetch or write_file);
//  4. `manifest` of the staging tree must equal the plan exactly (else exit 1, the next run resumes);
//  5. PLAN.json (the paths to remove) is written, then `move_tree` moves every staged file over its place (one
//     rename each), removes what the package no longer has and writes everything back; the staging tree goes;
//  6. `manifest` of the package must equal the local tree.
// Exit code 0: the stick holds the tree (or the dry run ran); 1: refused, failed or a hash differs; 2: bad
// arguments or BitsOS cannot be asked; 3: stopped by BITSOS_PUSH_STOP_AFTER=<files> or BITSOS_PUSH_STOP_AT_COMMIT=1
// (tests only). BITSOS_PUSH_PORTS=<lo>-<hi> replaces the port range, BITSOS_PUSH_CORRUPT=<rel> serves that file
// with its first byte flipped (the lane's tests only).
// --timeout-ms: each call's timeout_ms on BitsOS (default 10 min; a fetch gets at least 60 s + 1 s per MB). A call
// past it goes on there: push waits for it with `output` (up to --wait-ms, default 2 h), never starts it twice.
import { randomBytes } from 'node:crypto';
import { createReadStream, existsSync, readFileSync, statSync } from 'node:fs';
import { createServer } from 'node:http';
import { basename, resolve } from 'node:path';
import { connect, McpError, readToken, upload } from './mcp-call.mjs';
import { compare, FAT_MAX, fileTarget, isHome, journalProblem, makePlan, nameProblem, scanTree, sha256File } from './push-plan.mjs';

let PORT_LO = 8770, PORT_HI = 8799;
const fail = (why, code = 1) => { console.error(`push: ${why}`); process.exit(code); };
if (process.env.BITSOS_PUSH_PORTS) {
  const m = /^(\d+)-(\d+)$/.exec(process.env.BITSOS_PUSH_PORTS);
  if (!m || Number(m[1]) < 1024 || Number(m[2]) > 65535 || Number(m[1]) > Number(m[2])) fail(`BITSOS_PUSH_PORTS=${process.env.BITSOS_PUSH_PORTS}: want <lo>-<hi> within 1024-65535`, 2);
  [PORT_LO, PORT_HI] = [Number(m[1]), Number(m[2])];
}
const VALUE = ['--tree', '--file', '--to', '--name', '--bind', '--me', '--port', '--via', '--timeout-ms', '--wait-ms', '--token-file', '--root'];
const SWITCH = ['--dry-run', '--home'];
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
const USAGE = 'usage: node push.mjs <host[:port]> (--tree DIR [--name N] [--home] | --file F --to PATH [--home]) --bind A [--me A] [--port 8770-8799] [--via fetch|write] [--dry-run] [--timeout-ms N] [--wait-ms N] [--token-file F]';
const [host] = rest;
const fileMode = opts.file !== undefined;
if (!host || rest.length > 1 || fileMode === (opts.tree !== undefined)) fail(USAGE, 2);
if (fileMode && (opts.to === undefined || opts.name !== undefined)) fail(`--file needs --to and takes no --name\n${USAGE}`, 2);
if (!fileMode && opts.to !== undefined) fail(`--to goes with --file\n${USAGE}`, 2);
const via = opts.via ?? 'fetch';
if (!['fetch', 'write'].includes(via)) fail(`--via ${via}: fetch or write`, 2);
// --root (tests on tmpfs only): the folder that holds the packages.
const root = opts.root ?? '/boot/BITSOS2';
if (root !== '/boot/BITSOS2' && !/^\/tmp\/[A-Za-z0-9._-]+$/.test(root)) fail(`--root ${root}: /boot/BITSOS2, or /tmp/<name> for tests`, 2);
const allowHome = opts.home === true;
let tree, name, target, only = null;
if (fileMode) {
  tree = resolve(opts.file);
  if (!existsSync(tree) || !statSync(tree).isFile()) fail(`--file ${opts.file} is no regular file`, 2);
  if (statSync(tree).size > FAT_MAX) fail(`--file ${opts.file}: FAT holds less than 4 GiB per file`, 2);
  const ft = fileTarget(opts.to, root, { allowHome });
  if (ft.problem) fail(ft.problem, 2);
  [name, target, only] = [ft.pkg, ft.dir, ft.name];
} else {
  tree = resolve(opts.tree);
  if (!existsSync(tree) || !statSync(tree).isDirectory()) fail(`--tree ${opts.tree} is no directory`, 2);
  name = opts.name ?? basename(tree);
  const bad = nameProblem(name, { allowHome });
  if (bad) fail(bad, 2);
  target = `${root}/${name}`;
}
// The home by any spelling FAT folds to it is "home" here (one staging tree, one target).
const home = isHome(name);
if (home) name = 'home';
if (allowHome && !home) fail(`--home is only for \\BITSOS2\\home (--name home, or --to ${root}/home/...)`, 2);
if (home) target = fileMode ? `${root}/home${target.slice(root.length + 1).replace(/^[^/]*/, '')}` : `${root}/home`;
// --home and --file never remove anything (the home holds more than this tree; a file push touches one file).
const keep = home || fileMode;
const staging = `${root}/.push-${name}`, stagedFiles = `${staging}/files`;
const label = fileMode ? `${target}/${only}` : name;
const IPV4 = /^(\d{1,3})\.(\d{1,3})\.(\d{1,3})\.(\d{1,3})$/;
function privateIpv4(a) {
  const m = IPV4.exec(a);
  if (!m || m.slice(1).some((x) => Number(x) > 255)) return false;
  const [x, y] = [Number(m[1]), Number(m[2])];
  return x === 10 || x === 127 || (x === 172 && y >= 16 && y <= 31) || (x === 192 && y === 168) || (x === 169 && y === 254);
}
const bind = opts.bind;
if (via === 'fetch' && !opts['dry-run']) {
  if (!bind) fail(`--bind <this machine's LAN address> is missing (the server listens only there)\n${USAGE}`, 2);
  if (bind === '0.0.0.0') fail('--bind 0.0.0.0 would serve the package on every network of this machine; give its LAN address', 2);
  if (!privateIpv4(bind)) fail(`--bind ${bind}: an IPv4 address of this machine on a private network (BitsOS fetches only from those)`, 2);
}
const me = opts.me ?? (bind?.startsWith('127.') ? '10.0.2.2' : bind);
if (via === 'fetch' && me && !privateIpv4(me)) fail(`--me ${me}: BitsOS fetches only from private IPv4 addresses`, 2);
let port = opts.port === undefined ? null : Number(opts.port);
if (port !== null && !(Number.isInteger(port) && port >= PORT_LO && port <= PORT_HI)) fail(`--port ${opts.port}: the push server uses ${PORT_LO}-${PORT_HI}`, 2);
const timeoutMs = Number(opts['timeout-ms'] ?? 600000);
if (!(Number.isInteger(timeoutMs) && timeoutMs >= 1)) fail(`--timeout-ms ${opts['timeout-ms']}: a whole number of milliseconds >= 1`, 2);
const waitMs = Number(opts['wait-ms'] ?? 7200000);
if (!(Number.isInteger(waitMs) && waitMs >= 0)) fail(`--wait-ms ${opts['wait-ms']}: a whole number of milliseconds`, 2);
const corrupt = process.env.BITSOS_PUSH_CORRUPT ?? null;
const stopAfter = process.env.BITSOS_PUSH_STOP_AFTER ? Number(process.env.BITSOS_PUSH_STOP_AFTER) : null;

// ---- the local tree (--file: a tree of that one file, named as on the stick) ----
let local;
if (fileMode) local = { files: [{ rel: only, abs: tree, size: statSync(tree).size, sha256: await sha256File(tree) }], dirs: [] };
else try { local = await scanTree(tree); } catch (e) { fail(`${tree} cannot go onto the stick:\n${e.message}`, 2); }
const localBytes = local.files.reduce((n, f) => n + f.size, 0);
console.log(`${fileMode ? 'file' : 'tree'}: ${tree}: ${local.files.length} files, ${(localBytes / 1e6).toFixed(1)} MB -> ${fileMode ? label : target}${home ? ' (the user\'s home: nothing there is removed)' : ''}`);

let token;
try { token = readToken(opts['token-file']); } catch (e) { fail(e.message, 2); }
let session;
try { session = await connect(host, { token }); } catch (e) {
  if (!(e instanceof McpError)) throw e;
  fail(`${e.message}: is BitsOS running with its MCP server (CONFIG.TXT token=...)?`, 2);
}
console.log(`BitsOS answers at ${session.url}: ${(await session.call('version', {})).text.split('\n')[1] ?? ''}`);

// A tool call that must work; its answer text (JSON parsed when asked).
async function must(tool, args, { json = false, ms = timeoutMs } = {}) {
  let r;
  try { r = await session.call(tool, { ...args, timeout_ms: ms }); } catch (e) {
    if (!(e instanceof McpError)) throw e;
    fail(`${tool}: ${e.message}${/-32601|unknown tool/i.test(e.message) ? ' (this kernel has no MC-push tools: Reload into a newer one first)' : ''}`, e.kind === 'answer' ? 1 : 2);
  }
  // "Still running" comes as an isError answer; it is no failure: the call goes on.
  if (r.isError && /^Still running/.test(r.text)) r = await waitCall(tool, r.text);
  if (r.isError) fail(`${tool} ${JSON.stringify(args).slice(0, 200)}: ${r.text}`);
  return json ? JSON.parse(r.text) : r.text;
}
// A call past its timeout_ms keeps running on BitsOS: wait for it with `output` (never start the same work
// a second time beside it; move_tree would answer EBUSY anyway). -> the call's own answer { isError, text }.
// Bounded: --wait-ms (default 2 h) in all, then exit 1 with the call still running.
async function waitCall(tool, still) {
  const m = /\(call (\d+)\)/.exec(still);
  if (!m) fail(`${tool}: "Still running" without a call number: ${still}`);
  const call = Number(m[1]);
  console.log(`wait: ${tool} is still running on BitsOS (call ${call}); waiting for it with output`);
  const until = Date.now() + waitMs;
  for (let polls = 0; ; polls++) {
    await new Promise((done) => setTimeout(done, Math.min(5000, 500 * 2 ** polls)));
    let o;
    try { o = await session.call('output', { call }); } catch (e) {
      if (!(e instanceof McpError)) throw e;
      fail(`output ${call} (waiting for ${tool}): ${e.message}`, e.kind === 'answer' ? 1 : 2);
    }
    const [head = '', ...rest] = o.texts ?? [o.text];
    const first = head.split('\n')[0];
    for (const line of head.split('\n').slice(1)) if (line) console.log(`  ${line}`);
    if (o.isError) return { isError: true, text: o.text };
    if (/: finished after /.test(first)) return { isError: false, text: rest.join('') };
    if (!/: running/.test(first)) fail(`output ${call} (waiting for ${tool}): ${o.text}`);
    if (Date.now() > until) fail(`${tool} (call ${call}) still runs after --wait-ms ${waitMs}: run push again once it ended (output {"call":${call}}; meanwhile move_tree on these trees answers EBUSY)`);
  }
}
// Every entry of a tree (all pages); [] when it is missing. only: just that relative path's entry.
async function manifest(path, hash = 'sha256', only = null) {
  const out = [];
  let after;
  for (;;) {
    const j = await must('manifest', { path, hash, missing_ok: true, ...(only ? { only } : {}), ...(after ? { after } : {}) }, { json: true });
    out.push(...j.entries);
    if (!j.next) return { exists: j.exists, entries: out, ms: j.ms ?? 0 };
    after = j.next;
  }
}

// ---- 1. an interrupted commit ----
const st0 = await manifest(staging, 'none');
if (st0.entries.some((e) => e.path === 'PLAN.json')) {
  const plan = JSON.parse(Buffer.from((await must('read_file', { path: `${staging}/PLAN.json` }, { json: true })).data, 'base64').toString('utf8'));
  // Where that commit went (a --file push: a directory of the package; older journals: the package).
  const pkgDir = `${root}/${name}`;
  const jp = journalProblem(plan, root, name);
  if (jp) fail(`${staging}/PLAN.json: not a journal of ${pkgDir}: ${jp} (${JSON.stringify(plan).slice(0, 200)})`);
  const to = plan.to ?? pkgDir;
  if (opts['dry-run']) console.log(`plan: an interrupted commit to ${to} waits in ${staging} (${plan.remove.length} removals); a real run finishes it first`);
  else {
    console.log(`commit: finishing the interrupted commit in ${staging}`);
    console.log(`| ${await must('move_tree', { from: stagedFiles, to, remove: plan.remove })}`);
    await must('delete', { path: staging, recursive: true });
  }
}

// ---- 2. compare ----
const t0 = Date.now();
const remote = await manifest(target, 'sha256', only);
const notFile = remote.entries.find((e) => fileMode && e.type !== 'file');
if (notFile) fail(`${label} is a ${notFile.type} on BitsOS: --file replaces files only`);
const staged = opts['dry-run'] && st0.entries.some((e) => e.path === 'PLAN.json') ? { entries: [] } : await manifest(stagedFiles);
const plan = makePlan(local, remote.entries, staged.entries, { keep });
console.log(`compare: ${remote.entries.filter((e) => e.type === 'file').length} files on BitsOS hashed in ${Date.now() - t0} ms; `
  + `${plan.same} unchanged, ${plan.upload.length} to upload (${(plan.bytes / 1e6).toFixed(1)} MB), ${plan.reuse.length} staged already, `
  + `${plan.remove.length} to remove, ${plan.stale.length} stale in the staging tree${home ? `, ${plan.kept} only on BitsOS kept` : ''}`);
for (const f of plan.upload) console.log(`  upload ${f.rel} (${f.size} bytes)`);
for (const r of plan.remove) console.log(`  remove ${r}`);
if (opts['dry-run']) {
  console.log(`push: dry run: ${plan.upload.length + plan.reuse.length} files would change, ${plan.remove.length} would be removed; nothing was written`);
  process.exit(0);
}
if (plan.upload.length + plan.reuse.length + plan.remove.length === 0) {
  if (st0.exists) await session.call('delete', { path: staging, recursive: true }).catch(() => {});
  console.log(`push: ${label} is up to date (${local.files.length} files, SHA-256 equal on BitsOS)`);
  process.exit(0);
}
for (const s of plan.stale) await must('delete', { path: `${stagedFiles}/${s}`, recursive: true });

// ---- 3. upload ----
const dir = `/push-${randomBytes(6).toString('hex')}/`;
let server = null;
async function serve() {
  const files = new Map(plan.upload.map((f, i) => [`${dir}${i}`, f]));
  server = createServer((req, res) => {
    const f = files.get(req.url);
    if (!f || (req.method !== 'GET' && req.method !== 'HEAD')) {
      console.log(`server: ${req.method} ${req.url} from ${req.socket.remoteAddress}: 404`);
      return res.writeHead(404, { 'Content-Length': 0 }).end();
    }
    res.writeHead(200, { 'Content-Type': 'application/octet-stream', 'Content-Length': f.size });
    if (req.method === 'HEAD') return res.end();
    if (f.rel === corrupt) {
      const b = readFileSync(f.abs);
      if (b.length) b[0] ^= 0xff;
      console.log(`server: ${f.rel} served with its first byte flipped (BITSOS_PUSH_CORRUPT, tests)`);
      return res.end(b);
    }
    createReadStream(f.abs).pipe(res);
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
    if (opts.port !== undefined) fail(`port ${p} is taken on ${bind}: another --port in ${PORT_LO}-${PORT_HI}, or leave it out`, 2);
  }
  fail(`every port ${PORT_LO}-${PORT_HI} is taken on ${bind}`, 2);
}
if (via === 'fetch' && plan.upload.length) {
  await serve();
  console.log(`serving ${plan.upload.length} files as http://${me}:${port}${dir}<n> (listening on ${bind}:${port} only)`);
}
const tUp = Date.now();
let sent = 0, n = 0;
for (const [i, f] of plan.upload.entries()) {
  const path = `${stagedFiles}/${f.rel}`;
  // Per file: 60 s plus 1 s per MB (the LAN does 10+ MB/s, QEMU's slirp less), never less than --timeout-ms asks.
  const ms = Math.max(60000 + Math.ceil(f.size / 1000), timeoutMs);
  const t = Date.now();
  if (via === 'fetch') {
    const text = await must('fetch', { url: `http://${me}:${port}${dir}${i}`, path, parents: true, max_bytes: f.size }, { ms });
    const got = /: (\d+) bytes in/.exec(text);
    if (!got || Number(got[1]) !== f.size) fail(`fetch ${f.rel}: ${text}`);
  } else {
    try { await upload(session, readFileSync(f.abs), path, { parents: true }); } catch (e) {
      if (!(e instanceof McpError)) throw e;
      fail(`write_file ${f.rel}: ${e.message}`);
    }
  }
  sent += f.size;
  n++;
  if (f.size >= 4 << 20) console.log(`  ${f.rel}: ${(f.size / 1e6).toFixed(1)} MB in ${Date.now() - t} ms`);
  if (stopAfter !== null && n >= stopAfter) {
    console.log(`push: stopped after ${n} files (BITSOS_PUSH_STOP_AFTER, tests): run again to resume`);
    process.exit(3);
  }
}
if (server) server.close();
const upMs = Date.now() - tUp;
if (plan.upload.length) console.log(`upload: ${n} files, ${(sent / 1e6).toFixed(1)} MB in ${upMs} ms (${(sent / 1e3 / Math.max(upMs, 1)).toFixed(1)} MB/s, via ${via})`);

// ---- 4. verify the staging tree on BitsOS ----
const want = [...plan.upload, ...plan.reuse].sort((a, b) => Buffer.compare(Buffer.from(a.rel), Buffer.from(b.rel)));
const check = await manifest(stagedFiles);
const diff = compare({ files: want }, check.entries);
if (diff.length) {
  for (const d of diff.slice(0, 20)) console.error(`  ${d}`);
  for (const e of check.entries) if (e.type === 'file' && diff.some((d) => d.startsWith(`${e.path}:`))) await must('delete', { path: `${stagedFiles}/${e.path}` });
  fail(`the staging tree differs from the plan in ${diff.length} files (deleted again): run the push again`);
}
console.log(`verify: ${want.length} staged files, SHA-256 computed by BitsOS equal to the local files`);

// ---- 5. commit ----
await must('write_file', { path: `${staging}/PLAN.json`, text: JSON.stringify({ name, to: target, remove: plan.remove, files: want.length }), parents: true });
if (process.env.BITSOS_PUSH_STOP_AT_COMMIT === '1') {
  console.log('push: stopped before the commit (BITSOS_PUSH_STOP_AT_COMMIT, tests): run again to finish it');
  process.exit(3);
}
const tC = Date.now();
console.log(`| ${await must('move_tree', { from: stagedFiles, to: target, remove: plan.remove })}`);
await must('delete', { path: staging, recursive: true });
console.log(`commit: ${want.length} files in place, ${plan.remove.length} removed, staging tree gone in ${Date.now() - tC} ms`);

// ---- 6. the package as BitsOS has it ----
const after = await manifest(target, 'sha256', only);
const left = compare(local, after.entries.filter((e) => e.type === 'file'), { extraOk: home });
if (left.length) {
  for (const d of left.slice(0, 20)) console.error(`  ${d}`);
  fail(`${fileMode ? label : target} differs from ${tree} in ${left.length} files after the commit`);
}
const what = fileMode ? `${label} equals the file` : `${target} ${home ? 'holds' : 'equals'} the tree`;
console.log(`push: ${label}: ${n} files uploaded (${(sent / 1e6).toFixed(1)} MB), ${plan.reuse.length} from an earlier run, ${plan.remove.length} removed, ${plan.same} unchanged; ${what} (SHA-256 of all ${local.files.length} files on BitsOS)${home ? `; ${plan.kept} files only on BitsOS kept` : ''}`);
process.exit(0);
