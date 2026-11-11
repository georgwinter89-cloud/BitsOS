//! GM-8: `\BITSOS2\vkquake\` (vkQuake and SDL2 of their pinned releases, LibreQuake's data, on the Mesa tree's glibc) and its
//! launcher `\BITSOS2\bin\vkquake` as the package list names them. Class Program: on the Mini-PC only, where a missing tree
//! is a failure (CONTRACTS section 4); `host/tests/minipc/quake.sh` starts them first. Rerunnable; no window, no sound, no GPU
//! job: only the game's server starts, on the stick's files, with a home in a scratch folder.
//! - `ul.pkg.vkquake`: every file's size, the SHA-256 of all up to 1 MiB, the pieces a start needs, the licences, the loader
//!   the program names, `vkquake --check`, and the server: the tree's libraries load, map `start` of the data loads.
//! - `ul.pkg.vkquake_sha`: the SHA-256 of the bigger files (the program, SDL2, LibreQuake's two paks; ~200 MB).

use super::mesa::{sh, tree_ok};
use super::pkg::{Line, list, program_tree, tree_list};
use super::{SEC, finish, scratch, start_as, text};
use alloc::format;
use alloc::vec::Vec;
use ktest::{TestResult, kassert, ktest};

const TREE: &str = "/boot/BITSOS2/vkquake/";
const LAUNCHER: &str = "vkquake";
const LOADER: &str = "/boot/BITSOS2/mesa/lib/ld-linux-x86-64.so.2";
/// Files up to this size are hashed in `ul.pkg.vkquake` (a test cost, not a limit).
const HASH_IN_MAIN: u64 = 1 << 20;
const FILES: &[&str] = &[
    "bin/vkquake",
    "VERSION.txt",
    "PACKAGES.txt",
    "lib/libSDL2-2.0.so.0",
    "lib/libasound.so.2",
    "lib/libwayland-cursor.so.0",
    "lib/libvorbisfile.so.3",
    "id1/pak0.pak",
    "id1/pak1.pak",
    "LICENSES/vkquake/LICENSE.txt",
    "LICENSES/sdl2/LICENSE.txt",
    "LICENSES/librequake/COPYING-BSD3",
    "LICENSES/librequake/COPYING-GPL2",
];
/// The longest wait for one small run. A bound of a stuck program: the first start reads its libraries from the stick.
const RUN_SECS: u64 = 300;

fn vkquake() -> TestResult {
    let l = list()?;
    let own = tree_list(TREE)?;
    let (tree, launcher) = program_tree(&l, &own, TREE, LAUNCHER)?;
    let has = |rel: &str| tree.iter().any(|e| e.path.strip_prefix(TREE) == Some(rel));
    for rel in FILES {
        kassert!(has(rel), "{TREE}{rel} is not in the image's list (stage again with user/ports/vkquake/scripts/stage-v2.sh)");
    }
    let (hashed, sized) = tree_ok(&tree, |e| e.size <= HASH_IN_MAIN, "vkquake")?;

    // The program runs on the Mesa tree's glibc: its PT_INTERP names that loader, near the start of the file.
    let prog = format!("{TREE}bin/vkquake");
    let f = vfs::root_ns().open(&prog).map_err(|e| format!("{prog}: {e:?}"))?;
    let mut head = [0u8; 4096];
    let n = f.read_at(0, &mut head).map_err(|e| format!("read {prog}: {e:?}"))?;
    kassert!(head[..n].windows(LOADER.len()).any(|w| w == LOADER.as_bytes()), "{prog} does not name {LOADER} as its loader");

    let out = sh(&format!("exec sh {launcher} --check"), RUN_SECS)?;
    kassert!(out.lines().any(|x| x.starts_with("vkquake: libraries: ") && x.contains(" resolve inside ")), "vkquake --check:\n{out}");
    kassert!(out.contains("vkquake: drivers: radeon_icd"), "vkquake --check:\n{out}");

    // The server as uid 1000, whose home the scratch folder is: no window, no sound, no GPU. Only on loopback.
    let d = scratch("vkquake");
    let r = (|| -> TestResult {
        let s = start_as("/bin/sh", &["sh", "-c", &format!("mkdir -p {d}/home && chown -R 1000:1000 {d}")], None, false, Some(0))?;
        kassert!(finish(&s, 30 * SEC)? == 0, "mkdir {d}: {}", s.both());
        let cmd = format!("VKQ_HOME={d}/home exec sh {launcher} -dedicated 1 -ip 127.0.0.1 -port 26123 +map start +status +quit");
        let s = start_as("/bin/sh", &["sh", "-c", &cmd], None, false, Some(1000))?;
        let code = finish(&s, RUN_SECS * SEC)?;
        let out = text(&s.out);
        kassert!(code == 0, "{cmd}: exit {code}\n{}", s.both());
        for want in ["Using SDL version 2.32.10", "version: vkQuake 1.37.0", "map:     start"] {
            kassert!(out.contains(want), "{cmd}: no '{want}'\n{out}");
        }
        Ok(())
    })();
    let _ = sh(&format!("rm -rf {d}"), 60);
    r?;
    klog::event!(Info, "ul", "pkg.vkquake", files = tree.len(), hashed_bytes = hashed, sized_bytes = sized);
    Ok(())
}
ktest!("ul.pkg.vkquake", Program, vkquake);

/// The bigger files of the tree.
fn vkquake_sha() -> TestResult {
    let l = list()?;
    let own = tree_list(TREE)?;
    let (tree, _) = program_tree(&l, &own, TREE, LAUNCHER)?;
    let big: Vec<Line> = tree.into_iter().filter(|e| e.size > HASH_IN_MAIN).collect();
    kassert!(big.iter().any(|e| e.path.ends_with("/id1/pak0.pak")), "pak0.pak is not among the big files");
    let t0 = time::now_ns();
    let (hashed, _) = tree_ok(&big, |_| true, "vkquake")?;
    klog::event!(Info, "ul", "pkg.vkquake_sha", files = big.len(), hashed_bytes = hashed, read_ms = (time::now_ns() - t0) / 1_000_000);
    Ok(())
}
ktest!("ul.pkg.vkquake_sha", Program, vkquake_sha);
