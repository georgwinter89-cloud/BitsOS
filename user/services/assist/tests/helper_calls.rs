//! Version 4 of the helper contract (HELPER.md, "assist's tools for the helper's model"): a helper gets the chat's
//! tools in its request and calls them through the answer's tool socket; assist runs each call as it runs an HTTP
//! provider's (the same permission step, tool lines, results, round limit). Small helper scripts behind a real
//! engine and a real settingsd; the socket is spoken to by a few lines of Python (the test process, not `assist`,
//! listens here, so `assist --call` would rightly refuse it; tests/cli.rs-style runs of the real program check
//! `assist --call`). No real provider.

mod common;
use assist::conv::Stop;
use assist::engine::{Answer, Session};
use assist::json::{self, Json};
use assist::tools::settings::SettingsTools;
use common::Rec;
use common::stage::{Stage, calling};
use std::process::{Command, Stdio};

const BUDGET: usize = 64 << 20;
const DONE: &str = r#"echo '{"done":true,"stop":"end","error":null}'"#;

fn lines(s: &Stage, file: &str) -> Vec<Json> {
    s.file(file).lines().filter(|l| !l.trim().is_empty()).map(|l| json::parse(l).unwrap_or_else(|e| panic!("{e}: {l}"))).collect()
}

fn ask(s: &Stage, question: &str, answer: Answer) -> (Result<Stop, String>, Rec) {
    let tools = SettingsTools { settings: &s.w };
    let mut session = Session::new(&s.w, &tools, BUDGET);
    let mut ui = Rec::new(answer);
    let r = session.ask(question, &mut ui);
    (r, ui)
}

fn request(s: &Stage) -> Json {
    json::parse(s.file("request").trim()).unwrap()
}

