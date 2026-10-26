//! BR-2: `\BITSOS2\brave\` (CachyOS's Brave, ~500 MB with its libraries) and its launcher `\BITSOS2\bin\brave` as
//! the package list names them. Both Safe and rerunnable: they only read the stick and start `--check`, which
//! writes nothing.
//! - `ul.pkg.brave`: every file's size, the SHA-256 of all but the big ones, the libraries as V2's loader resolves
//!   them for brave and for what Chromium dlopens, the fontconfig cache against fontconfig's FAT rule, `brave --check`
//!   as uid 1000 and 0.
//! - `ul.pkg.brave_fonts`: fontconfig itself (as Brave loads it, through /bin/fc-probe) takes the tree's prebuilt
//!   cache: it lists the fonts and writes no cache of its own (as uid 1000, so a rescan could only write into a
//!   scratch HOME, never onto the stick).
//! - `ul.pkg.brave_fonts_mtime` (QemuOnly: it sets the fonts dir's mtime on the stick): the same with an mtime that
//!   has nanoseconds, as a cached dir inode reports it on the stick after a push.
//! - `ul.pkg.brave_sha`: the SHA-256 of the big files too (the brave binary alone is 328 MB: seconds in QEMU, longer
//!   from a USB stick, hence its own test).

use super::pkg::{Line, Sha256, TEST_STICK_ID, absent, entries, list, on_stick};
use super::{SEC, finish, need, read, scratch, start, start_as, text};
use alloc::collections::BTreeSet;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use ktest::{TestResult, kassert, ktest};

const TREE: &str = "/boot/BITSOS2/brave/";
const BRAVE: &str = "/boot/BITSOS2/brave/brave";
const LIBS: &str = "/boot/BITSOS2/brave/lib/";
const FONTS: &str = "/boot/BITSOS2/brave/fonts";
const LDSO: &str = "/usr/lib/ld-linux-x86-64.so.2";
/// The libraries Chromium (ui/gtk) and NSS dlopen; stage-v2.sh puts them into lib/, xtask adds what they link.
const DLOPENED: &[&str] = &["libgtk-3.so.0", "libsoftokn3.so", "libfreeblpriv3.so"];
/// Files up to this size are hashed in `ul.pkg.brave`, bigger ones in `ul.pkg.brave_sha` (a test cost, not a limit:
/// the tree's 8 bigger files hold 5/6 of its bytes).
const HASH_IN_MAIN: u64 = 8 << 20;
const PIECE: usize = 1 << 20;

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

/// The tree's list lines, or Ok(None) for a skip (an optional image without the tree, or no launcher on this
/// stick); the launcher's path with them.
fn tree_lines(l: &str) -> Result<Option<(Vec<Line<'_>>, String)>, String> {
    let tree: Vec<Line> = entries(l).into_iter().filter(|e| e.kind == "stick" && e.path.starts_with(TREE)).collect();
    if tree.is_empty() {
        absent(l, "no \\BITSOS2\\brave\\ in this image's list (xtask puts it into ktest images whose filter runs ul.pkg.brave)")?;
        return Ok(None);
    }
    let Some(launcher) = on_stick("brave")? else {
        return Ok(None);
    };
    Ok(Some((tree, launcher)))
}

