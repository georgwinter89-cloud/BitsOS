// push-plan.mjs (V2, MC-push): the pure half of push.mjs (no network): the local package tree with SHA-256 per
// file, the rules for a package name and for names FAT can hold, and the plan (what to upload, what to remove,
// which staged files are reusable). host/tests/mcp/push_plan.mjs tests it.
import { createHash } from 'node:crypto';
import { createReadStream, lstatSync, readdirSync } from 'node:fs';
import { join } from 'node:path';

// Names directly below \BITSOS2\ a push never targets: the generations, Georg's settings, the stick's identity,
// the user's home (YT-2) and anything hidden (staging trees are .push-<name>).
export const RESERVED = ['sys', 'config.txt', 'stick.id', 'home', 'efi'];
// FAT keeps files below 4 GiB (linux-6.6/fs/fat/inode.c: s_maxbytes 0xFFFFFFFF).
export const FAT_MAX = 0xFFFFFFFF;

// FAT compares names without case and without trailing dots/spaces.
export const fold = (p) => p.split('/').map((c) => c.replace(/[. ]+$/, '').toLowerCase()).join('/');

// Whether a package name is the user's home (by FAT's name rule: "Home." is home too).
export const isHome = (name) => fold(name) === 'home';

// Why a package name is refused (null = fine). allowHome (push.mjs --home, MC-push2): home is allowed.
export function nameProblem(name, { allowHome = false } = {}) {
  if (!/^[A-Za-z0-9_][A-Za-z0-9._-]{0,63}$/.test(name)) return `package name "${name}": letters, digits, . _ - only, not starting with . or -, at most 64`;
  if (allowHome && isHome(name)) return null;
  if (isHome(name)) return `package name "${name}": \\BITSOS2\\home is the user's home, pushed only with --home (it adds and replaces files there, never removes any)`;
  if (RESERVED.includes(fold(name))) return `package name "${name}": \\BITSOS2\\${name} is not a package (SYS, CONFIG.TXT, STICK.ID, home and EFI are never pushed)`;
  return null;
}

// push.mjs --file LOCAL --to PATH (MC-push2): where one file goes. PATH is <root>/<package>/<relative path>;
// -> { pkg, dir, name } (dir: the directory that receives it, name: its file name) or { problem }.
export function fileTarget(to, root, { allowHome = false } = {}) {
  if (typeof to !== 'string' || !to.startsWith(`${root}/`)) return { problem: `--to ${to}: a path below ${root}/` };
  const rel = to.slice(root.length + 1);
  const bad = fatProblem(rel);
  if (bad) return { problem: `--to ${to}: ${bad}` };
  const parts = rel.split('/');
  if (parts.length < 2) return { problem: `--to ${to}: <package>/<file> below ${root}/ (a file directly in ${root} is never pushed)` };
  const np = nameProblem(parts[0], { allowHome });
  if (np) return { problem: `--to ${to}: ${np}` };
  return { pkg: parts[0], dir: `${root}/${parts.slice(0, -1).join('/')}`, name: parts[parts.length - 1] };
}

// Why an interrupted commit's journal (PLAN.json in the staging tree, on the stick) cannot be finished (null =
// fine): `to` must be the package directory or below it, with no empty, . or .. part and no backslash (written by
// fileTarget, so it passes fatProblem; older journals carry no `to` and mean the package), and every removal a
// relative path without such parts. A damaged or hand-edited journal must not send the commit elsewhere.
export function journalProblem(plan, root, name) {
  if (!plan || typeof plan !== 'object' || !Array.isArray(plan.remove)) return 'no removal list';
  const to = plan.to ?? `${root}/${name}`;
  if (typeof to !== 'string' || !to.startsWith(`${root}/`)) return `to ${JSON.stringify(to)}: not below ${root}/`;
  const rel = to.slice(root.length + 1);
  const bad = fatProblem(rel);
  if (bad) return `to ${to}: ${bad}`;
  if (fold(rel.split('/')[0]) !== fold(name)) return `to ${to}: not in the package ${name}`;
  for (const r of plan.remove) {
    if (typeof r !== 'string' || r.split('/').some((c) => !c || c === '.' || c === '..' || c.includes('\\'))) return `removal ${JSON.stringify(r)}: not a relative path`;
  }
  return null;
}

