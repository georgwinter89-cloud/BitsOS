// stubs/chan-rfk1.mjs: test-chan.mjs's stand-in of rfk1.mjs (js/stubs/chan-rfk1.mjs) for
// chan-js.mjs: records where chan.mjs calls the RF parts (d.calls, with the channel), writes
// nothing. With d.failRf, setChannelRf sends the SWSI busy poll of an RF write
// (rtw89_phy_write_rf_a), so the scenario's onPoll can make it time out.
import { pathToFileURL } from 'node:url';

const WZ = process.env.WLAN_TOOLS ?? '/home/georg/BitsOS/werkzeuge/plattform/wlan';
const { R, BB } = await import(pathToFileURL(`${WZ}/kit/rtwdev.mjs`).href);

const mark = (d, name, ch) => (d.calls ??= []).push([name, d.trace.length + d.q.list.length, ch]);

// Linux: rtw8852b_set_channel_rf (rtw8852b_rfk.c:4164)
export async function setChannelRf(d, c) {
  mark(d, 'setChannelRf', c.channel);
  if (!d.failRf) return;
  d.poll32(BB + R('R_SWSI_V1'), R('B_SWSI_W_BUSY_V1') | R('B_SWSI_R_BUSY_V1'), 0, 1);
  await d.flush();
}
// Linux: rtw8852b_rfk_band_changed (rtw8852b.c:731)
export async function rfkBandChanged(d, c) { mark(d, 'rfkBandChanged', c.channel); }
// Linux: rtw8852b_rfk_channel (rtw8852b.c:711)
export async function rfkChannel(d) { mark(d, 'rfkChannel', d.chan.channel); }