/// Every file against the list (the big ones without SHA-256 here), the licences, the libraries, the fontconfig cache,
/// then `brave --check` as uid 1000 and as root.
fn brave() -> TestResult {
    let l = list()?;
    let Some((tree, launcher)) = tree_lines(&l)? else {
        return ktest::skip("no brave in \\BITSOS2\\ of this stick, or an optional image");
    };
    for name in ["README.txt", "brave.txt"] {
        let p = format!("{TREE}LICENSES/{name}");
        kassert!(tree.iter().any(|e| e.path == p), "{p} is not in the image's list (stage again with scripts/stage-v2.sh)");
    }
    for pkg in ["gtk3", "nss", "noto-fonts", "noto-fonts-emoji", "ttf-dejavu", "fontconfig"] {
        let p = format!("{TREE}LICENSES/{pkg}/");
        kassert!(tree.iter().any(|e| e.path.starts_with(&p)), "no licence text below {p}");
    }
    let id = read("/boot/BITSOS2/STICK.ID").trim().to_ascii_lowercase();
    let t0 = time::now_ns();
    let (mut hashed, mut sized) = (0u64, 0u64);
    for e in &tree {
        let hash = e.size <= HASH_IN_MAIN;
        if let Err(err) = file_ok(e, hash) {
            return Err(if id == TEST_STICK_ID { err } else { format!("{err} (copy \\BITSOS2\\brave\\ to this stick again)") });
        }
        if hash { hashed += e.size } else { sized += e.size }
    }
    let read_ms = (time::now_ns() - t0) / 1_000_000;
    if let Some(small) = tree.iter().min_by_key(|e| e.size) {
        let wrong: String = small.sha.chars().rev().collect();
        let bad = Line { sha: &wrong, ..*small };
        kassert!(wrong != small.sha && file_ok(&bad, true).is_err_and(|e| e.contains("SHA-256")), "a wrong SHA-256 for {} was not reported", small.path);
        let bad = Line { size: small.size + 1, ..*small };
        kassert!(file_ok(&bad, false).is_err_and(|e| e.contains("bytes")), "a wrong size for {} was not reported", small.path);
    }

    // The loader as the kernel starts brave (LD_LIBRARY_PATH: Brave has no RPATH), and as dlopen finds GTK 3 and
    // NSS's soft token in lib/ (ld.so --list, which is what ldd runs).
    let mut found: BTreeSet<(String, String)> = BTreeSet::new();
    let runs = [format!("LD_LIBRARY_PATH={LIBS} LD_TRACE_LOADED_OBJECTS=1 exec {BRAVE}")]
        .into_iter()
        .chain(DLOPENED.iter().map(|so| format!("exec {LDSO} --library-path {LIBS} --list {LIBS}{so}")));
    for cmd in runs {
        let s = start("/bin/sh", &["sh", "-c", &cmd], None, false)?;
        let code = finish(&s, 60 * SEC)?;
        let out = text(&s.out);
        kassert!(code == 0 && !out.contains("not found"), "{cmd}: exit {code}\n{}", s.both());
        for (so, r) in out.lines().filter_map(|x| x.trim().split_once(" => ")) {
            let path = r.split(" (").next().unwrap_or("");
            kassert!(path.starts_with("/usr/lib/") || path.starts_with(LIBS), "{cmd}: {so} from {path}, neither /usr/lib nor {LIBS}:\n{out}");
            found.insert((String::from(so), String::from(path)));
        }
    }
    for e in tree.iter().filter(|e| e.path.starts_with(LIBS)) {
        let so = &e.path[LIBS.len()..];
        kassert!(DLOPENED.contains(&so) || found.iter().any(|(n, p)| n == so && *p == e.path), "{so} is in {LIBS} but nothing loads it from there");
    }
    for so in DLOPENED {
        kassert!(tree.iter().any(|e| e.path == format!("{LIBS}{so}")), "{LIBS}{so} is not in the image's list");
    }

    fontcache(&tree)?;

    // The launcher counts the same: distinct "name => path" lines over brave and the dlopened libraries.
    let want_libs = format!("brave: libraries: {} found, none missing", found.len());
    for uid in [1000u32, 0] {
        let s = start_as("/bin/sh", &["sh", &launcher, "--check"], None, false, Some(uid))?;
        let code = finish(&s, 120 * SEC)?;
        let out = text(&s.out);
        let has = |p: &str| out.lines().any(|x| x.starts_with(p));
        kassert!(code == 0, "brave --check as uid {uid}: exit {code}\n{}", s.both());
        kassert!(out.lines().any(|x| x == "brave: tree /boot/BITSOS2/brave: ok") && out.lines().any(|x| x == want_libs), "uid {uid}, want '{want_libs}':\n{out}");
        kassert!(has("brave: fonts: ") && has("brave: desktop: ") && has("brave: profile: ") && has("brave: sound: ") && has("brave: running: "), "uid {uid}:\n{out}");
        let user = if uid == 0 { "brave: user: runs as uid 1000 (started by root: chpst -u 1000:1000)" } else { "brave: user: runs as uid 1000" };
        kassert!(out.lines().any(|x| x == user), "uid {uid}, want '{user}':\n{out}");
    }
    klog::event!(Info, "ul", "pkg.brave", files = tree.len(), hashed_bytes = hashed, sized_bytes = sized, read_ms = read_ms, libraries = found.len());
    Ok(())
}
ktest!("ul.pkg.brave", Safe, brave);

