# mesa: Mesa, Vulkan and their tools for BitsOS V2 (glibc tree)

The distribution's ready-made Mesa, packaged as one self-contained glibc tree in
`\BITSOS2\mesa\` (V2: `/boot/BITSOS2/mesa`), the way Brave's tree is: its own
`ld-linux-x86-64.so.2`, glibc and every library, so it never mixes with the
system's libraries. Our own trimmed Mesa build comes later (GM-1b).

| Path | What |
|---|---|
| `scripts/stage-v2.sh` | builds the tree into `out/mesa/` (git ignores `out/`) from pacman's cache, each package checked against its signature; nothing is installed, no root |
| `bitsos/run` | the start script inside the tree: `run vkcube --wsi wayland`, `run --check` |
| `bitsos/mesa-run` | the stick's `\BITSOS2\bin\mesa-run`: the tree's `run` from anywhere |
| `scripts/deqp-vk.sh` | builds Khronos's conformance runner deqp-vk from the pinned CTS tag into `out/deqp/tree/` (for `\BITSOS2\deqp\`) |
| `scripts/stage-stick.sh` | both, into `/var/tmp/claude-v2/stage/{mesa,deqp}`, where the stick manifest reads them |
| `../../../host/tests/gm/` | the failed-call logger, the DRM tracer (`tools/`, `30-drmtrace-tools.sh`); `80-vainfo-stage.sh` the VA-API check on the host |

## What is in the tree

- Drivers: the GPU's only (DECISIONS 18.11.: everything draws on the GPU): radeonsi (OpenGL, GLES, EGL, VA-API) and
  RADV (Vulkan). No swrast/kms_swrast and no lavapipe; libgallium still holds llvmpipe (Arch builds one libgallium for
  every gallium driver), but nothing in the tree selects it. Without a render node (`/dev/dri`) nothing draws.
- libglvnd (`libEGL`, `libGLESv2`, `libGL`), the Vulkan loader, libdrm, Wayland.
- Programs: `vulkaninfo`, `vkcube`, `vkgears`, `eglinfo`, `eglgears_wayland`, `egltri_wayland`, `es2gears_wayland`,
  `peglgears` (of mesa-utils everything without X11 and KMS), `vainfo`.
- VA-API (GM-3v): libva (`libva.so.2`, `libva-drm.so.2`, `libva-x11.so.2`, `libva-wayland.so.2`), `vainfo` of
  libva-utils, and Mesa's VA driver `lib/dri/radeonsi_drv_video.so`.

`run` sets `LD_LIBRARY_PATH`, `VK_DRIVER_FILES`,
`__EGL_VENDOR_LIBRARY_FILENAMES`, `LIBGL_DRIVERS_PATH`, `GBM_BACKENDS_PATH` and
`DRIRC_CONFIGDIR` to the tree, keeps Vulkan layers to the tree (it has none), and
starts the program through the tree's loader. Vulkan is RADV (`radeon_icd.json`,
the tree's one driver). For VA-API it sets `LIBVA_DRIVERS_PATH` to `lib/dri`
(libva's built-in `/usr/lib/dri` would load the host's driver) and
`LIBVA_DRIVER_NAME=radeonsi`, the tree's one VA driver.

## VA-API: the driver and the codecs

- Arch's `radeonsi_drv_video.so` is a link to `libgallium-<version>.so`, where
  `__vaDriverInit_1_24` lives. FAT has no links, so `stage-v2.sh` writes a
  stand-in of 9 KB with `ld`: no code, its one `DT_NEEDED` is libgallium's
  soname. libva's `dlsym` on the stand-in's handle searches its dependencies and
  finds the entry in libgallium. A copy would cost 65 MB more on the stick (and in
  `ul.pkg.mesa_sha`) and would load libgallium twice into a program that does GL
  and VA (two winsys on one device); the stand-in shares the one libgallium with
  `radeonsi_dri.so`, as Arch's link does.
- Codecs: the pacman Mesa is built with `-Dvideo-codecs=all`, so the tree has
  H.264, H.265 and VC-1 (Mesa's "patent codecs") besides AV1, VP9, MPEG-1/2 and
  JPEG. That is for Georg's own stick. `stage-v2.sh` refuses a Mesa without the
  H.264/H.265 signs in libgallium (d3d12's H.264/H.265 decoder and encoder names,
  compiled only with those codecs); `MESA_FREE_CODECS_OK=1` lets a free build
  through on purpose.
- Our own build script (GM-1b, not written yet) keeps Mesa's default
  `video-codecs=all_free` (av1dec, av1enc, vp9dec, mpeg12dec, jpegdec): no H.264,
  H.265 or VC-1. Open for the desk: `xtask update-release` and `xtask
  release-assets` pack this pacman stage as the mesa tree, so a release others
  install would carry the patent codecs; until that is decided such a release
  leaves out mesa together with xwayland, vkquake and steam (they run on it) or
  packs a free build (plan/guides/release.md, section A).
- Without a render node VA-API has nothing to run on (Mesa's VA driver with a DRM
  display needs a node). `run vainfo --display drm` then
  says `failed to initialize display 'drm'` and ends with 1; that is the plain
  answer, not a fault. Always pass `--display drm`: without it vainfo tries
  Wayland and X11 first.

## Make and test

```sh
bash user/ports/mesa/scripts/stage-v2.sh      # about 300 MB, seconds from the cache
bash host/tests/gm/80-vainfo-stage.sh         # host, no device: the VA files, vainfo's plain answer, the driver loads
```

The packages are the host's (CachyOS: x86-64-v3 builds, AVX2). Run the stage
again after a Mesa update. The trees are in the stick manifest since GM-1d
(`bash user/ports/mesa/scripts/stage-stick.sh` first, or every image build
says `tree mesa not on the stick`); images carry them only with
`V2_STICK_TREES`. On the Mini-PC: the ktests `ul.pkg.mesa*` and `ul.pkg.deqp` (on RADV,
the 780M), and the check list `plan/guides/games.md`.
