# BitsOS

We have no idea what we're doing.<br>
But it boots.<br>
...and plays sounds.<br>
...and connects to the internet.<br>
...and runs Brave.<br>
...and somehow it still boots.

---

BitsOS is a hobby operating system with its own kernel written in Rust. It speaks the Linux system-call
interface, so ordinary Linux programs (BusyBox, a Wayland compositor, the Brave browser) run on it unchanged.

It is built by someone who cannot program, together with AI agents (Anthropic's Claude) that write, review and
test the code. A line only goes into the list above once it is true on the real hardware.

## What is in it

- **Kernel** (`kernel/`, `crates/`): memory management with a page cache, a preemptive scheduler for all cores,
  processes and threads, a VFS with FAT32 and ext4, snapshots of the data volume, a kernel journal, seccomp
  and namespaces in progress.
- **Drivers**, written for BitsOS: NVMe, xHCI USB (keyboards, mice, mass storage, USB audio),
  Intel I226 Ethernet, Realtek RTL8852BE Wi-Fi with WPA2 (ported from Linux's rtw89, see `NOTICE.md`), HD Audio over HDMI/DisplayPort, the AMD Radeon 780M
  display engine (mode setting, hardware cursor), TPM, ACPI.
- **Boot loader** (`boot/bitsboot`): UEFI, generations with automatic fallback to the last good one.
- **User space** (`user/`): init, `bitscomp` (Wayland compositor), `settingsd`, `bitsjournal`, `ctxd`
  (a local context index), and ports of BusyBox, Brave, Cobalt and ffmpeg.
- **Remote control** (`crates/mcp`, `host/mcp`): an MCP server in the kernel, so an AI agent on another machine
  can read logs, inspect hardware and install a new generation, all behind a token.
- **Tests** (`host/xtask`, `host/tests`): unit tests, in-kernel tests and QEMU scenarios behind one command.

## Hardware

BitsOS is developed and tested on one machine: a mini PC with an AMD Ryzen 7 8845HS, Radeon 780M graphics,
an Intel I226 network card and a Realtek RTL8852BE Wi-Fi card. It boots from a USB stick. Other hardware will
mostly not work yet. QEMU (q35) is used for the automated tests.

## Building

You need Linux, `rustup` (the toolchain in `rust-toolchain.toml` installs itself), QEMU and `mtools`.

```sh
cargo xtask test            # the whole test suite (unit, in-kernel, QEMU scenarios)
cargo xtask image           # a bootable image under target/xtask/
cargo xtask qemu            # boot it in QEMU
```

`cargo xtask stick` writes a USB stick, and only a stick whose ID you recorded in `stick.id`.
BitsOS never writes to internal disks unless `nvme_write=on` is set in the stick's `config.txt`.

## Status

This is a hobby project and an experiment in building a whole OS with AI agents. It is not secure, not stable and
not meant for anything important. Expect things to break.

## Licence

MIT OR Apache-2.0, at your option. Third-party parts and their notices are listed in `NOTICE.md`.
