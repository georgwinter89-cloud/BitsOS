// pci-js.mjs: the JS driver's own pci.mjs and dmac.mjs on the kit's FakeDev,
// scenario by scenario, as text: `## <scenario>`, then every op as a
// bringup-golden.txt line, `= ...` lines for what the functions returned and
// `! error` where one failed. tests/pci_js.rs runs the same scenarios on the
// Rust side (MemBus with the same answers) and compares the text.
// The scenarios are test-pci.mjs's (its fake chip's answers), plus the full
// DLE/HFC init of the MAC init and the failures that both sides see the same.
//   node pci-js.mjs            (WLAN_TOOLS: the read-only WLAN tree)
// Reads only; writes nothing but stdout.
import { pathToFileURL } from 'node:url';

const WZ = process.env.WLAN_TOOLS ?? '/home/georg/BitsOS/werkzeuge/plattform/wlan';
const imp = (rel) => import(pathToFileURL(`${WZ}/${rel}`).href);
const { FakeDev, R, prep } = await imp('kit/rtwdev.mjs');
const { dmacPreInit, dleInit, hfcInit } = await imp('js/dmac.mjs');
const pci = await imp('js/pci.mjs');

const KIND = { 1: 'wr', 2: 'rd', 3: 'rmw', 4: 'poll', 5: 'delay', 6: 'mwr', 7: 'mrd', 8: 'mcpy', 9: 'cfgr', 10: 'cfgw', 11: 'mpoll', 12: 'mark', 13: 'now' };
const hex = (v) => `0x${(v >>> 0).toString(16).toUpperCase().padStart(8, '0')}`;
const line = ([k, s, t, a, b, c]) =>
  `${KIND[k]}${k <= 4 || k === 6 || k === 7 ? s * 8 : ''} ${hex(a)} ${hex(b)} ${hex(c)}${k === 4 || k === 11 ? ` ${t} ms` : ''}`;
const out = [];
const r = (n) => R(n) >>> 0;

// test-pci.mjs's chip: config space (PCIe capability at 0x70, gen 1, LTR on), MDIO done
// at once with 0xffff in RDATA, the L1 bit in DBI byte 0x719, an LBC watchdog value;
// `answers` adds register values (ring indexes, page counters)
function fake(cfg = {}, answers = new Map()) {
  const CFG = { 0x04: 0x00100002, 0x34: 0x70, 0x70: 0x00020010, 0x80: 0x10110000, 0x98: 0x400, ...cfg };
  const d = new FakeDev({ onRead: (kind, a) => {
    if (kind === 'cfg') return CFG[a] ?? 0;
    if (kind !== 'mmio') return undefined;
    if (a === r('R_AX_MDIO_CFG')) return 0;
    if (a === r('R_AX_MDIO_RDATA')) return 0xffff;
    if (a === r('R_AX_DBI_RDATA') + (r('RTW89_PCIE_L1_CTRL') & 3)) return 0x47;
    if (a === r('R_AX_LBC_WATCHDOG')) return 0x12345678;
    return answers.get(a);
  } });
  d.answers = answers;
  return d;
}

// one scenario: its ops since the last mark, then what fn returned
async function scenario(name, d, fn) {
  out.push(`## ${name}`);
  await d.flush();
  const t0 = d.trace.length;
  let res = [];
  try {
    res = (await fn()) ?? [];
    await d.flush();
  } catch (e) {
    res = ['! error'];
    if (process.env.PCI_JS_VERBOSE) console.error(name, e.message);
  }
  for (const op of d.trace.slice(t0)) out.push(line(op));
  out.push(...res);
}

// ---- test-pci.mjs's run, op by op ----
const d = fake();
await scenario('alloc, dmac pre-init, pci pre-init', d, async () => {
  await pci.pciAlloc(d);
  await dmacPreInit(d);
  await d.flush();
  await pci.pciPreInit(d);
});
const g2 = fake({ 0x80: 0x10120000 });
await scenario('pre-init at gen 2', g2, async () => { await pci.pciAlloc(g2); await pci.pciPreInit(g2); });
const g3 = fake({ 0x80: 0x10130000 });
await scenario('pre-init at a PHY rate without MDIO pages', g3, async () => { await pci.pciAlloc(g3); await pci.pciPreInit(g3); });
await scenario('capability walk, post-init, basic_cfg', d, async () => {
  const cap = await pci.pciExpCap(d);
  await pci.pciPostInit(d);
  const bc = await pci.pciBasicCfg(d);
  return [`= cap ${cap}`, `= basic ${bc.devctl2} ${bc.lnkctl}`];
});
const noLtr = fake({ 0x98: 0 });
await scenario('post-init without LTR', noLtr, async () => { await pci.pciAlloc(noLtr); await pci.pciPostInit(noLtr); });
const noCap = fake({ 0x34: 0x50 });
await scenario('no PCIe capability', noCap, async () => { await pci.pciAlloc(noCap); await pci.pciExpCap(noCap); });

