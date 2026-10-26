//! UL-5: the package of the image (`user/packaging/`, `host/xtask/src/package.rs`): the glibc library set in
//! /usr/lib behind CachyOS's merged-/usr links, every listed file with its SHA-256, a dynamic C++ program running
//! on it, a missing library refused by the loader as on CachyOS, and a CachyOS program run from the stick's
//! `\BITSOS2\bin`. The image lists what it carries in `/usr/share/bitsos/package.txt`. xtask fails a build that
//! lacks part of the package unless V2_PKG_OPTIONAL=1, which the list states (`mode optional`); only then a test
//! skips for a missing part, so the gate never passes on an image without the set.
//! All Safe and rerunnable: scratch files carry the run counter and are removed.

use super::{SEC, finish, scratch, start, text, wait_for};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use ktest::{TestResult, kassert, ktest};

const LIST: &str = "/usr/share/bitsos/package.txt";
/// `host/xtask/src/image.rs` TEST_STICK_ID: the stick of every QEMU image, which always carries the stick programs.
pub(super) const TEST_STICK_ID: &str = "00112233445566778899aabbccddeeff";
/// What `pkg-cxx` prints on CachyOS (`package::tests::pkg_cxx_on_the_host` checks the same file there).
const CXX_OUT: &str = include_str!("../../../../user/packaging/tests/pkg-cxx.out");
/// UL-6: what /bin/pkg-locale (user/abi-tests/dyn/locale.c) prints on CachyOS.
const LOCALE_OUT: &str = include_str!("../../../../user/packaging/tests/pkg-locale.out");
/// The largest file a list line may name (libstdc++ is 3.3 MB, busybox 1 MB).
const MAX_FILE: usize = 64 << 20;

/// One list line: kind, path, size, SHA-256.
#[derive(Clone, Copy)]
pub(super) struct Line<'a> {
    pub kind: &'a str,
    pub path: &'a str,
    pub size: u64,
    pub sha: &'a str,
}

pub(super) fn entries(list: &str) -> Vec<Line<'_>> {
    list.lines()
        .filter(|l| !l.starts_with('#'))
        .filter_map(|l| {
            let mut f = l.split_whitespace();
            Some(Line { kind: f.next()?, path: f.next()?, size: f.next()?.parse().ok()?, sha: f.next()? })
        })
        .collect()
}

pub(super) fn list() -> Result<String, String> {
    let b = vfs::root_ns().read_file(LIST, 1 << 20).map_err(|e| format!("{LIST}: {e:?} (an image from xtask before UL-5?)"))?;
    Ok(String::from_utf8_lossy(&b).into_owned())
}

/// The image was built with V2_PKG_OPTIONAL=1: a missing part of the package may skip a test.
fn optional(list: &str) -> bool {
    list.lines().any(|l| l.starts_with("mode optional"))
}

/// A part of the package the list does not name: skip on an optional image, a failure otherwise.
pub(super) fn absent(list: &str, what: &str) -> TestResult {
    if optional(list) {
        return ktest::skip(&format!("{what} (image built with V2_PKG_OPTIONAL=1)"));
    }
    Err(format!("{what}, and the image was not built with V2_PKG_OPTIONAL=1"))
}

/// The file at `path` (links followed) is a regular file with permissions `perm`, the size and SHA-256 of
/// line `l`; returns (dev, ino).
fn file_ok(path: &str, l: &Line, perm: u32) -> Result<(u64, u64), String> {
    let ns = vfs::root_ns();
    let a = ns.lookup(path).map_err(|e| format!("{path}: {e:?}"))?.attr();
    kassert!(a.mode & uapi::S_IFMT == uapi::S_IFREG && a.mode & 0o777 == perm, "{path}: mode {:o}, want {perm:o}", a.mode);
    kassert!(a.size == l.size, "{path}: {} bytes, the list says {}", a.size, l.size);
    let data = ns.read_file(path, MAX_FILE).map_err(|e| format!("read {path}: {e:?}"))?;
    let h = sha256(&data);
    kassert!(h == l.sha, "{path}: SHA-256 {h}, the list says {}", l.sha);
    Ok((a.dev, a.ino))
}

