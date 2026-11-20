// phase.mjs (GP-R3): round 2's phases of checks.json, the pure parts collect.mjs runs them with. A phase is one
// ordered piece of the round that the remote desk runs alone (`collect.mjs <host> --phase <id>`) and that stops at
// its first red; between phases Georg does something at the Gaming-PC (the guide plan/guides/gaming-pc-runde2.md).
// Plain Node, no shell, any OS.
import { mkdirSync, readFileSync, writeFileSync, existsSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const HERE = dirname(fileURLToPath(import.meta.url));

// Kinds a phase may hold. read/write/ktest as in the first round's list; sound = the one tone (only with --sound,
// Georg's yes); install = host/mcp/install.mjs with the desk's bundle (--bundle); confirm = wait until the running
// generation confirmed itself; keys = Georg types at the Gaming-PC's keyboard, the events are read back.
export const KINDS = ['read', 'write', 'ktest', 'sound', 'install', 'confirm', 'keys'];

export function phaseIds(checks) {
  return (checks.phases ?? []).map((p) => p.id);
}

// The phase with its steps resolved: a step `{"use": "<id>", ...}` is the top-level step of that id with the
// phase step's own fields over it (its own `id` always); its `want+`/`refuse+` become `plus` (`withPlus`).
export function phase(checks, id) {
  const p = (checks.phases ?? []).find((x) => x.id === id);
  if (!p) return null;
  const top = new Map((checks.steps ?? []).map((s) => [s.id, s]));
  const steps = p.steps.map((s) => {
    if (!s.use) return { ...s };
    const base = top.get(s.use);
    if (!base) throw new Error(`phase ${id} step ${s.id}: no step ${s.use} to use`);
    const out = { ...base, ...s, plus: { want: s['want+'] ?? [], refuse: s['refuse+'] ?? [] } };
    delete out['want+']; delete out['refuse+']; delete out.use;
    return out;
  });
  return { ...p, steps };
}

// A used step's own additions, put on after the facts shaped it (a `when` may replace the used step's lists:
// with gp_nvme= step 05's `want` is another list, and B2-01's `nvme_data=` must still be wanted).
export function withPlus(s) {
  if (!s.plus) return s;
  return { ...s, want: [...(s.want ?? []), ...s.plus.want], refuse: [...(s.refuse ?? []), ...s.plus.refuse] };
}

// What a phase needs from the command line before its first call: the bundle for a non-optional install step.
export function needs(p) {
  return { bundle: p.steps.some((s) => s.kind === 'install' && !s.optional), sound: p.steps.some((s) => s.kind === 'sound') };
}

// The facts a phase requires (`require: {fact: true|false}`): each one that does not hold, said plainly. An unknown
// fact is red too: the phase's meaning hangs on it.
export function requireProblems(p, facts) {
  return Object.entries(p.require ?? {}).flatMap(([name, want]) => {
    const v = facts[name];
    if (v === want) return [];
    const why = p.require_why?.[name] ?? '';
    return [`${name} is ${v ?? 'unknown'}, this phase needs ${want}${why ? `: ${why}` : ''}`];
  });
}

// The first number a regular expression with one group finds in `text`, or null.
export function number(text, re) {
  const m = new RegExp(re, 'm').exec(text);
  return m ? Number(m[1]) : null;
}

// `input` answers: the device numbers whose line names `id` (vendor:product), and the key codes pressed (value 1)
// on those devices, not injected (MCP's own devices are injected).
export function keysSeen(text, id) {
  const devices = new Set();
  for (const m of text.matchAll(/^device (\d+) name="[^"]*" phys=\S+ id=([0-9a-f]{4}:[0-9a-f]{4}) keyboard=true/gm)) if (m[2] === id) devices.add(m[1]);
  const codes = [];
  for (const m of text.matchAll(/^ev \d+ t=\S+ dev=(\S+) KEY code=(\d+) value=1(\s+injected)?$/gm)) {
    if (!m[3] && devices.has(m[1])) codes.push(Number(m[2]));
  }
  return { devices: [...devices], codes };
}

// The highest event sequence number an `input` answer shows (-1 without events).
export function lastSeq(text) {
  let n = -1;
  for (const m of text.matchAll(/^ev (\d+) /gm)) n = Math.max(n, Number(m[1]));
  return n;
}

// The next event's sequence number of an `input` answer.
export function nextSeq(text) {
  return number(text, 'next_seq=(\\d+)');
}

// The lines of `text` holding `pattern` (a guard hash keeps only those: the rest of the nvme answer changes).
export function lines(text, pattern) {
  return text.split(/\r?\n/).filter((l) => l.includes(pattern)).map((l) => l.trim());
}

// Kept answers between runs and boots (the guard hashes of the Lexar): host/remote-desk/out/keep/<name>.txt
// (BITSOS_DESK_KEEP moves the folder, for the tests).
export function keepFile(name, dir = process.env.BITSOS_DESK_KEEP ?? join(HERE, 'out', 'keep')) {
  if (!/^[a-z0-9-]+$/.test(name)) throw new Error(`keep name ${name}: lower-case letters, digits and - only`);
  return join(dir, `${name}.txt`);
}

export function keep(name, kept, dir) {
  const f = keepFile(name, dir);
  mkdirSync(dirname(f), { recursive: true });
  writeFileSync(f, `${kept.join('\n')}\n`);
  return f;
}

export function kept(name, dir) {
  const f = keepFile(name, dir);
  return existsSync(f) ? readFileSync(f, 'utf8').split(/\r?\n/).filter(Boolean) : null;
}

// install.mjs's arguments for an install step (the token goes in the environment, never on the command line).
export function installArgv(step, host, bundle) {
  const tool = resolve(HERE, '..', 'mcp', 'install.mjs');
  const a = [tool, host, '--bundle', bundle, '--upload'];
  if (step.dry_run) a.push('--dry-run');
  if (step.reboot) a.push('--reboot', '--back-s', String(step.back_s ?? 240), '--confirm-s', String(step.confirm_s ?? 300));
  return a;
}
