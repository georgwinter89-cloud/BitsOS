//! GM-1d: `\BITSOS2\mesa\` (Arch's Mesa, RADV, radeonsi, the Vulkan loader, vulkan-tools, mesa-utils with their own
//! glibc), its launcher `\BITSOS2\bin\mesa-run` and `\BITSOS2\deqp\` (the Vulkan conformance runner, its data and its
//! subset) as the package list names them. Class Program: on the Mini-PC only, where a missing tree is a failure
//! (CONTRACTS section 4); `host/tests/minipc/vk.sh` starts them first. Rerunnable, and nothing here assumes QEMU or the
//! machine's state: they read the stick, run on lavapipe (the CPU, so also before a GPU node exists and with the
//! desktop running) and write only below a scratch folder of their own. No system library is hashed: both trees bring
//! everything they load.
//! - `ul.pkg.mesa`: every file's size, the SHA-256 of all up to 1 MiB, the pieces a Vulkan and EGL start needs, each
//!   package's licence, the trees that run on this tree's loader, `mesa-run --check`, `vulkaninfo --summary` on lavapipe.
//! - `ul.pkg.mesa_sha`: the SHA-256 of the bigger files (libLLVM, libgallium, the drivers; ~280 MB).
//! - `ul.pkg.deqp`: the same for the deqp tree, and one conformance case (`dEQP-VK.info.build`) on lavapipe.

use super::pkg::{Line, Sha256, TEST_STICK_ID, entries, list, program_tree, tree_list};
use super::{SEC, finish, read, scratch, start_as, text};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use ktest::{TestResult, kassert, ktest};

const MESA: &str = "/boot/BITSOS2/mesa/";
const DEQP: &str = "/boot/BITSOS2/deqp/";
const LAUNCHER: &str = "mesa-run";
const LOADER: &str = "/boot/BITSOS2/mesa/lib/ld-linux-x86-64.so.2";
/// Files up to this size are hashed in `ul.pkg.mesa` (a test cost, not a limit).
const HASH_IN_MAIN: u64 = 1 << 20;
const PIECE: usize = 1 << 20;
/// What the Mini-PC's Vulkan run and the desktop's own pieces need of the tree.
const MESA_FILES: &[&str] = &[
    "run",
    "PACKAGES.txt",
    "bin/vulkaninfo",
    "bin/vkcube",
    "bin/vkgears",
    "bin/eglinfo",
    "bin/eglgears_wayland",
    "bin/es2gears_wayland",
    "bin/peglgears",
    "lib/ld-linux-x86-64.so.2",
    "lib/libc.so.6",
    "lib/libvulkan.so.1",
    "lib/libvulkan_radeon.so",
    "lib/libvulkan_lvp.so",
    "lib/libEGL_mesa.so.0",
    "lib/gbm/dri_gbm.so",
    "lib/dri/radeonsi_dri.so",
    "share/vulkan/icd.d/radeon_icd.json",
    "share/vulkan/icd.d/lvp_icd.json",
    "share/glvnd/egl_vendor.d/50_mesa.json",
    "share/libdrm/amdgpu.ids",
];
/// Packages whose licence text the tree must carry (the ones that make it what it is).
const MESA_LICENCES: &[&str] = &["mesa", "vulkan-radeon", "vulkan-swrast", "vulkan-icd-loader", "vulkan-tools", "mesa-utils", "glibc"];
const DEQP_FILES: &[&str] = &["deqp-vk", "subset.txt", "VERSION.txt", "LICENSES/vk-gl-cts/LICENSE"];
/// Programs of other parts of the stick that run on this tree's loader: without it they cannot start.
const ON_THIS_LOADER: &[&str] = &["/boot/BITSOS2/bin/bitscomp-gpu", "/boot/BITSOS2/xwayland/bin/Xwayland"];
/// The longest wait for one small program of the tree. A bound of a stuck program: the first start reads its libraries
/// from the stick.
const RUN_SECS: u64 = 300;

