// stubs/chan-macinit.mjs: test-chan.mjs's stand-in of macinit.mjs (js/stubs/chan-macinit.mjs) for
// chan-js.mjs: rtw89_mac_stop_sch_tx / rtw89_mac_resume_sch_tx (mac.c:3403, 3485) with the
// register path of rtw89_set_hw_sch_tx_en (mac.c:3363) and b-mac's signatures, plus each call
// noted with its place in the trace (d.calls).
import { pathToFileURL } from 'node:url';

const WZ = process.env.WLAN_TOOLS ?? '/home/georg/BitsOS/werkzeuge/plattform/wlan';
const { R } = await import(pathToFileURL(`${WZ}/kit/rtwdev.mjs`).href);

async function setHwSchTxEn(d, txEn, mask) {
  const v = await d.r32(R('R_AX_CMAC_FUNC_EN'));
  if (!(v & R('B_AX_CMAC_EN'))) return;
  const cur = await d.r16(R('R_AX_CTN_TXEN'));
  d.w16(R('R_AX_CTN_TXEN'), ((cur & ~mask) | (txEn & mask)) & 0xffff);
}

// Linux: rtw89_mac_stop_sch_tx (mac.c:3403), MAC 0, RTW89_SCH_TX_SEL_ALL
export async function stopSchTx(d /* , sel = 'all' */) {
  (d.calls ??= []).push(['stopSchTx', d.trace.length + d.q.list.length]);
  const txEn = await d.r16(R('R_AX_CTN_TXEN'));
  await setHwSchTxEn(d, 0, R('B_AX_CTN_TXEN_ALL_MASK'));
  return txEn;
}

// Linux: rtw89_mac_resume_sch_tx (mac.c:3485), MAC 0
export async function resumeSchTx(d, txEn) {
  (d.calls ??= []).push(['resumeSchTx', d.trace.length + d.q.list.length, `0x${txEn.toString(16)}`]);
  await setHwSchTxEn(d, txEn, R('B_AX_CTN_TXEN_ALL_MASK'));
}
