# user/ports/bubblewrap: bubblewrap for BitsOS V2 (SX-3b)

`build.sh` builds the real `bwrap` (bubblewrap 0.13.1, static-pie with the UL-1 musl sysroot, x86-64-v3) and what
its own test suite needs that V2 lacks. `tests/record-host.sh` records the suite on CachyOS with the same stage;
`host/tests/sx/30-bwrap-suite.sh` runs it on V2 in QEMU and compares (host/tests/sx/expected/).

## Downloads (download cache `$V2_DOWNLOADS`, SHA256 checked on every build)

| File | Source | SHA256 | Licence |
|---|---|---|---|
| `bubblewrap-0.13.1.tar.xz` | https://github.com/containers/bubblewrap/releases/download/v0.13.1/bubblewrap-0.13.1.tar.xz | `0e85c8702248b8dd64144f594147553650cdd0e4c951690345faace79c217c4d` (GitHub's published digest) | LGPL-2.0-or-later |
| `meson-1.12.1.tar.gz` (build tool only) | https://github.com/mesonbuild/meson/releases/download/1.12.1/meson-1.12.1.tar.gz | `ab0a6ca09f8ef70c564c8241fb5a23957886a0b53fb58412b5e07eaf07dba743` (GitHub's published digest) | Apache-2.0 |
| `bash-5.3.tar.gz` (test shell) | https://ftp.gnu.org/gnu/bash/bash-5.3.tar.gz | `0d5cd86965f869a26cf64f4b71be7b96f90a3ba8b3d74e27e8e9d9d5550f31ba` | GPL-3.0-or-later |

## What is ours

- `libcap-shim/`: bubblewrap uses one libcap function, `cap_from_name`; the shim (our code) and a pkg-config file
  stand in for libcap, so no libcap download. `capget`/`capset` are musl's.
- `tools/`: helpers for the suite, each doing only what the suite asks of the real tool:
  `getpcaps` (capget of a pid, libcap's text form for the cases the suite greps), `findmnt` (flat list of
  /proc/self/mountinfo), `df` (only `--output=size --block-size=1K PATH`, statfs), `bwrap-lockf` (the suite's
  Python lock probe in C).
- `patches/test-run-lockf.patch`: the suite's die-with-parent check writes a Python lock probe; V2 has no Python,
  so the probe calls `bwrap-lockf` (same open/fcntl calls). The only change to the suite.

## Running the suite (both sides the same way)

As the unprivileged user, inside one outer `bwrap --unshare-user --uid <me> --gid <me> --dev-bind / /`. On V2 the
outer bwrap also puts a real `/usr/bin` (busybox, its applets, bash as `sh`, the helpers) over V2's
`/usr/bin -> ../bin` link: the suite's "pivot to a new rootfs" check builds a root from /usr and /etc with
`/bin -> usr/bin`, which with V2's link would point at itself.

Not run on V2 (no Python): `test-sandbox.py`, `test-seccomp.py` (and `try-syscall`, which only they use).
