// access.mjs (MC-10): full access for this host at BitsOS's MCP service, over mutual TLS with the key pair.mjs
// made. Georg grants it only at the Mini-PC's real keyboard (1 h, 8 h, 24 h preselected, 7 Tage, optionally the
// NVMe write gate); nothing here can grant or extend it.
//   node access.mjs <host[:port]> request [--hours H] [--reason TEXT] [--tls DIR|auto] [--token-file F]
//   node access.mjs <host[:port]> status [--tls DIR|auto] [--token-file F]
// request waits up to 5 minutes for Georg's answer and prints it (the end time); status prints the time left.
// Exit code 0 granted (status: a grant runs), 1 refused (status: none), 2 not asked (unreachable, token, key;
// status: still "grants not loaded yet" after the wait, MC-11: right after a start the installer has not reported).
import { resolve } from 'node:path';
import { pathToFileURL } from 'node:url';
import { connect, McpError, NOT_LOADED, readToken } from './mcp-call.mjs';

const USAGE = 'usage: node access.mjs <host[:port]> request [--hours 1..168] [--reason TEXT] | status  [--tls DIR|auto] [--token-file F]';

export async function main(argv) {
  const opts = { tls: 'auto' }, rest = [];
  for (let i = 0; i < argv.length; i++) {
    if (['--hours', '--reason', '--tls', '--token-file'].includes(argv[i])) opts[argv[i].slice(2)] = argv[++i];
    else rest.push(argv[i]);
  }
  const [host, what] = rest;
  if (!host || !['request', 'status'].includes(what) || rest.length > 2 || Object.values(opts).includes(undefined)) { console.error(USAGE); return 2; }
  const args = {};
  if (opts.hours !== undefined) {
    const h = Number(opts.hours);
    if (!Number.isInteger(h) || h < 1 || h > 168) { console.error(`--hours ${opts.hours}: 1 to 168`); return 2; }
    args.hours = h;
  }
  if (opts.reason !== undefined) args.reason = String(opts.reason).slice(0, 200);
  try {
    const s = await connect(host, { token: readToken(opts['token-file']), tls: opts.tls, tokenFile: opts['token-file'], ms: 30000 });
    if (what === 'status') {
      const r = await s.call('access_status', {});
      console.log(r.texts[0] ?? r.text);
      if (r.isError || r.text.startsWith(NOT_LOADED)) return 2;
      return r.text.startsWith('full access') ? 0 : 1;
    }
    console.log('asking Georg at the Mini-PC\'s keyboard (the dialog waits up to 4 minutes) ...');
    const r = await s.call('request_access', args, 310000);
    console.log(r.text);
    return r.isError ? 1 : 0;
  } catch (e) {
    if (!(e instanceof McpError)) throw e;
    console.error(e.message);
    return 2;
  }
}

if (process.argv[1] && pathToFileURL(resolve(process.argv[1])).href === import.meta.url) process.exitCode = await main(process.argv.slice(2));
