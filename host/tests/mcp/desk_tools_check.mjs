// desk_tools_check.mjs (MC-11): the desk's tools against a stand-in of the Mini-PC's service on this host, no QEMU:
// a mutual-TLS MCP server (node:https, a P-256 key made like bitsmcp's) that says "grants not loaded yet" to the
// first access_status calls, as bitsmcp does while bitsinstall has not reported after a start. Checks:
//   1. mcp-call.mjs --tls waits while the service says so, then the tool runs (one note on stderr);
//   2. waitGrants gives up after its limit and goes on; an error or any other answer ends the wait at once;
//   3. host/tests/mcp/mcp_tls.py (the Python helpers' TLS, lan_rate.py) calls with the client key and the pin, and
//      refuses a server whose key is not the pinned one before sending anything.
// Usage: node desk_tools_check.mjs <run dir> <port>; prints "ok: ..." lines, exit 1 on the first red.
import { spawnSync, spawn } from 'node:child_process';
import { createServer } from 'node:https';
import { mkdirSync, rmSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { clientKey, makeClient, writePin } from '../../mcp/mcp-keys.mjs';
import { NOT_LOADED, waitGrants } from '../../mcp/mcp-call.mjs';

const HERE = dirname(fileURLToPath(import.meta.url));
const [runDir, portText] = process.argv.slice(2);
const port = Number(portText);
if (!runDir || !port) { console.error('usage: node desk_tools_check.mjs <run dir> <port>'); process.exit(2); }
const red = (m) => { console.log(`RED: ${m}`); process.exit(1); };
const ok = (m) => console.log(`ok: ${m}`);
const TOKEN = 'mc11-desk-tools-token';

// 2. waitGrants on a scripted session (no network).
{
  const script = (answers) => {
    const calls = [];
    return { calls, session: { async call(name) { calls.push(name); const a = answers[Math.min(calls.length - 1, answers.length - 1)]; return { isError: a === 'error', text: a }; } } };
  };
  let slept = 0;
  const sleep = async () => { slept += 1; };
  let s = script([`${NOT_LOADED}: ...`, `${NOT_LOADED}: ...`, 'full access for this client until 2027-01-15 09:00']);
  let r = await waitGrants(s.session, 60000, sleep);
  if (s.calls.length !== 3 || slept !== 2 || !r.text.startsWith('full access')) red(`waitGrants: ${s.calls.length} calls, ${slept} sleeps: ${r.text}`);
  s = script(['no full access for this client: read tools only']);
  r = await waitGrants(s.session, 60000, sleep);
  if (s.calls.length !== 1) red('waitGrants waited on "no full access"');
  s = script(['error']);
  r = await waitGrants(s.session, 60000, sleep);
  if (s.calls.length !== 1 || !r.isError) red('waitGrants waited on an error');
  s = script([`${NOT_LOADED}: ...`]);
  slept = 0;
  r = await waitGrants(s.session, 0, sleep);
  if (s.calls.length !== 1 || slept !== 0 || !r.text.startsWith(NOT_LOADED)) red('waitGrants with no time left did not go on at once');
  ok('waitGrants: waits while the grants are not loaded, ends at any other answer or an error, gives up at its limit');
}

// The stand-in service.
rmSync(runDir, { recursive: true, force: true });
mkdirSync(runDir, { recursive: true });
const store = resolve(runDir, 'store');
const other = resolve(runDir, 'other');
const me = clientKey(store, { make: true });
const server = makeClient('bitsmcp stand-in');
const pem = (der) => `-----BEGIN CERTIFICATE-----\n${der.toString('base64').replace(/.{64}/g, '$&\n').replace(/\n?$/, '\n')}-----END CERTIFICATE-----\n`;
writePin(store, '127.0.0.1', server.der);
clientKey(other, { make: true });
writePin(other, '127.0.0.1', makeClient('someone else').der);

let notLoaded = 2;
const seen = [];
const srv = createServer({ key: server.keyPem, cert: pem(server.der), requestCert: true, rejectUnauthorized: false, minVersion: 'TLSv1.3' }, (req, res) => {
  const parts = [];
  req.on('data', (d) => parts.push(d));
  req.on('end', () => {
    const peer = req.socket.getPeerCertificate(true)?.raw;
    if (!peer || Buffer.compare(peer, me.der) !== 0 || req.headers.authorization !== `Bearer ${TOKEN}`) { res.writeHead(403).end(); return; }
    const m = JSON.parse(Buffer.concat(parts).toString('utf8'));
    seen.push(m.method === 'tools/call' ? m.params.name : m.method);
    const answer = (result) => { res.writeHead(200, { 'Content-Type': 'application/json' }).end(JSON.stringify({ jsonrpc: '2.0', id: m.id, result })); };
    if (m.method === 'initialize') return answer({ protocolVersion: '2025-06-18', serverInfo: { name: 'stand-in', version: '1' }, capabilities: {} });
    if (m.id === undefined) { res.writeHead(202).end(); return; }
    const text = (t) => answer({ content: [{ type: 'text', text: t }], isError: false });
    if (m.params.name === 'access_status') return text(notLoaded-- > 0 ? `${NOT_LOADED}: the installer has not handed the kernel its table` : 'full access for this client until 2027-01-15 09:00');
    return text(`pong ${m.params.name}`);
  });
});
await new Promise((done) => srv.listen(port, '127.0.0.1', done));
const run = (cmd, args) => new Promise((done) => {
  const p = spawn(cmd, args, { env: { ...process.env, BITSOS_TOKEN: TOKEN, BITSOS_MCP_STORE: '' } });
  let out = '', err = '';
  p.stdout.on('data', (d) => { out += d; });
  p.stderr.on('data', (d) => { err += d; });
  p.on('close', (code) => done({ code, out, err }));
});
try {
  // 1. mcp-call.mjs over TLS waits through the "not loaded" answers.
  let r = await run('node', [join(HERE, '..', '..', 'mcp', 'mcp-call.mjs'), `127.0.0.1:${port}`, 'hw_info', '{}', '--tls', store]);
  if (r.code !== 0 || !r.out.includes('pong hw_info')) red(`mcp-call.mjs --tls: exit ${r.code}: ${r.out}${r.err}`);
  if (!r.err.includes(`waiting: BitsOS says "${NOT_LOADED}"`)) red(`mcp-call.mjs said nothing while waiting: ${r.err}`);
  const asks = seen.filter((n) => n === 'access_status').length;
  if (asks !== 3 || seen.at(-1) !== 'hw_info') red(`the service saw ${seen.join(', ')}`);
  ok(`mcp-call.mjs --tls waited through ${asks - 1} "${NOT_LOADED}" answers, then ran the tool`);
  // 3. Python: the key store, the pin, and a refusal before anything is sent.
  seen.length = 0;
  r = await run('python3', ['-I', join(HERE, 'mcp_tls.py'), `127.0.0.1:${port}`, 'fetch', '{"url":"x"}', '--tls', store]);
  if (r.code !== 0 || !r.out.includes('pong fetch')) red(`mcp_tls.py: exit ${r.code}: ${r.out}${r.err}`);
  r = await run('python3', ['-I', join(HERE, 'mcp_tls.py'), `127.0.0.1:${port}`, 'fetch', '{}', '--tls', other]);
  if (r.code !== 2 || !r.err.includes('ANOTHER KEY')) red(`mcp_tls.py with another pin: exit ${r.code}: ${r.out}${r.err}`);
  if (seen.length !== 1) red(`the service saw ${seen.join(', ')} (a refused pin must send nothing)`);
  const auto = spawnSync('python3', ['-I', '-c', `import sys; sys.path.insert(0, ${JSON.stringify(HERE)}); import mcp_tls; print(mcp_tls.store_of("auto", "/x/y/token"))`], { env: { ...process.env, BITSOS_MCP_STORE: '' }, encoding: 'utf8' });
  if (auto.stdout.trim() !== '/x/y/bitsos-mcp') red(`mcp_tls.store_of auto: ${auto.stdout}${auto.stderr}`);
  ok('mcp_tls.py: mutual TLS with the key store and the pin; another key is refused before anything is sent; --tls auto finds the store next to the token');
} finally {
  srv.close();
}