#[test]
fn a_helper_gets_the_chat_s_tools_and_calls_one() {
    let s = Stage::new("helper4-call");
    s.helper(&format!(
        "{}call '{{\"id\":\"l\",\"list\":true}}' > list\ncall '{{\"id\":\"c1\",\"name\":\"settings_get\",\"input\":{{\"key\":\"theme.scale\"}}}}' > got\necho '{{\"text\":\"Gelesen.\"}}'\n{DONE}",
        calling(&s)
    ));
    let tools = SettingsTools { settings: &s.w };
    let mut session = Session::new(&s.w, &tools, BUDGET);
    let mut ui = Rec::new(Answer::CannotAsk);
    let r = session.ask("Wie groß ist alles?", &mut ui);
    assert_eq!((r, ui.text.as_str()), (Ok(Stop::End), "Gelesen."), "{:?}", ui.logs);
    let req = request(&s);
    assert_eq!((req.get("bitsos_helper").and_then(Json::as_i64), req.str("level")), (Some(5), Some("ask")), "version 5 names the level");
    let names: Vec<String> = match req.get("tools") {
        Some(Json::Arr(a)) => a.iter().filter_map(|t| t.str("name").map(str::to_string)).collect(),
        other => panic!("no tools in the request: {other:?}"),
    };
    assert!(names.contains(&"settings_get".to_string()) && names.contains(&"settings_set".to_string()), "{names:?}");
    let get = match req.get("tools") {
        Some(Json::Arr(a)) => a.iter().find(|t| t.str("name") == Some("settings_get")).cloned().unwrap(),
        _ => unreachable!(),
    };
    assert!(get.get("schema").is_some() && get.str("class") == Some("read") && get.str("description").is_some_and(|d| !d.is_empty()), "{}", get.to_text());
    let sock = req.str("tool_socket").unwrap().to_string();
    assert!(sock.contains("assist-tools-") && !std::path::Path::new(&sock).exists(), "the socket is gone with the answer: {sock}");
    assert!(req.str("bell").is_some() && req.get("own_tools").is_none(), "the chat keeps the helper's own tools");
    // What the chat's prompt says about its tools comes apart, for a helper that hands them on.
    assert!(req.str("tools_system").is_some_and(|t| t.contains("settings_* tools")), "{:?}", req.str("tools_system"));
    assert!(req.str("system").is_some_and(|t| t.contains("BitsOS") && !t.contains("settings_")), "{:?}", req.str("system"));
    let list = &lines(&s, "list")[0];
    assert_eq!((list.str("id"), list.get("tools").map(Json::to_text)), (Some("l"), req.get("tools").map(Json::to_text)), "the socket lists exactly the offered tools");
    let got = &lines(&s, "got")[0];
    assert_eq!(got.str("id"), Some("c1"));
    assert!(got.str("result").is_some_and(|r| r.contains("125")) && got.get("error").is_none(), "{}", got.to_text());
    assert_eq!(ui.tools.len(), 2, "the same tool lines as for an HTTP provider: {:?}", ui.tools);
    assert!(ui.tools[0].starts_with("settings_get:Runs:") && ui.tools[1].starts_with("settings_get:Done:"), "{:?}", ui.tools);
    // Only the text of the answer is kept, as before.
    assert_eq!(session.ask("Und?", &mut Rec::new(Answer::CannotAsk)), Ok(Stop::End));
    let second = request(&s);
    assert_eq!(second.get("messages").map(Json::to_text).as_deref(), Some(r#"[{"role":"user","text":"Wie groß ist alles?"},{"role":"assistant","text":"Gelesen."},{"role":"user","text":"Und?"}]"#));
}

#[test]
fn a_changing_call_asks_georg_as_for_an_http_provider() {
    for (answer, value, said) in [(Answer::Yes, 150, None), (Answer::No, 125, Some("Georg said no")), (Answer::CannotAsk, 125, Some("nobody can be asked"))] {
        let s = Stage::new(&format!("helper4-change-{answer:?}"));
        s.helper(&format!("{}call '{{\"id\":7,\"name\":\"settings_set\",\"input\":{{\"key\":\"theme.scale\",\"value\":150}}}}' > got\necho '{{\"text\":\"ok\"}}'\n{DONE}", calling(&s)));
        let (r, ui) = ask(&s, "Mach alles größer", answer);
        assert_eq!(r, Ok(Stop::End), "{answer:?} {:?}", ui.logs);
        assert_eq!(ui.asked.len(), 1, "{answer:?}: one question, the engine's own: {:?}", ui.asked);
        assert!(ui.asked[0].contains("theme.scale") && ui.asked[0].contains("150"), "{:?}", ui.asked);
        assert_eq!(s.w.value("theme.scale"), Json::Int(value), "{answer:?}");
        let got = &lines(&s, "got")[0];
        assert_eq!(got.str("id"), Some("7"), "a number as id comes back as its digits");
        match said {
            None => assert!(got.str("result").is_some(), "{}", got.to_text()),
            Some(words) => assert!(got.str("error").is_some_and(|e| e.contains(words)), "{answer:?}: {}", got.to_text()),
        }
    }
}

#[test]
fn only_what_was_offered_runs() {
    let s = Stage::new("helper4-unoffered");
    // Nur antworten: nothing that changes is offered.
    s.w.set("ai.level", "answer");
    s.helper(&format!(
        "{}call '{{\"id\":\"a\",\"name\":\"Bash\",\"input\":{{\"command\":\"ls\"}}}}' >> got\ncall '{{\"id\":\"b\",\"name\":\"settings_set\",\"input\":{{\"key\":\"theme.scale\",\"value\":150}}}}' >> got\ncall '{{\"id\":\"c\",\"name\":\"settings_get\",\"input\":{{\"key\":7}}}}' >> got\ncall 'kein json' >> got\ncall '{{\"id\":\"e\",\"name\":\"settings_get\",\"input\":\"theme.scale\"}}' >> got\necho '{{\"text\":\"ok\"}}'\n{DONE}",
        calling(&s)
    ));
    let (r, ui) = ask(&s, "Versuch es", Answer::Yes);
    assert_eq!(r, Ok(Stop::End), "{:?}", ui.logs);
    let got = lines(&s, "got");
    assert_eq!(got.len(), 5, "{got:?}");
    assert!(got[0].str("error").is_some_and(|e| e.contains("unknown tool \"Bash\"")), "{}", got[0].to_text());
    assert!(got[1].str("error").is_some_and(|e| e.contains("Nur antworten")), "a changing tool at Nur antworten is not offered, and refused with why: {}", got[1].to_text());
    assert!(!request(&s).get("tools").map(Json::to_text).unwrap_or_default().contains("settings_set"));
    assert!(got[2].str("error").is_some_and(|e| e.contains("invalid arguments")), "the schema is checked: {}", got[2].to_text());
    assert!(got[3].str("error").is_some_and(|e| e.contains("not JSON")), "{}", got[3].to_text());
    assert!(got[4].str("error").is_some_and(|e| e.contains("INVALID_JSON")), "input that is no object: {}", got[4].to_text());
    assert!(ui.asked.is_empty(), "{:?}", ui.asked);
    assert_eq!(s.w.value("theme.scale"), Json::Int(125));
}

#[test]
fn the_round_limit_holds() {
    let s = Stage::new("helper4-rounds");
    s.w.set("ai.tool_rounds", 2i64);
    let get = "call '{\"id\":\"x\",\"name\":\"settings_get\",\"input\":{\"key\":\"theme.scale\"}}' >> got\n";
    s.helper(&format!("{}{get}{get}{get}echo '{{\"text\":\"zu viel\"}}'\n{DONE}", calling(&s)));
    let (r, ui) = ask(&s, "Lies dreimal", Answer::Yes);
    let e = r.unwrap_err();
    assert!(e.contains("mehr als 2 Werkzeug-Runden") && e.contains("ai.tool_rounds"), "the chat's own words: {e}");
    let got = lines(&s, "got");
    assert!(got[0].str("result").is_some() && got[1].str("result").is_some(), "{got:?}");
    assert!(got.get(2).is_none_or(|g| g.str("error").is_some()), "the third never ran: {got:?}");
    assert_eq!(ui.tools.iter().filter(|t| t.contains(":Done:")).count(), 2, "{:?}", ui.tools);
}

#[test]
fn a_stranger_and_a_late_call_are_refused() {
    let s = Stage::new("helper4-stranger");
    let get = r#"'{"id":"s","name":"settings_get","input":{"key":"theme.scale"}}'"#;
    s.helper(&format!(
        "{}( ( sleep 0.3; call {get} > orphan 2> orphan.err ) & )\nsetsid python3 -I client.py \"$sock\" {get} > session 2> session.err\nsleep 0.8\npython3 -I client.py \"$sock\" {get} 2.5 > late 2> late.err &\nsleep 0.3\ncall {get} > own\necho '{{\"text\":\"ok\"}}'\n{DONE}",
        calling(&s)
    ));
    let (r, ui) = ask(&s, "Lies", Answer::Yes);
    assert_eq!(r, Ok(Stop::End), "{:?}", ui.logs);
    assert!(lines(&s, "own")[0].str("result").is_some(), "{}", s.file("own"));
    assert!(lines(&s, "orphan").is_empty(), "a process that lost its parent is turned away: {} {}", s.file("orphan"), s.file("orphan.err"));
    assert!(lines(&s, "session").is_empty(), "a descendant in a session of its own is turned away: {} {}", s.file("session"), s.file("session.err"));
    let logs = ui.logs.join("\n");
    assert!(logs.contains("tool socket: turned away") && logs.contains("does not descend from the helper"), "{logs}");
    assert!(logs.contains("is not in the helper's process group"), "{logs}");
    // Connected during the answer, called after it: refused, nothing ran.
    let late = (0..100).find_map(|_| {
        let l = lines(&s, "late");
        if l.is_empty() {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        l.into_iter().next()
    });
    let late = late.unwrap_or_else(|| panic!("the late caller got an answer: {}", s.file("late.err")));
    assert_eq!(late.str("error"), Some(assist::provider::program::calls::ENDED), "{}", late.to_text());
    assert_eq!(ui.tools.iter().filter(|t| t.starts_with("settings_get:Done")).count(), 1, "only the helper's own call ran: {:?}", ui.tools);
}

#[test]
fn esc_ends_the_helper_and_its_calls() {
    let s = Stage::new("helper4-esc");
    let get = r#"'{"id":"e","name":"settings_get","input":{"key":"theme.scale"}}'"#;
    s.helper(&format!("{}echo '{{\"text\":\"Moment\"}}'\nsleep 0.3\ncall {get} > got\nsleep 30\n{DONE}", calling(&s)));
    let tools = SettingsTools { settings: &s.w };
    let mut session = Session::new(&s.w, &tools, BUDGET);
    let mut ui = Rec::new(Answer::Yes);
    ui.cancel_at = Some(1);
    let t0 = std::time::Instant::now();
    assert!(session.ask("Lies", &mut ui).is_err());
    assert!(t0.elapsed().as_secs() < 10, "Esc ended it, not the sleep");
    assert!(ui.tools.is_empty(), "nothing ran after Esc: {:?}", ui.tools);
}

#[test]
fn without_offered_tools_there_is_no_socket() {
    // `assist --test`'s single request offers no tools: a helper of any version sees what it always saw.
    let s = Stage::new("helper4-none");
    s.w.listen();
    s.helper(&format!("cat > request\necho '{{\"text\":\"ok\"}}'\n{DONE}"));
    let out = Command::new(env!("CARGO_BIN_EXE_assist")).arg("--socket").arg(&s.w.socket).args(["--test", "h"]).stdin(Stdio::null()).output().unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));
    let req = request(&s);
    assert!(req.get("tools").is_none() && req.get("tool_socket").is_none(), "{}", req.to_text());
}

#[test]
fn assist_call_speaks_to_the_socket_of_the_assistant_it_descends_from() {
    let s = Stage::new("helper4-cli");
    s.w.listen();
    let assist = env!("CARGO_BIN_EXE_assist");
    s.helper(&format!(
        "read -r request\nsock=$(printf '%s' \"$request\" | sed -n 's/.*\"tool_socket\":\"\\([^\"]*\\)\".*/\\1/p')\nprintf '%s\\n' '{{\"id\":1,\"list\":true}}' '{{\"id\":2,\"name\":\"settings_get\",\"input\":{{\"key\":\"theme.scale\"}}}}' | '{assist}' --call --tool-socket \"$sock\" > got 2> got.err\necho '{{\"text\":\"ok\"}}'\n{DONE}"
    ));
    let out = Command::new(assist).arg("--socket").arg(&s.w.socket).args(["--once", "Lies"]).stdin(Stdio::null()).output().unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));
    let got = lines(&s, "got");
    assert_eq!(got.len(), 2, "{}", s.file("got.err"));
    assert!(got[0].get("tools").is_some() && got[1].str("result").is_some_and(|r| r.contains("125")), "{got:?}");
    // In this test's own engine the listener is the test, not the program `assist`: refused, nothing said.
    let (r, _) = ask(&s, "Lies", Answer::Yes);
    assert_eq!(r, Ok(Stop::End));
    assert!(s.file("got").trim().is_empty() && s.file("got.err").contains("not the assistant"), "{}", s.file("got.err"));
}

