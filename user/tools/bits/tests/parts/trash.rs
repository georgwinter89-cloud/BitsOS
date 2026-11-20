//! The trash (PLAN K6) on disk as the kernel writes it (eins-c.md §2), made by the test: listing, restoring (never
//! over something), emptying only after the desktop's yes and never from the assistant's tree. There is no
//! `/dev/bitsos/papierkorb` on the host, so bits moves the names itself (the node's way: fs/60-papierkorb).

use std::path::Path;

use serde_json::json;

use crate::common::{World, keys};

fn entry(w: &World, id: &str, original: &Path, time: u64, body: &str) {
    let fs = w.dir.join("mnt");
    let d = fs.join(".papierkorb").join(id);
    let content = d.join("files").join(original.strip_prefix(&fs).unwrap());
    std::fs::create_dir_all(content.parent().unwrap()).unwrap();
    std::fs::write(&content, body).unwrap();
    let rec = json!({"v": 1, "id": id, "fs": fs, "path": original, "what": "unlink", "time": time, "pid": 42, "command": "rm", "chat": "c1", "tree": 1});
    std::fs::write(d.join("record.json"), rec.to_string()).unwrap();
}

fn with_answer(name: &str, answer: &'static str) -> World {
    let w = World::new(name);
    w.comp(move |l| if l.starts_with("bits-frage ") { "frage 2 120".into() } else if l == "bits-antwort 2" { answer.into() } else { "error: ?".into() });
    std::fs::create_dir_all(w.dir.join("mnt/home")).unwrap();
    w
}

#[test]
fn listing_and_restoring() {
    let w = with_answer("trash-list", "ja");
    let a = w.dir.join("mnt/home/brief.txt");
    entry(&w, "t1", &a, 100, "Lieber Georg");
    entry(&w, "t2", &w.dir.join("mnt/home/alt.txt"), 200, "x");
    let (code, v) = w.json(&["papierkorb", "liste"]);
    assert_eq!(code, 0);
    assert_eq!(keys(&v), ["bytes", "entries", "ok"]);
    assert_eq!(keys(&v["entries"][0]), ["bytes", "chat", "command", "id", "kind", "mount", "original", "pid", "time"]);
    assert_eq!(v["entries"][0]["id"], "t2", "newest first");
    assert_eq!(v["bytes"], 13);
    assert_eq!(w.json(&["papierkorb", "zurueck", "t1"]), (0, json!({"ok": true, "id": "t1", "restored": a})));
    assert_eq!(std::fs::read_to_string(&a).unwrap(), "Lieber Georg");
    entry(&w, "t3", &a, 300, "zweite");
    let (code, v) = w.json(&["papierkorb", "zurueck", "t3"]);
    assert_eq!(code, 1, "never over a file that is there: {v}");
    assert_eq!(std::fs::read_to_string(&a).unwrap(), "Lieber Georg");
    let other = w.dir.join("mnt/home/brief-2.txt");
    assert_eq!(w.json(&["papierkorb", "zurueck", "t3", "--nach", other.to_str().unwrap()]).0, 0);
    assert_eq!(std::fs::read_to_string(&other).unwrap(), "zweite");
    assert_eq!(w.json(&["papierkorb", "zurueck", "t9"]).0, 1);
}

#[test]
fn emptying_is_georgs_alone() {
    let w = with_answer("trash-empty", "ja");
    entry(&w, "t1", &w.dir.join("mnt/home/a"), 1, "aaaa");
    let (code, v) = w.tree(&["papierkorb", "leeren"]);
    assert_eq!((code, v["error"].as_str()), (5, Some("refused")));
    assert!(World::sent(&w.ctl).is_empty(), "the tree does not even ask");
    assert!(w.dir.join("mnt/.papierkorb/t1").is_dir());
    assert_eq!(w.json(&["papierkorb", "leeren"]), (0, json!({"ok": true, "removed": 1, "bytes": 4})));
    assert_eq!(World::sent(&w.ctl).first().map(String::as_str), Some("bits-frage leeren 1 4"));
    assert!(!w.dir.join("mnt/.papierkorb/t1").exists());
    let w = with_answer("trash-no", "nein");
    entry(&w, "t1", &w.dir.join("mnt/home/a"), 1, "aaaa");
    assert_eq!(w.json(&["papierkorb", "leeren", "t1"]).0, 5);
    assert!(w.dir.join("mnt/.papierkorb/t1").is_dir(), "a no keeps everything");
}
