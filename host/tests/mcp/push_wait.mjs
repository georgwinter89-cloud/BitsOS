// Protocol test of host/mcp/push.mjs's waiting (MC-push fixer) against a fake BitsOS MCP server (no QEMU, a few
// seconds): a call past its timeout_ms answers "Still running ... (call n)" as an isError answer; push.mjs must
// then wait for that call with `output {"call": n}` (printing its progress), take its result from there, and
// never start the same call a second time beside it. Cases: a slow move_tree, a slow manifest (JSON result
// through output), a move_tree that fails after the wait (exit 1), and --wait-ms 0 with a call that never ends.
//
//   node host/tests/mcp/push_wait.mjs        -> "push-wait: N of N ok", exit 0 (1 on the first failure)
import { createHash } from 'node:crypto';
import { spawn } from 'node:child_process';
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from 'node:fs';
import { createServer, get } from 'node:http';
import { tmpdir } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..', '..', '..');
const PUSH = join(ROOT, 'host', 'mcp', 'push.mjs');
const TOKEN = 'push-wait-token-81d0';
// push.mjs's HTTP server in these tests: the MC range (40-push.sh uses LO+480..489).
const LO = Number(process.env.BITSOS_PORT_LO ?? 25000) + 490, HI = LO + 9;

const tmp = mkdtempSync(join(tmpdir(), 'push-wait-'));
const tree = join(tmp, 'pkg');
mkdirSync(join(tree, 'lib'), { recursive: true });
writeFileSync(join(tree, 'lib', 'a.so'), 'library bytes');

const sha = (b) => createHash('sha256').update(b).digest('hex');
const fake = {};
function reset(mode) {
  Object.assign(fake, { mode, calls: [], files: new Map(), pending: null, polls: 0, slowManifestDone: false });
}
const ID = 7;
const still = (tool) => ({ isError: true, parts: [`Still running: ${tool} (call ${ID}) did not return before its deadline passed. output {"call":${ID}} shows its progress and, once it ends, its result; stop {"call":${ID}} asks it to stop`] });
const text = (t, isError = false) => ({ isError, parts: [t] });

function manifest(path) {
  const entries = [...fake.files.entries()].filter(([p]) => p.startsWith(`${path}/`)).map(([p, b]) => ({ path: p.slice(path.length + 1), type: 'file', size: b.length, sha256: sha(b) }))
    .sort((a, b) => Buffer.compare(Buffer.from(a.path), Buffer.from(b.path)));
  return JSON.stringify({ path, exists: entries.length > 0, entries, next: null });
}
function moveTree({ from, to, remove = [] }) {
  for (const [p, b] of [...fake.files]) if (p.startsWith(`${from}/`)) { fake.files.delete(p); fake.files.set(`${to}/${p.slice(from.length + 1)}`, b); }
  for (const r of remove) fake.files.delete(`${to}/${r}`);
  return 'move_tree: merged: 1 files and 0 directories moved (0 replaced), 0 removed (0 already gone), 0 empty directories pruned; written back (3 flushed) in 9 ms';
}
const fetchUrl = (url) => new Promise((done, failed) => get(url, (res) => {
  const parts = [];
  res.on('data', (d) => parts.push(d));
  res.on('end', () => done(Buffer.concat(parts)));
}).on('error', failed));

async function tool(name, a) {
  fake.calls.push([name, a]);
  switch (name) {
    case 'version': return text('BitsOS V2\nfake push-wait');
    case 'manifest':
      if (fake.mode === 'slow-manifest' && a.path.endsWith('/pkg') && !fake.slowManifestDone) {
        fake.slowManifestDone = true;
        fake.pending = { tool: 'manifest', finish: () => ({ isError: false, parts: [`call ${ID} manifest: finished after 1.1 s`, manifest(a.path)] }) };
        return still('manifest');
      }
      return text(manifest(a.path));
    case 'fetch': {
      const b = await fetchUrl(a.url);
      fake.files.set(a.path, b);
      return text(`fetch: ${a.url} -> ${a.path}: ${b.length} bytes in 1 ms (fnv 0x0)`);
    }
    case 'write_file': fake.files.set(a.path, Buffer.from(a.text ?? '')); return text(`write_file: ${a.path}`);
    case 'delete':
      for (const p of [...fake.files.keys()]) if (p === a.path || p.startsWith(`${a.path}/`)) fake.files.delete(p);
      return text(`delete: ${a.path}`);
    case 'move_tree':
      if (fake.mode === 'ok' || fake.mode === 'slow-manifest') return text(moveTree(a));
      if (fake.pending) return text('another move_tree is still working on it (EBUSY)', true);
      fake.pending = {
        tool: 'move_tree',
        finish: () => (fake.mode === 'move-fails'
          ? { isError: true, parts: [`call ${ID} move_tree: finished after 2.0 s\nresult: the write-back after the move failed for 1 files or file systems (the stick?)`] }
          : { isError: false, parts: [`call ${ID} move_tree: finished after 2.0 s`, moveTree(a)] }),
      };
      return still('move_tree');
    case 'output': {
      if (!fake.pending || a.call !== ID) return text(`no call ${a.call} here`, true);
      fake.polls++;
      if (fake.mode === 'never' || fake.polls < 3) return text(`call ${ID} ${fake.pending.tool}: running after ${fake.polls}.0 s\n${fake.pending.tool}: ${fake.polls} MiB so far`);
      const r = fake.pending.finish();
      fake.pending = null;
      return r;
    }
    default: return text(`unknown tool ${name}`, true);
  }
}

