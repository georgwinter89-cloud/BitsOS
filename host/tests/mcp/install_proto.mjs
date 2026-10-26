// Protocol test of host/mcp/install.mjs (MC-4b) against a fake BitsOS MCP server (no QEMU, a few seconds):
// what install.mjs sends (initialize, tools/call install_kernel with exactly the arguments asked for, write_file /
// stat / delete for --upload, reboot and reload_info for --reboot), what its HTTP server serves (only the two
// files below a random directory, only on the --bind address, a port in 8770-8799), how it reads the answer
// (generation, hashes against the local files, the read-back sentence; the answer text built like RB-6's
// `install_kernel` answer) and its exit codes (0 / 1 refused, failed or a hash differs / 2 arguments or BitsOS
// unreachable). The fake fetches the bundle the way the kernel does (10.0.2.2 = this machine, QEMU slirp style).
//
//   node host/tests/mcp/install_proto.mjs        -> "install-proto: N of N ok", exit 0 (1 on the first failure)
import { spawn } from 'node:child_process';
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from 'node:fs';
import { createServer, request } from 'node:http';
import { createServer as tcpServer, connect as tcpConnect } from 'node:net';
import { tmpdir } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { fnv1a64 } from '../../mcp/mcp-call.mjs';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..', '..', '..');
const INSTALL = join(ROOT, 'host', 'mcp', 'install.mjs');
const TOKEN = 'install-proto-token-5c2e';
// The install server's ports in these tests: the MC range (host/tests convention); one check uses the default.
const LO = Number(process.env.BITSOS_PORT_LO ?? 25000) + 450, HI = LO + 19;

// ---- a bundle and a scratch folder ----
const tmp = mkdtempSync(join(process.env.BITSOS_RUN_DIR ?? tmpdir(), 'mc4b-proto-'));
const kernel = Buffer.concat([Buffer.from('\x7fELF\x02\x01\x01', 'latin1'), Buffer.alloc(57), Buffer.from('BitsOS V2 kernel: entry'), Buffer.alloc(300000, 0x5a)]);
const initrd = Buffer.concat([Buffer.from('070701'), Buffer.alloc(5000, 0x41)]);
mkdirSync(join(tmp, 'b'));
writeFileSync(join(tmp, 'b', 'KERNEL'), kernel);
writeFileSync(join(tmp, 'b', 'INITRD'), initrd);
writeFileSync(join(tmp, 'junk'), 'no elf');
writeFileSync(join(tmp, 'CONFIG.TXT'), 'token=x\n# new line\n');

