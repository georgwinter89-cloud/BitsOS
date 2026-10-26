// mcp-call.mjs (V2): a small MCP client for BitsOS V2's kernel MCP server, as a command and as a module for
// reload.mjs and echt.mjs. Same command line as V1's BitsOS-Plattform/mcp-call.mjs:
//   node mcp-call.mjs <host[:port]> <tool> [json arguments] [--token-file <path>] [--png <file>] [--save <file>]
//   e.g. node mcp-call.mjs 192.168.100.113 hw_info '{"section":"cpu"}' --token-file token.txt
//        node mcp-call.mjs 127.0.0.1:25300 screen '{}' --png screen.png
//        node mcp-call.mjs 127.0.0.1:25300 read_file '{"path":"/bin/hello"}' --save hello.elf
// Each request goes on a connection of its own (initialize, notifications/initialized, tools/call), node:http
// without keep-alive (V1's reasons: Windows/undici probes and fetch's 300 s limit). The tool's text goes to
// stdout; a PNG (screen) to --png; --save writes the bytes of a read_file answer (its base64 data, decoded).
// The port is 8765 unless given. The token comes from --token-file (its first line) or BITSOS_TOKEN; it is
// never printed and never in a URL. The answer is awaited timeout_ms + 30 s (the tool's own deadline plus
// room), so long calls work.
// Exit code 0 when the tool did its work, 1 when it answered with an error (isError), 2 when BitsOS could not
// be asked (not reachable, the token refused, an answer that is no MCP).
import { readFileSync, writeFileSync } from 'node:fs';
import { request } from 'node:http';
import { resolve } from 'node:path';
import { pathToFileURL } from 'node:url';

export const PROTOCOL = '2025-06-18';

// kind: 'unreachable', 'timeout', 'token' or 'answer'; the message in plain words, without the token
export class McpError extends Error {
  constructor(kind, message) { super(message); this.kind = kind; }
}

// --token-file's first line, else BITSOS_TOKEN
export function readToken(file) {
  if (file) {
    let text;
    try { text = readFileSync(file, 'utf8'); } catch { throw new McpError('token', `cannot read the token file ${file}`); }
    const token = text.split(/\r?\n/)[0].trim();
    if (!token) throw new McpError('token', `the token file ${file} is empty`);
    return token;
  }
  const token = process.env.BITSOS_TOKEN?.trim();
  if (!token) throw new McpError('token', 'no token: set BITSOS_TOKEN or give --token-file <path> (the token= of CONFIG.TXT)');
  return token;
}

export const mcpUrl = (host) => `http://${/:\d+$/.test(host) ? host : `${host}:8765`}/mcp`;

function send(url, token, body, ms) {
  return new Promise((done, failed) => {
    const req = request(url, { method: 'POST', agent: false, signal: AbortSignal.timeout(ms), headers: {
      'Content-Type': 'application/json', Accept: 'application/json, text/event-stream', 'MCP-Protocol-Version': PROTOCOL,
      Authorization: `Bearer ${token}`, 'Content-Length': Buffer.byteLength(body) } }, (res) => {
      const parts = [];
      res.on('data', (d) => parts.push(d));
      res.on('end', () => done({ status: res.statusCode, text: Buffer.concat(parts).toString('utf8') }));
      res.on('error', failed);
    });
    req.on('error', failed);
    req.end(body);
  });
}

// One POST to url -> the JSON-RPC result (null for 202); McpError otherwise
async function post(url, token, message, ms) {
  let res;
  try {
    res = await send(url, token, JSON.stringify(message), ms);
  } catch (e) {
    if (e.name === 'AbortError' || e.name === 'TimeoutError') throw new McpError('timeout', `no answer from BitsOS at ${url} within ${Math.round(ms / 1000)} s`);
    throw new McpError('unreachable', `BitsOS does not answer at ${url} (${e.code ?? e.message})`);
  }
  if (res.status === 401) throw new McpError('token', 'BitsOS refused the token (401): it must be the token= of its CONFIG.TXT');
  if (res.status === 202) return null;
  if (res.status !== 200) throw new McpError('answer', `BitsOS answered HTTP ${res.status} at ${url}`);
  let json;
  try { json = JSON.parse(res.text); } catch { throw new McpError('answer', 'BitsOS sent an answer that is no JSON'); }
  if (json.error) throw new McpError('answer', `BitsOS: ${json.error.message} (${json.error.code})`);
  return json.result;
}

// initialize and notifications/initialized -> a session: url, server (name, version),
// call(tool, args, ms) -> { isError, text, texts (each text part), image }
export async function connect(host, { token, ms = 30000 } = {}) {
  const url = mcpUrl(host);
  let id = 0;
  const rpc = (method, params, wait = ms) => post(url, token, { jsonrpc: '2.0', id: ++id, method, params }, wait);
  const init = await rpc('initialize', { protocolVersion: PROTOCOL, capabilities: {}, clientInfo: { name: 'mcp-call-v2', version: '1' } });
  if (!init?.protocolVersion) throw new McpError('answer', `${url} answered initialize without a protocol version: no MCP server`);
  await post(url, token, { jsonrpc: '2.0', method: 'notifications/initialized' }, ms);
  return {
    url,
    server: init.serverInfo ?? {},
    async call(name, args = {}, wait = Math.max(ms, Number(args.timeout_ms ?? 0) + 30000)) {
      const r = await rpc('tools/call', { name, arguments: args }, wait);
      const content = r?.content ?? [];
      const texts = content.filter((c) => c.type === 'text').map((c) => c.text);
      return { isError: r?.isError === true, text: texts.join(''), texts, image: content.find((c) => c.type === 'image') };
    },
  };
}

