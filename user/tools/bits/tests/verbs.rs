//! Every verb of bits against fake services: what it sends, the JSON shape it answers with (pinned), its exit code.

mod common;

use common::{World, keys};
use serde_json::{Value, json};

const LIST: &str = "tile=0 page=1/1 visible=true focused=true frame=0,30 1280x1410 id=3 app_id=foot title=\"Terminal\" pid=77 hung=false ws=1\nfloat=0 visible=true focused=false frame=100,100 640x480 id=5 app_id=brave title=\"Neuer Tab - Brave\" pid=88 hung=false ws=1\nend";

fn form(loading: bool) -> String {
    let els = if loading { json!([]) } else { json!([{"id": "w5/ab", "role": "field", "name": "Suche", "rect": [10, 10, 100, 20], "actions": ["type", "submit"]}, {"id": "w5/cd", "role": "button", "name": "Anmelden", "rect": [10, 40, 80, 20], "actions": ["press"]}]) };
    json!({"window": "w5", "app": "brave", "title": "Neuer Tab - Brave", "pid": 88, "source": "atspi", "focused": true, "rect": [100, 100, 640, 480], "elements": els}).to_string()
}

fn desktop(name: &str) -> World {
    let w = World::new(name);
    let reads = std::sync::atomic::AtomicUsize::new(0);
    w.comp(move |l| {
        let first = l.split_whitespace().next().unwrap_or("");
        match first {
            "list" => LIST.into(),
            "ui" if l == "ui text w5" => "w5 brave \"Neuer Tab - Brave\"\n  field \"Suche\" #w5/ab".into(),
            "ui" if l == "ui" => format!("[{}]", form(false)),
            "ui" if l == "ui w9" => "error: Ein Fenster w9 gibt es nicht.".into(),
            // The page loads for the first three reads.
            "ui" => form(reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst) < 3),
            "ui-act" if l.contains("w5/zz") => "error: Das Element w5/zz gibt es nicht mehr.".into(),
            "ui-act" => "ok".into(),
            "close" | "bits-vor" => "ok".into(),
            "bits-groesse" => "rect=10,20,800,600".into(),
            "bits-meldung" => l.trim_start_matches("bits-meldung ").into(),
            "program-start" => "started Foot (pid 9); its window appears when the program has drawn it".into(),
            "program-find" => "foot\tFoot".into(),
            "bits-clip-get" => json!({"text": "Grüße\nzwei"}).to_string(),
            "bits-clip-set" => {
                let p = l.split_whitespace().nth(1).unwrap();
                let got = std::fs::read_to_string(p).unwrap_or_default();
                std::fs::write(format!("{p}.seen"), &got).unwrap();
                "ok".into()
            }
            "bits-foto" => {
                let p = l.split_whitespace().nth(1).unwrap();
                std::fs::write(p, b"P6\n2 1\n255\n\x01\x02\x03\x04\x05\x06").unwrap();
                "2x1".into()
            }
            _ => format!("error: unknown command '{first}'"),
        }
    });
    w
}

#[test]
fn fenster_lists_and_changes_windows() {
    let w = desktop("fenster");
    let (code, v) = w.json(&["fenster", "liste"]);
    assert_eq!(code, 0);
    assert_eq!(keys(&v), ["ok", "windows"]);
    assert_eq!(keys(&v["windows"][0]), ["app", "focused", "fullscreen", "hung", "id", "max", "pid", "place", "rect", "title", "visible", "workspace"]);
    assert_eq!(v["windows"][1]["id"], "w5");
    assert_eq!(v["windows"][1]["place"], "float");
    assert_eq!(w.json(&["fenster", "vor", "w5"]), (0, json!({"ok": true, "window": "w5"})));
    assert_eq!(w.json(&["fenster", "schliessen", "3"]), (0, json!({"ok": true, "window": "w3"})));
    assert_eq!(w.json(&["fenster", "groesse", "w5", "800", "600"]), (0, json!({"ok": true, "window": "w5", "rect": [10, 20, 800, 600]})));
    assert_eq!(w.json(&["fenster", "groesse", "w5", "voll"]).0, 0);
    assert_eq!(w.json(&["fenster", "groesse", "w5", "breit"]).0, 2);
    let sent = World::sent(&w.ctl);
    assert!(sent.contains(&"bits-vor 5".to_string()) && sent.contains(&"close 3".to_string()) && sent.contains(&"bits-groesse 5 800 600".to_string()) && sent.contains(&"bits-groesse 5 voll".to_string()), "{sent:?}");
    let o = w.run_env(&["fenster"], &[], None);
    assert!(String::from_utf8_lossy(&o.stdout).contains("* w3"), "the text marks the focused window");
}

