//! Whole questions through the engine: a real settingsd, a fake provider on a loopback port answering from the
//! recordings, and a screen that writes everything down.

mod common;
use assist::config;
use assist::conv::Stop;
use assist::engine::{Answer, Session};
use assist::json::Json;
use assist::provider::{Kind, Place, Provider};
use assist::secret::Secret;
use assist::settings;
use assist::tools::settings::SettingsTools;
use common::{Fake, Hit, KEY, Rec, Reply, World, recording, tool_stream};
use std::time::{Duration, Instant};

const BUDGET: usize = 64 << 20;

/// The recording a kind's fake answers with: the tool turn first, the closing text once results came back.
fn by_path(hit: &Hit, first: &str) -> Reply {
    let kind = if hit.path.starts_with("/v1/messages") {
        "anthropic"
    } else if hit.path.starts_with("/v1/chat/completions") {
        "openai"
    } else if hit.path.starts_with("/v1beta/models/") {
        "gemini"
    } else {
        return Reply::Status(404, vec![], "{}".into());
    };
    let has_result = hit.body.contains("tool_result") || hit.body.contains(r#""role":"tool""#) || hit.body.contains("functionResponse");
    Reply::Sse(recording(&format!("{kind}-{}.sse", if has_result { "done" } else { first })))
}

fn ask(w: &World, text: &str, answer: Answer) -> (Result<Stop, String>, Rec) {
    let tools = SettingsTools { settings: w };
    let mut ui = Rec::new(answer);
    let r = Session::new(w, &tools, BUDGET).ask(text, &mut ui);
    (r, ui)
}

const KINDS: [(&str, &str); 4] = [("anthropic", "ANTHROPIC"), ("openai", "OPENAI"), ("gemini", "GEMINI"), ("openai_compatible", "OPENAI")];

#[test]
fn a_text_answer_from_each_kind() {
    let w = World::new("engine-text");
    let fake = Fake::start(|h, _| by_path(h, "text"));
    for (kind, mark) in KINDS {
        let id = format!("p-{kind}");
        w.provider(&id, kind, &fake.address(kind), "local", (kind != "openai_compatible").then_some(KEY));
        w.set("ai.role.chat", id.as_str());
        let (r, ui) = ask(&w, "Hallo", Answer::CannotAsk);
        assert_eq!(r, Ok(Stop::End), "{kind}");
        assert_eq!(ui.text, format!("Hallo Georg, hier antwortet die Aufzeichnung {mark}-OK. Grüße mit Umlauten: äöüß."), "{kind}");
        assert!(!ui.everything().contains(KEY));
    }
    let hits = fake.hits();
    assert_eq!(hits.len(), 4);
    assert_eq!((hits[0].header("x-api-key"), hits[0].header("anthropic-version")), (Some(KEY), Some("2026-10-21")));
    assert_eq!(hits[1].header("authorization"), Some(format!("Bearer {KEY}").as_str()));
    assert_eq!((hits[2].header("x-goog-api-key"), hits[2].path.as_str()), (Some(KEY), "/v1beta/models/model-rec:streamGenerateContent?alt=sse"));
    assert_eq!(hits[3].header("authorization"), None, "a local engine without a key");
    // The six settings tools are offered to every kind, and the question arrived.
    for h in &hits {
        assert!(h.body.contains("settings_history") && h.body.contains("Hallo"), "{}", h.path);
    }
    assert!(!w.everything_settingsd_kept().contains(KEY), "settingsd wrote the key nowhere");
}

#[test]
fn the_model_changes_a_setting_and_takes_it_back() {
    for (kind, mark) in KINDS {
        let w = World::new(&format!("engine-tools-{kind}"));
        let fake = Fake::start(|h, _| by_path(h, "tools"));
        w.provider("p", kind, &fake.address(kind), "local", Some(KEY));
        w.set("ai.role.chat", "p");
        w.set("ai.level", "auto");
        let (r, ui) = ask(&w, "Mach alles größer", Answer::CannotAsk);
        assert_eq!(r, Ok(Stop::End), "{kind}");
        assert_eq!(ui.text, format!("Ich sehe nach und stelle es um.Erledigt: {mark}-DONE."), "{kind}");
        assert_eq!(w.value("theme.scale"), Json::Int(150), "{kind}: the setting really changed");
        assert_eq!(ui.notes, ["Einstellung „theme.scale“ lesen", "Einstellung „theme.scale“ ändern: 125 → 150"], "{kind}: Georg sees what the tools do");
        assert!(ui.asked.is_empty(), "{kind}");
        // Both results went back to the model in the second request: the value read, the change made.
        let hits = fake.hits();
        assert_eq!(hits.len(), 2, "{kind}");
        let second = hits[1].body.replace("\\\"", "\"");
        assert!(second.contains(r#""value":125"#) && second.contains(r#""old":125,"new":150"#), "{kind}: {second}");
        let h = w.request_text(r#"{"op":"history","n":1}"#);
        assert!(h.contains(r#""key":"theme.scale""#) && h.contains(r#""source":"assist""#), "{kind}: {h}");
    }
    // And back with settings_undo (the Anthropic recording of that call).
    let w = World::new("engine-undo");
    let fake = Fake::start(|h, _| if h.body.contains("tool_result") { Reply::Sse(recording("anthropic-done.sse")) } else { Reply::Sse(recording("anthropic-undo.sse")) });
    w.provider("p", "anthropic", &fake.address("anthropic"), "local", Some(KEY));
    w.set("ai.role.chat", "p");
    w.set("ai.level", "ask");
    w.set("theme.scale", 150i64);
    let (r, ui) = ask(&w, "Nimm das zurück", Answer::Yes);
    assert_eq!(r, Ok(Stop::End));
    assert_eq!(ui.asked, ["Änderung zurücknehmen: „theme.scale“: 150 → 125"], "at ask an undo asks too");
    assert_eq!(w.value("theme.scale"), Json::Int(125));
}

#[test]
fn asking_georg_first() {
    // At the level ask (Georg's choice; the default is auto) a change waits for his answer.
    for (answer, want_value, want_in_result) in [(Answer::Yes, 150, r#""new":150"#), (Answer::No, 125, "Georg said no"), (Answer::CannotAsk, 125, "nobody can be asked")] {
        let w = World::new(&format!("engine-ask-{answer:?}"));
        let fake = Fake::start(|h, _| by_path(h, "tools"));
        w.provider("p", "anthropic", &fake.address("anthropic"), "local", Some(KEY));
        w.set("ai.role.chat", "p");
        w.set("ai.level", "ask");
        let (r, ui) = ask(&w, "Mach alles größer", answer);
        assert_eq!(r, Ok(Stop::End), "{answer:?}");
        assert_eq!(ui.asked, ["Einstellung „theme.scale“ ändern: 125 → 150"], "{answer:?}");
        assert_eq!(w.value("theme.scale"), Json::Int(want_value), "{answer:?}");
        let second = fake.hits()[1].body.replace("\\\"", "\"");
        assert!(second.contains(want_in_result), "{answer:?}: the model is told: {second}");
        // The refusal is an error result, the read beside it still ran.
        assert_eq!(second.contains(r#""is_error":true"#), answer != Answer::Yes, "{answer:?}");
        assert!(second.contains(r#""value":125"#));
    }
}

/// One question to a provider whose first answer is `first`; the tool result the model got back, and the screen.
fn hostile(name: &str, first: Vec<u8>, allow_set: bool, answer: Answer) -> (World, String, Rec) {
    let w = World::new(&format!("engine-hostile-{name}"));
    let fake = Fake::start(move |h, _| if h.body.contains("tool_result") { Reply::Sse(recording("anthropic-done.sse")) } else { Reply::Sse(first.clone()) });
    w.provider("p", "anthropic", &fake.address("anthropic"), "local", Some(KEY));
    w.set("ai.role.chat", "p");
    if allow_set {
        w.set("ai.level", "auto");
    }
    let (r, ui) = ask(&w, "x", answer);
    assert_eq!(r, Ok(if name == "maxtokens" { Stop::MaxTokens } else { Stop::End }), "{name}");
    let hits = fake.hits();
    let back = hits.get(1).map(|h| h.body.replace("\\\"", "\"").replace("\\\\", "\\")).unwrap_or_default();
    (w, back, ui)
}

#[test]
fn hostile_tool_calls_never_run() {
    // Arguments that are not JSON: the model gets its own text back, marked as an error.
    let (w, back, _) = hostile("badjson", tool_stream("settings_set", r#"{"key": "theme.scale", "value": 150"#, "tool_use"), true, Answer::Yes);
    assert!(back.contains("INVALID_JSON") && back.contains(r#""is_error":true"#), "{back}");
    assert_eq!(w.value("theme.scale"), Json::Int(125));

    // Arguments against the schema.
    let (w, back, _) = hostile("schema", tool_stream("settings_set", r#"{"key": ["theme.scale"], "value": {"x": 1}}"#, "tool_use"), true, Answer::Yes);
    assert!(back.contains("invalid arguments: key: want string, got array"), "{back}");
    assert_eq!(w.value("theme.scale"), Json::Int(125));

    // A tool that does not exist.
    let (_, back, _) = hostile("unknown", tool_stream("run", r#"{"path": "/bin/sh"}"#, "tool_use"), true, Answer::Yes);
    assert!(back.contains(r#"unknown tool \"run\""#) || back.contains(r#"unknown tool "run""#), "{back}");

    // An R8 switch: refused without a question, although the level is Automodus.
    let (_, back, ui) = hostile("r8", tool_stream("settings_set", r#"{"key": "nvme_write", "value": true}"#, "tool_use"), true, Answer::Yes);
    assert!(back.contains("refused_r8") && ui.asked.is_empty(), "{back} {:?}", ui.asked);

    // The model tries to change its own leash in Automodus: Georg is asked all the same, and says no.
    let (w, back, ui) = hostile("leash", tool_stream("settings_set", r#"{"key": "ai.local_only", "value": true, "confirmed": true}"#, "tool_use"), true, Answer::No);
    assert_eq!(ui.asked, ["Einstellung „ai.local_only“ ändern: aus → an"]);
    assert!(back.contains("Georg said no"));
    assert_eq!(w.value("ai.local_only"), Json::Bool(false));

    // A complete-looking call in an answer cut at the length limit is not run either.
    let (w, back, ui) = hostile("maxtokens", tool_stream("settings_set", r#"{"key": "theme.scale", "value": 150}"#, "max_tokens"), true, Answer::Yes);
    assert_eq!((w.value("theme.scale"), back.as_str()), (Json::Int(125), ""), "no tool ran and no second request was sent");
    assert!(ui.notes.iter().any(|n| n.contains("abgeschnitten")), "{:?}", ui.notes);
}

#[test]
fn a_model_that_never_stops_calling_tools_is_stopped() {
    let w = World::new("engine-rounds");
    let fake = Fake::start(|_, _| Reply::Sse(tool_stream("settings_get", r#"{"key": "theme.scale"}"#, "tool_use")));
    w.provider("p", "anthropic", &fake.address("anthropic"), "local", Some(KEY));
    w.set("ai.role.chat", "p");
    w.set("ai.tool_rounds", 3i64);
    let tools = SettingsTools { settings: &w };
    let mut session = Session::new(&w, &tools, BUDGET);
    let mut ui = Rec::new(Answer::CannotAsk);
    let e = session.ask("x", &mut ui).unwrap_err();
    assert!(e.contains("mehr als 3 Werkzeug-Runden") && e.contains("ai.tool_rounds"), "{e}");
    assert_eq!(fake.hits().len(), 4);
    assert!(session.conv.messages.is_empty(), "a failed question leaves the conversation as it was");
}

#[test]
fn local_only_refuses_a_cloud_provider_before_any_connection() {
    let w = World::new("engine-local-only");
    let cloud = Fake::start(|h, _| by_path(h, "text"));
    let local = Fake::start(|h, _| by_path(h, "text"));
    // The cloud provider's address is the fake's port: had assist connected, the fake would have counted it.
    w.provider("wolke", "anthropic", &cloud.address("anthropic"), "cloud", Some(KEY));
    w.set("ai.role.chat", "wolke");
    w.set("ai.local_only", true);
    let (r, ui) = ask(&w, "Hallo", Answer::CannotAsk);
    let e = r.unwrap_err();
    assert!(e.contains("Nur lokal") && e.contains("wolke") && e.contains("ai.local_only"), "{e}");
    assert_eq!((cloud.connections(), cloud.hits().len()), (0, 0), "no connection was opened");
    assert!(ui.text.is_empty());

    // With a local fallback the chat goes there, and the cloud one is still never contacted.
    w.provider("daheim", "openai_compatible", &local.address("openai_compatible"), "local", None);
    w.set("ai.role.chat_fallback", "daheim");
    let (r, ui) = ask(&w, "Hallo", Answer::CannotAsk);
    assert_eq!(r, Ok(Stop::End));
    assert!(ui.text.contains("OPENAI-OK"));
    assert!(ui.notes.iter().any(|n| n.contains("Nur lokal") && n.contains("Ich nehme „daheim“")), "{:?}", ui.notes);
    assert_eq!((cloud.connections(), local.connections()), (0, 1));

    // Switched off, the cloud provider is refused all the same: plain http is for local providers only.
    w.set("ai.local_only", false);
    w.set("ai.role.chat_fallback", "");
    let (r, _) = ask(&w, "Hallo", Answer::CannotAsk);
    assert!(r.unwrap_err().contains("http://"));
    assert_eq!(cloud.connections(), 0);
}

#[test]
fn plain_http_only_to_the_home_network() {
    let p = Provider { id: "x".into(), kind: Kind::OpenAiCompatible, address: "http://example.com/v1".into(), model: "m".into(), key: Secret::default(), place: Place::Local, max_tokens: 1 };
    let public = ["93.184.216.34:80".parse().unwrap()];
    let mixed = ["192.168.100.20:80".parse().unwrap(), "[2606:2800:220:1::1]:80".parse().unwrap()];
    let home = ["192.168.100.20:8080".parse().unwrap(), "[fd00::20]:8080".parse().unwrap()];
    assert!(config::admit_addrs(&p, false, false, &public).unwrap_err().contains("93.184.216.34"));
    assert!(config::admit_addrs(&p, false, false, &mixed).is_err(), "every address a name resolves to must be at home");
    assert!(config::admit_addrs(&p, false, false, &home).is_ok());
    assert!(config::admit_addrs(&p, true, false, &public).is_ok(), "https may go anywhere Georg said");
    // "Nur lokal" goes by the address, not by the label: https to the outside is refused too.
    assert!(config::admit_addrs(&p, true, true, &public).unwrap_err().contains("Nur lokal"));
    assert!(config::admit_addrs(&p, true, true, &mixed).is_err());
    assert!(config::admit_addrs(&p, true, true, &home).is_ok() && config::admit_addrs(&p, false, true, &home).is_ok());
}

#[test]
fn local_only_goes_by_the_address_not_by_the_label() {
    let w = World::new("engine-local-label");
    // Labelled local, but the address is outside the home network (203.0.113.0/24 is reserved for
    // examples and routed nowhere: even a wrong program could reach nobody there).
    w.provider("falsch", "openai_compatible", "https://203.0.113.5/v1", "local", Some(KEY));
    w.set("ai.role.chat", "falsch");
    w.set("ai.local_only", true);
    let started = Instant::now();
    let (r, ui) = ask(&w, "Hallo", Answer::CannotAsk);
    let e = r.unwrap_err();
    assert!(e.contains("Nur lokal") && e.contains("falsch") && e.contains("203.0.113.5"), "{e}");
    // Refused before the connect: an attempt would have waited for an answer that never comes.
    assert!(started.elapsed() < Duration::from_secs(2), "{:?}", started.elapsed());
    assert!(ui.text.is_empty() && !ui.everything().contains(KEY));
}

/// An OpenAI-style stream with this text and finish reason.
fn openai_stream(text: &str, finish: &str) -> Vec<u8> {
    let delta = if text.is_empty() { Json::obj() } else { Json::obj().with("content", text) };
    let piece = |delta: Json, finish: Json| Json::obj().with("choices", vec![Json::obj().with("index", 0i64).with("delta", delta).with("finish_reason", finish)]).to_text();
    format!("data: {}\n\ndata: {}\n\ndata: [DONE]\n\n", piece(delta, Json::Null), piece(Json::obj(), Json::from(finish))).into_bytes()
}

#[test]
fn an_answer_that_ends_oddly_is_said_and_an_empty_one_is_not_kept() {
    let w = World::new("engine-odd-stop");
    let fake = Fake::start(|_, n| match n {
        1 => Reply::Sse(openai_stream("", "stop")),
        2 => Reply::Sse(openai_stream("Halb", "weird\nreason")),
        3 => Reply::Sse(openai_stream("Abgeschn", "length")),
        4 => Reply::Sse(openai_stream("", "content_filter")),
        _ => Reply::Sse(recording("openai-text.sse")),
    });
    w.provider("p", "openai_compatible", &fake.address("openai"), "local", None);
    w.set("ai.role.chat", "p");
    let tools = SettingsTools { settings: &w };
    let mut session = Session::new(&w, &tools, BUDGET);

    // Nothing at all: Georg is told, and neither the question nor an empty turn stays in the conversation.
    let mut ui = Rec::new(Answer::CannotAsk);
    assert!(matches!(session.ask("eins", &mut ui), Ok(Stop::Other(_))));
    assert_eq!(ui.notes, ["Der Anbieter hat eine leere Antwort geschickt."]);
    assert!(session.conv.messages.is_empty());

    // An unknown reason after some text: one German sentence, the raw reason on one line and in the log.
    let mut ui = Rec::new(Answer::CannotAsk);
    assert!(matches!(session.ask("zwei", &mut ui), Ok(Stop::Other(_))));
    assert_eq!(ui.notes, ["Der Anbieter hat die Antwort vorzeitig beendet; sie kann unvollständig sein (gemeldet: „weird\\nreason“)."]);
    assert!(ui.logs.iter().any(|l| l.starts_with("stop=")), "{:?}", ui.logs);

    let mut ui = Rec::new(Answer::CannotAsk);
    assert_eq!(session.ask("drei", &mut ui), Ok(Stop::MaxTokens));
    assert!(ui.notes[0].contains("ai.max_tokens"));
    let mut ui = Rec::new(Answer::CannotAsk);
    assert_eq!(session.ask("vier", &mut ui), Ok(Stop::Refusal));
    assert_eq!(ui.notes, ["Der Anbieter hat die Antwort abgelehnt."]);

    // The next request holds the two answered questions and no assistant turn without content.
    let mut ui = Rec::new(Answer::CannotAsk);
    assert_eq!(session.ask("fünf", &mut ui), Ok(Stop::End));
    let hits = fake.hits();
    let sent = hits[4].json();
    let Some(Json::Arr(messages)) = sent.get("messages") else { panic!("messages") };
    let turns: Vec<(&str, String)> = messages.iter().map(|m| (m.str("role").unwrap(), m.get("content").map(Json::to_text).unwrap_or_default())).collect();
    let said: Vec<(&str, &str)> = turns.iter().skip(1).map(|(r, c)| (*r, c.as_str())).collect();
    assert_eq!(said, [("user", "\"zwei\""), ("assistant", "\"Halb\""), ("user", "\"drei\""), ("assistant", "\"Abgeschn\""), ("user", "\"fünf\"")]);
}

#[test]
fn nothing_is_configured_by_default() {
    let w = World::new("engine-empty");
    let (r, _) = ask(&w, "Hallo", Answer::CannotAsk);
    let how = r.unwrap_err();
    for part in ["kein KI-Anbieter eingerichtet", "assist --add NAME", "ai.provider.NAME.kind", "ai.provider.NAME.place", "assist --key NAME", "ai.role.chat NAME"] {
        assert!(how.contains(part), "{part} missing in:\n{how}");
    }
    // A provider that is only half set up names what is missing; place is never guessed.
    let fake = Fake::start(|h, _| by_path(h, "text"));
    w.provider("p", "openai_compatible", &fake.address("openai"), "unset", None);
    let (r, _) = ask(&w, "Hallo", Answer::CannotAsk);
    assert!(r.unwrap_err().contains("settings set ai.role.chat NAME"));
    w.set("ai.role.chat", "p");
    let (r, _) = ask(&w, "Hallo", Answer::CannotAsk);
    let e = r.unwrap_err();
    assert!(e.contains("ai.provider.p.place") && e.contains("rät es nicht"), "{e}");
    w.set("ai.role.chat", "nobody");
    assert!(ask(&w, "Hallo", Answer::CannotAsk).0.unwrap_err().contains("nicht eingerichtet"));
    assert_eq!(fake.connections(), 0);
}

fn one_provider(name: &str, handler: impl Fn(&Hit, usize) -> Reply + Send + Sync + 'static) -> (World, Fake) {
    let w = World::new(name);
    let fake = Fake::start(handler);
    w.provider("p", "anthropic", &fake.address("anthropic"), "local", Some(KEY));
    w.set("ai.role.chat", "p");
    (w, fake)
}

#[test]
fn error_statuses_become_one_german_sentence_and_a_log_line() {
    // A provider that echoes the key it was sent, as some do in their 401.
    let (w, _fake) = one_provider("engine-401", |h, _| {
        let body = format!(r#"{{"type":"error","error":{{"type":"authentication_error","message":"invalid x-api-key: {}"}}}}"#, h.header("x-api-key").unwrap_or(""));
        Reply::Status(401, vec![], body)
    });
    let (r, ui) = ask(&w, "Hallo", Answer::CannotAsk);
    let e = r.unwrap_err();
    assert!(e.contains("lehnt den Schlüssel ab") && e.contains("assist --key p"), "{e}");
    assert_eq!(ui.logs.len(), 1);
    assert!(ui.logs[0].contains("provider=p http=401 kind=Key") && ui.logs[0].contains("authentication_error") && ui.logs[0].contains("***"), "{:?}", ui.logs);
    assert!(!format!("{e}\n{}", ui.everything()).contains(KEY), "the echoed key is scrubbed");
    assert!(!w.everything_settingsd_kept().contains(KEY));

    let (w, _fake) = one_provider("engine-404", |_, _| Reply::Status(404, vec![], r#"{"type":"error","error":{"type":"not_found_error","message":"model: model-rec"}}"#.into()));
    let e = ask(&w, "Hallo", Answer::CannotAsk).0.unwrap_err();
    assert!(e.contains("kennt das Modell „model-rec“ nicht") && e.contains("ai.provider.p.model"), "{e}");

    // A web page instead of an API: wrong address or kind.
    let (w, _fake) = one_provider("engine-html", |_, _| Reply::Status(302, vec![("Location", "https://login.example/".into())], String::new()));
    let (r, ui) = ask(&w, "Hallo", Answer::CannotAsk);
    assert!(r.unwrap_err().contains("nicht zu lesen"));
    assert!(ui.logs[0].contains("redirect"), "a redirect is never followed: {:?}", ui.logs);
}

#[test]
fn a_rate_limit_is_waited_out_a_bounded_number_of_times() {
    let limited = || Reply::Status(429, vec![("Retry-After", "1".into())], r#"{"type":"error","error":{"type":"rate_limit_error","message":"slow down"}}"#.into());
    // Once "later", then the answer: the pause the provider named is kept.
    let (w, fake) = one_provider("engine-429-once", move |_, n| if n == 1 { limited() } else { Reply::Sse(recording("anthropic-text.sse")) });
    let t = Instant::now();
    let (r, ui) = ask(&w, "Hallo", Answer::CannotAsk);
    assert_eq!(r, Ok(Stop::End));
    assert!(t.elapsed() >= Duration::from_secs(1), "retry-after was honoured");
    assert!(ui.text.contains("ANTHROPIC-OK") && ui.notes.iter().any(|n| n.contains("bittet um eine Pause") && n.contains("1 s")), "{:?}", ui.notes);
    assert_eq!(fake.hits().len(), 2);

    // Always "later": ai.retries more tries, then the sentence.
    let (w, fake) = one_provider("engine-429-always", move |_, _| Reply::Status(429, vec![("retry-after", "0".into())], "{}".into()));
    w.set("ai.retries", 2i64);
    let e = ask(&w, "Hallo", Answer::CannotAsk).0.unwrap_err();
    assert!(e.contains("bittet um eine Pause"), "{e}");
    assert_eq!(fake.hits().len(), 3, "one try and two more");

    // A pause longer than ai.timeout_s is reported at once, not sat out.
    let (w, fake) = one_provider("engine-429-long", move |_, _| Reply::Status(429, vec![("retry-after", "3600".into())], "{}".into()));
    w.set("ai.timeout_s", 5i64);
    let t = Instant::now();
    let e = ask(&w, "Hallo", Answer::CannotAsk).0.unwrap_err();
    assert!(e.contains("in 3600 Sekunden"), "{e}");
    assert!(fake.hits().len() == 1 && t.elapsed() < Duration::from_secs(3));

    // Overloaded (529) is tried again the same way.
    let (w, fake) = one_provider("engine-529", move |_, n| {
        if n == 1 { Reply::Status(529, vec![], r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#.into()) } else { Reply::Sse(recording("anthropic-text.sse")) }
    });
    let (r, ui) = ask(&w, "Hallo", Answer::CannotAsk);
    assert_eq!(r, Ok(Stop::End));
    assert!(ui.notes.iter().any(|n| n.contains("überlastet")) && fake.hits().len() == 2);
}

#[test]
fn a_cut_stream_a_silent_provider_and_esc() {
    // Cut inside the chunked body, and cut by a close without framing: the same sentence.
    for (name, cut) in [("chunked", true), ("close", false)] {
        let (w, _fake) = one_provider(&format!("engine-cut-{name}"), move |_, _| if cut { Reply::Cut(recording("anthropic-cut.sse")) } else { Reply::Close(recording("anthropic-cut.sse")) });
        let (r, ui) = ask(&w, "Hallo", Answer::CannotAsk);
        assert!(r.unwrap_err().contains("mittendrin abgebrochen"), "{name}");
        assert_eq!(ui.text, "Dieser Satz wird mitten", "{name}");
    }
    // A complete answer without framing is fine.
    let (w, _fake) = one_provider("engine-close-ok", |_, _| Reply::Close(recording("anthropic-text.sse")));
    assert_eq!(ask(&w, "Hallo", Answer::CannotAsk).0, Ok(Stop::End));

    // A provider that starts and then never ends is cut by ai.timeout_s.
    let (w, _fake) = one_provider("engine-hang", |_, _| Reply::Hang(recording("anthropic-cut.sse")));
    w.set("ai.timeout_s", 1i64);
    let t = Instant::now();
    let (r, ui) = ask(&w, "Hallo", Answer::CannotAsk);
    let e = r.unwrap_err();
    assert!(e.contains("nicht rechtzeitig") && e.contains("ai.timeout_s"), "{e}");
    assert!(t.elapsed() >= Duration::from_secs(1) && t.elapsed() < Duration::from_secs(4), "{:?}", t.elapsed());
    assert_eq!(ui.text, "Dieser Satz wird mitten");

    // Esc during the answer stops it at once.
    let (w, _fake) = one_provider("engine-esc", |_, _| Reply::Hang(recording("anthropic-cut.sse")));
    let tools = SettingsTools { settings: &w };
    let mut ui = Rec::new(Answer::CannotAsk);
    ui.cancel_at = Some(5);
    let t = Instant::now();
    assert_eq!(Session::new(&w, &tools, BUDGET).ask("Hallo", &mut ui), Err("Abgebrochen.".to_string()));
    assert!(t.elapsed() < Duration::from_secs(3));
}

#[test]
fn the_fallback_answers_when_the_first_provider_is_away() {
    let w = World::new("engine-fallback");
    let second = Fake::start(|h, _| by_path(h, "text"));
    // A port nobody listens on: bound, its number taken, closed again.
    let dead = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap();
    w.provider("erster", "anthropic", &format!("http://{dead}"), "local", Some(KEY));
    w.provider("zweiter", "gemini", &second.address("gemini"), "local", Some(KEY));
    w.set("ai.role.chat", "erster");
    let e = ask(&w, "Hallo", Answer::CannotAsk).0.unwrap_err();
    assert!(e.contains("„erster“ ist nicht erreichbar"), "{e}");
    w.set("ai.role.chat_fallback", "zweiter");
    let (r, ui) = ask(&w, "Hallo", Answer::CannotAsk);
    assert_eq!(r, Ok(Stop::End));
    assert!(ui.text.contains("GEMINI-OK"));
    assert!(ui.notes.iter().any(|n| n.contains("nicht erreichbar") && n.contains("Ich nehme „zweiter“")), "{:?}", ui.notes);

    // A wrong key is not "away": the fallback stays out of it.
    let bad = Fake::start(|_, _| Reply::Status(401, vec![], "{}".into()));
    // A stored key does not move with the address (settingsd, INT-CL3T): it is removed, the address changed,
    // the key entered again.
    let moved = settings::set(&w, "ai.provider.erster.address", bad.address("anthropic").as_str(), "test").unwrap_err();
    assert_eq!(moved.code, "key_bound", "{moved}");
    w.set("ai.provider.erster.key", "");
    w.set("ai.provider.erster.address", bad.address("anthropic").as_str());
    w.set("ai.provider.erster.key", KEY);
    let before = second.hits().len();
    assert!(ask(&w, "Hallo", Answer::CannotAsk).0.unwrap_err().contains("Schlüssel"));
    assert_eq!(second.hits().len(), before);
}

#[test]
fn an_answer_larger_than_the_machine_can_hold_is_stopped() {
    let (w, _fake) = one_provider("engine-budget", |_, _| Reply::Sse(recording("anthropic-text.sse")));
    let tools = SettingsTools { settings: &w };
    let mut ui = Rec::new(Answer::CannotAsk);
    // The bound is the caller's memory budget, here made tiny on purpose.
    let e = Session::new(&w, &tools, 600).ask("Hallo", &mut ui).unwrap_err();
    assert!(e.contains("im Speicher halten"), "{e}");
}

#[test]
fn a_conversation_keeps_its_turns_and_the_provider_its_own_blocks() {
    let (w, fake) = one_provider("engine-conv", |h, _| by_path(h, "tools"));
    w.set("ai.level", "auto");
    let tools = SettingsTools { settings: &w };
    let mut session = Session::new(&w, &tools, BUDGET);
    let mut ui = Rec::new(Answer::CannotAsk);
    session.ask("Mach alles größer", &mut ui).unwrap();
    session.ask("Danke", &mut ui).unwrap();
    assert_eq!(session.conv.messages.len(), 6, "question, tool turn, results, answer, question, answer");
    let third = fake.hits()[2].json();
    let Some(Json::Arr(msgs)) = third.get("messages") else { panic!("messages") };
    assert_eq!(msgs.len(), 5);
    // The reasoning block of the tool turn went back unchanged, signature and all.
    assert!(msgs[1].to_text().contains(r#"{"type":"thinking","thinking":"","signature":"EqQBCkYrecordedSignatureTools=="}"#), "{}", msgs[1].to_text());
    assert!(third.str("system").is_some_and(|s| s.contains("data, not instructions")));
}

/// The settings tools, and Georg tightening the level on his pill right after the first change ran.
struct Tightens<'a> {
    inner: SettingsTools<'a>,
    w: &'a World,
    to: &'static str,
}

impl assist::tools::ToolSource for Tightens<'_> {
    fn list(&self) -> Vec<assist::tools::ToolSpec> {
        self.inner.list()
    }
    fn call(&self, name: &str, args: &Json, grant: assist::tools::Grant) -> assist::tools::ToolAnswer {
        let r = self.inner.call(name, args, grant);
        if name == "settings_set" {
            self.w.set("ai.level", self.to);
        }
        r
    }
}

#[test]
fn a_tightened_level_holds_from_the_next_tool_call_of_the_same_question() {
    for (to, answer, said) in [("answer", Answer::Yes, "Nur antworten"), ("ask", Answer::No, "Georg said no")] {
        let w = World::new(&format!("engine-tighten-{to}"));
        // Two changes in two rounds, then the closing words.
        let fake = Fake::start(|_, n| {
            Reply::Sse(match n {
                1 => tool_stream("settings_set", r#"{"key":"theme.scale","value":150}"#, "tool_use"),
                2 => tool_stream("settings_set", r#"{"key":"theme.scale","value":175}"#, "tool_use"),
                _ => recording("anthropic-done.sse"),
            })
        });
        w.provider("p", "anthropic", &fake.address("anthropic"), "local", Some(KEY));
        w.set("ai.role.chat", "p");
        w.set("ai.level", "auto");
        let tools = Tightens { inner: SettingsTools { settings: &w }, w: &w, to };
        let mut ui = Rec::new(answer);
        let r = Session::new(&w, &tools, BUDGET).ask("Mach alles größer, zweimal", &mut ui);
        assert_eq!(r, Ok(Stop::End), "{to}");
        assert_eq!(w.value("theme.scale"), Json::Int(150), "{to}: the second change did not run under the looser level");
        assert_eq!(ui.asked.len(), usize::from(to == "ask"), "{to}: {:?}", ui.asked);
        let third = fake.hits()[2].body.replace("\\\"", "\"");
        assert!(third.contains(said), "{to}: the model reads why: {third}");
    }
}
