//! Protocol v2 of `assist serve` (plan/spec/assistent-eins K1) over its real socket, with a real settingsd and the
//! fake provider: chats (new, list, ask, history, rename, remove), two chats at once and the limit of three, a
//! restart that keeps the chats and skips a damaged file, a question's window, a remote question that gets at
//! most `ask`, the desktop tools to the view and back, and the scripted service the views test against.

mod common;
use assist::json::{self, Json};
use assist::serve::events::Events;
use assist::serve::peer::Gate;
use assist::serve::{self, Ctx, Parts};
use assist::tools::both::Tools;
use assist::tools::local::Why;
use common::{Fake, Hit, KEY, Reply, World, recording, tool_stream};
use settingsd::peer::{Peer, Source};
use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const BUDGET: usize = 64 << 20;

struct Svc {
    w: Arc<World>,
    path: PathBuf,
    events: Arc<Mutex<Vec<String>>>,
    ctx: Arc<Ctx>,
}

fn parts(w: &Arc<World>, chats: PathBuf, desk_wait: Duration, ev: Events) -> Parts {
    let gate = Gate { uid: 1000, seen: Box::new(|_| Peer { pid: 412, uid: Some(1000), exe: Some("/boot/BITSOS2/bin/bitscomp".into()), by: Source::Kernel }), holder: Box::new(|| Ok(Some("holder=412\n".into()))) };
    Parts {
        settings: w.clone(),
        tools: Box::new(|s, seat| Box::new(Tools::new(s, Err(Why::Absent)).with_native(seat.chat).with_desk(seat.desk))),
        budget: Box::new(|| BUDGET),
        gate,
        events: ev,
        question_wait: Duration::from_secs(5),
        desk_wait,
        stranger_idle: serve::conn::STRANGER_IDLE,
        chats,
    }
}

impl Svc {
    fn start(name: &str, desk_wait: Duration) -> Svc {
        let w = Arc::new(World::new(name));
        Svc::on(w, desk_wait, "assist.sock")
    }

    /// The service on `w` again, as after a restart: the same chats' folder, a new socket.
    fn on(w: Arc<World>, desk_wait: Duration, sock: &str) -> Svc {
        let path = w.root.join(sock);
        let (l, _) = serve::bind::bind_by(&path, &|_| unreachable!(), &|_| unreachable!()).unwrap();
        let (ev, events) = Events::capture();
        let ctx = serve::start(parts(&w, w.root.join("chats"), desk_wait, ev)).unwrap();
        let c = ctx.clone();
        std::thread::spawn(move || serve::serve(&c, &l));
        Svc { w, path, events, ctx }
    }

    fn fake(&self, handler: impl Fn(&Hit, usize) -> Reply + Send + Sync + 'static) -> Fake {
        let fake = Fake::start(handler);
        self.w.provider("p", "anthropic", &fake.address("anthropic"), "local", Some(KEY));
        self.w.set("ai.role.chat", "p");
        fake
    }

    fn connect(&self) -> Client {
        let s = UnixStream::connect(&self.path).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
        Client { r: BufReader::new(s.try_clone().unwrap()), w: s, replies: VecDeque::new(), events: VecDeque::new() }
    }

    fn events(&self) -> String {
        self.events.lock().unwrap().join("\n")
    }

