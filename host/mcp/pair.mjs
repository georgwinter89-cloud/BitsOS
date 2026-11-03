// pair.mjs (MC-8b): the one-time pairing of this host with BitsOS's user-space MCP service (bitsmcp).
//   node pair.mjs <host[:port]> --expect <sha256> | --trust-first-use [--store <dir>] [--token-file <path>]
//   e.g. node pair.mjs 192.168.100.113:8766 --expect <hex from the start line> --token-file token.txt
// Makes this host's key in the store (default ~/.config/bitsos-mcp/, 0700, never in the repository) unless it
// is there, sends its certificate over the service's plain path with the token (JSON-RPC bitsos/pair), keeps
// the Mini-PC's certificate from the answer as the pin (pins/<host>.der) and tries one call over TLS. The
// service takes a pairing only while no client is paired; from then on it serves TLS only (the kernel's own
// server under mcp=kernel/both stays plain, unchanged). The Mini-PC prints its key's fingerprint at every start
// ("bitsmcp: TLS server key sha256 <hex>", also on the console mirror): --expect <hex> is needed, the
// service pairs nothing when its key is another one (checked there, so the one pairing is not spent), and
// pair.mjs checks the answer again. The pairing request crosses the LAN in plain HTTP once (the token too):
// --expect is what stops another machine answering in the Mini-PC's place. --trust-first-use pairs without it
// (a loud warning; only for a network where nothing else can answer, the QEMU scenario's own).
// MC-10: under mcp=service the pairing is the only thing plain HTTP carries, and the service pairs only after
// Georg's yes at the Mini-PC's keyboard (the dialog names this host's key): pair.mjs waits up to 5 minutes for it.
// The store defaults to bitsos-mcp/ next to the token file (BITSOS_MCP_STORE, else ~/.config/bitsos-mcp/ without
// a token file); this host's certificate names it "bitsos-desk <hostname>" (the dialog shows that name).
// Exit code 0 paired, 1 the service refused, 2 not asked (unreachable, token, a pin is there already).
import { resolve } from 'node:path';
import { pathToFileURL } from 'node:url';
import { connect, McpError, plainRequest, readToken } from './mcp-call.mjs';
import { clientKey, defaultStore, fingerprint, KeyError, pinFile, writePin } from './mcp-keys.mjs';
import { existsSync } from 'node:fs';

const USAGE = 'usage: node pair.mjs <host[:port]> (--expect SHA256 | --trust-first-use) [--store DIR] [--token-file F]\n'
  + '  --expect  the fingerprint on the Mini-PC\'s start line "bitsmcp: TLS server key sha256 ..."\n'
  + '  --store   the key store (default: BITSOS_MCP_STORE, else bitsos-mcp/ next to the token file, else ~/.config/bitsos-mcp);\n'
  + '            the token from BITSOS_TOKEN or --token-file';

async function main(argv) {
  const opts = {}, rest = [];
  for (let i = 0; i < argv.length; i++) {
    if (['--store', '--token-file', '--expect'].includes(argv[i])) opts[argv[i].slice(2)] = argv[++i];
    else if (argv[i] === '--trust-first-use') opts.tofu = true;
    else rest.push(argv[i]);
  }
  const [host] = rest;
  if (!host || rest.length > 1 || Object.values(opts).includes(undefined)) { console.error(USAGE); return 2; }
  const store = opts.store ?? defaultStore(opts['token-file']);
  try {
    const pf = pinFile(store, host);
    if (existsSync(pf)) {
      console.error(`${host} is paired already (pin ${pf}); to pair again remove that file first (and \\BITSOS2\\mcp\\paired\\ on the Mini-PC's stick: the service takes one pairing only)`);
      return 2;
    }
    if (!opts.expect && !opts.tofu) {
      console.error(`--expect <sha256> is needed: the fingerprint from the Mini-PC's start line "bitsmcp: TLS server key sha256 ..." (console or console mirror); --trust-first-use pairs without it\n${USAGE}`);
      return 2;
    }
    if (!opts.expect) console.error('WARNING: pairing without --expect: whatever answers at this address is trusted from now on; compare the key printed below with the Mini-PC\'s start line');
    const token = readToken(opts['token-file']);
    const me = clientKey(store, { make: true });
    console.log(`${me.made ? 'made' : 'using'} this host's key: sha256 ${fingerprint(me.der)} (${resolve(store)})`);
    console.log('pairing; under mcp=service Georg confirms it at the Mini-PC\'s keyboard (up to 5 minutes)');
    let r;
    try {
      r = await plainRequest(host, token, 'bitsos/pair', { certificate: me.der.toString('base64'), ...(opts.expect ? { expect: opts.expect } : {}) }, 310000);
    } catch (e) {
      if (e instanceof McpError && e.kind === 'answer') { console.error(`the service refused the pairing: ${e.message}`); return 1; }
      throw e;
    }
    const server = Buffer.from(String(r?.server_certificate ?? ''), 'base64');
    const fp = fingerprint(server);
    if (!server.length || fp !== r.server_fingerprint) { console.error(`the answer carries no usable server key: ${JSON.stringify(r)}`); return 2; }
    if (r.paired !== fingerprint(me.der)) { console.error(`the service paired ${r.paired}, not this host's key ${fingerprint(me.der)}`); return 2; }
    if (opts.expect && opts.expect.toLowerCase() !== fp) {
      console.error(`REFUSED: the service's key is sha256 ${fp}, not the expected ${opts.expect}; nothing pinned. Something else may answer at ${host}.`);
      return 2;
    }
    const f = writePin(store, host, server);
    console.log(`paired. the Mini-PC's key: sha256 ${fp} (pinned in ${f}); compare it with its start line "bitsmcp: TLS server key sha256 ..."`);
    const s = await connect(host, { token, tls: store, grantWaitMs: 0 });

    const list = await s.request('tools/list', {});
    console.log(`over TLS: ${s.url} answers, ${list?.tools?.length ?? 0} tools`);
    console.log(`from now: the tools with --tls ${resolve(store)}; the service's plain HTTP is off`);
    return 0;
  } catch (e) {
    if (!(e instanceof McpError || e instanceof KeyError)) throw e;
    console.error(e.message);
    return 2;
  }
}

if (process.argv[1] && pathToFileURL(resolve(process.argv[1])).href === import.meta.url) process.exitCode = await main(process.argv.slice(2));
