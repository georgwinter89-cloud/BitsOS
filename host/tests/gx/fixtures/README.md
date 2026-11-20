# GX fixtures

Real dumps the host tests compare against. They are tracked; a test whose file (or the `zstd` tool) is missing
fails with the file's name, it never skips.

| File | From | Test |
|---|---|---|
| `minipc-780m-discovery.bin` | the Mini-PC's IP discovery binary: MCP `gx_probe` with `{"hex":true}`, the `hex ` line through `xxd -r -p` (8 KiB or `binary_size` bytes) | `amdgpu` host tests `tests::disc::minipc_real_blob` (against Linux's sysfs view in `crates/drivers/amdgpu/tests/data/minipc-780m-ip.txt`), the `blob()` checks in `tests::{fakes,state,bringup}` |
| `trace-load.txt.zst` | Linux's register trace of the 27.10. amdgpu load on the Mini-PC (`plan/hw/gx-780m-linux-trace-2026-10-27/gx-6d9cae1d/trace-load.txt.zst`, the same text recompressed with `zstd -19`: 281,965 bytes, the plan/hw copy 417,531) | `amdgpu` `tests::regdiff::capture()`, used by `tests::{regdiff,bringup,gfxring,gmc,mes,sdma}` |
| `amdgpu_ring_gfx_0.0.0` | the debugfs dump of GFX ring 0 from the same capture (`.../gx-6d9cae1d/bundle/load/rings/`, 8,204 bytes) | `tests::gfxring::csb_is_the_capture` |
| `trace-sample.txt` | the first 36 lines of the same 27.10. trace (`zstd -dc ... \| head -36`: the header, 11 `amdgpu_device_wreg`, 13 `amdgpu_device_rreg`) | `host/tests/gx/30-trace-kit.sh` (the capture's trace checks), run by the `amdgpu` host test `trace_kit` |

No firmware and nothing from the home network goes here (public repository). The trace's non-register lines
were checked for that on 17.11.: only kernel worker, idle and bash process names, no host names or paths.
