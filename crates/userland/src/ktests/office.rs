//! LO-1: `\BITSOS2\libreoffice\` (CachyOS's LibreOffice, ~530 MB with its libraries) and its launcher
//! `\BITSOS2\bin\libreoffice` as the package list names them. Class Program: on the Mini-PC only (Georg 30.10.:
//! programs are tested on the Mini-PC, not in QEMU; QEMU images carry no program tree). Rerunnable: they read the
//! stick, start `--check` and `--env` (which write nothing), and the conversion works in a scratch folder of its own.
//! - `ul.pkg.libreoffice`: every file's size, the SHA-256 of all up to 1 MiB, the three files that are links into
//!   /etc on CachyOS are real files with the package's content, the licences, the libraries as V2's loader resolves
//!   them for soffice.bin and for every library of program/ (each file of lib/ is loaded by one of them), only the
//!   colibre icon themes, `libreoffice --check` as uid 1000 and 0, and the environment the launcher gives
//!   LibreOffice (`libreoffice --env`).
//! - `ul.pkg.libreoffice_sha`: the SHA-256 of the bigger files (90 files, ~370 MB).
//! - `ul.pkg.libreoffice_convert`: the launcher's headless `--convert-to pdf` of a small text as uid 1000, with a
//!   scratch HOME (LO_HOME): the new profile's restart (code 81) is handled by the launcher, the PDF is whole.

use super::pkg::{Line, Sha256, TEST_STICK_ID, list, program_tree, tree_list};
use super::{SEC, finish, read, scratch, start_as, text};
use alloc::collections::BTreeSet;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use ktest::{TestResult, kassert, ktest};

const TREE: &str = "/boot/BITSOS2/libreoffice/";
const LIBS: &str = "/boot/BITSOS2/libreoffice/lib/";
const PROGRAM: &str = "/boot/BITSOS2/libreoffice/program/";
const LDSO: &str = "/usr/lib/ld-linux-x86-64.so.2";
/// Files up to this size are hashed in `ul.pkg.libreoffice` (5100 of 5200 files, ~160 MB), bigger ones in
/// `ul.pkg.libreoffice_sha` (a test cost, not a limit).
const HASH_IN_MAIN: u64 = 1 << 20;
const PIECE: usize = 1 << 20;
/// Links into /etc/libreoffice on CachyOS: real copies of the package's files in the tree, with these lines.
const WERE_LINKS: &[(&str, &str)] = &[
    ("program/bootstraprc", "UserInstallation=$SYSUSERCONFIG/libreoffice/4"),
    ("program/sofficerc", "URE_BOOTSTRAP="),
    ("share/psprint/psprint.conf", "[__Global_Printer_Defaults__]"),
];

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

/// Whether `path` starts with the ELF magic.
fn is_elf(path: &str) -> bool {
    let mut head = [0u8; 4];
    vfs::root_ns().open(path).ok().and_then(|f| f.read_at(0, &mut head).ok()) == Some(4) && head == *b"\x7fELF"
}

/// The tree and its launcher (pkg::program_tree: missing is a failure, never a skip).
fn tree_lines<'a>(l: &str, own: &'a str) -> Result<(Vec<Line<'a>>, String), String> {
    program_tree(l, own, TREE, "libreoffice")
}

/// A shell command as uid `uid` (None: the spawn default) that must end with 0; its stdout.
fn sh(cmd: &str, uid: Option<u32>, secs: u64) -> Result<String, String> {
    let s = start_as("/bin/sh", &["sh", "-c", cmd], None, false, uid)?;
    let code = finish(&s, secs * SEC)?;
    kassert!(code == 0, "{cmd}: exit {code}\n{}", s.both());
    Ok(text(&s.out))
}

