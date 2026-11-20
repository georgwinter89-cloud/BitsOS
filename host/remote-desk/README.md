# Remote desk for the Gaming-PC (read this first)

You are the Claude session on the **Mini-PC under Windows**. BitsOS V2 runs on the **Gaming-PC** (ASRock Z890
Taichi OCF, Intel Core Ultra 7 265K, RX 9070 XT, RTL8126 cable card) in its safe profile `gaming-pc` with the
tiers `gp_net=on` (cable network), maybe `gp_stick=on` (BitsOS may write its own boot stick, nothing else) and,
after Georg's SSD guide (plan/guides/gaming-pc-ssd.md), `gp_nvme=` (the Lexar SSD alone gets a driver, under the
Mini-PC's window rules).
You reach it only over MCP on the cable network. Georg does not program: he plugs, powers, photographs and
decides. This folder is everything you need; nothing here needs bash, WSL or a build tool.

**Round 2** (GP-R3) runs in phases, one at a time, each stopping at its first red: section 5b. Georg's own guide for
it is `plan/guides/gaming-pc-runde2.md` (in the repository); it says what he does at the keyboard in each phase.

## 1. What you need on this machine

- **Node.js 20 or newer** (22 LTS recommended), from nodejs.org, the Windows installer. `node --version` must
  say v20 or higher. The tools use only Node's own modules (http, tls, crypto, fs); no `npm install`.
  Tested with Node 26.13.1 on Linux from a copy in a path with spaces and with CR LF line ends, and with Node
  24.18.0 on Windows 11 in round 1 (15.11.).
- This folder **together with `host/mcp/`**, in the same layout as the repository (`<somewhere>\host\mcp\` and
  `<somewhere>\host\remote-desk\`). The tools find each other by relative paths. A path with spaces is fine.
- **The token, added by hand, never in git**: one line, the value of `token=` in the Gaming-PC stick's
  `\BITSOS2\CONFIG.TXT`, in
  `%USERPROFILE%\.config\bitsos-gaming-pc\token.txt` (for example `C:\Users\<you>\.config\bitsos-gaming-pc\token.txt`).
  `selftest.mjs` and `collect.mjs` always look there; `mcp-call.mjs` and the others in `host\mcp\` look there when
  the call carries **`--gaming-pc`** (one rule, `host\mcp\token.mjs`: below `%USERPROFILE%` first, then below
  `HOME`, e.g. `~/.config/bitsos-gaming-pc/token.txt` under Linux). Without `--gaming-pc` they read no default file,
  so a call to the Mini-PC never carries the Gaming-PC's token. `BITSOS_TOKEN_FILE=<path>` moves the place,
  `--token-file <path>` overrides it per call, `BITSOS_TOKEN` (the value itself) also works. The tools never print
  the token.
- **No TLS trust is needed** for this round: the stick has no `mcp=` line, so the kernel's own server answers
  plain HTTP on port 8765 with the token. Only if a later stick sets `mcp=both` or `mcp=service`: pair once with
  `node host\mcp\pair.mjs <host> --token-file <token file>`; the key store is then made next to the token file
  (`%USERPROFILE%\.config\bitsos-gaming-pc\bitsos-mcp\`: `client.key`, `client.crt`, `pins\<host>.der`) and every
  tool takes `--tls auto`. The store must stay outside the repository (the tools refuse one inside).

## 2. The Gaming-PC's address

The report page on the Gaming-PC's screen says it in the row **"MCP erreichbar unter a.b.c.d:8765"** (Georg's
photo), and `hw_info {"section":"report"}` says the same once you are in. If the row says
"MCP nicht erreichbar ..." or "MCP läuft, aber die Karte hat noch keine Adresse" (the page renews that row once when the address comes later), there is no network: work from
Georg's photos and the report file on the stick (section 6). The address is the card's own MAC's lease, so
probably the one the router gives CachyOS on that PC (round 1: 192.168.100.96, by DHCP from 192.168.100.1).

## 3. First steps

```
node host\remote-desk\selftest.mjs
node host\remote-desk\selftest.mjs --host <address>
node host\remote-desk\collect.mjs <address>
```
`selftest.mjs` checks Node, the tools, the arguments files and the token file (exit 0 = ready); with `--host` it
makes one read-only call. `collect.mjs` runs the ordered check list of section 5 (read-only steps only), saves
every answer to `host\remote-desk\out\<time>\<step>.txt` and prints `ok` or `RED` per step with what to look at.
`--write` adds the two write checks, `--ktest` the ktest round, `--only 03,15` single steps. The report (step 03)
comes up to a minute after MCP first answers: if it says "not ready yet", wait a minute and run `--only 03` again.
The stick's file is written after the page, by a thread of its own: while the row says "Bericht auf dem Stick: wird
geschrieben, sobald der Stick bereit ist", run `--only 03` again a little later.

**The new generation's confirmation needs a heartbeat.** A generation on its one try (reload_info: "generation N
waits for its confirmation") confirms itself only once MCP has answered for 120 s without a pause over 30 s; a
longer pause starts the 120 s again (`boot.confirm_gap gap_s=30` in the log), and without the confirmation the next
start falls back to the generation before. In round 1 the pauses between calls kept 107 unconfirmed until
reload_info ran every 10 s. `collect.mjs` therefore sends `reload_info` every 10 s on its own connection while it
runs, also during a ktest call that takes minutes (`--heartbeat-s N`), and after the last step it keeps that up
until reload_info says "confirmed (OK)", at most 200 s (`--confirm-wait-s N`, 0 = not; it prints `ok   confirm` or
`RED  confirm`). Working by hand with single `mcp-call.mjs` calls instead: once a generation waits, call
`reload_info` every 10 s for about two minutes (or run `collect.mjs <address> --only 01`, which does exactly that).

**What a step expects follows the machine.** Before the first step `collect.mjs` reads the kernel's own `config`
line (the same read as step 05) and the loader's lines, and prints one `fact <name>=true|false|unknown` line per fact.
The cable card always runs on its interrupt (no CONFIG line keeps it polled): the network self-check's stage 8 must
be ok and step 30 must show the card's vector. A `has` item shown as `a … b … c` means these parts on one
line in this order, with other fields between them.

The same read gives `fact gp_nvme_named=true|false|unknown` (GP-DISK): `true` when the kernel kept a `gp_nvme=` line
with a value (Georg named the Lexar SSD in CONFIG.TXT and the stick's bitsboot knows the line, so the loader added
`gp_nvme_loader=on`). Then steps 03, 04, 05, 19, 19b, 20 and 21 expect the Lexar at 81:00.0 with its driver
(section 5, "with gp_nvme="); without the line they expect what round 1 saw (no NVMe controller at all). If Georg
wrote the line but step 04 has no `nvme=on` and step 06 says `profile.guard changed=gp_nvme`, the stick still has the
old bitsboot: the kernel took the line away, nothing got a driver, the desk writes a new stick.

The same read gives `fact booted_from_ssd=true|false|unknown` from the loader's lines (GP-DISK, guide step 8): `true`
when bitsboot came from the Lexar's BITSOS partition (`stick: the boot volume is an NVMe partition`). Then steps 03,
04 and 25 also want the start from the SSD (the report's line "Gestartet von der SSD (Partition BITSOS) …", the
NVMe device path, `stick.decided … nvme=true rw=true`). Both CONFIG states are judged by the same list: the stick with
`gp_net=on`/`gp_stick=on` only (round 1) and the medium with the three Lexar lines (`gp_nvme=`, `nvme_write=on`,
`nvme_boot=`, and after bitsdisk `nvme_data=`); a step that needs the Lexar says so through `gp_nvme_named`.

The same read gives `fact cpu_perf` (GP-CPU2: Georg chose `performance` for the setting Prozessor-Abstimmung,
plan/guides/gaming-pc.md): then steps A17 and A18 want the energy preference 0x10 (`epp=0x10 policy=performance`,
`= 0x0000000010`) instead of balanced 0x40.

## 4. The tools

| Tool | What | Windows notes |
|---|---|---|
| `host\mcp\mcp-call.mjs <host> <tool> [json or @file.json] (--gaming-pc | --token-file F) [--png F] [--save F]` | one MCP call; exit 0 done, 1 the tool refused, 2 BitsOS not reachable | JSON quoting differs per shell: use `@host\remote-desk\args\<step>.json` (one file per check step), or PowerShell 7: `'{"section":"cpu"}'`, or cmd: `"{\"section\":\"cpu\"}"` |
| `host\remote-desk\collect.mjs <host>` | the check list | |
| `host\remote-desk\selftest.mjs` | this folder's self-test | |
| `host\mcp\install.mjs <host> --bundle B --upload --reboot` | a new generation onto the boot medium (the stick with `gp_stick=on`, or the Lexar's BITSOS partition when BitsOS started from it), reboot, wait for its confirmation; phase A runs it for you | use `--upload` (write_file pieces, no listening port, no `--bind`: GP-R3): the HTTP mode needs an inbound port 8770-8799 that Windows' firewall asks about [U: not reached in round 1] |
| `host\mcp\push.mjs <host> --tree <dir> --via write` | a package tree onto the stick | `--via write` for the same reason; `--add` adds and replaces files and removes nothing (the firmware set, section 6) |
| `host\mcp\reload.mjs <host> --kernel K --initrd I --bind <this PC's LAN address>` | a Reload without writing the stick | never builds on Windows (no cargo here): always `--kernel` and `--initrd`; it listens on port 25410, Windows' firewall asks once [U: not reached in round 1] |
| `host\mcp\pair.mjs`, `host\mcp\access.mjs` | TLS pairing, full-access status | only with `mcp=service`/`both`, not this round |

One call of each kind, as plain one-liners (PowerShell or cmd; `--gaming-pc` = the token from the file of section 1,
or `--token-file T` instead):
```
node host\mcp\mcp-call.mjs 192.168.100.20 hw_info @host\remote-desk\args\03-report.json --gaming-pc
node host\mcp\mcp-call.mjs 192.168.100.20 screen @host\remote-desk\args\01-version.json --png page.png --gaming-pc
node host\mcp\push.mjs 192.168.100.20 --tree bundle\BITSOS2\bin --via write --gaming-pc
node host\mcp\install.mjs 192.168.100.20 --bundle bundle --upload --reboot --gaming-pc
node host\mcp\reload.mjs 192.168.100.20 --kernel bundle\KERNEL --initrd bundle\INITRD --bind <this PC> --gaming-pc
```
(`192.168.100.20` stands for the address of section 2; `screen` with `{}` as arguments gives the picture.)
Kernels and initramfs come from the desk's build on CachyOS before the swap (the bundle is copied along with
this folder); nothing is built here.

