// Unit test of host/mcp/push-plan.mjs (MC-push), no BitsOS: package names, FAT name rules, the local scan
// (SHA-256, links, case collisions), the plan (unchanged / upload / reuse of staged files / stale / removals,
// type changes, FAT's case folding) and the final comparison; MC-push2: home only with --home (allowHome),
// --file's target (fileTarget) and plans that never remove (keep), a comparison that lets extra files be.
//
//   node host/tests/mcp/push_plan.mjs   -> "push-plan: N of N ok", exit 0 (1 on the first failure)
import { createHash } from 'node:crypto';
import { mkdirSync, mkdtempSync, rmSync, symlinkSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { compare, fatProblem, fileTarget, isHome, journalProblem, makePlan, nameProblem, scanTree } from '../../mcp/push-plan.mjs';

let n = 0;
function ok(cond, what) {
  n++;
  if (!cond) { console.error(`push-plan: FAIL ${what}`); process.exit(1); }
}
const sha = (s) => createHash('sha256').update(s).digest('hex');
const file = (path, s) => ({ path, type: 'file', size: Buffer.byteLength(s), sha256: sha(s) });
const dirE = (path) => ({ path, type: 'directory' });

for (const good of ['cobalt', 'bin', 'my_pkg-2.0']) ok(nameProblem(good) === null, `name ${good}`);
for (const bad of ['SYS', 'sys.', 'Config.TXT', 'STICK.ID', 'home', 'EFI', '.push-x', '-x', 'a/b', '', 'x'.repeat(65)]) ok(nameProblem(bad) !== null, `name ${bad} refused`);
for (const good of ['a/b.txt', 'lib/libcobalt.so', '.hidden']) ok(fatProblem(good) === null, `fat ${good}`);
for (const bad of ['a/b:c', 'x?', 'dir./x', 'x ', 'a//b', '../x', 'a\\b', 'x'.repeat(256)]) ok(fatProblem(bad) !== null, `fat ${bad} refused`);

const tmp = mkdtempSync(join(process.env.BITSOS_RUN_DIR ?? tmpdir(), 'mcpush-plan-'));
try {
  mkdirSync(join(tmp, 't', 'lib'), { recursive: true });
  writeFileSync(join(tmp, 't', 'lib', 'a.so'), 'AAA');
  writeFileSync(join(tmp, 't', 'run'), 'R');
  const local = await scanTree(join(tmp, 't'));
  ok(local.files.map((f) => f.rel).join(',') === 'lib/a.so,run' && local.dirs.join(',') === 'lib', 'scan order');
  ok(local.files[0].sha256 === sha('AAA') && local.files[0].size === 3, 'scan sha256');
  symlinkSync('run', join(tmp, 't', 'link'));
  let err = '';
  try { await scanTree(join(tmp, 't')); } catch (e) { err = e.message; }
  ok(/link: a symbolic link/.test(err), `link refused: ${err}`);
  rmSync(join(tmp, 't', 'link'));
  writeFileSync(join(tmp, 't', 'RUN'), 'r');
  err = '';
  try { await scanTree(join(tmp, 't')); } catch (e) { err = e.message; }
  ok(/one name on FAT/.test(err), `case collision refused: ${err}`);
} finally {
  rmSync(tmp, { recursive: true, force: true });
}

// The plan: local has keep (same), lib/a.so (changed), new/n (new; staged right already), b (file where the stick
// has a directory b/), c/ (directory where the stick has a file c).
const L = (rel, s) => ({ rel, size: Buffer.byteLength(s), sha256: sha(s) });
const local = { files: [L('Keep', 'k'), L('b', 'B'), L('c/x', 'cx'), L('lib/a.so', 'A2'), L('new/n', 'N')], dirs: ['c', 'lib', 'new'] };
const remote = [file('KEEP', 'k'), dirE('b'), file('b/old', 'o'), file('c', 'C'), dirE('gone'), file('gone/g1', 'g'), dirE('gone/sub'), file('gone/sub/g2', 'g'), dirE('lib'), file('lib/a.so', 'A1'), file('lib/old.so', 'O')];
const staged = [file('new/n', 'N'), file('lib/a.so', 'A0'), file('stale.bin', 'x'), file('lib/.a.so.mcp-3~', 'part'), dirE('lib'), dirE('new')];
const p = makePlan(local, remote, staged);
ok(p.same === 1, `same ${p.same} (KEEP matches Keep: FAT folds case)`);
ok(p.upload.map((f) => f.rel).join(',') === 'b,c/x,lib/a.so', `upload ${p.upload.map((f) => f.rel)}`);
ok(p.reuse.map((f) => f.rel).join(',') === 'new/n', `reuse ${p.reuse.map((f) => f.rel)}`);
ok(p.stale.sort().join(',') === 'lib/.a.so.mcp-3~,lib/a.so,stale.bin', `stale ${p.stale}`);
ok(p.remove.join(',') === 'gone,lib/old.so', `remove ${p.remove} (top-most only; nothing below b/ which becomes a file; c is replaced by the move)`);
ok(p.bytes === 1 + 2 + 2, `bytes ${p.bytes}`);
const none = makePlan(local, [...local.files.map((f) => ({ path: f.rel, type: 'file', size: f.size, sha256: f.sha256 })), dirE('c'), dirE('lib'), dirE('new')], []);
ok(none.upload.length + none.reuse.length + none.remove.length + none.stale.length === 0 && none.same === 5, 'up to date');

const after = local.files.map((f) => ({ path: f.rel.toUpperCase(), type: 'file', size: f.size, sha256: f.sha256 }));
ok(compare(local, after).length === 0, 'compare equal (case folded)');
ok(compare(local, [...after.slice(1), file('extra', 'e')]).join('|') === 'Keep: missing|extra: on BitsOS only', `compare ${compare(local, [...after.slice(1), file('extra', 'e')])}`);

// MC-push2: the home only with --home, by any spelling FAT folds to it; the other reserved names never.
for (const h of ['home', 'Home.', 'HOME']) {
  ok(isHome(h) && nameProblem(h, { allowHome: true }) === null, `${h} allowed with --home`);
  ok(/only with --home/.test(nameProblem(h) ?? ''), `${h} refused without --home`);
}
for (const bad of ['SYS', 'config.txt', 'EFI', '.push-home']) ok(nameProblem(bad, { allowHome: true }) !== null, `${bad} refused also with --home`);
ok(!isHome('homes') && nameProblem('homes') === null, 'homes is a package');
// --file --to: <root>/<package>/<path>.
const R = '/boot/BITSOS2';
const ft = fileTarget(`${R}/bin/cobalt`, R);
ok(ft.pkg === 'bin' && ft.dir === `${R}/bin` && ft.name === 'cobalt', `fileTarget ${JSON.stringify(ft)}`);
const deep = fileTarget(`${R}/cobalt/lib/x.so`, R);
ok(deep.pkg === 'cobalt' && deep.dir === `${R}/cobalt/lib` && deep.name === 'x.so', `fileTarget deep ${JSON.stringify(deep)}`);
const tmpT = fileTarget('/tmp/t1/pkg/f', '/tmp/t1');
ok(tmpT.dir === '/tmp/t1/pkg' && tmpT.name === 'f', `fileTarget tmp ${JSON.stringify(tmpT)}`);
for (const bad of [`${R}/x`, `${R}/SYS/x`, `${R}/config.txt/x`, `${R}/.push-bin/x`, '/boot/x/y', '/tmp/t1/pkg/f', `${R}/bin/../x`, `${R}/bin/a:b`, `${R}/bin/`, `${R}//x`, `${R}/home/.cobalt/x`]) {
  ok(fileTarget(bad, R).problem, `fileTarget ${bad} refused`);
}
const ht = fileTarget(`${R}/Home/.cobalt/x`, R, { allowHome: true });
ok(ht.pkg === 'Home' && ht.dir === `${R}/Home/.cobalt` && ht.name === 'x', `fileTarget home with --home ${JSON.stringify(ht)}`);
// keep: nothing removed, the would-be removals counted.
const kp = makePlan(local, remote, staged, { keep: true });
ok(kp.remove.length === 0 && kp.kept === 2 && kp.upload.length === p.upload.length, `keep: remove ${kp.remove} kept ${kp.kept}`);
ok(p.kept === 0, 'kept 0 without keep');
// A single-file plan (--file): the one file new, then the same.
const one = { files: [L('cobalt', '#!/bin/sh')], dirs: [] };
const fresh = makePlan(one, [], [], { keep: true });
ok(fresh.upload.length === 1 && fresh.remove.length === 0 && fresh.same === 0, 'one file new');
const sameOne = makePlan(one, [file('cobalt', '#!/bin/sh')], [], { keep: true });
ok(sameOne.upload.length + sameOne.reuse.length + sameOne.remove.length === 0 && sameOne.same === 1, 'one file up to date');
const changed = makePlan(one, [file('Cobalt', 'old')], [file('cobalt', '#!/bin/sh')], { keep: true });
ok(changed.reuse.length === 1 && changed.upload.length === 0 && changed.stale.length === 0, 'one file staged already');
// extraOk (--home): files only on BitsOS are fine, a missing or different one is not.
ok(compare(local, [...after, file('extra', 'e')], { extraOk: true }).length === 0, 'compare extraOk');
ok(compare(local, [...after.slice(1), file('extra', 'e')], { extraOk: true }).join('|') === 'Keep: missing', 'compare extraOk still finds a missing file');
// journalProblem: an interrupted commit goes only to its package or below (a damaged PLAN.json elsewhere is refused).
ok(journalProblem({ remove: ['a/b'] }, R, 'bin') === null, 'journal without to (the package)');
ok(journalProblem({ to: `${R}/bin`, remove: [] }, R, 'bin') === null, 'journal to the package');
ok(journalProblem({ to: `${R}/Home/.cobalt`, remove: [] }, R, 'home') === null, 'journal to a folder of the home');
for (const to of [`${R}/bin/../sys`, `${R}/bin/./x`, `${R}/bin//x`, `${R}/bin/x\\..\\y`, `${R}/sys`, `${R}/binx`, `${R}`, '/boot/other/bin', 7]) {
  ok(journalProblem({ to, remove: [] }, R, 'bin'), `journal to ${to} refused`);
}
for (const r of ['../x', 'a/../../b', '', 'a//b', 'a\\b', 3]) ok(journalProblem({ remove: [r] }, R, 'bin'), `journal removal ${r} refused`);
ok(journalProblem({ to: `${R}/bin` }, R, 'bin'), 'journal without a removal list refused');
console.log(`push-plan: ${n} of ${n} ok`);