/// Version 5: a call whose tool brought a picture (`foto`, `sehen` with a picture) answers with it on the socket,
/// so a helper can hand it to its model.
#[test]
fn a_picture_a_call_brought_goes_back_on_the_socket() {
    use assist::conv::Image;
    use assist::tools::{Class, Grant, ToolAnswer, ToolSource, ToolSpec};
    use std::cell::RefCell;
    struct Camera(RefCell<Vec<Image>>);
    impl ToolSource for Camera {
        fn list(&self) -> Vec<ToolSpec> {
            vec![ToolSpec { name: "foto".into(), description: "A picture.".into(), schema: json::parse(r#"{"type":"object","properties":{}}"#).unwrap(), class: Class::Read }]
        }
        fn call(&self, _name: &str, _args: &Json, _grant: Grant) -> ToolAnswer {
            self.0.borrow_mut().push(Image { media_type: "image/png".into(), base64: "iVBORw0KGgo=".into() });
            Ok("ein Foto vom Bildschirm".into())
        }
        fn take_pictures(&self) -> Vec<Image> {
            std::mem::take(&mut *self.0.borrow_mut())
        }
    }
    let s = Stage::new("helper5-picture");
    s.helper(&format!("{}call '{{\"id\":\"f\",\"name\":\"foto\",\"input\":{{}}}}' > got\necho '{{\"text\":\"Gesehen.\"}}'\n{DONE}", calling(&s)));
    let tools = Camera(RefCell::new(Vec::new()));
    let mut session = Session::new(&s.w, &tools, BUDGET);
    let mut ui = Rec::new(Answer::CannotAsk);
    assert_eq!(session.ask("Was ist auf dem Bildschirm?", &mut ui), Ok(Stop::End), "{:?}", ui.logs);
    let got = &lines(&s, "got")[0];
    assert_eq!(got.str("result"), Some("ein Foto vom Bildschirm"));
    assert_eq!(got.get("images").map(Json::to_text).as_deref(), Some(r#"[{"media_type":"image/png","data":"iVBORw0KGgo="}]"#));
}
