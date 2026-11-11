# Mini-PC runs (Georg 30.10.: programs are tested only on the Mini-PC; TS-1: every test has a run)

The desk runs these against the real machine over MCP (TLS, the arguments of `host/mcp/mcp-call.mjs`); no gate starts
them. Rule: `plan/CONTRACTS.md` section 4. The kernel must be one with ktests: the lean stick kernel has none, Reload first.

| Script | Run | What |
|---|---|---|
| `desktop.sh <host[:port]> --tls auto --token-file F [--generation N] [--record]` | `desktop` | every test, the desktop running. Sets `kernel.loglevel` to info and back to the value `settings_get` gave. Screen tests show as skip |
| `bare.sh <host[:port]> --tls auto --token-file F [--generation N] [--record]` | `bare` | dp., in., the screen tools with the desktop stopped: `bitscomp ctl quit`, a look that nothing holds /dev/fb* or /dev/input/event*, afterwards `bitscomp` typed into the console shell (red if it does not come back); no reboot, no setting on the stick. Log level as above. In a boot with CONFIG `desktop=off` nothing is stopped or started |
| `lo.sh <host[:port]> --tls auto --token-file F [--desktop]` | | LibreOffice over and above its ktests |
| `data-move.sh <host[:port]> --tls auto --token-file F` | | NV-2p: the stick's data.img contents to the new /data on the SSD, once, after Georg's bitsdisk; plan/notes/nv-NV-2p.md |
| `blk0.sh <host[:port]> --tls auto --token-file F [--label idle\|x4] [--state BL-0]` | | BL-0: the read-path numbers of plan/notes/desk-W35-mm-blk.md (blkbench seq, rand QD1/x32, small, mmap; CPU per `blk:` thread, lock waits, copy and in-flight counters) and the table row; refuses unless /data is the Lexar partition; needs a kernel with ktests (its cold step is ktest fs.drop_caches.all until MCP gets a root path to /proc/sys/vm/drop_caches, bl-BL-0.md CR 2). `blk0.sh --proof` runs it against a QEMU guest of lane BL (no numbers); plan/notes/bl-BL-0.md |
| `vk.sh <host[:port]> --tls <key store> --token-file F` | | milestone 1: Mesa and deqp on the real 780M (GM-1d); the first steps are MCP `ktest ul.pkg.mesa`, `ul.pkg.mesa_sha`, `ul.pkg.deqp` |
| `steam.sh <host[:port]> --tls <key store> --token-file F` | | Steam's login window on the real 780M under Xwayland (GM-2a); `steam --check`, the start, the stages, pictures, ENOSYS calls; `--ktest` runs `ul.pkg.steam` first, Georg logs in himself |
| `quake.sh <host[:port]> --tls <key store> --token-file F` | | milestone 3: vkQuake fullscreen on the real 780M (GM-8), silent (`-nosound`); `ul.pkg.vkquake`, `vkquake --check`, the window, its size, the picture, the chip's job counters, the close |
| `x11gpu.sh <host[:port]> --tls <key store> --token-file F` | | X11 on the real 780M (WY-3b): `xdpyinfo` lists DRI3, `glxinfo -B` names the Radeon, `vkcube --wsi xcb` and glxgears under Xwayland: windows, commits, dma-bufs, the chip's job counters, close, GX-D9 |

`ran.txt` is what `cargo xtask test`'s ledger counts for the tests only this machine can run (classes Program, Hardware,
Picture, WithGeorg): `--record` adds a round's passes. A test without a record, or one older than 30 days, is the line
"stale" in the gate's report, not red. `Picture` and `WithGeorg` stay out of these scripts: they are refused unless CONFIG
has `ktest_picture=on` (when GX-D9 is lifted) or `ktest_georg=on` (Georg listens to the tone, his headset is plugged in),
and both scripts refuse such a CONFIG; such a round is by hand: `ktest {"filter":"gx.flip.,gx.mode.roundtrip,sn.dpaudio.,sn.usb.capture"}`.

`selftest.py` (a step of `cargo xtask test`) runs these scripts against a stand-in for the machine: the machine put back after
a red run, the judgement, the record file. Not the machine itself: the real stop and start are `host/tests/lo/20-desktop-stop.sh`
in QEMU, and the first round.

Program trees carry their own list of files, `PKGLIST.TXT`, written with the tree (`V2_STICK_TREES`, `cargo xtask image`):
copy the tree from such an image, and the tree's tests compare the stick with that list. A tree copied any other way (a stage folder, a
stick older than this rule) has none and its tests FAIL: `/boot/BITSOS2/cobalt/PKGLIST.TXT` missing means Cobalt was left out of the last copy.
Over the network, from the Mini-PC's point of view only what changed goes (`--dry-run` first):
`V2_STICK_TREES=cobalt cargo xtask image --name cobalt-tree`, then
`node host/mcp/push.mjs <ip> --tree target/xtask/esp-cobalt-tree/BITSOS2/cobalt --bind <this PC's address> --token-file <F>`.
`host/tests/yt/tree-list.sh` proves the list on the build host.

The QEMU scripts that booted program trees (Georg's rule took them out of every run) were deleted by TS-1, not ported:
they lived on what only QEMU gives (QMP keys, the WAV file of the sound, a host reference picture). What a person checks
on the Mini-PC stays in the check lists:

| Deleted | Check list |
|---|---|
| `ac/10-actions.sh` | `plan/notes/ac-AC-3.md` |
| `br/10-start.sh` | `plan/notes/br-BR-3.md` |
| `br/40-download.sh` | `plan/notes/br-BR-7.md` |
| `br/50-sandbox.sh` | `plan/notes/br-BR-6b.md` |
| `yt/10-nplb.sh` | `plan/notes/yt-YT-4a.md` |
| `yt/20-avsync.sh` (plays sound) | `plan/notes/yt-YT-5.md` |
| `yt/30-root.sh` (plays sound, types at the console) | `plan/notes/yt-YT-6.md` |

What covers the same ground in a Mini-PC run: only the level below. `ul.pkg.brave*` and `ul.pkg.cobalt` prove the tree and its
libraries, `ul.pkg.libreoffice*` also a headless conversion. Brave starting, its download and its sandbox (BR-3, BR-7, BR-6b),
the actions (AC-3), NPLB, A/V sync and the start as root (YT-4a, YT-5, YT-6) have no scripted run now, only these lists.

Stay where they were: `ul.pkg.cobalt_profile` (QemuOnly: the launcher with a stub tree, no program), `ul.pkg.stick`
(ninja, no tree), the build-host checks `lo/05-stage.sh`, `br/20-launcher.sh`, `br/25-stage.sh`, `br/30-bench.sh`,
`pk/30-brave-package.sh`, `yt/launcher.sh`, `fs/31-fat-cold.sh` (a stand-in file in QEMU, no program).
Xwayland and Mesa count as the desktop's own pieces and stay with WY's scenarios.

`vk.sh` (GM-1d, milestone 1) is the first script here that speaks TLS (`--tls <key store> --token-file <file>`) and that
QEMU can prove: `host/tests/gm/40-vk-proof.sh`, started by hand (`# minipc:`, it boots the deqp tree, so no regular run
does), runs it with `--plain --profile qemu` against the fake GPU and lavapipe.
