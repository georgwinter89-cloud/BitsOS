//! Every built-in key: set, applied, stored, undone (V2-LANES CL-1 "set/apply/undo for every key"); kernel keys
//! through /sys/bitsos and CONFIG.TXT; R8 switches refused everywhere.

mod common;
use common::*;
use settingsd::builtin;
use settingsd::guard;
use settingsd::json::Json;
use settingsd::key::{Applier, KeyDef};
use settingsd::value::{Type, Value};

fn sample(d: &KeyDef) -> Value {
    match (&d.ty, &d.default) {
        (Type::Bool, Value::Bool(b)) => Value::Bool(!b),
        (Type::Int { min, max, .. }, Value::Int(n)) => Value::Int(if n == max { *min } else { *max }),
        // A word default (kernel.preload.max_mb = all): a number is the change.
        (Type::Int { min, word: Some(_), .. }, Value::Text(_)) => Value::Int(*min),
        (Type::Choice(c), v) => Value::Text(c.iter().find(|x| Value::Text((*x).clone()) != *v).unwrap().clone()),
        (Type::Color, _) => Value::Text("#123abc".into()),
        (Type::Text { .. }, _) => Value::Text("Test Font".into()),
        (Type::LogSpec, _) => Value::Text("warn,net=debug".into()),
        other => panic!("no sample for {other:?}"),
    }
}

#[test]
fn builtins_are_valid_and_named_for_wy5() {
    let all = builtin::all();
    for d in &all {
        d.validate().unwrap_or_else(|e| panic!("{}: {e}", d.name));
    }
    let names: Vec<&str> = all.iter().map(|d| d.name.as_str()).collect();
    for want in [
        "theme.mode",
        "theme.accent",
        "theme.font.ui",
        "theme.font.mono",
        "theme.scale",
        "theme.motion",
        "theme.effects",
        "theme.dark.bg",
        "theme.light.bg",
        "kernel.loglevel",
        "kernel.console_log",
        "kernel.snd_output",
        "kernel.preload.enabled",
        "kernel.preload.max_mb",
    ] {
        assert!(names.contains(&want), "{want} missing");
    }
    for a in &names {
        for b in &names {
            assert!(a == b || !b.starts_with(&format!("{a}.")), "{a} is a group of {b}");
        }
    }
    let scale = all.iter().find(|d| d.name == "theme.scale").unwrap();
    assert_eq!(scale.default, Value::Int(125));
    let mode = all.iter().find(|d| d.name == "theme.mode").unwrap();
    assert_eq!(mode.default, Value::Text("dark".into()), "dark first");
}

/// The fake attribute is a plain file written at offset 0 without O_TRUNC (as settingsd opens a real one):
/// its first line is the value the kernel would have taken.
fn attr(env: &Env, name: &str) -> String {
    let t = std::fs::read_to_string(env.paths.sys.join(name)).unwrap();
    assert!(t.contains('\n'), "{name}: {t:?}");
    t.lines().next().unwrap_or("").to_string()
}

#[test]
fn every_key_set_apply_undo() {
    let env = Env::new("everykey", true);
    std::fs::write(env.paths.sys.join("loglevel"), "info\n").unwrap();
    std::fs::write(env.paths.sys.join("console_log"), "warn\n").unwrap();
    std::fs::write(env.paths.sys.join("snd_output"), "auto\n").unwrap();
    std::fs::write(env.paths.sys.join("preload"), "on\n").unwrap();
    std::fs::write(env.paths.sys.join("preload_max_mb"), "1024\n").unwrap();
    let (mut r, _ev) = env.open();
    let (rx, _) = r.watch("");
    for d in builtin::all() {
        let v = sample(&d);
        let a = r.set(&d.name, &v.to_json(), &WHO).unwrap_or_else(|e| panic!("{}: {e:?}", d.name));
        assert_eq!(a.get("changed"), Some(&Json::Bool(true)), "{}", d.name);
        assert_eq!(value(&r, &d.name), v.to_json(), "{}", d.name);
        assert_eq!(a.str("stored"), Some("stick"), "{}", d.name);
        match &d.applier {
            Applier::Kernel { config, sys } => {
                let cfg = env.stick_file("CONFIG.TXT");
                assert!(cfg.contains(&format!("{config}={}\n", v.show())), "{}: {cfg}", d.name);
                match sys {
                    Some(s) => {
                        assert_eq!(a.str("applied"), Some("now"));
                        assert_eq!(attr(&env, s), v.show());
                    }
                    None => assert_eq!(a.str("applied"), Some("next_start")),
                }
            }
            Applier::Service(_) => {
                assert_eq!(a.str("applied"), Some("notified 1 watcher(s)"), "{}", d.name);
                let ev = settingsd::json::parse(&rx.try_recv().unwrap()).unwrap();
                assert_eq!(ev.str("key"), Some(d.name.as_str()));
                assert_eq!(ev.get("value"), Some(&v.to_json()));
                let file = env.stick_file(&format!("settings/{}.toml", settingsd::key::group(&d.name)));
                assert!(file.contains(&format!("{} = ", d.name)), "{}: {file}", d.name);
            }
            Applier::Settingsd => {}
        }
        let u = r.undo(1, &WHO).unwrap();
        assert_eq!(u.get("undone").map(|x| matches!(x, Json::Arr(a) if a.len() == 1)), Some(true));
        assert_eq!(value(&r, &d.name), d.default.to_json(), "{} after undo", d.name);
        if let Applier::Kernel { config, sys: Some(s) } = &d.applier {
            assert!(!env.stick_file("CONFIG.TXT").contains(&format!("{config}=")), "{}: line gone after undo", d.name);
            assert_eq!(attr(&env, s), d.default.show());
        }
        if matches!(d.applier, Applier::Service(_)) {
            let _ = rx.try_recv();
        }
    }
    let h = r.history(1000, None);
    let Some(Json::Arr(e)) = h.get("entries") else { panic!() };
    assert_eq!(e.len(), builtin::all().len() * 2);
}

