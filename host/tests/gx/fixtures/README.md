# GX fixtures

Real dumps the host tests compare against; each test skips while its file is missing.

| File | From | Test |
|---|---|---|
| `minipc-780m-discovery.bin` | the Mini-PC's IP discovery binary: MCP `gx_probe` with `{"hex":true}`, the `hex ` line through `xxd -r -p` (8 KiB or `binary_size` bytes) | `amdgpu` host test `tests::disc::minipc_real_blob` (against Linux's sysfs view in `crates/drivers/amdgpu/tests/data/minipc-780m-ip.txt`) |

No firmware and nothing from the home network goes here (public repository).
