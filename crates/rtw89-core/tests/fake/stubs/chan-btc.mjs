// stubs/chan-btc.mjs: test-chan.mjs's stand-in of btc.mjs (js/stubs/chan-btc.mjs) for
// chan-js.mjs: rtw89_btc_ntfy_switch_band (coex.c:8837) only noted with its place in the trace
// (d.calls). Reads nothing, writes nothing.
export async function btcSwitchBand(d) {
  (d.calls ??= []).push(['btcSwitchBand', d.trace.length + d.q.list.length]);
}