/// fontconfig's cache for the tree's fonts: valid on V2 only if its `checksum` (FcCache, int at offset 48) is what
/// fontconfig computes on FAT: an Adler-32 over the directory's entry names (each with its NUL) and d_types, walked
/// from the end of the strcmp-sorted list (fontconfig fcstat.c FcDirChecksum, used when FcIsFsMtimeBroken: statfs
/// f_type 0x4d44, which V2's vfat answers, crates/fs-fat/src/glue.rs), and its `checksum_nano` (offset 56) equals the
/// directory's mtime nanoseconds (0 on FAT). The names come from getdents64 (BusyBox ls), as fontconfig reads them.
fn fontcache(tree: &[Line]) -> TestResult {
    let caches: Vec<&Line> = tree.iter().filter(|e| e.path.starts_with(&format!("{TREE}fontcache/")) && e.path.contains(".cache-")).collect();
    kassert!(!caches.is_empty(), "no fontconfig cache below {TREE}fontcache/ in the list");
    let s = start("/bin/sh", &["sh", "-c", &format!("LC_ALL=C exec ls -1a {FONTS}")], None, false)?;
    let code = finish(&s, 30 * SEC)?;
    kassert!(code == 0, "ls {FONTS}: exit {code}\n{}", s.both());
    let out = text(&s.out);
    let mut names: Vec<&str> = out.lines().filter(|n| *n != "." && *n != "..").collect();
    names.sort_unstable();
    for n in &names {
        kassert!(tree.iter().any(|e| e.path == format!("{FONTS}/{n}")), "{FONTS}/{n} is not a font of the list (a directory or a stray file changes the checksum)");
    }
    let (mut a, mut b) = (1u32, 0u32);
    for n in names.iter().rev() {
        for c in n.bytes().chain([0u8]).chain(8i32.to_le_bytes()) {
            a = (a + c as u32) % 65521;
            b = (b + a) % 65521;
        }
    }
    let want = a.wrapping_add(b << 16);
    // The dir's own mtime nanoseconds do not take part: fontconfig 2.18 writes checksum_nano 0 and keeps the cache
    // whatever the dir's nanoseconds are (BR-4a: on the stick a cached dir inode reports 707123246 ns after a push,
    // and ul.pkg.brave_fonts_mtime gives the dir such an mtime in QEMU).
    let nsec = vfs::root_ns().lookup(FONTS).map_err(|e| format!("{FONTS}: {e:?}"))?.attr().mtime.nsec;
    klog::event!(Info, "ul", "pkg.brave_fontcache", fonts = names.len(), dir_nsec = nsec);
    let ns = vfs::root_ns();
    for c in caches {
        let head = ns.read_file(c.path, c.size as usize + 1).map_err(|e| format!("{}: {e:?}", c.path))?;
        kassert!(head.len() >= 64, "{}: {} bytes", c.path, head.len());
        let u32_at = |o: usize| u32::from_le_bytes([head[o], head[o + 1], head[o + 2], head[o + 3]]);
        let nano = i64::from_le_bytes(head[56..64].try_into().unwrap_or([0xff; 8]));
        kassert!(u32_at(0) == 0xFC02_FC04, "{}: magic {:#x}", c.path, u32_at(0));
        kassert!(u32_at(48) == want, "{}: checksum {:#x}, fontconfig computes {want:#x} for {FONTS} on FAT ({} fonts)", c.path, u32_at(48), names.len());
        kassert!(nano == 0, "{}: checksum_nano {nano}, fontconfig writes 0 (stage-v2.sh sets it)", c.path);
        let dir = format!("{FONTS}\0");
        kassert!(head.windows(dir.len()).any(|w| w == dir.as_bytes()), "{}: does not name {FONTS}", c.path);
    }
    Ok(())
}

/// The big files' SHA-256 (the others are hashed by `ul.pkg.brave`).
fn brave_sha() -> TestResult {
    let l = list()?;
    let Some((tree, _)) = tree_lines(&l)? else {
        return ktest::skip("no brave in \\BITSOS2\\ of this stick, or an optional image");
    };
    let t0 = time::now_ns();
    let mut bytes = 0;
    for e in tree.iter().filter(|e| e.size > HASH_IN_MAIN) {
        file_ok(e, true)?;
        bytes += e.size;
    }
    klog::event!(Info, "ul", "pkg.brave_sha", bytes = bytes, read_ms = (time::now_ns() - t0) / 1_000_000);
    Ok(())
}
ktest!("ul.pkg.brave_sha", Safe, brave_sha);

/// fontconfig with Brave's FONTCONFIG_FILE, as uid 1000 with a fresh XDG_CACHE_HOME: the fonts are there and no cache
/// was written, so the prebuilt one was valid (an invalid one makes fontconfig rescan and write its own).
fn brave_fonts() -> TestResult {
    let l = list()?;
    let Some((tree, _)) = tree_lines(&l)? else {
        return ktest::skip("no brave in \\BITSOS2\\ of this stick, or an optional image");
    };
    fc_probe(&tree)
}
ktest!("ul.pkg.brave_fonts", Safe, brave_fonts);

