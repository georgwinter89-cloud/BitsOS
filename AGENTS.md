# AGENTS.md

A guide for AI coding agents (Claude Code, Codex, Cursor and others) that help a person try or develop BitsOS.

BitsOS is a hobby operating system: its own Rust kernel with the Linux system-call interface, a UEFI loader
(`boot/bitsboot`), a Wayland desktop and ports of Linux programs. It is developed on one machine (Ryzen 7 8845HS,
Radeon 780M) and tested in QEMU through one tool, `cargo xtask` (`host/xtask`).

Read `README.md`, section "Trying BitsOS", before you suggest anything to the person. Everything there is derived from
the code; when the code and a document disagree, the code wins, and you say so.

## Hard rules

These protect the person's machine and data. Do not bend them, not even when asked to "just try".

1. **Never write to an internal disk or partition.** Do not run `dd`, `mkfs`, `parted`, `sgdisk`, `wipefs`,
   `fdisk` or anything similar against any device yourself. Preparing a USB stick (README section 4, step 1) erases
   it: show the person the commands, let them check the device with `lsblk`, and let them run it. Never point QEMU at a
   device: start QEMU only through `cargo xtask qemu`, which refuses host disks, passthrough and foreign ports, or
   through the start script `bitsos-install --qemu` wrote, which boots its own image file only.
2. **Never set, change or remove a guarded CONFIG key.** The guarded keys are `nvme_write`, `nvme_data`, `nvme_boot`,
   `stick_write`, `usb_msd_write`, `mcp` and `mcp_confirmer`. Only the person sets them, by hand, in
   `\BITSOS2\CONFIG.TXT`. Do not write a `CONFIG.TXT` that contains them, do not offer to "add the line for them",
   and do not look for a way around BitsOS's refusal. You may explain what a key does and what the rules are.
   The one tool that writes `mcp=service` is `bitsos-install`, and only for `--remote yes`: pass that only when the
   person said yes to remote control in this conversation.
3. **`bitsos-install` erases the stick the person names.** Run `bitsos-install --list`, show the person the whole
   output, and pass `--erase <id>` (or `--update <id>`) only with the id the person gave you for that run, character
   for character. The id ends in a fingerprint of the stick's vendor, model and serial (`sdb-31.0G-3fa2`), so an id
   never fits another stick; do not build one, guess one or reuse one from an earlier run. Never choose a disk
   yourself, never retry with another id after a refusal, never type the installer's `ERASE <id>` line for the
   person, and never point `--image` at a device. A stick without a serial number is refused in this mode: the
   person runs the installer without `--erase` and chooses it there.
4. **Never write a stick whose ID the person did not confirm.** Do not create or change `stick.id` or a stick's
   `\BITSOS2\STICK.ID` unless the person asked for exactly that. Run `cargo xtask stick` without `--write` first,
   show the person its output (the first line names the device, bus, model, label and size), and add `--write` only
   after they said yes to that stick. Never feed the `J` into the tool's question yourself (no pipes, no `yes`), and
   never use `--force` unless the person asked for it for that one run.
5. **Ask before anything touches real hardware**: writing a stick, installing a generation, rebooting, starting a
   program on the machine.
6. **Never print, log, paste or commit secrets.** `token`, `wlan_psk` and `claude_api_key` in `CONFIG.TXT`, the
   network name `wlan_ssid`, the `nvme_*` values (drive serials, partition GUIDs), `stick.id`, and the MCP key store
   (`bitsos-mcp/`, `~/.config/bitsos-mcp/`), and the installer's `token.txt`. Pass tokens with `--token-file`, never
   in a URL or an argument of a real machine's command; `bitsos-install` run with `--erase`/`--update` prints no
   token, only the folder it is kept in. When you show a `CONFIG.TXT`, replace those values with `***`. Real IP addresses, MAC addresses
   and serial numbers do not go into commits or issues; use 192.168.100.x in examples.
7. **Stop only processes you started.** `cargo xtask qemu` writes the PID of its QEMU to
   `target/xtask/qemu-<name>.pid` and names the process `v2-<lane>-<name>`. Stop that PID and nothing else. Never
   `pkill -f`, `killall qemu-system-x86_64` or similar: other runs (another agent, a test suite) may be using QEMU.
