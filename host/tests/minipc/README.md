# Mini-PC runs (Georg 30.10.: programs are tested only on the Mini-PC)

The desk runs these against the real machine; `xtask fullrun` does not. Rule: `plan/CONTRACTS.md` section 4.

| Test | Was | Runs now |
|---|---|---|
| `ul.pkg.libreoffice`, `_sha`, `_convert` | gate ktests, class Safe | class Program: MCP `ktest` on the Mini-PC (`lo.sh`) |
| `ul.pkg.brave`, `_sha`, `_fonts` | gate ktests, class Safe | class Program: MCP `ktest ul.pkg.brave` on the Mini-PC |
| `ul.pkg.cobalt` | gate ktest, class Safe | class Program: MCP `ktest ul.pkg.cobalt` on the Mini-PC |
| `ul.pkg.brave_fonts_mtime` | QemuOnly | deleted: it made in QEMU the state the Mini-PC's stick has, which `ul.pkg.brave_fonts` meets there |
| `lo/10-office.sh` | QEMU scenario | deleted; `lo.sh` and the check list in `plan/notes/lo-LO-1.md` |
| `br/10-start.sh` | QEMU scenario | check list in `plan/notes/br-BR-3.md` |
| `br/40-download.sh` | QEMU scenario | check list in `plan/notes/br-BR-7.md` |
| `br/50-sandbox.sh` | QEMU scenario | check list in `plan/notes/br-BR-6b.md` |
| `ac/10-actions.sh` | QEMU scenario | check list in `plan/notes/ac-AC-3.md` |
| `yt/10-nplb.sh` | QEMU scenario | check list in `plan/notes/yt-YT-4a.md` |
| `yt/20-avsync.sh` | QEMU scenario | check list in `plan/notes/yt-YT-5.md` |
| `yt/30-root.sh` | QEMU scenario | check list in `plan/notes/yt-YT-6.md` |

Stay where they were: `ul.pkg.cobalt_profile` (QemuOnly: the launcher with a stub tree, no program), `ul.pkg.stick`
(ninja, no tree), the build-host checks `lo/05-stage.sh`, `br/20-launcher.sh`, `br/25-stage.sh`, `br/30-bench.sh`,
`pk/30-brave-package.sh`, `yt/launcher.sh`, `fs/31-fat-cold.sh` (a stand-in file in QEMU, no program).
Xwayland and Mesa count as the desktop's own pieces and stay with WY's scenarios.