/// Every file of the list (except the stick's, see `stick`) with its mode, size and SHA-256; the loader and libc
/// also by the names merged /usr gives them (/lib, /lib64 and /usr/lib64 lead to /usr/lib, as on CachyOS).
fn set() -> TestResult {
    let l = list()?;
    if l.lines().any(|x| x.starts_with("libset none")) {
        return absent(&l, "no glibc library set in this image");
    }
    let all = entries(&l);
    let (mut libs, mut bytes) = (0, 0);
    for e in all.iter().filter(|e| e.kind != "stick") {
        let perm = match e.kind {
            "lib" | "bin" => 0o755,
            "data" | "licence" => 0o644,
            k => return Err(format!("{LIST}: unknown kind {k} ({})", e.path)),
        };
        let id = file_ok(e.path, e, perm)?;
        bytes += e.size;
        if e.kind != "lib" {
            continue;
        }
        libs += 1;
        let others: &[&str] = match e.path {
            "/usr/lib/ld-linux-x86-64.so.2" => &["/lib64/ld-linux-x86-64.so.2", "/lib/ld-linux-x86-64.so.2", "/usr/lib64/ld-linux-x86-64.so.2"],
            "/usr/lib/libc.so.6" => &["/lib/libc.so.6", "/lib64/libc.so.6", "/usr/lib64/libc.so.6"],
            _ => &[],
        };
        for o in others {
            let a = vfs::root_ns().lookup(o).map_err(|e| format!("{o}: {e:?}"))?.attr();
            kassert!((a.dev, a.ino) == id, "{o} is not the same file as {}", e.path);
        }
    }
    for want in ["/usr/lib/ld-linux-x86-64.so.2", "/usr/lib/libc.so.6", "/usr/lib/libstdc++.so.6", "/usr/lib/libgcc_s.so.1"] {
        kassert!(all.iter().any(|e| e.kind == "lib" && e.path == want), "{want} not in {LIST}");
    }
    kassert!(all.iter().any(|e| e.kind == "licence"), "no licence text in {LIST}");
    klog::event!(Info, "ul", "pkg.set", libraries = libs, files = all.len(), bytes = bytes);
    Ok(())
}
ktest!("ul.pkg.set", Safe, set);

/// `/bin/pkg-cxx` (g++ against CachyOS's glibc and libstdc++) prints what it prints on CachyOS, and the loader
/// finds its libraries in /usr/lib by itself (`ld.so --list`, no LD_LIBRARY_PATH, no ld.so.cache).
fn cxx() -> TestResult {
    let l = list()?;
    let all = entries(&l);
    let Some(e) = all.iter().find(|e| e.kind == "bin" && e.path == "/bin/pkg-cxx") else {
        return absent(&l, "no /bin/pkg-cxx in this image");
    };
    let path = e.path;
    file_ok(path, e, 0o755)?;
    let s = start(path, &["pkg-cxx"], None, false)?;
    let code = finish(&s, 60 * SEC)?;
    kassert!(code == 0 && text(&s.out) == CXX_OUT, "exit {code}; got:\n{}", s.both());
    let s = start("/lib64/ld-linux-x86-64.so.2", &["ld.so", "--list", path], None, false)?;
    let code = finish(&s, 60 * SEC)?;
    let out = text(&s.out);
    kassert!(code == 0, "ld.so --list: exit {code}\n{}", s.both());
    for so in ["libstdc++.so.6", "libm.so.6", "libgcc_s.so.1", "libc.so.6"] {
        kassert!(out.lines().any(|x| x.trim_start().starts_with(&format!("{so} => /usr/lib/{so} ("))), "{so} not found in /usr/lib:\n{out}");
    }
    klog::event!(Info, "ul", "pkg.cxx", bytes = out.len());
    Ok(())
}
ktest!("ul.pkg.cxx", Safe, cxx);