8. **Sound over HDMI can be loud.** Before anything plays sound on the real machine (`speaker-test`, `aplay`, a game,
   a video), ask the person and have the volume turned down.
9. **The remote control's yes is the person's.** Pairing and full access are answered at the machine's real keyboard.
   Never try to answer those dialogs with injected keys or clicks (BitsOS marks them, refuses them and logs it).

## Commands and how long they take

All from the repository root. Long runs belong in the background; run one full gate at a time.

| Command | What it does | Time (rough) |
|---|---|---|
| `V2_PKG_OPTIONAL=1 cargo xtask image --name N [--config-extra "k=v;k2=v2"] [--release]` | kernel, bitsboot, user programs, initramfs, FAT32 image `target/xtask/N.img` and its folder `target/xtask/esp-N/` | first build 10 to 30 min, then 1 to 3 min |
| `cargo xtask qemu --name N [--display gtk] [--timeout S] [--net user] [-- extra QEMU args]` | boots image N as a USB stick; serial log to the terminal and `target/xtask/N.log` | under a minute to the console with KVM; stops after `--timeout` (default 60 s) |
| `cargo xtask ktest [FILTER] [--split f1,f2]` | builds a ktest kernel and boots one QEMU per filter | a few minutes per boot |
| `cargo xtask scenario <lane>/<script>` | one script of `host/tests/` (default limit 1200 s) | seconds to 20 min |
| `cargo test -p <crate>` | one crate's host tests | seconds to minutes |
| `cargo xtask test` | the gate: host tests, user crates, warnings, in-kernel tests, scenarios | 10 to 40 min on a 20-thread machine |
| `cargo xtask fullrun [--quick]` | the gate plus every other scenario | an hour or more |
| `cargo xtask stick --target <mount point>` | shows the stick and the plan; `--write` writes after a typed `J` | seconds plus the build |
| `cargo xtask --help`, `cargo xtask stick --help` | every option | |

`V2_PKG_OPTIONAL=1` is needed on any machine that lacks the developer's program stages (README section 2). The gate
needs those stages and the musl sysroot; on a machine without them, those steps are red. That is expected; say so
instead of "fixing" the tests.

## Reading results

- **Gate** (`cargo xtask test`, `fullrun`): the last line is `GREEN: host tests green, ..., ktest green, ...` or
  `RED: ...` with the red steps marked `RED`. Scroll up to the step's own output.
- **ktest**: one line per boot, `ktest <name> filter <f>: passed=<p> failed=<f> skipped=<s>`, then `RED: <problem>`
  lines and `log: target/xtask/<name>.log`, and at the end `ktest: green` or `ktest: red`. In the serial log:
  `KTEST ok <name> (<ms> ms)`, `KTEST FAIL <name>: <reason>`, `KTEST skip <name>: class <class> belongs to another
  run`, `KTEST done passed=... failed=... skipped=...`, and `KTEST PANIC <message>` from the panic handler. A `skip`
  line only means the test belongs to another run; the gate's ledger checks that every test ran somewhere.
- **QEMU exit**: xtask prints `guest reported PASS` (exit code 33), `guest reported test FAILURE` (35),
  `guest reported KERNEL PANIC` (37), or `timeout: QEMU was stopped by xtask`. A plain `cargo xtask qemu` session
  that you end yourself or that reaches its timeout is not a failure of BitsOS.
- **Scenarios**: `scenario <lane>/<script>: ok` or `FAIL`; the script's output is in
  `target/xtask/runs/<lane>-<script>/output.log`. Conventions: `host/tests/README.md`.
- **Firmware stalls**: `OVMF stalled before bitsboot` or `OVMF found nothing to boot` lines are QEMU's firmware;
  xtask retries the boot by itself.

## Rules for contributions

- **Zero compiler warnings.** The gate's `warnings` step counts every `warning` line of every build it makes; one is
  red. Do not silence a warning with `allow` to get past it; fix the cause.
- **No skipped tests.** A test that cannot do its work fails (`ktest::fail(...)`, a non-zero exit), it never skips
  or returns early as a pass. Do not mark a test ignored to make a run green.
