# Scenario tests (`host/tests/`)

`cargo xtask fullrun` runs host tests, the forbid-unsafe gate, all ktests, `cargo xtask deps`, and then every
scenario script found here. Scripts are how a lane proves a QEMU-level behaviour that a host test or a ktest cannot.
`cargo xtask test` (the gate) runs the scripts that test classes name, see "Scenario boots and the ledger" below.
`cargo xtask scenario <folder>/<script>` runs one script with this environment, the way to repeat one by hand.

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
  | `BITSOS_KTEST_RUN` | `<folder>/<script>`: `xtask image` writes it as CONFIG `ktest_run=`, unless the script's own config names one. A script that shares another's tests exports its name (several with commas) |

- Per-script timeout: 1200 s by default (`--script-timeout SECS`). On a timeout the script's process group is killed.
- `--quick` (the "done" criterion of a lane item) skips scripts that carry `# slow` in one of their first five lines.
  Mark scripts that take more than about a minute.
- Use `cargo xtask image` / `cargo xtask qemu --name <unique>` (names must be unique per script, e.g. the script
  name) so parallel scripts do not share an image. QEMU is always started through `xtask qemu`, which refuses host
  disks, passthrough and ports outside the lane's range; never call `qemu-system-x86_64` directly from a script.
- A script that boots a program tree (Brave, LibreOffice, Cobalt, ...) carries `# minipc: <where its checks run now>`
  in its first five lines and no run starts it (Georg 30.10., CONTRACTS section 4); `host/tests/minipc/` holds the desk's
  scripts against the real Mini-PC and is no lane folder.
- Scripts never touch the stick, a block device, the Mini-PC or another lane's folder.

## Scenario boots and the ledger (TS-1, Georg 30.10.: no skipped tests)

- A ktest cannot skip. Its class says up front which run it belongs to (`crates/ktest/src/lib.rs`); inside its run a
  test that cannot work is FAIL. A test of another run prints `KTEST skip <name>: class <class> belongs to another run`.
- `Run("nv/10-nvme")` is a test only that boot can run: the script supplies its device or CONFIG line. `RunHw("...")`
  is the same under QEMU and runs on the Mini-PC too. The plain boot prints them, the gate runs the scripts they name.
- A boot whose CONFIG makes one test of its filter meaningless takes it out by name: `ktest=nv.,-nv.irq;nvme_irq=off`.
- The gate's ledger counts `KTEST ok` lines of every boot (and of every `*.log`/`*.txt` in the run folder) and is red
  for a test that ran nowhere. A test that ends the boot or answers over MCP prints no such line on the serial port: its
  script, after checking the result, writes `KTEST ok <name> (checked by this script)` into `$BITSOS_RUN_DIR/ledger.txt`.
- `# helper` in the first five lines: not a scenario, other scripts call it with arguments; no run starts it.

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
