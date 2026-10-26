// stubs/rfk2-macinit.mjs: test-rfk2.mjs's stand-in of macinit.mjs (js/stubs/rfk2-macinit.mjs)
// for rfk2-js.mjs: the register path of rtw89_set_hw_sch_tx_en (mac.c:3363) with b-mac's
// signatures, plus each call noted with its place in the trace (d.calls).
import { pathToFileURL } from 'node:url';

const WZ = process.env.WLAN_TOOLS ?? '/home/georg/BitsOS/werkzeuge/plattform/wlan';
const { R, lowbit } = await import(pathToFileURL(`${WZ}/kit/rtwdev.mjs`).href);

async function setHwSchTxEn(d, txEn, mask) {
  d.mask16(R('R_AX_CTN_TXEN'), mask, (txEn & mask) >>> lowbit(mask));
  await d.flush();
}

// rtw89_mac_stop_sch_tx (mac.c:3403), MAC 0, RTW89_SCH_TX_SEL_ALL
export async function stopSchTx(d, sel = 'all') {
  (d.calls ??= []).push(['stopSchTx', d.trace.length + d.q.list.length, sel]);
  const txEn = await d.r16(R('R_AX_CTN_TXEN'));
  if (sel === 'all') await setHwSchTxEn(d, 0, R('B_AX_CTN_TXEN_ALL_MASK'));
  return txEn;
}

// rtw89_mac_resume_sch_tx (mac.c:3485), MAC 0
export async function resumeSchTx(d, txEn) {
  (d.calls ??= []).push(['resumeSchTx', d.trace.length + d.q.list.length, txEn]);
  await setHwSchTxEn(d, txEn, R('B_AX_CTN_TXEN_ALL_MASK'));
}
