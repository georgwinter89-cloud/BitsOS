// is-main.mjs (GP-ROUND): whether a module is the one node was started with, also on Windows. Node's own
// `pathToFileURL(resolve(argv[1])).href === import.meta.url` differs there when the drive letter or a folder is
// typed in another case than the disk has it (c:\bitsos vs C:\BitsOS), or the path holds a link: then the command
// silently did nothing. Both sides are made real paths and, on Windows, compared without case.
import { realpathSync } from 'node:fs';
import { resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const real = (p) => {
  try { return realpathSync.native(p); } catch { return resolve(p); }
};

export function isMain(moduleUrl, argv1 = process.argv[1], platform = process.platform) {
  if (!argv1) return false;
  const a = real(resolve(argv1)), b = real(fileURLToPath(moduleUrl));
  return platform === 'win32' ? a.toLowerCase() === b.toLowerCase() : a === b;
}
