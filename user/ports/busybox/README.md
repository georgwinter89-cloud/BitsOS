# BusyBox for BitsOS V2 (UL-3a)

`build.sh` makes `out/busybox` (static-pie, musl 1.2.6 from `user/sysroot`, `-march=x86-64-v3`, stripped) and
`out/applets` (the applet names). Version, URL and SHA256 are pinned in `build.sh`; the tarball comes from the
download cache (`../README.md`). Config: busybox's `defconfig` plus `config.fragment` (each of its lines must
survive `oldconfig`, else the build stops); the full result is kept as `out/config`.

The image (xtask) puts `busybox` into `/bin` and every applet as a link `/bin/<name> -> busybox` (a real V2
program of the same name wins). init starts `/bin/sh` on the console as a session leader with the console as
controlling terminal, so ash has job control.

## Choices (config.fragment)

| Choice | Why |
|---|---|
| `CONFIG_STATIC`/`CONFIG_PIE` off | `v2-cc` links static-pie itself; busybox's own switches would fight it |
| `FEATURE_PREFER_APPLETS`, `FEATURE_SH_STANDALONE` off | applets run through the `/bin` links, never by re-executing `/proc/self/exe` |
| `FEATURE_USE_SENDFILE` off | V2 has no `sendfile` yet |
| init, linuxrc, halt, poweroff, reboot, getty, bootchartd off | PID 1 is our init; no power switching from a shell |
| devmem, fdisk, mkfs.*, mke2fs, mkswap, fsck, blkdiscard, hdparm, partprobe, losetup, swapon/off, watchdog, hwclock, rtcwake, adjtimex, i2c*, nand*, ubi*, setserial, raidautorun, fsfreeze, fstrim, fatattr, freeramdisk, fdflush, fdformat off | nothing on V2 needs them; the kernel's guards (`nvme_write`, `stick_write`) stay the only way to write a disk, and Georg's stick and NVMe never meet mkfs or fdisk by a typo |
| `tc` off | does not build against current Linux UAPI headers (CBQ removed in 6.8) |
| ash with editing, history 255, saved history, tab completion, job control | the console shell |

## Tests

`tests/<case>.sh` run as `sh -c <case> sh <scratch dir>` with `PATH` = the applet links, stdin empty.
`tests/<case>.out` is the transcript on CachyOS (`tests/run-host.sh --update` writes it, `tests/run-host.sh`
compares; scenario `host/tests/ul/20-busybox-linux.sh`). On V2 the ktests `ul.bb.<case>` compare the same files;
`tests/<case>.known` lists lines that may be missing on V2, each with its owner (the to-do list for other lanes).
`ul.bb.console_shell` (QemuOnly) checks the console shell's session and job control.

Cases (UL-3a: `pipeline files jobs system net errors`; UL-3b, the A2 subset: `text coreutils procs netbasics
applets root`). A case whose first line is `# as: root` runs as uid 0 on V2 and, on the host, as root of new user,
mount, net and uts namespaces (`unshare --user --map-root-user --mount --net --uts`), so it never touches the host.
A line that depends on a V2 gap is written so that the gap makes it *missing* (`... && echo name`, `| grep -x`),
never different: `.known` can excuse a missing line only. ENOSYS calls show up once per boot in the kernel log
(`syscall enosys nr=... program=...`); `plan/notes/ul-UL-3b.md` has the list by applet and owner.
