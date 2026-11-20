//! Provider kind `program`: small helper scripts, written here, behind a real engine and a real settingsd.
//! A helper that answers in pieces, refuses, reports an error, prints garbage, nothing or too much, never
//! ends, ignores SIGTERM; Esc; "Nur lokal"; what an address must be.

mod common;
use assist::conv::Stop;
use assist::engine::{Answer, Session};
use assist::json::{self, Json};
use assist::tools::settings::SettingsTools;
use common::stage::Stage;
use common::{Rec, World};
use std::io::Write;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const BUDGET: usize = 64 << 20;

fn ask(w: &World, text: &str) -> (Result<Stop, String>, Rec) {
    ask_with(w, text, Rec::new(Answer::CannotAsk), BUDGET)
}

fn ask_with(w: &World, text: &str, mut ui: Rec, budget: usize) -> (Result<Stop, String>, Rec) {
    let tools = SettingsTools { settings: w };
    let r = Session::new(w, &tools, budget).ask(text, &mut ui);
    (r, ui)
}

const DONE: &str = r#"echo '{"done":true,"stop":"end","error":null}'"#;

#[test]
fn a_helper_answers_in_pieces_and_gets_the_whole_conversation() {
    let s = Stage::new("program-pieces");
    s.helper(&format!(
        r#"echo "args=$#" > args; n=$(cat calls 2>/dev/null || echo 0); n=$((n+1)); echo $n > calls
cat > request.$n
echo "warming up" >&2
echo '{{"note":"denkt nach"}}'
echo '{{"text":"Hal"}}'
sleep 0.1
echo
echo '{{"usage":{{"words":2}}}}'
echo '{{"text":"lo Georg, Antwort '$n'"}}'
{DONE}
echo '{{"text":" (zu spät)"}}'"#
    ));
    let tools = SettingsTools { settings: &s.w };
    let mut session = Session::new(&s.w, &tools, BUDGET);
    let mut ui = Rec::new(Answer::CannotAsk);
    assert_eq!(session.ask("Wie geht's, äöü?", &mut ui), Ok(Stop::End));
    assert_eq!(ui.text, "Hallo Georg, Antwort 1", "nothing after done is read");
    assert_eq!(ui.notes, ["denkt nach"]);
    assert!(ui.logs.iter().any(|l| l == "provider=h helper: warming up"), "{:?}", ui.logs);
    assert!(!format!("{}{:?}", ui.text, ui.notes).contains("warming"), "stderr goes to the log, never to the screen");
    assert_eq!(s.file("args").trim(), "args=0", "no shell, no arguments");

    // One line: the protocol's version, the request's options, the conversation so far.
    let first = s.file("request.1");
    assert_eq!(first.matches('\n').count(), 1, "one line, then the end of stdin");
    let req = json::parse(first.trim()).unwrap();
    assert_eq!((req.get("bitsos_helper").and_then(Json::as_i64), req.str("provider"), req.str("model")), (Some(5), Some("h"), Some("")));
    assert_eq!(req.get("max_tokens").and_then(Json::as_i64), Some(64_000));
    // The one prompt without its paragraph about the tools: that one is `tools_system`, for a helper that hands
    // them on.
    assert!(req.str("system").is_some_and(|t| t.contains("BitsOS") && t.contains("Fragt vorher") && !t.contains("settings_") && !t.contains("befehl runs")), "{}", req.to_text());
    // Version 2: where to ask, what runs unasked, and the paragraph for a helper whose model has tools.
    assert!(req.str("bell").is_some_and(|b| b.starts_with('/') && b.contains("assist-bell-")), "{first}");
    assert_eq!(req.get("allow").map(Json::to_text).as_deref(), Some(r#"{"read":"always","write":"ask","run":"ask","net":"ask","other":"ask"}"#));
    assert!(req.str("system_tools").is_some_and(|t| t.contains("BitsOS asks Georg itself")) && req.str("home").is_some());
    assert!(!std::path::Path::new(req.str("bell").unwrap()).exists(), "the bell is gone with the answer");
    assert_eq!(req.get("messages").map(Json::to_text).as_deref(), Some(r#"[{"role":"user","text":"Wie geht's, äöü?"}]"#));

    // The second question carries the first one and its answer.
    s.w.set("ai.provider.h.model", "großes-modell");
    assert_eq!(session.ask("Und jetzt?", &mut ui), Ok(Stop::End));
    let req = json::parse(s.file("request.2").trim()).unwrap();
    assert_eq!(req.str("model"), Some("großes-modell"));
    assert_eq!(
        req.get("messages").map(Json::to_text).as_deref(),
        Some(r#"[{"role":"user","text":"Wie geht's, äöü?"},{"role":"assistant","text":"Hallo Georg, Antwort 1"},{"role":"user","text":"Und jetzt?"}]"#)
    );
    assert!(ui.text.ends_with("Hallo Georg, Antwort 2"));
    // No key exists for a helper and none was asked for.
    assert!(!s.w.events.lock().unwrap().iter().any(|e| e.contains("ai.provider.h.key")), "{:?}", s.w.events.lock().unwrap());
}

#[test]
fn how_a_helper_ends_an_answer() {
    let s = Stage::new("program-ends");
    let say = |stop_line: &str| {
        s.helper(&format!("cat > /dev/null\necho '{{\"text\":\"Teil\"}}'\necho '{stop_line}'"));
        ask(&s.w, "x")
    };
    let (r, ui) = say(r#"{"done":true,"stop":"refused"}"#);
    assert!(r == Ok(Stop::Refusal) && ui.notes.iter().any(|n| n.contains("abgelehnt")), "{r:?} {:?}", ui.notes);
    let (r, ui) = say(r#"{"done":true,"stop":"length"}"#);
    assert!(r == Ok(Stop::MaxTokens) && ui.notes.iter().any(|n| n.contains("abgeschnitten")), "{r:?}");
    let (r, ui) = say(r#"{"done":true,"stop":"pause"}"#);
    assert!(r == Ok(Stop::Other("pause".into())) && ui.notes.iter().any(|n| n.contains("vorzeitig beendet")), "{r:?}");
    assert_eq!(say(r#"{"done":true}"#).0, Ok(Stop::End));

    // An error is the helper's own German sentence, as one bounded line whatever it holds.
    let (r, ui) = say(r#"{"done":true,"stop":"error","error":"Der Dienst ist nicht angemeldet. Melde dich dort an."}"#);
    assert_eq!(r, Err("Der Dienst ist nicht angemeldet. Melde dich dort an.".into()));
    assert_eq!(ui.text, "Teil");
    let (r, _) = say(r#"{"done":true,"stop":"error"}"#);
    assert_eq!(r, Err("Das Hilfsprogramm von „h“ hat einen Fehler gemeldet, ohne ihn zu nennen.".into()));
    s.helper("cat > /dev/null\nprintf '{\"done\":true,\"stop\":\"error\",\"error\":\"a\\\\u001b[2Jb\\\\nc'; head -c 20000 /dev/zero | tr '\\0' 'x'; echo '\"}'");
    let e = ask(&s.w, "x").0.unwrap_err();
    assert!(e.starts_with("a\\u{1b}[2Jb\\nc") && e.chars().count() < 500 && e.contains("gekürzt"), "{e}");

    // An empty answer is said, and nothing of it is kept.
    s.helper(&format!("cat > /dev/null\n{DONE}"));
    let (r, ui) = ask(&s.w, "x");
    assert!(matches!(r, Ok(Stop::Other(_))) && ui.notes.iter().any(|n| n.contains("leere Antwort")), "{r:?} {:?}", ui.notes);
}

#[test]
fn garbage_nothing_and_too_much() {
    let s = Stage::new("program-garbage");
    let tools = SettingsTools { settings: &s.w };

    s.helper("cat > /dev/null\necho $$ > pid\necho 'Hallo, ich spreche kein JSON'\nsleep 30");
    let started = Instant::now();
    let (r, ui) = ask(&s.w, "x");
    assert_eq!(r, Err("Das Hilfsprogramm von „h“ hat etwas ausgegeben, das nicht zu lesen war.".into()));
    assert!(ui.logs.iter().any(|l| l.contains("ich spreche kein JSON")) && ui.text.is_empty(), "{:?}", ui.logs);
    assert!(started.elapsed() < Duration::from_secs(10) && s.gone("pid"), "it is not waited for");

    // Garbage after a piece of the answer: the piece was shown, the question is not kept.
    s.helper("cat > /dev/null\necho '{\"text\":\"Teil\"}'\necho '[1,2]'");
    let mut session = Session::new(&s.w, &tools, BUDGET);
    let mut ui = Rec::new(Answer::CannotAsk);
    assert!(session.ask("x", &mut ui).unwrap_err().contains("nicht zu lesen"));
    assert!(ui.text == "Teil" && session.conv.messages.is_empty());

    // Nothing at all, with a word on stderr and an exit code; and an end in the middle.
    s.helper("echo 'boom: no config' >&2\nexit 3");
    let (r, ui) = ask(&s.w, "x");
    assert_eq!(r, Err("Das Hilfsprogramm von „h“ hat sich beendet, ohne die Antwort abzuschließen.".into()));
    assert!(ui.logs.iter().any(|l| l.contains("boom: no config")), "{:?}", ui.logs);
    s.helper("cat > /dev/null\necho '{\"text\":\"Teil\"}'");
    assert!(ask(&s.w, "x").0.unwrap_err().contains("ohne die Antwort abzuschließen"));
    // It closes its output and stays: nothing more can come, so it is not waited for either.
    s.helper("echo $$ > pid\nexec > /dev/null 2>&1\nsleep 30");
    let started = Instant::now();
    assert!(ask(&s.w, "x").0.unwrap_err().contains("ohne die Antwort abzuschließen"));
    assert!(started.elapsed() < Duration::from_secs(10) && s.gone("pid"));

    // Too much: more than this machine is asked to hold (here 64 KiB) stops it, and the helper is ended.
    s.helper("cat > /dev/null\necho $$ > pid\nwhile :; do echo '{\"text\":\"xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx\"}'; done");
    let started = Instant::now();
    let (r, ui) = ask_with(&s.w, "x", Rec::new(Answer::CannotAsk), 64 * 1024);
    assert!(r.unwrap_err().contains("größer, als dieser Rechner"), "{:?}", ui.logs);
    assert!(ui.text.len() <= 64 * 1024 && started.elapsed() < Duration::from_secs(10) && s.gone("pid"));
    // One line without end is the same.
    s.helper("cat > /dev/null\nprintf '{\"text\":\"'\nhead -c 4000000 /dev/zero | tr '\\0' 'x'\nsleep 30");
    assert!(ask_with(&s.w, "x", Rec::new(Answer::CannotAsk), 64 * 1024).0.unwrap_err().contains("größer, als dieser Rechner"));

    // A flood on stderr neither blocks the helper nor fills the log.
    s.helper(&format!("cat > /dev/null\ni=0; while [ $i -lt 20000 ]; do echo \"noise $i 0123456789012345678901234567890123456789\" >&2; i=$((i+1)); done\necho '{{\"text\":\"ok\"}}'\n{DONE}"));
    let (r, ui) = ask(&s.w, "x");
    assert!(r == Ok(Stop::End) && ui.text == "ok" && ui.logs.len() < 250, "{r:?} {}", ui.logs.len());
    assert!(ui.logs.iter().any(|l| l.contains("more lines not kept")), "{:?}", ui.logs.last());

    // A helper that never reads its stdin still answers, however long the question.
    s.helper(&format!("echo '{{\"text\":\"ungelesen\"}}'\n{DONE}"));
    let (r, ui) = ask(&s.w, &"lang ".repeat(400_000));
    assert!(r == Ok(Stop::End) && ui.text == "ungelesen", "{r:?}");
}

#[test]
fn silence_until_the_deadline_and_a_helper_that_will_not_go() {
    let s = Stage::new("program-deadline");
    s.w.set("ai.timeout_s", 1i64);
    // Silent, with a child of its own: both are ended at the deadline.
    s.helper("cat > /dev/null\necho $$ > pid\nsleep 30 &\necho $! > child\nwait");
    let started = Instant::now();
    let (r, ui) = ask(&s.w, "x");
    assert!(r.unwrap_err().contains("nicht rechtzeitig geantwortet"), "{:?}", ui.logs);
    assert!(started.elapsed() < Duration::from_secs(6), "{:?}", started.elapsed());
    assert!(s.gone("pid") && s.gone("child"), "the helper and what it started are gone");

    // It ignores SIGTERM (and so does what it starts): SIGKILL after a moment.
    s.helper("trap '' TERM\ncat > /dev/null\necho $$ > pid\necho '{\"text\":\"ich bleibe\"}'\nsleep 30 &\necho $! > child\nwhile :; do sleep 0.1; done");
    let started = Instant::now();
    let (r, ui) = ask(&s.w, "x");
    assert!(r.unwrap_err().contains("nicht rechtzeitig") && ui.text == "ich bleibe");
    assert!(started.elapsed() < Duration::from_secs(8), "{:?}", started.elapsed());
    assert!(s.gone("pid") && s.gone("child"));

    // It says done and stays: the answer counts, and it is ended all the same.
    s.helper(&format!("trap '' TERM\ncat > /dev/null\necho $$ > pid\necho '{{\"text\":\"fertig\"}}'\n{DONE}\nwhile :; do sleep 0.1; done"));
    let started = Instant::now();
    let (r, ui) = ask(&s.w, "x");
    assert!(r == Ok(Stop::End) && ui.text == "fertig" && started.elapsed() < Duration::from_secs(8), "{r:?} {:?}", started.elapsed());
    assert!(s.gone("pid"));
}

#[test]
fn esc_ends_the_helper() {
    let s = Stage::new("program-esc");
    // A program in a session of its own (as a shell command of the helper's program may be) is no longer in
    // the helper's group: it is ended all the same.
    s.helper("cat > /dev/null\necho $$ > pid\nsleep 30 &\necho $! > child\nsetsid sleep 300 &\necho $! > stray\nsleep 0.2\necho '{\"text\":\"Anfang\"}'\nwait");
    let tools = SettingsTools { settings: &s.w };
    let mut session = Session::new(&s.w, &tools, BUDGET);
    let mut ui = Rec::new(Answer::CannotAsk);
    ui.cancel_at = Some(3);
    let started = Instant::now();
    assert_eq!(session.ask("x", &mut ui), Err("Abgebrochen.".into()));
    assert!(ui.text == "Anfang" && started.elapsed() < Duration::from_secs(5), "{:?}", started.elapsed());
    assert!(s.gone("pid") && s.gone("child") && s.gone("stray") && session.conv.messages.is_empty());

    // The next question works.
    s.helper(&format!("cat > /dev/null\necho '{{\"text\":\"wieder da\"}}'\n{DONE}"));
    ui.cancel_at = None;
    assert_eq!(session.ask("y", &mut ui), Ok(Stop::End));
}

#[test]
fn nur_lokal_and_the_fallback() {
    let s = Stage::new("program-local-only");
    let cloud = s.script("cloud", &format!("touch started.cloud\ncat > /dev/null\necho '{{\"text\":\"aus der Cloud\"}}'\n{DONE}"));
    let home = s.script("home", &format!("cat > /dev/null\necho '{{\"text\":\"von daheim\"}}'\n{DONE}"));
    s.provider("wolke", &cloud, "cloud");
    s.provider("daheim", &home, "local");
    s.w.set("ai.role.chat", "wolke");

    // A helper labelled cloud runs (no http rule applies to a path), until "Nur lokal" is on.
    let (r, ui) = ask(&s.w, "x");
    assert!(r == Ok(Stop::End) && ui.text == "aus der Cloud", "{r:?}");
    std::fs::remove_file(s.dir.join("started.cloud")).unwrap();
    s.w.set("ai.local_only", true);
    let (r, _) = ask(&s.w, "x");
    let e = r.unwrap_err();
    assert!(e.contains("Nur lokal") && e.contains("wolke"), "{e}");
    assert!(!s.dir.join("started.cloud").exists(), "refused before the helper is started");

    // The local one answers as the fallback, and by itself.
    s.w.set("ai.role.chat_fallback", "daheim");
    let (r, ui) = ask(&s.w, "x");
    assert!(r == Ok(Stop::End) && ui.text == "von daheim" && ui.notes.iter().any(|n| n.contains("Ich nehme „daheim“")), "{:?}", ui.notes);
    assert!(!s.dir.join("started.cloud").exists());
    s.w.set("ai.local_only", false);

    // A helper that fails before it said anything hands over to the fallback too; after a piece it does not.
    s.script("cloud", "cat > /dev/null\necho '{\"done\":true,\"stop\":\"error\",\"error\":\"Heute nicht.\"}'");
    let (r, ui) = ask(&s.w, "x");
    assert!(r == Ok(Stop::End) && ui.text == "von daheim" && ui.notes.iter().any(|n| n == "Heute nicht. Ich nehme „daheim“."), "{:?}", ui.notes);
    s.script("cloud", "cat > /dev/null\necho '{\"text\":\"halb\"}'\necho '{\"done\":true,\"stop\":\"error\",\"error\":\"Heute nicht.\"}'");
    let (r, ui) = ask(&s.w, "x");
    assert!(r == Err("Heute nicht.".into()) && ui.text == "halb");
}

#[test]
fn an_address_is_the_full_path_of_a_program() {
    let s = Stage::new("program-path");
    let good = s.helper(&format!("cat > /dev/null\necho '{{\"text\":\"ok\"}}'\n{DONE}"));
    assert_eq!(ask(&s.w, "x").0, Ok(Stop::End));
    let plain = s.dir.join("plain.txt");
    std::fs::write(&plain, "#!/bin/sh\necho no\n").unwrap();
    let link = s.dir.join("link");
    std::os::unix::fs::symlink(&good, &link).unwrap();

    let with = |address: &str| {
        s.w.set("ai.provider.h.address", address);
        ask(&s.w, "x").0
    };
    for (address, why) in [
        ("helper", "voller Pfad"),
        ("./helper", "voller Pfad"),
        ("sh -c 'echo hi'", "voller Pfad"),
        ("https://example.invalid/v1", "voller Pfad"),
        ("/no/such/helper", "gibt es nicht"),
        (s.dir.to_str().unwrap(), "keine Datei"),
        (plain.to_str().unwrap(), "nicht als Programm ausführbar"),
        // Arguments are not part of an address: this names a file that does not exist.
        (&format!("{} --flag", good.display()), "gibt es nicht"),
    ] {
        let e = with(address).unwrap_err();
        assert!(e.contains("Das Hilfsprogramm des Anbieters „h“ ist nicht brauchbar") && e.contains(why) && e.contains("ai.provider.h.address"), "{address}: {e}");
    }
    assert!(with("").unwrap_err().contains("fehlt die Adresse"));
    // A link to a program is a program.
    assert_eq!(with(link.to_str().unwrap()), Ok(Stop::End));

    // A file that cannot be run although it is marked so (no interpreter): one sentence, no hang.
    let broken = s.script("broken", "");
    let mut sh = Command::new("sh").args(["-c", "printf '#!/no/such/interpreter\\n' > \"$1\"", "sh"]).arg(&broken).spawn().unwrap();
    assert!(sh.wait().unwrap().success());
    let e = with(broken.to_str().unwrap()).unwrap_err();
    assert!(e.contains("ließ sich nicht starten"), "{e}");

    // The place is still Georg's to say.
    s.w.set("ai.provider.h.address", good.to_str().unwrap());
    s.w.set("ai.provider.h.place", "unset");
    assert!(ask(&s.w, "x").0.unwrap_err().contains("cloud|local"));
}

#[test]
fn the_real_program_with_a_helper() {
    let s = Stage::new("program-cli");
    s.w.listen();
    let helper = s.helper(&format!("cat > /dev/null\necho 'helper speaking' >&2\necho '{{\"text\":\"Hallo \"}}'\necho '{{\"text\":\"Georg\"}}'\n{DONE}"));
    let run = |args: &[&str], stdin: &str| {
        let mut child = Command::new(env!("CARGO_BIN_EXE_assist")).arg("--socket").arg(&s.w.socket).args(args).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
        child.stdin.take().unwrap().write_all(stdin.as_bytes()).unwrap();
        let o = child.wait_with_output().unwrap();
        (o.status.code().unwrap_or(-1), String::from_utf8_lossy(&o.stdout).into_owned(), String::from_utf8_lossy(&o.stderr).into_owned())
    };
    let (code, stdout, stderr) = run(&["--once", "Hallo"], "");
    assert_eq!((code, stdout.as_str()), (0, "Hallo Georg\n"), "{stderr}");
    assert!(stderr.contains("assist: log provider=h helper: helper speaking"), "{stderr}");

    let (code, stdout, _) = run(&["--providers"], "");
    assert!(code == 0 && stdout.contains(&format!("h: program · lokal · {} · Modell nicht genannt · Schlüssel nicht gesetzt", helper.display())), "{stdout}");
    // A helper gets no key, so none is taken for it.
    let (code, stdout, stderr) = run(&["--key", "h"], "sk-not-for-a-helper\n");
    assert!(code == 2 && stdout.is_empty() && stderr.contains("keinen Schlüssel"), "{stderr}");
    assert!(!s.w.everything_settingsd_kept().contains("sk-not-for-a-helper"));
    // The help and the first-run text name the kind in one line each.
    let (_, help, _) = run(&["--help"], "");
    assert_eq!(help.lines().filter(|l| l.contains("program")).count(), 1, "{help}");
    assert_eq!(assist::de::howto().lines().filter(|l| l.contains("program")).count(), 1);
    assert!(assist::de::howto().lines().all(|l| l.chars().count() <= 76));
}

/// Rec, and Georg tightening the level on his pill as soon as the helper's first words arrived.
struct Tightens<'a> {
    rec: Rec,
    w: &'a World,
}

impl assist::engine::Ui for Tightens<'_> {
    fn text(&mut self, delta: &str) {
        self.rec.text(delta);
        self.w.set("ai.level", "ask");
    }
    fn note(&mut self, line: &str) {
        self.rec.note(line);
    }
    fn log(&mut self, line: &str) {
        self.rec.log(line);
    }
    fn ask(&mut self, question: &str) -> Answer {
        self.rec.ask(question)
    }
    fn cancelled(&mut self) -> bool {
        self.rec.cancelled()
    }
}

#[test]
fn a_tightened_level_ends_a_helper_whose_own_tools_were_allowed_ahead() {
    let s = Stage::new("program-tighten");
    s.w.set("ai.level", "auto");
    s.helper("cat > /dev/null\necho $$ > pid\necho '{\"text\":\"Anfang\"}'\nsleep 30\necho '{\"text\":\" zu spät\"}'");
    let tools = SettingsTools { settings: &s.w };
    let mut session = Session::new(&s.w, &tools, BUDGET);
    let mut ui = Tightens { rec: Rec::new(Answer::CannotAsk), w: &s.w };
    let started = Instant::now();
    assert_eq!(session.ask("x", &mut ui), Err(assist::engine::TIGHTENED.into()));
    assert!(ui.rec.text == "Anfang" && started.elapsed() < Duration::from_secs(8), "{:?}", started.elapsed());
    assert!(s.gone("pid") && session.conv.messages.is_empty());
}
