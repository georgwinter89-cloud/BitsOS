// stubs/rfk2-btc.mjs: test-rfk2.mjs's stand-in of btc.mjs (js/stubs/rfk2-btc.mjs) for
// rfk2-js.mjs, which also notes each call with its place in the trace (d.calls), so the Rust
// side's neighbour calls can be compared where they happen. Reads nothing, writes nothing.
import { pathToFileURL } from 'node:url';

const WZ = process.env.WLAN_TOOLS ?? '/home/georg/BitsOS/werkzeuge/plattform/wlan';
const { R, prep } = await import(pathToFileURL(`${WZ}/kit/rtwdev.mjs`).href);

// rtw89_btc_ntfy_wl_rfk (coex.c:9466) under b-btc's name: only recorded
export async function btcWlRfk(d, phyMap, type, state) {
  (d.calls ??= []).push(['btcWlRfk', d.trace.length + d.q.list.length, phyMap, type, state]);
  d.btc ??= {};
  (d.btc.rfkLog ??= []).push([phyMap, type, state]);
  return true;
}

// Linux: rtw89_btc_phymap (coex.h:353), as btc.mjs has it (rfk1-util.mjs re-exports it)
export function btcPhyMap(d, phyIdx, paths) {
  return (prep(R('BTC_RFK_PATH_MAP'), paths) | prep(R('BTC_RFK_PHY_MAP'), 1 << phyIdx) |
    prep(R('BTC_RFK_BAND_MAP'), { '2g': 0, '5g': 1 }[d.chan?.band ?? '2g'])) >>> 0;
}