const mcp = createServer((req, res) => {
  const parts = [];
  req.on('data', (d) => parts.push(d));
  req.on('end', async () => {
    if (req.headers.authorization !== `Bearer ${TOKEN}`) return res.writeHead(401).end();
    const m = JSON.parse(Buffer.concat(parts).toString('utf8'));
    if (m.id === undefined) return res.writeHead(202).end();
    let result = {};
    if (m.method === 'initialize') result = { protocolVersion: '2025-06-18', serverInfo: { name: 'fake' }, capabilities: { tools: {} } };
    else if (m.method === 'tools/call') {
      const r = await tool(m.params.name, m.params.arguments ?? {});
      result = { content: r.parts.map((t) => ({ type: 'text', text: t })), isError: r.isError };
    }
    res.writeHead(200, { 'Content-Type': 'application/json' }).end(JSON.stringify({ jsonrpc: '2.0', id: m.id, result }));
  });
});
await new Promise((done) => mcp.listen(0, '127.0.0.1', done));
const HOST = `127.0.0.1:${mcp.address().port}`;

function run(extra = []) {
  return new Promise((done) => {
    const p = spawn(process.execPath, [PUSH, HOST, '--tree', tree, '--bind', '127.0.0.1', '--me', '127.0.0.1', '--root', '/tmp/pw', ...extra],
      { env: { ...process.env, BITSOS_TOKEN: TOKEN, BITSOS_PUSH_PORTS: `${LO}-${HI}` } });
    let out = '';
    p.stdout.on('data', (d) => { out += d; });
    p.stderr.on('data', (d) => { out += d; });
    p.on('close', (code) => done({ code, out }));
  });
}
const count = (name) => fake.calls.filter(([n]) => n === name).length;
const outputs = () => fake.calls.filter(([n]) => n === 'output').map(([, a]) => a.call);

let n = 0;
async function check(what, f) {
  try {
    await f();
    n++;
    console.log(`ok: ${what}`);
  } catch (e) {
    console.log(`FAIL: ${what}: ${e.message}`);
    mcp.close();
    rmSync(tmp, { recursive: true, force: true });
    process.exit(1);
  }
}
function need(cond, why, out = '') {
  if (!cond) throw new Error(`${why}${out ? `\n${out}` : ''}`);
}

await check('a slow move_tree: waited for with output, its progress shown, its result used, never called twice', async () => {
  reset('slow-move');
  const r = await run();
  need(r.code === 0, `exit ${r.code}`, r.out);
  need(/wait: move_tree is still running on BitsOS \(call 7\)/.test(r.out), 'no wait line', r.out);
  need(/move_tree: 2 MiB so far/.test(r.out), 'progress not shown', r.out);
  need(/\| move_tree: merged/.test(r.out), 'the result from output not printed', r.out);
  need(/push: pkg: 1 files uploaded/.test(r.out), 'no success line', r.out);
  need(count('move_tree') === 1, `move_tree called ${count('move_tree')} times`);
  need(outputs().length === 3 && outputs().every((c) => c === 7), `output calls ${JSON.stringify(outputs())}`);
  need(fake.files.has('/tmp/pw/pkg/lib/a.so'), 'the file is not in the package');
});

await check('a slow manifest: its JSON comes through output', async () => {
  reset('slow-manifest');
  const r = await run();
  need(r.code === 0, `exit ${r.code}`, r.out);
  need(/wait: manifest is still running on BitsOS \(call 7\)/.test(r.out), 'no wait line', r.out);
  need(/push: pkg: 1 files uploaded/.test(r.out), 'no success line', r.out);
});

await check('a move_tree that fails after the wait: exit 1 with its reason, not called again', async () => {
  reset('move-fails');
  const r = await run();
  need(r.code === 1, `exit ${r.code}`, r.out);
  need(/result: the write-back after the move failed/.test(r.out), 'the reason is missing', r.out);
  need(count('move_tree') === 1, `move_tree called ${count('move_tree')} times`);
  need(count('delete') === 0, 'the staging tree was deleted after a failed commit');
});

await check('--wait-ms 0 and a call that never ends: exit 1, the call left running, not started again', async () => {
  reset('never');
  const r = await run(['--wait-ms', '0']);
  need(r.code === 1, `exit ${r.code}`, r.out);
  need(/still runs after --wait-ms 0/.test(r.out), 'no wait-ms line', r.out);
  need(count('move_tree') === 1, `move_tree called ${count('move_tree')} times`);
});

mcp.close();
rmSync(tmp, { recursive: true, force: true });
console.log(`push-wait: ${n} of ${n} ok`);
process.exit(0);
