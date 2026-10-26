// Prints V1's answers as JSON for tests/oracle.rs: hid_parse's PAD_LAYOUT (hidparse-model.mjs) for
// the named descriptors of hidparse-test.mjs, every prefix of DragonRise and DualShock 4, and the
// 6,000 random/mutated descriptors that test uses (same seed); and pad_report's bits
// (gamepad-model.mjs) for its named and 600 random cases. Reads V1, writes nothing.
//   node oracle/v1-oracle.mjs > oracle/v1-oracle.json      (V1=<dir> to use another V1 tree)
import { readFileSync } from 'node:fs';
import { join } from 'node:path';
import { pathToFileURL } from 'node:url';

const V1 = process.env.V1 ?? '/home/georg/BitsOS/BitsOS-Plattform';
process.chdir(V1); // gamepad-model.mjs reads usb/hidparse.asm relative to the working directory
const load = (f) => import(pathToFileURL(join(V1, f)).href);
const { hidParse, layoutFields, randomDescriptors } = await load('hidparse-model.mjs');
const { decodeCases } = await load('gamepad-model.mjs');
const pads = await load('usbredir-pad.mjs');

const REPORT = Number(readFileSync('usb/xhci.asm', 'latin1').match(/^XR_REPORT\s+equ\s+(\d+)/m)[1]);

// The named list of hidparse-test.mjs, taken from its source (the file boots QEMU when imported).
const src = readFileSync('hidparse-test.mjs', 'utf8');
const body = src.slice(src.indexOf('const h = (text)'), src.indexOf('// a program that puts'));
const names = ['BUTTONS0', 'DRAGONRISE', 'DUALSHOCK3', 'HATPAD', 'NOBUTTONS', 'SIGNEDPAD', 'STICKID3', 'VENDORHID'];
const named = new Function('Buffer', ...names, `${body}\nreturn { named, DS4 };`)(Buffer, ...names.map((n) => pads[n]));

const hex = (b) => Buffer.from(b).toString('hex');
const v1 = (d) => {
  const r = hidParse(d, REPORT);
  return r.cf ? null : layoutFields(r.layout);
};
const parse = [];
for (const [name, d, expect] of named.named) parse.push({ group: 'named', name, desc: hex(d), v1: v1(d), expect });
for (const [name, d] of [['DragonRise', pads.DRAGONRISE.report], ['DualShock 4', named.DS4]]) {
  for (let k = 0; k <= d.length; k++) parse.push({ group: 'prefix', name: `${name}, first ${k} bytes`, desc: hex(d.subarray(0, k)), v1: v1(d.subarray(0, k)) });
}
const random = randomDescriptors(0x5EED, 6000, named.named.map(([, b]) => b));
random.forEach((d, i) => parse.push({ group: 'random', name: `random ${i}`, desc: hex(d), v1: v1(d) }));

const decode = decodeCases(600).map((c) => ({
  name: c.name,
  layout: layoutFields(c.bytes.subarray(0, 40)),
  report: hex(c.bytes.subarray(40, 104)),
  len: c.bytes[104],
  want: c.want,
}));

process.stdout.write(`${JSON.stringify({ report_bytes: REPORT, parse, decode }, null, 0).replaceAll('},{', '},\n{')}\n`);