#[test]
fn bad_values_refused() {
    let env = Env::new("badvals", true);
    let (mut r, _) = env.open();
    for (k, v) in [
        ("theme.scale", Json::Int(99)),
        ("theme.scale", Json::Int(301)),
        ("theme.accent", s("cyan")),
        ("theme.accent", s("#12345")),
        ("theme.mode", s("blue")),
        ("theme.font.ui", s("a\nb")),
        ("theme.font.ui", s(&"x".repeat(65))),
        ("kernel.loglevel", s("loud")),
        ("kernel.loglevel", s("net=")),
        ("kernel.loglevel", s(",")),
        ("theme.scale", Json::Bool(true)),
    ] {
        let e = r.set(k, &v, &WHO).unwrap_err();
        assert_eq!(e.code, "bad_value", "{k} {v:?}");
    }
    assert_eq!(r.set("theme.accent", &s("#ABCDEF"), &WHO).unwrap().get("new"), Some(&s("#abcdef")));
    assert_eq!(r.set("theme.scale", &s("150%"), &WHO).unwrap().get("new"), Some(&Json::Int(150)));
    assert_eq!(r.set("theme.mode", &s("LIGHT"), &WHO).unwrap().get("new"), Some(&s("light")));
    assert_eq!(r.set("nope.key", &s("1"), &WHO).unwrap_err().code, "unknown_key");
}

#[test]
fn kernel_refusal_changes_nothing() {
    let env = Env::new("sysrefuse", true);
    // A directory cannot be written: stands in for the kernel answering EINVAL.
    std::fs::create_dir_all(env.paths.sys.join("console_log")).unwrap();
    let (mut r, _) = env.open();
    let e = r.set("kernel.console_log", &s("debug"), &WHO).unwrap_err();
    assert_eq!(e.code, "apply_failed");
    assert_eq!(value(&r, "kernel.console_log"), s("warn"));
    assert!(!env.stick_file("CONFIG.TXT").contains("console_log"));
    assert!(r.undo(1, &WHO).is_err(), "nothing journaled");
}

#[test]
fn r8_switches_are_never_keys() {
    let env = Env::new("r8", true);
    let guarded =
        "# Georg's file\r\nwlan=off\r\nnvme_write=off\r\nstick_write=off\r\nusb_msd_write = off\r\nloglevel=info\r\n";
    std::fs::write(env.paths.stick.join("CONFIG.TXT"), guarded).unwrap();
    let (mut r, _) = env.open();
    for k in ["nvme_write", "stick_write", "usb_msd_write", "kernel.stick_write", "net.nvme_write", "x.Usb_Msd_Write"] {
        assert_eq!(r.set(k, &s("on"), &WHO).unwrap_err().code, "refused_r8", "{k}");
        assert_eq!(r.get(k, None).unwrap_err().code, "refused_r8", "{k}");
    }
    let reg = settingsd::json::parse(
        r#"{"key":"disk.nvme_write","schema":{"type":"bool"},"default":false,"text":{"de":"x","en":"x"}}"#,
    )
    .unwrap();
    assert!(KeyDef::from_json(&reg, "netd").unwrap_err().contains("R8"));
    for k in guard::R8_SWITCHES.iter().chain(guard::KERNEL_SECRETS) {
        assert!(guard::edit_config("", k, Some("on")).is_err(), "{k}");
    }
    // R8d since 21.10.: wlan is no switch any more.
    assert!(guard::refuse_key("net.wlan").is_ok());
    assert_eq!(guard::edit_config("wlan=off\n", "wlan", Some("on")).unwrap(), "wlan=on\n");
    r.set("kernel.loglevel", &s("debug"), &WHO).unwrap();
    r.set("kernel.console_font", &s("vga"), &WHO).unwrap();
    let after = env.stick_file("CONFIG.TXT");
    assert_eq!(
        after,
        "# Georg's file\r\nwlan=off\r\nnvme_write=off\r\nstick_write=off\r\nusb_msd_write = off\r\nloglevel=debug\r\nconsole_font=vga\r\n"
    );
    assert!(guard::config_guard(guarded.as_bytes(), after.as_bytes()).is_ok());
}

