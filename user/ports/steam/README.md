# user/ports/steam: Valve's Steam client on V2 (GM-2a)

`bitsos/steam` is the launcher (`\BITSOS2\bin\steam`), `scripts/stage-v2.sh` makes the tree `\BITSOS2\steam\` from Valve's
own files. Nothing of Valve's is in git: `pins.txt` holds names and hashes. Steam runs only on the Mini-PC (Georg 30.10.):
`bash host/tests/minipc/steam.sh <host> --tls auto --token-file F`. The tree holds regular files only (FAT): the client stays in
its published zips, the provider in a tar. Georg logs in himself; no credential is read, stored or passed anywhere.

| Tree path | What |
|---|---|
| `bootstraplinux_ubuntu12_32.tar.xz` | Valve's bootstrap (`steam.sh`, the old 32-bit bootstrapper), from the steam-launcher deb |
| `package/` | the client's manifest (channel `publicbeta`) and its 28 zips, as published; the launcher seeds Steam's `package/` with them |
| `gfx.tar.gz` | the graphics provider `user/ports/steamrt/out/gfx` (Mesa, glibc; SX-3c), unpacked on `/data` at the first start |
| `bin/realpath` | `files/realpath`: `--no-symlinks` for Valve's `pv-run.sh`, which BusyBox's realpath lacks |
| `dbus/` | the build host's `dbus-daemon` with its libraries and `files/session.conf`: the session bus the launcher starts for Steam |
| `LICENSES/`, `VERSION.txt` | the deb's copyright file and the Steam subscriber agreement; what the tree was made from |

`reaper/` is not part of the tree: a small crate, `/bin/reaper` on the image. Steam starts every game through `steamrt64/reaper`, which
is a 32-bit program V2 cannot run, so before every start the launcher copies the 64-bit stand-in over it (stage `reaper`; `steam --check`
says which one is there). Differences from Valve's and the list of other i386 files: `plan/notes/sx-GM-2b.md`.

The start follows Valve's own path: `steam.sh` sees `package/beta` (not a stable name) and `.steam-enable-steamrt64-client`, runs
`steamrt64/steam` (64-bit), which installs its packages, then `pv-run.sh` -> `srt-client-container` -> pressure-vessel (the client's
own steamrt3c runtime, in `runtime_steamrt_ubuntu12.zip`) -> the client in the container -> `steamwebhelper` (CEF). Needs on V2:
`/bin/bash` on the image, an ext4 data volume, the desktop's Xwayland on `:0`, the network.

Windows games go through Proton in Wine's new WoW64 mode: the launcher gives Steam `PROTON_USE_WOW64=1`, so Proton takes its
64-bit-only Wine (`files/bin-wow64/`) instead of the classic one, whose `files/bin/wine` is i386. `steam --check` lists the
installed Proton versions and whether each has that Wine. Why, and the Mini-PC check list: `plan/notes/gm-PROTON.md`.

## Downloads (cache `$V2_DOWNLOADS/steam`, 720 MB; the stage 825 MB; Georg's OK for fetching Steam: 2.11.)

| File | Source | Pinned by | Licence |
|---|---|---|---|
| `steam-launcher_1.0.0.87_amd64.deb` | https://repo.steampowered.com/steam/archive/stable/ | SHA256 in `pins.txt` | Steam subscriber agreement; launcher scripts see its copyright file |
| `steam_client_publicbeta_ubuntu12` | https://client-update.steamstatic.com/ (no versioned URL: the pinned copy is the cache's) | version + SHA256 in `pins.txt` | Steam subscriber agreement |
| the manifest's zips | same site, by hashed names | the manifest's `sha2` | Steam subscriber agreement; the runtime's packages their own open-source licences |

`STEAM_REPIN=1 bash scripts/stage-v2.sh` takes Valve's newest manifest and rewrites `pins.txt`. A client newer than the pin needs
nothing on the stick: Steam updates itself from the CDN on `/data`.