/// Every file against the list (the bigger ones without SHA-256 here), the former links, the licences, the
/// icon themes, the libraries, then `libreoffice --check` and `--env`.
fn libreoffice() -> TestResult {
    let l = list()?;
    let own = tree_list(TREE)?;
    let (tree, launcher) = tree_lines(&l, &own)?;
    let has = |rel: &str| tree.iter().any(|e| e.path.strip_prefix(TREE) == Some(rel));
    for rel in ["LICENSES/README.txt", "LICENSES/libreoffice.txt", "VERSION.txt", "program/soffice.bin", "share/registry/bitsos.xcd",
        "gtk/share/icons/hicolor/48x48/apps/libreoffice-writer.png", "gtk/share/icons/Adwaita/index.theme", "etc/fonts/fonts.conf"] {
        kassert!(has(rel), "{TREE}{rel} is not in the image's list (stage again with scripts/stage-v2.sh)");
    }
    for pkg in ["ttf-liberation", "ttf-dejavu", "noto-fonts", "adwaita-icon-theme", "shared-mime-info", "nss", "glycin"] {
        let p = format!("{TREE}LICENSES/{pkg}/");
        kassert!(tree.iter().any(|e| e.path.starts_with(&p)), "no licence text below {p}");
    }
    // LibreOffice's own icon themes: colibre (and its dark twin) only; the others and every SVG zip stay out.
    let themes: Vec<&str> = tree.iter().filter_map(|e| e.path.strip_prefix(TREE)?.strip_prefix("share/config/images_")).collect();
    kassert!(themes == ["colibre.zip", "colibre_dark.zip"], "icon themes {themes:?}, want colibre and colibre_dark");
    let svg = format!("{TREE}gtk/share/icons/");
    kassert!(!tree.iter().any(|e| e.path.starts_with(&svg) && e.path.ends_with(".svg")), "an SVG icon below {svg} (GTK has no SVG loader in the tree)");
    let id = read("/boot/BITSOS2/STICK.ID").trim().to_ascii_lowercase();
    let t0 = time::now_ns();
    let (mut hashed, mut sized) = (0u64, 0u64);
    for e in &tree {
        let hash = e.size <= HASH_IN_MAIN;
        if let Err(err) = file_ok(e, hash) {
            return Err(if id == TEST_STICK_ID { err } else { format!("{err} (copy \\BITSOS2\\libreoffice\\ to this stick again)") });
        }
        if hash { hashed += e.size } else { sized += e.size }
    }
    let read_ms = (time::now_ns() - t0) / 1_000_000;
    let ns = vfs::root_ns();
    for (rel, want) in WERE_LINKS {
        let p = format!("{TREE}{rel}");
        let body = ns.read_file(&p, 1 << 16).map_err(|e| format!("{p}: {e:?}"))?;
        kassert!(String::from_utf8_lossy(&body).contains(want), "{p} (a link to /etc/libreoffice on CachyOS) lacks '{want}'");
    }

    // The loader as the launcher runs soffice.bin (LD_LIBRARY_PATH=lib; program/ through RUNPATH $ORIGIN), and as
    // it loads every other ELF file of program/ (the libraries LibreOffice dlopens, its helper programs such as
    // xpdfimport) and GTK's image loader (program/ on the path too: Firebird's intl/ library needs a neighbour).
    let glycin = format!("{TREE}gtk/glycin/glycin-image-rs");
    let progs: Vec<&str> = tree.iter().map(|e| e.path).filter(|p| (p.starts_with(PROGRAM) && is_elf(p)) || *p == glycin).collect();
    kassert!(progs.len() > 150 && progs.iter().any(|p| p.ends_with("/xpdfimport")), "{} ELF files in {PROGRAM}", progs.len());
    let mut cmd = format!("exec 2>&1; LD_LIBRARY_PATH={LIBS} {LDSO} --list {PROGRAM}soffice.bin || echo LDSO-FAILED soffice.bin; for f in");
    for p in &progs {
        cmd.push(' ');
        cmd.push_str(p);
    }
    cmd.push_str(&format!("; do {LDSO} --library-path {LIBS}:{PROGRAM} --list $f || echo LDSO-FAILED $f; done | sed -n 's/^[[:space:]]*//; s/ (0x[0-9a-f]*)$//; /=>/p; /LDSO-FAILED/p' | sort -u"));
    let out = sh(&cmd, None, 300)?;
    let bad: Vec<&str> = out.lines().filter(|x| x.contains("not found") || x.starts_with("LDSO-FAILED")).collect();
    kassert!(bad.is_empty(), "the loader misses libraries: {bad:?}");
    let mut found: BTreeSet<(String, String)> = BTreeSet::new();
    for (so, path) in out.lines().filter_map(|x| x.split_once(" => ")) {
        kassert!(path.starts_with("/usr/lib/") || path.starts_with(TREE), "{so} from {path}, neither /usr/lib nor {TREE}");
        found.insert((String::from(so), String::from(path)));
    }
    for e in tree.iter().filter(|e| e.path.starts_with(LIBS)) {
        let so = &e.path[LIBS.len()..];
        kassert!(so == "libsoftokn3.so" || so == "libfreeblpriv3.so" || found.iter().any(|(_, p)| *p == e.path), "{so} is in {LIBS} but nothing loads it from there");
    }

    // The launcher: --check as uid 1000 and as root (it hands over to uid 1000).
    for uid in [1000u32, 0] {
        let s = start_as("/bin/sh", &["sh", &launcher, "--check"], None, false, Some(uid))?;
        let code = finish(&s, 120 * SEC)?;
        let out = text(&s.out);
        let has = |p: &str| out.lines().any(|x| x.starts_with(p));
        kassert!(code == 0, "libreoffice --check as uid {uid}: exit {code}\n{}", s.both());
        kassert!(has("libreoffice: tree /boot/BITSOS2/libreoffice: ok") && has("libreoffice: libraries: ") && out.contains(" found, none missing"), "uid {uid}:\n{out}");
        kassert!(has("libreoffice: desktop: ") && has("libreoffice: home: ") && has("libreoffice: profile: ") && has("libreoffice: language: LANG=") && has("libreoffice: running: "), "uid {uid}:\n{out}");
        let user = if uid == 0 { "libreoffice: user: runs as uid 1000 (started by root: chpst -u 1000:1000)" } else { "libreoffice: user: runs as uid 1000" };
        kassert!(out.lines().any(|x| x == user), "uid {uid}, want '{user}':\n{out}");
    }
    env_ok(&launcher)?;
    klog::event!(Info, "ul", "pkg.libreoffice", files = tree.len(), hashed_bytes = hashed, sized_bytes = sized, read_ms = read_ms, libraries = found.len());
    Ok(())
}
ktest!("ul.pkg.libreoffice", Program, libreoffice);

