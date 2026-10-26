//! Files, journal, undo, registration, secrets, fallback and resync.

mod common;
use common::*;
use settingsd::change::Who;
use settingsd::json::{Json, parse};
use settingsd::key::KeyDef;

fn reg_def(text: &str, owner: &str) -> KeyDef {
    KeyDef::from_json(&parse(text).unwrap(), owner).unwrap()
}

fn entries(r: &settingsd::registry::Registry, n: usize) -> Vec<Json> {
    match r.history(n, None).get("entries") {
        Some(Json::Arr(a)) => a.clone(),
        _ => panic!(),
    }
}

#[test]
fn values_survive_a_restart_and_hand_edits_are_read() {
    let env = Env::new("persist", true);
    {
        let (mut r, _) = env.open();
        r.set("theme.accent", &s("#ff8800"), &WHO).unwrap();
        r.set("theme.scale", &Json::Int(150), &WHO).unwrap();
        r.set("kernel.console_log", &s("info"), &WHO).unwrap();
    }
    let theme = env.stick_file("settings/theme.toml");
    assert!(theme.contains("theme.accent = \"#ff8800\"\n") && theme.contains("theme.scale = 150\n"), "{theme}");
    let (r, _) = env.open();
    assert_eq!(value(&r, "theme.accent"), s("#ff8800"));
    assert_eq!(value(&r, "kernel.console_log"), s("info"));
    assert_eq!(entries(&r, 10).len(), 3);
    drop(r);
    // Georg edits on the Gaming-PC: a table header, a quoted key, a bad line, an unknown key.
    std::fs::write(
        env.paths.stick.join("settings/theme.toml"),
        "# mine\n[theme]\naccent = \"#00FF00\" # green\n\"radius\" = 4\nscale = lots\n[theme.dark]\nbg = '#000000'\n",
    )
    .unwrap();
    std::fs::write(env.paths.stick.join("settings/context.toml"), "context.depth = 3\n").unwrap();
    let (mut r, ev) = env.open();
    assert_eq!(value(&r, "theme.accent"), s("#00ff00"));
    assert_eq!(value(&r, "theme.radius"), Json::Int(4));
    assert_eq!(value(&r, "theme.dark.bg"), s("#000000"));
    assert_eq!(value(&r, "theme.scale"), Json::Int(125), "unreadable line = default");
    assert!(ev.lock().unwrap().iter().any(|l| l.contains("file.bad_line") && l.contains("line=5")));
    // The unknown key is kept for the service that registers it later.
    let ctx = reg_def(
        r#"{"key":"context.depth","schema":{"type":"int","min":0,"max":9},"default":1,"text":{"de":"Tiefe","en":"Depth"}}"#,
        "ctxd",
    );
    r.register("ctxd", vec![ctx]).unwrap();
    assert_eq!(value(&r, "context.depth"), Json::Int(3));
}

#[test]
fn undo_walks_back_and_history_marks_it() {
    let env = Env::new("undo", true);
    let (mut r, _) = env.open();
    r.set("theme.radius", &Json::Int(1), &WHO).unwrap();
    r.set("theme.radius", &Json::Int(2), &WHO).unwrap();
    r.set("theme.spacing", &Json::Int(3), &WHO).unwrap();
    let same = r.set("theme.spacing", &Json::Int(3), &WHO).unwrap();
    assert_eq!(same.get("changed"), Some(&Json::Bool(false)), "no journal entry for no change");
    let u = r.undo(2, &WHO).unwrap();
    assert!(matches!(u.get("undone"), Some(Json::Arr(a)) if a.len() == 2));
    assert_eq!(value(&r, "theme.spacing"), Json::Int(8));
    assert_eq!(value(&r, "theme.radius"), Json::Int(1));
    let h = entries(&r, 10);
    assert_eq!(h.len(), 5);
    assert_eq!(h[0].str("op"), Some("undo"));
    assert_eq!(h[2].get("undone"), Some(&Json::Bool(true)));
    r.undo(1, &WHO).unwrap();
    assert_eq!(value(&r, "theme.radius"), Json::Int(8));
    assert_eq!(r.undo(1, &WHO).unwrap_err().code, "nothing_to_undo");
    // The journal on the stick says the same after a restart.
    drop(r);
    let (r, _) = env.open();
    assert_eq!(entries(&r, 10).len(), 6);
    assert_eq!(entries(&r, 10)[0].get("seq"), Some(&Json::Int(6)));
}

#[test]
fn journal_trims_to_history_keep() {
    let env = Env::new("trim", true);
    let (mut r, _) = env.open();
    r.set("settings.history_keep", &Json::Int(10), &WHO).unwrap();
    for i in 0..30 {
        r.set("theme.radius", &Json::Int(i % 20), &WHO).unwrap();
    }
    let lines = env.stick_file("settings/journal.log").lines().count();
    assert!(lines <= 11, "journal has {lines} lines");
    assert_eq!(entries(&r, 100).len(), 10);
}