- **Big programs are tested on the real machine only.** A scenario that would boot a program tree carries
  `# minipc: <where its checks run>` in its first five lines and no QEMU run starts it.
- **Scenarios never touch a stick, a block device or a real machine**, use only their lane's ports and unique image
  names (`host/tests/README.md`).
- The gate also checks `#![forbid(unsafe_code)]` where it is required, `Drop` bounds, and that no absolute home
  path is in the tree.
- Code is in English, files stay short, comments explain why, not what.
- Commit only when the person asks. Never commit `stick.id`, a `CONFIG.TXT`, tokens, keys, firmware files or program
  trees (none of these belong in git).

## Using the remote control safely

The remote control is an MCP server on the BitsOS machine; the client tools are in `host/mcp/` (Node.js). README
section 9 has every command.

- Try it in QEMU first: an image with a throwaway `token=`, `cargo xtask qemu --net user --hostfwd 20001:8765`, then
  `node host/mcp/mcp-call.mjs 127.0.0.1:20001 version --token-file token.txt`. Host ports must lie in 20000 to 20499.
- On a real machine the default (`mcp=kernel`) is plain HTTP where the token opens everything. Tell the person, and
  recommend `mcp=service`, which they set by hand.
- Pairing (`host/mcp/pair.mjs <host>:8765 --expect <hex> --token-file <file>`) is the person's step: they read the
  `bitsmcp: TLS server key sha256 <hex>` line on the machine's screen, give you the hex, and answer the dialog at
  the machine. Never use `--trust-first-use` outside QEMU.
- After pairing, use `--tls auto` on every call. A paired client gets a narrow read-only set; installs and reboots
  ask at the machine each time.
- Ask for full access (`host/mcp/access.mjs <host>:8765 request --hours 1 --reason "<why>"`) only when you need it,
  for the shortest time, with an honest reason. Never ask for the NVMe tick unless the person wants exactly that.
- Run `install.mjs` with `--dry-run` first and show the person the plan.

## Troubleshooting