// ---- the fake BitsOS ----
const fake = {
  mode: 'ok',          // ok | refuse | badhash | unknown | fallback | noline
  calls: [],           // [name, arguments]
  fetched: [],         // [path, status, bytes]
  files: new Map(),    // write_file store
  boot: 1, generation: 1, newest: 1, polls: 0, status: 'nothing to confirm: generation 1 started as NEWEST_OK',
};
const mib = (b) => `${Math.floor(b / 1048576)}.${Math.floor(((b % 1048576) * 10) / 1048576)} MiB`;
function fetchFile(url, method = 'GET') {
  return new Promise((done) => {
    // The raw path (no URL normalisation: a ".." must reach the server as it is); 10.0.2.2 = this machine.
    const m = /^http:\/\/([\d.]+):(\d+)(\/.*)$/.exec(url);
    const host = m[1] === '10.0.2.2' ? '127.0.0.1' : m[1];
    const req = request({ host, port: Number(m[2]), path: m[3], method, agent: false }, (res) => {
      const parts = [];
      res.on('data', (d) => parts.push(d));
      res.on('end', () => { const b = Buffer.concat(parts); fake.fetched.push([`${method === 'GET' ? '' : `${method} `}${m[3]}`, res.statusCode, b.length]); done({ status: res.statusCode, body: b }); });
    });
    req.on('error', (e) => done({ status: 0, body: Buffer.alloc(0), error: e.code }));
    req.end();
  });
}
// RB-6's answer (crates/kexec/src/install/tool.rs `answer`), for a stick with generation 1 (OK).
function installAnswer(k, i, dry, n) {
  let t = dry ? `install_kernel (dry run, nothing written): would install generation ${n} (NEW)` : `installed generation ${n} (NEW): \\BITSOS2\\SYS\\${n}\\`;
  let kf = fnv1a64(k);
  if (fake.mode === 'badhash') kf = `0x${(BigInt(kf) ^ 1n).toString(16).padStart(16, '0')}`;
  t += `\nKERNEL ${mib(k.length)} (fnv ${kf}, entry 0xffffffff80000040, image ${k.length} bytes), INITRD ${mib(i.length)} (fnv ${fnv1a64(i)}, 1 entries)${dry ? '' : '; both read back from the stick and equal'}`;
  t += '\nfallback (KERNEL.OLD): generation 1';
  if (!dry) t += `\nnext boot starts: generation ${n} (NEWEST_TRY: newest, new: TRY is written, its one try)\nThe new generation starts at the next reboot as its one try; ...`;
  return t;
}
async function tool(name, args) {
  fake.calls.push([name, args]);
  switch (name) {
    case 'version': return { text: `BitsOS V2 kernel 0.1.0\nstart: reload 0, generation ${fake.generation}, boot id ${String(fake.boot).padStart(32, '0')}` };
    case 'install_kernel': {
      if (fake.mode === 'unknown') return { rpc: { code: -32602, message: 'Unknown tool: install_kernel' } };
      if (fake.mode === 'refuse') return { isError: true, text: 'Refused: CONFIG.TXT: the line usb_msd_write=on is Georg\'s to set (R8a) (nothing on the stick changed)' };
      let k, i;
      if (args.kernel_path) {
        k = fake.files.get(args.kernel_path); i = fake.files.get(args.initrd_path);
        if (!k || !i) return { isError: true, text: 'ERROR: no such file (nothing on the stick changed)' };
      } else {
        if (fake.probe) await fake.probe(args.url);
        // Probes the server's rules first: the directory itself, a path with .., another name.
        for (const p of ['', 'KERNEL/../INITRD', 'kernel']) await fetchFile(`${args.url}${p}`);
        await fetchFile(`${args.url}KERNEL`, 'POST');
        const kr = await fetchFile(`${args.url}KERNEL`), ir = await fetchFile(`${args.url}INITRD`);
        if (kr.status !== 200 || ir.status !== 200) return { isError: true, text: `ERROR: kernel: ${args.url}KERNEL answered HTTP ${kr.status} (nothing on the stick changed)` };
        [k, i] = [kr.body, ir.body];
      }
      const n = fake.newest + 1;
      if (!args.dry_run) fake.newest = n;
      return { text: installAnswer(k, i, args.dry_run === true, n) };
    }
    case 'write_file': {
      const d = Buffer.from(args.data, 'base64');
      const old = fake.files.get(args.path) ?? Buffer.alloc(0);
      fake.files.set(args.path, args.offset === undefined ? d : Buffer.concat([old.subarray(0, args.offset), d]));
      return { text: `write_file ${args.path}: ${d.length} bytes written` };
    }
    case 'stat': {
      const f = fake.files.get(args.path);
      return f ? { text: JSON.stringify({ size: f.length, fnv1a64: fnv1a64(f) }) } : { isError: true, text: 'ERROR: ENOENT' };
    }
    case 'delete':
      for (const p of [...fake.files.keys()]) if (p.startsWith(`${args.path}/`)) fake.files.delete(p);
      return { text: `deleted ${args.path}` };
    case 'reboot':
      fake.rebootAt = Date.now();
      return { text: `rebooting once this answer is out: running generation ${fake.generation} (NEWEST_OK).\nnext start: KERNEL: generation ${fake.newest} (NEWEST_TRY: newest, new: TRY is written, its one try), as bitsboot's rule reads the boot stick now` };
    case 'reload_info': {
      fake.polls++;
      if (fake.mode === 'noline') return { text: 'reload counter 0\nboot stick: read-write' };
      if (fake.generation === fake.newest && fake.generation > 1) {
        fake.status = fake.polls >= 3 ? `generation ${fake.generation} confirmed (OK) in this boot`
          : `generation ${fake.generation} waits for its confirmation: MCP answers seen over ${fake.polls * 5} s of 120 s without a pause over 30 s (counter mcp.answered); a reboot before that falls back`;
      }
      return { text: `reload counter 0\nboot confirmation (RB-6): ${fake.status}\nboot stick: read-write` };
    }
    default: return { rpc: { code: -32602, message: `Unknown tool: ${name}` } };
  }
}
const mcp = createServer((req, res) => {
  const parts = [];
  req.on('data', (d) => parts.push(d));
  req.on('end', async () => {
    // The reboot: down for a second, then the new start (the fallback generation in mode 'fallback').
    if (fake.rebootAt) {
      if (Date.now() - fake.rebootAt < 1500) { req.socket.destroy(); return; }
      fake.boot++; fake.polls = 0; fake.rebootAt = null;
      fake.generation = fake.mode === 'fallback' ? 1 : fake.newest;
    }
    if (req.headers.authorization !== `Bearer ${TOKEN}`) return res.writeHead(401).end();
    const m = JSON.parse(Buffer.concat(parts).toString('utf8'));
    if (m.id === undefined) return res.writeHead(202).end();
    let result;
    if (m.method === 'initialize') result = { protocolVersion: '2025-06-18', serverInfo: { name: 'fake' }, capabilities: { tools: {} } };
    else if (m.method === 'tools/call') {
      const r = await tool(m.params.name, m.params.arguments ?? {});
      if (r.rpc) return res.writeHead(200, { 'Content-Type': 'application/json' }).end(JSON.stringify({ jsonrpc: '2.0', id: m.id, error: r.rpc }));
      result = { content: [{ type: 'text', text: r.text }], isError: r.isError === true };
    } else result = {};
    res.writeHead(200, { 'Content-Type': 'application/json' }).end(JSON.stringify({ jsonrpc: '2.0', id: m.id, result }));
  });
});
await new Promise((done) => mcp.listen(0, '127.0.0.1', done));
const HOST = `127.0.0.1:${mcp.address().port}`;

