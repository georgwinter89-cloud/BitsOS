use super::*;

#[test]
fn schemas_are_json_and_names_fit_the_kernel() {
    for a in list() {
        let s = Json::parse(&a.schema).unwrap_or_else(|e| panic!("{}: {e}", a.name));
        assert_eq!(s.get("type").and_then(Json::as_str), Some("object"));
        assert!(mcpreg::Tool::new(&a.name, &a.description, &a.schema, a.safety).is_ok());
        assert!(a.name.len() <= 64 && !a.name.starts_with("app.") && !a.name.starts_with("settings"));
        assert_eq!(a.gated(), matches!(a.name.as_str(), "brave_fill_form" | "brave_download"), "{}", a.name);
    }
}

#[test]
fn urls() {
    for ok in ["http://a.test/", "https://x", "about:blank", "HTTPS://X"] {
        assert!(allowed_url(ok), "{ok}");
    }
    // file: would let read actions read any file of Brave's user without a permission.
    for bad in ["file:///etc/passwd", "FILE:///data/home/1000/x", " file://localhost/etc/passwd", "view-source:file:///etc/passwd", "chrome://settings", "javascript:alert(1)", "data:text/html,x", "http://", "brave://rewards", "about:config", ""] {
        assert!(!allowed_url(bad), "{bad}");
    }
}

#[test]
fn text_fits_the_answer() {
    assert_eq!(fit_text("abc", 100), ("abc".to_string(), false));
    // Three bytes a character in UTF-8, cut on a character boundary.
    let cjk = "語".repeat(1000);
    let (t, cut) = fit_text(&cjk, 302);
    assert!(cut && Json::str(&t).to_string().len() <= 302 && t.chars().all(|c| c == '語'), "{}", t.len());
    // Control characters become \u escapes (6 bytes): the JSON text counts, not the characters.
    let ctl = "\u{1}".repeat(1000);
    let (t, cut) = fit_text(&ctl, 600);
    assert!(cut && Json::str(&t).to_string().len() <= 600 && !t.is_empty());
    let big = "x".repeat(5 << 20);
    let (t, cut) = fit_text(&big, ANSWER_BUDGET);
    assert!(cut && t.len() <= ANSWER_BUDGET && t.len() > ANSWER_BUDGET - 16);
}

