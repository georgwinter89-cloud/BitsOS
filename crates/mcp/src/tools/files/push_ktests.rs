//! In-kernel tests of MC-push over the kernel loopback (a server of the test's own, the kernel's real tool list)
//! with the files served by a small HTTP server thread on 127.0.0.1, the way `host/mcp/push.mjs` works:
//! - `mc.push.tmpfs` (Safe, rerunnable, only /tmp): fetch into a staging tree (parents:true), `manifest` hashes
//!   equal to SHA-256 of the served bytes, `move_tree` over a package tree with a removal, the second run
//!   finishing nothing;
//! - `mc.push.stick` (QemuOnly: writes the stick image of this run, below `\BITSOS2\` only, and removes what it
//!   made): the same on the boot stick, and the stick rule's refusals (SYS, CONFIG.TXT, STICK.ID, `\EFI\`, the
//!   stick's root; fs-fat's own tests cover 8.3 aliases) with CONFIG.TXT and SYS unchanged, and owner and mode
//!   of what write_file, fetch and move_tree put below `\BITSOS2\home\` (uid 1000, 0700/0600) and of what
//!   move_tree takes out of it (root, 0755); the home itself never moved away.

use crate::sha256::{Sha256, hex};
use crate::testclient::{Client, with_server};
use crate::tools::install_ktests::Http;
use crate::{Json, TOOLS};
use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use ktest::{TestResult, kassert, ktest};

/// MCP and HTTP servers of these tests (the next ones of 16 when taken); MC range 25000-25499.
const PORT: u16 = 25330;
const HTTP_PORT: u16 = 25350;

fn sha(d: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(d);
    hex(&h.finish())
}

fn body(n: usize, seed: u32) -> Vec<u8> {
    (0..n as u32).map(|i| (i.wrapping_mul(2654435761) >> 13 ^ seed) as u8).collect()
}

fn refused(c: &mut Client, tool: &str, args: &str) -> TestResult {
    let r = c.call(tool, args)?;
    kassert!(r.error && r.text.starts_with("Refused: "), "{tool} {args}: want a refusal, got {:?} {}", r.rpc_error, r.text);
    Ok(())
}