/// What `libreoffice --env` gives LibreOffice: the tree's libraries, fontconfig, GTK's data, the gtk3 plugin, a
/// German or English locale, no D-Bus, and HOME on the data volume when one is mounted read-write at /data.
fn env_ok(launcher: &str) -> TestResult {
    let out = sh(&format!("exec sh {launcher} --env"), Some(1000), 60)?;
    let get = |k: &str| out.lines().find_map(|l| l.strip_prefix(k).and_then(|r| r.strip_prefix('=')));
    for (k, want) in [
        ("LD_LIBRARY_PATH", "/boot/BITSOS2/libreoffice/lib"),
        ("SAL_USE_VCLPLUGIN", "gtk3"),
        ("SAL_DISABLE_OPENCL", "1"),
        ("GDK_BACKEND", "wayland"),
        ("FONTCONFIG_FILE", "/boot/BITSOS2/libreoffice/etc/fonts/fonts.conf"),
        ("XDG_DATA_DIRS", "/boot/BITSOS2/libreoffice/gtk/share"),
        ("GSETTINGS_SCHEMA_DIR", "/boot/BITSOS2/libreoffice/gtk/share/glib-2.0/schemas"),
        ("GSETTINGS_BACKEND", "memory"),
        ("GDK_PIXBUF_MODULE_FILE", "/boot/BITSOS2/libreoffice/gtk/etc/gdk-pixbuf/loaders.cache"),
        ("XCURSOR_PATH", "/boot/BITSOS2/libreoffice/gtk/share/icons"),
        ("DBUS_SESSION_BUS_ADDRESS", "disabled:"),
        ("CUPS_SERVER", "/run/cups/cups.sock"),
    ] {
        kassert!(get(k) == Some(want), "libreoffice --env: {k}={:?}, want {want}\n{out}", get(k));
    }
    let lang = get("LANG").unwrap_or("");
    kassert!(lang == "de_DE.UTF-8" || lang == "en_US.UTF-8", "LANG={lang}\n{out}");
    kassert!(get("PATH").is_some_and(|p| p.ends_with(":/boot/BITSOS2/libreoffice/gtk/libexec")), "PATH\n{out}");
    kassert!(get("DISPLAY").is_none() && get("LO_HOME").is_none(), "an X display or a test setting leaks into LibreOffice\n{out}");
    // The mount table as the launcher sees it: this test may run on a kernel thread (MCP's `ktest`), which has no
    // /proc/self (procfs `self_link`). Mini-PC 3.11.: the launcher saw /data, this test did not.
    let mounts = sh("exec cat /proc/self/mounts", Some(1000), 60)?;
    let lines: Vec<&str> = mounts.lines().filter(|l| l.split_whitespace().nth(1) == Some("/data")).collect();
    // The launcher's own rule (`mounted_rw`): the last /data line that is read-write says the file system.
    let fs = lines.iter().rev().find_map(|l| {
        let f: Vec<&str> = l.split_whitespace().collect();
        (f.len() > 3 && f[3].split(',').next() == Some("rw")).then(|| f[2])
    });
    let data = fs.is_some_and(|fs| fs != "vfat");
    let home = get("HOME").unwrap_or("");
    if data {
        kassert!(home == "/data/home/1000", "a data volume is mounted ({lines:?}), HOME={home}\n{out}");
    } else {
        kassert!(home.ends_with("/home") && home.contains("/libreoffice"), "no writable data volume ({lines:?}): HOME={home}, want the run folder's home in RAM\n{out}");
    }
    klog::event!(Info, "ul", "pkg.libreoffice_env", home = home, lang = lang, data_volume = data);
    Ok(())
}