- **No boot, back in the firmware**: Secure Boot is on (bitsboot is not signed); the firmware's legacy or non-UEFI
  entry was chosen; the processor lacks x86-64-v3 (bitsboot's screen names the missing features); the stick has no
  generation or a kernel bitsboot refuses (its screen says why). bitsboot returns to the firmware after a key or a
  few seconds.
- **Black screen**: the desktop waits up to 30 s for the 780M to come up. Press a key during the boot screen for the
  text console. Try `gpu=off`, `bitscomp=rescue` or `desktop=off` in `CONFIG.TXT`. On other graphics the picture is
  the firmware's framebuffer; if the firmware set none, nothing is shown.
- **No network**: only Intel I226/82576 and the RTL8852BE have drivers (virtio-net in QEMU, with `--net user`). An
  Intel processor starts in safe mode without network drivers (`profile=normal` changes that). Wi-Fi needs `wlan=on`,
  `wlan_ssid=`, `wlan_psk=` and the firmware in `\BITSOS2\firmware\rtw89\` (copied from the image folder); without
  `wlan_ssid=` the log says `wlan config.no_network`. The monitor command `net` lists cards and addresses.
- **Stick read-only**: look for the kernel log line `fat stick.decided ... rw=false ... reason=...`. The reasons:
  `bitsboot handed over no \BITSOS2\STICK.ID` (safe mode, or an old loader); `the volume has no readable
  \BITSOS2\STICK.ID`; `the volume's STICK.ID differs from the boot stick's`; `the volume label is not BITSOS`;
  `more than one disk looks like the boot volume` (a cloned stick plugged in); `a disk could not be told apart from
  the boot volume`; `the boot device is write-protected or refuses writes`; `its partition table is protected (R8a)`
  (a GPT "Microsoft basic data" or EFI system partition, or an MBR Windows or `EFh` type on the stick: the person
  re-partitions it as MBR `0Ch`); `the volume does not keep two mirrored FATs`. A USB disk that might be the stick
  appearing later turns writing off for the rest of the boot.
- **`/boot` read-only when booted from the NVMe**: the same line, `nvme=true`, with one of: `CONFIG.TXT has no
  nvme_write=on`; `CONFIG.TXT has no nvme_data= line (the boot partition is written only beside its window)`;
  `CONFIG.TXT has no nvme_boot= line naming the boot partition`; `nvme_boot= names another disk than nvme_data=`;
  `nvme_data='s window is not armed on the boot disk`; `the boot disk's partition table no longer passes the window
  check`; `the boot volume is not the EFI System partition nvme_boot= names`. `cat /proc/bitsos/data-window` shows
  the data window's state. Every fix is the person's edit of guarded keys; explain, do not edit.
- **`cargo xtask stick` refused**: it prints `refused: <why>. Nothing was written.` Read the reason to the person;
  the usual ones are a missing or different `STICK.ID`, a wrong label, a `Windows` folder on the stick, a loader of
  another system in `\EFI\BOOT\BOOTX64.EFI`, or a stick that is not on USB.
- **The image build stops** at a missing package source: add `V2_PKG_OPTIONAL=1`, or install the package named in
  the message.

## Driving the installer for a person

`bitsos-install` with `--erase <id>` or `--update <id>` asks nothing; every answer is a flag. What stays the person's:
the choice of the stick, the passphrase, the yes to remote control, and the root password.

1. **Download** `bitsos-install-<V>-linux-x86_64` (or `-windows-x86_64.exe`) from the GitHub release `v<V>` of
   `https://github.com/georgwinter89-cloud/BitsOS`, and `chmod +x` it. It installs release `<V>` only.
2. **List:** `./bitsos-install-<V>-linux-x86_64 --list` (no root needed). Show the person the whole output, including
   the `not offered` lines, and ask which stick to erase. Use the id they give you. If no stick is offered, read
   them the reason on its line (mounted: they unmount it in the file manager; a fixed disk or no serial: they use
   another stick or run the installer interactively).
3. **Ask the questions yourself:** cable only, or Wi-Fi (the person writes the passphrase into a file of their own
   on its first line, for example with an editor, and gives you only the path; you never see or type it); remote
   control yes or no (`--remote yes` only after their yes in this conversation); Steam yes or no (Valve's client,
   about 720 MB from Valve); Brave yes or no.
4. **Run it**, or let the person run it, since `sudo` asks for their password:

   ```sh
   sudo ./bitsos-install-<V>-linux-x86_64 --erase <id> --cable-only --remote no --steam no --brave yes
   sudo ./bitsos-install-<V>-linux-x86_64 --erase <id> --wifi <name> --wifi-psk-file <file> --remote yes --steam yes --brave yes
   ```

   For a stick that already holds BitsOS, `--update <id> --steam yes|no --brave yes|no` adds the release as a new
   generation and keeps its `CONFIG.TXT` and data. On Windows the same flags run in an administrator terminal.
5. **Read the result.** `Done: <n> files (<m> MB) written and read back; generations: 1 OK.` is success; the lines
   after it say where the stick's ID and token are kept and what to do next. `refused: ... Nothing was written.`
   means the stick is as it was: read the reason to the person and do not try another id. `NOT INSTALLED: <tree>:
   <why>` stops the run before writing; only with the person's agreement add `--allow-missing` to go on without that
   tree. `The stick was erased and is incomplete` means run the same command again.
6. **Then** the boot steps the installer printed: Secure Boot off, the firmware's boot menu, the stick's UEFI entry,
   the German keyboard layout, the volume down.

## BitsOS in a window with the installer (`--qemu`)

The first and easiest way to show BitsOS to a person: nothing to build, one command, then a shortcut. It writes an
image file and a start script in the person's data folder and never a disk; it runs without `sudo` (it refuses root).

1. **Ask** whether they want remote control in the window (yes or no) and whether BitsOS should start at the end.
2. **Look first:** `./bitsos-install-<V>-linux-x86_64 --qemu --dry-run --remote no` prints what is found (QEMU, its
   firmware, KVM or WHPX), what is missing and the install command, and the QEMU command. Nothing is written.
3. **QEMU missing:** the run names the command (`sudo pacman ...`, `sudo apt-get ...`, or the pinned QEMU for
   Windows download). Show it to the person. Add `--install-qemu` only after the person said yes to installing it in
   this conversation, never on your own; `sudo` asks for their password (let them run it), on Windows they answer
   Windows' prompt. The release is checked before anything is installed, so a run that stops on the release has
   installed nothing.
4. **Run:** `./bitsos-install-<V>-linux-x86_64 --qemu --remote yes|no --start yes|no` (Windows: the `.exe`, an ordinary
   terminal). `Done: ... generations: 1 OK.` is success; a second run adds the release as a new generation
   (`1 OK, 2 NEW`) and keeps `CONFIG.TXT` and `data.img`. Programs are untested in QEMU: add `--trees`,
   `--steam yes` or `--brave yes` only when the person asks for them.
5. **Read along:** the serial log is `bitsos-serial.log` beside the image (`~/.local/share/bitsos/`,
   `%LOCALAPPDATA%\BitsOS\`); `console desktop.up` there means the desktop is up. The QEMU PID is in `qemu.pid`
   there; stop that PID and nothing else. With remote control the forward is `127.0.0.1:18765` (the run prints the
   port); pairing is the person's step as in "Using the remote control safely", `--trust-first-use` is allowed here.
   On Windows QEMU's own messages are in `qemu-output.log` there. A slow-start note (no KVM, no WHPX) names the fix;
   the start script checks again at each start, so after the fix the person just starts BitsOS again.
6. **Remove:** `--qemu --uninstall --dry-run` only lists the files it made; show that list. `--qemu --uninstall`
   lists and asks; pass `--yes` only after the person agreed to that list.

## Helping a person try BitsOS, step by step

1. **Ask about their machines.** Which computer builds (Linux distribution, CPU, free disk space, `/dev/kvm`), and
   which computer they want to boot BitsOS on (CPU maker and model, graphics, network card, USB keyboard). Compare
   with README section 3 and tell them plainly what will likely not work.
2. **QEMU first.** Without building: the installer's QEMU mode (above). To build it yourself: install the packages
   (README section 1), the toolchains, build
   `V2_PKG_OPTIONAL=1 cargo xtask image --name try --config-extra "desktop=on"` in the background, then start
   `cargo xtask qemu --name try --display gtk --timeout 3600 --mem 4G --smp 4 --net user -- -device usb-kbd,bus=xhci.0 -device usb-tablet,bus=xhci.0`.
   Tell them about the German keyboard layout. Read `target/xtask/try.log` with them if something looks wrong.
3. **Decide about the big programs.** If they want Brave, LibreOffice or Steam, go through README section 2 with
   them: which packages they need, which downloads the scripts make (each one checked against a pinned SHA-256), and
   that this path is untested outside the developer's machine.
   Windows games from Steam run through Proton, 64-bit and 32-bit ones (WoW64). 32-bit Linux programs do not run.
4. **The stick.** From a release, use the installer (the section above). For an image they built themselves: let
   them choose a stick they can erase, show the partition and format commands (README section 4) and let them run
   them after checking the device. Let them make `stick.id`. Build the image, write their
   `CONFIG.TXT` with them (without guarded keys, and with secrets only in their own file), run the dry run, show
   them its first line, and only on their yes run it with `--write`; they type the `J`. Copy firmware, `bin` and the
   trees with them, then unmount.
5. **Configuration.** Wi-Fi keys, `panic=reboot`, `mirror=` to their computer if they want logs live. If they want
   remote control, explain `mcp=kernel` versus `mcp=service` and that the guarded keys are theirs to write.
6. **Boot.** Secure Boot off, the firmware's boot menu, the USB stick's UEFI entry. Ask what they see: bitsboot's
   error screen, the boot screen and its rows, the desktop, or the text console.
7. **Collect logs when it breaks.** A photo of the screen; the `mirror=` lines; `bitsjournal query ... >
   /boot/BITSOS2/journal.txt` and the stick read on another computer; or the remote control's `log` and `events`.
   Remove secrets, IP and MAC addresses and serial numbers before anything leaves their machine.
8. **Afterwards.** If a generation worked and no remote control confirmed it, offer
   `cargo xtask stick --target <mount point> --confirm <n>` (dry run, then `--write`). Help them write an issue with
   hardware, commit, steps, what they saw and the cleaned logs.
