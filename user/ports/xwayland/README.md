# Xwayland for BitsOS V2 (WY-3)

X11 programs run on bitscomp through Xwayland (rootless). This port builds the Xwayland tree that goes to
`\BITSOS2\xwayland\` on the stick; bitscomp starts it when the first X11 client connects to `DISPLAY=:0`.

    XW_TEST=1 bash user/ports/xwayland/scripts/build-v2.sh    # the tree in $CARGO_TARGET_DIR/wy-xwayland/xwayland

- WY-3b: built with glamor, DRI3 and GLX. bitscomp starts it with them when it draws on the chip itself and names a render
  node that opens, else with `XWAYLAND_NO_GLAMOR=1`, the software Xwayland; a glamor start that fails or a node that is lost costs an
  X11 client nothing (`user/services/bitscomp/src/xwayland.rs`). `--xwayland-glamor off` on bitscomp's command line
  switches it off. Without glamor there is no GL for X11 programs: the server has no GLX provider, they say so and end.
- `clients/` holds `glxgears`, `glxinfo`, `xdpyinfo` from the distribution's signed packages, started with `run` (the tree's
  `bitsos/run`), which puts this tree's `lib/` before the Mesa tree's: `libGLX_mesa.so.0` lies here, and an X11 GL client's
  libGLX loads it by name. `host/tests/minipc/x11gpu.sh` uses them on the stick.
- `bin/Xwayland` and `bin/xkbcomp` are built from the X.Org source releases with the host's compiler. Their
  PT_INTERP is the Mesa tree's loader (`/boot/BITSOS2/mesa/lib/ld-linux-x86-64.so.2`) and their DT_RPATH
  `/boot/BITSOS2/xwayland/lib:/boot/BITSOS2/mesa/lib`, as bitscomp's gpu build: the tree uses the Mesa tree's glibc,
  never the initramfs one. Only the final link gets these (meson and configure run test programs on the host).
- `lib/` holds what both link that the Mesa tree lacks (the host's files, by soname, no links: FAT has none);
  `share/X11/xkb/` is xkeyboard-config's data (xkbcomp compiles the keymap bitscomp sends); `LICENSES/`, `SOURCES.txt`,
  `LIBS.txt` say where everything came from.
- `XW_TEST=1` adds `test/bin` and `test/lib` with the test clients (xterm, xeyes, xclock, xdpyinfo, xinput, xrandr,
  xclip, wl-copy, wl-paste) from the distribution's signed packages. They are never put on the stick; the scenarios
  start them through the Mesa tree's loader (`ld-linux --library-path test/lib:lib:/boot/BITSOS2/mesa/lib`).
- `V2_OFFLINE=1` refuses downloads; sources are cached in `~/.cache/bitsos-v2/downloads` (`V2_DOWNLOADS`).

## Downloads

| File | Source | SHA256 (as published by the project) | Licence |
|---|---|---|---|
| `xwayland-24.1.14.tar.xz` | https://xorg.freedesktop.org/releases/individual/xserver/xwayland-24.1.14.tar.xz | `4eb6b98d678299a4b96138d886345b294130695760345ef84596225a8224078a` (xorg-announce 2026-October/003749) | MIT/X11 (COPYING) |
| `xtrans-1.6.0.tar.xz` (headers, build only) | https://xorg.freedesktop.org/releases/individual/lib/xtrans-1.6.0.tar.xz | `faafea166bf2451a173d9d593352940ec6404145c5d1da5c213423ce4d359e92` (xorg-announce 2025-March/003588) | MIT/X11 |
| `xkbcomp-1.5.0.tar.xz` | https://xorg.freedesktop.org/releases/individual/app/xkbcomp-1.5.0.tar.xz | `2ac31f26600776db6d9cd79b3fcd272263faebac7eb85fb2f33c7141b8486060` (xorg-announce 2025-December/003645) | MIT/X11 |
| `wayland-protocols-1.47.tar.xz` (XML, build only) | https://gitlab.freedesktop.org/wayland/wayland-protocols/-/releases/1.47/downloads/wayland-protocols-1.47.tar.xz | `5fd4349bcbc9bab9a46f8cf77d1f434296a7a052c87440a094f63fcf62a58e20` (recorded at download; the .sig's key is not in a local keyring) | MIT |
| `meson-1.12.1.tar.gz` (build tool) | https://github.com/mesonbuild/meson/releases/download/1.12.1/meson-1.12.1.tar.gz | `ab0a6ca09f8ef70c564c8241fb5a23957886a0b53fb58412b5e07eaf07dba743` | Apache-2.0 |

Packages (pacman's cache or the configured mirror, each checked against its signature with pacman's keyring):
`xorgproto` (headers, build only), `xkeyboard-config` (the xkb data), `mesa-utils`, `xorg-xdpyinfo` (`clients/`), and for `XW_TEST=1` only `xterm`, `xorg-xeyes`,
`xorg-xclock`, `xorg-xdpyinfo`, `xorg-xinput`, `xorg-xrandr`, `xclip`, `wl-clipboard`. The exact files are listed in
the tree's `SOURCES.txt`.

Build dependencies of glamor from the host: `libepoxy`, `libdrm`, `mesa` (gbm), `libglvnd`, `libxshmfence` (headers and link
only: every file the tree runs with comes from `lib/` or the Mesa stage, which `build-v2.sh` checks). `libepoxy.so.0` and
`libGLX_mesa.so.0` are the host's files, as the rest of `lib/`; the host's Mesa must be the stage's (the build says so).