#[test]
fn ui_lists_acts_and_waits_until_settled() {
    let w = desktop("ui");
    let (code, v) = w.json(&["ui", "liste"]);
    assert_eq!((code, keys(&v)), (0, vec!["ok".to_string(), "windows".to_string()]));
    let (_, v) = w.json(&["ui", "liste", "5"]);
    assert_eq!(keys(&v), ["ok", "window"]);
    let o = w.run_env(&["ui", "liste", "w5"], &[], None);
    assert!(String::from_utf8_lossy(&o.stdout).contains("#w5/ab"), "the text form for people");
    assert_eq!(w.json(&["ui", "liste", "w9"]).0, 1);
    assert_eq!(w.json(&["ui", "tun", "w5/ab", "type", "Hallo", "Welt"]), (0, json!({"ok": true, "element": "w5/ab", "action": "type", "answer": "ok"})));
    let (code, v) = w.json(&["ui", "tun", "w5/zz", "press"]);
    assert_eq!((code, v["error"].as_str()), (1, Some("failed")));
    assert_eq!(w.json(&["ui", "tun", "w5/ab", "kick"]).0, 2);
    w.json(&["ui", "tun", "w5/cd", "press", "--rueckgaengig"]);
    w.json(&["ui", "tun", "--endgueltig", "w5/cd", "press"]);
    assert_eq!(w.json(&["ui", "tun", "w5/cd", "press", "--endgueltig", "--rueckgaengig"]).0, 2, "both words: usage");
    let sent = World::sent(&w.ctl);
    assert!(sent.contains(&"ui-act w5/ab type Hallo Welt".to_string()), "{sent:?}");
    assert!(sent.contains(&"bits-schritt rueckgaengig ui-act w5/cd press".to_string()), "{sent:?}");
    assert!(sent.contains(&"bits-schritt endgueltig ui-act w5/cd press".to_string()), "{sent:?}");
    assert_eq!(sent.iter().filter(|l| l.contains("w5/cd press")).count(), 2, "the usage error sent nothing: {sent:?}");
    // The button shows from the fourth read on; two equal reads 0.3 s apart end the wait.
    let (code, v) = w.json(&["ui", "warten", "--fenster", "Brave", "--text", "anmelden", "--bis-zu", "10"]);
    assert_eq!(code, 0, "{v}");
    assert_eq!(keys(&v), ["element", "ok", "waited_ms", "window"]);
    assert_eq!(v["element"]["id"], "w5/cd");
    assert_eq!(v["window"]["id"], "w5");
    let ui_reads = World::sent(&w.ctl).iter().filter(|l| *l == "ui w5").count();
    assert!(ui_reads >= 5, "read until settled: {ui_reads}");
    let (code, v) = w.json(&["ui", "warten", "--fenster", "gimp", "--bis-zu", "0.5"]);
    assert_eq!((code, v["error"].as_str()), (4, Some("timeout")));
    assert!(v["message"].as_str().unwrap().contains("gimp"));
    assert_eq!(w.json(&["ui", "warten", "--fenster", "gimp", "--weg", "--bis-zu", "2"]).0, 0);
    assert_eq!(w.json(&["ui", "warten"]).0, 2);
}

