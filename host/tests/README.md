# Scenario tests (`host/tests/`)

`cargo xtask fullrun` runs host tests, the forbid-unsafe gate, all ktests, `cargo xtask deps`, and then every
scenario script found here. Scripts are how a lane proves a QEMU-level behaviour that a host test or a ktest cannot.

## Who owns what

- `host/tests/<lane>/` belongs to that lane, the folder name is the lane code in lower case (`bt`, `mm`, `fs`, ...;
  the table is in `V2-LANES.md` section 6). A lane edits only its own folder.
- `xtask fullrun*` (the runner itself) and this README belong to the desk. Changes to the convention are change
  requests to the desk.

## The convention

- A scenario is a bash script `host/tests/<lane>/<name>.sh`. Exit status 0 = pass, anything else = fail.
  Print what happens; everything the script writes to stdout/stderr goes to `<RUN_DIR>/output.log`.
- Scripts of one lane folder run one after the other, in file name order (use `10-boot.sh`, `20-net.sh` to order
  them). Different lane folders run in parallel chains. A script must not depend on another lane's script.
- Environment the runner sets:

  | Variable | Value |
  |---|---|
  | `BITSOS_ROOT` | the repo (worktree) root, also the working directory |
  | `BITSOS_LANE` | the folder's lane code (so `cargo xtask qemu` inside the script uses that lane's names and ports) |
  | `BITSOS_PORT_LO`, `BITSOS_PORT_HI` | the lane's port range; use only ports in it |
  | `BITSOS_RUN_DIR` | `<root>/target/xtask/runs/<lane>-<script>`, created fresh; put logs and scratch files here |

- Per-script timeout: 600 s by default (`--script-timeout SECS`). On a timeout the script's process group is killed.
- `--quick` (the "done" criterion of a lane item) skips scripts that carry `# slow` in one of their first five lines.
  Mark scripts that take more than about a minute.
- Use `cargo xtask image` / `cargo xtask qemu --name <unique>` (names must be unique per script, e.g. the script
  name) so parallel scripts do not share an image. QEMU is always started through `xtask qemu`, which refuses host
  disks, passthrough and ports outside the lane's range; never call `qemu-system-x86_64` directly from a script.
- Scripts never touch the stick, a block device, the Mini-PC or another lane's folder.

## Minimal example

```bash
#!/bin/bash
# slow
set -eu
cd "$BITSOS_ROOT"
cargo xtask image --name "$BITSOS_LANE-boot" --config-extra "loglevel=debug"
cargo xtask qemu --name "$BITSOS_LANE-boot" --timeout 60 --serial-log "$BITSOS_RUN_DIR/serial.log" || true
grep -q "BitsOS V2" "$BITSOS_RUN_DIR/serial.log"
```