/// (path, sha256) of every file of a `manifest` answer.
fn files(c: &mut Client, dir: &str) -> Result<Vec<(String, String)>, String> {
    let t = c.ok("manifest", &format!(r#"{{"path":"{dir}","timeout_ms":60000}}"#))?;
    let j = Json::parse(&t).map_err(|e| format!("manifest: no JSON ({e:?}): {t}"))?;
    kassert!(matches!(j.get("next"), Some(Json::Null)), "one page expected: {t}");
    let e = j.get("entries").and_then(Json::as_array).ok_or("no entries")?;
    Ok(e.iter()
        .filter(|x| x.get("type").and_then(Json::as_str) == Some("file"))
        .map(|x| (String::from(x.get("path").and_then(Json::as_str).unwrap_or("?")), String::from(x.get("sha256").and_then(Json::as_str).unwrap_or("?"))))
        .collect())
}

/// The push flow on `pkg` (an existing package with `keep`, `old` and `gone`) through `stage`.
fn push_flow(c: &mut Client, http: &Http, stage: &str, pkg: &str, big: &[u8], small: &[u8]) -> TestResult {
    for (name, rel) in [("big", "lib/big.bin"), ("small", "old")] {
        let t = c.ok("fetch", &format!(r#"{{"url":"{}","path":"{stage}/{rel}","parents":true,"timeout_ms":120000}}"#, http.url(&format!("/p/{name}"))))?;
        kassert!(t.contains("fetched "), "{t}");
    }
    let m = files(c, stage)?;
    kassert!(m == vec![(String::from("lib/big.bin"), sha(big)), (String::from("old"), sha(small))], "staged manifest {m:?}");
    let t = c.ok("move_tree", &format!(r#"{{"from":"{stage}","to":"{pkg}","remove":["gone","never"],"timeout_ms":120000}}"#))?;
    kassert!(t.contains("1 files and 1 directories moved (1 replaced), 1 removed (1 already gone)"), "{t}");
    let m = files(c, pkg)?;
    kassert!(
        m == vec![(String::from("keep"), sha(b"keep")), (String::from("lib/big.bin"), sha(big)), (String::from("old"), sha(small))],
        "package manifest {m:?}"
    );
    let t = c.ok("move_tree", &format!(r#"{{"from":"{stage}","to":"{pkg}","remove":["gone"]}}"#))?;
    kassert!(t.contains("nothing left to move") && t.contains("(1 already gone)"), "{t}");
    Ok(())
}

fn push_tmpfs() -> TestResult {
    let (stage, pkg) = ("/tmp/mcpush-k/stage", "/tmp/mcpush-k/pkg");
    let ns = vfs::root_ns();
    let _ = ns.remove_tree("/tmp/mcpush-k");
    ns.mkdir_p(pkg).map_err(|e| format!("mkdir: {e}"))?;
    for (n, d) in [("keep", &b"keep"[..]), ("old", b"old"), ("gone", b"gone")] {
        ns.write_file(&format!("{pkg}/{n}"), d, Some(0)).map_err(|e| format!("write {n}: {e}"))?;
    }
    let (big, small) = (body(700_001, 3), body(5, 9));
    let http = Http::start_on(HTTP_PORT, vec![(String::from("/p/big"), big.clone()), (String::from("/p/small"), small.clone())])?;
    let r = with_server(PORT, TOOLS.entries(), |c| push_flow(c, &http, stage, pkg, &big, &small));
    drop(http);
    let _ = ns.remove_tree("/tmp/mcpush-k");
    sched::reap();
    r
}
ktest!("mc.push.tmpfs", Safe, push_tmpfs);

/// uid, gid and permission bits of `p` must be `want`.
fn owner(ns: &vfs::MountNs, p: &str, want: (u32, u32, u32)) -> TestResult {
    let a = ns.lookup_path(p.as_bytes(), false).map_err(|e| format!("{p}: {e}"))?.attr();
    let got = (a.uid, a.gid, a.mode & 0o7777);
    kassert!(got == want, "{p}: uid {} gid {} mode {:04o}, want {} {} {:04o}", got.0, got.1, got.2, want.0, want.1, want.2);
    Ok(())
}

/// `\BITSOS2\home\` (fs-fat class Home: uid/gid 1000, directories 0700, files 0600) stays the user's: what
/// write_file and fetch make there, and what move_tree moves in (whole, by a merge, through directories
/// make_parents makes there, by any spelling), gets the class's owner and mode at once; what move_tree moves
/// out (whole or merged) is root's again (0755). The home itself is never moved away. What the test made goes
/// again (home itself too when it was not there before).
fn home_keeps_its_owner(c: &mut Client, ns: &vfs::MountNs, url: &str) -> TestResult {
    let (home, dir, stage, out) = ("/boot/BITSOS2/home", "/boot/BITSOS2/home/mcpush-k", "/boot/BITSOS2/.push-mcpush-h", "/boot/BITSOS2/mcpush-out");
    const USER_DIR: (u32, u32, u32) = (1000, 1000, 0o700);
    const USER_FILE: (u32, u32, u32) = (1000, 1000, 0o600);
    const ROOT: (u32, u32, u32) = (0, 0, 0o755);
    let had_home = ns.lookup_path(home.as_bytes(), false).is_ok();
    let clean = || {
        let _ = ns.remove_tree(dir);
        let _ = ns.remove_tree(stage);
        let _ = ns.remove_tree(out);
    };
    clean();
    let r = (|| -> TestResult {
        let fetch = |c: &mut Client, path: &str| c.ok("fetch", &format!(r#"{{"url":"{url}","path":"{path}","parents":true,"timeout_ms":60000}}"#));
        c.ok("write_file", &format!(r#"{{"path":"{dir}/a","text":"h","parents":true}}"#))?;
        fetch(c, &format!("{dir}/c"))?;
        for (p, want) in [(String::from(dir), USER_DIR), (format!("{dir}/a"), USER_FILE), (format!("{dir}/c"), USER_FILE)] {
            owner(ns, &p, want)?;
        }
        // The home itself never moves away, by any spelling, also onto a missing target.
        refused(c, "move_tree", &format!(r#"{{"from":"{home}","to":"{out}"}}"#))?;
        refused(c, "move_tree", &format!(r#"{{"from":"/boot/bitsos2/HOME.","to":"{out}"}}"#))?;
        kassert!(ns.lookup_path(out.as_bytes(), false).is_err(), "{out} made by a refused move_tree");
        // In, whole, below a directory make_parents makes in the home.
        for f in ["b", "sub/deep/d"] {
            fetch(c, &format!("{stage}/files/{f}"))?;
        }
        owner(ns, &format!("{stage}/files/b"), ROOT)?;
        let t = c.ok("move_tree", &format!(r#"{{"from":"{stage}/files","to":"{dir}/new/in"}}"#))?;
        kassert!(t.contains("moved whole") && t.contains("into the user's home"), "{t}");
        for (p, want) in [("new", USER_DIR), ("new/in", USER_DIR), ("new/in/b", USER_FILE), ("new/in/sub", USER_DIR), ("new/in/sub/deep", USER_DIR), ("new/in/sub/deep/d", USER_FILE)] {
            owner(ns, &format!("{dir}/{p}"), want)?;
        }
        // In, merged: a replaced file and a new one in an existing home directory.
        for f in ["b", "sub/e"] {
            fetch(c, &format!("{stage}/files/{f}"))?;
        }
        let t = c.ok("move_tree", &format!(r#"{{"from":"{stage}/files","to":"{dir}/new/in"}}"#))?;
        kassert!(t.contains("merged into") && t.contains("(1 replaced)") && t.contains("into the user's home"), "{t}");
        for p in ["new/in/b", "new/in/sub/e"] {
            owner(ns, &format!("{dir}/{p}"), USER_FILE)?;
        }
        // In by another spelling of the home.
        fetch(c, &format!("{stage}/files/s"))?;
        let t = c.ok("move_tree", &format!(r#"{{"from":"{stage}/files","to":"/boot/bitsos2/HOME./mcpush-k/sp"}}"#))?;
        kassert!(t.contains("into the user's home"), "{t}");
        owner(ns, &format!("{dir}/sp"), USER_DIR)?;
        owner(ns, &format!("{dir}/sp/s"), USER_FILE)?;
        // Out, whole: root's again, the subtree too.
        let t = c.ok("move_tree", &format!(r#"{{"from":"{dir}/new","to":"{out}"}}"#))?;
        kassert!(t.contains("moved whole") && t.contains("out of the user's home"), "{t}");
        for p in ["", "/in", "/in/b", "/in/sub", "/in/sub/deep", "/in/sub/deep/d", "/in/sub/e"] {
            owner(ns, &format!("{out}{p}"), ROOT)?;
        }
        // Out, merged into an existing directory outside.
        c.ok("write_file", &format!(r#"{{"path":"{dir}/m/x","text":"x","parents":true}}"#))?;
        owner(ns, &format!("{dir}/m/x"), USER_FILE)?;
        let t = c.ok("move_tree", &format!(r#"{{"from":"{dir}/m","to":"{out}/in"}}"#))?;
        kassert!(t.contains("merged into") && t.contains("out of the user's home"), "{t}");
        owner(ns, &format!("{out}/in/x"), ROOT)?;
        kassert!(ns.lookup_path(format!("{dir}/m").as_bytes(), false).is_err(), "{dir}/m left after the merge");
        // Within the home: no note, the owner stays.
        let t = c.ok("move_tree", &format!(r#"{{"from":"{dir}/sp","to":"{dir}/sp2"}}"#))?;
        kassert!(!t.contains("user's home"), "{t}");
        owner(ns, &format!("{dir}/sp2/s"), USER_FILE)?;
        c.ok("delete", &format!(r#"{{"path":"{dir}","recursive":true}}"#))?;
        Ok(())
    })();
    clean();
    if !had_home {
        let _ = ns.remove_tree(home);
    }
    r
}

fn push_stick() -> TestResult {
    if fs_fat::stick::wait_mounted(40_000).is_none() {
        return match fs_fat::stick::last_reason() {
            Some(r) => Err(format!("the boot stick was not mounted: {r}")),
            None => ktest::skip("no boot stick in this run (no USB disk recognised as the stick)"),
        };
    }
    let ns = vfs::root_ns();
    let (stage, pkg) = ("/boot/BITSOS2/.push-mcpush-k", "/boot/BITSOS2/mcpush-k");
    let _ = ns.remove_tree(stage);
    let _ = ns.remove_tree(pkg);
    ns.mkdir_p(pkg).map_err(|e| format!("mkdir {pkg}: {e} (stick read-only? {:?})", fs_fat::stick::last_reason()))?;
    for (n, d) in [("keep", &b"keep"[..]), ("old", b"old"), ("gone", b"gone")] {
        ns.write_file(&format!("{pkg}/{n}"), d, Some(0)).map_err(|e| format!("write {n}: {e}"))?;
    }
    let config_before = ns.read_file("/boot/BITSOS2/CONFIG.TXT", 1 << 20).map_err(|e| format!("CONFIG.TXT: {e}"))?;
    let sys_before = ns.list_dir("/boot/BITSOS2/SYS").map_err(|e| format!("SYS: {e}"))?.len();
    let (big, small) = (body(1_300_007, 5), body(17, 1));
    let http = Http::start_on(HTTP_PORT, vec![(String::from("/p/big"), big.clone()), (String::from("/p/small"), small.clone())])?;
    let r = with_server(PORT, TOOLS.entries(), |c| {
        push_flow(c, &http, stage, pkg, &big, &small)?;
        kassert!(ns.read_file(&format!("{pkg}/lib/big.bin"), 4 << 20).ok().as_deref() == Some(&big[..]), "big.bin differs on the stick");
        // The stick rule: SYS, CONFIG.TXT, STICK.ID, outside BITSOS2, by any spelling.
        let url = http.url("/p/small");
        for p in ["/boot/BITSOS2/SYS/99/KERNEL", "/boot/BITSOS2/sys./x", "/boot/BITSOS2/CONFIG.TXT", "/boot/BITSOS2/config.txt", "/boot/BITSOS2/STICK.ID", "/boot/EFI/x", "/boot/x"] {
            refused(c, "write_file", &format!(r#"{{"path":"{p}","text":"x","parents":true}}"#))?;
        }
        for p in ["/boot/BITSOS2/SYS/x", "/boot/BITSOS2/CONFIG.TXT", "/boot/BITSOS2/STICK.ID", "/boot/EFI/x", "/boot/x"] {
            refused(c, "fetch", &format!(r#"{{"url":"{url}","path":"{p}"}}"#))?;
        }
        for p in ["/boot/BITSOS2/SYS", "/boot/BITSOS2/CONFIG.TXT", "/boot/BITSOS2/STICK.ID", "/boot/BITSOS2", "/boot/EFI"] {
            refused(c, "delete", &format!(r#"{{"path":"{p}","recursive":true}}"#))?;
        }
        refused(c, "move_tree", &format!(r#"{{"from":"{pkg}","to":"/boot/BITSOS2/SYS/98"}}"#))?;
        refused(c, "move_tree", &format!(r#"{{"from":"{pkg}","to":"/boot/mcpush-k"}}"#))?;
        refused(c, "move_tree", r#"{"from":"/boot/BITSOS2/SYS","to":"/boot/BITSOS2/mcpush-sys"}"#)?;
        home_keeps_its_owner(c, &ns, &url)
    });
    drop(http);
    let _ = ns.remove_tree(stage);
    let _ = ns.remove_tree(pkg);
    sched::reap();
    r?;
    kassert!(ns.read_file("/boot/BITSOS2/CONFIG.TXT", 1 << 20).ok() == Some(config_before), "CONFIG.TXT changed");
    kassert!(ns.list_dir("/boot/BITSOS2/SYS").map(|l| l.len()).ok() == Some(sys_before), "SYS changed");
    kassert!(ns.lookup_path(pkg.as_bytes(), false).is_err() && ns.lookup_path(stage.as_bytes(), false).is_err(), "scratch left on the stick");
    Ok(())
}
ktest!("mc.push.stick", QemuOnly, push_stick);