#[test]
fn clipboard_picture_notice_and_programs() {
    let w = desktop("clip");
    assert_eq!(w.json(&["zwischenablage", "lesen"]), (0, json!({"ok": true, "text": "Grüße\nzwei"})));
    let o = w.run_env(&["zwischenablage", "lesen"], &[], None);
    assert_eq!(String::from_utf8_lossy(&o.stdout), "Grüße\nzwei\n");
    assert_eq!(w.json(&["zwischenablage", "setzen", "eins", "zwei"]), (0, json!({"ok": true, "chars": 9})));
    let o = w.run_env(&["--json", "zwischenablage", "setzen", "-"], &[], Some("aus der Eingabe\n"));
    assert_eq!(o.status.code(), Some(0));
    let seen: Vec<String> = std::fs::read_dir(w.dir.join("run")).unwrap().filter_map(Result::ok).map(|e| e.file_name().to_string_lossy().into_owned()).collect();
    assert!(!seen.iter().any(|n| n.starts_with(".bits-clip-") && !n.ends_with(".seen")), "the text file is gone: {seen:?}");
    let texts: Vec<String> = seen.iter().filter(|n| n.ends_with(".seen")).map(|n| std::fs::read_to_string(w.dir.join("run").join(n)).unwrap()).collect();
    assert!(texts.contains(&"eins zwei".to_string()) && texts.contains(&"aus der Eingabe\n".to_string()), "{texts:?}");
    let png = w.dir.join("bild.png");
    let (code, v) = w.json(&["foto", "--fenster", "5", png.to_str().unwrap()]);
    assert_eq!((code, keys(&v)), (0, vec!["file".to_string(), "height".into(), "ok".into(), "width".into()]));
    assert_eq!((v["width"].as_u64(), v["height"].as_u64()), (Some(2), Some(1)));
    assert_eq!(&std::fs::read(&png).unwrap()[..8], b"\x89PNG\r\n\x1a\n");
    assert!(World::sent(&w.ctl).iter().any(|l| l.starts_with("bits-foto ") && l.ends_with(" w5")));
    assert_eq!(w.json(&["meldung", "Fertig", "kopiert"]), (0, json!({"ok": true, "shown": "Fertig kopiert"})));
    assert_eq!(w.json(&["meldung"]).0, 2);
    let (code, v) = w.json(&["programm", "start", "Foot"]);
    assert_eq!((code, keys(&v)), (0, vec!["answer".to_string(), "ok".into()]));
    assert_eq!(w.json(&["programm", "finde", "foot"]), (0, json!({"ok": true, "id": "foot", "name": "Foot"})));
}

#[test]
fn settings_go_through_settingsd() {
    let w = World::new("settings");
    w.settingsd(|r| match r["op"].as_str() {
        Some("list") => json!({"ok": true, "keys": [{"key": "desktop.theme", "type": "choice", "value": "hell", "changed": false, "text": "Farben"}, {"key": "ai.provider.a.key", "type": "text", "value": "***", "changed": true, "text": "Schlüssel"}]}),
        Some("get") if r["key"] == "nix" => json!({"ok": false, "error": "unknown_key", "message": "nix: unknown key"}),
        Some("get") => json!({"ok": true, "key": r["key"], "value": "hell", "changed": false}),
        Some("set") if r["key"] == "boot.x" => json!({"ok": false, "error": "needs_confirm", "message": "boot.x: ask Georg first"}),
        Some("set") => json!({"ok": true, "key": r["key"], "old": "hell", "new": r["value"], "applied": "now", "restart": "none", "seq": 4, "stored": "stick"}),
        _ => json!({"ok": false, "error": "bad_request", "message": "?"}),
    });
    let (code, v) = w.json(&["einstellungen", "liste", "desktop."]);
    assert_eq!(code, 0);
    assert_eq!(keys(&v["keys"][0]), ["changed", "key", "text", "type", "value"]);
    assert_eq!(v["keys"][1]["value"], "***", "a secret stays ***");
    assert_eq!(w.json(&["einstellungen", "lesen", "desktop.theme"]), (0, json!({"ok": true, "key": "desktop.theme", "value": "hell", "changed": false})));
    let (code, v) = w.json(&["einstellungen", "lesen", "nix"]);
    assert_eq!((code, v["error"].as_str()), (1, Some("not_found")));
    assert_eq!(w.json(&["einstellungen", "setzen", "desktop.theme", "dunkel"]), (0, json!({"ok": true, "key": "desktop.theme", "old": "hell", "value": "dunkel"})));
    assert_eq!(w.json(&["einstellungen", "setzen", "boot.x", "1"]).0, 5, "needs_confirm is a refusal");
    w.json(&["einstellungen", "setzen", "desktop.theme", "dunkel", "--bestaetigt"]);
    let sent: Vec<Value> = World::sent(&w.settings).iter().map(|l| serde_json::from_str(l).unwrap()).collect();
    let sets: Vec<&Value> = sent.iter().filter(|r| r["op"] == "set").collect();
    assert_eq!(sets[0]["confirmed"], false);
    assert_eq!(sets[0]["source"], "bits");
    assert_eq!(sets.last().unwrap()["confirmed"], true, "Georg's own terminal may say he agreed");
    assert_eq!(w.json(&["einstellungen", "setzen", "x"]).0, 2);
}

