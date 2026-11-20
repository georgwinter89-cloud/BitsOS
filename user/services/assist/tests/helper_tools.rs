//! Version 2 of the helper contract (HELPER.md): a helper tells what its tools do, and asks before one does
//! something. Small helper scripts behind a real engine and a real settingsd: tool lines; a question
//! answered yes, no, not at all, and where nobody can be asked; the one level (K2); a helper that lies about
//! what its tool is; a stranger at the bell; silence and signs of life; and that nothing secret goes in or
//! stays. (A helper of version 1, which reads its stdin to the end and never asks, is every test of
//! tests/program.rs: they run unchanged.)

mod common;
use assist::conv::Stop;
use assist::engine::{Answer, Session};
use assist::json::{self, Json};
use assist::settings;
use assist::tools::settings::SettingsTools;
use common::stage::{Stage, asking};
use common::{KEY, Rec};
use settingsd::peer::Peer;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const BUDGET: usize = 64 << 20;
const DONE: &str = r#"echo '{"done":true,"stop":"end","error":null}'"#;

fn ask(s: &Stage, home: Option<&Path>, answer: Answer) -> (Result<Stop, String>, Rec) {
    let tools = SettingsTools { settings: &s.w };
    let mut session = Session::new(&s.w, &tools, BUDGET);
    session.home = home.map(Path::to_path_buf);
    let mut ui = Rec::new(answer);
    let r = session.ask("Mach mal", &mut ui);
    (r, ui)
}

/// Georg's own hand at the settings: root at the console, confirmed.
fn georg_sets(s: &Stage, key: &str, value: &str) {
    let a = s.w.request_as(&Peer::program(0, "/bin/settings"), &Json::obj().with("op", "set").with("key", key).with("value", value).with("confirmed", true));
    assert_eq!(a.get("ok").and_then(Json::as_bool), Some(true), "{}", a.to_text());
}

fn lines(s: &Stage, file: &str) -> Vec<Json> {
    s.file(file).lines().map(|l| json::parse(l).unwrap_or_else(|e| panic!("{e}: {l}"))).collect()
}

