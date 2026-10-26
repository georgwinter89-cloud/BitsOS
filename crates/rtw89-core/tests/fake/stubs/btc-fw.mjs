// stubs/btc-fw.mjs: fw.mjs's h2cSend for btc-js.mjs: each coex H2C is only noted with its place in
// the trace (d.calls), its header fields and its payload, so the Rust side's H2Cs can be compared
// byte for byte where they happen. The CH12 path itself is checked by the golden trace
// (tests/fake_golden.rs). Reads nothing, writes nothing.
export async function h2cSend(d, { cat, cls, func, payload, rack = false, dack = false }) {
  (d.calls ??= []).push(['h2c', d.trace.length + d.q.list.length, cat, cls, func, rack ? 1 : 0, dack ? 1 : 0,
    Buffer.from(payload).toString('hex')]);
}