#[test]
fn workspaces_and_files_through_ctxd() {
    let w = World::new("ctxd");
    w.ctxd(|r| {
        let (op, args) = (r["op"].as_str().unwrap_or(""), &r["args"]);
        let result = match op {
            "context_search" => json!({"hits": [{"path": "/data/home/1000/Brief.odt", "title": "Brief", "snippet": "…"}], "query": args["query"]}),
            "workspace_list" => json!({"workspaces": [{"id": "ws1", "name": "Steuer", "pinned": true, "items": 3, "last_used": "2026-11-17"}]}),
            "workspace_open" => json!({"workspace": {"id": "ws1", "name": args["name"]}}),
            "workspace_pin" => json!({"id": args["workspace"], "pinned": args["pinned"]}),
            "workspace_add" => json!({"added": args["paths"], "workspace": args["workspace"]}),
            "workspace_rename" => json!({"id": args["workspace"], "name": args["name"]}),
            "mcp" => json!({"jsonrpc": "2.0", "id": 1, "result": {"content": [{"type": "text", "text": "…"}], "structuredContent": {"hits": [], "via": "mcp"}, "isError": false}}),
            _ => return json!({"id": r["id"], "ok": false, "error": {"code": "bad_request", "message": "unknown op"}}),
        };
        json!({"id": r["id"], "ok": true, "result": result})
    });
    let (code, v) = w.json(&["dateien", "suchen", "Brief", "an", "--anzahl", "3"]);
    assert_eq!((code, keys(&v)), (0, vec!["ok".to_string(), "result".into()]));
    assert_eq!(v["result"]["hits"][0]["title"], "Brief");
    let (_, v) = w.json(&["arbeitsraum", "liste"]);
    assert_eq!(v["result"]["workspaces"][0]["name"], "Steuer");
    assert_eq!(w.json(&["arbeitsraum", "oeffnen", "Steuer", "2026"]).1["result"]["workspace"]["name"], "Steuer 2026");
    assert_eq!(w.json(&["arbeitsraum", "anheften", "ws1", "--los"]).1["result"]["pinned"], false);
    let doc = w.dir.join("brief.txt");
    std::fs::write(&doc, "x").unwrap();
    let (_, v) = w.json(&["arbeitsraum", "hinzufuegen", "ws1", doc.to_str().unwrap()]);
    assert_eq!(v["result"]["added"], json!([doc.canonicalize().unwrap()]), "the file by its full path");
    assert_eq!(w.json(&["arbeitsraum", "umbenennen", "ws1", "Steuer", "2027"]).1["result"]["name"], "Steuer 2027");
    assert_eq!(w.json(&["arbeitsraum", "umbenennen", "ws1"]).0, 2);
    let (_, v) = w.tree(&["dateien", "suchen", "Brief"]);
    assert_eq!(v["result"]["via"], "mcp", "from the assistant's tree through ctxd's MCP door");
    let sent: Vec<Value> = World::sent(&w.ctxd).iter().map(|l| serde_json::from_str(l).unwrap()).collect();
    assert_eq!(sent[0]["args"], json!({"query": "Brief an", "limit": 3}));
    let mcp = sent.iter().find(|r| r["op"] == "mcp").unwrap();
    assert_eq!(mcp["args"]["params"]["name"], "context_search");
    assert_eq!(w.json(&["dateien", "suchen"]).0, 2);
}

#[test]
fn measurements_by_name() {
    let w = World::new("metrics");
    std::fs::write(w.dir.join("metrics"), "cpu.0.khz 3800000 kHz\ncpu.1.khz 400000 kHz\npower.package 152 dW\ncrash.programs 0 n\n").unwrap();
    let (code, v) = w.json(&["messwerte"]);
    assert_eq!((code, v["values"].as_array().unwrap().len()), (0, 4));
    assert_eq!(keys(&v["values"][0]), ["name", "unit", "value"]);
    let (_, v) = w.json(&["messwerte", "lesen", "cpu.*", "crash.programs"]);
    assert_eq!(v["values"].as_array().unwrap().iter().map(|x| x["name"].as_str().unwrap()).collect::<Vec<_>>(), ["cpu.0.khz", "cpu.1.khz", "crash.programs"]);
    assert_eq!(w.json(&["messwerte", "lesen", "gpu.*"]).0, 1);
    std::fs::remove_file(w.dir.join("metrics")).unwrap();
    assert_eq!(w.json(&["messwerte"]).0, 3);
}