/// UL-6: glibc finds C.utf8, de_DE.utf8 and en_US.utf8 in /usr/lib/locale (no locale-archive): /bin/pkg-locale
/// prints what it prints on CachyOS (setlocale names, nl_langinfo, localeconv, strftime %A in German, grouping,
/// wide characters, collation, newlocale/uselocale). Rerunnable: it only reads.
fn locale() -> TestResult {
    let l = list()?;
    let all = entries(&l);
    let Some(e) = all.iter().find(|e| e.kind == "bin" && e.path == "/bin/pkg-locale") else {
        return absent(&l, "no /bin/pkg-locale in this image");
    };
    for want in ["/usr/lib/locale/de_DE.utf8/LC_CTYPE", "/usr/lib/locale/en_US.utf8/LC_COLLATE", "/usr/lib/locale/C.utf8/LC_CTYPE"] {
        if !all.iter().any(|e| e.kind == "data" && e.path == want) {
            return absent(&l, &format!("{want} not in {LIST}"));
        }
    }
    file_ok(e.path, e, 0o755)?;
    let s = start(e.path, &["pkg-locale"], None, false)?;
    let code = finish(&s, 60 * SEC)?;
    let out = text(&s.out);
    if code != 0 || out != LOCALE_OUT {
        let diff = out.lines().zip(LOCALE_OUT.lines()).position(|(a, b)| a != b).unwrap_or(out.lines().count().min(LOCALE_OUT.lines().count()));
        return Err(format!("exit {code}; first difference at line {}: got '{}', want '{}'\n{}", diff + 1, out.lines().nth(diff).unwrap_or("<end>"), LOCALE_OUT.lines().nth(diff).unwrap_or("<end>"), s.both()));
    }
    // UL-6 fixer: the console's path, LANG=de_DE.UTF-8 and setlocale(LC_ALL, ""), gives the same de_DE answers.
    let s = start(e.path, &["pkg-locale", "lang"], None, false)?;
    let code = finish(&s, 60 * SEC)?;
    let got = text(&s.out);
    let mut want = String::from("lang: setlocale(LC_ALL, \"\") = de_DE.UTF-8\n");
    for x in LOCALE_OUT.lines().filter(|x| x.starts_with("de_DE.UTF-8: ")) {
        want.push_str(x);
        want.push('\n');
    }
    kassert!(code == 0 && got == want, "pkg-locale lang: exit {code}, want:\n{want}got:\n{}", s.both());
    klog::event!(Info, "ul", "pkg.locale", bytes = out.len() + got.len());
    Ok(())
}
ktest!("ul.pkg.locale", Safe, locale);

/// The stick's program `name`: Ok(None) = skip (not on this stick, which is not the test stick, or not in an
/// optional image); a test stick without it, or a required image whose list lacks it, fails.
pub(super) fn on_stick(name: &str) -> Result<Option<String>, String> {
    let l = list()?;
    let want = format!("/boot/BITSOS2/bin/{name}");
    let all = entries(&l);
    let Some(e) = all.iter().find(|e| e.kind == "stick" && e.path == want) else {
        return if optional(&l) { Ok(None) } else { Err(format!("{want} not in {LIST}, and the image was not built with V2_PKG_OPTIONAL=1")) };
    };
    let ns = vfs::root_ns();
    if !wait_for(30 * SEC, || ns.lookup("/boot/BITSOS2/STICK.ID").is_ok()) {
        return Ok(None);
    }
    let id = String::from_utf8_lossy(&ns.read_file("/boot/BITSOS2/STICK.ID", 128).unwrap_or_default()).trim().to_ascii_lowercase();
    match file_ok(e.path, e, 0o755) {
        Ok(_) => Ok(Some(String::from(e.path))),
        Err(err) if id == TEST_STICK_ID => Err(format!("the test stick lacks its program: {err}")),
        Err(err) => {
            klog::event!(Info, "ul", "pkg.stick_absent", why = err.as_str());
            Ok(None)
        }
    }
}