#[test]
fn config_edit_rules() {
    let t = "loglevel=info\n# loglevel=trace\nloglevel = warn # second\nother=1";
    assert_eq!(guard::config_value(t, "loglevel").as_deref(), Some("warn"));
    let e = guard::edit_config(t, "loglevel", Some("debug")).unwrap();
    assert_eq!(e, "loglevel=info\n# loglevel=trace\nloglevel=debug\nother=1");
    let e = guard::edit_config(t, "loglevel", None).unwrap();
    assert_eq!(e, "# loglevel=trace\nother=1");
    assert_eq!(guard::edit_config("a=1", "console_log", Some("info")).unwrap(), "a=1\nconsole_log=info\n");
    assert!(guard::edit_config("", "loglevel", Some("info # x")).is_err());
    // The guard port itself: adding or flipping a guarded line is refused, dropping one is allowed.
    assert_eq!(guard::config_guard(b"stick_write=off\n", b"stick_write=off\nstick_write=on\n").unwrap_err(), "stick_write");
    assert!(guard::config_guard(b"stick_write=off\n", b"STICK_WRITE=on\n").is_err());
    assert!(guard::config_guard(b"stick_write=off\n", b"").is_ok());
    assert!(guard::config_guard(b"wlan=off\n", b"wlan=on\n").is_ok());
    assert!(guard::config_guard(b"nvme_write=off\n", b"\xef\xbb\xbfnvme_write=on\n").is_err());
}

#[test]
fn non_utf8_config_left_alone() {
    let env = Env::new("nonutf8", true);
    std::fs::write(env.paths.stick.join("CONFIG.TXT"), b"loglevel=info\n\xff\xfe\n").unwrap();
    let (mut r, _) = env.open();
    let e = r.set("kernel.console_font", &s("vga"), &WHO).unwrap_err();
    assert_eq!(e.code, "config_refused");
    assert_eq!(r.status().str("place"), Some("stick"), "a bad file is no reason to leave the stick");
    assert_eq!(std::fs::read(env.paths.stick.join("CONFIG.TXT")).unwrap(), b"loglevel=info\n\xff\xfe\n");
}

#[test]
fn kernel_secrets_are_never_keys() {
    let env = Env::new("secretnames", true);
    std::fs::write(env.paths.stick.join("CONFIG.TXT"), "token=abc\nwlan_psk=pw\n").unwrap();
    let (mut r, _) = env.open();
    for k in ["kernel.token", "kernel.wlan_psk", "kernel.claude_api_key", "token", "x.wlan_psk"] {
        assert_eq!(r.set(k, &Json::from("x"), &WHO).unwrap_err().code, "refused_r8", "{k}");
        assert_eq!(r.get(k, Some("settingsd")).unwrap_err().code, "refused_r8", "{k}");
        if !k.contains('.') {
            continue; // not a key name at all
        }
        let d = format!(
            r#"{{"key":"{k}","schema":{{"type":"text","max_bytes":10}},"default":"","text":{{"de":"d","en":"e"}},"safety":"secret"}}"#
        );
        let e = KeyDef::from_json(&settingsd::json::parse(&d).unwrap(), "x").unwrap_err();
        assert!(e.contains("R8"), "{k}: {e}");
    }
    assert_eq!(env.stick_file("CONFIG.TXT"), "token=abc\nwlan_psk=pw\n");
    assert!(env.stick_file("settings/journal.log").is_empty());
    assert!(!r.list("").to_text().contains("abc"));
}

#[test]
fn hung_kernel_file_is_bounded_and_changes_nothing() {
    let env = Env::new("hungsys", true);
    let fifo = env.paths.sys.join("loglevel");
    mkfifo(&fifo);
    let (mut r, ev) = env.open();
    let t = std::time::Instant::now();
    assert_eq!(r.set("kernel.loglevel", &Json::from("debug"), &WHO).unwrap_err().code, "apply_failed");
    assert!(t.elapsed() < std::time::Duration::from_secs(3), "waited {:?}", t.elapsed());
    // The next try does not wait again while the first write still hangs.
    let t = std::time::Instant::now();
    assert_eq!(r.set("kernel.loglevel", &Json::from("debug"), &WHO).unwrap_err().code, "apply_failed");
    assert!(t.elapsed() < std::time::Duration::from_millis(200));
    assert!(!env.stick_file("CONFIG.TXT").contains("loglevel"), "a refused kernel key changes nothing");
    assert_eq!(r.status().str("place"), Some("stick"), "a hung kernel file is no reason to leave the stick");
    // Theme keys are not held up by the kernel file.
    r.set("theme.radius", &Json::Int(2), &WHO).unwrap();
    assert!(ev.lock().unwrap().iter().any(|l| l.contains("apply.failed")));
    let _ = std::fs::File::open(&fifo);
}

