// regs.mjs: src/gen/regs.rs, every rtw89 #define and enum member the JS
// driver names, with the selection of werkzeuge/plattform/wlan/gen/regs.mjs
// (R('...'), template names, quoted names, the extra enums) and the values of
// kit/regs.mjs. Plus NAMES, the sorted (name, value) list.
import { readFileSync, readdirSync } from 'node:fs';
import { WZ, wzImport, header, hex } from './lib.mjs';

const HEADERS = ['reg.h', 'pci.h', 'mac.h', 'phy.h', 'fw.h', 'core.h', 'txrx.h', 'efuse.h', 'rtw8852b_common.h', 'cam.h',
  'coex.h', 'chan.h', 'rtw8852b_rfk_table.h'];

// the names the original picks, in sorted order
export function usedNames() {
  const all = new Set();
  for (const f of HEADERS) {
    const text = readFileSync(`${WZ}/linux/${f}`, 'latin1');
    for (const m of text.matchAll(/^#define\s+([A-Za-z_]\w*)\s/gm)) all.add(m[1]);
    for (const em of text.matchAll(/\benum\s+\w+\s*\{([^}]*)\}/g)) {
      for (const m of em[1].replace(/\/\*[\s\S]*?\*\//g, '').matchAll(/^\s*([A-Za-z_]\w*)\s*(=|,|$)/gm)) all.add(m[1]);
    }
  }
  const used = new Set(), patterns = [];
  const pattern = (tpl) => new RegExp('^' + tpl.split(/\$\{[^}]*\}/).map((s) => s.replace(/[^\w]/g, '')).join('\\w+') + '$');
  for (const dir of ['js', 'kit']) for (const f of readdirSync(`${WZ}/${dir}`).filter((n) => n.endsWith('.mjs'))) {
    const text = readFileSync(`${WZ}/${dir}/${f}`, 'utf8');
    for (const m of text.matchAll(/\bR\('([A-Za-z_]\w*)'\)/g)) used.add(m[1]);
    for (const m of text.matchAll(/\bR\(`([^`]*)`\)/g)) patterns.push(pattern(m[1]));
    for (const m of text.matchAll(/'([A-Za-z_]\w*)'/g)) if (all.has(m[1])) used.add(m[1]);
    for (const m of text.matchAll(/`([A-Z_][A-Z0-9_]*\$\{[^`]*)`/g)) patterns.push(pattern(m[1]));
  }
  for (const n of ['RTW89_DMAC_SEL', 'RTW89_CMAC_SEL', 'RTW89_QTA_SCC', 'RTW89_QTA_DBCC', 'RTW89_QTA_DLFW', 'RTW89_QTA_WOW',
    'RTW89_QTA_INVALID']) used.add(n);
  for (const n of all) if (patterns.some((re) => re.test(n))) used.add(n);
  return [...used].sort();
}

export async function regsRs() {
  const { R, has } = await wzImport('kit/regs.mjs');
  const consts = [], names = [];
  let skipped = 0;
  for (const n of usedNames()) {
    if (!has(n)) { skipped++; continue; }
    let v;
    try { v = R(n); } catch { continue; }                 // a define that is no number (a type, a string)
    if (!Number.isInteger(v)) continue;
    // C masks written as ~x evaluate negative in JS: kept signed, callers cast (`as u32`)
    const type = v < 0 ? 'i32' : v <= 0xffffffff ? 'u32' : 'u64';
    if (v < -0x80000000) throw new Error(`${n} = ${v} does not fit i32`);
    consts.push(`pub const ${n}: ${type} = ${v < 0 ? v : hex(v)};`);
    names.push(`    ("${n}", ${v < 0 ? v : hex(v)}),`);
  }
  const text = [...header('kit/regs.mjs over linux/*.h, names chosen as gen/regs.mjs does'),
    '#![allow(dead_code, non_upper_case_globals)]', '', ...consts, '',
    '/// Every name above with its value, sorted by name (binary search: `crate::tables::reg_value`).',
    'pub static NAMES: &[(&str, i64)] = &[', ...names, '];', ''].join('\n');
  return { text, count: consts.length, skipped };
}