/// The bigger files' SHA-256 (the others are hashed by `ul.pkg.libreoffice`).
fn libreoffice_sha() -> TestResult {
    let l = list()?;
    let own = tree_list(TREE)?;
    let (tree, _) = tree_lines(&l, &own)?;
    let t0 = time::now_ns();
    let mut bytes = 0;
    for e in tree.iter().filter(|e| e.size > HASH_IN_MAIN) {
        file_ok(e, true)?;
        bytes += e.size;
    }
    klog::event!(Info, "ul", "pkg.libreoffice_sha", bytes = bytes, read_ms = (time::now_ns() - t0) / 1_000_000);
    Ok(())
}
ktest!("ul.pkg.libreoffice_sha", Program, libreoffice_sha);

/// init's source: its /etc/passwd (`PASSWD`) is the one the convert test writes into a ktest boot.
const INIT_SRC: &str = include_str!("../../../../user/init/src/lib.rs");

/// init's /etc/passwd, read out of its source (one file to change, never two that can drift apart): LibreOffice asks
/// the C library for its user (getpwuid) and stops with "User installation could not be completed" without one.
fn init_passwd() -> Result<String, String> {
    const HEAD: &str = "pub const PASSWD: &str = \"";
    let at = INIT_SRC.find(HEAD).ok_or("user/init/src/lib.rs has no `pub const PASSWD: &str = \"...\"`")?;
    let rest = &INIT_SRC[at + HEAD.len()..];
    let lit = &rest[..rest.find("\";").ok_or("init's PASSWD literal has no end")?];
    kassert!(!lit.contains('\'') && !lit.replace("\\n", "").contains('\\'), "init's PASSWD uses escapes other than \\n: {lit}");
    let p = lit.replace("\\n", "\n");
    kassert!(p.lines().any(|l| l.starts_with("user:x:1000:1000:")), "init's PASSWD has no uid 1000: {p}");
    Ok(p)
}