#[test]
fn secrets_and_confirm() {
    let env = Env::new("secret", true);
    let (mut r, _) = env.open();
    let k = reg_def(
        r#"{"key":"claude.api_key","schema":{"type":"text","max_bytes":200},"default":"","text":{"de":"API-Schlüssel","en":"API key"},"safety":"secret"}"#,
        "claude",
    );
    let c = reg_def(
        r#"{"key":"claude.allow_run","schema":{"type":"bool"},"default":false,"text":{"de":"Darf Programme starten","en":"May run programs"},"safety":"confirm"}"#,
        "claude",
    );
    r.register("claude", vec![k, c]).unwrap();
    let a = r.set("claude.api_key", &s("sk-very-secret"), &WHO).unwrap();
    assert_eq!(a.get("new"), Some(&s("***")));
    assert_eq!(value(&r, "claude.api_key"), s("***"));
    assert_eq!(r.get("claude.api_key", Some("claude")).unwrap().get("value"), Some(&s("sk-very-secret")));
    assert_eq!(entries(&r, 1)[0].get("new"), Some(&s("***")));
    assert!(!env.stick_file("settings/journal.log").contains("very-secret"));
    assert_eq!(r.undo(1, &WHO).unwrap_err().code, "not_undoable");
    assert_eq!(r.set("claude.allow_run", &Json::Bool(true), &WHO).unwrap_err().code, "needs_confirm");
    let yes = Who { source: "test", confirmed: true };
    r.set("claude.allow_run", &Json::Bool(true), &yes).unwrap();
    // An undo is a change too: Georg's confirmed change is not reverted without a confirmation.
    assert_eq!(r.undo(1, &WHO).unwrap_err().code, "needs_confirm");
    assert_eq!(value(&r, "claude.allow_run"), Json::Bool(true));
    r.undo(1, &yes).unwrap();
    assert_eq!(value(&r, "claude.allow_run"), Json::Bool(false));
    // Secret-class values are kept in clear in the group file (like wlan_psk in CONFIG.TXT): the stick is
    // Georg's; only the journal, history and answers hide them.
    assert!(env.stick_file("settings/claude.toml").contains("sk-very-secret"));
}

#[test]
fn namespaces_have_owners() {
    let env = Env::new("owners", true);
    let (mut r, _) = env.open();
    let d = |k: &str, o: &str| {
        KeyDef::from_json(
            &parse(&format!(
                r#"{{"key":"{k}","schema":{{"type":"bool"}},"default":false,"text":{{"de":"x","en":"x"}}}}"#
            ))
            .unwrap(),
            o,
        )
    };
    r.register("ctxd", vec![d("context.a", "ctxd").unwrap()]).unwrap();
    assert_eq!(r.register("other", vec![d("context.b", "other").unwrap()]).unwrap_err().code, "not_owner");
    assert_eq!(r.register("other", vec![d("kernel.x", "other").unwrap()]).unwrap_err().code, "not_owner");
    assert_eq!(r.register("other", vec![d("settings.x", "other").unwrap()]).unwrap_err().code, "not_owner");
    assert_eq!(r.register("bitscomp", vec![d("theme.accent", "bitscomp").unwrap()]).unwrap_err().code, "not_owner");
    r.register("bitscomp", vec![d("theme.blur", "bitscomp").unwrap()]).unwrap();
    assert_eq!(r.register("bitscomp", vec![d("theme.accent.x", "bitscomp").unwrap()]).unwrap_err().code, "bad_request");
    assert!(d("theme", "bitscomp").is_err(), "a key needs a namespace and a name");
    // PK-2: a removed package leaves nothing behind.
    r.set("context.a", &Json::Bool(true), &WHO).unwrap();
    assert!(env.stick_file("settings/context.toml").contains("context.a"));
    r.unregister("ctxd", "context", true).unwrap();
    assert!(!env.paths.stick.join("settings/context.toml").exists());
    assert!(!env.stick_file("settings/journal.log").contains("context."));
    assert_eq!(r.get("context.a", None).unwrap_err().code, "unknown_key");
    r.register("other", vec![d("context.b", "other").unwrap()]).unwrap();
}

