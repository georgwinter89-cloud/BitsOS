//! The socket protocol of spec/sofa-mode.md 3.5 against the real binary with `--sink null` (a clock-paced
//! sink: the host's /dev/snd is never opened): library, play of a list, status with a moving position, pause,
//! resume, next, prev, stop, refusals, a second instance refused, a line too long.

mod common;

use common::*;
use settingsd::json::{self, Json};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

struct Server(Child);
impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn start(dir: &Path, sock: &Path) -> Server {
    start_with(dir, sock, &[])
}

/// The memory of the last list goes beside the socket (never under the host's /data).
fn start_with(dir: &Path, sock: &Path, extra: &[&str]) -> Server {
    let c = Command::new(env!("CARGO_BIN_EXE_bitsmusik"))
        .args(["serve", "--sink", "null", "--dir"])
        .arg(dir)
        .arg("--socket")
        .arg(sock)
        .arg("--last")
        .arg(sock.with_extension("last"))
        .args(extra)
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let end = Instant::now() + Duration::from_secs(10);
    while UnixStream::connect(sock).is_err() {
        assert!(Instant::now() < end, "the socket did not come up");
        std::thread::sleep(Duration::from_millis(20));
    }
    Server(c)
}

struct Conn(BufReader<UnixStream>);
impl Conn {
    fn new(sock: &Path) -> Conn {
        let s = UnixStream::connect(sock).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        Conn(BufReader::new(s))
    }
    fn ask(&mut self, line: &str) -> Json {
        writeln!(self.0.get_mut(), "{line}").unwrap();
        let mut a = String::new();
        self.0.read_line(&mut a).unwrap();
        json::parse(a.trim()).unwrap_or_else(|e| panic!("{e}: {a:?}"))
    }
}

fn num(j: &Json, k: &str) -> f64 {
    match j.get(k) {
        Some(Json::Float(f)) => *f,
        Some(Json::Int(i)) => *i as f64,
        other => panic!("{k}: {other:?} in {}", j.to_text()),
    }
}

