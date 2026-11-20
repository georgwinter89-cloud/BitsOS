// collect.mjs (GP-ROUND, GP-R2, GP-R3): runs the Gaming-PC's remote check list (checks.json) against the machine, in
// order, saves every answer to <out>/<step>.txt and judges each step by its want/refuse words. Read-only by default;
// --write adds the two write checks, --ktest the ktest round. Plain Node (20 or newer), no shell, any OS.
//
//   node host/remote-desk/collect.mjs <host[:port]> [--token-file <path>] [--out <dir>] [--only <id prefixes, comma-separated>]
//        [--write] [--ktest] [--heartbeat-s N] [--confirm-wait-s N]
//   node host/remote-desk/collect.mjs <host[:port]> --phase <id> [--from <step id>] [--bundle <dir>] [--sound] [...]
//
// The token as mcp-call.mjs takes it (--token-file, BITSOS_TOKEN, else the default token file of host/mcp/token.mjs).
// GP-R2: the machine's own settings decide what a step expects (checks.json "facts": read from its kernel log
// before the first step, never guessed), and a heartbeat (reload_info every --heartbeat-s, default 10 s, on its own
// connection, also while a long ktest call runs) keeps MCP answering: a generation on its one try confirms itself
// only after MCP answered for 120 s without a pause over 30 s (crates/kexec/src/install/confirm.rs). After the
// steps it keeps the heartbeat up until reload_info says confirmed (at most --confirm-wait-s, default 200; 0: not).
// GP-R3: --phase runs one phase of round 2 (checks.json "phases", phase.mjs) and stops at its first red; its write
// and ktest steps belong to the phase, its install needs --bundle (the desk's KERNEL and INITRD). A step that needs
// Georg (`ask`: his yes to the tone, his keys) holds the phase (exit 3) until it is started again with --from that
// step (the tone also with --sound), after the session asked him.
// Exit 0: every judged step is green; 1: a step is red (its answer is in the out folder); 2: BitsOS could not be
// asked at all, or the arguments are wrong; 3: the phase waits for Georg (run it again with --from the named step).
import { spawn } from 'node:child_process';
import { mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { connect, McpError, readToken } from '../mcp/mcp-call.mjs';
import { isMain } from '../mcp/is-main.mjs';
import { tokenFileFor } from '../mcp/token.mjs';
import * as ph from './phase.mjs';

const HERE = dirname(fileURLToPath(import.meta.url));

// One want/refuse item: a string anywhere in the answer, or a list of parts that must all stand on one line, in
// this order (a real line carries more fields between them: GP-R2, step 28 of the round).
export function holds(item, text) {
  if (typeof item === 'string') return text.includes(item);
  return text.split(/\r?\n/).some((line) => {
    let at = 0;
    for (const part of item) {
      const i = line.indexOf(part, at);
      if (i < 0) return false;
      at = i + part.length;
    }
    return true;
  });
}

const show = (item) => (typeof item === 'string' ? item : item.join(' ... '));

// The verdict of one answer against a step: [] when green, else what is wrong.
export function judge(step, isError, text) {
  const bad = [];
  if (step.expect_error) {
    if (!isError) bad.push('the call should have been refused, and it was not');
    return bad;
  }
  if (isError) bad.push('the tool answered with an error');
  for (const w of step.want ?? []) if (!holds(w, text)) bad.push(`missing: ${show(w)}`);
  for (const r of step.refuse ?? []) if (holds(r, text)) bad.push(`must not appear: ${show(r)}`);
  return bad;
}

// A fact of the machine from one read answer: true when its word stands there as a whole setting (or, for a fact
// with `key`, when that key stands there with a value: `gp_nvme=81:00.0/...`, never the empty `gp_nvme=`; for one
// with `has`, when that text stands there), false when the answer holds the line but not that, null when the line
// the fact is read from is not in the answer.
export function readFact(fact, isError, text) {
  if (isError || !text.includes(fact.line)) return null;
  if (fact.has) return text.includes(fact.has);
  const esc = (w) => w.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
  if (fact.key) return new RegExp(`(^|[\\s"'])${esc(fact.key)}=[^\\s"']`, 'm').test(text);
  return new RegExp(`(^|[\\s"'])${esc(fact.word)}($|[\\s"'])`, 'm').test(text);
}

// The step as the machine's facts make it: `when` holds overrides per fact ("gp_nvme_named" when it holds,
// "!gp_nvme_named" when not); `want+`/`refuse+` add to the lists, other fields
// replace. An unknown fact applies neither override: the step's own lists are still judged (a real red stays red) and `partly` names what was not;
// only a step whose whole meaning hangs on the fact says so with its `unknown` fields (e.g. judge: false).
export function shape(step, facts) {
  const s = { ...step };
  for (const [key, over] of Object.entries(step.when ?? {})) {
    const name = key.replace(/^!/, '');
    const v = facts[name];
    if (v === null || v === undefined) {
      const gone = `${name} is not known (the line it is read from is not in the kernel log any more)`;
      if (step.unknown) Object.assign(s, { why: gone }, step.unknown);
      else s.partly = `${gone}: what depends on it was not judged`;
      continue;
    }
    if (v !== !key.startsWith('!')) continue;
    for (const [k, val] of Object.entries(over)) {
      if (k === 'want+' || k === 'refuse+') s[k.slice(0, -1)] = [...(s[k.slice(0, -1)] ?? []), ...val];
      else s[k] = val;
    }
  }
  return s;
}

export function list(file = join(HERE, 'checks.json')) {
  return JSON.parse(readFileSync(file, 'utf8').replace(/^﻿/, ''));
}

export function steps(file) {
  return list(file).steps;
}

// What reload_info says about this boot's confirmation (crates/kexec/src/install/confirm.rs `describe`): 'pending',
// 'writing' (proven, OK going onto the stick), 'confirmed', 'none' (nothing to confirm) or 'other' (failed,
// replaced, unknown), with the generation when one is named.
export function confirmState(text) {
  let m = /generation (\d+) waits for its confirmation/.exec(text);
  if (m) return { state: 'pending', generation: Number(m[1]) };
  m = /generation (\d+): writing OK to the stick/.exec(text);
  if (m) return { state: 'writing', generation: Number(m[1]) };
  m = /generation (\d+) confirmed \(OK\) in this boot/.exec(text);
  if (m) return { state: 'confirmed', generation: Number(m[1]) };
  if (/nothing to confirm/.test(text)) return { state: 'none' };
  return { state: 'other' };
}

// The heartbeat: one reload_info every `everyMs` while the steps run, never two at once. Each answered call is an
// answer the kernel's proof counts. `seen(text)` gets each answer.
export function heartbeat(session, everyMs, seen = () => {}) {
  let busy = false, stopped = false;
  const hb = { beats: 0, misses: 0 };
  const beat = async () => {
    if (busy || stopped) return;
    busy = true;
    try {
      const r = await session.call('reload_info', {}, Math.max(10000, everyMs * 2));
      hb.beats++;
      if (!r.isError) seen(r.text);
    } catch (e) {
      if (!(e instanceof McpError)) throw e;
      hb.misses++;
    } finally {
      busy = false;
    }
  };
  const timer = setInterval(beat, everyMs);
  hb.stop = () => { stopped = true; clearInterval(timer); };
  return hb;
}


// Where a keys step's held run keeps the event sequence number of the moment Georg was asked.
const keysKeep = (id) => `keys-${id.toLowerCase()}`;
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const waiting = (c) => c.state === 'pending' || c.state === 'writing';

// One MCP call that never throws (an error answer says why).
async function ask(session, tool, args, ms = 120000) {
  try {
    return await session.call(tool, args, ms);
  } catch (e) {
    if (!(e instanceof McpError)) throw e;
    return { isError: true, text: `(no answer: ${e.message})` };
  }
}

// The machine's facts (checks.json "facts"), each printed.
async function readFacts(session, checks, out, say) {
  const facts = {};
  for (const f of checks.facts ?? []) {
    const r = await ask(session, f.tool, f.args);
    facts[f.name] = readFact(f, r.isError, r.text);
    writeFileSync(join(out, `fact-${f.name}.txt`), `${f.tool} ${JSON.stringify(f.args)}\n${r.text}\n`);
    say(`fact ${f.name}=${facts[f.name] ?? 'unknown'} (${f.about})`);
  }
  return facts;
}

// A link to BitsOS that survives a reboot: the session, its heartbeat and what reload_info said last.
class Link {
  constructor(host, token, everyS, say) {
    Object.assign(this, { host, token, everyS, say, confirm: { state: 'other' } });
  }
  async open(waitMs = 0) {
    const end = Date.now() + waitMs;
    for (;;) {
      try {
        this.session = await connect(this.host, { token: this.token, ms: 60000 });
        break;
      } catch (e) {
        if (!(e instanceof McpError) || Date.now() >= end) throw e;
        await sleep(2000);
      }
    }
    this.hb = heartbeat(this.session, this.everyS * 1000, (t) => {
      const c = confirmState(t);
      if (c.state === 'confirmed' && waiting(this.confirm)) this.say(`ok   heartbeat: generation ${c.generation} confirmed itself while the steps ran`);
      this.confirm = c;
    });
    const first = await ask(this.session, 'reload_info', {});
    if (!first.isError) this.confirm = confirmState(first.text);
    else this.say(`note confirm: reload_info did not answer (${first.text}); the steps run anyway`);
    if (this.confirm.state === 'pending') this.say(`note generation ${this.confirm.generation} waits for its confirmation: the heartbeat (every ${this.everyS} s) keeps MCP answering`);
  }
  close() {
    this.hb?.stop();
  }
  // Until the running generation confirmed itself (or has nothing to confirm), at most `waitS`.
  async confirmed(waitS) {
    const end = Date.now() + waitS * 1000;
    while (waiting(this.confirm) && Date.now() < end) await sleep(1000);
    return !waiting(this.confirm) && this.confirm.state !== 'other';
  }
}

// install.mjs as a child (the desk's bundle, --upload: no port opens on this machine); its output is the answer.
function runInstall(step, host, bundle, token) {
  return new Promise((done) => {
    const p = spawn(process.execPath, ph.installArgv(step, host, bundle), { env: { ...process.env, BITSOS_TOKEN: token } });
    let text = '';
    const take = (d) => { text += d; process.stdout.write(`       | ${String(d).replace(/\n(?=.)/g, '\n       | ')}`); };
    p.stdout.on('data', take);
    p.stderr.on('data', take);
    p.on('close', (code) => done({ isError: code !== 0, text: `${text}\n(install.mjs exit ${code})` }));
  });
}

// One step: its answer and what is wrong with it ([] = green), or `{ judged: false, why }`.
async function runStep(s, link, ctx) {
  const save = (text, suffix = '') => writeFileSync(join(ctx.out, `${s.id}${suffix}.txt`), `${s.tool ?? s.kind} ${JSON.stringify(s.args ?? {})}\n${text}\n`);
  if (s.wait_s) { ctx.say(`wait ${s.id}: ${s.wait_s} s first (MCP kept answering)`); await sleep(s.wait_s * 1000); }
  if (s.kind === 'confirm') {
    const ok = await link.confirmed(ctx.waitS);
    save(JSON.stringify(link.confirm));
    return { bad: ok ? [] : [`reload_info says ${link.confirm.state} after ${ctx.waitS} s`] };
  }
  if (s.kind === 'install') {
    if (!ctx.bundle) return { judged: false, why: 'no --bundle given (an optional install)' };
    const r = await runInstall(s, link.host, ctx.bundle, link.token);
    save(r.text);
    if (s.reboot && !r.isError) {
      link.close();
      ctx.say(`note ${s.id}: BitsOS restarted into the new generation: a new connection, the facts again`);
      await link.open(240000);
      Object.assign(ctx.facts, await readFacts(link.session, ctx.checks, ctx.out, ctx.say));
    }
    return { bad: judge(s, r.isError, r.text) };
  }
  if (s.kind === 'keys') {
    const a = await ask(link.session, 'input', { max: 50 });
    const now = ph.nextSeq(a.text);
    save(a.text, '-before');
    if (now === null) return { bad: ['input names no next_seq'] };
    // The held run kept where the events stood when Georg was asked: keys he typed before this run count. A kept
    // number above today's belongs to an earlier boot.
    const was = Number((ph.kept(keysKeep(s.id)) ?? [])[0]);
    let since = Number.isInteger(was) && was <= now ? was : now;
    ctx.say(`keys ${s.id}: waiting up to ${s.wait_s_keys ?? 120} s for Georg's keys (events from ${since} on)`);
    let seen = { devices: [], codes: [] }, text = '';
    const end = Date.now() + (s.wait_s_keys ?? 120) * 1000;
    while (Date.now() < end && !s.keys.codes.every((c) => seen.codes.includes(c))) {
      const wait = Math.max(1, Math.min(30000, end - Date.now()));
      const r = await ask(link.session, 'input', { since, wait_ms: wait, max: 400 }, wait + 30000);
      if (r.isError) break;
      text += `${r.text}\n`;
      // Each call asks only for what came after the last one, so it really waits for the next key.
      since = Math.max(since, ph.lastSeq(r.text) + 1);
      seen = ph.keysSeen(text, s.keys.id);
    }
    save(text);
    const missing = s.keys.codes.filter((c) => !seen.codes.includes(c));
    return { bad: missing.length ? [`no key press of ${s.keys.id} for code(s) ${missing.join(', ')} (devices ${seen.devices.join(',') || 'none'} of that id)`] : [] };
  }
  const r = await ask(link.session, s.tool, s.args, s.kind === 'ktest' ? 900000 : 120000);
  save(r.text);
  const bad = judge(s, r.isError, r.text);
  if (s.twice && !r.isError) {
    await sleep(s.twice.after_s * 1000);
    const r2 = await ask(link.session, s.tool, s.args);
    save(r2.text, '-again');
    const [a, b] = [ph.number(r.text, s.twice.rises), ph.number(r2.text, s.twice.rises)];
    if (a === null || b === null || !(b > a)) bad.push(`the count did not rise in ${s.twice.after_s} s (${a} -> ${b})`);
  }
  if (s.keep && !r.isError) ctx.say(`kept ${s.keep.name}: ${ph.lines(r.text, s.keep.lines).length} line(s) in ${ph.keep(s.keep.name, ph.lines(r.text, s.keep.lines))}`);
  if (s.same && !r.isError) {
    const was = ph.kept(s.same.name), now = ph.lines(r.text, s.same.lines);
    if (!was) return { bad, partly: `nothing kept as ${s.same.name} yet: compare by eye` };
    if (was.join('\n') !== now.join('\n')) bad.push(`differs from what ${s.same.name} kept (${was.length} line(s) then, ${now.length} now)`);
  }
  return { bad };
}

function parse(argv) {
  const opts = { write: false, ktest: false, sound: false }, rest = [];
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i];
    if (a === '--write' || a === '--ktest' || a === '--sound') opts[a.slice(2)] = true;
    else if (['--token-file', '--out', '--only', '--heartbeat-s', '--confirm-wait-s', '--phase', '--from', '--bundle', '--checks'].includes(a)) opts[a.slice(2)] = argv[++i];
    else rest.push(a);
  }
  return { opts, rest };
}

