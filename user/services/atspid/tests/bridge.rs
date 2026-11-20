//! End to end on the host: atspid's bus and registry, the fake program on it, C3 `tree` and `act` over the socket
//! (the same check the ktest db.atspi makes on V2).

use serde_json::{Value, json};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

struct Kill(Child);
impl Drop for Kill {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn wait_for(what: &str, mut f: impl FnMut() -> bool) {
    let end = Instant::now() + Duration::from_secs(10);
    while !f() {
        assert!(Instant::now() < end, "{what} did not happen within 10 s");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn ask(sock: &Path, op: &str, args: Value) -> Value {
    atspid::client::ask(sock, op, args).expect("an answer")
}

fn find<'a>(es: &'a [Value], name: &str) -> Option<&'a Value> {
    es.iter().find_map(|e| if e["name"] == name { Some(e) } else { find(e["children"].as_array()?, name) })
}

fn flat(es: &[Value], out: &mut Vec<Value>) {
    for e in es {
        out.push(e.clone());
        flat(e["children"].as_array().unwrap(), out);
    }
}

#[test]
fn tree_and_act_through_c3() {
    // Short: a unix socket path has about a hundred bytes.
    let dir = PathBuf::from(format!("/tmp/us-at-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let (bus, sock) = (dir.join("bus"), dir.join("c3.sock"));
    let serve = Kill(
        Command::new(env!("CARGO_BIN_EXE_atspid"))
            .args(["serve", "--no-settings", "--bus"])
            .arg(&bus)
            .arg("--socket")
            .arg(&sock)
            // This test stands in for the desktop: the only program of the session's user C3 answers.
            .arg("--peer")
            .arg(std::env::current_exe().unwrap())
            .spawn()
            .unwrap(),
    );
    wait_for("the C3 socket", || sock.exists());
    wait_for("the bus", || ask(&sock, "ping", Value::Null)["running"] == true);
    let mut fake = Kill(Command::new(env!("CARGO_BIN_EXE_atspid-fake")).args(["--child", "--bus"]).arg(&bus).stdout(Stdio::piped()).spawn().unwrap());
    let mut line = String::new();
    BufReader::new(fake.0.stdout.take().unwrap()).read_line(&mut line).unwrap();
    assert!(line.contains("embedded under :"), "the registry answered Embed: {line}");
    let pid = fake.0.id();
    let child: u32 = line.rsplit_once("child ").and_then(|(_, c)| c.trim().parse().ok()).expect("the fake's child pid");

    // Another program of the session's user (here atspid's own command line) gets one German answer, nothing more.
    let other = Command::new(env!("CARGO_BIN_EXE_atspid")).args(["tree", "--socket"]).arg(&sock).arg(pid.to_string()).output().unwrap();
    let said = String::from_utf8_lossy(&other.stdout);
    assert!(!other.status.success() && said.contains("nur für den Schreibtisch") && !said.contains("Formular"), "{said}");

    // Only the asked process itself answers: not its tree-less child, not the process that started it, unless
    // that one names one of the program's windows exactly.
    let me = std::process::id();
    for (p, title) in [(child, None), (me, None), (me, Some("Form"))] {
        let mut args = json!({"pid": p, "window": "rel"});
        if let Some(t) = title {
            args["title"] = json!(t);
        }
        let w = ask(&sock, "tree", args);
        assert_eq!(w["ok"], false, "pid {p} title {title:?} got a tree: {w}");
    }
    let launcher = ask(&sock, "tree", json!({"pid": me, "window": "rel", "title": "Formular"}));
    assert_eq!((launcher["ok"].as_bool(), launcher["title"].as_str()), (Some(true), Some("Formular")), "{launcher}");

    let w = ask(&sock, "tree", json!({"pid": pid, "window": "w7", "origin": [100, 50]}));
    assert_eq!(w["ok"], true, "{w}");
    assert_eq!((w["window"].as_str(), w["source"].as_str(), w["title"].as_str()), (Some("w7"), Some("atspi"), Some("Formular")));
    assert_eq!((w["app"].as_str(), w["pid"].as_u64(), w["focused"].as_bool()), (Some("Fake"), Some(u64::from(pid)), Some(true)));
    assert_eq!(w["rect"], json!([100, 50, 800, 600]));
    let es = w["elements"].as_array().unwrap();
    let mut all = Vec::new();
    flat(es, &mut all);
    assert!(all.iter().all(|e| e["id"].as_str().unwrap().starts_with("w7/")));
    assert!(!w.to_string().contains("geheim"), "a password never leaves");
    let pw = find(es, "Passwort").unwrap();
    assert_eq!((pw["role"].as_str(), pw.get("value")), (Some("field"), None));
    assert!(pw["state"].as_array().unwrap().contains(&json!("secret")));
    assert!(find(es, "Versteckt").is_none(), "what is not on screen is left out");
    assert!(all.iter().all(|e| e["role"] != "group" || !e["name"].as_str().unwrap().is_empty()), "an unnamed panel is lifted");
    let name = all.iter().find(|e| e["role"] == "field" && e["name"] == "Name").unwrap();
    assert_eq!((name["value"].as_str(), name["rect"].clone()), (Some("Georg"), json!([200, 60, 200, 20])));
    assert_eq!(name["state"], json!(["focused"]));
    assert_eq!(name["actions"], json!(["type", "submit", "focus"]));
    let farbe = find(es, "Farbe").unwrap();
    assert_eq!((farbe["role"].as_str(), farbe["value"].as_str()), (Some("choice"), Some("Rot")));
    assert_eq!(find(es, "Lautstärke").unwrap()["value"], "30");
    assert!(find(es, "Gesperrt").unwrap()["state"].as_array().unwrap().contains(&json!("disabled")));
    let id = |n: &str| find(es, n).unwrap()["id"].as_str().unwrap().to_string();

    let act = |el: &str, a: &str, t: Option<&str>| {
        let mut args = json!({"pid": pid, "element": el, "action": a});
        if let Some(t) = t {
            args["text"] = json!(t);
        }
        ask(&sock, "act", args)
    };
    // Only what the element offers: a toggle or a choice never turns into a press of a button.
    for (n, a) in [("Senden", "toggle"), ("Senden", "choose"), ("Senden", "type"), ("Name", "press"), ("Farbe", "toggle")] {
        let r = act(&id(n), a, Some("Grün"));
        assert_eq!(r["ok"], false, "{a} on {n}: {r}");
        assert!(r["error"].as_str().unwrap().contains("geht"), "{r}");
    }
    let unchanged = ask(&sock, "tree", json!({"pid": pid, "window": "w7"}));
    assert!(find(unchanged["elements"].as_array().unwrap(), "Status: bereit").is_some(), "nothing was done: {unchanged}");
    let elsewhere = ask(&sock, "act", json!({"pid": me, "element": id("Senden"), "action": "press"}));
    assert_eq!(elsewhere["ok"], false, "an id of w7 for another process: {elsewhere}");
    assert_eq!(act(&name["id"].as_str().unwrap().to_string(), "type", Some("Hallo"))["ok"], true);
    assert_eq!(act(&id("Merken"), "toggle", None)["ok"], true);
    assert_eq!(act(&id("Farbe"), "choose", Some("grün"))["ok"], true);
    assert_eq!(act(&id("Zwei"), "choose", None)["ok"], true);
    assert_eq!(act(&id("Lautstärke"), "type", Some("55"))["ok"], true);
    assert_eq!(act(&id("Senden"), "press", None)["ok"], true);
    let refused = act(&id("Gesperrt"), "press", None);
    assert_eq!(refused["ok"], false);
    assert!(refused["error"].as_str().unwrap().contains("abgelehnt"), "{refused}");
    assert!(act(&id("Farbe"), "choose", Some("Blau"))["error"].as_str().unwrap().contains("„Blau“"));
    assert_eq!(act("w7/9999", "press", None)["ok"], false);
    assert_eq!(act("nowindow/1", "press", None)["ok"], false);

    let w2 = ask(&sock, "tree", json!({"pid": pid, "window": "w7", "origin": [100, 50]}));
    let es2 = w2["elements"].as_array().unwrap();
    assert_eq!(find(es2, "Status: gedrückt Senden mit Hallo").map(|e| e["role"].clone()), Some(json!("text")), "{w2}");
    assert!(find(es2, "Merken").unwrap()["state"].as_array().unwrap().contains(&json!("checked")));
    assert_eq!(find(es2, "Farbe").unwrap()["value"], "Grün");
    assert!(find(es2, "Zwei").unwrap()["state"].as_array().unwrap().contains(&json!("selected")));
    assert_eq!(find(es2, "Lautstärke").unwrap()["value"], "55");
    assert_eq!(find(es2, "Senden").unwrap()["id"].as_str(), Some(id("Senden").as_str()), "ids stay stable");

    assert_eq!(ask(&sock, "tree", json!({"pid": 1}))["ok"], false, "a process without a tree is a picture");
    // A program that counts in other pixels (the size it gives is twice the desktop's): placed in the desktop's.
    let big = ask(&sock, "tree", json!({"pid": pid, "window": "w8", "origin": [100, 50], "size": [1600, 1200]}));
    assert_eq!(big["rect"], json!([100, 50, 1600, 1200]), "{big}");
    let bn = big["elements"].as_array().unwrap();
    let field = |es: &[Value]| {
        let mut all = Vec::new();
        flat(es, &mut all);
        all.into_iter().find(|e| e["role"] == "field" && e["name"] == "Name").unwrap()
    };
    assert_eq!(field(bn)["rect"], json!([300, 70, 400, 40]));
    assert_eq!(ask(&sock, "nonsense", json!({"pid": pid}))["ok"], false);
    drop(fake);
    drop(serve);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_browser_field_gets_the_focus_and_the_desktop_writes() {
    let dir = PathBuf::from(format!("/tmp/us-ab-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let (bus, sock) = (dir.join("bus"), dir.join("c3.sock"));
    let me = std::env::current_exe().unwrap();
    let serve = Kill(Command::new(env!("CARGO_BIN_EXE_atspid")).args(["serve", "--no-settings", "--bus"]).arg(&bus).arg("--socket").arg(&sock).arg("--peer").arg(&me).spawn().unwrap());
    wait_for("the C3 socket", || sock.exists());
    wait_for("the bus", || ask(&sock, "ping", Value::Null)["running"] == true);
    let mut fake = Kill(Command::new(env!("CARGO_BIN_EXE_atspid-fake")).arg("--browser").arg(&dir).arg("--bus").arg(&bus).stdout(Stdio::piped()).spawn().unwrap());
    let mut line = String::new();
    BufReader::new(fake.0.stdout.take().unwrap()).read_line(&mut line).unwrap();
    let pid = fake.0.id();
    let w = ask(&sock, "tree", json!({"pid": pid, "window": "w9"}));
    let es = w["elements"].as_array().unwrap();
    const ADDRESS: &str = "Adress- und Suchleiste";
    let url = find(es, ADDRESS).unwrap();
    assert_eq!((url["value"].as_str(), url["state"].clone()), (Some("https://example.org/"), json!(["focused", "singleline"])));
    assert_eq!(url["actions"], json!(["type", "submit", "focus"]));
    let search = find(es, "Suche").unwrap();
    assert_eq!(search["actions"], json!(["press", "type", "submit", "choose", "focus"]));
    assert_eq!(search["state"], json!(["multiline", "search"]), "Google's box says it is a search box: {search}");
    assert_eq!(search["value"], "");
    assert!(find(es, "Kommentar").unwrap()["state"].as_array().unwrap().contains(&json!("multiline")));
    let r = ask(&sock, "act", json!({"pid": pid, "element": url["id"], "action": "type", "text": "Georg"}));
    assert_eq!((r["ok"].as_bool(), r["by"].as_str()), (Some(true), Some("desktop")), "no EditableText: the desktop writes: {r}");
    assert_eq!(std::fs::read_to_string(dir.join("focus")).unwrap(), "3", "the field got the focus first");
    assert_eq!(std::fs::read_to_string(dir.join("text.3")).unwrap(), "https://example.org/", "atspid itself wrote nothing");
    let f = |n: &str| ask(&sock, "focused", json!({"pid": pid, "element": find(es, n).unwrap()["id"]}));
    assert_eq!((f(ADDRESS)["ok"].as_bool(), f(ADDRESS)["focused"].as_bool()), (Some(true), Some(true)), "the desktop's check before a paste");
    assert_eq!(f("Suche")["focused"], false);
    // EINGABE2: the page's box while the address bar has the keyboard: the first GrabFocus moves only the page's
    // focus (both say FOCUSED); atspid grabs again and answers only once the box alone has it.
    let r = ask(&sock, "act", json!({"pid": pid, "element": search["id"], "action": "type", "text": "Georg"}));
    assert_eq!(r["by"], "desktop", "{r}");
    assert_eq!(std::fs::read_to_string(dir.join("focus")).unwrap(), "4", "the keyboard really moved into the page");
    assert_eq!((f("Suche")["focused"].as_bool(), f(ADDRESS)["focused"].as_bool()), (Some(true), Some(false)));
    let sel = ask(&sock, "select", json!({"pid": pid, "element": url["id"]}));
    assert_eq!(sel["selected"], true, "{sel}");
    assert_eq!(std::fs::read_to_string(dir.join("sel.3")).unwrap(), "0 20", "all of the URL selected");
    let again = ask(&sock, "tree", json!({"pid": pid, "window": "w9"}));
    assert!(find(again["elements"].as_array().unwrap(), "Suche").unwrap()["state"].as_array().unwrap().contains(&json!("focused")));
    std::fs::write(dir.join("text.4"), "Grüße").unwrap();
    let read = ask(&sock, "tree", json!({"pid": pid, "window": "w9"}));
    assert_eq!(find(read["elements"].as_array().unwrap(), "Suche").unwrap()["value"], "Grüße", "what the window was given is read back");
    // D: a press on the tile: the title says Google at once, the tree keeps the new-tab page a moment longer.
    let tile = find(read["elements"].as_array().unwrap(), "Google").unwrap();
    assert_eq!(ask(&sock, "act", json!({"pid": pid, "element": tile["id"], "action": "press"}))["ok"], true);
    let stale = ask(&sock, "tree", json!({"pid": pid, "window": "w9", "title": "Google - Brave"}));
    assert!(find(stale["elements"].as_array().unwrap(), "Kommentar").is_some(), "without settling: the old page (the Mini-PC's fault)");
    let quick = ask(&sock, "tree", json!({"pid": pid, "window": "w9", "title": "Google - Brave", "settle": {"ms": 300}}));
    assert_eq!((quick["settled"].as_bool(), quick["loading"].as_bool()), (Some(false), Some(true)), "the desktop's new title and the page disagree at the deadline: still loading");
    let t0 = Instant::now();
    let fresh = ask(&sock, "tree", json!({"pid": pid, "window": "w9", "title": "Google - Brave", "settle": {"ms": 3000, "calm": true}}));
    let fe = fresh["elements"].as_array().unwrap();
    assert_eq!((fresh["settled"].as_bool(), fresh["loading"].as_bool()), (Some(true), Some(false)), "{fresh}");
    assert!(find(fe, "Kommentar").is_none() && find(fe, "Google Suche").is_some(), "the new page: {fresh}");
    assert!(t0.elapsed() < Duration::from_millis(2500), "settled once the page was there");
    let mut all = Vec::new();
    flat(fe, &mut all);
    assert_eq!(all.iter().filter(|e| e["name"] == "Google Suche").count(), 1, "the hidden copy in the closed suggestions is not offered");
    drop(fake);
    drop(serve);
    let _ = std::fs::remove_dir_all(&dir);
}
