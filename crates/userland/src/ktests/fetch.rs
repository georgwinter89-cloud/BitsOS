//! UL-4a: `/bin/fetch` (rustls HTTPS downloader, `user/tools/fetch`) on V2.
//! - `ul.fetch.local`: usage, help and a refused loopback connect; no partner needed.
//! - `ul.fetch.bundle`: the system CA bundle in the image parses (connect then fails as refused, not as TLS
//!   setup); skips while the image has no bundle (change request UL-4a CR1 to the desk's xtask).
//! - `ul.fetch.cases`: every line of `user/tools/fetch/tests/cases.txt` (the table CachyOS answers in
//!   `host/tests/ul/30-fetch-linux.sh`) against `host/tests/ul/fetch_server.py`, which
//!   `host/tests/ul/31-fetch-qemu.sh` starts on the host (seen as 10.0.2.2:28540 HTTPS / 28541 HTTP); skips at
//!   once when that partner is missing. The test CAs and the blob's hash come from the partner over plain HTTP.
//! Rerunnable: each run has its own scratch dir, removed at the end; fetch runs as root only to write there.

use super::{SEC, finish, need, read, scratch, start_as, text};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use ktest::{TestResult, kassert, ktest};

const CASES: &str = include_str!("../../../../user/tools/fetch/tests/cases.txt");
const HOST: &str = "10.0.2.2";
/// Nothing listens here on V2's loopback (lane UL ports 28500-28999).
const LO_REFUSED: &str = "127.0.0.1:28545";

struct Ran {
    code: i32,
    out: String,
    err: String,
}

fn fetch(args: &[&str], timeout_s: u64) -> Result<Ran, String> {
    let mut argv = Vec::with_capacity(args.len() + 1);
    argv.push("fetch");
    argv.extend_from_slice(args);
    let s = start_as("/bin/fetch", &argv, None, false, Some(0))?;
    let code = finish(&s, timeout_s * SEC)?;
    Ok(Ran { code, out: text(&s.out), err: text(&s.err) })
}

fn exists(p: &str) -> bool {
    vfs::root_ns().lookup(p).is_ok()
}

fn scratch_dir(what: &str) -> Result<String, String> {
    let d = scratch(what);
    vfs::root_ns().mkdir_p(&d).map_err(|e| format!("mkdir {d}: {e:?}"))?;
    Ok(d)
}

fn local() -> TestResult {
    need("/bin/fetch")?;
    let d = scratch_dir("fetch-local")?;
    let r = (|| -> TestResult {
        let h = fetch(&["--help"], 30)?;
        kassert!(h.code == 0 && h.out.contains("exit codes"), "fetch --help: exit {}\n{}{}", h.code, h.out, h.err);
        for args in [&[][..], &["--bogus", "http://x/"], &["http://x/", "--sha256", "12"], &["ftp://x/"]] {
            let r = fetch(args, 30)?;
            kassert!(r.code == 2, "fetch {args:?}: exit {}, want 2 (usage)\n{}", r.code, r.err);
        }
        let out = format!("{d}/x");
        let url = format!("http://{LO_REFUSED}/x");
        let r = fetch(&["-q", "--timeout", "10", "-o", &out, &url], 60)?;
        kassert!(r.code == 4, "fetch {url}: exit {}, want 4 (connect refused)\n{}", r.code, r.err);
        kassert!(!exists(&out) && !exists(&format!("{out}.part")), "a failed fetch left {out} or its .part behind");
        Ok(())
    })();
    let _ = vfs::root_ns().remove_tree(&d);
    r
}
ktest!("ul.fetch.local", Safe, local);

fn bundle() -> TestResult {
    need("/bin/fetch")?;
    let path = "/etc/ssl/certs/ca-certificates.crt";
    if !exists(path) {
        return ktest::skip("no /etc/ssl/certs/ca-certificates.crt in this image (change request UL-4a CR1: xtask copies CachyOS's bundle)");
    }
    let d = scratch_dir("fetch-bundle")?;
    let out = format!("{d}/x");
    let url = format!("https://{LO_REFUSED}/x");
    let r = fetch(&["-q", "--timeout", "10", "-o", &out, &url], 60);
    let _ = vfs::root_ns().remove_tree(&d);
    let r = r?;
    kassert!(r.code == 4, "fetch {url} with the system bundle: exit {}, want 4 (bundle loads, then refused)\n{}", r.code, r.err);
    Ok(())
}
ktest!("ul.fetch.bundle", Safe, bundle);