/// A dynamic program whose library is missing: the loader refuses it as on CachyOS (stderr with argv[0], exit
/// 127, nothing on stdout). The program is pkg-cxx with DT_NEEDED libstdc++.so.6 renamed to libstdc++.so.9 in
/// its string table (same length, so nothing else moves); the message is CachyOS's for the same patched file.
fn missing_lib() -> TestResult {
    let l = list()?;
    if !entries(&l).iter().any(|e| e.kind == "bin" && e.path == "/bin/pkg-cxx") {
        return absent(&l, "no /bin/pkg-cxx in this image");
    }
    let ns = vfs::root_ns();
    let mut b = ns.read_file("/bin/pkg-cxx", MAX_FILE).map_err(|e| format!("/bin/pkg-cxx: {e:?}"))?;
    let needle = b"libstdc++.so.6\0";
    let at: Vec<usize> = b.windows(needle.len()).enumerate().filter(|(_, w)| *w == needle).map(|(i, _)| i).collect();
    kassert!(at.len() == 1, "libstdc++.so.6 found {} times in /bin/pkg-cxx", at.len());
    b[at[0] + needle.len() - 2] = b'9';
    let dir = scratch("pkgmiss");
    let r = (|| -> TestResult {
        let f = format!("{dir}/pkg-missing");
        ns.create_file(&f, 0o755, b).map_err(|e| format!("{f}: {e:?}"))?;
        let s = start(&f, &["pkg-missing"], None, false)?;
        let code = finish(&s, 60 * SEC)?;
        let (out, err) = (text(&s.out), text(&s.err));
        let want = "pkg-missing: error while loading shared libraries: libstdc++.so.9: cannot open shared object file: No such file or directory\n";
        kassert!(code == 127 && out.is_empty() && err == want, "exit {code}, stdout '{out}', stderr '{err}'");
        Ok(())
    })();
    let _ = ns.remove_tree(&dir);
    r
}
ktest!("ul.pkg.missing_lib", Safe, missing_lib);

/// CachyOS's `ninja` (C++: libstdc++, libgcc_s, libm, libc) run from `/boot/BITSOS2/bin`: its version, a build
/// file with nothing to do, a tool, and a missing build file (strerror text, exit 1), as on CachyOS.
fn stick() -> TestResult {
    let Some(ninja) = on_stick("ninja")? else {
        return ktest::skip("no ninja in \\BITSOS2\\bin of this stick (xtask stick does not copy it yet; MCP write_file does), or an optional image");
    };
    let dir = scratch("pkg");
    let ns = vfs::root_ns();
    let r = (|| -> TestResult {
        ns.mkdir_p(&dir).map_err(|e| format!("mkdir {dir}: {e:?}"))?;
        let bf = format!("{dir}/build.ninja");
        ns.create_file(&bf, 0o644, b"rule cp\n  command = cp $in $out\nbuild all: phony\ndefault all\n".to_vec()).map_err(|e| format!("{bf}: {e:?}"))?;
        let run = |argv: &[&str]| -> Result<(i32, String, String), String> {
            let s = start(&ninja, argv, None, false)?;
            let code = finish(&s, 60 * SEC)?;
            Ok((code, text(&s.out), text(&s.err)))
        };
        let (code, out, err) = run(&["ninja", "--version"])?;
        let v = out.trim_end();
        kassert!(code == 0 && !v.is_empty() && v.chars().all(|c| c.is_ascii_digit() || c == '.'), "--version: exit {code} '{out}' {err}");
        let (code, out, err) = run(&["ninja", "-f", &bf])?;
        kassert!(code == 0 && out == "ninja: no work to do.\n", "build: exit {code} '{out}' {err}");
        let (code, out, err) = run(&["ninja", "-f", &bf, "-t", "targets", "all"])?;
        kassert!(code == 0 && out == "all: phony\n", "-t targets: exit {code} '{out}' {err}");
        let missing = format!("{dir}/none.ninja");
        let (code, out, err) = run(&["ninja", "-f", &missing])?;
        let want = format!("ninja: error: loading '{missing}': No such file or directory\n");
        kassert!(code == 1 && out.is_empty() && err == want, "missing file: exit {code} '{out}' '{err}'");
        klog::event!(Info, "ul", "pkg.stick", version = v);
        Ok(())
    })();
    let _ = ns.remove_tree(&dir);
    r
}
ktest!("ul.pkg.stick", Safe, stick);