#[test]
fn download_folders() {
    let d = Path::new("/tmp/Downloads");
    assert_eq!(download_dir(d, None).ok().unwrap(), (d.to_path_buf(), d.to_path_buf()));
    assert_eq!(download_dir(d, Some("sub/x")).ok().unwrap().1, d.join("sub/x"));
    assert_eq!(download_dir(d, Some("/tmp/Downloads/a")).ok().unwrap().1, d.join("a"));
    assert_eq!(download_dir(d, Some("/data/home/1000/Docs")).ok().unwrap(), (PathBuf::from(HOME_1000), PathBuf::from("/data/home/1000/Docs")));
    for bad in ["/etc", "/", "/tmp/Downloads2", "/tmp/Downloads/../x", "../x", "", "/data/home/10000"] {
        assert!(download_dir(d, Some(bad)).is_err(), "{bad}");
    }
    // A link below the root is not followed; folders are made below it only.
    let root = std::env::temp_dir().join(format!("brave-act-dirs-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    std::os::unix::fs::symlink("/", root.join("up")).unwrap();
    assert!(make_dir(&root, &root.join("up/etc"), true).is_err());
    make_dir(&root, &root.join("a/b"), true).unwrap();
    assert!(root.join("a/b").is_dir());
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn place_never_replaces() {
    let root = std::env::temp_dir().join(format!("brave-act-place-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("g1"), b"new").unwrap();
    std::fs::write(root.join("f.txt"), b"old").unwrap();
    std::os::unix::fs::symlink("/nonexistent-ac3", root.join("f (1).txt")).unwrap();
    let p = place(&root, "g1", "f.txt", &|_| Ok(())).ok().unwrap();
    assert_eq!(p, root.join("f (2).txt"));
    assert_eq!(std::fs::read(root.join("f.txt")).unwrap(), b"old");
    assert_eq!(std::fs::read(&p).unwrap(), b"new");
    assert!(!root.join("g1").exists());
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn names_and_png() {
    assert_eq!(safe_name("report.pdf"), "report.pdf");
    assert_eq!(safe_name("../../etc/passwd"), "passwd");
    assert_eq!(safe_name(".hidden"), "hidden");
    assert_eq!(safe_name(""), "download");
    assert_eq!(safe_name("a\nb"), "a_b");
    let mut p = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
    p.extend_from_slice(&1280u32.to_be_bytes());
    p.extend_from_slice(&800u32.to_be_bytes());
    assert_eq!(png_size(&p), Some((1280, 800)));
    assert_eq!(png_size(b"nope"), None);
}

/// The pipe protocol against a fake Brave: answers by id, events to subscribers, an error answer, EOF ends a
/// waiting call, an over-long message ends the connection.
#[test]
fn cdp_over_a_pipe() {
    let (to_brave_r, to_brave_w) = std::io::pipe().unwrap();
    let (from_brave_r, mut from_brave_w) = std::io::pipe().unwrap();
    let (ev, _) = Events::capture("t");
    let c = Conn::new(from_brave_r, to_brave_w, ev);
    let fake = std::thread::spawn(move || {
        let mut r = std::io::BufReader::new(to_brave_r);
        let mut msg = Vec::new();
        use std::io::{BufRead, Write};
        for n in 0..3 {
            msg.clear();
            r.read_until(0, &mut msg).unwrap();
            let j = Json::parse(std::str::from_utf8(&msg[..msg.len() - 1]).unwrap()).unwrap();
            let id = j.get("id").and_then(Json::as_u64).unwrap();
            let out = match n {
                0 => {
                    assert_eq!(j.get("sessionId").and_then(Json::as_str), Some("S1"));
                    format!("{{\"method\":\"Page.x\",\"params\":{{\"a\":1}}}}\0{{\"id\":{id},\"result\":{{\"ok\":true}}}}\0")
                }
                1 => format!("{{\"id\":{id},\"error\":{{\"code\":-32000,\"message\":\"No target\"}}}}\0"),
                _ => String::new(),
            };
            from_brave_w.write_all(out.as_bytes()).unwrap();
        }
        // The third call gets no answer: Brave "ends".
        drop(from_brave_w);
    });
    let dl = || Instant::now() + Duration::from_secs(10);
    let sub = c.subscribe(|j| j.get("method").and_then(Json::as_str) == Some("Page.x"));
    let r = c.call("A.b", Json::Obj(Vec::new()), Some("S1"), dl()).unwrap();
    assert_eq!(r.get("ok").and_then(Json::as_bool), Some(true));
    assert!(sub.next(dl()).is_some());
    assert_eq!(c.call("A.c", Json::Obj(Vec::new()), None, dl()).unwrap_err(), "A.c: No target");
    let e = c.call("A.d", Json::Obj(Vec::new()), None, dl()).unwrap_err();
    assert!(e.contains("pipe ended"), "{e}");
    assert!(!c.alive());
    assert!(c.call("A.e", Json::Obj(Vec::new()), None, dl()).is_err());
    fake.join().unwrap();

    let (_keep, w) = std::io::pipe().unwrap();
    let (r, mut bw) = std::io::pipe().unwrap();
    let (ev, _) = Events::capture("t");
    let c = Conn::new(r, w, ev);
    let big = vec![b'x'; crate::cdp::MAX_MSG + 70_000];
    std::thread::spawn(move || {
        use std::io::Write;
        let _ = bw.write_all(&big);
    });
    let end = Instant::now() + Duration::from_secs(20);
    while c.alive() && Instant::now() < end {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(!c.alive() && c.why().contains("longer than"), "{}", c.why());
    // The end closes brave-act's writer: Brave sees EOF on its fd 3 (and ends), not only once the Conn is dropped.
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        use std::io::Read;
        let mut keep = _keep;
        let mut b = [0u8; 16];
        let _ = tx.send(keep.read(&mut b).ok());
    });
    assert_eq!(rx.recv_timeout(Duration::from_secs(10)), Ok(Some(0)), "EOF on Brave's side");
    drop(c);
}
