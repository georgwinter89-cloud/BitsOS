//! YT-2: `\BITSOS2\cobalt\` (Cobalt for YouTube TV, ~155 MB) and its launcher `\BITSOS2\bin\cobalt` as the
//! package list names them.
//! - `ul.pkg.cobalt` (Safe, rerunnable): only reads the stick and starts `--check`, which writes nothing.
//! - `ul.pkg.cobalt_profile` (QemuOnly: it writes the stick): the launcher's sign-in round trip on V2's vfat
//!   with a stub Cobalt, into a scratch home `\BITSOS2\YTKHOME\` that it removes again.

use super::pkg::{Line, Sha256, TEST_STICK_ID, absent, entries, list, on_stick};
use super::{SEC, finish, read, scratch, start, start_as, text, wait_for};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use ktest::{TestResult, kassert, ktest};

const TREE: &str = "/boot/BITSOS2/cobalt/";
const LOADER: &str = "/boot/BITSOS2/cobalt/loader_app";
const LIBS: &str = "/boot/BITSOS2/cobalt/lib/";
/// Read size for the SHA-256 of the big files (libcobalt.so is 124 MB).
const PIECE: usize = 1 << 20;

/// A regular file of the size and SHA-256 of `l`, read in pieces.
fn file_ok(l: &Line) -> TestResult {
    let ns = vfs::root_ns();
    let a = ns.lookup(l.path).map_err(|e| format!("{}: {e:?}", l.path))?.attr();
    kassert!(a.mode & uapi::S_IFMT == uapi::S_IFREG, "{}: mode {:o}", l.path, a.mode);
    kassert!(a.size == l.size, "{}: {} bytes, the list says {}", l.path, a.size, l.size);
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

/// Every file of the tree; then the loader resolves loader_app's libraries when the kernel starts it
/// (`LD_TRACE_LOADED_OBJECTS=1`, as glibc's ldd: the RPATH `$ORIGIN/lib` needs /proc/self/exe), the tree's own
/// libraries from `cobalt/lib`, the rest from /usr/lib; then `cobalt --check` as uid 1000 and as root.
fn cobalt() -> TestResult {
    let l = list()?;
    let all = entries(&l);
    let tree: Vec<&Line> = all.iter().filter(|e| e.kind == "stick" && e.path.starts_with(TREE)).collect();
    if tree.is_empty() {
        return absent(&l, "no \\BITSOS2\\cobalt\\ in this image's list");
    }
    let Some(launcher) = on_stick("cobalt")? else {
        return ktest::skip("no cobalt in \\BITSOS2\\bin of this stick (xtask stick does not copy the package yet), or an optional image");
    };
    // YT-5 CR (INT-W7): the licence texts are part of the package (LICENSES/, made by stage-v2.sh).
    for name in ["README.txt", "libcobalt.txt", "loader_app.txt", "fonts.txt", "ffmpeg.txt"] {
        let p = format!("{TREE}LICENSES/{name}");
        kassert!(tree.iter().any(|e| e.path == p), "{p} is not in the image's list (stage again with scripts/stage-v2.sh)");
    }
    let id = read("/boot/BITSOS2/STICK.ID").trim().to_ascii_lowercase();
    let t0 = time::now_ns();
    let mut bytes = 0;
    for e in &tree {
        if let Err(err) = file_ok(e) {
            return Err(if id == TEST_STICK_ID { err } else { format!("{err} (copy \\BITSOS2\\cobalt\\ to this stick again)") });
        }
        bytes += e.size;
    }
    let read_ms = (time::now_ns() - t0) / 1_000_000;
    // The check itself: a file whose list line has another SHA-256 (or size) is reported.
    if let Some(small) = tree.iter().min_by_key(|e| e.size) {
        let wrong: String = small.sha.chars().rev().collect();
        let bad = Line { sha: &wrong, ..**small };
        kassert!(wrong != small.sha && file_ok(&bad).is_err_and(|e| e.contains("SHA-256")), "a wrong SHA-256 for {} was not reported", small.path);
        let bad = Line { size: small.size + 1, ..**small };
        kassert!(file_ok(&bad).is_err_and(|e| e.contains("bytes")), "a wrong size for {} was not reported", small.path);
    }

    let s = start("/bin/sh", &["sh", "-c", &format!("LD_TRACE_LOADED_OBJECTS=1 exec {LOADER}")], None, false)?;
    let code = finish(&s, 60 * SEC)?;
    let out = text(&s.out);
    kassert!(code == 0 && !out.contains("not found"), "LD_TRACE_LOADED_OBJECTS: exit {code}\n{}", s.both());
    let found: Vec<(&str, &str)> = out.lines().filter_map(|x| x.trim().split_once(" => ")).map(|(so, r)| (so, r.split(" (").next().unwrap_or(""))).collect();
    for (so, path) in &found {
        kassert!(path.starts_with("/usr/lib/") || path.starts_with(LIBS), "{so} from {path}, neither /usr/lib nor {LIBS}:\n{out}");
    }
    // FFmpeg's libraries (desk, 21.10.) are dlopened by Cobalt's media code, so ldd's walk never names them.
    let dlopened = |so: &str| ["libavcodec.", "libavformat.", "libavutil.", "libswresample."].iter().any(|p| so.starts_with(p));
    for e in tree.iter().filter(|e| e.path.starts_with(LIBS) && !dlopened(&e.path[LIBS.len()..])) {
        let so = &e.path[LIBS.len()..];
        kassert!(found.iter().any(|(n, p)| n == &so && *p == e.path), "{so} not taken from {LIBS}:\n{out}");
    }

    let boot_rw = read("/proc/self/mounts").lines().any(|m| {
        let f: Vec<&str> = m.split(' ').collect();
        f.len() > 3 && f[1] == "/boot" && (f[3] == "rw" || f[3].starts_with("rw,"))
    });
    let want_libs = format!("cobalt: libraries: {} found, none missing", found.len());
    for uid in [1000u32, 0] {
        // Through sh: the kernel's own spawn takes ELF files only (the shell's execve runs `#!` scripts).
        let s = start_as("/bin/sh", &["sh", &launcher, "--check"], None, false, Some(uid))?;
        let code = finish(&s, 120 * SEC)?;
        let out = text(&s.out);
        let has = |p: &str| out.lines().any(|x| x.starts_with(p));
        kassert!(code == 0, "cobalt --check as uid {uid}: exit {code}\n{}", s.both());
        kassert!(out.lines().any(|x| x == "cobalt: tree /boot/BITSOS2/cobalt: ok") && out.lines().any(|x| x == want_libs), "uid {uid}, want '{want_libs}':\n{out}");
        kassert!(has("cobalt: screen: ") && has("cobalt: sound: ") && has("cobalt: user: runs as uid "), "uid {uid}:\n{out}");
        if uid == 0 && boot_rw {
            kassert!(has("cobalt: profile: /boot/BITSOS2/home/cobalt_files.tar (packed back"), "root on a writable stick:\n{out}");
        } else {
            kassert!(has("cobalt: profile: "), "uid {uid}:\n{out}");
        }
    }
    // YT-5 CR: `cobalt --stats` (as uid 1000, the user MCP starts it as) names the numbers file, or says that
    // none exists yet. Both are fine here: on the Mini-PC a session may have run before this test.
    let s = start_as("/bin/sh", &["sh", &launcher, "--stats"], None, false, Some(1000))?;
    let code = finish(&s, 30 * SEC)?;
    let out = text(&s.out);
    let none = code == 1 && out.lines().any(|x| x.starts_with("cobalt: no numbers in "));
    let some = code == 0 && out.lines().any(|x| x.starts_with("bitsos-av 1"));
    kassert!(none || some, "cobalt --stats: exit {code}, want 1 with 'cobalt: no numbers in ' or 0 with 'bitsos-av 1':\n{}", s.both());
    klog::event!(Info, "ul", "pkg.cobalt", files = tree.len(), bytes = bytes, read_ms = read_ms, libraries = found.len(), boot_rw = boot_rw);
    Ok(())
}
ktest!("ul.pkg.cobalt", Safe, cobalt);

/// The scratch home on the stick (under `\BITSOS2\`, writable for root; never the real `HOME`).
const KHOME: &str = "/boot/BITSOS2/YTKHOME";
/// The stub Cobalt: counts its starts in a cookie file and makes a name FAT refuses (the tar carries it).
const STUB: &str = "#!/bin/sh\nd=$HOME/.cobalt_files/x\nmkdir -p \"$d\"\nn=$(cat \"$d/Cookies\" 2>/dev/null || echo 0)\necho $((n + 1)) > \"$d/Cookies\"\necho y > \"$d/a:b\"\nexit 0\n";

/// One launcher start as root with the stub tree in `d`; returns its output.
fn launch(launcher: &str, d: &str) -> Result<String, String> {
    let cmd = format!("COBALT_TREE={d}/tree COBALT_HOME={KHOME} COBALT_TMP={d}/tmp COBALT_USER_RUN={d}/user COBALT_ALSA_SYS=/nonexistent COBALT_MOUNTS={d}/mounts exec sh {launcher}");
    let s = start_as("/bin/sh", &["sh", "-c", &cmd], None, false, Some(0))?;
    let code = finish(&s, 120 * SEC)?;
    kassert!(code == 0, "cobalt (stub) exit {code}\n{}", s.both());
    Ok(text(&s.out))
}

/// The sign-in round trip on the real vfat: saved as one tar file by .new + renames, taken back after RAM is
/// wiped, the copy before (.old) taken when the copy is gone (an interrupted swap).
fn cobalt_profile() -> TestResult {
    let Some(launcher) = on_stick("cobalt")? else {
        return ktest::skip("no cobalt in \\BITSOS2\\bin of this stick, or an optional image");
    };
    // ktest boots mount no /proc and the stick turns writable only once its identity is decided: wait for a
    // write to succeed, then hand the launcher a mount table that says so (its /proc/mounts rule is host-tested).
    let ns = vfs::root_ns();
    if !wait_for(30 * SEC, || ns.mkdir_p(KHOME).is_ok()) {
        return ktest::skip("the stick does not take writes below \\BITSOS2\\ (identity not confirmed)");
    }
    let d = scratch("cobalt");
    ns.mkdir_p(&format!("{d}/tree/app/cobalt/lib")).map_err(|e| format!("mkdir {d}: {e:?}"))?;
    let prep = format!(
        "set -e; cd {d}/tree; mkdir -p app/cobalt/content/ssl/certs; : > app/cobalt/lib/libcobalt.so; : > app/cobalt/content/cobalt_shell.pak; printf '{}' > loader_app; chmod 755 loader_app; echo 'stick /boot vfat rw 0 0' > {d}/mounts; rm -rf {KHOME}",
        STUB.replace('\\', "\\\\").replace('\n', "\\n").replace('%', "%%")
    );
    root_sh(&prep)?;
    let r = (|| -> TestResult {
        let tar = format!("{KHOME}/cobalt_files.tar");
        let ram = format!("{d}/user/cobalt/home/.cobalt_files/x/Cookies");
        let has = |out: &str, l: &str| out.lines().any(|x| x == l);
        let saved = |out: &str| out.lines().any(|x| x.starts_with("cobalt: profile saved on the stick"));
        let gone = |p: &str| ns.lookup(p).is_err();

        let out = launch(&launcher, &d)?;
        kassert!(has(&out, "cobalt: profile: new (sign in once)") && saved(&out), "first start:\n{out}");
        kassert!(ns.lookup(&tar).map(|i| i.attr().size).unwrap_or(0) > 0 && gone(&format!("{tar}.new")) && gone(&format!("{tar}.old")), "first save: {tar}");
        let out = launch(&launcher, &d)?;
        kassert!(has(&out, "cobalt: profile: taken from the stick (cobalt_files.tar)") && saved(&out), "second start:\n{out}");
        kassert!(!gone(&format!("{tar}.old")), "second save keeps the copy before as {tar}.old");

        let wipe = format!("rm -rf {d}/user/cobalt/home");
        root_sh(&wipe)?;
        let out = launch(&launcher, &d)?;
        kassert!(has(&out, "cobalt: profile: taken from the stick (cobalt_files.tar)"), "after wiping RAM:\n{out}");
        kassert!(read(&ram).trim() == "3", "cookie after three starts: '{}'", read(&ram).trim());

        root_sh(&format!("rm -f {tar}; {wipe}"))?;
        let out = launch(&launcher, &d)?;
        kassert!(has(&out, "cobalt: profile: taken from the stick (cobalt_files.tar.old)") && saved(&out), "interrupted swap:\n{out}");
        kassert!(read(&ram).trim() == "3", "cookie from the copy before (2) plus one: '{}'", read(&ram).trim());
        klog::event!(Info, "ul", "pkg.cobalt_profile", tar_bytes = ns.lookup(&tar).map(|i| i.attr().size).unwrap_or(0));
        Ok(())
    })();
    let cleaned = root_sh(&format!("rm -rf {KHOME} {d}"));
    r.and(cleaned)
}

/// A shell line as root that must succeed.
fn root_sh(cmd: &str) -> TestResult {
    let s = start_as("/bin/sh", &["sh", "-c", cmd], None, false, Some(0))?;
    let code = finish(&s, 30 * SEC)?;
    kassert!(code == 0, "sh -c '{cmd}': exit {code}\n{}", s.both());
    Ok(())
}
ktest!("ul.pkg.cobalt_profile", QemuOnly, cobalt_profile);