function reset(mode = 'ok') {
  Object.assign(fake, { mode, calls: [], fetched: [], files: new Map(), boot: 1, generation: 1, newest: 1, polls: 0, rebootAt: null,
    status: 'nothing to confirm: generation 1 started as NEWEST_OK' });
}
function run(args, env = {}) {
  return new Promise((done) => {
    const p = spawn(process.execPath, [INSTALL, ...args], { env: { ...process.env, BITSOS_TOKEN: TOKEN, BITSOS_INSTALL_PORTS: `${LO}-${HI}`, ...env } });
    let out = '';
    p.stdout.on('data', (d) => { out += d; });
    p.stderr.on('data', (d) => { out += d; });
    p.on('close', (code) => done({ code, out }));
  });
}
const BUNDLE = ['--bundle', join(tmp, 'b'), '--bind', '127.0.0.1'];
const installCalls = () => fake.calls.filter(([n]) => n === 'install_kernel').map(([, a]) => a);

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
  if (!cond) throw new Error(`${why}${out ? `\n--- output ---\n${out}` : ''}`);
}

await check('dry run: install_kernel gets url + dry_run + timeout_ms only, both files fetched whole, exit 0', async () => {
  reset();
  const r = await run([HOST, ...BUNDLE, '--dry-run']);
  need(r.code === 0 && r.out.includes('install: dry run ok') && r.out.includes('generation 2 would be installed') && r.out.includes('nothing was written'), `exit ${r.code}`, r.out);
  const [a] = installCalls();
  const m = /^http:\/\/10\.0\.2\.2:(\d+)\/install-[0-9a-f]{12}\/$/.exec(a?.url ?? '');
  need(m && Number(m[1]) >= LO && Number(m[1]) <= HI, `url ${a?.url}`);
  need(JSON.stringify(Object.keys(a).sort()) === '["dry_run","timeout_ms","url"]' && a.dry_run === true && a.timeout_ms === 300000, JSON.stringify(a));
  need(fake.fetched.some(([p, s, b]) => p.endsWith('/KERNEL') && s === 200 && b === kernel.length) && fake.fetched.some(([p, s, b]) => p.endsWith('/INITRD') && s === 200 && b === initrd.length), JSON.stringify(fake.fetched));
  // Only the two files: the directory, a path with .., another name, a POST are 404.
  need(fake.fetched.filter(([, s]) => s === 404).length === 4, `404s: ${JSON.stringify(fake.fetched)}`);
  need(fake.calls[0][0] === 'version', 'asks version first');
});

await check('install: the answer is printed, generation, exact bytes and both hashes checked, exit 0', async () => {
  reset();
  const r = await run([HOST, ...BUNDLE]);
  need(r.code === 0 && r.out.includes(`install: generation 2 installed (NEW)`) && r.out.includes(`fetched ${kernel.length} + ${initrd.length} bytes`)
    && r.out.includes(`KERNEL ${kernel.length} bytes, INITRD ${initrd.length} bytes`) && r.out.includes('next boot starts: generation 2 (NEWEST_TRY'), `exit ${r.code}`, r.out);
  need(r.out.includes('| installed generation 2 (NEW)'), 'the answer printed', r.out);
  need(installCalls()[0].dry_run === undefined, 'no dry_run');
});

