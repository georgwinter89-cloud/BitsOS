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
  Print what happens; everything the script writes to stdout/stderr goes to `<RUN_DIR>/output.log`. A script
  that empties `RUN_DIR` unlinks that file; xtask still reads it through its own handle (the skip check) and puts
  it back when the script ends.
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
- A boot whose CONFIG makes one test of its filter meaningless takes it out by name: `ktest=nv.,-nv.irq;ktest_poll=nvme`.
- The gate's ledger counts `KTEST ok` lines of every boot (and of every `*.log`/`*.txt` in the run folder) and is red
  for a test that ran nowhere. A test that ends the boot or answers over MCP prints no such line on the serial port: its
  script, after checking the result, writes `KTEST ok <name> (checked by this script)` into `$BITSOS_RUN_DIR/ledger.txt`.
- `# helper` in the first five lines: not a scenario, other scripts call it with arguments; no run starts it.
- A scenario of user space only, which no ktest class can name, is in `ALWAYS_IN_GATE` (host/xtask/src/suite.rs).
- A scenario never skips: a line of its output that starts with `SKIP`/`SKIPPED` or says `(SKIPPED` makes it red in
  every run xtask starts it in (gate, fullrun, `cargo xtask scenario`), even with exit code 0 (`ledger::skip_line`).
  A new script says a missing tool or stage as `echo "RED: <what> missing (install <package>)"; exit 1`.
- Host tests (`cargo test`) never skip either: a missing tool, fixture, header or tree is a failure that names it
  and what to install. A host test that only a scenario can feed (a screendump, a pool image) is `#[ignore]` with
  one line next to it saying why and which scenario runs it.

## Interim runs (`cargo xtask test --interim`, Georg 13.11.)

- `host/tests/stable.txt` lists the tests that were never red in any kept log and green in at least 20 of them, one
  per line with its kind: `host:<crate>::<target source>::<test path>`, `ktest:<name>`, `scenario:<folder>/<script>`,
  `step:<gate step>`. `cargo xtask stable-list [--min-green N] [--logs DIR]... [--cache F] [--dry-run]` makes it from
  the logs below `/var/tmp/claude-v2` and `$HOME/BitsOS/V2/target` and those its cache remembers (a run folder is
  overwritten by the next run); never edit it by hand. A ktest that needs another group's tests earlier in its boot
  (`history.rs` `NEEDS_EARLIER`) keeps that group out of the list. A test that turns red once drops out at the
  next regeneration; the desk regenerates after each integrator full run and commits the list.
- `cargo xtask test --interim` is the gate without them: host tests by `--exact --skip` per cargo run, ktests by the
  boot filter (`-name`; a boot whose filter has nothing else is not started), listed gate steps and the listed script
  `rb/20-reload` not started. Every build and the warnings check still run. A script a test of the boot names always
  runs; a ktest whose class names a script is listed only together with that script.
- Its summary line starts with `INTERIM` and says how many tests it left out; its ledger counts every other test as
  strictly as the full gate. It never replaces the integrator's full run: `cargo xtask test` (and `fullrun`) never read
  the list and run everything (Georg 30.10.: no test is skipped in the full run).

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
