//! DESIGN §3/§9.4: file events wake the indexer, so a new, changed or deleted file is in (or out of) the
//! index within seconds, long before the rescan (set to an hour here).

mod common;
use common::*;
use serde_json::json;
use std::time::{Duration, Instant};

fn paths(s: &Served, q: &str) -> Vec<String> {
    let v = sock_call(&s.sock, "context_search", json!({"query": q})).unwrap();
    v["result"]["items"].as_array().unwrap().iter().map(|i| i["path"].as_str().unwrap().to_string()).collect()
}

fn wait_for(what: &str, mut f: impl FnMut() -> bool) {
    // Generous: timing is not judged here, only that the change arrives without a rescan.
    let deadline = Instant::now() + Duration::from_secs(60);
    while !f() {
        assert!(Instant::now() < deadline, "{what} did not arrive through the file watch");
        std::thread::sleep(Duration::from_millis(200));
    }
}

#[test]
fn changes_arrive_through_file_events() {
    let d = tmp("watch");
    let root = d.join("docs");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("bleibt.txt"), "Gartenzaun streichen im Frühling").unwrap();
    std::fs::write(root.join("weg.txt"), "Wird gelöscht: Pinguinfutter bestellen").unwrap();
    let cfg =
        write_config(&d, 29548, &[(&root, false)], "[context.index]\nrescan_seconds = 3600\nwatch_debounce_ms = 300\n");
    let s = serve(&d, &cfg, "fake", 29548);
    // The socket and the data folder (it holds extracted text) are Georg's alone.
    use std::os::unix::fs::PermissionsExt;
    let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&s.sock), 0o600);
    assert_eq!(mode(&d.join("data")), 0o700);
    assert!(paths(&s, "Pinguinfutter").iter().any(|p| p.ends_with("weg.txt")));

    std::fs::remove_file(root.join("weg.txt")).unwrap();
    wait_for("a delete", || !paths(&s, "Pinguinfutter").iter().any(|p| p.ends_with("weg.txt")));

    // A new folder and a file in it right away (the watch on the new folder comes after its creation).
    std::fs::create_dir_all(root.join("neu/tiefer")).unwrap();
    std::fs::write(root.join("neu/tiefer/rechnung.txt"), "Rechnung Dachdecker Ziegel").unwrap();
    wait_for("a new file in a new folder", || paths(&s, "Dachdecker").iter().any(|p| p.ends_with("rechnung.txt")));
    std::fs::write(root.join("neu/tiefer/rechnung.txt"), "Rechnung Schornsteinfeger").unwrap();
    wait_for("an edit", || paths(&s, "Schornsteinfeger").iter().any(|p| p.ends_with("rechnung.txt")));
}
