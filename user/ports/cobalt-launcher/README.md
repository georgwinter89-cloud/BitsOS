# cobalt-launcher: YouTube TV (Cobalt 27) for BitsOS V2

Cobalt's own Linux build (loader_app + our GL-free libcobalt) with a small
V2 backend: picture on the Linux framebuffer, input from evdev, sound through
the existing ALSA/Pulse code. Plan: `plan/V2-LANES.md` (YT), `plan/spec/gpu.md`
§6. State and numbers: `plan/notes/YT-1.md`.

## Layout

| Path | What |
|---|---|
| `patches/0001-0005` | libcobalt side, GL-free (V1 YouTube plan, unchanged) |
| `patches/0006` | Linux test side: `dev.bitsos.Canvas` via X11 (the debug backend) and the failing stub EGL (unchanged) |
| `patches/0007` | the V2 backend (this item): `starboard/shared/bitsos/fb/*` + dispatch in the Linux platform, AC-3 off, `VideoFrame::is_cpu_frame()` |
| `src/fb/` | the backend sources (the copy in the tree is synced from here) |
| `scripts/` | build, run, test helpers (most run inside the lane container) |
| `scripts/stage-v2.sh` | YT-2: the stick tree `\BITSOS2\cobalt\` from the build (manifest `tree cobalt`) |
| `bitsos/cobalt` | YT-2: the start command on V2 (`cobalt`, `cobalt --check`), in `\BITSOS2\bin\` |
| `../../../host/tests/yt/` | host unit tests and the offline end-to-end test |

The series applies to Cobalt tag `27.lts.3` (commit 3befd332), in order.
`scripts/export-patches.sh` writes 0007 from the copy and proves that
0001..0007 on the pristine files give exactly the copy's files.

## Choosing the backend (environment)

| Variable | Meaning | Default |
|---|---|---|
| `BITSOS_BACKEND=fb` | framebuffer + evdev (V2); unset = X11 debug backend | X11 |
| `BITSOS_FB` | framebuffer device (a character device with major 29, else refused before opening), or the fake file | `/dev/fb0` |
| `BITSOS_FB_FAKE=WxH` | `BITSOS_FB` is a plain file (BGRX, no ioctls); refused in `/dev`, `/proc`, `/sys` (also through links or `..`), refused if a link or not a plain file | off |
| `BITSOS_WINDOW=WxH` | Cobalt's window, at most 1920x1080 | 1920x1080 |
| `BITSOS_EVDEV` | input paths/globs, `:`-separated; FIFOs and files carry raw `input_event` records | `/dev/input/event*`; with `BITSOS_FB_FAKE`: none |
| `BITSOS_EVDEV_GRAB` | take real devices exclusively | 1; with `BITSOS_FB_FAKE`: 0 |
| `BITSOS_KEYMAP` | `de` (QWERTZ) or `us` | de |
| `BITSOS_CURSOR` | `auto` (after the first mouse use), `on`, `off` | auto |
| `BITSOS_AV_STATS` | YT-5: file for the A/V numbers (frames shown/dropped, frame size, picture against the audio clock), replaced at most once a second while video plays (`src/fb/av_stats.h`) | off; the V2 launcher sets `<run dir>/av-stats.txt` |
| `BITSOS_AV_TRACE` | YT-5: one line per shown frame (`shown <monotonic us> <pts us> <offset us>`) | off; the launcher sets it with `COBALT_AV_TRACE=1` |
| `BITSOS_KD_GRAPHICS` | put the controlling VT into graphics mode while running; text mode comes back at exit, on crash signals (console guard) and, as a backstop, from `georg-vt-check.sh` | 0 |

Keys: arrows, Enter, Escape (back) and the rest of a keyboard; a gamepad
becomes remote keys (A = Enter, B = Escape, d-pad/hat/left stick = arrows,
X = play/pause, shoulders = rewind/forward). A mouse moves a drawn pointer;
an absolute pointer (tablet such as QEMU's usb-tablet, touch screen) points
directly; touchpads are not supported. A device pulled out lets go of
whatever it held; one that could not be opened yet is tried again every 2 s.
Ctrl+Alt+Backspace ends Cobalt (needed because a grabbed keyboard cannot
switch VTs).

## Commands

Host (CachyOS, no Cobalt tree needed):

    host/tests/yt/run-host-tests.sh     # unit tests, ASan/UBSan, speed, PNG tool

Lane container `cobalt-yt-v2` (the copy at `/work`, this worktree at `/lane`):

    user/ports/cobalt-launcher/scripts/sync-into-tree.sh   # src/fb -> copy
    user/ports/cobalt-launcher/scripts/build.sh            # loader_app, -j8, nice
    host/tests/yt/fb-e2e.sh                                # offline end-to-end; exit 0 = pass
    podman exec cobalt-yt-v2 bash /lane/user/ports/cobalt-launcher/scripts/session-yt.sh <name> fb|x11 <seconds>
    podman exec cobalt-yt-v2 bash /lane/user/ports/cobalt-launcher/scripts/nplb-yt.sh <name> fb|x11

    user/ports/cobalt-launcher/scripts/licenses-v2.sh       # YT-5: licence texts for the stick tree (<copy>/lic)
    host/tests/yt/20-avsync.sh                             # YT-5, QEMU: click/flash clip, A/V numbers, format answers

`nplb-yt.sh` is a gate: exit 0 only if the run reached its end and every
failure is in `host/tests/yt/nplb-reference-fails.txt` (the V1 M0 reference
run). Build only `loader_app` and `elf_loader_sandbox`: the `nplb` target
rebuilds the whole test suite (4,700 steps), and `libnplb.so` does not
contain the backend.

Ports: DevTools 29222, test page 29080, Xvfb display :290 (lane range
29000-29499). Run folders: `<copy>/runs-yt/<name>/` (`fb0` = fake screen,
`input` = fake evdev FIFO; `fbshot.py` makes a PNG (`--pitch` for padded
rows), `evsend.py` sends keys).

## On V2 (YT-2)

After a Cobalt rebuild run `scripts/stage-v2.sh`; `cargo xtask image` then puts the tree into
`\BITSOS2\cobalt\` (ktest images, or any image with `V2_STICK_TREES=1`) and the launcher into
`\BITSOS2\bin\cobalt`. On V2: `cobalt` in the BusyBox shell (over MCP: `/bin/sh /boot/BITSOS2/bin/cobalt`,
since MCP `run` takes ELF files only). Details and the sign-in rule: `plan/notes/yt-YT-2.md`.
Started while the desktop runs (e.g. in a foot terminal) it goes on in the background, ends bitscomp through
its control socket, runs Cobalt and starts bitscomp again with the same arguments and environment. The
sign-in is kept as one file, `\BITSOS2\home\cobalt_files.tar` (plus `.old`, the copy before).
`cobalt --stats` prints the A/V numbers of the running or last session (YT-5, `plan/notes/yt-YT-5.md`).
Cobalt never runs as root (YT-6): started by root (the console shell, a terminal of a desktop started from the
console) the launcher hands it to uid 1000 with BusyBox `chpst -u 1000:1000` and says so; its run directory is
`/run/user/1000/cobalt`, and root makes or writes nothing in it itself (every step there runs as uid 1000; the
desktop's start line stays in a root-only `mktemp -d` directory). If root cannot hand over it refuses in German and English (`COBALT_ALLOW_ROOT=1` runs it
as root anyway, for diagnosis). Scenario: `host/tests/yt/30-root.sh`; notes `plan/notes/yt-YT-6.md`.
Formats on BitsOS: video up to 1920x1080 (either orientation), SDR only, no AC-3/E-AC-3 (patch 0007).