const K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3, 0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

/// SHA-256 (FIPS 180-4) for the list check, the same digest as xtask's `package::sha256`, fed in pieces (YT-2
/// reads a 124 MB library 1 MiB at a time).
pub(super) struct Sha256 {
    h: [u32; 8],
    tail: Vec<u8>,
    len: u64,
}

impl Sha256 {
    pub(super) fn new() -> Sha256 {
        Sha256 { h: [0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19], tail: Vec::new(), len: 0 }
    }

    fn block(&mut self, b: &[u8]) {
        let mut w = [0u32; 64];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([b[4 * i], b[4 * i + 1], b[4 * i + 2], b[4 * i + 3]]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16].wrapping_add(s0).wrapping_add(w[i - 7]).wrapping_add(s1);
        }
        let mut v = self.h;
        for i in 0..64 {
            let s1 = v[4].rotate_right(6) ^ v[4].rotate_right(11) ^ v[4].rotate_right(25);
            let ch = (v[4] & v[5]) ^ (!v[4] & v[6]);
            let t1 = v[7].wrapping_add(s1).wrapping_add(ch).wrapping_add(K[i]).wrapping_add(w[i]);
            let s0 = v[0].rotate_right(2) ^ v[0].rotate_right(13) ^ v[0].rotate_right(22);
            let t2 = s0.wrapping_add((v[0] & v[1]) ^ (v[0] & v[2]) ^ (v[1] & v[2]));
            v = [t1.wrapping_add(t2), v[0], v[1], v[2], v[3].wrapping_add(t1), v[4], v[5], v[6]];
        }
        for (a, b) in self.h.iter_mut().zip(v) {
            *a = a.wrapping_add(b);
        }
    }

    /// Whole blocks straight from `data` (no copy of a 3 MB library); a partial block waits in `tail`.
    pub(super) fn update(&mut self, mut data: &[u8]) {
        self.len = self.len.wrapping_add(data.len() as u64);
        if !self.tail.is_empty() {
            let take = (64 - self.tail.len()).min(data.len());
            self.tail.extend_from_slice(&data[..take]);
            data = &data[take..];
            if self.tail.len() < 64 {
                return;
            }
            let t = core::mem::take(&mut self.tail);
            self.block(&t);
        }
        let full = data.len() / 64 * 64;
        for b in data[..full].chunks_exact(64) {
            self.block(b);
        }
        self.tail.extend_from_slice(&data[full..]);
    }

    pub(super) fn finish(mut self) -> String {
        let mut tail = core::mem::take(&mut self.tail);
        tail.push(0x80);
        while tail.len() % 64 != 56 {
            tail.push(0);
        }
        tail.extend_from_slice(&self.len.wrapping_mul(8).to_be_bytes());
        for b in tail.chunks_exact(64) {
            self.block(b);
        }
        self.h.iter().map(|x| format!("{x:08x}")).collect()
    }
}

fn sha256(data: &[u8]) -> String {
    let mut s = Sha256::new();
    s.update(data);
    s.finish()
}

/// The FIPS 180-4 examples, in the kernel (the list check depends on it).
fn sha() -> TestResult {
    kassert!(sha256(b"") == "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855");
    kassert!(sha256(b"abc") == "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
    let two = b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq";
    kassert!(sha256(two) == "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1");
    // YT-2: the same digest fed in pieces that straddle the 64-byte blocks.
    let mut s = Sha256::new();
    for piece in [&two[..3], &two[3..50], &two[50..], b""] {
        s.update(piece);
    }
    kassert!(s.finish() == "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1", "sha256 in pieces");
    Ok(())
}
ktest!("ul.pkg.sha256", Safe, sha);
