# rtw89-core/gen: the Rust emitter for the WLAN generators

`werkzeuge/plattform/wlan/gen/*.mjs` write NASM includes into the V1 tree, so they
are never run here. `emit.mjs` takes the same values from the same places (the JS
driver's modules and their source text, `kit/regs.mjs` over the Linux headers in
`wlan/linux/`) and writes typed Rust into `../src/gen/`:

| Original generator | Rust file | Content |
|---|---|---|
| `regs.mjs` | `regs.rs` | 2613 register/enum names the JS uses, as `pub const` plus the sorted `NAMES` list |
| `dmac.mjs` | `dmac.rs` | DLE quota modes, HFC channel/public/precision configuration |
| `fw-feat.mjs` | `fw_feat.rs` | fw_feat_tbl rows for the 8852B (checked against `featuresFor`) |
| `txrx.mjs` | `txrx.rs` | rtw89_bitrates, PHY status IE lengths |
| `btc.mjs` | `btc.rs` | coex enums, slot defaults, monitor registers, policy tables |
| `chan-tables.mjs` | `chan.rs` | country rules (DE, world), SCO, gain registers, TX FIR, DFIR, TX power element ids |
| `phy.mjs` | `phy.rs` | PMAC/pre-AGC tables, delay pseudo addresses, DIG thresholds, efuse offsets |
| `rfk1-tables.mjs` | `rfk1.rs` | RFK sequence tables as `Rfk` entries, number arrays |
| `rfk2-tables.mjs` | `rfk2.rs` | TSSI/DPK sequences, the path/band pickers, TSSI groups, alignment arrays |
| `rfk2-trkdata.mjs` | `trk.rs` | TX power tracking deltas by type |

Run from the worktree (the permission model lets it write only into `src/gen`):

    node --permission --allow-fs-read='*' --allow-fs-write="$PWD/crates/rtw89-core/src/gen" crates/rtw89-core/gen/emit.mjs
    node crates/rtw89-core/gen/emit.mjs --check      # nothing written; exit 1 if a file is stale

`WLAN_TOOLS=<plattform/wlan>` points at another copy of the WLAN tools.
`spot.mjs` prints values straight from the JS tables for `tests/gen_spot.rs`;
`spot-tables.mjs` prints a fingerprint of every generated table (rows, integer
count and sum, row-order weighted sum, first and last row) from the JS values.