#[test]
fn tool_lines_are_shown_and_not_kept_in_the_conversation() {
    let s = Stage::new("helper2-lines");
    let long = "x".repeat(1000);
    s.helper(&format!(
        r#"n=$(cat calls 2>/dev/null || echo 0); n=$((n+1)); echo $n > calls
cat > request.$n
echo '{{"tool":"Bash","state":"runs","text":"ls -la"}}'
echo '{{"tool":"Bash","state":"done","text":""}}'
echo '{{"tool":"Lesen\nJa","state":"failed","text":"{long}"}}'
echo '{{"tool":"Morgen","state":"schwebt","text":"x"}}'
echo '{{"text":"Fertig."}}'
{DONE}"#
    ));
    let tools = SettingsTools { settings: &s.w };
    let mut session = Session::new(&s.w, &tools, BUDGET);
    let mut ui = Rec::new(Answer::CannotAsk);
    assert_eq!(session.ask("Zeig die Dateien", &mut ui), Ok(Stop::End));
    assert_eq!(ui.text, "Fertig.");
    assert_eq!(ui.tools.len(), 3, "a state of a later version is not shown: {:?}", ui.tools);
    assert_eq!(ui.tools[..2], ["Bash:Runs:ls -la", "Bash:Done:"]);
    // A name or a text from the helper stays one bounded line.
    assert!(ui.tools[2].starts_with("Lesen\\nJa:Failed:xxxx") && ui.tools[2].ends_with("(gekürzt, 1000 Zeichen)") && ui.tools[2].len() < 500, "{}", ui.tools[2]);
    assert_eq!(ui.notes, ["ls -la"], "the console shows a tool at work as a line");
    assert!(ui.asked.is_empty());
    assert_eq!(session.ask("Und nun?", &mut ui), Ok(Stop::End));
    let second = json::parse(s.file("request.2").trim()).unwrap();
    assert_eq!(
        second.get("messages").map(Json::to_text).as_deref(),
        Some(r#"[{"role":"user","text":"Zeig die Dateien"},{"role":"assistant","text":"Fertig."},{"role":"user","text":"Und nun?"}]"#)
    );
}

#[test]
fn a_question_is_georgs_to_answer() {
    let cases = [
        (Answer::Yes, "yes", None, "Bash:Confirmed:Befehl ausführen: touch x"),
        (Answer::No, "no", Some(("declined", "Georg hat Nein gesagt")), "Bash:Declined:Befehl ausführen: touch x"),
        (Answer::Unanswered, "no", Some(("unanswered", "nicht geantwortet")), "Bash:Declined:Befehl ausführen: touch x"),
        (Answer::CannotAsk, "no", Some(("cannot_ask", "kann ihn niemand fragen")), "Bash:Refused:Nicht ausgeführt, weil niemand gefragt werden kann: Befehl ausführen: touch x"),
    ];
    for (answer, word, why, step) in cases {
        let s = Stage::new(&format!("helper2-ask-{answer:?}"));
        s.helper(&format!("{}ask '{{\"ask\":\"Bash\",\"id\":\"t1\",\"kind\":\"run\",\"text\":\"touch x\"}}' > reply\necho '{{\"text\":\"ok\"}}'\n{DONE}", asking()));
        let (r, ui) = ask(&s, None, answer);
        assert_eq!((r, ui.text.as_str()), (Ok(Stop::End), "ok"), "{answer:?} {:?}", ui.logs);
        assert_eq!(ui.asked, ["Befehl ausführen: touch x"], "{answer:?}");
        assert_eq!(ui.tools, [step], "{answer:?}");
        let reply = &lines(&s, "reply")[0];
        assert_eq!((reply.str("id"), reply.str("answer")), (Some("t1"), Some(word)), "{}", reply.to_text());
        match why {
            None => assert!(reply.get("why").is_none()),
            Some((code, words)) => assert!(reply.str("why") == Some(code) && reply.str("text").is_some_and(|t| t.contains(words)), "{}", reply.to_text()),
        }
    }
}

#[test]
fn the_level_decides_for_the_helpers_own_tools_and_only_georg_loosens_it() {
    let s = Stage::new("helper2-levels");
    s.helper(&format!("{}ask '{{\"ask\":\"Bash\",\"kind\":\"run\",\"text\":\"touch x\"}}' >> reply\nask '{{\"ask\":\"Write\",\"kind\":\"write\",\"path\":\"/tmp/x\"}}' >> reply\necho '{{\"text\":\"ok\"}}'\n{DONE}", asking()));
    // Fragt vorher: both are asked, and a no is a no.
    let (r, ui) = ask(&s, None, Answer::No);
    assert_eq!(r, Ok(Stop::End), "{:?}", ui.logs);
    assert_eq!(ui.asked, ["Befehl ausführen: touch x", "Datei schreiben oder ändern: /tmp/x"]);
    let req = json::parse(s.file("request").trim()).unwrap();
    assert_eq!(req.get("allow").map(Json::to_text).as_deref(), Some(r#"{"read":"always","write":"ask","run":"ask","net":"ask","other":"ask"}"#));
    assert_eq!((req.str("level"), req.get("bitsos_helper").and_then(Json::as_i64)), (Some("ask"), Some(5)));
    // The assistant's own program cannot loosen the level, confirmed or not; tightening is anyone's.
    let loosen = Json::obj().with("op", "set").with("key", "ai.level").with("value", "auto").with("confirmed", true);
    assert_eq!(s.w.request_as(&s.w.me, &loosen).str("error"), Some("not_confirmer"));
    assert_eq!(settings::set(&s.w, "ai.level", "auto", "test").unwrap_err().code, "not_confirmer");
    settings::set(&s.w, "ai.level", "answer", "test").unwrap();
    assert_eq!(settings::set(&s.w, "ai.level", "ask", "test").unwrap_err().code, "not_confirmer", "back up is loosening too");
    // Nur antworten: no bell, and the helper is told its own tools are off.
    std::fs::remove_file(s.dir.join("reply")).ok();
    let (_, ui) = ask(&s, None, Answer::Yes);
    let req = json::parse(s.file("request").trim()).unwrap();
    assert!(req.get("bell").is_none() && req.get("own_tools") == Some(&Json::Bool(false)) && req.str("level") == Some("answer"), "{}", req.to_text());
    assert!(ui.asked.is_empty());
    // Automodus, set by Georg's own hand: both run unasked.
    georg_sets(&s, "ai.level", "auto");
    std::fs::remove_file(s.dir.join("reply")).ok();
    let (_, ui) = ask(&s, None, Answer::No);
    assert!(ui.asked.is_empty(), "{:?}", ui.asked);
    assert_eq!(ui.tools, ["Bash:Runs:Befehl ausführen: touch x", "Write:Runs:Datei schreiben oder ändern: /tmp/x"]);
    let replies = lines(&s, "reply");
    assert_eq!((replies[0].str("answer"), replies[1].str("answer")), (Some("yes"), Some("yes")));
    let req = json::parse(s.file("request").trim()).unwrap();
    assert_eq!(req.get("allow").map(Json::to_text).as_deref(), Some(r#"{"read":"always","write":"always","run":"always","net":"always","other":"always"}"#));
}

#[test]
fn a_helper_that_lies_about_the_kind_is_asked_about() {
    let s = Stage::new("helper2-lies");
    let home = s.w.root.join("heim");
    std::fs::create_dir_all(home.join("Dokumente")).unwrap();
    let h = home.to_str().unwrap();
    // (kind, text, path, runs unasked at Fragt vorher): reading runs wherever it reads (K2), what cannot be
    // seen to be a read is asked.
    let asks: Vec<(&str, &str, Option<String>, bool)> = vec![
        ("read", "", Some(format!("{h}/Dokumente/a.txt")), true),
        ("read", "sucht „Rechnung“", Some(format!("{h}/Dokumente")), true),
        ("read", "", Some("/etc/passwd".into()), true),
        ("read", "rm -rf ~", None, false),
        ("read", "rm -rf ~", Some("  ".into()), false),
        ("fly", "abheben", None, false),
        ("net", "https://example.org/", None, false),
        ("write", "1 Zeile", Some(format!("{h}/Dokumente/b.txt")), false),
    ];
    let mut body = asking();
    for (kind, text, path, _) in &asks {
        let mut q = Json::obj().with("ask", "Read").with("kind", *kind).with("text", *text);
        if let Some(p) = path {
            q = q.with("path", p.as_str());
        }
        body.push_str(&format!("ask '{}' >> reply\n", q.to_text()));
    }
    body.push_str(&format!("ask '{{\"ask\":\"Read\",\"kind\":\"read\",\"must_ask\":true,\"path\":\"{h}/Dokumente/a.txt\"}}' >> reply\necho '{{\"text\":\"ok\"}}'\n{DONE}"));
    s.helper(&body);
    let (r, ui) = ask(&s, Some(&home), Answer::No);
    assert_eq!(r, Ok(Stop::End), "{:?}", ui.logs);
    let replies = lines(&s, "reply");
    assert_eq!(replies.len(), asks.len() + 1);
    for ((kind, text, path, unasked), reply) in asks.iter().zip(&replies) {
        assert_eq!(reply.str("answer"), Some(if *unasked { "yes" } else { "no" }), "{kind} {text:?} {path:?}");
    }
    assert_eq!(replies.last().unwrap().str("why"), Some("declined"), "must_ask asks whatever the level");
    assert_eq!(ui.asked.len(), asks.iter().filter(|a| !a.3).count() + 1, "{:?}", ui.asked);
    // What a lie looks like to Georg: the kind's word and the helper's data, as they are.
    assert!(ui.asked.contains(&"Lesen: rm -rf ~".to_string()), "{:?}", ui.asked);
    assert!(ui.asked.contains(&"Werkzeug „Read“ benutzen: abheben".to_string()), "a kind nobody knows is `other`: {:?}", ui.asked);
    assert!(ui.tools.contains(&format!("Read:Runs:Lesen: {h}/Dokumente (sucht „Rechnung“)")), "{:?}", ui.tools);
    // In Automodus only must_ask still asks.
    georg_sets(&s, "ai.level", "auto");
    std::fs::remove_file(s.dir.join("reply")).unwrap();
    let (_, ui) = ask(&s, Some(&home), Answer::No);
    assert_eq!(ui.asked, [format!("Lesen: {h}/Dokumente/a.txt")]);
    let replies = lines(&s, "reply");
    assert!(replies[..asks.len()].iter().all(|r| r.str("answer") == Some("yes")), "{replies:?}");
}

#[test]
fn a_command_too_long_to_be_shown_whole_is_not_asked() {
    // What a card would hide at its end: the harmless start fills it.
    let sly = format!("echo {}; rm -rf ~", "a".repeat(1300));
    let fits = format!("echo {}", "a".repeat(1100));
    let mut body = asking();
    for (kind, text, must) in [("run", sly.as_str(), false), ("net", sly.as_str(), false), ("other", sly.as_str(), true), ("run", fits.as_str(), false)] {
        let q = Json::obj().with("ask", "Bash").with("kind", kind).with("text", text).with("must_ask", must);
        body.push_str(&format!("ask '{}' >> reply\n", q.to_text()));
    }
    body.push_str(&format!("echo '{{\"text\":\"ok\"}}'\n{DONE}"));
    // Fragt vorher: what is too long to be shown is refused without a card; what fits is asked.
    let s = Stage::new("helper2-long");
    s.helper(&body);
    let (r, ui) = ask(&s, None, Answer::Yes);
    assert_eq!(r, Ok(Stop::End), "{:?}", ui.logs);
    let replies = lines(&s, "reply");
    assert_eq!(replies.iter().map(|r| r.str("why").unwrap_or("yes")).collect::<Vec<_>>(), ["too_long", "too_long", "too_long", "yes"]);
    assert!(replies[0].str("text").is_some_and(|t| t.contains(&assist::tools::helper::COMMAND_MAX.to_string())));
    assert_eq!(ui.asked.len(), 1, "only what fits reaches the card: {:?}", ui.asked);
    assert!(ui.tools.iter().filter(|t| t.starts_with("Bash:Refused:Nicht gefragt")).count() == 3, "{:?}", ui.tools);
    // Automodus shows nothing, so length does not matter; a question that must be asked still must be shown.
    let s = Stage::new("helper2-long-auto");
    georg_sets(&s, "ai.level", "auto");
    s.helper(&body);
    let (_, ui) = ask(&s, None, Answer::Yes);
    let replies = lines(&s, "reply");
    assert_eq!(replies.iter().map(|r| r.str("why").unwrap_or("yes")).collect::<Vec<_>>(), ["yes", "yes", "too_long", "yes"]);
    assert!(ui.asked.is_empty(), "{:?}", ui.asked);
}

#[test]
fn only_the_helper_and_what_it_started_is_heard_at_the_bell() {
    let s = Stage::new("helper2-stranger");
    // The inner shell loses its parent at once and belongs to nobody: it rings as a stranger.
    s.helper(&format!(
        "{}( ( sleep 0.2; ask '{{\"ask\":\"Bash\",\"kind\":\"run\",\"text\":\"fremd\"}}' > stranger ) & )\nsleep 1\n( ask '{{\"ask\":\"Bash\",\"kind\":\"run\",\"text\":\"eigen\"}}' > own )\necho '{{\"text\":\"ok\"}}'\n{DONE}",
        asking()
    ));
    let (r, ui) = ask(&s, None, Answer::Yes);
    assert_eq!(r, Ok(Stop::End), "{:?}", ui.logs);
    assert_eq!(ui.asked, ["Befehl ausführen: eigen"], "the stranger's question never reached Georg");
    assert_eq!(lines(&s, "own")[0].str("answer"), Some("yes"));
    assert_eq!(lines(&s, "stranger")[0].str("answer"), Some("no"));
    assert!(ui.logs.iter().any(|l| l.contains("bell: turned away") && l.contains("does not descend from the helper")), "{:?}", ui.logs);
    // The bell is gone with the answer, and its folder too.
    let bell = json::parse(s.file("request").trim()).unwrap().str("bell").unwrap().to_string();
    assert!(!Path::new(&bell).exists() && !Path::new(&bell).parent().unwrap().exists());
}

#[test]
fn silence_ends_a_helper_and_every_sign_of_life_renews_its_time() {
    let s = Stage::new("helper2-silence");
    s.w.set("ai.timeout_s", 1i64);
    // Two and a half seconds of work, with a line now and then: an empty object, a tool line, a question.
    s.helper(&format!(
        "{}for i in 1 2 3; do sleep 0.4; echo '{{}}'; done\nsleep 0.4; echo '{{\"tool\":\"Bash\",\"state\":\"runs\",\"text\":\"lang\"}}'\nsleep 0.4; ask '{{\"ask\":\"Bash\",\"kind\":\"run\",\"text\":\"x\"}}' > reply\nsleep 0.6\necho '{{\"text\":\"geschafft\"}}'\n{DONE}",
        asking()
    ));
    let started = Instant::now();
    let (r, ui) = ask(&s, None, Answer::Yes);
    assert_eq!((r, ui.text.as_str()), (Ok(Stop::End), "geschafft"), "{:?}", ui.logs);
    assert!(started.elapsed() > Duration::from_secs(2), "{:?}", started.elapsed());
    // After a sign of life the same second of silence ends it.
    s.helper(&format!("{}echo '{{}}'\necho $$ > pid\nsleep 30", asking()));
    let started = Instant::now();
    let (r, _) = ask(&s, None, Answer::Yes);
    assert!(r.unwrap_err().contains("nicht rechtzeitig geantwortet") && started.elapsed() < Duration::from_secs(6));
    assert!(s.gone("pid"));
}

#[test]
fn nothing_secret_goes_to_a_helper_and_nothing_of_its_tools_is_kept() {
    let s = Stage::new("helper2-secret");
    s.w.provider("wolke", "anthropic", "https://api.example.com", "cloud", Some(KEY));
    let secret = "GEHEIM-AUS-EINER-DATEI";
    s.helper(&format!(
        "{}echo '{{\"tool\":\"Bash\",\"state\":\"done\",\"text\":\"cat schluessel: {secret}\"}}'\nask '{{\"ask\":\"Bash\",\"kind\":\"run\",\"text\":\"echo {secret}\"}}' > reply\necho '{{\"text\":\"ok\"}}'\n{DONE}",
        asking()
    ));
    let before = s.w.events.lock().unwrap().len();
    let (r, ui) = ask(&s, None, Answer::Yes);
    assert_eq!(r, Ok(Stop::End), "{:?}", ui.logs);
    // The request: no key of any provider, and none was asked for.
    assert!(!s.file("request").contains(KEY) && !ui.everything().contains(KEY));
    assert!(!s.w.events.lock().unwrap()[before..].iter().any(|e| e.contains(".key")), "{:?}", s.w.events.lock().unwrap());
    // What a tool of the helper showed is on Georg's screen and nowhere in the settings' records or the log.
    assert!(ui.tools.iter().any(|t| t.contains(secret)) && ui.asked[0].contains(secret));
    assert!(!ui.logs.iter().any(|l| l.contains(secret)), "{:?}", ui.logs);
    let kept = s.w.everything_settingsd_kept();
    assert!(!kept.contains(secret) && !kept.contains(KEY));
}

/// A helper that hands BitsOS's own tools on: it starts `assist --mcp --bell` and makes two calls, one that
/// reads and one that changes.
fn mcp_helper(s: &Stage) -> String {
    let call = |id: u32, name: &str, args: &str| format!(r#"{{"jsonrpc":"2.0","id":{id},"method":"tools/call","params":{{"name":"{name}","arguments":{args}}}}}"#);
    format!(
        "{}printf '%s\\n' '{}' '{}' | '{}' --socket '{}' --mcp --bell \"$bell\" > mcp.out 2> mcp.err\necho '{{\"text\":\"ok\"}}'\n{DONE}",
        asking(),
        call(1, "settings_get", r#"{"key":"theme.scale"}"#),
        call(2, "settings_set", r#"{"key":"theme.scale","value":150}"#),
        env!("CARGO_BIN_EXE_assist"),
        s.w.socket.display()
    )
}

fn mcp_answers(s: &Stage) -> (String, String) {
    let out = lines(s, "mcp.out");
    let text = |j: &Json| j.get("result").and_then(|r| r.get("content")).map(Json::to_text).unwrap_or_default();
    assert_eq!(out.len(), 2, "{}", s.file("mcp.err"));
    (text(&out[0]), text(&out[1]))
}

#[test]
fn bitsos_own_tools_under_a_helper_ask_through_the_assistant_that_started_it() {
    let s = Stage::new("helper2-mcp");
    s.w.listen();
    s.helper(&mcp_helper(&s));
    // The real program: assist -> the helper -> assist --mcp. The reading call runs. The changing one is
    // put to the assistant above, which here (`--once`) has nobody to ask, and says so.
    let out = Command::new(env!("CARGO_BIN_EXE_assist")).arg("--socket").arg(&s.w.socket).args(["--once", "Mach alles größer"]).stdin(Stdio::null()).output().unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!((out.status.code(), String::from_utf8_lossy(&out.stdout).as_ref()), (Some(0), "ok\n"), "{stderr}");
    let (read, change) = mcp_answers(&s);
    assert!(read.contains("125"), "{read}");
    assert!(change.contains("kann ihn niemand fragen"), "{change}");
    assert!(stderr.contains("Nicht ausgeführt, weil niemand gefragt werden kann: Einstellung „theme.scale“ ändern: 125 → 150"), "the question reached the assistant's screen: {stderr}");
    assert!(!s.file("mcp.err").contains("not usable"), "{}", s.file("mcp.err"));
    assert_eq!(s.w.value("theme.scale"), Json::Int(125));

    // A bell that is not the assistant's (here: this test, which is not the program `assist`): the server
    // does not ring it, whatever would be answered there, and nothing changes.
    let (r, ui) = ask(&s, None, Answer::Yes);
    assert_eq!(r, Ok(Stop::End), "{:?}", ui.logs);
    let (read, change) = mcp_answers(&s);
    assert!(read.contains("125") && change.contains("kann ihn niemand fragen"), "{change}");
    assert!(s.file("mcp.err").contains("not the assistant"), "{}", s.file("mcp.err"));
    assert!(ui.asked.is_empty());
    assert_eq!(s.w.value("theme.scale"), Json::Int(125));
}
