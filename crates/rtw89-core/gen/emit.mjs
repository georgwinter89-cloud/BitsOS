// emit.mjs: the Rust emitter for werkzeuge/plattform/wlan/gen/*.mjs. Takes the
// same values (the JS driver's modules and source text, kit/regs.mjs over the
// Linux headers) and writes typed Rust into crates/rtw89-core/src/gen/.
// Writes nothing else; run it under node's permission model (README.md).
//   node --permission --allow-fs-read='*' --allow-fs-write=<crate>/src/gen crates/rtw89-core/gen/emit.mjs
//   node crates/rtw89-core/gen/emit.mjs --check     (writes nothing; exit 1 when a file is not up to date)
import { readFileSync, writeFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { regsRs } from './regs.mjs';
import { dmacRs, fwFeatRs, txrxRs, btcRs } from './tables-mac.mjs';
import { chanRs, phyRs } from './tables-phy.mjs';
import { rfk1Rs, rfk2Rs, trkRs } from './tables-rfk.mjs';

const OUT = fileURLToPath(new URL('../src/gen/', import.meta.url));

const regs = await regsRs();
const files = {
  'regs.rs': regs.text,
  'dmac.rs': dmacRs(),
  'fw_feat.rs': await fwFeatRs(),
  'txrx.rs': await txrxRs(),
  'btc.rs': await btcRs(),
  'chan.rs': await chanRs(),
  'phy.rs': phyRs(),
  'rfk1.rs': await rfk1Rs(),
  'rfk2.rs': await rfk2Rs(),
  'trk.rs': await trkRs(),
};
if (process.argv.includes('--check')) {
  const stale = Object.entries(files).filter(([name, text]) => {
    try { return readFileSync(OUT + name, 'utf8') !== text; } catch { return true; }
  }).map(([name]) => name);
  if (stale.length) {
    console.log(`not up to date: ${stale.join(', ')}`);
    process.exit(1);
  }
  console.log(`${Object.keys(files).length} files up to date`);
  process.exit(0);
}
for (const [name, text] of Object.entries(files)) writeFileSync(OUT + name, text);
console.log(`${Object.keys(files).length} files in ${OUT}; regs: ${regs.count} names (${regs.skipped} not defined)`);