/// LibreOffice itself, headless, by the launcher as uid 1000 in a scratch folder (its HOME and the launcher's run
/// folder too, so the user's own profile is never touched and nothing stays behind): a text becomes a PDF; the new
/// profile makes LibreOffice ask for a restart (81) first. A ktest boot has no init and so no /etc/passwd: the test
/// writes init's for its run when it is missing and takes it away again, whatever the result; where init runs (every
/// boot that is not a ktest boot, the Mini-PC's MCP run included) it is there and stays untouched.
fn libreoffice_convert() -> TestResult {
    let l = list()?;
    let own = tree_list(TREE)?;
    let (_, launcher) = tree_lines(&l, &own)?;
    let passwd = init_passwd()?;
    let d = scratch("lo-convert");
    let r = (|| -> TestResult {
        let made = format!("[ -e /etc/passwd ] || {{ printf '%s' '{passwd}' > /etc/passwd && : > {d}.passwd; }}");
        let s = start_as("/bin/sh", &["sh", "-c", &format!("mkdir -p {d}/home {d}/run && printf 'LO-1 ktest\\n' > {d}/brief.txt && chown -R 1000:1000 {d} && {made}")], None, false, Some(0))?;
        kassert!(finish(&s, 30 * SEC)? == 0, "mkdir {d}: {}", s.both());
        let t0 = time::now_ns();
        let cmd = format!("LO_HOME={d}/home LO_TMP={d}/run exec sh {launcher} --convert-to pdf --outdir {d} {d}/brief.txt");
        let s = start_as("/bin/sh", &["sh", "-c", &cmd], None, false, Some(1000))?;
        let code = finish(&s, 600 * SEC)?;
        let out = text(&s.out);
        kassert!(code == 0 && out.contains("LibreOffice ended (code 0)"), "{cmd}: exit {code}\n{}", s.both());
        kassert!(out.contains("asked for a restart (code 81"), "a new profile: LibreOffice asks for its restart and the launcher starts it again\n{out}");
        let pdf = vfs::root_ns().read_file(&format!("{d}/brief.pdf"), 16 << 20).map_err(|e| format!("{d}/brief.pdf: {e:?}\n{out}"))?;
        kassert!(pdf.starts_with(b"%PDF-") && pdf[pdf.len().saturating_sub(1024)..].windows(5).any(|w| w == b"%%EOF"), "{d}/brief.pdf: {} bytes, not a whole PDF", pdf.len());
        klog::event!(Info, "ul", "pkg.libreoffice_convert", pdf_bytes = pdf.len(), ms = (time::now_ns() - t0) / 1_000_000);
        Ok(())
    })();
    // Always, also after a failure: the passwd this test made, its marker and the scratch folder.
    let s = start_as("/bin/sh", &["sh", "-c", &format!("if [ -e {d}.passwd ]; then rm -f /etc/passwd {d}.passwd; fi; rm -rf {d}")], None, false, Some(0))?;
    let cleaned = finish(&s, 60 * SEC).and_then(|c| if c == 0 { Ok(()) } else { Err(format!("cleaning {d}: exit {c}\n{}", s.both())) });
    let left = vfs::root_ns().lookup(&format!("{d}.passwd")).is_ok() || vfs::root_ns().lookup(&d).is_ok();
    r.and(cleaned).and(if left { Err(format!("{d} or its passwd marker is still there")) } else { Ok(()) })
}
ktest!("ul.pkg.libreoffice_convert", Program, libreoffice_convert);
