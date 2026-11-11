//! GM-2a: `\BITSOS2\steam\` (Valve's bootstrap, the client's zips, the graphics provider) and its launcher
//! `\BITSOS2\bin\steam` as the package list names them. Class Program: on the Mini-PC only, where a missing tree is a
//! failure (CONTRACTS section 4); `host/tests/minipc/steam_check.py --ktest` starts it first. Rerunnable; `--check` and
//! `--env` start nothing and write nothing. The desktop and the data volume must be up, as for the start itself.
//! - `ul.pkg.steam`: every file's size, the SHA-256 of all up to 1 MiB, the pieces a start needs, `steam --check` as uid 0
//!   and 1000, and the environment Steam gets (`steam --env`).

use super::mesa::{sh, tree_ok};
use super::pkg::{list, program_tree, tree_list};
use super::{SEC, finish, start_as, text};
use alloc::format;
use ktest::{TestResult, kassert, ktest};

const TREE: &str = "/boot/BITSOS2/steam/";
const LAUNCHER: &str = "steam";
/// Files up to this size are hashed (a test cost, not a limit): the client's zips are checked by Steam itself.
const HASH_IN_MAIN: u64 = 1 << 20;
const FILES: &[&str] = &["bootstraplinux_ubuntu12_32.tar.xz", "gfx.tar.gz", "bin/realpath", "VERSION.txt"];
/// The longest wait for one `--check`. A bound of a stuck program: it reads the tree's list from the stick.
const RUN_SECS: u64 = 300;

/// `steam --check` as `uid`: exit 0, every line a start depends on, nothing left running.
fn check_as(launcher: &str, uid: u32) -> TestResult {
    let cmd = format!("exec sh {launcher} --check");
    let s = start_as("/bin/sh", &["sh", "-c", &cmd], None, false, Some(uid))?;
    let code = finish(&s, RUN_SECS * SEC)?;
    let out = text(&s.out);
    kassert!(code == 0, "uid {uid}: {cmd}: exit {code}\n{}", s.both());
    for want in ["steam: tree ", "steam: bash: /bin/bash", "steam: xwayland display=:0", "steam: user: runs as uid 1000, groups 1000", "steam: data: /data/home/1000 on "] {
        kassert!(out.lines().any(|x| x.starts_with(want)), "uid {uid}: {cmd}: no line '{want}'\n{out}");
    }
    kassert!(out.lines().any(|x| x.starts_with("steam: tree ") && x.contains(": ok, ")), "uid {uid}: {cmd}: the tree\n{out}");
    for bad in ["steam: running:", "steam: still running"] {
        kassert!(!out.lines().any(|x| x.starts_with(bad)), "uid {uid}: {cmd}: '{bad}': a Steam is up, end it first\n{out}");
    }
    Ok(())
}

fn steam() -> TestResult {
    let l = list()?;
    let own = tree_list(TREE)?;
    let (tree, launcher) = program_tree(&l, &own, TREE, LAUNCHER)?;
    let has = |rel: &str| tree.iter().any(|e| e.path.strip_prefix(TREE) == Some(rel));
    for rel in FILES {
        kassert!(has(rel), "{TREE}{rel} is not in the image's list (stage again with user/ports/steam/scripts/stage-v2.sh)");
    }
    let (hashed, sized) = tree_ok(&tree, |e| e.size <= HASH_IN_MAIN, "steam")?;

    // GM-2b: the launcher copies it over Valve's 32-bit reaper before every start; without it no game starts.
    sh("test -x /bin/reaper", RUN_SECS)?;
    check_as(&launcher, 0)?;
    check_as(&launcher, 1000)?;

    let env = sh(&format!("exec sh {launcher} --env"), RUN_SECS)?;
    for want in ["DISPLAY=:0", "HOME=/data/home/1000"] {
        kassert!(env.lines().any(|x| x == want), "steam --env: no {want}\n{env}");
    }
    kassert!(!env.lines().any(|x| x.starts_with("WAYLAND_DISPLAY=")), "steam --env: Steam must see X11 only\n{env}");
    klog::event!(Info, "ul", "pkg.steam", files = tree.len(), hashed_bytes = hashed, sized_bytes = sized);
    Ok(())
}
ktest!("ul.pkg.steam", Program, steam);