#[test]
fn no_stick_means_fallback_then_resync() {
    let env = Env::new("fallback", false);
    let (mut r, ev) = env.open();
    assert_eq!(r.status().str("place"), Some("fallback"));
    let a = r.set("theme.accent", &s("#111111"), &WHO).unwrap();
    assert_eq!(a.str("stored"), Some("fallback"));
    r.set("kernel.loglevel", &s("debug"), &WHO).unwrap();
    assert!(env.fb_file("theme.toml").contains("#111111"));
    assert_eq!(env.fb_file("CONFIG.TXT"), "loglevel=debug\n");
    assert_eq!(env.fb_file("PENDING"), "kernel.loglevel\ntheme.accent\n");
    r.tick_now();
    assert_eq!(r.status().str("place"), Some("fallback"), "still no stick");
    // The stick comes back, with an edit Georg made on the Gaming-PC meanwhile.
    std::fs::create_dir_all(env.paths.stick.join("settings")).unwrap();
    std::fs::write(env.paths.stick.join("settings/theme.toml"), "theme.radius = 3\ntheme.accent = \"#222222\"\n")
        .unwrap();
    std::fs::write(env.paths.stick.join("CONFIG.TXT"), "wlan=off\ntoken=abc\n").unwrap();
    let (rx, _) = r.watch("theme");
    r.tick_now();
    assert_eq!(r.status().str("place"), Some("stick"), "{:?}", ev.lock().unwrap());
    assert_eq!(value(&r, "theme.accent"), s("#111111"), "the change made on the fallback wins for its key");
    assert_eq!(value(&r, "theme.radius"), Json::Int(3), "Georg's edit of another key survives");
    assert_eq!(env.stick_file("CONFIG.TXT"), "wlan=off\ntoken=abc\nloglevel=debug\n");
    assert!(env.stick_file("settings/journal.log").contains("theme.accent"));
    assert!(!env.paths.fallback.join("PENDING").exists() && !env.paths.fallback.join("theme.toml").exists());
    let ev_line = parse(&rx.try_recv().unwrap()).unwrap();
    assert_eq!(ev_line.str("key"), Some("theme.radius"));
}

#[test]
fn failing_stick_moves_to_fallback_and_restart_finds_it() {
    let env = Env::new("stickfail", true);
    {
        let (mut r, _) = env.open();
        r.set("theme.radius", &Json::Int(2), &WHO).unwrap();
        // The stick turns read-only: the settings directory cannot take a new file.
        let dir = env.paths.stick.join("settings");
        let mut p = std::fs::metadata(&dir).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut p, 0o555);
        std::fs::set_permissions(&dir, p).unwrap();
        let a = r.set("theme.radius", &Json::Int(4), &WHO).unwrap();
        let mut p = std::fs::metadata(&dir).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut p, 0o755);
        std::fs::set_permissions(&dir, p).unwrap();
        if a.str("stored") == Some("stick") {
            eprintln!("SKIP part: running as root, permissions do not stop writes");
            return;
        }
        assert_eq!(a.str("stored"), Some("fallback"));
        assert_eq!(r.status().str("place"), Some("fallback"));
    }
    // settingsd restarted in the same boot with the stick usable again: the fallback's change is carried over.
    let (r, _) = env.open();
    assert_eq!(r.status().str("place"), Some("stick"));
    assert_eq!(value(&r, "theme.radius"), Json::Int(4));
    assert!(env.stick_file("settings/theme.toml").contains("theme.radius = 4"));
    assert!(!env.paths.fallback.join("PENDING").exists());
}

#[test]
fn hung_stick_write_times_out() {
    let env = Env::new("hung", true);
    let (mut r, ev) = env.open();
    std::fs::create_dir_all(env.paths.stick.join("settings")).unwrap();
    // Opening a FIFO for writing blocks until a reader comes: a stick that never answers.
    let fifo = env.paths.stick.join("settings/theme.toml.new");
    mkfifo(&fifo);
    let t = std::time::Instant::now();
    let a = r.set("theme.radius", &Json::Int(5), &WHO).unwrap();
    assert!(t.elapsed() < std::time::Duration::from_secs(3), "waited {:?}", t.elapsed());
    assert_eq!(a.str("stored"), Some("fallback"));
    assert!(ev.lock().unwrap().iter().any(|l| l.contains("fallback") && l.contains("slower")));
    // Further changes do not wait for the hung writer.
    let t = std::time::Instant::now();
    r.set("theme.radius", &Json::Int(6), &WHO).unwrap();
    assert!(t.elapsed() < std::time::Duration::from_millis(400));
    // While the abandoned write still hangs, the stick is not touched again (no second stuck thread, and its
    // late write can never land after a resync).
    let t = std::time::Instant::now();
    assert!(!r.tick_now());
    assert!(t.elapsed() < std::time::Duration::from_millis(100));
    assert!(ev.lock().unwrap().iter().any(|l| l.contains("resync.waiting")));
    assert_eq!(r.status().str("place"), Some("fallback"));
    // The stick answers again: the hung write ends (it fails on the FIFO), then the resync carries the
    // fallback's change over and removes the `.new` leftover.
    let _ = std::fs::File::open(&fifo);
    assert!(until(5, || {
        r.tick_now();
        r.status().str("place") == Some("stick")
    }));
    assert!(env.stick_file("settings/theme.toml").contains("theme.radius = 6"));
    assert!(!fifo.exists(), "leftover removed");
    assert!(ev.lock().unwrap().iter().any(|l| l.contains("file.leftover_removed")));
}

