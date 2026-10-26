# user/abi-tests: programs for the ABI diff (`cargo xtask abi-diff`)

The harness (AB, `host/xtask/src/abi_diff.rs`) builds every program here once, runs the same file on CachyOS
and on V2 (a QEMU boot, started through the `run` MCP tool) and compares how it ended, its stdout and its
stderr. Linux is the oracle: there are no hand-written expectations. The programs are UL's to extend
(V2-LANES §2); wave A was seeded by AB-3.

| Path | What |
|---|---|
| `c/*.c` | One program per file, `abi-<stem>`, built with UL-1's `user/sysroot/bin/v2-cc` (musl, static-PIE). `c/abi.h` has the helpers (`SYS(label, nr, ...)` prints the raw result or `-ERRNO`, `yes(label, cond)`). |
| `rust/` | A Cargo package (std only, its own workspace); every `src/bin/<name>.rs` is a program. |
| `dyn/*.c` | AB-8: dynamic glibc programs, `dyn-<stem>`, built with the host's gcc (PIE, PT_INTERP `/lib64/ld-linux-x86-64.so.2`); with CachyOS's own `ls`/`cat` (`dyn-ls`, `dyn-cat`) and the libraries `ldd` lists (`host/xtask/src/abi_dyn.rs`). On V2 the harness writes the libraries to `/lib64` and `/usr/lib` over MCP before the dynamic cases. |
| `cases.txt` | Arguments and stdin per run (format in the file). |

Rules for a program (so a difference always means a kernel difference):
- print results, errno names and yes/no relations, never pids, addresses, absolute times, CPU or memory
  counts, or anything the host's limits or users decide (lower a limit to a known value first);
- clean up what it creates (files in `/tmp` named with the pid);
- never let a signal reach a handler until AB-6 delivers signals in V2;
- keep stdin small (below 32 KiB).

Commands: `cargo xtask abi-diff` (all), `cargo xtask abi-diff abi-time` (one program's cases),
`cargo xtask abi-diff --host-only` (CachyOS only, prints the transcripts: for writing a new program).
Transcripts land in `target/xtask/abi-diff/<case>.{linux,v2}.txt`.