await check('--config/--unpin/--allow-no-fallback/--timeout-ms are passed as asked', async () => {
  reset();
  const r = await run([HOST, ...BUNDLE, '--config', join(tmp, 'CONFIG.TXT'), '--unpin', '--allow-no-fallback', '--timeout-ms', '60000']);
  const [a] = installCalls();
  need(r.code === 0 && a.config === 'token=x\n# new line\n' && a.unpin === true && a.allow_no_fallback === true && a.timeout_ms === 60000, JSON.stringify(a), r.out);
});

await check('a refusal is printed and exits 1', async () => {
  reset('refuse');
  const r = await run([HOST, ...BUNDLE]);
  need(r.code === 1 && r.out.includes('| Refused: CONFIG.TXT') && r.out.includes('refused or failed'), `exit ${r.code}`, r.out);
});

await check('a hash BitsOS names that differs from the local file exits 1', async () => {
  reset('badhash');
  const r = await run([HOST, ...BUNDLE]);
  need(r.code === 1 && r.out.includes('KERNEL hash'), `exit ${r.code}`, r.out);
});

await check('a kernel without install_kernel: exit 1 with the Reload hint', async () => {
  reset('unknown');
  const r = await run([HOST, ...BUNDLE]);
  need(r.code === 1 && r.out.includes('Unknown tool') && r.out.includes('reload.mjs'), `exit ${r.code}`, r.out);
});

await check('the server listens only on --bind, in its range; a taken port moves on; the default range is 8770-8799', async () => {
  reset();
  // The first port taken (by this blocker, or already by someone else): install.mjs must move on.
  const blocker = tcpServer();
  await new Promise((done) => { blocker.once('error', done); blocker.listen(LO, '127.0.0.1', done); });
  const r = await run([HOST, ...BUNDLE, '--dry-run']);
  blocker.close();
  const port = Number(/:(\d+)\/install-/.exec(installCalls()[0].url)[1]);
  need(r.code === 0 && port > LO && port <= HI, `port ${port}`, r.out);
  need(r.out.includes(`listening on 127.0.0.1:${port} only`), 'the listening line', r.out);
  // Without the test override: the desk's range.
  reset();
  const d = await run([HOST, ...BUNDLE, '--dry-run'], { BITSOS_INSTALL_PORTS: '' });
  const dp = Number(/:(\d+)\/install-/.exec(installCalls()[0]?.url ?? ':0/install-')[1]);
  need(d.code === 0 && dp >= 8770 && dp <= 8799, `default range: port ${dp}`, d.out);
  reset();
  const o = await run([HOST, ...BUNDLE, '--port', '8800'], { BITSOS_INSTALL_PORTS: '' });
  need(o.code === 2 && o.out.includes('uses 8770-8799'), `--port 8800: exit ${o.code}`, o.out);
});

