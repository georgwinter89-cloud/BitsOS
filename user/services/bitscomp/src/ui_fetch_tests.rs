use std::io::{BufRead, BufReader};
use std::os::unix::net::UnixListener;

use super::*;

fn need() -> Need {
    Need { window: "w7".into(), pid: 4242, app: "brave".into(), title: "Formular".into(), origin: [90, 40], size: [800, 600], settle: None, again: false }
}

/// A provider that answers one connection with `reply` and hands back the line it got.
fn provider(name: &str, reply: &'static str) -> (std::path::PathBuf, std::thread::JoinHandle<String>) {
    let path = std::path::PathBuf::from(format!("/tmp/cl-c3-{name}-{}", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let l = UnixListener::bind(&path).unwrap();
    let t = std::thread::spawn(move || {
        let (s, _) = l.accept().unwrap();
        let mut line = String::new();
        BufReader::new(s.try_clone().unwrap()).read_line(&mut line).unwrap();
        (&s).write_all(reply.as_bytes()).unwrap();
        line
    });
    (path, t)
}

#[test]
fn a_tree_is_asked_with_the_fields_atspid_reads() {
    let (path, t) = provider("tree", "{\"id\":\"1\",\"ok\":true,\"truncated\":true,\"window\":\"w7\",\"app\":\"brave\",\"title\":\"Formular\",\"pid\":4242,\"source\":\"atspi\",\"focused\":true,\"rect\":[90,40,800,600],\"elements\":[{\"id\":\"w7/3\",\"role\":\"field\",\"name\":\"Passwort\",\"value\":\"geheim\",\"state\":[\"secret\"],\"rect\":[190,80,200,20],\"actions\":[\"type\"],\"children\":[]}]}\n");
    let (w, loading) = ask_tree(&path, &Need { settle: Some((SETTLE_STEP, true)), ..need() }, Duration::from_secs(2)).expect("a window");
    let asked: serde_json::Value = serde_json::from_str(t.join().unwrap().trim()).unwrap();
    assert_eq!(asked["op"], "tree");
    assert_eq!(asked["args"], serde_json::json!({"pid": 4242, "window": "w7", "app": "brave", "title": "Formular", "origin": [90, 40], "size": [800, 600], "settle": {"ms": 3000, "calm": true}}));
    assert!(!loading, "no word of loading: not loading");
    assert_eq!(w.elements[0].name, "Passwort");
    assert_eq!(w.elements[0].value, None, "a secret field never carries its value, whatever the provider sent");
    let _ = std::fs::remove_file(&path);
}

#[test]
fn no_tree_is_a_picture() {
    let (path, t) = provider("none", "{\"id\":\"1\",\"ok\":false,\"error\":\"Dieses Programm zeigt Durchblick keine Bedienelemente\"}\n");
    let r = ask_tree(&path, &need(), Duration::from_secs(2));
    assert!(matches!(&r, Err(e) if e.contains("keine Bedienelemente")), "{r:?}");
    t.join().unwrap();
    let _ = std::fs::remove_file(&path);
}

#[test]
fn a_silent_provider_costs_the_bounded_wait_on_its_own_thread() {
    let path = std::path::PathBuf::from(format!("/tmp/cl-c3-silent-{}", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let l = UnixListener::bind(&path).unwrap();
    let wait = Duration::from_millis(200);
    let t0 = Instant::now();
    assert!(ask_tree(&path, &need(), wait).is_err());
    assert!(t0.elapsed() >= wait && t0.elapsed() < wait * 4, "{:?}", t0.elapsed());
    assert!(l.accept().is_ok(), "asked, it connected");
    assert!(WAIT >= Duration::from_secs(11), "atspid's own bound is 10 s");
    let _ = std::fs::remove_file(&path);
}

#[test]
fn the_cache_follows_the_window_and_maps_ids_back() {
    let mut f = Fetch::default();
    let tree: UiWindow = serde_json::from_str(r#"{"window":"w7","app":"brave","title":"Formular","pid":4242,"source":"atspi","focused":true,"rect":[90,40,800,600],"elements":[{"id":"w7/3","role":"button","name":"Senden","rect":[190,170,80,24],"actions":["press"],"children":[]},{"id":"w7/4","role":"text","name":"weit weg","rect":[5000,5000,10,10],"children":[]}]}"#).unwrap();
    f.keep(need(), Ok((tree, true)));
    assert!(f.loading("w7"), "atspid said the page still loads");
    // The window moved 10 px right since the tree was read; scale 2 doubles every pixel.
    let content = Rect { x: 100, y: 40, w: 800, h: 600 };
    let v = f.elements("w7", 4242, [100, 40], content, 2.0).expect("cached");
    assert_eq!(v[0].rect, [400, 340, 160, 48]);
    assert!(v[0].visible && !v[1].visible, "outside the window's content is not shown");
    assert!(f.elements("w7", 99, [100, 40], content, 2.0).is_none(), "another process under the same id");
    let mut w = UiWindow { window: "w7".into(), app: "brave".into(), title: String::new(), pid: 4242, source: uilist::Source::Atspi, focused: true, rect: [0; 4], elements: v };
    w.number();
    assert_eq!(f.provider_id("w7", 4242, &w.elements[0].id).as_deref(), Some("w7/3"));
    f.keep(need(), Err("weg".into()));
    assert!(f.elements("w7", 4242, [100, 40], content, 2.0).is_none(), "a failed read leaves only the picture");
}

#[test]
fn atspid_says_who_writes_the_text() {
    // EINGABE: the program's EditableText wrote it (as before); none: the field has the focus, the desktop writes.
    assert_eq!(acted(&serde_json::json!({"id": 1, "ok": true})), Ok(false));
    assert_eq!(acted(&serde_json::json!({"id": 1, "ok": true, "by": "desktop"})), Ok(true));
    assert_eq!(acted(&serde_json::json!({"id": 1, "ok": false, "error": "Das Feld hat die Eingabe nicht bekommen"})), Err("Das Feld hat die Eingabe nicht bekommen".into()));
}
