# vkquake: the first native Linux game on BitsOS V2 (milestone 3)

vkQuake and SDL2 built from their pinned releases, with LibreQuake's free data, as the tree `\BITSOS2\vkquake\` on the stick. It
runs on the glibc, Vulkan loader and RADV of `\BITSOS2\mesa\`: one needs the other. Game data never in git.

| Path | What |
|---|---|
| `scripts/stage-v2.sh` | fetches and checks the pinned releases, builds, fills `/var/tmp/claude-v2/stage/vkquake` (the manifest's `tree vkquake`); `out/` is git-ignored |
| `patches/` | the one change to vkQuake: SDL2 asks for the output's real pixels |
| `bitsos/vkquake` | the launcher, `\BITSOS2\bin\vkquake`: `vkquake`, `vkquake --check`, `vkquake --env`, `vkquake -nosound +map start` |

Tests: `host/tests/gm/50-vkquake.sh` (launcher on made-up trees), `51-vkquake-window.sh` (the tree on the build host, lavapipe in a
headless bitscomp), `ul.pkg.vkquake*` (the stick), `host/tests/minipc/quake.sh` (the chip). Notes: `plan/notes/gm-GM-8.md`.
