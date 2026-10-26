# user/ports: foreign programs built for BitsOS V2

Each port builds a program from its upstream source with the UL-1 toolchain (`user/sysroot/bin/v2-cc`, static-pie,
musl) for CachyOS and V2 alike.

| Port | What | Built by |
|---|---|---|
| `busybox/` | BusyBox 1.36.1: the console shell (ash) and the small Unix tools (UL-3a) | `busybox/build.sh` |
| `cobalt-launcher/` | YouTube TV (Cobalt) for the framebuffer (YT lane) | its own README |

## Download cache

Source tarballs are downloaded once, only with Georg's OK per tarball, and kept outside the repository in
`$V2_DOWNLOADS` (default `~/.cache/bitsos-v2/downloads/`), shared by every worktree of the machine.

- A port's build script names the exact URL, version and SHA256 and checks the hash on every build; a mismatch
  stops the build.
- The tarball is unpacked into the port's own `build/` directory (fresh each build, ignored by git) and its
  contents only ever run through the port's `make`.
- `V2_OFFLINE=1` makes a build script refuse to download (xtask and the scenario tests set it); without the
  tarball in the cache the port is then left out with a note.

| File | Source | SHA256 (as published next to it) | OK |
|---|---|---|---|
| `busybox-1.36.1.tar.bz2` | https://busybox.net/downloads/busybox-1.36.1.tar.bz2 | `b8cc24c9574d809e7279c3be349795c5d5ceb6fdf19ca709f80cde50e47de314` | Georg 20.10.2026 |
