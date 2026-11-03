# mesa: Mesa, Vulkan and their tools for BitsOS V2 (glibc tree)

The distribution's ready-made Mesa, packaged as one self-contained glibc tree in
`\BITSOS2\mesa\` (V2: `/boot/BITSOS2/mesa`), the way Brave's tree is: its own
`ld-linux-x86-64.so.2`, glibc and every library, so it never mixes with the
system's libraries. Our own trimmed Mesa build comes later (GM-1b).

| Path | What |
|---|---|
| `scripts/stage-v2.sh` | builds the tree into `out/mesa/` (git ignores `out/`) from pacman's cache, each package checked against its signature; nothing is installed, no root |
| `bitsos/run` | the start script inside the tree: `run vkcube --wsi wayland`, `run --check` |
| `../../../host/tests/gm/` | the QEMU scenario (`10-mesa.sh`), its host side and the failed-call logger |

## What is in the tree

- Drivers: llvmpipe (OpenGL, GLES, EGL), lavapipe (Vulkan), RADV and radeonsi
  (they need a render node, `/dev/dri`, and step aside without one).
- libglvnd (`libEGL`, `libGLESv2`, `libGL`), the Vulkan loader, libdrm, Wayland.
- Programs: `vulkaninfo`, `vkcube`, `eglinfo`, `eglgears_wayland`, `es2gears_wayland`.

`run` sets `LD_LIBRARY_PATH`, `VK_DRIVER_FILES`/`VK_ICD_FILENAMES`,
`__EGL_VENDOR_LIBRARY_FILENAMES`, `LIBGL_DRIVERS_PATH`, `GBM_BACKENDS_PATH` and
`DRIRC_CONFIGDIR` to the tree, keeps Vulkan layers to the tree (it has none), and
starts the program through the tree's loader. `MESA_VK_DRIVERS=lvp_icd` or
`radeon_icd` picks one Vulkan driver.

## Make and test

```sh
bash user/ports/mesa/scripts/stage-v2.sh      # about 300 MB, seconds from the cache
bash host/tests/gm/10-mesa.sh                 # QEMU: vulkaninfo, eglinfo, vkcube, gears in bitscomp
```

The packages are the host's (CachyOS: x86-64-v3 builds, AVX2). Run the stage
again after a Mesa update. The tree is not in the stick manifest yet: the
scenario puts it onto its own test image.