// Why a relative path cannot go onto FAT (null = fine): vfat's invalid characters (linux-6.6/fs/fat/namei_vfat.c
// vfat_bad_char: control characters and "*/:<>?\|), names ending in a dot or space, more than 255 UTF-16 units.
export function fatProblem(rel) {
  for (const c of rel.split('/')) {
    if (!c || c === '.' || c === '..') return `${rel}: an empty, . or .. part`;
    if (/[\x00-\x1f"*:<>?\\|]/.test(c)) return `${rel}: "${c}" has a character FAT cannot hold (control characters and "*:<>?\\|)`;
    if (/[. ]$/.test(c)) return `${rel}: "${c}" ends in a dot or space (FAT drops those)`;
    if (c.length > 255) return `${rel}: "${c.slice(0, 20)}..." is longer than FAT's 255 characters`;
  }
  return null;
}

export function sha256File(path) {
  return new Promise((done, failed) => {
    const h = createHash('sha256');
    createReadStream(path).on('data', (d) => h.update(d)).on('error', failed).on('end', () => done(h.digest('hex')));
  });
}

// The local tree: { files: [{ rel, abs, size, sha256 }], dirs: [rel] }, sorted by rel; throws on links, devices,
// FAT-impossible names, files of 4 GiB or more and names that differ only in case.
export async function scanTree(root) {
  const files = [], dirs = [], problems = [];
  const walk = (rel) => {
    for (const name of readdirSync(join(root, rel)).sort()) {
      const r = rel ? `${rel}/${name}` : name;
      const abs = join(root, r);
      const st = lstatSync(abs);
      const bad = fatProblem(r);
      if (bad) problems.push(bad);
      else if (st.isDirectory()) { dirs.push(r); walk(r); }
      else if (st.isFile()) {
        if (st.size > FAT_MAX) problems.push(`${r}: ${st.size} bytes, FAT holds less than 4 GiB per file`);
        files.push({ rel: r, abs, size: st.size });
      } else problems.push(`${r}: ${st.isSymbolicLink() ? 'a symbolic link' : 'no regular file'} (FAT holds files and directories only)`);
    }
  };
  walk('');
  const seen = new Map();
  for (const r of [...files.map((f) => f.rel), ...dirs]) {
    const k = fold(r);
    if (seen.has(k)) problems.push(`${seen.get(k)} and ${r} are one name on FAT (case)`);
    seen.set(k, r);
  }
  if (problems.length) throw new Error(problems.join('\n'));
  for (const f of files) f.sha256 = await sha256File(f.abs);
  const by = (a, b) => (Buffer.compare(Buffer.from(a), Buffer.from(b)));
  files.sort((a, b) => by(a.rel, b.rel));
  return { files, dirs: dirs.sort(by) };
}

// The plan from the local tree and two manifests (entries as `manifest` answers them: path, type, size, sha256):
// remote = the package on the stick, staged = the staging tree's files/. keep (--home, --file): nothing is
// removed; what would have been is counted in `kept`.
// -> { upload: [file], reuse: [file], stale: [staged rel], remove: [remote rel], kept: n, same: n, bytes }
export function makePlan(local, remote, staged, { keep = false } = {}) {
  const rmap = new Map(remote.map((e) => [fold(e.path), e]));
  const smap = new Map(staged.map((e) => [e.path, e]));
  const need = [], reuse = [], upload = [];
  let same = 0;
  for (const f of local.files) {
    const r = rmap.get(fold(f.rel));
    if (r && r.type === 'file' && r.size === f.size && r.sha256 === f.sha256) same++;
    else need.push(f);
  }
  const needRel = new Set(need.map((f) => f.rel));
  for (const f of need) {
    const s = smap.get(f.rel);
    if (s && s.type === 'file' && s.size === f.size && s.sha256 === f.sha256) reuse.push(f);
    else upload.push(f);
  }
  // Staged entries that are not a needed file as it is now (older sources, temp files of a cut fetch). Their
  // directories go with the staging tree's own removal or stay harmless (move_tree moves only what is there).
  const stale = staged.filter((e) => e.type !== 'directory' && !(needRel.has(e.path) && reuse.some((f) => f.rel === e.path))).map((e) => e.path);
  // Remote entries the package no longer has: the top-most of each removed subtree, never below a path that
  // becomes a file (its whole directory is replaced by the move).
  const localKeys = new Set([...local.files.map((f) => fold(f.rel)), ...local.dirs.map(fold)]);
  const localFiles = new Set(local.files.map((f) => fold(f.rel)));
  const remove = [];
  const removedKeys = new Set();
  for (const e of [...remote].sort((a, b) => Buffer.compare(Buffer.from(a.path), Buffer.from(b.path)))) {
    const k = fold(e.path);
    if (localKeys.has(k)) continue;
    const parts = k.split('/');
    const ancestors = parts.slice(0, -1).map((_, i) => parts.slice(0, i + 1).join('/'));
    if (ancestors.some((a) => removedKeys.has(a) || localFiles.has(a))) continue;
    remove.push(e.path);
    removedKeys.add(k);
  }
  const kept = keep ? remove.length : 0;
  return { upload, reuse, stale, remove: keep ? [] : remove, kept, same, bytes: upload.reduce((n, f) => n + f.size, 0) };
}

// Differences between the local tree and a manifest of the copy (after the commit): [] when equal. extraOk
// (--home): files only on BitsOS are no difference.
export function compare(local, entries, { extraOk = false } = {}) {
  const m = new Map(entries.filter((e) => e.type === 'file').map((e) => [fold(e.path), e]));
  const out = [];
  for (const f of local.files) {
    const e = m.get(fold(f.rel));
    if (!e) out.push(`${f.rel}: missing`);
    else if (e.size !== f.size || e.sha256 !== f.sha256) out.push(`${f.rel}: ${e.size} bytes sha256 ${e.sha256}, here ${f.size} bytes ${f.sha256}`);
    m.delete(fold(f.rel));
  }
  if (!extraOk) for (const e of m.values()) out.push(`${e.path}: on BitsOS only`);
  return out;
}