#[test]
fn the_scripting_set_through_bitspkgd() {
    let w = World::new("pkg");
    w.pkgd(|r| match r["op"].as_str() {
        Some("list") => vec![json!({"ok": true, "programs": [{"name": "jq", "current": "e0_1.8.2-1+1", "previous": null, "versions": ["e0_1.8.2-1+1"], "id": 4}]})],
        Some("plan") => vec![json!({"ok": true, "name": r["name"], "installable": true, "download": 10_485_760, "packages": [{"name": "glibc", "csize": 5_000_000}, {"name": r["name"], "csize": 5_485_760}], "summary": format!("{}: 3 Teile", r["name"].as_str().unwrap()), "plan_id": format!("p-{}", r["name"].as_str().unwrap())})],
        Some("do") => vec![json!({"progress": "download", "done_bytes": 1}), json!({"ok": true, "done": true, "program": r["plan_id"].as_str().unwrap().trim_start_matches("p-"), "report": ["fertig"]})],
        _ => vec![json!({"ok": false, "error": "bad_request", "message": "?"})],
    });
    let (code, v) = w.json(&["programm", "satz"]);
    assert_eq!(code, 0);
    assert_eq!(keys(&v), ["download", "ok", "programs", "set"]);
    assert_eq!(v["download"], 5_000_000 + 4 * 5_485_760, "jq is there, four programs to load, glibc once");
    assert_eq!(v["programs"].as_array().unwrap().iter().map(|p| p["program"].as_str().unwrap()).collect::<Vec<_>>(), ["python", "git", "curl", "jq", "ripgrep"]);
    assert_eq!(v["programs"][3]["installed"], "e0_1.8.2-1+1");
    assert_eq!(keys(&v["programs"][0]), ["commands", "download", "installable", "installed", "program", "summary"]);
    let (code, v) = w.json(&["programm", "satz", "skripte", "--installieren"]);
    assert_eq!(code, 0, "{v}");
    assert_eq!(v["programs"][0]["installed"], "python");
    assert_eq!(keys(&v), ["download", "ok", "programs", "set"]);
    assert_eq!(keys(&v["programs"][0]), ["commands", "download", "installable", "installed", "program", "report", "summary"]);
    assert_eq!(v["programs"][0]["report"], json!(["fertig"]));
    assert_eq!(keys(&v["programs"][3]), ["commands", "installed", "program"], "an installed one");
    assert!(World::sent(&w.pkg).iter().any(|l| l.contains("\"plan_id\":\"p-ripgrep\"")));
    assert_eq!(w.tree(&["programm", "satz", "--installieren"]).0, 5, "never from the assistant's tree");
    assert_eq!(w.json(&["programm", "satz", "spiele"]).0, 2);
    let (code, v) = w.json(&["programm", "befehle"]);
    assert_eq!((code, keys(&v)), (0, vec!["commands".to_string(), "ok".into()]));
    assert_eq!(v["commands"][0], json!({"program": "python", "commands": ["python3", "python"], "installed": null}));
}

#[test]
fn help_errors_and_the_envelope() {
    let w = World::new("help");
    let (code, v) = w.json(&["hilfe"]);
    assert_eq!((code, keys(&v)), (0, vec!["ok".to_string(), "text".into(), "topic".into(), "topics".into()]));
    assert!(v["topics"].as_array().unwrap().iter().any(|t| t == "papierkorb"));
    let (_, v) = w.json(&["hilfe", "ui"]);
    assert_eq!((keys(&v), v["topic"].as_str()), (vec!["ok".to_string(), "text".into(), "topic".into()], Some("ui")));
    assert!(v["text"].as_str().unwrap().starts_with("## ui"));
    assert_eq!(w.json(&["hilfe", "kaffee"]).0, 2);
    let (code, v) = w.json(&["kaffee"]);
    assert_eq!((code, keys(&v)), (2, vec!["error".to_string(), "message".into(), "ok".into()]));
    let (code, v) = w.json(&["fenster", "liste"]);
    assert_eq!((code, v["error"].as_str()), (3, Some("unreachable")), "no desktop runs here");
    let o = w.run_env(&["fenster", "liste"], &[], None);
    assert!(o.stdout.is_empty() && String::from_utf8_lossy(&o.stderr).starts_with("bits: "), "without --json the error goes to stderr");
    for topic in ["fenster", "ui", "einstellungen", "arbeitsraum", "dateien", "zwischenablage", "foto", "meldung", "messwerte", "programm", "papierkorb"] {
        assert!(bits_md().contains(&format!("## {topic}\n")), "BITS.md documents {topic}");
    }
}

fn bits_md() -> String {
    std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/BITS.md")).unwrap()
}

#[path = "parts/tree.rs"]
mod tree;
#[path = "parts/trash.rs"]
mod trash;