/// The stick state the desk saw on the Mini-PC (BR-4a): after a push the fonts dir's cached inode reports an mtime
/// with nanoseconds. A file made and removed in the dir gives it one here (QemuOnly: it writes the stick), then fontconfig
/// must still take the prebuilt cache.
fn brave_fonts_mtime() -> TestResult {
    let l = list()?;
    let Some((tree, _)) = tree_lines(&l)? else {
        return ktest::skip("no brave in \\BITSOS2\\ of this stick, or an optional image");
    };
    // An entry made and removed again in the cached dir sets its mtime from the clock (vfs touch_mtime), as the push
    // did; `touch` alone gives 0 ns on vfat. The names stay the same, so the FAT checksum does too.
    let p = format!("{FONTS}/br4a-mtime-probe");
    // The stick turns writable only once its identity is decided (see ul.pkg.cobalt_profile): retry for 30 s, then
    // exit 3 (a skip: no writable stick, nothing to reproduce).
    let cmd = format!("i=0; until (: > {p}) 2>/dev/null; do i=$((i + 1)); [ $i -ge 30 ] && exit 3; sleep 1; done; exec rm {p}");
    let r = (|| -> TestResult {
        let s = start_as("/bin/sh", &["sh", "-c", &cmd], None, false, Some(0))?;
        let code = finish(&s, 60 * SEC)?;
        if code == 3 {
            return ktest::skip("the stick did not turn writable within 30 s");
        }
        kassert!(code == 0, "create and remove {p}: exit {code}\n{}", s.both());
        let nsec = vfs::root_ns().lookup(FONTS).map_err(|e| format!("{FONTS}: {e:?}"))?.attr().mtime.nsec;
        klog::event!(Info, "ul", "pkg.brave_fonts_mtime", dir_nsec = nsec);
        // The precondition: without nanoseconds this is ul.pkg.brave_fonts again (1 in 10^9 by chance; always once
        // vfs or fs-fat round directory times to FAT granularity as Linux vfat does, and then the stick state this
        // reproduces no longer exists either).
        if nsec == 0 {
            return ktest::skip("the fonts dir mtime is FAT-granular (0 ns) after a create and remove: nothing to reproduce");
        }
        fc_probe(&tree)
    })();
    // Always, also after a failure or a timeout kill: a probe entry left in fonts/ changes the FAT checksum and makes
    // the prebuilt cache invalid for every later test of this boot.
    let s = start_as("/bin/sh", &["sh", "-c", &format!("[ -e {p} ] || exit 0; exec rm -f {p}")], None, false, Some(0))?;
    let cleaned = finish(&s, 30 * SEC).and_then(|c| if c == 0 { Ok(()) } else { Err(format!("rm -f {p}: exit {c}\n{}", s.both())) });
    r.and(cleaned)
}
ktest!("ul.pkg.brave_fonts_mtime", QemuOnly, brave_fonts_mtime);

fn fc_probe(tree: &[Line]) -> TestResult {
    need("/bin/fc-probe")?;
    let d = scratch("brave-fc");
    let s = start_as("/bin/sh", &["sh", "-c", &format!("mkdir -p {d}/cache && chown -R 1000:1000 {d}")], None, false, Some(0))?;
    kassert!(finish(&s, 30 * SEC)? == 0, "mkdir {d}: {}", s.both());
    let cmd = format!("HOME={d} XDG_CACHE_HOME={d}/cache FONTCONFIG_FILE={TREE}etc/fonts/fonts.conf FC_DEBUG=16 exec /bin/fc-probe");
    let r = (|| -> TestResult {
        let s = start_as("/bin/sh", &["sh", "-c", &cmd], None, false, Some(1000))?;
        let code = finish(&s, 120 * SEC)?;
        let out = text(&s.out);
        let fonts = tree.iter().filter(|e| e.path.starts_with(&format!("{FONTS}/"))).count();
        let n: usize = out.lines().find_map(|x| x.strip_prefix("fc-probe: fonts ")).and_then(|n| n.trim().parse().ok()).unwrap_or(0);
        kassert!(code == 0 && n >= fonts, "fc-probe: exit {code}, {n} fonts, the tree has {fonts} font files:\n{}", s.both());
        let written = vfs::root_ns().list_dir(&format!("{d}/cache")).map(|v| v.len()).unwrap_or(0);
        kassert!(written == 0, "fontconfig wrote {written} entries into {d}/cache: the prebuilt cache in {TREE}fontcache was not valid\n{}", s.both());
        klog::event!(Info, "ul", "pkg.brave_fonts", fonts = n, files = fonts);
        Ok(())
    })();
    let s = start_as("/bin/sh", &["sh", "-c", &format!("rm -rf {d}")], None, false, Some(0))?;
    let cleaned = finish(&s, 30 * SEC);
    r.and(cleaned.map(|_| ()))
}
