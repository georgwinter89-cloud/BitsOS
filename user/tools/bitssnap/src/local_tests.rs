use std::os::unix::fs::MetadataExt;
use super::*;

/// Records what reached the pool; answers a fixed object.
#[derive(Default)]
struct Fake {
    calls: Vec<String>,
}

impl Ops for Fake {
    fn list(&mut self) -> Result<Json, String> {
        self.calls.push("list".into());
        Ok(Json::obj(vec![("snapshots", Json::Arr(Vec::new()))]))
    }
    fn browse(&mut self, id: u64, path: &str, uid: u32) -> Result<Json, String> {
        self.calls.push(format!("browse {id} {path} {uid}"));
        Ok(Json::obj(vec![("path", Json::str(path))]))
    }
    fn programs(&mut self, id: u64) -> Result<Json, String> {
        self.calls.push(format!("programs {id}"));
        Ok(Json::obj(vec![("programs", Json::Arr(Vec::new()))]))
    }
    fn restore_path(&mut self, id: u64, path: &str, uid: u32, dry: bool) -> Result<Json, String> {
        self.calls.push(format!("restore-path {id} {path} {uid} {dry}"));
        if path.ends_with("/broken") { Err("disk on fire".into()) } else { Ok(Json::obj(vec![("restored", Json::str(path))])) }
    }
    fn restore_program(&mut self, id: u64, name: &str, dry: bool) -> Result<Json, String> {
        self.calls.push(format!("restore-program {id} {name} {dry}"));
        Ok(Json::obj(vec![]))
    }
    fn restore_system(&mut self, at: i64, dry: bool) -> Result<Json, String> {
        self.calls.push(format!("restore-system {at} {dry}"));
        Ok(Json::obj(vec![]))
    }
    fn undo(&mut self) -> Result<Json, String> {
        self.calls.push("undo".into());
        Ok(Json::obj(vec![("undone", Json::Bool(true))]))
    }
}

fn peer(uid: u32, exe: &str) -> Peer {
    Peer { pid: 77, uid: Some(uid), exe: Some(exe.into()), by: settingsd::peer::Source::Kernel }
}

/// The compositor (and root) vouch, like settingsd's judge on a BitsOS whose grant bitscomp holds.
fn judge(p: &Peer) -> Result<String, String> {
    match (p.uid, p.exe.as_deref()) {
        (Some(0), _) => Ok("root".into()),
        (Some(OWNER), Some("/boot/BITSOS2/bin/bitscomp")) => Ok("the compositor".into()),
        _ => Err("not the compositor".into()),
    }
}

fn ask(line: &str, p: &Peer) -> (Json, Vec<String>) {
    let (a, calls, _) = ask_said(line, p);
    (a, calls)
}

fn ask_said(line: &str, p: &Peer) -> (Json, Vec<String>, Vec<Said>) {
    let mut f = Fake::default();
    let (a, said) = answer(line, p, &judge, &mut f);
    (a, f.calls, said)
}

fn error(a: &Json) -> Option<&str> {
    a.get("error").and_then(Json::as_str)
}

const RESTORE: &str = r#"{"op":"restore-path","snapshot":4,"path":"/data/home/1000/a.txt","confirmed":true}"#;