## 5. The ordered check list

The same list as `checks.json` (collect.mjs runs it). Kinds: `read` (always), `write` (only `--write`), `ktest`
(only `--ktest`). "has"/"not" are what collect.mjs judges; the rest you compare by eye. [U] = nobody has seen it
on this machine yet. Expected texts of the page: the round-1 page `plan/hw/gaming-pc-round1-2026-11-15/GP000001.TXT` (in the repository).

| Step | Kind | Call | Expected |
|---|---|---|---|
| 01-version | read | `version` `{}` | has `BitsOS` — the kernel answers over the cable |
| 02-kernel | read | `hw_info` `{"section": "kernel"}` | has `profile=gaming-pc reason=dmi tiers=net` — seen 15.11.: profile=gaming-pc reason=dmi tiers=net,stick (gp_stick=on); reason=cpuid would mean the board's names were not read |
| 03-report | read | `hw_info` `{"section": "report"}` | has `BitsOS-Bericht für den Gaming-PC`; `Startprogramm, Profil und Freigaben`; `Prozessor, Uhr und Tabellen`; `Geräteschutz`; `Geräteliste (beide Anschluss-Stränge)`; `Platten`; `Bild`; `USB`; `Netzwerk`; `Grafikkarte`; `Wächter`; `Ton`; `MCP erreichbar unter`; not `not ready yet`; `EINGESCHALTET`; with gp_nvme=: also has `Treiber an (in CONFIG.TXT mit gp_nvme freigegeben)`; `(Georgs Windows- oder CachyOS-Platte, nie freigebbar)`; with booted from the SSD: also has `Gestartet von der SSD (Partition BITSOS)` — the page Georg photographed, as text; compare row by row with the round-1 page plan/hw/gaming-pc-round1-2026-11-15/GP000001.TXT. EINGESCHALTET in Geräteschutz = stop (a minimal VT-d identity map would have to come first); with gp_nvme=: in Platten the Lexar's row 'Treiber an (in CONFIG.TXT mit gp_nvme freigegeben)', both Samsungs 'kein Treiber, nicht gelesen (... nie freigebbar)', booted from the SSD the start line 'Gestartet von der SSD (Partition BITSOS) ...' |
| 04-loader | read | `log` `{"subsystem": "boot", "name": "loader.line", "max": 2000}` | has `profile.chosen name=gaming-pc reason=dmi`; `profile.tiers net=on`; not `generation.unguarded`; `profile.normal_refused`; `profile.intel_fallback`; with gp_nvme=: also has `profile.tiers … nvme=on`; `safe mode: only the NVMe controller gp_nvme=`; without gp_nvme=: also not `profile.tiers … nvme=on`; with booted from the SSD: also has `stick: the boot volume is an NVMe partition`; `stick: device path … NVMe(` — stick.id_withheld without gp_stick, stick.id_handed with it; with gp_nvme= (the Lexar named): profile.tiers ... nvme=on and the safe-mode line naming the controller (booted from the SSD also 'stick: the boot volume is an NVMe partition') |
| 05-config | read | `log` `{"subsystem": "kernel", "name": "config"}` | has `gp_net=on`; `nvme_bind=off`; `nvme_write=off`; `stick_write=off`; `usb_msd_write=off`; `screen=firmware`; not `nvme_data=`; `nvme_boot=`; with gp_nvme= instead: has `gp_net=on`; `gp_nvme=81:00.0/1d97:5236`; `gp_nvme_loader=on`; `nvme_bind=off`; `nvme_write=on`; `stick_write=off`; `usb_msd_write=off`; `screen=firmware` — the forced lines of the profile; with gp_nvme= the stick's own nvme_write=on, nvme_boot= and (after bitsdisk) nvme_data= stand for the named controller, gp_nvme=81:00.0/1d97:5236 and the loader's gp_nvme_loader=on beside them (GP-DISK decision 6, 8) |
| 06-profile-guard | read | `log` `{"subsystem": "kernel", "name": "profile."}` | not `profile.guard`; `profile.missing` — nothing: the loader forced every line itself (a line with reason=dmi means the bitsboot on the stick is older than GP-GUARD: write a new stick) |
| 07-smp | read | `log` `{"subsystem": "smp"}` | has `done cpus=20 planned=19 up=19 failed=0`; `p_cores=8 e_cores=12` — seen 15.11.: done cpus=20 planned=19 up=19 failed=0 total_ms=607 tsc_worst_ticks=343 |
| 08-cpu-log | read | `log` `{"subsystem": "cpu", "max": 200}` | has `hwp.on cpu=0`; `spec_ctrl value=0x401`; not `hwp.failed`; `msr.faults` — seen 15.11.: 20 hwp.on lines, no msr.faults; a msr.faults line names the registers the CPU refused |
| 09-time | read | `log` `{"subsystem": "time"}` | has `hpet.untrusted reason=pc10`; `clock.cross_check`; `cpuid15_hz=3878400000`; not `clock.hint_disagrees`; `clock.cpuid15_disagrees` — seen 15.11.: the PM timer measured 3878402521 Hz against CPUID 0x15's 3878400000, ppm=0 |
| 10-acpi | read | `log` `{"subsystem": "acpi", "max": 400}` | has `madt.empty_slots count=28`; `dmar width=42`; `dma.state verdict=free blocked=false`; `tco bus=0x80`; not `dma.blocked`; `dmar.interrupts_remapped`; `table.problem` — seen 15.11.: dmar width=42 flags=0x5 units=2 satc=1, both units gsts=0x0 (translation off), tco bus=0x80 enabled=true; dma.blocked = STOP, photo, power off |
| 11-hw-cpu | read | `hw_info` `{"section": "cpu"}` | has `cpu hwp state=on`; `spec_ctrl=0x401`; `cpu msr faults count=0 registers=none` |
| 12-hw-firmware | read | `hw_info` `{"section": "firmware"}` | has `DMAR`; `TCO` — ends with the DMAR, DMA and TCO lines |
| 13-msr-10a | read | `msr_read` `{"msr": "0x10A"}` | bit 1 (IBRS_ALL) set; seen 15.11.: 0x0df9fd6b |
| 14-msr-774 | read | `msr_read` `{"msr": "0x774"}` | the HWP request of the CPU that answers; seen 15.11. with the round-1 kernel: 0x80005701 (on cpu 6); from GP-CPU2 on bits 31:24 are 0x40 (0x40005701), 0x10 with cpu_policy=performance |
| 15-hw-pci | read | `hw_info` `{"section": "pci"}` | has `config access ecam`; `80:14.0`; `83:00.0`; `86:00.0`; `for good on this PC (gp_` — seen 15.11.: first line 'pci: 41 functions, 12 bridges' with today's single card; nine lines 'untouched for good on this PC (gp_...)': 00:0d.2, 00:0e.0, 00:1f.5, 04:00.1, 80:16.0, 80:1c.3, 80:1f.3, 80:1f.5, 85:00.0 |
| 16-pci-peer | read | `log` `{"subsystem": "pci", "name": "peer", "level": "info"}` | has `peer.root bus=0x80 first_device=0x14 vendor=0x8086`; `peer.plan mode=gaming-pc segment=0 window=00-df`; not `peer.refused`; `peer.unexpected` |
| 17-pci-warn | read | `log` `{"subsystem": "pci", "level": "warn"}` | not `bar.beyond_phys`; `write.refused`; `root.skipped`; `W pci cap.`; `W pci bar.`; `W pci bridge.`; `W pci bus.` — a probe.failed line is a driver that gave up (the page's PCI row says the same). GP-R2: a vendor capability of length 0 (00:0d.0, 80:14.0) and a function in D3 (80:15.0, 80:15.2) are info lines now, as Linux takes them; any cap./bar./bridge./bus. warning left is a broken table |
| 18-pci-vmd | read | `log` `{"subsystem": "pci", "name": "vmd", "level": "info"}` | has `vmd function=00:0e.0 device=0xad0b nvme_seen=3` |
| 19-pci-untouched | read | `log` `{"subsystem": "pci", "name": "untouched", "max": 400}` | has `function=01:00.0 key=nvme_bind`; `function=81:00.0 key=nvme_bind`; `function=86:00.0 key=nvme_bind`; `function=85:00.0 key=gp_wlan`; not `key=gp_net`; with gp_nvme= instead: has `function=01:00.0 key=nvme_bind`; `function=86:00.0 key=nvme_bind`; `function=85:00.0 key=gp_wlan`; not `key=gp_net`; `function=81:00.0` — 82:00.0, 84:00.0 (SATA) and their root ports with nvme_bind too; VMD with gp_vmd or nvme_bind; with gp_nvme=: 81:00.0 (the Lexar) is not among them, its root port neither (step 19b) |
| 19b-pci-gp-nvme | read | `log` `{"subsystem": "pci", "name": "gp_nvme."}` | not `gp_nvme.released`; with gp_nvme= instead: has `gp_nvme.released … function=81:00.0 … vendor=0x1d97 … device=0x5236`; not `gp_nvme.refused`; `gp_nvme.absent`; `gp_nvme.unreadable` — without gp_nvme=: nothing (no line at all); with it exactly one gp_nvme.released line; refused/absent/unreadable name the place or the line that did not match (the report's Platten row says the same) |
| 20-nvme | read | `nvme` `{}` | has `no NVMe controller`; with gp_nvme= instead: has `nvme0 at 81:00.0 1d97:5236`; `Lexar SSD NQ790 2TB`; not `no NVMe controller`; `nvme1 at` — no controller; with gp_nvme=: exactly one, nvme0 at 81:00.0 1d97:5236 with model "Lexar SSD NQ790 2TB" (no second nvme line); with gp_nvme= also log {"subsystem":"nvme","name":"probe"}: only function=81:00.0 (phase B1, step B1-08) |
| 21-block | read | `log` `{"subsystem": "block", "max": 200}` | has `internal_disks.off`; `stick_write=false usb_msd_write=false nvme_write=false`; not `internal_disks.one`; with gp_nvme= instead: has `internal_disks.one`; `stick_write=false usb_msd_write=false nvme_write=true`; not `internal_disks.off` — stick_id=true only with gp_stick=on; with gp_nvme=: only the named controller is driven |
| 22-snapshot | read | `log` `{"subsystem": "acpi", "name": "pci.snapshot"}` | has `storage_bars=`; not `storage_bars=-1` — seen 15.11.: storage_bars=18; -1 means poweroff and reboot would fail |
| 23-xhci | read | `log` `{"subsystem": "usb", "name": "xhci", "level": "info", "max": 400}` | has `xhci.probe`; `function=80:14.0`; `xhci.up`; not `xhci.probe_failed`; `xhci.start_stopped` — seen 15.11.: xhci0 00:0d.0 ports=3 and xhci1 80:14.0 ports=24, both xhci.up and legacy=true (the BIOS handed them over) |
| 24-gopfb | read | `log` `{"subsystem": "gopfb", "level": "info"}` | has `via=vmap width=2560 height=1440`; not `range.refused`; `map.fallback` — seen 15.11.: the framebuffer at 0xa000000000 inside card A's BAR0, write-combining, 234 us/MB |
| 25-stick | read | `log` `{"subsystem": "fat", "name": "stick"}` | has `stick.mounted`; `stick.decided`; with booted from the SSD: also has `stick.decided … nvme=true … rw=true` — rw=true with gp_stick=on (identity confirmed: writes below \BITSOS2\ only), else rw=false; booted from the SSD: stick.decided ... nvme=true rw=true (the BITSOS partition is /boot) |
| 26-input | read | `input` `{"max": 50}` | input devices of 10f5:501b (keyboard and mouse parts) and 046d:c08b (G502); seen 15.11. |
| 26b-latency | read | `latency` `{"last": 64}` | has `console:` — GP-START: after Georg turned the report pages with keys (and typed at the console), the console path's frames: the `console:` line (min/avg/p99/max, over budget) and the `console arrival_ns=…` frames with queue/draw/copy each; expected draw a few ms and copy about 3.4 ms for a whole frame [U]; a large queue stage is the next item (the input thread's wake-up), not the drawing |
| 27-net-info | read | `net_info` `{}` | has `rtl0`; `link=up 1000 Mbit/s`; `default-route` |
| 28-rtl-log | read | `log` `{"name": "rtl8126.", "max": 200}` | has `rtl8126.probe`; `rtl8126.fw_file … path=/usr/lib/firmware/rtl_nic/rtl8126a-3.fw … bytes=13232 … as_measured=true`; `rtl8126.up … xid=0x64a … firmware=true`; `rtl8126.stage … stage=probe … verdict=ok`; `rtl8126.stage … stage=firmware … verdict=ok`; `rtl8126.stage … stage=phy … verdict=ok`; `rtl8126.stage … stage=link … verdict=ok`; `rtl8126.stage … stage=rings … verdict=ok`; `rtl8126.stage … stage=lease … verdict=ok`; `rtl8126.stage … stage=gateway … verdict=ok`; `rtl8126.stage … stage=irq … verdict=ok`; not `rtl8126.passed_over`; `rtl8126.restart`; `rtl8126.mac_restart_failed`; `rtl8126.bring_up_failed`; `rtl8126.stage … verdict=failed` — stages 1-8 ok, stage 8 (irq) with irq_armed (kind=msix [U]); fw_played max_wait_us under 250 and slow_waits=0 (seen 15.11.: 0 and 0, U20) |
| 29-link | read | `log` `{"name": "link.", "max": 20}` | has `link.up ifname=rtl0 speed_mbps=1000 full_duplex=true`; not `link.down` |
| 30-msi | read | `hw_info` `{"section": "msi"}` | has `83:00.0` — one MSI-X vector for 83:00.0, its count rising between two calls [U] |
| 31-gpu-list | read | `gpu` `{"device": "list"}` | has `0000:04:00.0`; `RX 9070 XT`; `the boot display` — gpu devices: 1 with today's single card |
| 32-gpu-card0 | read | `gpu` `{"device": "card0"}` | has `writes: 0 register writes, 0 refused, 0 memory writes refused`; `attach: PASS`; `memsize: 16304 MiB`; not `MISSING on the stick` — bar0 0xa000000000 16384 MiB, discovery at VRAM offset 0x3faff0000, 46 ip lines, 12 module lines 'not built yet' (seen 15.11.), 12 firmware lines 'present ... (as measured)' [U: the stick of 15.11. had none: write the set with xtask stick --firmware gaming-pc, plan/guides/gaming-pc-stick.md]. Any other writes: line = a bug, stop |
| 33-gx-bringup | read | `gx_bringup` `{"until": "boot"}` | has `REFUSED`; `RX 9070 XT attached read-only` |
| 34-report-file | read | `list_dir` `{"path": "/boot/BITSOS2/REPORT"}` | has `GP0000` — only with gp_stick=on: the report file of this boot (and of up to two before; seen 15.11.: GP000001.TXT, 6330 bytes); read it with read_file encoding text and compare with step 03 |
| 35-snd | read | `snd` `{}` | seen 15.11.: five USB outputs (the headset at 10.4 twice; the board's jacks at 12.1: speaker, headphone, S/PDIF), no HDMI; the default goes to the USB headset. The tone is phase C's alone (snd_ton, only after Georg's yes) |
| 36-nvme-command | read | `pci_read` `{"function": "01:00.0", "offset": 4, "width": 2}` | bit 2 = the firmware left bus mastering on (gpd decision 4); also 81:00.0 and 86:00.0 with the same arguments; seen 15.11.: 0x0006 on all three |
| 37-data-window | read | `read_file` `{"path": "/proc/bitsos/data-window", "encoding": "text"}` | not judged — with gp_nvme= after bitsdisk at the keyboard (guide step 5): `window=armed … first=<385 GiB partition's first block> blocks=<its blocks>`, booted from the SSD also `boot_first=`/`boot_blocks=` of the 16 GiB BITSOS partition; any `why=`: stop, ask the desk |
| 38-data-moved | read | `read_file` `{"path": "/data/.stickdata-moved", "encoding": "text"}` | not judged — after `bitsdisk umzug` at the keyboard (guide step 6): `moved from /stickdata`; before it the file is missing (an error answer, fine) |
| 40-write-stick | write | `write_file` `{"path": "/boot/BITSOS2/GPTEST.TXT", "text": "remote desk check\r\n"}` | with gp_stick=on: written (then delete it with delete {"path":"/boot/BITSOS2/GPTEST.TXT"}); without: read-only file system (EROFS) |
| 41-write-config | write | `write_file` `{"path": "/boot/BITSOS2/CONFIG.TXT", "text": "nvme_bind=on\n"}` | refused — refused in every state |
| 50-ktest-rtl | ktest | `ktest` `{"filter": "gp.rtl."}` | has `passed=6 failed=0` — gp.rtl.registry and the five GamingPc tests; gp.rtl.arp holds the stack off the card for up to 5 s |
| 51-ktest-round | ktest | `ktest` `{"filter": "cp.cpu,cp.msr,cp.trap,cp.acpi,cp.irq,cp.time.calibration,cp.smp.all_up,cp.smp.hwp,cp.smp.spec_ctrl,cp.smp.topology,cp.smp.mtrr,cp.smp.apic_map,cp.smp.ap_registers,sc.idle,so.sysk,rb.metrics,gp.rtl."}` | has `failed=0` — expected passed=68 failed=0 skipped=15 (seen 15.11.: 67/1/15, the one red so.sysk.diskstats on an empty USB disk, repaired in GP-R2); cp.smp.hwp, cp.smp.topology, cp.irq.*, cp.acpi.live.* green on the real parts since 15.11. |

**ktest round, the counts** (class `GamingPc` runs only on this machine, class `Hardware` only on the Mini-PC):
- `ktest {"filter":"gp.rtl."}`: `passed=6 failed=0 skipped=0`.
- step 51's filter: `passed=68 failed=0 skipped=15` (the 62 of cp/69's bare boot, the six `gp.rtl.` tests; 7
  tests of QEMU runs and the 8 Mini-PC sensor tests skipped as "belongs to ..."). A `refused` line is not
  expected in it.
- Run one by one afterwards, each alone, [U]: `cp.smp.tsc_offsets_hardware` and `so.sysk.cpufreq_real` are class
  `Hardware` and therefore skipped here ("belongs to the Mini-PC run"); they stay unproven on this CPU.

**With the Lexar named, before Georg types at the console** (guide step 5, bitsdisk): `input {"max":50}`, note
`next_seq`; Georg presses `a s d f` at the console; `input {"since":<next_seq>,"wait_ms":30000}` must show key events of
the 10f5:501b keyboard (not the MCP device 0xffffff00), and the screen `asdf` behind `/ #`. Nothing: stop, Georg
switches off (nothing is written).

**Record** what ran (round 1 is in it since GP-R2): one line per test that passed in `host/tests/gamingpc/ran.txt` of the repository,
`<test> remote <yyyy-mm-dd> <generation>` (the generation from `hw_info {"section":"kernel"}`), handed to the
desk with the out folder. The ledger counts them; a missing line is "stale", never red.

## 5b. Round 2: the phases

One phase at a time, in this order: **A** (a new version over the network), **B1**, **B2**, **B3** (the SSD, in three
parts with Georg's guide steps between them), **C** (the tone in the headset), **D** (the network card with its
interrupt), **E** (the graphics card's read-only stage 2). Georg's guide `plan/guides/gaming-pc-runde2.md` has the same
order; tell him before each phase what he does, then run it:

```
node host\remote-desk\collect.mjs <address> --phase A --bundle <folder with KERNEL and INITRD>
node host\remote-desk\collect.mjs <address> --phase B1
```

- A phase prints its `before:` lines (check that they hold, ask Georg if one is unclear) and its `never:` lines, then
  runs its steps in order. **The first red ends it** (`STOP phase X at <step>: <what to do>`, exit 1): do what the
  line says, nothing else; the answers are in `host\remote-desk\out\<time>-phase-<X>\`. Never go on to the next
  phase after a red; tell the desk (a `Test | cause | fix` row) and wait.
- A step that needs Georg holds the phase (`ASK <step>: ...`, exit 3). For the tone (C): ask him exactly that, wait
  for his answer, then run the same command again with `--sound --from C04-tone`. For the keyboard check (B1-13):
  tell him what to type and start the same command with `--from B1-13-keys` at once, without waiting for an answer:
  the held run kept where the key events stood, so keys he types before the command reaches the step count too, and
  the step waits two minutes for them.
- A phase that needs a fact of the machine says it red before any step (`RED phase B1: gp_nvme_named is false, ...`)
  with what Georg or the desk must do first.
- `--from <step>` also resumes a phase after the desk fixed what stopped it (the facts are read again).
- Its write and ktest steps belong to the phase (no `--write`/`--ktest`); its install needs `--bundle`.
- The heartbeat runs as in section 3; phase A's install restarts the Gaming-PC, collect.mjs then connects again and
  reads the facts of the new start.
- `host\remote-desk\out\keep\lexar-guard-b1.txt`, `-b2.txt`, `-b3.txt` hold the Lexar's guard hash, each kept at the
  start of its phase and compared at its end. A hash is never compared across phases: before bitsdisk it covers all
  three partitions, after it the data window and the boot ESP are left out, and CachyOS mounts "daten" before B3.
  `keys-b1-13-keys.txt` is where the key events stood when Georg was asked.

**The tone (phase C), with Georg's yes.** `collect.mjs <address> --phase C` lists the outputs and stops with
`ASK C04-tone: ...`. Ask Georg in these words: "Setz bitte das Headset auf. Ich spiele gleich einen leisen Piepton,
eine Sekunde lang. Soll ich? (ja/nein)". Only on "ja": `collect.mjs <address> --phase C --from C04-tone --sound`. Read
the answer line part by part to the desk's table: `gefunden` (the headset), the rate (`Rate laut Gerät 48000 Hz`, or
`nennt seine Rate nicht`), `Strom gestartet`, `192000 Bytes in ~1000 ms gesendet`, `0 Pakete verpasst, 0 Fehler`,
`gestoppt, fertig`. Then ask Georg "Hast du einen leisen Piepton gehört, eine Sekunde lang, auf beiden Ohren?" and
write his answer word for word. A second tone only after a second yes. snd_ton is refused on any machine whose CONFIG.TXT
lacks the test key `ktest_georg=on` (the Gaming-PC stick has it, plan/guides/gaming-pc.md); it is the only call of
the round that makes a sound.

**The network interrupt (phase D).** The cable card runs on its interrupt in every boot; phase D watches it for two
minutes. Nothing is changed on the medium for it.

### Phase A: Neue Version über das Netz (install.mjs --upload, Neustart, Bestätigung)

Before (Georg / the desk):
- Georg started the Gaming-PC with F11 from the medium BitsOS runs from (round 2 starts from the stick; after phase B3 from the Lexar) and the report page names 'MCP erreichbar unter <address>'
- the desk's bundle is copied beside this folder: a folder holding KERNEL and INITRD (its numbered release target/xtask/update-<N>/, or BITSOS2\SYS\1\ of its image); --bundle <that folder>
- booted from the stick: the stick has gp_stick=on (A02 shows tiers=net,stick); booted from the Lexar: nothing more (the BITSOS partition takes the install)

| Step | Kind | Call | Expected | Red: what to do |
|---|---|---|---|---|
| A01-version | read | `version` `{}` | as step 01-version | stop; the answer file for the desk (Test, cause, fix) |
| A02-kernel | read | `hw_info` `{"section": "kernel"}` | as step 02-kernel — write the generation and bitsos_version= down (before) | stop; the answer file for the desk (Test, cause, fix) |
| A03-confirmed | confirm | heartbeat until reload_info says confirmed | answers — the running generation must be confirmed before a new one goes on the medium (the heartbeat confirms one that waits) | the running generation does not confirm: no install; reload_info by hand, tell the desk |
| A04-generations | read | `system_list` `{}` | answers — the generations on the boot medium before the install (write down the newest number: the bundle's must lie above it for a numbered release) | stop; the answer file for the desk (Test, cause, fix) |
| A05-install-dry | install | install.mjs `--bundle B --upload --dry-run` | has `install: dry run ok`; `would be installed` — nothing is written: BitsOS reads the uploads, checks the bundle (bitsboot's rules, disk guard, version marks) and plans the generation | the refusal line names why (a number not above the newest folder: the desk must give a higher one); nothing was written; tell the desk |
| A06-install | install | install.mjs `--bundle B --upload --reboot` | has `installed (NEW)`; `read back from the stick`; `reboot: MCP back`; `runs and confirmed itself (OK)` — about 1,000 write_file pieces for a 120 MB kernel, then the reboot (the connection ends for about a minute), the new generation's one try, reload_info every 2 s until it confirms itself | 'did not answer MCP from a new start' or 'not confirmed': the new kernel hangs or lost its network. Georg photographs the screen and holds the power button; at the next start bitsboot starts the generation before by itself (the one try is used up). Then A01-A04 again and tell the desk; never install again |
| A07-version | read | `version` `{}` | as step 01-version — the start line names the new generation | stop; the answer file for the desk (Test, cause, fix) |
| A08-kernel | read | `hw_info` `{"section": "kernel"}` | as step 02-kernel: also has `bitsos_version=` — bitsos_version=<N>(<date>) for the desk's numbered release, none(development) for a build without a number | stop; the answer file for the desk (Test, cause, fix) |
| A09-version-file | read | `read_file` `{"path": "/proc/bitsos/version", "encoding": "text"}` | has `number=` — number=<N> date=<YYYY-MM-DD> (0 and an empty date for a development build) | stop; the answer file for the desk (Test, cause, fix) |
| A10-confirmed | read | `reload_info` `{}` | has `confirmed (OK) in this boot` | stop; the answer file for the desk (Test, cause, fix) |
| A11-report | read | `hw_info` `{"section": "report"}` | as step 03-report: also has `Versionsnummer: ` — the start group says 'Laufende Version: <generation>' and 'Versionsnummer: <N> vom <date>' (or 'keine (ein Entwicklungsstand)') | stop; the answer file for the desk (Test, cause, fix) |
| A12-config | read | `log` `{"subsystem": "kernel", "name": "config"}` | as step 05-config | stop; the answer file for the desk (Test, cause, fix) |
| A13-loader | read | `log` `{"subsystem": "boot", "name": "loader.line", "max": 2000}` | as step 04-loader | stop; the answer file for the desk (Test, cause, fix) |
| A14-rtl-log | read | `log` `{"name": "rtl8126.", "max": 200}` | as step 28-rtl-log | stop; the answer file for the desk (Test, cause, fix) |
| A15-gpu-card0 | read | `gpu` `{"device": "card0"}` | as step 32-gpu-card0 | stop; the answer file for the desk (Test, cause, fix) |
| A16-report-file | read | `list_dir` `{"path": "/boot/BITSOS2/REPORT"}` | as step 34-report-file | stop; the answer file for the desk (Test, cause, fix) |
| A17-hwp-on | read | `log` `{"subsystem": "cpu", "max": 400}` | has `hwp.on cpu=0 … epp=0x40 policy=balanced`; `hwp.on cpu=1 … epp=0x40 policy=balanced`; … the same for every core up to `hwp.on cpu=19 … epp=0x40 policy=balanced`; not `hwp.failed`; `msr.faults`; `config.bad key=cpu_policy`; with cpu_policy=performance instead: `epp=0x10 policy=performance` on the 20 lines — GP-CPU2: one line per core 0-19 (Linux 7.3.1's Arrow Lake balanced value 64, performance 16); round 1's kernel had no epp= field | stop; the answer file for the desk (Test, cause, fix) |
| A18-msr-774 | read | `msr_read` `{"msr": "0x774"}` | as step 14-msr-774, but has `= 0x0000000040`; with cpu_policy=performance instead `= 0x0000000010` — bits 31:24 of the HWP request are the energy preference (0x0000000040005701; round 1 read 0x80005701); the high word is the activity window BitsOS keeps from the firmware, 0 on this PC: if it is not 0, read digits 9-10 by eye | stop; the answer file for the desk (Test, cause, fix) |
| A19-idle-events | read | `log` `{"subsystem": "sched", "name": "idle.", "max": 400}` | has `idle.state_mwait kind=2`; `idle.state_mwait kind=3`; `idle.mwait source= … states=2 … check=pending`; `idle.check … result=ok … states=2`; not `idle.c1_only`; `result=failed`; `idle.port_refused` — GP-CPU2: hints 0x21 (127 us) and 0x60 (1048 us), source=model (cst if the byte scan read the board's _CST), four idle.check_state lines, then `idle.check result=ok states=2 samples=4` | not a stop for the round (a failed self-check leaves every core in C1, round 1's state): write the `idle.check result=failed … reason=… detail=…` line down for the desk and go on with `--from A21-ktest-cpu`; only if no idle.check line is there 60 s after the boot: Georg photographs the screen and holds the power button, and the desk takes the photo to the next build |
| A20-idle-line | read | `sched` `{}` | has `idle mode=auto … states=C1,C2@mwait: … ,C3@mwait: … check=passed`; not `c1_only=`; `check_failed=` — the idle line `idle mode=auto factor=4 states=C1,C2@mwait:0x21/127us,C3@mwait:0x60/1048us source=model check=passed bm_demotions=0` | as A19: write it down for the desk and go on with `--from A21-ktest-cpu` |
| A21-ktest-cpu | ktest | `ktest` `{"filter": "gp.cpu."}` | has `passed=5 failed=0` — gp.cpu.cpufreq, gp.cpu.epp ('Energievorgabe 0x40 (ausgewogen) auf 20 Kernen'), gp.cpu.idle (waits up to 15 s for the self-check), gp.cpu.rank, gp.cpu.tsc_offsets, all read-only; the desk records the first run in host/tests/gamingpc/ran.txt | stop; the answer file for the desk (Test, cause, fix) |

Never in this phase:
- never install.mjs without --upload (Windows opens no port) and never twice in a row: after a red at A06 Georg's photo first
- never --config, --unpin or --allow-no-fallback
- no NVMe tool but the read-only nvme ({} or guard_hash), never data_move, never speed_mb
- no CONFIG.TXT line asked for or written: Georg edits it himself (BitsOS refuses it anyway)
- no reboot or poweroff unless the step says so; nothing on GitHub; no token or serial number into the repository

After it:
- write down the new generation and its Versionsnummer (A07, A08); the next phase is B1 once Georg did the SSD guide's steps 0-5.3

### Phase B1: SSD, vom Stick mit gp_nvme (vor bitsdisk)

Needs: `gp_nvme_named=true` (collect.mjs says it red before any step otherwise).

Before (Georg / the desk):
- the desk wrote the stick anew under CachyOS with this wave's bitsboot (guide plan/guides/gaming-pc-ssd.md step 0), Georg did steps 1-4 (the three lines on the stick) and 5.1-5.3 (F11, the stick, the yellow line with nvme=on, the photo of 'Platten')
- Georg has NOT typed bitsdisk yet; he is at the Gaming-PC's keyboard

| Step | Kind | Call | Expected | Red: what to do |
|---|---|---|---|---|
| B1-01-loader | read | `log` `{"subsystem": "boot", "name": "loader.line", "max": 2000}` | as step 04-loader | stop; the answer file for the desk (Test, cause, fix) |
| B1-02-config | read | `log` `{"subsystem": "kernel", "name": "config"}` | as step 05-config | stop; the answer file for the desk (Test, cause, fix) |
| B1-03-profile-guard | read | `log` `{"subsystem": "kernel", "name": "profile."}` | as step 06-profile-guard | stop; the answer file for the desk (Test, cause, fix) |
| B1-04-released | read | `log` `{"subsystem": "pci", "name": "gp_nvme."}` | as step 19b-pci-gp-nvme | stop; the answer file for the desk (Test, cause, fix) |
| B1-05-untouched | read | `log` `{"subsystem": "pci", "name": "untouched", "max": 400}` | as step 19-pci-untouched | stop; the answer file for the desk (Test, cause, fix) |
| B1-06-nvme | read | `nvme` `{}` | as step 20-nvme | stop; the answer file for the desk (Test, cause, fix) |
| B1-07-guard-hash | read | `nvme` `{"guard_hash": true}` | has `guard_hash ns1 `; not `UNREADABLE PARTS`; keeps the `guard_hash` lines as lexar-guard-b1 — before bitsdisk (no window armed yet) the hash covers the whole disk's first and last MiB and those of all three partitions, the two BitsOS ones included: compared only at B1-15 (after bitsdisk the window and the boot ESP leave the hash, so B2 keeps its own) | stop; the answer file for the desk (Test, cause, fix) |
| B1-08-nvme-probe | read | `log` `{"subsystem": "nvme", "name": "probe"}` | has `function=81:00.0`; not `function=01:00.0`; `function=86:00.0` — only the Lexar was probed; the two Samsungs never | stop; the answer file for the desk (Test, cause, fix) |
| B1-09-block | read | `log` `{"subsystem": "block", "max": 200}` | as step 21-block | stop; the answer file for the desk (Test, cause, fix) |
| B1-10-samsung-01 | read | `pci_read` `{"function": "01:00.0", "offset": 4, "width": 2}` | as step 36-nvme-command — the Samsung at 01:00.0: the command word as the firmware left it (round 1: 0x0006), nothing changed it | stop; the answer file for the desk (Test, cause, fix) |
| B1-11-samsung-86 | read | `pci_read` `{"function": "86:00.0", "offset": 4, "width": 2}` | as step 36-nvme-command — the Samsung at 86:00.0: as the firmware left it (round 1: 0x0006) | stop; the answer file for the desk (Test, cause, fix) |
| B1-12-report | read | `hw_info` `{"section": "report"}` | as step 03-report — Platten: the Lexar 'Treiber an (in CONFIG.TXT mit gp_nvme freigegeben)', both Samsungs '... nie freigebbar'; compare with Georg's photo | stop; the answer file for the desk (Test, cause, fix) |
| B1-13-keys | keys | `input` before, then `input {"since":…,"wait_ms":30000}` (Georg types) | key presses [30, 31, 32, 33] of 10f5:501b — the key events of the keyboard 10f5:501b itself (not MCP's injected device) within 2 minutes; Georg's photo shows 'asdf' behind '/ #' | **Asks Georg first:** Georg: an der Tastatur des Gaming-PCs erst die Leertaste, bis unten '/ #' steht, dann langsam a s d f tippen (kein Enter). Hinterher die vier Buchstaben mit der Rücktaste wieder löschen. no key of the real keyboard arrived: Georg switches off, nothing was written; tell the desk (bitsdisk needs the keyboard) |
| B1-14-window | read | `read_file` `{"path": "/proc/bitsos/data-window", "encoding": "text"}` | has `window=none why=no_nvme_data`; not `window=armed` — before bitsdisk: no nvme_data= line yet | stop; the answer file for the desk (Test, cause, fix) |
| B1-15-guard-again | read | `nvme` `{"guard_hash": true}` | not `UNREADABLE PARTS`; the `guard_hash` lines equal lexar-guard-b1 | what BitsOS must never write changed: STOP, Georg switches off, tell the desk |

Never in this phase:
- never start bitsdisk or 'bitsdisk umzug' over MCP (both refuse MCP; they are Georg's at the keyboard)
- no NVMe tool but the read-only nvme ({} or guard_hash), never data_move, never speed_mb
- no CONFIG.TXT line asked for or written: Georg edits it himself (BitsOS refuses it anyway)
- no reboot or poweroff unless the step says so; nothing on GitHub; no token or serial number into the repository

After it:
- tell Georg: guide steps 5.4-5.8 (Leertaste bis / #, bitsdisk, 1, die sechs Zeichen, Fertig, reboot, F11, Stick) and 6.1-6.2 (bitsdisk umzug, Fertig), NOT yet poweroff; then phase B2

### Phase B2: SSD, nach bitsdisk und bitsdisk umzug (vom Stick)

Needs: `gp_nvme_named=true`, `booted_from_ssd=false` (collect.mjs says it red before any step otherwise).

Before (Georg / the desk):
- Georg did guide steps 5.4-5.8 (bitsdisk: 'Fertig', reboot, F11, the stick) and 6.1-6.2 ('bitsdisk umzug': 'Fertig: ... Eintraege vom Stick sind auf /data ...'); he has not switched off

| Step | Kind | Call | Expected | Red: what to do |
|---|---|---|---|---|
| B2-00-guard | read | `nvme` `{"guard_hash": true}` | has `guard_hash ns1 `; not `UNREADABLE PARTS`; keeps the `guard_hash` lines as lexar-guard-b2 — kept at B2's start, compared at B2-08: with the window armed the data partition and the boot ESP are not hashed, so B1's hash (over all three partitions) is no measure here | stop; the answer file for the desk (Test, cause, fix) |
| B2-01-config | read | `log` `{"subsystem": "kernel", "name": "config"}` | as step 05-config: also has `nvme_data=` — bitsdisk added nvme_data=<the 385 GiB partition> | stop; the answer file for the desk (Test, cause, fix) |
| B2-02-window | read | `read_file` `{"path": "/proc/bitsos/data-window", "encoding": "text"}` | has `window=armed`; not `why=` — first=/blocks= of the 385 GiB partition (compare with ssd-zeilen.sh's lines) | a why= names the rule that refused: stop, tell the desk; nothing was written outside the window |
| B2-03-data-state | read | `read_file` `{"path": "/run/bitsos/data-state", "encoding": "text"}` | has `mounted pool rw` | stop; the answer file for the desk (Test, cause, fix) |
| B2-04-mounts | read | `run` `{"path": "/bin/sh", "args": ["-c", "grep -E ' /(boot|data) ' /proc/mounts"], "timeout_ms": 20000}` | has ` /data ext4 rw` — /data on the Lexar's window (/dev/mapper/data), /boot the stick; through /bin/sh because /proc/mounts is self/mounts, which MCP's own read_file (a kernel thread) cannot open | stop; the answer file for the desk (Test, cause, fix) |
| B2-05-moved | read | `read_file` `{"path": "/data/.stickdata-moved", "encoding": "text"}` | as step 38-data-moved: has `moved from /stickdata` | stop; the answer file for the desk (Test, cause, fix) |
| B2-06-block | read | `log` `{"subsystem": "block", "max": 200}` | as step 21-block | stop; the answer file for the desk (Test, cause, fix) |
| B2-07-nvme | read | `nvme` `{}` | as step 20-nvme | stop; the answer file for the desk (Test, cause, fix) |
| B2-08-guard | read | `nvme` `{"guard_hash": true}` | not `UNREADABLE PARTS`; the `guard_hash` lines equal lexar-guard-b2 — the same hash as B2-00 (both with the window armed: the data partition and the boot ESP are not in it) | what BitsOS must never write changed: STOP, Georg switches off, tell the desk |

Never in this phase:
- no NVMe tool but the read-only nvme ({} or guard_hash), never data_move, never speed_mb
- no CONFIG.TXT line asked for or written: Georg edits it himself (BitsOS refuses it anyway)
- no reboot or poweroff unless the step says so; nothing on GitHub; no token or serial number into the repository

After it:
- tell Georg: guide step 6.3 (poweroff), then step 7 under CachyOS (copy EFI and BITSOS2 without data.img onto the BITSOS partition) and step 8 (F11, the Lexar); then phase B3

### Phase B3: SSD, von der Lexar gestartet

Needs: `gp_nvme_named=true`, `booted_from_ssd=true` (collect.mjs says it red before any step otherwise).

Before (Georg / the desk):
- Georg did guide step 7 under CachyOS (EFI and BITSOS2 without data.img on the BITSOS partition) and step 8 (F11, 'UEFI OS (Lexar SSD NQ790 2TB)', the photo with 'Gestartet von der SSD')
- optional: the desk's bundle beside this folder (--bundle) for the install onto the BITSOS partition (B3-12)

| Step | Kind | Call | Expected | Red: what to do |
|---|---|---|---|---|
| B3-01-loader | read | `log` `{"subsystem": "boot", "name": "loader.line", "max": 2000}` | as step 04-loader | stop; the answer file for the desk (Test, cause, fix) |
| B3-02-guard | read | `nvme` `{"guard_hash": true}` | has `guard_hash ns1 `; not `UNREADABLE PARTS`; keeps the `guard_hash` lines as lexar-guard-b3 — kept anew for B3 (CachyOS mounted 'daten' since B2, and from the SSD both windows are armed): compared at B3-14 | stop; the answer file for the desk (Test, cause, fix) |
| B3-03-stick | read | `log` `{"subsystem": "fat", "name": "stick"}` | as step 25-stick | stop; the answer file for the desk (Test, cause, fix) |
| B3-04-mounts | read | `run` `{"path": "/bin/sh", "args": ["-c", "grep -E ' /(boot|data) ' /proc/mounts"], "timeout_ms": 20000}` | has `nvme0n1p2 /boot `; ` /data ext4 rw` — /boot the Lexar's second partition (BITSOS), /data its window; through /bin/sh (see B2-04) | stop; the answer file for the desk (Test, cause, fix) |
| B3-05-window | read | `read_file` `{"path": "/proc/bitsos/data-window", "encoding": "text"}` | has `window=armed … boot_first= … boot_blocks=`; not `why=` — both windows: the data partition and the 16 GiB BITSOS partition | stop; the answer file for the desk (Test, cause, fix) |
| B3-06-report | read | `hw_info` `{"section": "report"}` | as step 03-report | stop; the answer file for the desk (Test, cause, fix) |
| B3-07-untouched | read | `log` `{"subsystem": "pci", "name": "untouched", "max": 400}` | as step 19-pci-untouched | stop; the answer file for the desk (Test, cause, fix) |
| B3-08-nvme | read | `nvme` `{}` | as step 20-nvme | stop; the answer file for the desk (Test, cause, fix) |
| B3-09-block | read | `log` `{"subsystem": "block", "max": 200}` | as step 21-block | stop; the answer file for the desk (Test, cause, fix) |
| B3-10-no-samsung | write | `write_file` `{"path": "/boot/BITSOS2/CONFIG.TXT", "text": "gp_nvme=86:00.0/144d:a80a\n"}` | refused — a gp_nvme line for a Samsung through MCP: refused (CONFIG.TXT is never written by the file tools) | stop; the answer file for the desk (Test, cause, fix) |
| B3-11-generations | read | `system_list` `{}` | answers — the generations on the BITSOS partition (Georg copied the stick's) | stop; the answer file for the desk (Test, cause, fix) |
| B3-12-install | install | install.mjs `--bundle B --upload --reboot` (only with --bundle) | has `installed (NEW)`; `reboot: MCP back`; `runs and confirmed itself (OK)` — only with --bundle: a new generation onto the BITSOS partition (as phase A on the stick) | as A06: photo, power button, the previous generation starts by itself; tell the desk |
| B3-13-stick-again | read | `log` `{"subsystem": "fat", "name": "stick"}` | as step 25-stick — after B3-12's restart still from the SSD | stop; the answer file for the desk (Test, cause, fix) |
| B3-14-guard-again | read | `nvme` `{"guard_hash": true}` | not `UNREADABLE PARTS`; the `guard_hash` lines equal lexar-guard-b3 | what BitsOS must never write changed: STOP, Georg switches off, tell the desk |

Never in this phase:
- no NVMe tool but the read-only nvme ({} or guard_hash), never data_move, never speed_mb
- no CONFIG.TXT line asked for or written: Georg edits it himself (BitsOS refuses it anyway)
- no reboot or poweroff unless the step says so; nothing on GitHub; no token or serial number into the repository

After it:
- Georg's photo of 'lsblk -o NAME,SIZE,FSTYPE,LABEL,MODEL' under CachyOS later: the same three partitions below the Lexar, 'daten' still mounts

### Phase C: Ton im Headset (der eine Tontest, nur nach Georgs Ja)

Before (Georg / the desk):
- the stick's CONFIG.TXT has the test key ktest_georg=on (the guide's stick lines; without it snd_ton answers 'gesperrt')
- Georg's USB headset is plugged in (round 1: port 10.4) and he wears it; the session asked him and he said yes (README section 5, phase C)

| Step | Kind | Call | Expected | Red: what to do |
|---|---|---|---|---|
| C01-snd | read | `snd` `{}` | has `target=auto` — the USB headset first (round 1: 10.4 twice), the board's jacks at 12.1 after it, no HDMI | stop; the answer file for the desk (Test, cause, fix) |
| C02-uac | read | `log` `{"name": "uac", "max": 100}` | has `uac.bound`; not `uac2.jack_refused` — uac.bound for the headset and the board (vendor 0x26ce); uac2.jack lines with after=48000 | stop; the answer file for the desk (Test, cause, fix) |
| C03-no-sound-yet | read | `log` `{"subsystem": "usb_audio", "name": "uac.tone_check", "max": 20}` | not `uac.tone_check` — nothing played this boot before C04 (snd_ton writes that event) | a tone already played in this boot: no second one without Georg's new yes; with it run the phase with --sound --from C04-tone |
| C04-tone | sound | `snd_ton` `{"output": "headset"}` | has `Tontest `; `gefunden`; `Strom gestartet`; `0 Pakete verpasst, 0 Fehler`; `gestoppt, fertig`; not `gesperrt`; `Hauptschalter stumm`; `nicht gestartet`; `abgebrochen` — read the line part by part: 'gefunden' (the headset), 'Rate laut Gerät 48000 Hz' or 'nennt seine Rate nicht', 'Strom gestartet', '192000 Bytes in ~1000 ms gesendet', '0 Pakete verpasst, 0 Fehler', 'gestoppt, fertig' | **Asks Georg first:** Frag Georg: 'Setz bitte das Headset auf. Ich spiele gleich einen leisen Piepton, eine Sekunde lang. Soll ich? (ja/nein)'. Nur bei 'ja' weiter. the line names the step that failed (Strom nicht gestartet, zu wenig Bandbreite, Pakete verpasst ...): write it down, do not play again, tell the desk |
| C05-snd-after | read | `snd` `{}` | has `target=auto` — the output choice unchanged after the tone | stop; the answer file for the desk (Test, cause, fix) |

Never in this phase:
- never play anything but C04 (snd_ton once per yes), never an audible ktest, never snd_ton on the Mini-PC
- never twice without a new yes; nothing in the board's jacks unless Georg asks for the optional speaker check
- no NVMe tool but the read-only nvme ({} or guard_hash), never data_move, never speed_mb
- no CONFIG.TXT line asked for or written: Georg edits it himself (BitsOS refuses it anyway)
- no reboot or poweroff unless the step says so; nothing on GitHub; no token or serial number into the repository

After it:
- ask Georg: 'Hast du einen leisen Piepton gehört, eine Sekunde lang, auf beiden Ohren?' and write his answer word for word

### Phase D: Netzwerk mit Unterbrechung

Before (Georg / the desk):
- BitsOS runs on the Gaming-PC with gp_net=on (phase A's start): the cable card is driven on its interrupt

| Step | Kind | Call | Expected | Red: what to do |
|---|---|---|---|---|
| D01-rtl-log | read | `log` `{"name": "rtl8126.", "max": 200}` | as step 28-rtl-log: also has `rtl8126.irq_armed`; also not `rtl8126.irq_lost` — stage 8 ok 'Die Karte meldet sich von selbst (Unterbrechung)', rtl8126.irq_armed kind=msix [U: or msi]; no irq_lost, no restart | stop; the answer file for the desk (Test, cause, fix) |
| D02-msi | read | `hw_info` `{"section": "msi"}` | has `83:00.0 rtl8126`; not `stormed`; its count rises in 10 s — one vector for 83:00.0 (msix #0), its count rising over 10 s | stop; the answer file for the desk (Test, cause, fix) |
| D03-link | read | `log` `{"name": "link.", "max": 20}` | as step 29-link | stop; the answer file for the desk (Test, cause, fix) |
| D04-net-info | read | `net_info` `{}` | has `rtl0 mac= … link=up 1000 Mbit/s … irq=on`; `net.rtl8126.restarts=0`; `net.rtl8126.rx_errors=0` | stop; the answer file for the desk (Test, cause, fix) |
| D05-ktest-rtl | ktest | `ktest` `{"filter": "gp.rtl."}` | as step 50-ktest-rtl | stop; the answer file for the desk (Test, cause, fix) |
| D06-ping-irq | read | `log` `{"name": "ktest.rtl8126_ping", "max": 5}` | has `ktest.rtl8126_ping … irq=irq` — worst_rtt_us under 2000, rx_dropped=0, irqs rising | stop; the answer file for the desk (Test, cause, fix) |
| D07-later | read | `log` `{"name": "rtl8126.", "max": 200}` | as step 28-rtl-log: after 120 s; also has `rtl8126.irq_armed`; also not `rtl8126.irq_lost` — two minutes later: still no restart and the interrupt kept | stop; the answer file for the desk (Test, cause, fix) |
| D08-net-later | read | `net_info` `{}` | has `rtl0 mac= … irq=on`; `net.rtl8126.restarts=0`; `net.rtl8126.rx_errors=0` | stop; the answer file for the desk (Test, cause, fix) |
| D09-no-storm | read | `log` `{"subsystem": "irq", "name": "storm"}` | not `source=msi:83:00.0` — an irq storm line names its source since GP-R3; none may name the cable card | stop; the answer file for the desk (Test, cause, fix) |

Never in this phase:
- never restart the card (no reload of the driver, no ktest beyond D05's gp.rtl.)
- no NVMe tool but the read-only nvme ({} or guard_hash), never data_move, never speed_mb
- no CONFIG.TXT line asked for or written: Georg edits it himself (BitsOS refuses it anyway)
- no reboot or poweroff unless the step says so; nothing on GitHub; no token or serial number into the repository


### Phase E: Grafikkarte Stufe 2 (nur die lesenden Schritte: state, alive, bootloader, smu)

Before (Georg / the desk):
- the RX 9070 XT's firmware set is on the boot medium (A15 / step 32: 12 lines 'present ... (as measured)')

| Step | Kind | Call | Expected | Red: what to do |
|---|---|---|---|---|
| E01-gpu-list | read | `gpu` `{"device": "list"}` | as step 31-gpu-list | stop; the answer file for the desk (Test, cause, fix) |
| E02-gpu-card0 | read | `gpu` `{"device": "card0"}` | as step 32-gpu-card0 — the look writes nothing here: its temperature read (Linux's metrics message, three writes of the SMU's mailbox) goes out only once `pmfw` passed in this boot, which this phase never runs | stop; the answer file for the desk (Test, cause, fix) |
| E03-state | read | `gx_bringup` `{"device": "card0", "stage": "state"}` | has `Stand zeigen: fertig.`; `state: PASS`; `writes: 0 register writes (0 failed), 0 refused`; not `refused: ` | stop; the answer file for the desk (Test, cause, fix) |
| E04-alive | read | `gx_bringup` `{"device": "card0", "stage": "alive"}` | has `Sicherheitsprozessor prüfen`; `writes: 0 register writes (0 failed), 0 refused`; `C2PMSG_81`; not `refused: ` — a look at the card, nothing written: 'fertig.' with C2PMSG_81 0x00000000 and 'secure OS: not running (a cold card); the stage sos loads it' (the trace of 18.11.: a cold card has no secure OS), and where the HDP flush's remap points ('in this boot the stage nbif did not write it yet'; 'vram: not planned yet (the stage gmc plans it)'). 'secure OS: running' (C2PMSG_81 not 0): write it down for the desk | a refused: line or a write: a bug; stop the GPU calls, tell the desk |
| E05-bootloader | read | `gx_bringup` `{"device": "card0", "stage": "bootloader"}` | has `Startlader prüfen`; `writes: 0 register writes (0 failed), 0 refused`; not `refused: ` — Linux's wait for the bootloader alone, nothing written: 'Startlader prüfen: fertig.' with 'C2PMSG_35 after the wait: Ok(80000000)' on a cold card, or with 'secure OS alive (C2PMSG_81 …)' when E04 said running. 'angehalten: Startlader der Karte meldet sich nicht bereit' (after Linux's 1000000 reads): write it down for the desk, no GPU call after it | stop; the answer file for the desk (Test, cause, fix) |
| E06-smu | read | `gx_bringup` `{"device": "card0", "stage": "smu"}` | has `Energieverwaltung befragen: fertig.`; `smu version`; `driver interface`; `(0 failed), 0 refused`; not `refused: ` — twelve questions through the SMU's mailbox (37 register writes, each a question, no setting): firmware flags at SMN 0x3b10024, smu version 104.80.0 [U10], driver interface 0x33 | write the first line down; never ask the SMU again in this boot; the phase ends here |
| E07-state | read | `gx_bringup` `{"device": "card0", "stage": "state"}` | has `Stand zeigen: fertig.`; `state: PASS`; not `refused: ` — the session line: what this boot did ('next of the chain nbif', nbif not run, gart off, ih off, window none, the smu asked), no bootloader command, no ring | stop; the answer file for the desk (Test, cause, fix) |

Never in this phase:
- never the stages nbif, gmc, ih, sos, ring, toc, tmr, fw, autoload, fwrest, ta, pmfw, gcboot, gchub, gccp, mes, mesq, gcring, sdma, sdmatest, ringtest or destroy in this round (Georg 18.11.: the card is touched only when all waves up to the first desktop are built, plan/spec/gpu-rdna4.md section 6), never the 780M's until
- never smu twice in one boot ('noch mit einer alten Frage' or 'Fehlerzustand' on a repeat is the protection working); after a red of E06 the SMU part is over for this round
- after this phase the PC goes fully off (power button or poweroff), never a warm reboot into CachyOS
- no NVMe tool but the read-only nvme ({} or guard_hash), never data_move, never speed_mb
- no CONFIG.TXT line asked for or written: Georg edits it himself (BitsOS refuses it anyway)
- no reboot or poweroff unless the step says so; nothing on GitHub; no token or serial number into the repository

After it:
- tell Georg: the Gaming-PC fully off now (not a restart) before CachyOS starts again


## 6. When a step is red

| Red | Means | Do |
|---|---|---|
| step 03 or 10 says `EINGESCHALTET` / `dma.blocked` | the firmware left the IOMMU translating; no DMA driver runs | **stop**. Nothing more over MCP; tell Georg: photo, power off. The next step is a minimal VT-d identity map, not a retry |
| no MCP at all | no link, no address, or MCP did not start | Georg's photo of the page: the network rows (`Netzwerk n/8: ...`) say the stage that stopped; with `gp_stick=on` the report is also in `\BITSOS2\REPORT\GP<number>.TXT` on the stick (Georg brings the stick to this PC, or the next CachyOS start reads it) |
| step 04 has `generation.unguarded` | a generation on the stick is older than the disk guard and was skipped | harmless for this boot; the desk removes or replaces that generation |
| step 06 has `profile.guard` | the stick's bitsboot is older than GP-GUARD: everything is off | the desk writes a new stick; nothing to repair remotely |
| step 15/16 lack `80:14.0` or `peer.root` | the second PCI root was not found: no keyboard, no network | photo of the page; desk repair (B2) |
| step 22 has `storage_bars=-1` | the ACPI guard refuses every firmware register: power off and reboot fail | do not call `reboot`; Georg holds the power button |
| step 28 has `restart` or a stage `Failed` | the card stopped or restarted | save steps 27-30, nothing else on the network; the stage line names the step |
| step 32 `writes:` names anything but 0 | the read-only attach wrote | stop GPU calls, report it as a bug |
| step 32 has `MISSING on the stick` | the RX 9070 XT's firmware set is not in `\BITSOS2\firmware\amdgpu\` (round 1's stick had none) | harmless for the read-only attach. The desk made the set as a tree (`cargo xtask stick --firmware gaming-pc --tree <dir>`, copied along with this folder); with `gp_stick=on`: `node host\mcp\push.mjs <address> --tree <dir>\firmware --name firmware --add --via write --gaming-pc --dry-run`, then the same without `--dry-run`; step 32 again: 12 lines `present ... (as measured)` |
| any other | the answer file in the out folder | note it in a `Test | cause | fix` table for the desk |

## 7. Never

- Never send a tool that writes a disk other than the boot stick. Of the NVMe tools only the read-only `nvme` is
  allowed, with `{}` or `{"guard_hash":true}`, never `speed_mb`; never an NVMe write tool and never `data_move` (it
  starts the copy; the move is `bitsdisk umzug`, typed by Georg at the keyboard, and step 38 reads its marker).
  Never ask for `nvme_write`, `nvme_data`, `nvme_boot`, `stick_write`, `usb_msd_write`, `mcp`, `mcp_confirmer`,
  `gp_net`, `gp_stick`, `gp_nvme` lines in CONFIG.TXT (they are Georg's own edit; BitsOS refuses them anyway).
- Never run an audible ktest. The one sound of the round is phase C's `snd_ton` to the headset, only after Georg's
  yes in this session, once per yes (section 5b); nothing else plays.
- Never change the picture (`ktest_picture`), the BIOS, or anything on this Windows machine's own disks beyond this
  folder and its `out\`.
- Never push to GitHub, never copy the token or a capture with serial numbers or MAC addresses into the repository.
- Never `poweroff` while Georg is not told; `reboot` only when a step needs it (it ends your connection for about a
  minute).
- Never a GPU stage beyond `state`, `alive`, `bootloader`, `smu` (phase E), never `smu` twice in one boot; after phase E
  the Gaming-PC goes fully off before CachyOS, never a warm restart into it.

## 8. Back to normal

Georg holds the power button (or you call `poweroff`), takes the stick out and starts CachyOS: nothing on Georg's
Windows and CachyOS SSDs was read or written; with `gp_nvme=` BitsOS wrote on the Lexar only inside its own two
partitions (BITSOS and the data partition), never in "daten". The desk on CachyOS takes over again with the out folder.