/// FS-13b: kernel.preload.max_mb takes `all` (any case) or a number; `all` is the default and what the kernel
/// file gets; describe names the word.
#[test]
fn preload_max_mb_takes_all_or_a_number() {
    let d = builtin::all().into_iter().find(|d| d.name == "kernel.preload.max_mb").unwrap();
    assert_eq!(d.default, Value::Text("all".into()));
    assert_eq!(d.ty.parse_text("ALL"), Ok(Value::Text("all".into())));
    assert_eq!(d.ty.parse_text("512 MB"), Ok(Value::Int(512)));
    assert_eq!(d.ty.parse_text("0"), Ok(Value::Int(0)));
    assert!(d.ty.parse_text("alles").unwrap_err().contains("or all"));
    assert!(d.ty.parse_text("1048577").is_err());
    assert_eq!(d.ty.describe().str("word"), Some("all"));
    assert_eq!(Type::from_json(&d.ty.describe()), Ok(d.ty.clone()));
}

/// FS-13b migration: a stored 1024 (the old default) becomes `all` once, with a journal entry and the marker;
/// a 1024 set afterwards stays, also across a restart; another stored number is never touched.
#[test]
fn a_stored_1024_preload_cap_migrates_to_all_once() {
    let env = Env::new("migrate", true);
    std::fs::write(env.paths.sys.join("preload_max_mb"), "1024\n").unwrap();
    std::fs::write(env.paths.stick.join("CONFIG.TXT"), "token=x\npreload_max_mb=1024\n").unwrap();
    let (mut r, ev) = env.open();
    assert_eq!(env.stick_file("CONFIG.TXT"), "token=x\n", "the line goes (= default all)");
    // A sysfs write goes to offset 0 without truncating (here a plain file keeps its tail).
    assert!(std::fs::read_to_string(env.paths.sys.join("preload_max_mb")).unwrap().starts_with("all\n"), "applied now");
    assert!(ev.lock().unwrap().iter().any(|l| l.contains("migrated key=kernel.preload.max_mb from=1024 to=all")));
    assert!(env.stick_file("settings/MIGRATED").lines().any(|l| l == "preload_all"));
    assert!(env.stick_file("settings/journal.log").contains("\"migrate\""));
    r.set("kernel.preload.max_mb", &Json::Int(1024), &WHO).unwrap();
    drop(r);
    let (r, _) = env.open();
    assert_eq!(r.effective("kernel.preload.max_mb"), Some(Value::Int(1024)), "an explicit 1024 stays");
    drop(r);
    let env = Env::new("migrate512", true);
    std::fs::write(env.paths.sys.join("preload_max_mb"), "512\n").unwrap();
    std::fs::write(env.paths.stick.join("CONFIG.TXT"), "preload_max_mb=512\n").unwrap();
    let (r, _) = env.open();
    assert_eq!(r.effective("kernel.preload.max_mb"), Some(Value::Int(512)));
    assert_eq!(env.stick_file("CONFIG.TXT"), "preload_max_mb=512\n");
    assert!(env.stick_file("settings/MIGRATED").contains("preload_all"));
}

/// The marker of a migration done while the stick still refused writes (at boot, before `fat stick.decided`)
/// is written later from `tick`, once the stick takes it.
#[test]
fn the_migration_marker_waits_for_a_writable_stick() {
    use std::os::unix::fs::PermissionsExt;
    let env = Env::new("marker", true);
    let dir = env.paths.stick.join("settings");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(env.paths.sys.join("preload_max_mb"), "1024\n").unwrap();
    std::fs::write(env.paths.stick.join("CONFIG.TXT"), "preload_max_mb=1024\n").unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o555)).unwrap();
    let (mut r, ev) = env.open();
    let refused = !dir.join("MIGRATED").exists();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    if !refused {
        return; // root ignores the mode bits: nothing to test here
    }
    assert!(ev.lock().unwrap().iter().any(|l| l.contains("migrate.marker_later")));
    std::thread::sleep(std::time::Duration::from_millis(2100));
    r.tick();
    assert!(env.stick_file("settings/MIGRATED").contains("preload_all"), "{:?}", ev.lock().unwrap());
}
