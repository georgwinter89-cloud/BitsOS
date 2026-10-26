// echt.mjs (V2): a program built on this machine runs on the real BitsOS V2: uploads a static x86-64 Linux ELF
// with write_file (in pieces, hash compared with stat), runs it with `run` and shows how it ended, its stdout
// and its stderr. The idea of V1's werkzeuge/plattform/echt/echt.mjs (runTest: upload, run, ok/FAIL); the A1
// check suite of the MCP server itself is host/tests/mcp/echt.mjs.
//
//   node echt.mjs <host[:port]> <local ELF> [program arguments ...] [--to <path>] [--keep]
//        [--timeout-ms N] [--stdin-file <file>] [--expect-exit N] [--token-file <path>]
//
// --to: where the program goes on BitsOS (default /tmp/echt/<file name>); --keep leaves it there (default:
// deleted after the run). --timeout-ms: run's deadline (default 30000; a program still running then is
// reported with its call number, and `output`/`stop` reach it: mcp-call.mjs <host> output '{"call":n}').
// --stdin-file: bytes for its standard input. --expect-exit: the exit code that counts as success (default 0).
// Prints "ok <name>: ..." or "FAIL <name>: ..." and the run's answer. Exit code 0 = ok, 1 = FAIL (refused,
// not the expected end), 2 = BitsOS cannot be asked.
import { readFileSync } from 'node:fs';
import { basename } from 'node:path';
import { connect, McpError, readToken, upload } from './mcp-call.mjs';

const opts = {}, rest = [];
for (let i = 2; i < process.argv.length; i++) {
  const a = process.argv[i];
  if (['--to', '--timeout-ms', '--stdin-file', '--expect-exit', '--token-file'].includes(a)) opts[a.slice(2)] = process.argv[++i] ?? '';
  else if (a === '--keep') opts.keep = true;
  else rest.push(a);
}
const [host, elfFile, ...progArgs] = rest;
if (!host || !elfFile) {
  console.error('usage: node echt.mjs <host[:port]> <local ELF> [args ...] [--to <path>] [--keep] [--timeout-ms N] [--stdin-file F] [--expect-exit N] [--token-file F]');
  process.exit(2);
}
const name = basename(elfFile);
const to = opts.to ?? `/tmp/echt/${name}`;
const timeout = Number(opts['timeout-ms'] ?? 30000);
const expectExit = Number(opts['expect-exit'] ?? 0);
const say = (ok, what) => { console.log(`${ok ? 'ok' : 'FAIL'} ${name}: ${what}`); return ok ? 0 : 1; };

let elf;
try { elf = readFileSync(elfFile); } catch (e) { console.error(`cannot read ${elfFile}: ${e.message}`); process.exit(2); }
if (elf.length < 64 || elf.toString('latin1', 0, 4) !== '\x7fELF' || elf[4] !== 2 || elf.readUInt16LE(18) !== 62) {
  process.exit(say(false, `${elfFile} is no x86-64 ELF (64-bit, machine 62)`));
}

async function main() {
  const session = await connect(host, { token: readToken(opts['token-file']) });
  let up;
  try {
    up = await upload(session, elf, to, { mode: '0755', parents: true });
  } catch (e) {
    if (e instanceof McpError && e.kind === 'answer') return say(false, `upload: ${e.message}`);
    throw e;
  }
  console.log(`uploaded ${up.bytes} bytes to ${to} in ${up.ms} ms (fnv ${up.fnv})`);
  const args = { path: to, args: progArgs, timeout_ms: timeout };
  if (opts['stdin-file']) args.stdin_b64 = readFileSync(opts['stdin-file']).toString('base64');
  const t0 = Date.now();
  const r = await session.call('run', args);
  const ms = Date.now() - t0;
  process.stdout.write(r.text.split('\n').map((l) => `  ${l}`).join('\n') + '\n');
  if (!opts.keep) {
    const d = await session.call('delete', { path: to });
    if (d.isError) console.log(`(could not delete ${to}: ${d.text})`);
  }
  if (r.isError) return say(false, `run answered an error after ${ms} ms`);
  const still = r.text.match(/\(call (\d+)\)/);
  if (/^Still running/.test(r.text) && still) {
    return say(false, `still running after ${timeout} ms (call ${still[1]}: output/stop with {"call":${still[1]}})`);
  }
  const ended = r.text.match(/ended: exit (-?\d+)/);
  if (!ended) return say(false, `no exit code in the answer (${ms} ms): it ended by a signal or fault`);
  const code = Number(ended[1]);
  return say(code === expectExit, `exit ${code}${code === expectExit ? '' : `, expected ${expectExit}`} after ${ms} ms`);
}

try {
  process.exitCode = await main();
} catch (e) {
  if (!(e instanceof McpError)) throw e;
  console.error(e.message);
  process.exitCode = 2;
}