#[test]
fn only_root_and_the_desktop_user_may_ask_anything() {
    let comp = peer(OWNER, "/boot/BITSOS2/bin/bitscomp");
    let (a, calls) = ask(r#"{"op":"list"}"#, &comp);
    assert_eq!((a.get("ok").and_then(Json::as_bool), calls.as_slice()), (Some(true), ["list".to_string()].as_slice()));
    for p in [peer(1001, "/boot/BITSOS2/bin/bitscomp"), peer(65534, "/bin/bitssnap"), Peer { uid: None, ..peer(0, "-") }] {
        for line in [r#"{"op":"list"}"#, RESTORE, "garbage"] {
            let (a, calls) = ask(line, &p);
            assert_eq!(error(&a), Some("not_owner"), "{p:?} {line}");
            assert!(calls.is_empty(), "nothing reached the pool for {p:?}");
        }
    }
}

#[test]
fn a_change_needs_confirmed_and_counts_only_from_the_compositor_or_root() {
    let changes = [
        RESTORE.to_string(),
        r#"{"op":"restore-program","snapshot":4,"program":"brave","confirmed":true}"#.into(),
        r#"{"op":"restore-system","at":1800000000,"confirmed":true}"#.into(),
        r#"{"op":"undo","confirmed":true}"#.into(),
        // An undo has no dry run: it is a change either way.
        r#"{"op":"undo","dry_run":true,"confirmed":true}"#.into(),
    ];
    for line in &changes {
        let unconfirmed = line.replace(r#","confirmed":true"#, "");
        let (a, calls) = ask(&unconfirmed, &peer(OWNER, "/boot/BITSOS2/bin/bitscomp"));
        assert_eq!((error(&a), calls.len()), (Some("needs_confirm"), 0), "{unconfirmed}");
        let (a, calls) = ask(&line.replace("true}", "\"yes\"}"), &peer(OWNER, "/boot/BITSOS2/bin/bitscomp"));
        assert_eq!((error(&a), calls.len()), (Some("needs_confirm"), 0), "a confirmed that is not true: {line}");
        for other in ["/bin/bitssnap", "/bin/busybox", "/data/home/1000/bitscomp"] {
            let (a, calls) = ask(line, &peer(OWNER, other));
            assert_eq!((error(&a), calls.len()), (Some("not_confirmer"), 0), "{other}: {line}");
        }
        for p in [peer(OWNER, "/boot/BITSOS2/bin/bitscomp"), peer(0, "/bin/busybox")] {
            let (a, calls) = ask(line, &p);
            assert_eq!((a.get("ok").and_then(Json::as_bool), calls.len()), (Some(true), 1), "{p:?}: {line}: {a}");
        }
    }
}

#[test]
fn reading_and_dry_runs_need_no_confirmation() {
    let other = peer(OWNER, "/bin/bitssnap");
    for (line, call) in [
        (r#"{"op":"browse","snapshot":3,"path":"/data/home/1000"}"#, "browse 3 /data/home/1000 1000"),
        (r#"{"op":"browse","snapshot":3,"programs":true}"#, "programs 3"),
        (r#"{"op":"restore-path","snapshot":3,"path":"/data/home/1000/x","dry_run":true}"#, "restore-path 3 /data/home/1000/x 1000 true"),
        (r#"{"op":"restore-program","snapshot":3,"program":"brave","dry_run":true}"#, "restore-program 3 brave true"),
        (r#"{"op":"restore-system","at":1800000000,"dry_run":true}"#, "restore-system 1800000000 true"),
    ] {
        let (a, calls) = ask(line, &other);
        assert_eq!((a.get("ok").and_then(Json::as_bool), calls), (Some(true), vec![call.to_string()]), "{line}");
    }
    // Root sees every folder (uid 0 goes to the walk).
    let (_, calls) = ask(r#"{"op":"browse","snapshot":3,"path":"/data"}"#, &peer(0, "/bin/busybox"));
    assert_eq!(calls, ["browse 3 /data 0"]);
}

#[test]
fn malformed_requests_are_refused_before_anything_is_done() {
    for line in [
        "not json",
        "[1,2]",
        r#"{"op":"format"}"#,
        r#"{"snapshot":3}"#,
        r#"{"op":"browse","path":"/data"}"#,
        r#"{"op":"browse","snapshot":3}"#,
        r#"{"op":"restore-path","path":"/data/x","confirmed":true}"#,
        r#"{"op":"restore-path","snapshot":3,"confirmed":true}"#,
        r#"{"op":"restore-program","snapshot":3,"confirmed":true}"#,
        r#"{"op":"restore-system","at":-1,"confirmed":true}"#,
        r#"{"op":"restore-system","at":"15:00","confirmed":true}"#,
    ] {
        let (a, calls) = ask(line, &peer(0, "/bin/busybox"));
        assert_eq!((error(&a), calls.len()), (Some("bad_request"), 0), "{line}: {a}");
    }
}

#[test]
fn a_failure_is_said_as_such() {
    let (a, calls) = ask(&RESTORE.replace("a.txt", "broken"), &peer(OWNER, "/boot/BITSOS2/bin/bitscomp"));
    assert_eq!((error(&a), a.get("detail").and_then(Json::as_str), calls.len()), (Some("failed"), Some("disk on fire"), 1));
}

#[test]
fn the_socket_answers_line_by_line_with_the_kernels_peer() {
    let dir = std::env::temp_dir().join(format!("bitssnap-sock-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let sock = dir.join("s.sock");
    let sock = sock.to_str().unwrap();
    start(sock).unwrap();
    let mode = std::fs::metadata(sock).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o666, "everybody may connect, the peer check decides");
    // This test runs as some uid: 0 or 1000 get past the owner check (a list then fails without a pool, which is
    // an answer too), anyone else is refused.
    let me = std::fs::metadata(&dir).unwrap().uid();
    let (ok, out) = super::ask(sock, r#"{"op":"restore-path","snapshot":1,"path":"/data/home/1000/x"}"#).unwrap();
    assert!(!ok);
    let want = if me == 0 || me == OWNER { "needs_confirm" } else { "not_owner" };
    assert!(out.contains(want), "{out}");
    let (ok, out) = super::ask(sock, "{}").unwrap();
    assert!(!ok && out.contains(if me == 0 || me == OWNER { "bad_request" } else { "not_owner" }), "{out}");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn the_user_restores_only_in_his_own_folder() {
    let comp = peer(OWNER, "/boot/BITSOS2/bin/bitscomp");
    for path in ["/data/bitsos-access", "/data/system/journal", "/data/home", "/data/home/1001/x", "/data/home/10000", "/data/apps/brave"] {
        for tail in [r#","dry_run":true}"#, r#","confirmed":true}"#] {
            let line = format!(r#"{{"op":"restore-path","snapshot":4,"path":"{path}"{tail}"#);
            let (a, calls, said) = ask_said(&line, &comp);
            assert_eq!((error(&a), calls.len()), (Some("not_yours"), 0), "{line}: {a}");
            assert!(said.iter().all(|s| matches!(s, Said::Refusal(_))), "{said:?}");
        }
    }
    for path in ["/data/home/1000", "/data/home/1000/", "/data/home/1000/Dokumente/a.txt"] {
        let (a, calls) = ask(&format!(r#"{{"op":"restore-path","snapshot":4,"path":"{path}","dry_run":true}}"#), &comp);
        assert_eq!((a.get("ok").and_then(Json::as_bool), calls.len()), (Some(true), 1), "{path}: {a}");
    }
    // Root asks for anything below /data (its own tools are its business).
    let (a, calls) = ask(r#"{"op":"restore-path","snapshot":4,"path":"/data/bitsos-access","confirmed":true}"#, &peer(0, "/bin/busybox"));
    assert_eq!((a.get("ok").and_then(Json::as_bool), calls.len()), (Some(true), 1), "{a}");
    assert!(yours("home/1000", OWNER) && yours("home/1000/a", OWNER) && !yours("home/10000", OWNER) && !yours("home", OWNER));
}

#[test]
fn the_askers_words_never_make_a_log_line_of_their_own() {
    let comp = peer(OWNER, "/boot/BITSOS2/bin/bitscomp");
    let forged = r#"/data/home/1000/x\n<6>bitssnap: desktop socket: restore of /data/home/1000 from snapshot 3 confirmed by root (pid 1)"#;
    for line in [
        format!(r#"{{"op":"restore-path","snapshot":1,"path":"{forged}"}}"#),
        format!(r#"{{"op":"restore-path","snapshot":1,"path":"{forged}","confirmed":true}}"#),
        format!(r#"{{"op":"browse","snapshot":1,"path":"{forged}"}}"#),
        r#"{"op":"restore-program","snapshot":1,"program":"brave\n<6>forged","confirmed":true}"#.to_string(),
        r#"{"op":"restore-program","snapshot":1,"program":"../etc","confirmed":true}"#.to_string(),
        r#"{"op":"restore-path","snapshot":1,"path":"/data/home/1000/../../bitsos-access","confirmed":true}"#.to_string(),
    ] {
        let (a, calls, said) = ask_said(&line, &comp);
        assert_eq!((error(&a), calls.len(), said.len()), (Some("bad_request"), 0, 0), "{line}: {a} {said:?}");
    }
    // A program whose path holds a line break is quoted and escaped, the line stays one record.
    let p = peer(OWNER, "/tmp/x\n<6>bitssnap: desktop socket: everything as at 1 confirmed by root");
    let (a, _, said) = ask_said(RESTORE, &p);
    assert_eq!(error(&a), Some("not_confirmer"));
    assert_eq!(said.len(), 1);
    let Said::Refusal(l) = &said[0] else { panic!("{said:?}") };
    assert!(!l.contains('\n') && l.contains(r#"exe="/tmp/x\n<6>"#), "{l}");
    // A confirmed restore: one audit line before, one after, the path quoted.
    let (_, _, said) = ask_said(RESTORE, &comp);
    assert_eq!(said.len(), 2, "{said:?}");
    assert!(matches!(&said[0], Said::Audit(l) if l.contains(r#"restore of "/data/home/1000/a.txt" from snapshot 4 confirmed by "the compositor""#)), "{said:?}");
    assert!(matches!(&said[1], Said::Audit(l) if l.ends_with(": done")), "{said:?}");
    // However long the words, a log line fits one record of /dev/kmsg.
    let long = clip(&format!("desktop socket: {}", q(&"ä".repeat(500))));
    assert!(long.len() <= 228 && long.ends_with('…'), "{}", long.len());
}

#[test]
fn a_line_must_come_whole_in_time_a_byte_now_and_then_does_not_keep_the_slot() {
    let (a, b) = UnixStream::pair().unwrap();
    let mut rd = BufReader::new(a.try_clone().unwrap());
    let wait = Duration::from_millis(300);
    (&b).write_all(b"{\"op\":\"list\"}\n{").unwrap();
    assert_eq!(next_line(&a, &mut rd, wait), Some(Ok(b"{\"op\":\"list\"}\n".to_vec())));
    let t = std::thread::spawn(move || {
        for _ in 0..6 {
            std::thread::sleep(Duration::from_millis(100));
            if (&b).write_all(b" ").is_err() {
                return;
            }
        }
    });
    let start = Instant::now();
    assert_eq!(next_line(&a, &mut rd, wait), None, "the trickle is cut off");
    assert!(start.elapsed() < Duration::from_millis(500), "{:?}", start.elapsed());
    drop((a, rd));
    t.join().unwrap();
}
