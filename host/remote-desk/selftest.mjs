// selftest.mjs (GP-ROUND): checks this folder on the remote desk's machine (the Mini-PC under Windows) before
// the first call: the Node version, that every tool loads, the arguments files, the token file (never printed)
// and, with --host, one read-only call to the Gaming-PC. Plain Node, no shell, any OS.
//
//   node host/remote-desk/selftest.mjs [--host <address[:port]>] [--token-file <path>]
//
// Exit 0: ready; 1: something to fix (each line says what).
import { existsSync, readdirSync, readFileSync, statSync } from 'node:fs';
import { dirname, isAbsolute, join, relative, resolve } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { isMain } from '../mcp/is-main.mjs';
import { defaultTokenFile } from '../mcp/token.mjs';

const HERE = dirname(fileURLToPath(import.meta.url));
const REPO = resolve(HERE, '..', '..');
export const MIN_NODE = 20;

// Where the token of the Gaming-PC's stick lives on this machine, outside the repository and never in git: the
// one rule of host/mcp/token.mjs, which every tool shares (GP-R2).
export { defaultTokenFile } from '../mcp/token.mjs';

// The token rules of the kernel (8 to 127 characters from ! to ~; CONFIG's comment sign # cuts it).
export function tokenProblem(text) {
  const t = text.replace(/^﻿/, '').split(/\r?\n/)[0].trim();
  if (!t) return 'the first line is empty';
  if (t.length < 8 || t.length > 127) return `the token has ${t.length} characters, the kernel wants 8 to 127`;
  if (!/^[!-~]+$/.test(t)) return 'the token holds a space or a character outside ! to ~';
  if (t.includes('#')) return 'the token holds #: CONFIG.TXT cuts it there';
  return null;
}

async function main(argv) {
  const opts = {};
  for (let i = 0; i < argv.length; i++) if (['--host', '--token-file'].includes(argv[i])) opts[argv[i].slice(2)] = argv[++i];
  let bad = 0;
  const say = (ok, what) => { console.log(`${ok ? 'ok  ' : 'FIX '} ${what}`); if (!ok) bad++; };
  const major = Number(process.versions.node.split('.')[0]);
  say(major >= MIN_NODE, `Node ${process.versions.node} (${MIN_NODE} or newer wanted) on ${process.platform}`);
  say(true, `this folder: ${HERE}${/\s/.test(HERE) ? ' (a path with spaces: fine)' : ''}`);
  for (const m of ['mcp-call.mjs', 'mcp-keys.mjs', 'is-main.mjs', 'token.mjs', 'install.mjs', 'push-plan.mjs']) {
    const f = join(REPO, 'host', 'mcp', m);
    if (!existsSync(f)) { say(false, `host/mcp/${m} is missing: copy the whole host/mcp folder beside host/remote-desk`); continue; }
    if (m === 'install.mjs') { say(true, `host/mcp/${m} present (not loaded: it starts on import)`); continue; }
    try { await import(pathToFileURL(f).href); say(true, `host/mcp/${m} loads`); } catch (e) { say(false, `host/mcp/${m} does not load: ${e.message}`); }
  }
  const { isMain: im } = await import(pathToFileURL(join(REPO, 'host', 'mcp', 'is-main.mjs')).href);
  const me = fileURLToPath(import.meta.url);
  say(im(import.meta.url, me.toUpperCase(), 'win32') && im(import.meta.url, me.toLowerCase(), 'win32'), 'a command typed in another case still starts its tool (Windows rule)');
  const argsDir = join(HERE, 'args');
  for (const f of existsSync(argsDir) ? readdirSync(argsDir).filter((n) => n.endsWith('.json')) : []) {
    try { JSON.parse(readFileSync(join(argsDir, f), 'utf8').replace(/^﻿/, '')); say(true, `args/${f} is JSON`); } catch (e) { say(false, `args/${f}: ${e.message}`); }
  }
  try {
    const checks = JSON.parse(readFileSync(join(HERE, 'checks.json'), 'utf8').replace(/^﻿/, ''));
    say(checks.steps.length > 0, `checks.json: ${checks.steps.length} steps`);
    // GP-R3: round 2's phases, each resolved as collect.mjs --phase runs it.
    const ph = await import(pathToFileURL(join(HERE, 'phase.mjs')).href);
    const ids = ph.phaseIds(checks);
    const sizes = ids.map((id) => `${id} ${ph.phase(checks, id).steps.length}`);
    say(ids.length > 0, `phases of round 2: ${sizes.join(', ')}`);
  } catch (e) { say(false, `checks.json: ${e.message}`); }
  const tf = opts['token-file'] ?? defaultTokenFile();
  if (process.env.BITSOS_TOKEN && !opts['token-file']) {
    const p = tokenProblem(process.env.BITSOS_TOKEN);
    say(!p, `the token from BITSOS_TOKEN${p ? `: ${p}` : ' looks right (not shown)'}`);
  } else if (!existsSync(tf)) {
    say(false, `no token file at ${tf}: put the token= value of the Gaming-PC stick's CONFIG.TXT there (one line)`);
  } else {
    const p = tokenProblem(readFileSync(tf, 'utf8'));
    say(!p, `token file ${tf}${p ? `: ${p}` : ' looks right (not shown)'}`);
    // Another drive than the repository's gives an absolute "relative" path on Windows: outside too.
    const rel = relative(REPO, resolve(tf));
    const outside = rel.startsWith('..') || isAbsolute(rel);
    say(outside, `the token file is outside the repository${outside ? '' : ` (it is inside: ${rel}; move it out, it must never reach git)`}`);
    if (process.platform !== 'win32') {
      const mode = statSync(tf).mode & 0o077;
      say(mode === 0, `token file readable by its owner only${mode ? ` (chmod 600 ${tf})` : ''}`);
    }
  }
  if (opts.host) {
    const { connect, readToken } = await import(pathToFileURL(join(REPO, 'host', 'mcp', 'mcp-call.mjs')).href);
    try {
      const s = await connect(opts.host, { token: readToken(process.env.BITSOS_TOKEN && !opts['token-file'] ? undefined : tf), ms: 20000 });
      const v = await s.call('hw_info', { section: 'kernel' });
      say(!v.isError && v.text.includes('profile=gaming-pc'), `MCP at ${opts.host} answers and names the Gaming-PC's profile${v.text.includes('profile=gaming-pc') ? '' : `: ${v.text.split('\n').find((l) => l.startsWith('profile=')) ?? 'no profile line'}`}`);
    } catch (e) {
      say(false, `MCP at ${opts.host}: ${e.message}`);
    }
  }
  console.log(bad ? `${bad} thing(s) to fix` : 'ready');
  return bad ? 1 : 0;
}

if (isMain(import.meta.url)) process.exitCode = await main(process.argv.slice(2));