async function main(argv) {
  const { opts, rest } = parse(argv);
  const [host] = rest;
  const num = (k, d) => (opts[k] === undefined ? d : /^\d+$/.test(opts[k]) ? Number(opts[k]) : NaN);
  const everyS = num('heartbeat-s', 10), waitS = num('confirm-wait-s', 200);
  if (!host || rest.length > 1 || !(everyS >= 1) || Number.isNaN(waitS)) {
    console.error('usage: node host/remote-desk/collect.mjs <host[:port]> [--token-file <path>] [--out <dir>] [--only <id prefixes, comma-separated>] [--write] [--ktest] [--heartbeat-s N (1 or more)] [--confirm-wait-s N] [--phase <id> [--from <step id>] [--bundle <dir>] [--sound]]');
    return 2;
  }
  const checks = list(opts.checks ? resolve(opts.checks) : undefined);
  let p = null;
  if (opts.phase) {
    p = ph.phase(checks, opts.phase);
    if (!p) { console.error(`no phase ${opts.phase}; the phases are ${ph.phaseIds(checks).join(', ')}`); return 2; }
    if (ph.needs(p).bundle && !opts.bundle) { console.error(`phase ${p.id} installs the desk's bundle: --bundle <folder with KERNEL and INITRD>`); return 2; }
    if (opts.from && !p.steps.some((s) => s.id === opts.from)) { console.error(`phase ${p.id} has no step ${opts.from}`); return 2; }
  } else if (opts.from || opts.bundle || opts.sound) {
    console.error('--from, --bundle and --sound belong to --phase'); return 2;
  }
  const stamp = new Date().toISOString().replace(/[:.]/g, '-').slice(0, 19);
  const out = resolve(opts.out ?? join(HERE, 'out', `${stamp}${p ? `-phase-${p.id}` : ''}`));
  mkdirSync(out, { recursive: true });
  const say = (t) => console.log(t);
  let link;
  try {
    link = new Link(host, readToken(tokenFileFor(opts['token-file'], true)), everyS, say);
    await link.open();
  } catch (e) {
    if (!(e instanceof McpError)) throw e;
    console.error(e.message);
    return 2;
  }
  let red = 0, ran = 0, held = false;
  try {
    const facts = await readFacts(link.session, checks, out, say);
    const ctx = { out, say, facts, checks, bundle: opts.bundle && resolve(opts.bundle), waitS };
    let steps = checks.steps;
    if (p) {
      say(`phase ${p.id}: ${p.title}`);
      for (const l of p.before ?? []) say(`  before: ${l}`);
      for (const l of p.never ?? []) say(`  never:  ${l}`);
      const problems = ph.requireProblems(p, facts);
      if (problems.length) {
        for (const m of problems) say(`RED  phase ${p.id}: ${m}`);
        return 1;
      }
      steps = opts.from ? p.steps.slice(p.steps.findIndex((s) => s.id === opts.from)) : p.steps;
    }
    for (const raw of steps) {
      if (opts.only && !opts.only.split(',').some((x) => raw.id.startsWith(x))) continue;
      if (!p && ((raw.kind === 'write' && !opts.write) || (raw.kind === 'ktest' && !opts.ktest))) continue;
      // A step that needs Georg (his yes, his hands) holds the phase until the session asked him and starts it
      // again from there; the tone needs --sound on top.
      if (raw.ask && !(opts.from === raw.id && (raw.kind !== 'sound' || opts.sound))) {
        if (raw.kind === 'keys') {
          const seq = ph.nextSeq((await ask(link.session, 'input', { max: 1 })).text);
          if (seq !== null) ph.keep(keysKeep(raw.id), [String(seq)]);
        }
        say(`ASK  ${raw.id}: ${raw.ask}`);
        say(raw.kind === 'keys'
          ? `     then: tell Georg and at once the same command with --from ${raw.id} (do not wait for his answer: his keys count from now on)`
          : `     then: the same command with ${raw.kind === 'sound' ? '--sound ' : ''}--from ${raw.id}`);
        held = true;
        break;
      }
      const s = ph.withPlus(shape(raw, facts));
      const r = await runStep(s, link, ctx);
      ran++;
      const look = (s.partly || r.partly ? `\n       not judged: ${s.partly ?? r.partly}` : '') + (s.note ? `\n       look: ${s.note}` : '');
      if (s.judge === false || r.judged === false) {
        say(`note ${s.id} ${s.tool ?? s.kind}: not judged: ${s.why ?? r.why}${look}`);
        continue;
      }
      if (r.bad.length) red++;
      say(`${r.bad.length ? 'RED ' : 'ok  '} ${s.id} ${s.tool ?? s.kind}${r.bad.length ? `: ${r.bad.join('; ')}` : ''}${look}`);
      if (r.bad.length && p) {
        say(`STOP phase ${p.id} at ${s.id}: ${s.red ?? 'the answer is in the out folder; tell the desk (Test | cause | fix)'}`);
        for (const l of p.never ?? []) say(`  never:  ${l}`);
        break;
      }
    }
    if (waiting(link.confirm) && waitS > 0) {
      const t0 = Date.now();
      say(`wait generation ${link.confirm.generation}: keeping MCP answering until it confirms itself (at most ${waitS} s)`);
      if (await link.confirmed(waitS)) say(`ok   confirm: generation ${link.confirm.generation} confirmed (OK)`);
      else {
        red++;
        say(`RED  confirm: reload_info says ${link.confirm.state} after ${Math.round((Date.now() - t0) / 1000)} s (heartbeats ${link.hb.beats}, missed ${link.hb.misses}); run reload_info by hand`);
      }
    } else if (waiting(link.confirm)) {
      say(`note confirm: generation ${link.confirm.generation} still waits (--confirm-wait-s 0): keep MCP answering, or it falls back at the next start`);
    }
    if (p && !red && !held) for (const l of p.after ?? []) say(`  after:  ${l}`);
  } finally {
    link.close();
  }
  say(`${ran} steps, ${red} red; heartbeats ${link.hb.beats} (missed ${link.hb.misses}); the answers are in ${out}`);
  return red ? 1 : held ? 3 : 0;
}

if (isMain(import.meta.url)) process.exitCode = await main(process.argv.slice(2));