/// A regular file of the size of `l`; with `hash` also its SHA-256, read in pieces.
fn file_ok(l: &Line, hash: bool) -> TestResult {
    let ns = vfs::root_ns();
    let a = ns.lookup(l.path).map_err(|e| format!("{}: {e:?}", l.path))?.attr();
    kassert!(a.mode & uapi::S_IFMT == uapi::S_IFREG, "{}: mode {:o}", l.path, a.mode);
    kassert!(a.size == l.size, "{}: {} bytes, the list says {}", l.path, a.size, l.size);
    if !hash {
        return Ok(());
    }
    let f = ns.open(l.path).map_err(|e| format!("open {}: {e:?}", l.path))?;
    let mut buf: Vec<u8> = Vec::new();
    buf.try_reserve_exact(PIECE).map_err(|_| String::from("no memory for the read buffer"))?;
    buf.resize(PIECE, 0);
    let (mut h, mut at) = (Sha256::new(), 0u64);
    while at < l.size {
        let n = f.read_at(at, &mut buf).map_err(|e| format!("read {} at {at}: {e:?}", l.path))?;
        kassert!(n > 0, "{}: end of file at {at} of {}", l.path, l.size);
        h.update(&buf[..n]);
        at += n as u64;
    }
    let got = h.finish();
    kassert!(got == l.sha, "{}: SHA-256 {got}, the list says {}", l.path, l.sha);
    Ok(())
}

/// A shell command that must end with 0; its stdout. Its scratch folder is `d`.
pub(super) fn sh(cmd: &str, secs: u64) -> Result<String, String> {
    let s = start_as("/bin/sh", &["sh", "-c", cmd], None, false, None)?;
    let code = finish(&s, secs * SEC)?;
    kassert!(code == 0, "{cmd}: exit {code}\n{}", s.both());
    Ok(text(&s.out))
}

/// `cmd` with a new scratch folder as $D (removed again, also after a failure).
pub(super) fn in_scratch(what: &str, cmd: &str, secs: u64) -> Result<String, String> {
    let d = scratch(what);
    let r = sh(&format!("D={d}; mkdir -p $D/home && HOME=$D/home && export HOME && {cmd}"), secs);
    let _ = sh(&format!("rm -rf {d}"), 60);
    r
}

/// Every line of `tree` against the file on the stick, hashed when `hash(line)` says so; the failure names the stick.
pub(super) fn tree_ok(tree: &[Line], hash: impl Fn(&Line) -> bool, name: &str) -> Result<(u64, u64), String> {
    let id = read("/boot/BITSOS2/STICK.ID").trim().to_ascii_lowercase();
    let (mut hashed, mut sized) = (0u64, 0u64);
    for e in tree {
        let h = hash(e);
        if let Err(err) = file_ok(e, h) {
            return Err(if id == TEST_STICK_ID { err } else { format!("{err} (copy \\BITSOS2\\{name}\\ to this stick again)") });
        }
        if h { hashed += e.size } else { sized += e.size }
    }
    Ok((hashed, sized))
}

