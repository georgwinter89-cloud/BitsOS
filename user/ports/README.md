# user/ports: foreign programs built for BitsOS V2

Each port builds a program from its upstream source with the UL-1 toolchain (`user/sysroot/bin/v2-cc`, static-pie,
musl) for CachyOS and V2 alike.

| Port | What | Built by |
|---|---|---|
| `busybox/` | BusyBox 1.36.1: the console shell (ash) and the small Unix tools (UL-3a) | `busybox/build.sh` |
| `cobalt-launcher/` | YouTube TV (Cobalt) for the framebuffer (YT lane) | its own README |
| `bubblewrap/` | bubblewrap 0.13.1 as `/bin/bwrap` (SX-3b; xtask puts it into every image), plus GNU bash 5.3 (static) and small helpers for its own test suite (scenario only) | `bubblewrap/build.sh` |
| `xwayland/` | Xwayland 24.1.14 and xkbcomp 1.5.0 in software, the tree `\BITSOS2\xwayland\` on the stick (WY-3; glibc build on the Mesa tree's loader, not musl) | `xwayland/scripts/build-v2.sh` |
| `brave-launcher/` | the `brave` launcher in `\BITSOS2\bin\` and the script that stages CachyOS's Brave as the glibc tree `\BITSOS2\brave\` (BR-2, BR-7) | `brave-launcher/scripts/stage-v2.sh` |
| `libreoffice-launcher/` | the `libreoffice` launcher in `\BITSOS2\bin\` and the script that stages CachyOS's signed libreoffice-fresh package as the glibc tree `\BITSOS2\libreoffice\` (LO-1) | `libreoffice-launcher/scripts/stage-v2.sh` |
| `brave/` | `brave-act`, Brave's actions for Claude over the DevTools pipe (AC-3; a Rust crate of the user/ workspace) | `cargo` (user/ workspace) |
| `ffmpeg-min/` | FFmpeg 7.1 (LGPL, decoders only) as shared libraries for Cobalt (VP9, H.264, AAC) | `ffmpeg-min/build.sh` |
| `mesa/` | the distribution's Mesa and Vulkan with their tools as the glibc tree `\BITSOS2\mesa\` | `mesa/scripts/stage-v2.sh` (its own README) |
| `steamrt/` | the Steam Linux Runtime sniper tree with pressure-vessel, plus a software graphics provider (SX-3c; scenario only, never on an image or the stick; `out/` ignored) | `steamrt/build.sh` |

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
| `bubblewrap-0.13.1.tar.xz` | https://github.com/containers/bubblewrap/releases/download/v0.13.1/bubblewrap-0.13.1.tar.xz (LGPL-2.0-or-later) | `0e85c8702248b8dd64144f594147553650cdd0e4c951690345faace79c217c4d` | Georg 28.10.2026 |
| `meson-1.12.1.tar.gz` | https://github.com/mesonbuild/meson/releases/download/1.12.1/meson-1.12.1.tar.gz (Apache-2.0; build tool of bubblewrap and Xwayland) | `ab0a6ca09f8ef70c564c8241fb5a23957886a0b53fb58412b5e07eaf07dba743` | Georg 28.10.2026 |
| `bash-5.3.tar.gz` | https://ftp.gnu.org/gnu/bash/bash-5.3.tar.gz (GPL-3.0-or-later; bubblewrap's test suite only) | `0d5cd86965f869a26cf64f4b71be7b96f90a3ba8b3d74e27e8e9d9d5550f31ba` | Georg 24.10.2026 rule (the project's own site) |
| `xwayland-24.1.14.tar.xz` | https://xorg.freedesktop.org/releases/individual/xserver/xwayland-24.1.14.tar.xz (MIT/X11) | `4eb6b98d678299a4b96138d886345b294130695760345ef84596225a8224078a` | Georg 28.10.2026 |
| `xtrans-1.6.0.tar.xz` | https://xorg.freedesktop.org/releases/individual/lib/xtrans-1.6.0.tar.xz (MIT/X11) | `faafea166bf2451a173d9d593352940ec6404145c5d1da5c213423ce4d359e92` | Georg 28.10.2026 |
| `xkbcomp-1.5.0.tar.xz` | https://xorg.freedesktop.org/releases/individual/app/xkbcomp-1.5.0.tar.xz (MIT/X11) | `2ac31f26600776db6d9cd79b3fcd272263faebac7eb85fb2f33c7141b8486060` | Georg 28.10.2026 |
| `wayland-protocols-1.47.tar.xz` | https://gitlab.freedesktop.org/wayland/wayland-protocols/-/releases/1.47/downloads/wayland-protocols-1.47.tar.xz (MIT) | `5fd4349bcbc9bab9a46f8cf77d1f434296a7a052c87440a094f63fcf62a58e20` | Georg 28.10.2026 |
| `SteamLinuxRuntime_sniper-3.0.20261021.266025.tar.xz` | https://repo.steampowered.com/steamrt-images-sniper/snapshots/3.0.20261021.266025/SteamLinuxRuntime_sniper.tar.xz (many packages, each with its own open-source licence; pressure-vessel MIT with LGPL-2.1-or-later and LGPL-2.0-or-later parts) | `86956ad0ee62e89134f5e30359cd1019d1ecb614c7b677dad82d4d00f51a2696` | Georg 28.10.2026 |