await check('the server is not reachable on another address of this machine', async () => {
  reset();
  // From inside the fake's install_kernel (install.mjs's server is up then): the same port on 127.0.0.2.
  let refusedElsewhere = null;
  fake.probe = async (url) => {
    const port = Number(/:(\d+)\//.exec(url)[1]);
    refusedElsewhere = await new Promise((done) => {
      const s = tcpConnect(port, '127.0.0.2');
      s.once('connect', () => { s.destroy(); done(false); });
      s.once('error', () => done(true));
    });
  };
  const r = await run([HOST, ...BUNDLE, '--dry-run']);
  need(r.code === 0, `exit ${r.code}`, r.out);
  need(refusedElsewhere === true, `127.0.0.2 ${refusedElsewhere === null ? 'not probed' : 'answered'}`);
  delete fake.probe;
});

await check('--upload: write_file in pieces, stat hash, install_kernel from the paths, uploads deleted', async () => {
  reset();
  const r = await run([HOST, '--kernel', join(tmp, 'b', 'KERNEL'), '--initrd', join(tmp, 'b', 'INITRD'), '--bind', '127.0.0.1', '--upload']);
  const [a] = installCalls();
  need(r.code === 0 && r.out.includes('read from the uploads') && /^\/tmp\/install-\d+\/KERNEL$/.test(a?.kernel_path ?? '') && a.initrd_path.endsWith('/INITRD') && a.url === undefined, JSON.stringify(a), r.out);
  const writes = fake.calls.filter(([c]) => c === 'write_file').length;
  need(writes === Math.ceil(kernel.length / 131072) + Math.ceil(initrd.length / 131072), `${writes} write_file calls`);
  need(fake.calls.some(([c, x]) => c === 'delete' && x.recursive === true && a.kernel_path.startsWith(x.path)) && fake.files.size === 0, 'uploads deleted');
  need(fake.fetched.length === 0, 'nothing served with --upload');
});

await check('--reboot: reboot, MCP from a new start with the new generation, reload_info until confirmed, exit 0', async () => {
  reset();
  const r = await run([HOST, ...BUNDLE, '--reboot', '--back-s', '30', '--confirm-s', '60']);
  need(r.code === 0 && r.out.includes('reboot: MCP back') && r.out.includes('confirm: generation 2 waits') && r.out.includes('install: generation 2 runs and confirmed itself'), `exit ${r.code}`, r.out);
  const names = fake.calls.map(([c]) => c);
  need(names.indexOf('reboot') > names.indexOf('install_kernel') && names.filter((c) => c === 'reload_info').length >= 3, names.join(','));
});

await check('--reboot: the fallback started instead of the new generation, exit 1', async () => {
  reset('fallback');
  const r = await run([HOST, ...BUNDLE, '--reboot', '--back-s', '30', '--confirm-s', '60']);
  need(r.code === 1 && r.out.includes('runs generation 1, not 2'), `exit ${r.code}`, r.out);
});

await check('--reboot: reload_info without the confirmation line fails early (5 answers), not after --confirm-s', async () => {
  reset('noline');
  const t0 = Date.now();
  const r = await run([HOST, ...BUNDLE, '--reboot', '--back-s', '30', '--confirm-s', '300']);
  need(r.code === 1 && r.out.includes('has no "boot confirmation (RB-6): ..." line in 5 answers'), `exit ${r.code}`, r.out);
  need(Date.now() - t0 < 60000, `took ${Date.now() - t0} ms`, r.out);
  need(fake.calls.filter(([c]) => c === 'reload_info').length === 5, 'five reload_info calls', r.out);
});

await check('arguments: exit 2 for each wrong one, nothing sent', async () => {
  for (const [args, want] of [
    [[HOST, ...BUNDLE, '--port', '8800'], 'the install server uses'],
    [[HOST, '--bundle', join(tmp, 'b'), '--bind', '0.0.0.0'], 'every network'],
    [[HOST, '--bundle', join(tmp, 'b'), '--bind', '8.8.8.8'], 'private network'],
    [[HOST, '--bundle', join(tmp, 'b')], '--bind'],
    [[HOST, '--bind', '127.0.0.1'], 'bundle is missing'],
    [[HOST, '--kernel', join(tmp, 'junk'), '--initrd', join(tmp, 'b', 'INITRD'), '--bind', '127.0.0.1'], 'no ELF'],
    [[HOST, '--kernel', join(tmp, 'b', 'KERNEL'), '--initrd', join(tmp, 'junk'), '--bind', '127.0.0.1'], 'newc'],
    [[HOST, ...BUNDLE, '--dry-run', '--reboot'], 'would start nothing'],
    [[HOST, ...BUNDLE, '--bogus'], 'unknown option'],
    [[HOST, ...BUNDLE, '--me', '8.8.8.8'], 'private IPv4'],
    [[HOST, '--bundle', join(tmp, 'b'), '--bind', '10.255.255.254'], 'no address of this machine'],
  ]) {
    reset();
    const r = await run(args);
    need(r.code === 2 && r.out.includes(want) && installCalls().length === 0, `${args.slice(1).join(' ')}: exit ${r.code}, want '${want}'`, r.out);
  }
  reset();
  const r = await run([HOST, ...BUNDLE], { BITSOS_TOKEN: 'wrong' });
  need(r.code === 2 && r.out.includes('401'), `wrong token: exit ${r.code}`, r.out);
  const r2 = await run(['127.0.0.1:1', ...BUNDLE]);
  need(r2.code === 2, `unreachable: exit ${r2.code}`, r2.out);
});

mcp.close();
rmSync(tmp, { recursive: true, force: true });
console.log(`install-proto: ${n} of ${n} ok`);
