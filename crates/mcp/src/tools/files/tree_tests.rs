//! Host tests of MC-push's rules and tools on the host's tmpfs root view: the stick rule (pure), `manifest`
//! (hashes, order, pages, missing_ok) and `move_tree` (whole move, merge with replace/type change/remove/prune,
//! the second run finishing nothing, refusals).

extern crate std;
use super::*;
use crate::sha256::{Sha256, hex};
use crate::{Json, TOOLS, rpc};
use std::string::ToString;

fn call(name: &str, args: &str) -> Json {
    let body = format!(r#"{{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{{"name":"{name}","arguments":{args}}}}}"#);
    let a = rpc::handle(body.as_bytes(), 1_000_000_000, TOOLS.entries());
    Json::parse(&a.body).unwrap()
}
fn text_of(j: &Json) -> String {
    let r = j.get("result").unwrap_or_else(|| panic!("no result: {}", j.to_string()));
    r.get("content").and_then(Json::as_array).and_then(|c| c.first()).and_then(|c| c.get("text")).and_then(Json::as_str).unwrap_or("").to_string()
}
fn failed(j: &Json) -> bool {
    j.get("result").and_then(|r| r.get("isError")).and_then(Json::as_bool) == Some(true)
}
fn ok(name: &str, args: &str) -> String {
    let j = call(name, args);
    assert!(!failed(&j) && j.get("error").is_none(), "{name} {args}: {}", j.to_string());
    text_of(&j)
}
fn refused(name: &str, args: &str) -> String {
    let j = call(name, args);
    let t = text_of(&j);
    assert!(failed(&j) && t.starts_with("Refused: "), "{name} {args} should be refused: {}", j.to_string());
    t
}
fn put(path: &str, text: &str) {
    ok("write_file", &format!(r#"{{"path":"{path}","text":"{text}","parents":true}}"#));
}
fn sha(d: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(d);
    hex(&h.finish())
}
fn manifest(path: &str, extra: &str) -> Json {
    Json::parse(&ok("manifest", &format!(r#"{{"path":"{path}"{extra}}}"#))).unwrap()
}
/// (path, type, size, sha256) of every entry.
fn entries(j: &Json) -> Vec<(String, String, Option<u64>, Option<String>)> {
    j.get("entries")
        .and_then(Json::as_array)
        .unwrap()
        .iter()
        .map(|e| {
            let s = |k| e.get(k).and_then(Json::as_str).map(String::from);
            (s("path").unwrap(), s("type").unwrap(), e.get("size").and_then(Json::as_u64), s("sha256"))
        })
        .collect()
}
fn read(path: &str) -> Vec<u8> {
    vfs::root_ns().read_file(path, 1 << 20).unwrap()
}
fn exists(path: &str) -> bool {
    vfs::root_ns().lookup_path(path.as_bytes(), false).is_ok()
}

#[test]
fn the_stick_rule() {
    for p in ["/boot/BITSOS2/home", "/boot/bitsos2/HOME/a", "/boot/BITSOS2/home./x", "/boot/BITSOS2/Home /x"] {
        assert!(in_stick_home(p), "{p}");
    }
    for p in ["/boot/BITSOS2", "/boot/BITSOS2/homes", "/boot/BITSOS2/cobalt/home", "/tmp/home", "/boot/home"] {
        assert!(!in_stick_home(p), "{p}");
    }
    for p in ["/boot/BITSOS2/home", "/boot/bitsos2/HOME.", "/boot/BITSOS2/Home /", "/boot//BITSOS2/home/"] {
        assert!(is_stick_home(p), "{p}");
    }
    for p in ["/boot/BITSOS2/home/a", "/boot/BITSOS2", "/boot/BITSOS2/homes", "/boot/BITSOS2/cobalt/home", "/tmp/home"] {
        assert!(!is_stick_home(p), "{p}");
    }
    for (dir, name) in [("/boot/BITSOS2", "cobalt"), ("/boot/BITSOS2", ".push-cobalt"), ("/boot/bitsos2/cobalt/lib", "x.so"), ("/boot/BITSOS2/home/georg", "a")] {
        assert!(stick_refusal(dir, name.as_bytes()).is_none(), "{dir} {name}");
    }
    for (dir, name) in [
        ("/boot", "BITSOS2"),
        ("/boot", "x"),
        ("/boot/EFI/BOOT", "BOOTX64.EFI"),
        ("/boot/BITSOS2", "SYS"),
        ("/boot/BITSOS2", "sys."),
        ("/boot/BITSOS2", "Config.Txt"),
        ("/boot/BITSOS2", "STICK.ID"),
        ("/boot/BITSOS2/SYS", "5"),
        ("/boot/BITSOS2/sys/1", "KERNEL"),
        ("/mnt/BITSOS2", "x"),
    ] {
        assert!(stick_refusal(dir, name.as_bytes()).is_some(), "{dir} {name} must be refused");
    }
    assert!(rel_ok("a/b.txt") && rel_ok(".hidden/x"));
    for r in ["", "/a", "a//b", "a/./b", "../x", "a/..", "a\0b"] {
        assert!(!rel_ok(r), "{r:?}");
    }
    assert!(within("/boot/BITSOS2/cobalt/x", "/boot/bitsos2/COBALT") && within("/a", "/a/") && !within("/ab", "/a") && !within("/a", "/a/b"));
    assert!(overlaps("/a/b", "/a") && overlaps("/a", "/a/b") && !overlaps("/a/b", "/a/c"));
}

#[test]
fn manifest_hashes_sorts_and_pages() {
    put("/t1/pkg/b.txt", "bee");
    put("/t1/pkg/a/z.txt", "zed");
    put("/t1/pkg/a.txt", "");
    let j = manifest("/t1/pkg", "");
    assert_eq!(j.get("files").and_then(Json::as_u64), Some(3));
    assert_eq!(j.get("bytes").and_then(Json::as_u64), Some(6));
    let e = entries(&j);
    let names: Vec<&str> = e.iter().map(|x| x.0.as_str()).collect();
    assert_eq!(names, ["a", "a.txt", "a/z.txt", "b.txt"], "byte order of the relative path");
    assert_eq!(e[0].1, "directory");
    assert_eq!((e[2].2, e[2].3.as_deref()), (Some(3), Some(sha(b"zed").as_str())));
    assert_eq!(e[1].3.as_deref(), Some(sha(b"").as_str()));
    assert!(entries(&manifest("/t1/pkg", r#","hash":"none""#)).iter().all(|x| x.3.is_none()));
    assert_eq!(entries(&manifest("/t1/pkg", r#","after":"a.txt""#)).len(), 2);
    let m = manifest("/t1/none", r#","missing_ok":true"#);
    assert_eq!(m.get("exists").and_then(Json::as_bool), Some(false));
    assert!(failed(&call("manifest", r#"{"path":"/t1/none"}"#)));
    assert!(failed(&call("manifest", r#"{"path":"/t1/pkg/b.txt"}"#)), "a file is no tree");
    assert!(call("manifest", r#"{"path":"/t1/pkg","hash":"md5"}"#).get("error").is_some());
    // only: one entry (push.mjs --file), none when missing, exact on tmpfs (case counts there).
    let e = entries(&manifest("/t1/pkg", r#","only":"a/z.txt""#));
    assert_eq!((e.len(), e[0].0.as_str(), e[0].3.as_deref()), (1, "a/z.txt", Some(sha(b"zed").as_str())));
    assert_eq!(entries(&manifest("/t1/pkg", r#","only":"a""#)).len(), 1, "a directory alone, nothing below it");
    assert!(entries(&manifest("/t1/pkg", r#","only":"A/z.txt""#)).is_empty());
    assert!(entries(&manifest("/t1/pkg", r#","only":"nope""#)).is_empty());
    for bad in [r#""../x""#, r#""/a""#, r#""a//b""#, "5"] {
        assert!(call("manifest", &format!(r#"{{"path":"/t1/pkg","only":{bad}}}"#)).get("error").is_some(), "only {bad}");
    }
    // Pages: more names than one answer's budget; every page continues after the last, none is lost.
    let ns = vfs::root_ns();
    ns.mkdir_p("/t1/many").unwrap();
    let n = super::tree::ANSWER_BUDGET / 120 + 500;
    for i in 0..n {
        ns.write_file(&format!("/t1/many/f{i:06}"), b"", None).unwrap();
    }
    let (mut seen, mut after, mut pages) = (0usize, String::new(), 0);
    loop {
        let extra = if after.is_empty() { String::from(r#","hash":"none""#) } else { format!(r#","hash":"none","after":"{after}""#) };
        let j = manifest("/t1/many", &extra);
        let e = entries(&j);
        seen += e.len();
        pages += 1;
        match j.get("next").and_then(Json::as_str) {
            Some(nx) => {
                assert_eq!(nx, e.last().unwrap().0);
                after = String::from(nx);
            }
            None => break,
        }
    }
    assert_eq!((seen, pages > 1), (n, true), "{pages} pages");
}

#[test]
fn move_tree_commits_and_finishes() {
    // First push: the target is missing, the staged tree is moved whole.
    put("/t2/stage1/lib/x.so", "x1");
    put("/t2/stage1/run", "r1");
    let t = ok("move_tree", r#"{"from":"/t2/stage1","to":"/t2/pkg"}"#);
    assert!(t.contains("moved whole") && t.contains("0 files and 1 directories"), "{t}");
    assert!(!exists("/t2/stage1") && read("/t2/pkg/lib/x.so") == b"x1");
    // An update: one file replaced, one new in a new directory, a file becomes a directory, two removed (one
    // missing already), the directory they leave empty pruned.
    put("/t2/pkg/old/only.txt", "o");
    put("/t2/pkg/keep.txt", "k");
    put("/t2/pkg/swap", "file");
    put("/t2/stage2/lib/x.so", "x2");
    put("/t2/stage2/new/deep/n.txt", "n");
    put("/t2/stage2/swap/inner", "dir now");
    let t = ok("move_tree", r#"{"from":"/t2/stage2","to":"/t2/pkg","remove":["old/only.txt","gone.txt"]}"#);
    assert!(t.contains("1 files and 2 directories moved (2 replaced), 1 removed (1 already gone), 1 empty directories pruned"), "{t}");
    assert_eq!(read("/t2/pkg/lib/x.so"), b"x2");
    assert_eq!(read("/t2/pkg/new/deep/n.txt"), b"n");
    assert_eq!(read("/t2/pkg/swap/inner"), b"dir now");
    assert_eq!(read("/t2/pkg/keep.txt"), b"k");
    assert!(!exists("/t2/pkg/old") && !exists("/t2/stage2") && read("/t2/pkg/run") == b"r1");
    // Again (as after an interruption): nothing left to move, the removals count as gone.
    let t = ok("move_tree", r#"{"from":"/t2/stage2","to":"/t2/pkg","remove":["old/only.txt"]}"#);
    assert!(t.contains("nothing left to move") && t.contains("0 removed (1 already gone)"), "{t}");
}

#[test]
fn move_tree_refusals() {
    put("/t3/pkg/a", "a");
    put("/t3/pkg/sub/b", "b");
    put("/t3/stage/a", "A");
    let bad = |args: &str| assert!(call("move_tree", args).get("error").is_some(), "{args}");
    bad(r#"{"from":"/t3/stage"}"#);
    bad(r#"{"from":"/t3/stage","to":"/t3/pkg","remove":["../x"]}"#);
    bad(r#"{"from":"/t3/stage","to":"/t3/pkg","remove":["/etc/passwd"]}"#);
    bad(r#"{"from":"/t3/stage","to":"/t3/pkg","remove":"a"}"#);
    bad(r#"{"from":"/t3/stage","to":"/t3/pkg","remove":[5]}"#);
    refused("move_tree", r#"{"from":"/t3/pkg/sub","to":"/t3/pkg"}"#);
    refused("move_tree", r#"{"from":"/t3/pkg","to":"/t3/pkg/sub/deeper"}"#);
    refused("move_tree", r#"{"from":"/t3/stage","to":"/dev/x"}"#);
    refused("move_tree", r#"{"from":"/t3/stage","to":"/proc/x"}"#);
    assert!(failed(&call("move_tree", r#"{"from":"/t3/pkg/a","to":"/t3/x"}"#)), "from is no directory");
    assert_eq!(read("/t3/pkg/a"), b"a", "nothing moved by a refused call");
    assert_eq!(read("/t3/stage/a"), b"A");
}

#[test]
fn move_tree_is_busy_while_another_works_on_the_tree() {
    put("/t4/stage/a", "A");
    put("/t4/other/stage/b", "B");
    let claim = super::tree::Moving::take("/t4/stage", "/t4/pkg").unwrap();
    // The same trees (also by another spelling or from inside) are EBUSY at once, nothing moved.
    for args in [r#"{"from":"/t4/stage","to":"/t4/pkg"}"#, r#"{"from":"/T4/Stage/","to":"/t4/pkg"}"#, r#"{"from":"/t4/x","to":"/t4/pkg/sub"}"#] {
        let j = call("move_tree", args);
        assert!(failed(&j) && text_of(&j).contains("EBUSY"), "{args}: {}", j.to_string());
    }
    assert_eq!(read("/t4/stage/a"), b"A");
    // Another tree goes on meanwhile.
    ok("move_tree", r#"{"from":"/t4/other/stage","to":"/t4/other/pkg"}"#);
    drop(claim);
    let t = ok("move_tree", r#"{"from":"/t4/stage","to":"/t4/pkg"}"#);
    assert!(t.contains("moved whole"), "{t}");
}

#[test]
fn manifest_pages_end_by_hashed_bytes() {
    let ns = vfs::root_ns();
    ns.mkdir_p("/t5/big").unwrap();
    let half = (super::tree::HASH_BUDGET / 2 + 1) as usize;
    for n in ["a", "b", "c"] {
        ns.write_file(&format!("/t5/big/{n}"), &vec![n.as_bytes()[0]; half], None).unwrap();
    }
    let j = manifest("/t5/big", "");
    assert_eq!(entries(&j).iter().map(|e| e.0.as_str()).collect::<Vec<_>>(), ["a", "b"], "the page ends once 32 MiB are hashed");
    assert_eq!(j.get("next").and_then(Json::as_str), Some("b"));
    let j = manifest("/t5/big", r#","after":"b""#);
    let e = entries(&j);
    assert_eq!((e.len(), e[0].3.as_deref()), (1, Some(sha(&vec![b'c'; half]).as_str())));
    assert!(matches!(j.get("next"), Some(Json::Null)));
    // Without hashes only the answer size counts: one page.
    assert_eq!(entries(&manifest("/t5/big", r#","hash":"none""#)).len(), 3);
}

#[test]
fn deep_and_long_trees_stay_bounded() {
    // 300 levels of "d", and 40 levels of 200-byte names (far beyond PATH_MAX as one path): manifest and
    // move_tree walk them without recursion; each answers or fails, none panics.
    let ns = vfs::root_ns();
    let c = vfs::Ctx::kernel(&ns);
    ns.mkdir_p("/t6/stage").unwrap();
    let mut cur = c.lookup(None, b"/t6/stage", true).unwrap();
    for _ in 0..300 {
        cur = c.mkdir(Some(&cur), b"d", 0o755).unwrap();
    }
    let long = [b'n'; 200];
    let mut cur2 = c.lookup(None, b"/t6/stage", true).unwrap();
    for _ in 0..40 {
        cur2 = c.mkdir(Some(&cur2), &long, 0o755).unwrap();
    }
    let j = manifest("/t6/stage", "");
    let e = entries(&j);
    assert_eq!(e.len(), 340, "every directory listed");
    assert!(e.iter().any(|x| x.0.len() > 8000), "the long chain's relative path");
    // Moved whole into a missing target, then merged into the existing one from a second staging tree.
    let t = ok("move_tree", r#"{"from":"/t6/stage","to":"/t6/pkg"}"#);
    assert!(t.contains("moved whole"), "{t}");
    ns.mkdir_p("/t6/stage2").unwrap();
    let mut cur3 = c.lookup(None, b"/t6/stage2", true).unwrap();
    for _ in 0..300 {
        cur3 = c.mkdir(Some(&cur3), b"d", 0o755).unwrap();
    }
    let j = call("move_tree", r#"{"from":"/t6/stage2","to":"/t6/pkg"}"#);
    assert!(j.get("result").is_some(), "an answer, not a crash: {}", j.to_string());
    let _ = ns.remove_tree("/t6");
}

#[test]
fn fetch_refuses_before_it_makes_directories() {
    let j = call("fetch", r#"{"url":"http://127.0.0.1:1/x","path":"/t7/new/deeper/CONFIG.TXT","parents":true}"#);
    assert!(failed(&j) && text_of(&j).starts_with("Refused: "), "{}", j.to_string());
    assert!(!exists("/t7/new"), "a refused fetch left directories behind");
    let j = call("fetch", r#"{"url":"http://8.8.8.8/x","path":"/t7/new/f","parents":true}"#);
    assert!(failed(&j) && !exists("/t7/new"), "{}", j.to_string());
}
