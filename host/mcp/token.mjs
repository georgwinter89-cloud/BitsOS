// token.mjs (GP-R2): where every host MCP tool finds the token when the command line names no --token-file and
// BITSOS_TOKEN is not set: one rule for mcp-call.mjs, the remote desk's selftest.mjs and collect.mjs and the other
// tools that import mcp-call.mjs, given --gaming-pc. The round of 15.11. moved the token to
// %USERPROFILE%\.config\bitsos-gaming-pc\ on Windows; mcp-call.mjs alone did not look there. Plain Node, any OS.
import { existsSync } from 'node:fs';
import { homedir } from 'node:os';
import { join } from 'node:path';

export const TOKEN_PATH = ['.config', 'bitsos-gaming-pc', 'token.txt'];

// The places looked at, in order: BITSOS_TOKEN_FILE alone when set; else the token path below USERPROFILE (Windows'
// home, also when Node runs under a shell that sets HOME elsewhere), HOME, and Node's own idea of the home (only for
// the process's own environment: a test's made-up one stays closed).
export function tokenFileCandidates(env = process.env) {
  if (env.BITSOS_TOKEN_FILE) return [env.BITSOS_TOKEN_FILE];
  const homes = [env.USERPROFILE, env.HOME, env === process.env ? homedir() : undefined].filter((h) => typeof h === 'string' && h !== '');
  return [...new Set(homes.map((h) => join(h, ...TOKEN_PATH)))];
}

// The default token file: the first place that exists, else the first place (where selftest.mjs says to put it).
export function defaultTokenFile(env = process.env) {
  const c = tokenFileCandidates(env);
  return c.find((f) => existsSync(f)) ?? c[0];
}

// The file a tool reads its token from: --token-file as given; none when BITSOS_TOKEN carries the token; else, only
// when the target is the Gaming-PC (--gaming-pc; the remote desk's tools always), the default file when it exists
// (undefined otherwise: readToken then says where to put it). Without --gaming-pc no default is read: a call to
// the Mini-PC that forgot its token fails "no token" as before, and the Gaming-PC's token never goes to it.
export function tokenFileFor(given, gamingPc = false, env = process.env) {
  if (given) return given;
  if (!gamingPc || env.BITSOS_TOKEN?.trim()) return undefined;
  const f = defaultTokenFile(env);
  return existsSync(f) ? f : undefined;
}