#[test]
fn hung_stick_read_at_start_and_resync_is_bounded() {
    let env = Env::new("hungread", true);
    std::fs::create_dir_all(env.paths.stick.join("settings")).unwrap();
    // Opening a FIFO for reading blocks until a writer comes: the stick hangs while settingsd reads it.
    let fifo = env.paths.stick.join("settings/theme.toml");
    mkfifo(&fifo);
    let t = std::time::Instant::now();
    let (mut r, ev) = env.open();
    assert!(t.elapsed() < std::time::Duration::from_secs(3), "start waited {:?}", t.elapsed());
    assert_eq!(r.status().str("place"), Some("fallback"));
    assert!(ev.lock().unwrap().iter().any(|l| l.contains("fallback") && l.contains("slower")));
    r.set("theme.radius", &Json::Int(7), &WHO).unwrap();
    assert!(!r.tick_now(), "the stuck read is still in the kernel");
    // The read ends (empty file); the FIFO goes, the stick is a normal one again.
    drop(std::fs::OpenOptions::new().write(true).open(&fifo).unwrap());
    std::fs::remove_file(&fifo).unwrap();
    assert!(until(5, || {
        r.tick_now();
        r.status().str("place") == Some("stick")
    }));
    assert!(env.stick_file("settings/theme.toml").contains("theme.radius = 7"));
}

/// MM-9 CR6: a start on the fallback (the stick not decided yet at boot) tries the stick again after 1 s, not
/// only after 10 s; then 2, 4, 8 s apart up to the steady 10 s.
#[test]
fn a_start_on_the_fallback_tries_the_stick_soon() {
    let env = Env::new("soon", false);
    let (mut r, _ev) = env.open();
    assert_eq!(r.status().str("place"), Some("fallback"));
    assert!(!r.tick(), "not at once");
    std::thread::sleep(std::time::Duration::from_millis(1100));
    assert!(r.tick(), "tried after 1 s");
    assert_eq!(r.status().str("place"), Some("fallback"), "no stick yet");
    std::fs::create_dir_all(&env.paths.stick).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(1100));
    assert!(!r.tick(), "the next try waits 2 s");
    std::thread::sleep(std::time::Duration::from_millis(1000));
    assert!(r.tick());
    assert_eq!(r.status().str("place"), Some("stick"));
}

/// WY-10/1: watchers hear when a service's keys come and go (the settings page reads its rows again).
#[test]
fn watchers_hear_registered_and_unregistered() {
    let env = Env::new("regev", true);
    let (mut r, _) = env.open();
    let (all, _) = r.watch("");
    let (other, _) = r.watch("theme");
    let def = reg_def(r#"{"key":"test.risky","schema":{"type":"bool"},"default":false,"text":{"de":"x","en":"x"},"safety":"confirm"}"#, "tester");
    r.register("tester", vec![def]).unwrap();
    let l = parse(&all.try_recv().unwrap()).unwrap();
    assert_eq!((l.str("event"), l.str("owner")), (Some("registered"), Some("tester")));
    assert_eq!(l.get("keys").map(Json::to_text).as_deref(), Some(r#"["test.risky"]"#));
    assert!(other.try_recv().is_err(), "a watcher of theme hears nothing of test.*");
    r.unregister("tester", "test", false).unwrap();
    let l = parse(&all.try_recv().unwrap()).unwrap();
    assert_eq!(l.str("event"), Some("unregistered"));
}

/// WY-10/2: an undo that names its change refuses when another change came meanwhile, and undoes nothing.
#[test]
fn undo_with_expect_seq_refuses_a_newer_change() {
    let env = Env::new("expect", true);
    let (mut r, _) = env.open();
    let a = r.set("theme.accent", &s("#111111"), &WHO).unwrap();
    let seq = a.get("seq").and_then(Json::as_i64).unwrap() as u64;
    r.set("theme.radius", &Json::Int(3), &WHO).unwrap();
    let e = r.undo_expect(1, Some(seq), &WHO).unwrap_err();
    assert_eq!(e.code, "changed_meanwhile");
    assert_eq!(value(&r, "theme.radius"), Json::Int(3), "nothing undone");
    let newest = entries(&r, 1)[0].get("seq").and_then(Json::as_i64).unwrap() as u64;
    let u = r.undo_expect(1, Some(newest), &WHO).unwrap();
    assert_eq!(u.str("stopped"), None);
    assert_ne!(value(&r, "theme.radius"), Json::Int(3));
}