// FNV-1a 64 of a buffer as "0x%016x" (the hash stat, fetch and reload_info show)
export function fnv1a64(buf) {
  // two 32-bit halves (BigInt per byte is slow for megabytes): (hi:lo) * (2^40 + 0x1b3) mod 2^64
  let hi = 0xcbf29ce4, lo = 0x84222325;
  for (const b of buf) {
    lo = (lo ^ b) >>> 0;
    const p = lo * 0x1b3;  // < 2^41: exact in a double
    hi = (Math.imul(hi, 0x1b3) + Math.floor(p / 4294967296) + (lo << 8)) >>> 0;
    lo = p >>> 0;
  }
  return `0x${hi.toString(16).padStart(8, '0')}${lo.toString(16).padStart(8, '0')}`;
}

// Bytes per write_file request (a request is at most 256 KB on the wire; base64 makes 128 KiB about 175 KB)
export const PIECE = 128 * 1024;

// Writes buf to path on BitsOS with write_file in pieces (the first replaces the file with mode, the rest go at
// their offsets), then compares stat's hash with the local one. -> { bytes, ms, fnv }; throws on a refusal.
export async function upload(session, buf, path, { mode = '0644', parents = false } = {}) {
  const t0 = Date.now();
  for (let off = 0; off < buf.length || off === 0; off += PIECE) {
    const piece = buf.subarray(off, off + PIECE);
    const args = off === 0 ? { path, data: piece.toString('base64'), mode, parents } : { path, data: piece.toString('base64'), offset: off };
    const r = await session.call('write_file', args);
    if (r.isError) throw new McpError('answer', `write_file ${path} at ${off}: ${r.text}`);
    if (buf.length === 0) break;
  }
  // hash_max: stat refuses to hash more than 256 MiB unless asked; an upload asks for exactly its own length.
  const s = await session.call('stat', { path, hash: true, hash_max: buf.length });
  if (s.isError) throw new McpError('answer', `stat ${path}: ${s.text}`);
  const st = JSON.parse(s.text);
  const fnv = fnv1a64(buf);
  if (st.size !== buf.length || st.fnv1a64 !== fnv) {
    throw new McpError('answer', `${path} on BitsOS has ${st.size} bytes, hash ${st.fnv1a64}; the local file ${buf.length} bytes, hash ${fnv}`);
  }
  return { bytes: buf.length, ms: Date.now() - t0, fnv };
}

// The command line: <host[:port]> <tool> [json] [--token-file <path>] [--png <file>] [--save <file>] -> exit code
async function main(argv) {
  const opts = {}, rest = [];
  for (let i = 0; i < argv.length; i++) {
    if (['--token-file', '--png', '--save'].includes(argv[i])) opts[argv[i].slice(2)] = argv[++i];
    else rest.push(argv[i]);
  }
  const [host, tool, json] = rest;
  if (!host || !tool || rest.length > 3 || Object.values(opts).includes(undefined)) {
    console.error('usage: node mcp-call.mjs <host[:port]> <tool> [json arguments] [--token-file <path>] [--png <file>] [--save <file>]\n'
      + 'the token from BITSOS_TOKEN or --token-file');
    return 2;
  }
  let args = {};
  if (json) {
    try { args = JSON.parse(json); } catch { console.error(`the arguments are no JSON object: ${json}`); return 2; }
    if (typeof args !== 'object' || args === null || Array.isArray(args)) { console.error('the arguments must be a JSON object'); return 2; }
  }
  try {
    const session = await connect(host, { token: readToken(opts['token-file']) });
    const r = await session.call(tool, args);
    if (opts.save && !r.isError) {
      let data;
      try { data = JSON.parse(r.text).data; } catch { /* not a read_file answer */ }
      if (typeof data !== 'string') { console.error('--save: the answer carries no base64 data (read_file answers do)'); return 1; }
      const bytes = Buffer.from(data, 'base64');
      writeFileSync(opts.save, bytes);
      const j = JSON.parse(r.text);
      console.log(`${j.path}: ${bytes.length} bytes from offset ${j.offset} saved to ${opts.save}${j.eof ? '' : ` (more: offset ${j.next_offset})`}`);
      return 0;
    }
    process.stdout.write(r.text.endsWith('\n') || !r.text ? r.text : `${r.text}\n`);
    if (r.image && opts.png) {
      const png = Buffer.from(r.image.data, 'base64');
      writeFileSync(opts.png, png);
      console.log(`${r.image.mimeType} saved to ${opts.png} (${png.length} bytes)`);
    } else if (r.image) console.log('(an image came: --png <file> saves it)');
    return r.isError ? 1 : 0;
  } catch (e) {
    if (!(e instanceof McpError)) throw e;
    console.error(e.message);
    return 2;
  }
}

if (process.argv[1] && pathToFileURL(resolve(process.argv[1])).href === import.meta.url) process.exitCode = await main(process.argv.slice(2));