#[test]
fn the_protocol() {
    let d = tmp("socket");
    let m = d.join("Musik");
    std::fs::create_dir_all(m.join("Album")).unwrap();
    for (n, t) in [("01", "Eins"), ("02", "Zwei"), ("03", "Drei")] {
        let comments = [
            format!("TITLE={t}"),
            "ALBUM=Album".into(),
            "ARTIST=Prüfer".into(),
            format!("TRACKNUMBER={n}"),
        ];
        let c: Vec<&str> = comments.iter().map(String::as_str).collect();
        std::fs::write(
            m.join(format!("Album/{n}.flac")),
            flac(22050, 1, &sine(22050, 1, 4.0, 330.0), &c),
        )
        .unwrap();
    }
    std::fs::write(m.join("Abend.m3u"), "Album/02.flac\nAlbum/03.flac\n").unwrap();
    let sock = d.join("s.sock");
    let _srv = start(&m, &sock);
    let mut c = Conn::new(&sock);
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        std::fs::metadata(&sock).unwrap().permissions().mode() & 0o777,
        0o600
    );

    let end = Instant::now() + Duration::from_secs(10);
    let lib = loop {
        let l = c.ask(r#"{"op":"library"}"#);
        if l.get("scanning") == Some(&Json::Bool(false)) {
            break l;
        }
        assert!(Instant::now() < end, "the scan did not finish");
        std::thread::sleep(Duration::from_millis(50));
    };
    assert_eq!(num(&lib, "tracks"), 3.0, "{}", lib.to_text());
    assert!(
        lib.to_text()
            .contains(r#""lists":[{"name":"Abend","tracks":2}]"#),
        "{}",
        lib.to_text()
    );
    assert!(
        lib.to_text()
            .contains(r#""albums":[{"name":"Album","tracks":3,"artist":"Prüfer"}]"#),
        "{}",
        lib.to_text()
    );

    let s = c.ask("status");
    assert_eq!(
        (s.str("state"), s.get("title")),
        (Some("stopped"), Some(&Json::Null))
    );

    assert_eq!(
        c.ask(r#"{"op":"play","list":"Abend","index":0}"#).get("ok"),
        Some(&Json::Bool(true))
    );
    std::thread::sleep(Duration::from_millis(700));
    let a = c.ask(r#"{"op":"status"}"#);
    std::thread::sleep(Duration::from_millis(700));
    let b = c.ask(r#"{"op":"status"}"#);
    assert_eq!(
        (
            a.str("state"),
            a.str("title"),
            a.str("list"),
            a.str("artist"),
            a.str("output")
        ),
        (
            Some("playing"),
            Some("Zwei"),
            Some("Abend"),
            Some("Prüfer"),
            Some("null")
        ),
        "{}",
        a.to_text()
    );
    assert_eq!((num(&a, "count"), num(&a, "index")), (2.0, 0.0));
    assert!(
        num(&b, "position") > num(&a, "position"),
        "position did not move: {} then {}",
        a.to_text(),
        b.to_text()
    );
    assert!((num(&a, "duration") - 4.0).abs() < 0.05, "{}", a.to_text());

    assert_eq!(c.ask("pause").get("ok"), Some(&Json::Bool(true)));
    std::thread::sleep(Duration::from_millis(300));
    let p1 = c.ask("status");
    std::thread::sleep(Duration::from_millis(500));
    let p2 = c.ask("status");
    assert_eq!(p1.str("state"), Some("paused"));
    assert_eq!(
        num(&p1, "position"),
        num(&p2, "position"),
        "a paused position moved"
    );
    c.ask("resume");
    std::thread::sleep(Duration::from_millis(400));
    assert!(num(&c.ask("status"), "position") > num(&p2, "position"));

    c.ask("next");
    std::thread::sleep(Duration::from_millis(300));
    let n = c.ask("status");
    assert_eq!(
        (n.str("title"), num(&n, "index")),
        (Some("Drei"), 1.0),
        "{}",
        n.to_text()
    );
    c.ask("prev");
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(c.ask("status").str("title"), Some("Zwei"));

    // An album by name, a track by title; the queue's end stops.
    c.ask(r#"{"op":"play","album":"album","index":2}"#);
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(c.ask("status").str("title"), Some("Drei"));
    c.ask(r#"{"op":"play","track":"eins"}"#);
    std::thread::sleep(Duration::from_millis(300));
    let t = c.ask("status");
    assert_eq!(
        (t.str("title"), t.str("kind")),
        (Some("Eins"), Some("track"))
    );
    assert_eq!(c.ask("stop").get("ok"), Some(&Json::Bool(true)));
    std::thread::sleep(Duration::from_millis(200));
    let st = c.ask("status");
    assert_eq!(
        (st.str("state"), st.str("route")),
        (Some("stopped"), Some("null")),
        "{}",
        st.to_text()
    );

    // Two connections at once (the shell and the quick menu): answers stay on their own connection.
    let mut c2 = Conn::new(&sock);
    c.ask(r#"{"op":"play","list":"Abend"}"#);
    for _ in 0..5 {
        assert_eq!(c2.ask("status").get("ok"), Some(&Json::Bool(true)));
        assert_eq!(c.ask("library").get("ok"), Some(&Json::Bool(true)));
    }
    assert_eq!(c2.ask("pause").get("ok"), Some(&Json::Bool(true)));
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(c.ask("status").str("state"), Some("paused"));
    c2.ask("stop");
    drop(c2);

    // A file added later shows after `rescan`.
    std::fs::write(
        m.join("Album/04.flac"),
        flac(
            22050,
            1,
            &sine(22050, 1, 0.5, 330.0),
            &["TITLE=Vier", "ALBUM=Album"],
        ),
    )
    .unwrap();
    assert_eq!(c.ask("rescan").get("ok"), Some(&Json::Bool(true)));
    let end = Instant::now() + Duration::from_secs(10);
    loop {
        let l = c.ask("library");
        if l.get("scanning") == Some(&Json::Bool(false)) && num(&l, "tracks") == 4.0 {
            break;
        }
        assert!(
            Instant::now() < end,
            "the rescan did not show the new file: {}",
            l.to_text()
        );
        std::thread::sleep(Duration::from_millis(50));
    }

    // Refusals: German text, English error; the connection stays usable.
    for (line, err) in [
        (r#"{"op":"play","list":"Gibtsnicht"}"#, "no such list"),
        (
            r#"{"op":"play","list":"Abend","index":5}"#,
            "index out of range",
        ),
        (
            r#"{"op":"play","list":"Abend","album":"Album"}"#,
            "bad request",
        ),
        (r#"{"op":"play","index":-1}"#, "bad request"),
        (r#"{"op":"lauter"}"#, "bad request"),
        ("{kaputt", "bad request"),
    ] {
        let r = c.ask(line);
        assert_eq!(
            (r.get("ok"), r.str("error")),
            (Some(&Json::Bool(false)), Some(err)),
            "{line}"
        );
        assert!(r.str("text").is_some_and(|t| !t.is_empty()));
    }
    assert_eq!(c.ask("status").get("ok"), Some(&Json::Bool(true)));

    // ctl, the shell's way in.
    let out = Command::new(env!("CARGO_BIN_EXE_bitsmusik"))
        .args(["ctl", "--socket"])
        .arg(&sock)
        .arg("status")
        .output()
        .unwrap();
    assert!(
        out.status.success()
            && String::from_utf8_lossy(&out.stdout).contains(r#""state":"stopped""#)
    );
    let ctl = |line: &str| {
        Command::new(env!("CARGO_BIN_EXE_bitsmusik"))
            .args(["ctl", "--socket"])
            .arg(&sock)
            .arg(line)
            .output()
            .unwrap()
    };
    let out = ctl(r#"{"op":"play","album":"Album","index":3}"#);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(c.ask("status").str("title"), Some("Vier"));
    let out = ctl(r#"{"op":"play","list":"Gibtsnicht"}"#);
    assert_eq!(out.status.code(), Some(1));
    assert!(ctl("stop").status.success());

    // A second player on the same socket is refused (exit 1), the first answers on.
    let second = Command::new(env!("CARGO_BIN_EXE_bitsmusik"))
        .args(["serve", "--sink", "null", "--dir"])
        .arg(&m)
        .arg("--socket")
        .arg(&sock)
        .arg("--last")
        .arg(d.join("second.last"))
        .stderr(Stdio::null())
        .status()
        .unwrap();
    assert_eq!(second.code(), Some(1));
    assert_eq!(c.ask("status").get("ok"), Some(&Json::Bool(true)));

    // A line over the bound is refused and the connection closed.
    let mut s = UnixStream::connect(&sock).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let _ = s.write_all(&vec![b'x'; bitsmusik::proto::LINE_MAX + 10]);
    let mut a = String::new();
    BufReader::new(s).read_line(&mut a).unwrap();
    assert!(a.contains("line too long"), "{a}");
}

#[test]
fn empty_library_refuses_play() {
    let d = tmp("empty");
    let sock = d.join("s.sock");
    let _srv = start(&d.join("gibtsnicht"), &sock);
    let mut c = Conn::new(&sock);
    std::thread::sleep(Duration::from_millis(200));
    let r = c.ask("play");
    assert_eq!(r.str("error"), Some("no music"), "{}", r.to_text());
    let l = c.ask("library");
    assert_eq!(
        (num(&l, "tracks"), l.get("folders")),
        (0.0, Some(&Json::Arr(vec![])))
    );
}

#[test]
fn the_last_list_survives_a_restart() {
    let d = tmp("last");
    let m = d.join("Musik");
    std::fs::create_dir_all(&m).unwrap();
    for n in ["a", "b"] {
        std::fs::write(
            m.join(format!("{n}.wav")),
            wav(8000, 1, &sine(8000, 1, 2.0, 300.0)),
        )
        .unwrap();
    }
    std::fs::write(m.join("Nacht.m3u"), "a.wav\nb.wav\n").unwrap();
    let sock = d.join("s.sock");
    {
        let _srv = start(&m, &sock);
        let mut c = Conn::new(&sock);
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(
            c.ask(r#"{"op":"play","list":"Nacht"}"#).get("ok"),
            Some(&Json::Bool(true))
        );
        std::thread::sleep(Duration::from_millis(1300));
    }
    let _srv = start(&m, &sock);
    let mut c = Conn::new(&sock);
    let end = Instant::now() + Duration::from_secs(10);
    let s = loop {
        let s = c.ask("status");
        if num(&s, "count") == 2.0 {
            break s;
        }
        assert!(Instant::now() < end, "{}", s.to_text());
        std::thread::sleep(Duration::from_millis(50));
    };
    assert_eq!(
        (s.str("state"), s.str("list"), s.str("kind"), s.str("route")),
        (Some("stopped"), Some("Nacht"), Some("list"), Some("null")),
        "{}",
        s.to_text()
    );
}

#[test]
fn an_old_scan_is_redone_by_library() {
    let d = tmp("stale");
    let m = d.join("Musik");
    std::fs::create_dir_all(&m).unwrap();
    let sock = d.join("s.sock");
    let _srv = start_with(&m, &sock, &["--stale-secs", "0"]);
    let mut c = Conn::new(&sock);
    std::thread::sleep(Duration::from_millis(300));
    std::fs::write(m.join("neu.wav"), wav(8000, 1, &sine(8000, 1, 0.2, 300.0))).unwrap();
    // No `rescan`: `library` itself starts the new scan when the last one is too old.
    let end = Instant::now() + Duration::from_secs(10);
    while num(&c.ask("library"), "tracks") != 1.0 {
        assert!(Instant::now() < end, "library did not scan again");
        std::thread::sleep(Duration::from_millis(50));
    }
}
