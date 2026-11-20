//! The real `assist` program against a real settingsd socket and a fake provider: setting a provider up from
//! nothing, `--providers`, `--once`, and that a key never appears in anything the program prints.

mod common;
use common::{Fake, KEY, Reply, World, recording};
use std::io::Write;
use std::process::{Command, Output, Stdio};

fn assist(w: &World, args: &[&str], stdin: Option<&str>) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_assist"))
        .arg("--socket")
        .arg(&w.socket)
        .args(args)
        .stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    if let Some(text) = stdin {
        child.stdin.take().unwrap().write_all(text.as_bytes()).unwrap();
    }
    child.wait_with_output().unwrap()
}

fn out(o: &Output) -> (i32, String, String) {
    (o.status.code().unwrap_or(-1), String::from_utf8_lossy(&o.stdout).into_owned(), String::from_utf8_lossy(&o.stderr).into_owned())
}

#[test]
fn from_nothing_to_an_answer_and_the_key_is_never_printed() {
    let w = World::new("cli");
    w.listen();
    let fake = Fake::start(|h, n| {
        if h.header("x-api-key") != Some(KEY) {
            // Like a provider that repeats the key it refuses.
            let body = format!(r#"{{"type":"error","error":{{"type":"authentication_error","message":"invalid x-api-key {}"}}}}"#, h.header("x-api-key").unwrap_or(""));
            return Reply::Status(401, vec![], body);
        }
        Reply::Sse(recording(if n > 100 { "anthropic-done.sse" } else { "anthropic-text.sse" }))
    });
    let mut printed = String::new();
    let mut run = |args: &[&str], stdin: Option<&str>| {
        let (code, stdout, stderr) = out(&assist(&w, args, stdin));
        printed.push_str(&stdout);
        printed.push_str(&stderr);
        (code, stdout, stderr)
    };

    // Nothing is configured: every way in explains how to add a provider, and nothing is contacted.
    let (code, stdout, _) = run(&["--providers"], None);
    assert!(code == 0 && stdout.contains("kein KI-Anbieter eingerichtet") && stdout.contains("assist --add NAME"), "{stdout}");
    let (code, stdout, stderr) = run(&["--once", "Hallo"], None);
    assert!(code == 1 && stdout.is_empty() && stderr.contains("assist --add NAME"), "{stderr}");
    let (code, _, stderr) = run(&[], None);
    assert!(code == 2 && stderr.contains("braucht eine Konsole"), "the chat refuses to run without a terminal: {stderr}");

    // Add one, as the help says.
    let (code, _, stderr) = run(&["--add", "Mein Anbieter"], None);
    assert!(code == 2 && stderr.contains("Kleinbuchstaben"), "{stderr}");
    let (code, stdout, _) = run(&["--add", "claude"], None);
    assert!(code == 0 && stdout.contains("settings set ai.provider.claude.kind"), "{stdout}");
    assert_eq!(w.value("ai.providers").as_str(), Some("claude"));
    let (_, stdout, _) = run(&["--providers"], None);
    assert!(stdout.contains("claude: Art fehlt · Ort fehlt · Adresse fehlt") && stdout.contains("Schlüssel nicht gesetzt"), "{stdout}");
    w.set("ai.provider.claude.kind", "anthropic");
    w.set("ai.provider.claude.address", fake.address("anthropic").as_str());
    w.set("ai.provider.claude.model", "claude-opus-5-5");
    w.set("ai.provider.claude.place", "local");
    w.set("ai.role.chat", "claude");

    // A wrong key first: the provider echoes it, the program must not.
    let wrong = "sk-wrong-4444-ALSO-NEVER-SHOWN";
    let (code, stdout, _) = run(&["--key", "claude"], Some(&format!("{wrong}\n")));
    assert!(code == 0 && stdout.contains("gespeichert"), "{stdout}");
    let (code, stdout, stderr) = run(&["--once", "Hallo"], None);
    assert!(code == 1 && stdout.is_empty() && stderr.contains("lehnt den Schlüssel ab"), "{stderr}");
    assert!(stderr.contains("log provider=claude http=401") && stderr.contains("***"), "{stderr}");

    // The right key: the answer on stdout, nothing else there.
    let (code, _, _) = run(&["--key", "claude"], Some(&format!("{KEY}\n")));
    assert_eq!(code, 0);
    let (code, stdout, stderr) = run(&["--once", "Hallo"], None);
    assert_eq!((code, stdout.as_str()), (0, "Hallo Georg, hier antwortet die Aufzeichnung ANTHROPIC-OK. Grüße mit Umlauten: äöüß.\n"), "{stderr}");
    let (_, stdout, _) = run(&["--providers"], None);
    assert!(stdout.contains("claude: anthropic · lokal · http://127.0.0.1:") && stdout.contains("Modell claude-opus-5-5 · Schlüssel gesetzt"), "{stdout}");
    assert!(stdout.contains("Gespräch (ai.role.chat): claude") && stdout.contains("Nur lokal (ai.local_only): aus"), "{stdout}");
    let (code, _, stderr) = run(&["--key", "nobody"], Some("x\n"));
    assert!(code == 2 && stderr.contains("nicht eingerichtet"));
    let (code, _, stderr) = run(&["--bogus"], None);
    assert!(code == 2 && stderr.contains("assist --once"));

    // Neither key in anything the program printed, in settingsd's events, history, lists or files.
    for secret in [KEY, wrong] {
        assert!(!printed.contains(secret), "a key was printed");
        assert!(!w.everything_settingsd_kept().contains(secret), "settingsd kept a key in the open");
    }
    // On the stick the key sits in its settings file, and only there (settingsd's own rule for secrets).
    let journal = std::fs::read_to_string(w.root.join("stick/settings/journal.log")).unwrap_or_default();
    assert!(!journal.contains(KEY) && !journal.contains(wrong), "the journal holds no secret value");
}

#[test]
fn an_answer_cannot_drive_the_terminal() {
    let w = World::new("cli-escape");
    w.listen();
    let hostile = "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"gut\\u001b[2J\\u001b]0;x\\u0007 so\\r\\n\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
    let fake = Fake::start(move |_, _| Reply::Sse(hostile.as_bytes().to_vec()));
    w.provider("p", "openai_compatible", &fake.address("openai"), "local", None);
    w.set("ai.role.chat", "p");
    let (code, stdout, _) = out(&assist(&w, &["--once", "x"], None));
    assert_eq!((code, stdout.as_str()), (0, "gut[2J]0;x so\n"));
}

#[test]
fn a_script_can_tell_a_cut_or_empty_answer_from_a_whole_one() {
    let w = World::new("cli-cut");
    w.listen();
    let stream = |content: &str, finish: &str| format!("data: {{\"choices\":[{{\"index\":0,\"delta\":{{{content}}},\"finish_reason\":\"{finish}\"}}]}}\n\ndata: [DONE]\n\n").into_bytes();
    let fake = Fake::start(move |_, n| match n {
        1 => Reply::Sse(stream("\"content\":\"Der Anfang\"", "length")),
        2 => Reply::Sse(stream("", "stop")),
        _ => Reply::Sse(stream("", "function_call")),
    });
    w.provider("p", "openai_compatible", &fake.address("openai"), "local", None);
    w.set("ai.role.chat", "p");
    let (code, stdout, stderr) = out(&assist(&w, &["--once", "x"], None));
    assert_eq!((code, stdout.as_str()), (3, "Der Anfang\n"), "{stderr}");
    assert!(stderr.contains("an der Längengrenze abgeschnitten"), "{stderr}");
    let (code, stdout, stderr) = out(&assist(&w, &["--once", "x"], None));
    assert!(code == 3 && stdout.is_empty() && stderr.contains("leere Antwort"), "{code} {stderr}");
    let (code, stdout, stderr) = out(&assist(&w, &["--once", "x"], None));
    assert!(code == 3 && stdout.is_empty() && stderr.contains("nichts geantwortet") && stderr.contains("function_call"), "{code} {stderr}");
}

/// INT-CL3T: a key is entered for the provider as it is. `assist --key` says where the key will go before it
/// asks, takes none for a provider whose address, kind or place is still missing, and stores it only if the
/// three are still what it showed when the key has been typed (settingsd checks and stores in one step).
/// Afterwards settingsd lets nobody change them while the key is there.
#[test]
fn a_key_is_entered_for_the_address_shown() {
    use assist::settings;
    use std::io::{BufRead, BufReader};
    let w = World::new("cli-key");
    w.listen();
    let stored = || settings::get_secret(&w, "ai.provider.neu.key").unwrap();
    assert_eq!(out(&assist(&w, &["--add", "neu"], None)).0, 0);
    let (code, _, stderr) = out(&assist(&w, &["--key", "neu"], Some(&format!("{KEY}\n"))));
    assert!(code == 2 && stderr.contains("fehlt noch: Art (kind), Adresse (address), Ort (place)"), "{code} {stderr}");
    assert_eq!(stored(), "");
    w.set("ai.provider.neu.kind", "anthropic");
    w.set("ai.provider.neu.address", "https://api.example.com");
    let (code, _, stderr) = out(&assist(&w, &["--key", "neu"], Some(&format!("{KEY}\n"))));
    assert!(code == 2 && stderr.contains("fehlt noch: Ort (place)"), "{code} {stderr}");
    w.set("ai.provider.neu.place", "cloud");

    // Somebody moves the address while the key is being typed.
    let mut child = Command::new(env!("CARGO_BIN_EXE_assist"))
        .arg("--socket")
        .arg(&w.socket)
        .args(["--key", "neu"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut err = BufReader::new(child.stderr.take().unwrap());
    let mut shown = String::new();
    err.read_line(&mut shown).unwrap();
    assert!(shown.contains("wird nur hierhin geschickt: https://api.example.com (Art anthropic, Ort cloud)"), "{shown}");
    w.set("ai.provider.neu.address", "https://evil.example.net");
    child.stdin.take().unwrap().write_all(format!("{KEY}\n").as_bytes()).unwrap();
    let mut rest = String::new();
    std::io::Read::read_to_string(&mut err, &mut rest).unwrap();
    let o = child.wait_with_output().unwrap();
    assert!(o.status.code() == Some(1) && rest.contains("während der Eingabe geändert"), "{:?} {rest}", o.status);
    assert!(!rest.contains(KEY) && !String::from_utf8_lossy(&o.stdout).contains("gespeichert"));
    assert_eq!(stored(), "", "the key was stored for an address nobody was shown");

    // Unchanged while typed: stored, and from then on the provider stays where it is.
    w.set("ai.provider.neu.address", "https://api.example.com");
    let (code, stdout, stderr) = out(&assist(&w, &["--key", "neu"], Some(&format!("{KEY}\n"))));
    assert!(code == 0 && stdout.contains("gespeichert") && stderr.contains("https://api.example.com"), "{code} {stdout} {stderr}");
    assert_eq!(stored(), KEY);
    for (field, value) in [("address", "https://evil.example.net"), ("kind", "openai"), ("place", "local")] {
        let e = settings::set(&w, &format!("ai.provider.neu.{field}"), value, "test").unwrap_err();
        assert_eq!(e.code, "key_bound", "{field}: {e}");
    }
    assert!(!w.everything_settingsd_kept().contains(KEY));
}

/// `assist --test NAME` (CL-5s): one smallest request to that provider alone, one German line on stdout, and
/// an exit code that tells answered (0) from asked in vain (1) from not asked (2).
#[test]
fn a_provider_is_tested_with_one_small_request() {
    let w = World::new("cli-test");
    w.listen();
    let fake = Fake::start(|h, _| match h.header("x-api-key") {
        Some(k) if k == KEY => Reply::Sse(recording("anthropic-text.sse")),
        // Like a provider that repeats the key it refuses.
        other => Reply::Status(401, vec![], format!(r#"{{"type":"error","error":{{"type":"authentication_error","message":"invalid x-api-key {}"}}}}"#, other.unwrap_or(""))),
    });
    let test = |id: &str| {
        let (code, stdout, stderr) = out(&assist(&w, &["--test", id], None));
        assert_eq!(stdout.matches('\n').count(), 1, "one line: {stdout:?}");
        assert!(!stdout.contains(KEY) && !stderr.contains(KEY), "a key was printed");
        (code, stdout, stderr)
    };

    // Not asked: unknown, half set up, without a key.
    let (code, stdout, _) = test("claude");
    assert!(code == 2 && stdout.contains("„claude“ ist nicht eingerichtet"), "{code} {stdout}");
    assert_eq!(out(&assist(&w, &["--add", "claude"], None)).0, 0);
    let (code, stdout, _) = test("claude");
    assert!(code == 2 && stdout.contains("fehlt die Art"), "{code} {stdout}");
    w.set("ai.provider.claude.kind", "anthropic");
    w.set("ai.provider.claude.address", fake.address("anthropic").as_str());
    w.set("ai.provider.claude.model", "claude-opus-5-5");
    w.set("ai.provider.claude.place", "cloud");
    let (code, stdout, _) = test("claude");
    assert!(code == 2 && stdout.contains("http://") && stdout.contains("Cloud"), "a cloud provider is not asked without TLS: {code} {stdout}");
    w.set("ai.provider.claude.place", "local");
    assert_eq!(fake.hits().len(), 0, "nothing was sent so far");

    // Asked in vain: the provider refuses the key (and repeats it; the program does not).
    let wrong = "sk-wrong-5555-ALSO-NEVER-SHOWN";
    w.set("ai.provider.claude.key", wrong);
    let (code, stdout, stderr) = test("claude");
    assert!(code == 1 && stdout.contains("lehnt den Schlüssel ab"), "{code} {stdout}");
    assert!(stderr.contains("log provider=claude http=401") && !stderr.contains(wrong), "{stderr}");
    assert_eq!(fake.hits().len(), 1, "one request, no second try");

    // Answered. The request is the smallest there is: one short question, no tools, room for one word.
    w.set("ai.provider.claude.key", KEY);
    let (code, stdout, stderr) = test("claude");
    assert_eq!((code, stdout.as_str()), (0, "Der Anbieter „claude“ ist erreichbar, das Modell „claude-opus-5-5“ hat geantwortet.\n"), "{stderr}");
    let hits = fake.hits();
    assert_eq!(hits.len(), 2);
    let sent = hits[1].json();
    assert_eq!(sent.get("max_tokens").and_then(assist::json::Json::as_i64), Some(16), "{}", hits[1].body);
    assert!(sent.get("tools").is_none() && matches!(sent.get("messages"), Some(assist::json::Json::Arr(m)) if m.len() == 1), "{}", hits[1].body);
    // The model's words are not the test's business.
    assert!(!stdout.contains("ANTHROPIC-OK"));

    // The test asks the provider named, whoever the chat uses, and obeys "Nur lokal" like a question does.
    w.provider("fern", "anthropic", "https://api.example.invalid", "cloud", Some(KEY));
    w.set("ai.role.chat", "fern");
    assert_eq!(test("claude").0, 0);
    w.set("ai.local_only", true);
    let (code, stdout, _) = test("fern");
    assert!(code == 2 && stdout.contains("Nur lokal"), "{code} {stdout}");
    assert_eq!(fake.hits().len(), 3);

    // Away: a port nobody listens on.
    w.provider("weg", "openai_compatible", "http://127.0.0.1:9/v1", "local", None);
    let (code, stdout, _) = test("weg");
    assert!(code == 1 && stdout.contains("„weg“ ist nicht erreichbar"), "{code} {stdout}");
    assert!(!w.everything_settingsd_kept().contains(KEY));
}

/// `assist --test` has one wait for everything: a settingsd that does not answer costs what is left of it,
/// not a minute per request, and once it is over nothing is asked at all.
#[test]
fn a_settings_request_ends_with_the_callers_wait() {
    use assist::json::Json;
    use assist::settings::{Settings, Socket};
    use std::time::{Duration, Instant};
    let path = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("assist-until-{}.sock", std::process::id()));
    let _ = std::fs::remove_file(&path);
    // Connections wait in the backlog; nobody reads or answers.
    let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
    let req = Json::obj().with("op", "list").with("prefix", "ai");
    let start = Instant::now();
    let sock = Socket::new(path.to_str(), 1 << 20).until(start + Duration::from_millis(300));
    assert_eq!(sock.request(&req).unwrap_err().code, "unreachable");
    let took = start.elapsed();
    assert!(took >= Duration::from_millis(250) && took < Duration::from_secs(10), "{took:?}");
    let again = Instant::now();
    assert_eq!(sock.request(&req).unwrap_err().code, "unreachable");
    assert!(again.elapsed() < Duration::from_millis(200), "the wait is over: no new one starts");
    drop(listener);
    let _ = std::fs::remove_file(&path);
}