    fn idle(&self) {
        let end = Instant::now() + Duration::from_secs(10);
        while self.ctx.state.view().0 > 0 {
            assert!(Instant::now() < end, "a question never ended");
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

struct Client {
    r: BufReader<UnixStream>,
    w: UnixStream,
    replies: VecDeque<Json>,
    events: VecDeque<Json>,
}

impl Client {
    fn send(&mut self, line: &str) {
        writeln!(self.w, "{line}").unwrap();
    }
    fn read(&mut self) {
        let mut line = String::new();
        let n = self.r.read_line(&mut line).unwrap();
        assert!(n > 0, "the service closed the connection");
        let j = json::parse(line.trim()).unwrap();
        if j.get("ev").is_some() { self.events.push_back(j) } else { self.replies.push_back(j) }
    }
    fn reply(&mut self) -> Json {
        while self.replies.is_empty() {
            self.read();
        }
        self.replies.pop_front().unwrap()
    }
    fn ask(&mut self, line: &str) -> Json {
        self.send(line);
        self.reply()
    }
    fn event(&mut self) -> Json {
        while self.events.is_empty() {
            self.read();
        }
        self.events.pop_front().unwrap()
    }
    /// The events of question `id` up to its last one.
    fn to_end(&mut self, id: i64) -> Vec<Json> {
        let mut out = Vec::new();
        loop {
            let e = self.event();
            assert_eq!(e.get("id").and_then(Json::as_i64), Some(id), "{}", e.to_text());
            let last = matches!(e.str("ev"), Some("done" | "error"));
            out.push(e);
            if last {
                return out;
            }
        }
    }
    /// Events up to the first that `stop` says yes to (it is returned last).
    fn until(&mut self, stop: impl Fn(&Json) -> bool) -> Vec<Json> {
        let mut out = Vec::new();
        loop {
            let e = self.event();
            let done = stop(&e);
            out.push(e);
            if done {
                return out;
            }
        }
    }
    fn new_chat(&mut self, extra: &str) -> String {
        let r = self.ask(&format!(r#"{{"op":"new"{extra}}}"#));
        r.str("chat").unwrap_or_else(|| panic!("{}", r.to_text())).to_string()
    }
}

fn text_of(ev: &[Json]) -> String {
    ev.iter().filter(|e| e.str("ev") == Some("delta")).filter_map(|e| e.str("text")).collect()
}

/// Waits until the provider has seen `n` requests (the start of a question comes before its request).
fn hits_reach(fake: &Fake, n: usize) {
    let end = Instant::now() + Duration::from_secs(10);
    while fake.hits().len() < n {
        assert!(Instant::now() < end, "only {} requests reached the provider", fake.hits().len());
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Takes what a test moved into the trash out again (its record names where it lay).
fn clean_trash(events: &str) {
    for at in events.split("where=\"").skip(1).filter_map(|w| w.split(", at ").nth(1)).filter_map(|w| w.split(')').next()) {
        let entry = Path::new(at).parent().unwrap().to_path_buf();
        let _ = std::fs::remove_file(entry.with_extension("json"));
        let _ = std::fs::remove_dir_all(&entry);
        let _ = std::fs::remove_dir(entry.parent().unwrap());
    }
}

#[test]
fn chats_are_made_listed_asked_shown_renamed_and_removed() {
    let s = Svc::start("v2-chats", serve::DESK_WAIT);
    let fake = s.fake(|_, _| Reply::Sse(recording("anthropic-text.sse")));
    let mut c = s.connect();
    assert_eq!(c.ask(r#"{"op":"chats"}"#).to_text(), r#"{"ok":true,"op":"chats","chats":[]}"#);
    let popup = c.new_chat(r#","origin":"popup","app":"brave""#);
    let side = c.new_chat(r#","origin":"sidebar","title":"Belege für Oktober""#);
    assert_ne!(popup, side);
    assert_eq!(c.ask(r#"{"op":"new","origin":"fenster"}"#).str("error"), Some("bad_request"));
    let file = s.w.root.join("chats").join(format!("{popup}.jsonl"));
    assert_eq!(std::fs::metadata(&file).unwrap().permissions().mode() & 0o777, 0o600, "the chat is Georg's alone");
    assert_eq!(std::fs::metadata(file.parent().unwrap()).unwrap().permissions().mode() & 0o777, 0o700);

    // A question of the popup with the window in front: its words reach the model, its picture is not stored.
    let ctx = r#"{"window":{"id":"w3","app":"brave","title":"Suchseite"},"list":"textbox \"Adresse\" #w3/1a","image":{"media_type":"image/png","data":"iVBORw0KGgo="}}"#;
    c.send(&format!(r#"{{"op":"ask","chat":"{popup}","id":1,"text":"Was ist das?","by":"hand","context":{ctx}}}"#));
    let ev = c.to_end(1);
    assert!(ev.iter().all(|e| e.str("chat") == Some(popup.as_str())), "every event names its chat");
    assert_eq!(ev.last().unwrap().str("ev"), Some("done"));
    let body = fake.hits()[0].body.clone();
    assert!(body.contains("Suchseite") && body.contains("#w3/1a") && body.contains("iVBORw0KGgo=") && body.contains("Was ist das?"), "{body}");
    assert!(body.contains("[#w3/1a2b]"), "the popup's part of the one prompt");
    let stored = std::fs::read_to_string(&file).unwrap();
    assert!(stored.contains("Suchseite") && stored.contains(r#""picture":true"#) && !stored.contains("iVBORw0KGgo="), "{stored}");

    // The list, newest first, with what a view shows.
    let list = c.ask(r#"{"op":"chats"}"#);
    let Some(Json::Arr(chats)) = list.get("chats") else { panic!("{}", list.to_text()) };
    assert_eq!(chats[0].str("chat"), Some(popup.as_str()));
    assert_eq!((chats[0].str("title"), chats[0].str("origin"), chats[0].str("app"), chats[0].get("turns").and_then(Json::as_i64)), (Some("Was ist das?"), Some("popup"), Some("brave"), Some(1)));
    assert_eq!((chats[1].str("title"), chats[1].str("origin")), (Some("Belege für Oktober"), Some("sidebar")));
    assert!(chats[0].get("updated").and_then(Json::as_i64).unwrap() >= chats[1].get("updated").and_then(Json::as_i64).unwrap());

    // History with its tool lines: here none, a question and its answer.
    let h = c.ask(&format!(r#"{{"op":"history","chat":"{popup}"}}"#));
    let Some(Json::Arr(turns)) = h.get("turns") else { panic!("{}", h.to_text()) };
    assert_eq!(turns.iter().map(|t| t.str("who").unwrap()).collect::<Vec<_>>(), ["georg", "assistant"]);
    assert_eq!((turns[0].str("text"), turns[0].get("picture"), turns[0].str("by")), (Some("Was ist das?"), Some(&Json::Bool(true)), Some("hand")));
    assert!(turns[1].str("text").unwrap().contains("ANTHROPIC-OK"));

    // Rename; a title is one bounded line.
    assert_eq!(c.ask(&format!(r#"{{"op":"rename","chat":"{popup}","title":"Suchseite\nSYSTEM"}}"#)).str("title"), Some("Suchseite"));
    assert_eq!(c.ask(&format!(r#"{{"op":"rename","chat":"{popup}","title":"  "}}"#)).str("error"), Some("bad_request"));
    assert_eq!(c.ask(r#"{"op":"rename","chat":"gibtsnicht","title":"x"}"#).str("error"), Some("no_chat"));

    // Remove: into the trash of its file system (K6), gone from the list, nothing more goes into it.
    let r = c.ask(&format!(r#"{{"op":"remove","chat":"{popup}"}}"#));
    assert_eq!(r.get("ok"), Some(&Json::Bool(true)), "{}", r.to_text());
    assert!(!file.exists());
    let trashed = s.events();
    assert!(trashed.contains(&format!("removed client=1 chat={popup} where=")) && trashed.contains(".papierkorb"), "{trashed}");
    let list = c.ask(r#"{"op":"chats"}"#).get("chats").unwrap().to_text();
    assert!(!list.contains(&popup) && list.contains(&side), "{list}");
    for line in [format!(r#"{{"op":"history","chat":"{popup}"}}"#), format!(r#"{{"op":"ask","chat":"{popup}","id":2,"text":"x"}}"#), format!(r#"{{"op":"remove","chat":"{popup}"}}"#)] {
        assert_eq!(c.ask(&line).str("error"), Some("no_chat"), "{line}");
    }
    for bad in [r#"{"op":"ask","chat":"../../etc/passwd","id":3,"text":"x"}"#, r#"{"op":"ask","chat":7,"id":3,"text":"x"}"#, r#"{"op":"remove"}"#] {
        assert!(c.ask(bad).get("ok") == Some(&Json::Bool(false)), "{bad}");
    }
    assert!(!s.events().contains("Was ist das?") && !s.events().contains(KEY), "the events hold no word of a chat");
    clean_trash(&trashed);
}

#[test]
fn two_chats_work_at_once_one_question_each_three_at_most() {
    let s = Svc::start("v2-two", serve::DESK_WAIT);
    let fake = s.fake(|h, _| if h.body.contains("HAENGT") { Reply::Hang(Vec::new()) } else { Reply::Sse(recording("anthropic-text.sse")) });
    let mut c = s.connect();
    let ids: Vec<String> = (0..4).map(|_| c.new_chat("")).collect();
    for (i, chat) in ids[..2].iter().enumerate() {
        c.send(&format!(r#"{{"op":"ask","chat":"{chat}","id":{},"text":"HAENGT"}}"#, i + 1));
    }
    hits_reach(&fake, 2);
    // A second question into a chat at work is busy; another chat answers meanwhile.
    let busy = c.ask(&format!(r#"{{"op":"ask","chat":"{}","id":9,"text":"x"}}"#, ids[0]));
    assert_eq!((busy.str("error"), busy.str("chat")), (Some("busy"), Some(ids[0].as_str())), "{}", busy.to_text());
    c.send(&format!(r#"{{"op":"ask","chat":"{}","id":3,"text":"Hallo"}}"#, ids[2]));
    let done: Vec<Json> = c.until(|e| e.get("id").and_then(Json::as_i64) == Some(3) && e.str("ev") == Some("done"));
    assert!(text_of(&done.iter().filter(|e| e.get("id").and_then(Json::as_i64) == Some(3)).cloned().collect::<Vec<_>>()).contains("ANTHROPIC-OK"));
    // Three at work: the fourth chat waits its turn.
    c.send(&format!(r#"{{"op":"ask","chat":"{}","id":4,"text":"HAENGT"}}"#, ids[2]));
    hits_reach(&fake, 4);
    let full = c.ask(&format!(r#"{{"op":"ask","chat":"{}","id":5,"text":"x"}}"#, ids[3]));
    assert!(full.str("error") == Some("busy") && full.str("message").unwrap().contains("drei Fragen"), "{}", full.to_text());
    let st = c.ask(r#"{"op":"status"}"#);
    assert_eq!((st.get("running").and_then(Json::as_i64), st.get("chats").and_then(Json::as_i64), st.get("busy")), (Some(3), Some(4), Some(&Json::Bool(true))));
    // Cancel by chat ends that one only.
    let r = c.ask(&format!(r#"{{"op":"cancel","chat":"{}"}}"#, ids[0]));
    assert_eq!(r.get("id").and_then(Json::as_i64), Some(1), "{}", r.to_text());
    let ended = c.until(|e| e.get("id").and_then(Json::as_i64) == Some(1) && e.str("ev") == Some("error"));
    assert_eq!(ended.last().unwrap().str("message"), Some("abgebrochen"));
    c.send(&format!(r#"{{"op":"ask","chat":"{}","id":6,"text":"Hallo"}}"#, ids[3]));
    c.until(|e| e.get("id").and_then(Json::as_i64) == Some(6) && e.str("ev") == Some("done"));
    for chat in [&ids[1], &ids[2]] {
        c.ask(&format!(r#"{{"op":"cancel","chat":"{chat}"}}"#));
    }
    s.idle();
    // The stopped question stays in the chat's history, marked; the conversation goes on without it.
    let h = c.ask(&format!(r#"{{"op":"history","chat":"{}"}}"#, ids[0])).get("turns").unwrap().to_text();
    assert!(h.contains(r#""who":"georg","text":"HAENGT""#) && h.contains(r#""who":"error","text":"abgebrochen""#), "{h}");
}

#[test]
fn chats_survive_a_restart_and_a_damaged_file_is_skipped() {
    let s = Svc::start("v2-restart", serve::DESK_WAIT);
    let fake = s.fake(|_, _| Reply::Sse(recording("anthropic-text.sse")));
    let mut c = s.connect();
    let chat = c.new_chat(r#","origin":"sidebar""#);
    c.send(&format!(r#"{{"op":"ask","chat":"{chat}","id":1,"text":"Merk dir die Zahl 7"}}"#));
    c.to_end(1);
    let before = c.ask(&format!(r#"{{"op":"history","chat":"{chat}"}}"#)).get("turns").unwrap().to_text();
    drop(c);
    std::fs::write(s.w.root.join("chats/kaputt.jsonl"), "{\"t\":\"chat\"\n").unwrap();
    std::fs::write(s.w.root.join("chats/c0-0.jsonl"), [0xffu8, 0xfe, 0x00]).unwrap();

    let again = Svc::on(s.w.clone(), serve::DESK_WAIT, "assist2.sock");
    let mut c = again.connect();
    let list = c.ask(r#"{"op":"chats"}"#);
    assert!(list.get("chats").unwrap().to_text().contains(&chat), "{}", list.to_text());
    assert_eq!(list.str("notice"), Some("2 Chat-Dateien ließen sich nicht lesen und fehlen in der Liste."));
    assert_eq!(again.events().matches("assist serve.chat.skipped").count(), 2, "{}", again.events());
    assert_eq!(c.ask(&format!(r#"{{"op":"history","chat":"{chat}"}}"#)).get("turns").unwrap().to_text(), before);
    // The conversation goes on where it was: the provider gets the earlier turn.
    c.send(&format!(r#"{{"op":"ask","chat":"{chat}","id":2,"text":"Welche Zahl?"}}"#));
    c.to_end(2);
    let body = fake.hits()[1].body.clone();
    assert!(body.contains("Merk dir die Zahl 7") && body.contains("ANTHROPIC-OK") && body.contains("Welche Zahl?"), "{body}");
    // A request of before K1 means the newest sidebar chat.
    assert!(c.ask(r#"{"op":"history"}"#).get("turns").unwrap().to_text().contains("Welche Zahl?"));
}

#[test]
fn a_remote_question_gets_at_most_ask_and_a_hand_one_the_level() {
    let s = Svc::start("v2-remote", serve::DESK_WAIT);
    // Every question: the tool turn, then the closing text.
    let _fake = s.fake(|_, n| Reply::Sse(if n % 2 == 0 { recording("anthropic-done.sse") } else { tool_stream("settings_set", r#"{"key":"theme.scale","value":150}"#, "tool_use") }));
    s.w.set("ai.level", "auto");
    let mut c = s.connect();
    let chat = c.new_chat("");
    // From the control socket or MCP: asked, though the level is Automodus.
    c.send(&format!(r#"{{"op":"ask","chat":"{chat}","id":1,"text":"Mach groesser","by":"remote"}}"#));
    let asks = c.until(|e| e.str("state") == Some("asks"));
    let m = asks.last().unwrap().get("ask_id").and_then(Json::as_i64).unwrap();
    assert_eq!(asks.last().unwrap().str("line"), Some("Einstellung „theme.scale“ ändern: 125 → 150"));
    c.ask(&format!(r#"{{"op":"answer","ask_id":{m},"yes":false}}"#));
    c.until(|e| e.str("ev") == Some("done"));
    assert_eq!(s.w.value("theme.scale"), Json::Int(125));
    // From Georg's hand: Automodus runs it unasked.
    c.send(&format!(r#"{{"op":"ask","chat":"{chat}","id":2,"text":"Mach groesser","by":"hand"}}"#));
    let ev = c.to_end(2);
    assert!(!ev.iter().any(|e| e.str("state") == Some("asks")), "{:?}", ev.iter().map(Json::to_text).collect::<Vec<_>>());
    assert_eq!(s.w.value("theme.scale"), Json::Int(150));
    assert_eq!(c.ask(&format!(r#"{{"op":"ask","chat":"{chat}","id":3,"text":"x","by":"irgendwer"}}"#)).str("error"), Some("bad_request"));
    // A question that does not say who asked counts as remote: asked, and the card is told so.
    s.w.set("theme.scale", 125i64);
    c.send(&format!(r#"{{"op":"ask","chat":"{chat}","id":4,"text":"Mach groesser"}}"#));
    let asks = c.until(|e| e.str("state") == Some("asks"));
    let last = asks.last().unwrap();
    assert_eq!(last.str("by"), Some("remote"), "{}", last.to_text());
    c.ask(&format!(r#"{{"op":"answer","ask_id":{},"yes":false}}"#, last.get("ask_id").and_then(Json::as_i64).unwrap()));
    c.until(|e| e.str("ev") == Some("done"));
    assert_eq!(s.w.value("theme.scale"), Json::Int(125));
    let h = c.ask(&format!(r#"{{"op":"history","chat":"{chat}"}}"#)).get("turns").unwrap().to_text();
    assert!(h.contains(r#""by":"remote""#) && h.contains(r#""who":"tool","name":"settings_set","state":"refused""#) && h.contains(r#""state":"done""#), "{h}");
}

/// The view's side of a desktop call: answers `desk` events as told.
fn view_answers(c: &mut Client, id: i64, answer: impl Fn(&Json) -> Option<String>) -> Vec<Json> {
    let mut seen = Vec::new();
    loop {
        let e = c.event();
        assert_eq!(e.get("id").and_then(Json::as_i64), Some(id), "{}", e.to_text());
        if e.str("ev") == Some("desk")
            && let Some(line) = answer(&e)
        {
            let r = c.ask(&line);
            assert_eq!(r.get("ok"), Some(&Json::Bool(true)), "{}", r.to_text());
        }
        let last = matches!(e.str("ev"), Some("done" | "error"));
        seen.push(e);
        if last {
            return seen;
        }
    }
}

#[test]
fn desktop_tools_go_to_the_view_and_their_results_come_back() {
    let s = Svc::start("v2-desk", Duration::from_secs(1));
    // Each question: the tool named in its words, then the closing text.
    let fake = s.fake(|h, n| {
        if n % 2 == 0 {
            return Reply::Sse(recording("anthropic-done.sse"));
        }
        // The newest question's words are the last of the conversation.
        let last = h.body.rsplit("\"role\":\"user\"").next().unwrap_or("").to_string();
        let (name, input) = if last.contains("HANDLE") {
            ("handeln", r#"{"element":"w3/1a","aktion":"type","text":"Georg","name":"Adresse","warum":"Georg sucht."}"#)
        } else if last.contains("SIEH") {
            ("sehen", r#"{"window":"w3","picture":true}"#)
        } else {
            ("fenster", "{}")
        };
        Reply::Sse(tool_stream(name, input, "tool_use"))
    });
    let mut c = s.connect();
    let chat = c.new_chat(r#","origin":"popup""#);
    let result = |call: i64, content: &str| Json::obj().with("op", "desk_result").with("call_id", call).with("content", content).to_text();

    c.send(&format!(r#"{{"op":"ask","chat":"{chat}","id":1,"text":"Welche Fenster?","by":"hand"}}"#));
    let ev = view_answers(&mut c, 1, |e| Some(result(e.get("call_id").and_then(Json::as_i64).unwrap(), "w3 brave „Suchseite“ (vorne)")));
    let desk = ev.iter().find(|e| e.str("ev") == Some("desk")).unwrap();
    assert_eq!((desk.str("chat"), desk.str("name"), desk.get("args").map(Json::to_text).as_deref(), desk.str("level"), desk.str("by")), (Some(chat.as_str()), Some("fenster"), Some("{}"), Some("auto"), Some("hand")));
    assert_eq!(ev.last().unwrap().str("ev"), Some("done"));
    assert!(fake.hits()[1].body.contains("w3 brave „Suchseite“ (vorne)"), "the view's result reached the model");

    // A step: the engine asks nothing itself and hands the level along; the compositor judges.
    s.w.set("ai.level", "auto");
    c.send(&format!(r#"{{"op":"ask","chat":"{chat}","id":2,"text":"HANDLE bitte","by":"remote"}}"#));
    let ev = view_answers(&mut c, 2, |e| Some(Json::obj().with("op", "desk_result").with("call_id", e.get("call_id").cloned().unwrap()).with("content", "Georg hat Nein gesagt.").with("is_error", true).to_text()));
    let desk = ev.iter().find(|e| e.str("ev") == Some("desk")).unwrap();
    assert_eq!((desk.str("name"), desk.str("level"), desk.str("by")), (Some("handeln"), Some("ask"), Some("remote")), "a remote question: at most ask");
    assert!(!ev.iter().any(|e| e.str("state") == Some("asks")), "the engine never asks about a step");
    assert!(fake.hits()[3].body.contains("Georg hat Nein gesagt.") && fake.hits()[3].body.contains(r#""is_error":true"#));
    c.send(&format!(r#"{{"op":"ask","chat":"{chat}","id":3,"text":"HANDLE nochmal","by":"hand"}}"#));
    let ev = view_answers(&mut c, 3, |e| Some(result(e.get("call_id").and_then(Json::as_i64).unwrap(), "erledigt")));
    assert_eq!(ev.iter().find(|e| e.str("ev") == Some("desk")).unwrap().str("level"), Some("auto"));

    // A picture from the view goes to the model with the result.
    c.send(&format!(r#"{{"op":"ask","chat":"{chat}","id":4,"text":"SIEH hin"}}"#));
    view_answers(&mut c, 4, |e| Some(Json::obj().with("op", "desk_result").with("call_id", e.get("call_id").cloned().unwrap()).with("content", "textbox Adresse #w3/1a").with("image", Json::obj().with("media_type", "image/png").with("data", "iVBORw0KGgo=")).to_text()));
    let body = fake.hits()[7].body.clone();
    assert!(body.contains("textbox Adresse #w3/1a") && body.contains(r#""type":"image""#) && body.contains("iVBORw0KGgo="), "{body}");

    // A view that does not answer: an error result after the wait, never a hang. A late result finds nothing.
    let t = Instant::now();
    c.send(&format!(r#"{{"op":"ask","chat":"{chat}","id":5,"text":"Welche Fenster?"}}"#));
    let ev = view_answers(&mut c, 5, |_| None);
    let late = ev.iter().find(|e| e.str("ev") == Some("desk")).and_then(|e| e.get("call_id")).and_then(Json::as_i64).unwrap();
    assert_eq!(ev.last().unwrap().str("ev"), Some("done"));
    assert!(t.elapsed() < Duration::from_secs(10) && fake.hits()[9].body.contains("did not answer within 1 s"), "{:?}", t.elapsed());
    let gone = c.ask(&Json::obj().with("op", "desk_result").with("call_id", late).with("content", "x").to_text());
    assert_eq!(gone.str("error"), Some("no_call"));
    assert_eq!(c.ask(r#"{"op":"desk_result","content":"x"}"#).str("error"), Some("bad_request"));

    // The view goes away during the call: the question ends at once and the service goes on.
    c.send(&format!(r#"{{"op":"ask","chat":"{chat}","id":6,"text":"Welche Fenster?"}}"#));
    c.until(|e| e.str("ev") == Some("desk"));
    drop(c);
    s.idle();
    assert!(s.events().contains(&format!("chat={chat} id=6 outcome=cancelled")), "{}", s.events());
    let mut c = s.connect();
    assert_eq!(c.ask(r#"{"op":"status"}"#).get("running").and_then(Json::as_i64), Some(0));
}

#[test]
fn the_level_decides_what_is_offered_and_the_native_tools_run_as_georg() {
    let s = Svc::start("v2-native", serve::DESK_WAIT);
    let dir = s.w.root.join("arbeit");
    std::fs::create_dir_all(&dir).unwrap();
    let cmd = Json::obj().with("command", "echo hallo > probe.txt; cat probe.txt").with("cwd", dir.to_str().unwrap()).to_text();
    let fake = s.fake(move |_, n| Reply::Sse(if n % 2 == 0 { recording("anthropic-done.sse") } else { tool_stream("befehl", &cmd, "tool_use") }));
    let mut c = s.connect();
    let chat = c.new_chat("");
    let offered = |h: &Hit| -> Vec<String> {
        match h.json().get("tools") {
            Some(Json::Arr(t)) => t.iter().filter_map(|t| t.str("name").map(str::to_string)).collect(),
            _ => Vec::new(),
        }
    };
    // Fragt vorher: the command is asked, with what runs and where.
    c.send(&format!(r#"{{"op":"ask","chat":"{chat}","id":1,"text":"Leg eine Probe an"}}"#));
    let asks = c.until(|e| e.str("state") == Some("asks"));
    let q = asks.last().unwrap();
    assert_eq!(q.str("line"), Some(format!("Befehl ausführen (in {}): echo hallo > probe.txt; cat probe.txt", dir.display()).as_str()));
    c.ask(&format!(r#"{{"op":"answer","ask_id":{},"yes":true}}"#, q.get("ask_id").and_then(Json::as_i64).unwrap()));
    c.until(|e| e.str("ev") == Some("done"));
    assert_eq!(std::fs::read_to_string(dir.join("probe.txt")).unwrap(), "hallo\n");
    assert!(fake.hits()[1].body.contains("exit code 0") && fake.hits()[1].body.contains("hallo"));
    let all = offered(&fake.hits()[0]);
    for t in ["befehl", "datei_lesen", "datei_schreiben", "datei_aendern", "dateien_finden", "text_suchen", "web_holen", "loeschen", "fenster", "sehen", "handeln", "foto", "settings_set"] {
        assert!(all.contains(&t.to_string()), "{t} in {all:?}");
    }
    // Nur antworten: only what reads is offered.
    s.w.set("ai.level", "answer");
    c.send(&format!(r#"{{"op":"ask","chat":"{chat}","id":2,"text":"Leg eine Probe an"}}"#));
    c.until(|e| e.str("ev") == Some("done"));
    let reading = offered(&fake.hits()[2]);
    assert!(reading.contains(&"datei_lesen".to_string()) && reading.contains(&"sehen".to_string()) && !reading.iter().any(|t| ["befehl", "datei_schreiben", "loeschen", "handeln", "settings_set", "web_holen"].contains(&t.as_str())), "{reading:?}");
    assert!(fake.hits()[2].json().get("system").map(Json::to_text).unwrap_or_default().contains("Nur antworten"));
    // A model that calls a change anyway hears why it does not run (K2: not offered, and refused), and nothing ran.
    std::fs::remove_file(dir.join("probe.txt")).unwrap();
    c.send(&format!(r#"{{"op":"ask","chat":"{chat}","id":3,"text":"Leg eine Probe an"}}"#));
    c.until(|e| e.str("ev") == Some("done"));
    assert!(fake.hits()[4].body.contains("Nur antworten") && !fake.hits()[4].body.contains("unknown tool"), "{}", fake.hits()[4].body);
    assert!(!dir.join("probe.txt").exists(), "nothing ran");
}

#[test]
fn the_scripted_service_the_views_test_against() {
    let w = World::new("v2-fake");
    let sock = w.root.join("fake.sock");
    let script = assist::serve::fake::Script::parse(
        r#"{"level":"ask","answers":[{"match":"such","rounds":[{"text":"Ich schreibe es hinein. ","tool":"handeln","args":{"element":"w3/1a","aktion":"type","text":"Georg","name":"Adresse","warum":"Georg sucht."}},{"tool":"probe_aendern","args":{"was":"x"}},{"text":"Erledigt."}]}]}"#,
    )
    .unwrap();
    let ctx = assist::serve::fake::start(&sock, w.root.join("chats"), script).unwrap();
    let s = UnixStream::connect(&sock).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
    let mut c = Client { r: BufReader::new(s.try_clone().unwrap()), w: s, replies: VecDeque::new(), events: VecDeque::new() };
    let chat = c.new_chat(r#","origin":"popup""#);
    c.send(&format!(r#"{{"op":"ask","chat":"{chat}","id":1,"text":"such nach Georg","context":{{"window":{{"id":"w3","app":"brave","title":"Suchseite"}}}}}}"#));
    let desk = c.until(|e| e.str("ev") == Some("desk")).pop().unwrap();
    assert_eq!((desk.str("name"), desk.str("level")), (Some("handeln"), Some("ask")));
    c.ask(&Json::obj().with("op", "desk_result").with("call_id", desk.get("call_id").cloned().unwrap()).with("content", "Adresse: Georg").to_text());
    let asks = c.until(|e| e.str("state") == Some("asks")).pop().unwrap();
    assert_eq!(asks.str("name"), Some("probe_aendern"));
    c.ask(&format!(r#"{{"op":"answer","ask_id":{},"yes":true}}"#, asks.get("ask_id").and_then(Json::as_i64).unwrap()));
    let rest = c.until(|e| e.str("ev") == Some("done"));
    assert!(text_of(&rest).contains("Erledigt."), "{:?}", rest.iter().map(Json::to_text).collect::<Vec<_>>());
    assert_eq!(c.ask(r#"{"op":"status"}"#).str("level"), Some("ask"));
    drop(ctx);
}

/// HELPER.md version 5 in both views: the popup's chat and the sidebar's get the same level, the same tools of
/// their own and the same of assist's; only the view's part of the prompt differs.
#[test]
fn a_helper_gets_the_same_in_the_popup_and_the_sidebar() {
    let s = Svc::start("v2-helper-views", serve::DESK_WAIT);
    let dir = s.w.root.join("helper");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("h");
    let body = format!("#!/bin/sh\nread -r line\nn=$(ls '{d}' | grep -c '^req') \nprintf '%s\\n' \"$line\" > '{d}'/req.$n\necho '{{\"text\":\"ok\"}}'\necho '{{\"done\":true,\"stop\":\"end\",\"error\":null}}'\n", d = dir.display());
    let mut sh = std::process::Command::new("sh").args(["-c", "cat > \"$1\" && chmod 755 \"$1\"", "sh"]).arg(&path).stdin(std::process::Stdio::piped()).spawn().unwrap();
    sh.stdin.take().unwrap().write_all(body.as_bytes()).unwrap();
    assert!(sh.wait().unwrap().success());
    s.w.set("ai.providers", "h");
    s.w.set("ai.provider.h.kind", "program");
    s.w.set("ai.provider.h.address", path.to_str().unwrap());
    s.w.set("ai.provider.h.place", "local");
    s.w.set("ai.role.chat", "h");
    let mut c = s.connect();
    for (level, n0) in [("auto", 0), ("answer", 2)] {
        s.w.set("ai.level", level);
        for (i, origin) in ["popup", "sidebar"].iter().enumerate() {
            let chat = c.new_chat(&format!(r#","origin":"{origin}""#));
            c.send(&format!(r#"{{"op":"ask","chat":"{chat}","id":{},"text":"Hallo","by":"hand"}}"#, 10 + n0 + i));
            c.until(|e| e.str("ev") == Some("done"));
        }
        let req = |n: usize| json::parse(std::fs::read_to_string(dir.join(format!("req.{n}"))).unwrap().trim()).unwrap();
        let (popup, side) = (req(n0), req(n0 + 1));
        for key in ["bitsos_helper", "level", "allow", "own_tools", "tools", "tools_system", "system_tools"] {
            assert_eq!(popup.get(key).map(Json::to_text), side.get(key).map(Json::to_text), "{level}: {key}");
        }
        assert_eq!(popup.str("level"), Some(level));
        assert_eq!(popup.get("bell").is_some(), level != "answer", "{level}: a bell exactly when the helper has tools of its own");
        assert!(popup.str("system").unwrap().contains("[#w3/1a2b]") && !side.str("system").unwrap().contains("[#w3/1a2b]"), "only the view's part differs");
    }
}