// the firmware command channel: the BD, the kick, the wait; then the buffer's bytes
d.onPoll = () => true;
for (const [n, len, fwDl] of [[1, 100, false], [2, 2020, true]]) {
  await scenario(`fwcmd ${n}`, d, async () => {
    const ring = d.pci.tx.CH12, at = ring.bufs[ring.wp];
    await pci.pciFwcmd(d, Buffer.alloc(len, 0xab + n), { fwDl });
    const m = await d.memRead(at, 24 + len);
    return [`= mem ${m.subarray(0, 48).toString('hex')} ${m.subarray(24).every((b) => b === 0xab + n)}`, `= wp ${ring.wp} ${ring.rp}`];
  });
}
await scenario('fwcmd too long', d, () => pci.pciFwcmd(d, Buffer.alloc(2025)));

// RX: three frames, an empty poll, then a wrap with a budget
const rxq = d.pci.rx.RXQ;
const fill = async (i, n, tag) => {
  const info = (prep(r('RTW89_PCI_RXBD_WRITE_SIZE'), 4 + n) | prep(r('RTW89_PCI_RXBD_TAG'), tag) |
    (i % 3 ? r('RTW89_PCI_RXBD_FS') : 0) | (i % 2 ? 0 : r('RTW89_PCI_RXBD_LS'))) >>> 0;
  const b = Buffer.alloc(4); b.writeUInt32LE(info, 0);
  await d.memWrite(rxq.bufs[i], Buffer.concat([b, Buffer.alloc(n, (i + 1) & 0xff)]));
};
const show = (got) => got.map((x) => `= rx ${x.data.length} ${+x.fs} ${+x.ls} ${x.data[0] ?? -1} ${x.tag} ${x.len}`);
for (const [i, n] of [60, 1500, 3000].entries()) await fill(i, n, i);
d.answers.set(rxq.idx, prep(r('TXBD_HW_IDX_MASK'), 3));
await scenario('rx three', d, async () => show(await pci.pciRxPoll(d, 'RXQ')));
await scenario('rx empty', d, async () => show(await pci.pciRxPoll(d, 'RXQ')));
for (let i = 3; i < 64; i++) await fill(i, (i * 137) % 4300, i);   // some longer than a buffer: cut to it
for (let i = 0; i < 2; i++) await fill(i, 10 + i, 100 + i);
d.answers.set(rxq.idx, prep(r('TXBD_HW_IDX_MASK'), 2));
await scenario('rx budget 5', d, async () => show(await pci.pciRxPoll(d, 'RXQ', 5)));
await scenario('rx wrap', d, async () => show(await pci.pciRxPoll(d, 'RXQ', 100)));

await scenario('deinit', d, () => pci.pciDeinit(d));
await scenario('reset', d, async () => {
  await pci.pciReset(d);
  return [`= idx ${Object.values(d.pci.tx).map((x) => `${x.wp}/${x.rp}`).join(' ')} ${Object.values(d.pci.rx).map((x) => `${x.wp}/${x.rp}`).join(' ')}`];
});

// ---- the DLE and HFC init of the MAC init (SCC), on a chip whose page counters add up ----
const hfcShow = (m) => {
  const p = m.hfcParam, pi = p.pubInfo;
  return [`= hfc ${+p.en} ${+p.h2cEn} ${pi.g0Used} ${pi.g1Used} ${pi.g0Aval} ${pi.g1Aval} ${pi.pubAval} ${pi.wpAval}`,
    `= ch ${Object.entries(p.chInfo).map(([ch, x]) => `${ch}:${x.aval}/${x.used}`).join(' ')}`,
    `= dle ${m.dleInfo.qtaMode} ${m.dleInfo.plePgSize} ${m.dleInfo.pleFreePg} ${m.dleInfo.c0RxQta} ${m.dleInfo.c1RxQta}`];
};
const pages = (pubAval) => new Map([
  [r('R_AX_PUB_PAGE_INFO1'), prep(r('B_AX_G0_USE_PG_MASK'), 6) | prep(r('B_AX_G1_USE_PG_MASK'), 0)],
  [r('R_AX_PUB_PAGE_INFO3'), prep(r('B_AX_G0_AVAL_PG_MASK'), 440)],
  [r('R_AX_PUB_PAGE_INFO2'), prep(r('B_AX_PUB_AVAL_PG_MASK'), pubAval)],
  [r('R_AX_WP_PAGE_INFO1'), prep(r('B_AX_WP_AVAL_PG_MASK'), 77)],
  [r('R_AX_ACH0_PAGE_INFO') + 4, prep(r('B_AX_AVAL_PG_MASK'), 5) | prep(r('B_AX_USE_PG_MASK'), 3)],
  [r('R_AX_ACH0_PAGE_INFO') + 9 * 4, prep(r('B_AX_AVAL_PG_MASK'), 4) | prep(r('B_AX_USE_PG_MASK'), 1)],
]);
for (const [name, pubAval] of [['scc dle and hfc', 440], ['scc hfc public pages do not add up', 400]]) {
  const m = fake({}, pages(pubAval));
  await scenario(name, m, async () => {
    await dmacPreInit(m);
    await dleInit(m, 'SCC', 'SCC');
    await hfcInit(m, true, true, true);
    return hfcShow(m.mac);
  });
}
const off = fake({}, new Map([[r('R_AX_DMAC_FUNC_EN'), 0xEAEAEAEA]]));
await scenario('dmac not enabled', off, () => dmacPreInit(off));
const half = fake({}, new Map([[r('R_AX_DMAC_FUNC_EN'), r('B_AX_MAC_FUNC_EN')]]));
await scenario('dmac half enabled', half, () => dmacPreInit(half));

process.stdout.write(out.join('\n') + '\n');
