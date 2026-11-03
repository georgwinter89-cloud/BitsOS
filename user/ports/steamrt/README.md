# user/ports/steamrt: the Steam Linux Runtime 3.0 (sniper) for the V2 scenario (SX-3c)

`build.sh` puts Valve's runtime tree with pressure-vessel and a software graphics provider into `out/` (git
ignores it, ~1.1 GB); `host/tests/sx/20-pressure-vessel.sh` streams both to V2 in QEMU and runs `true` and
`steam-runtime-system-info` in the container; `record-host.sh` makes the same runs on CachyOS as the reference.
Nothing of it goes into the image or onto the stick.

| Path | What |
|---|---|
| `out/SteamLinuxRuntime_sniper/` | Valve's tree as published (`run`, `pressure-vessel/`, `sniper_platform_<version>/`), the arm64 pressure-vessel left out |
| `out/gfx/` | the graphics provider (`PRESSURE_VESSEL_GRAPHICS_PROVIDER`): an Arch-shaped sysroot with the GM-1a Mesa tree (llvmpipe, lavapipe, RADV, radeonsi, glvnd, the Vulkan loader; `user/ports/mesa/scripts/stage-v2.sh`, staged into `out/mesa-stage/`) and the whole glibc of the same signature-checked package, `usr/lib` + `lib`/`lib64` links, `etc/ld.so.cache` from `ldconfig -r` |

## Download (download cache `$V2_DOWNLOADS`, SHA256 checked on every build)

| File | Source | SHA256 | Licence |
|---|---|---|---|
| `SteamLinuxRuntime_sniper-3.0.20261021.266025.tar.xz` | https://repo.steampowered.com/steamrt-images-sniper/snapshots/3.0.20261021.266025/SteamLinuxRuntime_sniper.tar.xz | `86956ad0ee62e89134f5e30359cd1019d1ecb614c7b677dad82d4d00f51a2696` (= the snapshot's SHA256SUMS) | many packages, each its own open-source licence (README.md of the tree; texts in `files/share/doc/*/copyright`); pressure-vessel and steam-runtime-tools MIT with LGPL-2.1-or-later (libcapsule, Flatpak) and LGPL-2.0-or-later (bubblewrap) parts (`pressure-vessel/metadata/README.txt`); sources at https://repo.steampowered.com/steamrt3/images/ |

The Mesa tree and glibc come from pacman's cache (CachyOS's packages, each checked against its signature with
pacman's keyring), as GM-1a's stage does; their licence texts are in `out/gfx/usr/share/licenses/`.

## Why a provider tree

The sniper runtime has Mesa's GLX/EGL front ends but no drivers; pressure-vessel takes the drivers, and the newer
glibc with all of its libraries and `ldconfig`, from the graphics provider. V2's root has no Mesa and only the
glibc libraries its programs link, so the provider is its own small sysroot (on CachyOS too, so both sides use
the same drivers). The Vulkan manifests get Arch's absolute `/usr/lib/<driver>` paths (the GM-1a tree names a
bare soname because its `run` script sets the search path).