fn mesa() -> TestResult {
    let l = list()?;
    let own = tree_list(MESA)?;
    let (tree, launcher) = program_tree(&l, &own, MESA, LAUNCHER)?;
    let has = |rel: &str| tree.iter().any(|e| e.path.strip_prefix(MESA) == Some(rel));
    for rel in MESA_FILES {
        kassert!(has(rel), "{MESA}{rel} is not in the image's list (stage again with user/ports/mesa/scripts/stage-stick.sh)");
    }
    for pkg in MESA_LICENCES {
        let p = format!("{MESA}LICENSES/{pkg}/");
        kassert!(tree.iter().any(|e| e.path.starts_with(&p)), "no licence text below {p}");
    }
    let (hashed, sized) = tree_ok(&tree, |e| e.size <= HASH_IN_MAIN, "mesa")?;

    // The stick's other programs on this loader: bitscomp's gpu build and Xwayland name it in their PT_INTERP, near the
    // start of the file.
    let all = entries(&l);
    for prog in ON_THIS_LOADER {
        kassert!(all.iter().any(|e| e.path == *prog), "{prog} is not in the image's list");
        let f = vfs::root_ns().open(prog).map_err(|e| format!("{prog}: {e:?} (it runs on this tree's loader)"))?;
        let mut head = [0u8; 4096];
        let n = f.read_at(0, &mut head).map_err(|e| format!("read {prog}: {e:?}"))?;
        kassert!(head[..n].windows(LOADER.len()).any(|w| w == LOADER.as_bytes()), "{prog} does not name {LOADER} as its loader");
    }

    let out = sh(&format!("exec sh {launcher} --check"), RUN_SECS)?;
    kassert!(out.lines().any(|x| x.starts_with("mesa: libraries: ") && x.contains(" resolve inside ")), "mesa-run --check:\n{out}");
    kassert!(out.contains("mesa: driver radeon_icd.json: libvulkan_radeon.so") && out.contains("mesa: driver lvp_icd.json: libvulkan_lvp.so"), "mesa-run --check:\n{out}");

    // The glibc stage really starts: the Vulkan loader, lavapipe and the tree's libraries on the CPU. RADV may answer
    // too on a machine with a node; only lavapipe is asked for here (vk.sh is the run on the chip).
    let out = in_scratch("mesa-vk", &format!("MESA_VK_DRIVERS=lvp_icd exec sh {launcher} vulkaninfo --summary"), RUN_SECS)?;
    kassert!(out.contains("GPU0:") && out.contains("llvmpipe") && out.contains("DRIVER_ID_MESA_LLVMPIPE"), "vulkaninfo --summary on lavapipe:\n{out}");
    klog::event!(Info, "ul", "pkg.mesa", files = tree.len(), hashed_bytes = hashed, sized_bytes = sized);
    Ok(())
}
ktest!("ul.pkg.mesa", Program, mesa);

/// The bigger files of the Mesa tree.
fn mesa_sha() -> TestResult {
    let l = list()?;
    let own = tree_list(MESA)?;
    let (tree, _) = program_tree(&l, &own, MESA, LAUNCHER)?;
    let big: Vec<Line> = tree.into_iter().filter(|e| e.size > HASH_IN_MAIN).collect();
    kassert!(big.iter().any(|e| e.path.ends_with("/lib/libvulkan_radeon.so")), "libvulkan_radeon.so is not among the big files");
    let t0 = time::now_ns();
    let (hashed, _) = tree_ok(&big, |_| true, "mesa")?;
    klog::event!(Info, "ul", "pkg.mesa_sha", files = big.len(), hashed_bytes = hashed, read_ms = (time::now_ns() - t0) / 1_000_000);
    Ok(())
}
ktest!("ul.pkg.mesa_sha", Program, mesa_sha);

fn deqp() -> TestResult {
    let l = list()?;
    let mesa_own = tree_list(MESA)?;
    let (_, launcher) = program_tree(&l, &mesa_own, MESA, LAUNCHER)?;
    let own = tree_list(DEQP)?;
    let (tree, _) = program_tree(&l, &own, DEQP, LAUNCHER)?;
    let has = |rel: &str| tree.iter().any(|e| e.path.strip_prefix(DEQP) == Some(rel));
    for rel in DEQP_FILES {
        kassert!(has(rel), "{DEQP}{rel} is not in the image's list (stage again with user/ports/mesa/scripts/stage-stick.sh)");
    }
    let t0 = time::now_ns();
    let (hashed, _) = tree_ok(&tree, |_| true, "deqp")?;
    let read_ms = (time::now_ns() - t0) / 1_000_000;
    let subset = read(&format!("{DEQP}subset.txt"));
    kassert!(subset.lines().any(|x| x == "dEQP-VK.api.smoke.triangle"), "{DEQP}subset.txt has no draw:\n{subset}");
    // The runner on this tree's glibc: it reads its data from the stick, writes its log to the scratch folder.
    let out = in_scratch("deqp", &format!("cd $D && MESA_VK_DRIVERS=lvp_icd exec sh {launcher} {DEQP}deqp-vk --deqp-archive-dir={DEQP} --deqp-case=dEQP-VK.info.build --deqp-log-filename=$D/t.qpa"), RUN_SECS)?;
    kassert!(out.contains("Test case 'dEQP-VK.info.build'") && out.contains("Passed:        1/1"), "deqp-vk on lavapipe:\n{out}");
    klog::event!(Info, "ul", "pkg.deqp", files = tree.len(), hashed_bytes = hashed, read_ms = read_ms);
    Ok(())
}
ktest!("ul.pkg.deqp", Program, deqp);