/// Gets the partner's CA files and the blob's hash over plain HTTP. `Ok(None)`: no partner (skip). Waits while
/// the network is still coming up ("unreachable"), at most 30 s.
fn bootstrap(d: &str) -> Result<Option<String>, String> {
    let end = time::now_ns() + 30 * SEC;
    loop {
        let r = fetch(&["-q", "--timeout", "5", "-o", &format!("{d}/ca.pem"), &format!("http://{HOST}:28541/ca.pem")], 60)?;
        match r.code {
            0 => break,
            4 | 11 if r.err.contains("nreachable") && time::now_ns() < end => sched::sleep_ns(SEC),
            4 | 11 => return Ok(None),
            c => return Err(format!("fetching the test CA from the partner: exit {c}\n{}", r.err)),
        }
    }
    for f in ["ca2.pem", "blob.sha256"] {
        let r = fetch(&["-q", "--timeout", "30", "-o", &format!("{d}/{f}"), &format!("http://{HOST}:28541/{f}")], 90)?;
        if r.code != 0 {
            return Err(format!("fetching {f} from the partner: exit {}\n{}", r.code, r.err));
        }
    }
    let sha: String = read(&format!("{d}/blob.sha256")).chars().take(64).collect();
    if sha.len() != 64 {
        return Err(format!("blob.sha256 from the partner is {sha:?}"));
    }
    Ok(Some(sha))
}

fn one_case(d: &str, sha: &str, name: &str, want: i32, rest: &str) -> Result<(), String> {
    let cd = format!("{d}/case-{name}");
    vfs::root_ns().mkdir_p(&cd).map_err(|e| format!("mkdir {cd}: {e:?}"))?;
    let line = rest
        .replace("@SHA", sha)
        .replace("@CA2", &format!("{d}/ca2.pem"))
        .replace("@CA", &format!("{d}/ca.pem"))
        .replace("@S", &format!("https://{HOST}:28540"))
        .replace("@P", &format!("http://{HOST}:28541"))
        .replace("@R", &format!("https://{HOST}:28549"))
        .replace("@D", &cd)
        .replace("@H", HOST);
    let mut args: Vec<&str> = line.split_whitespace().collect();
    let explicit = args.contains(&"-o");
    let out = format!("{cd}/{name}");
    if !explicit {
        args.extend_from_slice(&["-o", &out]);
    }
    let r = fetch(&args, 120)?;
    klog::event!(Info, "ul", "fetch.case", case = name, code = r.code, want = want);
    let left = if explicit {
        ""
    } else if want == 0 {
        if exists(&out) { "" } else { " (no output file)" }
    } else if exists(&out) || exists(&format!("{out}.part")) || exists(&format!("{out}.part.1")) {
        " (left a file behind)"
    } else {
        ""
    };
    if r.code != want || !left.is_empty() {
        return Err(format!("{name}: exit {}, want {want}{left}\n  fetch {}\n  {}", r.code, args.join(" "), r.err.trim_end().replace('\n', "\n  ")));
    }
    Ok(())
}

/// Two runs to the same -o at once (the NW-7 64-download case in small): each stages in its own O_EXCL file
/// (FILE.part, FILE.part.1), both succeed, and only FILE is left.
fn parallel(d: &str, sha: &str) -> Result<(), String> {
    let (out, ca, url) = (format!("{d}/par"), format!("{d}/ca.pem"), format!("https://{HOST}:28540/blob"));
    let argv = ["fetch", "-q", "--ca", &ca, "--sha256", sha, "-o", &out, &url];
    let a = start_as("/bin/fetch", &argv, None, false, Some(0))?;
    let b = start_as("/bin/fetch", &argv, None, false, Some(0));
    let ra = finish(&a, 120 * SEC);
    let b = b?;
    let rb = finish(&b, 120 * SEC);
    let (ra, rb) = (ra?, rb?);
    let left = exists(&format!("{out}.part")) || exists(&format!("{out}.part.1"));
    if ra != 0 || rb != 0 || !exists(&out) || left {
        return Err(format!("parallel: exits {ra} and {rb}, want 0 and 0; {out} there: {}; a .part left: {left}\n  {}\n  {}", exists(&out), text(&a.err), text(&b.err)));
    }
    Ok(())
}

fn cases() -> TestResult {
    need("/bin/fetch")?;
    let d = scratch_dir("fetch-cases")?;
    let r = (|| -> TestResult {
        let Some(sha) = bootstrap(&d)? else {
            return ktest::skip("no fetch partner at 10.0.2.2:28541 (host/tests/ul/31-fetch-qemu.sh starts host/tests/ul/fetch_server.py)");
        };
        let mut bad = Vec::new();
        let mut n = 0;
        for l in CASES.lines().filter(|l| !l.starts_with('#') && !l.trim().is_empty()) {
            let mut it = l.splitn(3, ' ');
            let (name, want, rest) = (it.next().unwrap_or(""), it.next().unwrap_or(""), it.next().unwrap_or(""));
            let Ok(want) = want.parse::<i32>() else {
                bad.push(format!("cases.txt: bad line {l:?}"));
                continue;
            };
            n += 1;
            if let Err(e) = one_case(&d, &sha, name, want, rest) {
                bad.push(e);
            }
        }
        if let Err(e) = parallel(&d, &sha) {
            bad.push(e);
        }
        kassert!(bad.is_empty(), "{} of {n} fetch cases (+ the parallel one) differ from CachyOS:\n{}", bad.len(), bad.join("\n"));
        Ok(())
    })();
    let _ = vfs::root_ns().remove_tree(&d);
    r
}
ktest!("ul.fetch.cases", Safe, cases);
