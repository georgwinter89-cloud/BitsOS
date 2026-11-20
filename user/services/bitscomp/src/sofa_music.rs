//! GM-6b: the sofa mode's view of the music player `bitsmusik` (GM-6m; plan/spec/sofa-mode.md 3.5): JSON lines on
//! `/run/user/1000/bitsmusik.sock` (`BITSCOMP_MUSIC_SOCKET` for tests), asked on a thread of its own with a
//! deadline (B4), the answer cached for the start card and the collection Musik. No socket or no answer: the card
//! says "Musik: noch nicht da" (2.1). Nothing here plays or opens a sound device: a tile only asks the player.

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use mcpreg::Json;
use smithay::reexports::calloop::LoopHandle;
use smithay::reexports::calloop::channel::{self, Sender};

pub const SOCKET: &str = "/run/user/1000/bitsmusik.sock";
/// Writing the requests and reading the whole answer at most this long each.
pub const DEADLINE: Duration = Duration::from_secs(2);
/// One answer line at most (R1: per input; bitsmusik's own bound is 64 KiB per request line).
pub const MAX_REPLY: u64 = 1 << 20;

/// What the player said last.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Music {
    pub tracks: usize,
    /// (name, tracks) of the lists, then of the albums.
    pub lists: Vec<(String, usize)>,
    pub albums: Vec<(String, usize)>,
    /// The list (or album) playing or played last, its track count and the German output word.
    pub last: Option<(String, usize, String)>,
    pub playing: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Job {
    /// `status` and `library`.
    Ask,
    /// `play {list|album: name}`.
    Play { kind: &'static str, name: String },
}

/// The player's route word in German (GM-6m's change request 2); `auto` (or an unknown word) is left out.
pub fn output_word(o: &str) -> &'static str {
    match o {
        "hdmi" => "über HDMI",
        "jack" => "über Kopfhörer",
        "usb" => "über USB",
        "none" => "keine Ausgabe",
        _ => "",
    }
}

fn named(v: Option<&Json>) -> Vec<(String, usize)> {
    v.and_then(Json::as_array).unwrap_or(&[]).iter().filter_map(|j| Some((j.get("name")?.as_str()?.to_string(), j.get("tracks").and_then(Json::as_u64).unwrap_or(0) as usize))).collect()
}

/// The model from the two answers (`status`, `library`); Err when the player refused.
pub fn model(status: &Json, library: &Json) -> Result<Music, String> {
    for j in [status, library] {
        if j.get("ok").and_then(Json::as_bool) != Some(true) {
            return Err(j.get("text").or(j.get("error")).and_then(Json::as_str).unwrap_or("no ok").to_string());
        }
    }
    let list = status.get("list").and_then(Json::as_str).filter(|s| !s.is_empty());
    let last = list.map(|l| {
        let count = status.get("count").and_then(Json::as_u64).unwrap_or(0) as usize;
        // `route`, not `output`: `output` is `none` whenever nothing plays (INT-GM6, GM-6m's change request 2).
        let route = status.get("route").or(status.get("output")).and_then(Json::as_str).unwrap_or("none");
        (l.to_string(), count, output_word(route).to_string())
    });
    Ok(Music {
        tracks: library.get("tracks").and_then(Json::as_u64).unwrap_or(0) as usize,
        lists: named(library.get("lists")),
        albums: named(library.get("albums")),
        last,
        playing: status.get("playing").and_then(Json::as_bool).unwrap_or(false),
    })
}

fn line(r: &mut impl BufRead) -> Result<Json, String> {
    let mut v = Vec::new();
    r.take(MAX_REPLY).read_until(b'\n', &mut v).map_err(|e| e.to_string())?;
    if v.last() != Some(&b'\n') {
        return Err(if v.len() as u64 >= MAX_REPLY { "answer too long".into() } else { "no answer".into() });
    }
    Json::parse(String::from_utf8_lossy(&v).trim())
}

/// The socket read with one deadline for the whole answer (a player that trickles bytes cannot hold the thread).
struct Until<'a> {
    s: &'a UnixStream,
    end: Instant,
}

impl Read for Until<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let left = self.end.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "no answer in time"));
        }
        self.s.set_read_timeout(Some(left))?;
        self.s.read(buf)
    }
}

fn talk(p: &Path, reqs: &[Json], dl: Duration) -> Result<Vec<Json>, String> {
    let s = UnixStream::connect(p).map_err(|e| e.to_string())?;
    s.set_write_timeout(Some(dl)).map_err(|e| e.to_string())?;
    let mut out = String::new();
    for r in reqs {
        r.write_to(&mut out);
        out.push('\n');
    }
    (&s).write_all(out.as_bytes()).map_err(|e| e.to_string())?;
    let mut rd = BufReader::new(Until { s: &s, end: Instant::now() + dl });
    reqs.iter().map(|_| line(&mut rd)).collect()
}

fn op(name: &str) -> Json {
    Json::obj(vec![("op", Json::str(name))])
}

/// The socket's path.
pub fn socket() -> PathBuf {
    PathBuf::from(SOCKET)
}

/// The thread: each job's answer goes to `came` on the main loop (None: no player, with the reason logged once
/// per change).
pub fn start<D: 'static>(handle: &LoopHandle<'static, D>, p: PathBuf, came: impl Fn(&mut D, Option<Music>) + 'static) -> Result<mpsc::Sender<Job>, String> {
    let (tx, rx) = channel::channel::<Option<Music>>();
    let (jtx, jrx) = mpsc::channel::<Job>();
    handle
        .insert_source(rx, move |ev, _, d| {
            if let channel::Event::Msg(m) = ev {
                came(d, m);
            }
        })
        .map_err(|e| format!("music channel: {e}"))?;
    std::thread::Builder::new().name("bitscomp-music".into()).spawn(move || worker(p, tx, jrx)).map_err(|e| format!("music thread: {e}"))?;
    Ok(jtx)
}

fn worker(p: PathBuf, tx: Sender<Option<Music>>, jobs: mpsc::Receiver<Job>) {
    let mut said = String::new();
    // A job taken while Asks were merged waits here for the next turn: a Play queued behind an Ask is never lost.
    let mut next: Option<Job> = None;
    loop {
        let job = match next.take() {
            Some(j) => j,
            None => match jobs.recv() {
                Ok(j) => j,
                Err(_) => return,
            },
        };
        if let Job::Play { kind, name } = &job {
            let r = talk(&p, &[Json::obj(vec![("op", Json::str("play")), (kind, Json::str(name))])], DEADLINE);
            match r.as_ref().map(|v| v[0].get("ok").and_then(Json::as_bool)) {
                Ok(Some(true)) => eprintln!("bitscomp: sofa: music: playing {kind} {name:?}"),
                _ => eprintln!("bitscomp: sofa: music: play {kind} {name:?} refused: {r:?}"),
            }
        }
        while let Ok(j) = jobs.try_recv() {
            if j != Job::Ask {
                next = Some(j);
                break;
            }
        }
        let m = ask(&p, DEADLINE);
        let now = match &m {
            Ok(_) => "there".to_string(),
            Err(e) => e.clone(),
        };
        if now != said {
            eprintln!("bitscomp: sofa: music player {}: {now}", p.display());
            said = now;
        }
        if tx.send(m.ok()).is_err() {
            return;
        }
    }
}

/// `status` and `library` in one connection, as the card's model.
fn ask(p: &Path, dl: Duration) -> Result<Music, String> {
    talk(p, &[op("status"), op("library")], dl).and_then(|v| model(&v[0], &v[1]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_cards_model_comes_from_status_and_library() {
        let st = Json::parse(r#"{"ok":true,"state":"playing","playing":true,"title":"Sinus","list":"Abendliste","kind":"list","index":0,"count":34,"output":"hdmi"}"#).unwrap();
        let lib = Json::parse(r#"{"ok":true,"scanning":false,"tracks":412,"lists":[{"name":"Abendliste","tracks":34}],"albums":[{"name":"Blau","tracks":10,"artist":"X"},{"bad":1}]}"#).unwrap();
        let m = model(&st, &lib).unwrap();
        assert_eq!(m.tracks, 412);
        assert_eq!(m.lists, vec![("Abendliste".to_string(), 34)]);
        assert_eq!(m.albums, vec![("Blau".to_string(), 10)]);
        assert_eq!(m.last, Some(("Abendliste".into(), 34, "über HDMI".into())));
        let refused = Json::parse(r#"{"ok":false,"error":"no music","text":"Keine Musik"}"#).unwrap();
        assert_eq!(model(&refused, &lib), Err("Keine Musik".into()));
        let stopped = Json::parse(r#"{"ok":true,"state":"stopped","playing":false,"list":""}"#).unwrap();
        assert_eq!(model(&stopped, &lib).unwrap().last, None);
        let idle = Json::parse(r#"{"ok":true,"playing":false,"list":"Abendliste","count":34,"output":"none","route":"jack"}"#).unwrap();
        assert_eq!(model(&idle, &lib).unwrap().last, Some(("Abendliste".into(), 34, "über Kopfhörer".into())));
        let auto = Json::parse(r#"{"ok":true,"playing":false,"list":"Abendliste","count":34,"output":"none","route":"auto"}"#).unwrap();
        assert_eq!(model(&auto, &lib).unwrap().last, Some(("Abendliste".into(), 34, String::new())));
    }

    #[test]
    fn no_socket_is_no_player() {
        let p = PathBuf::from(std::env::var("CARGO_TARGET_DIR").unwrap_or_else(|_| "target".into())).join("no-bitsmusik.sock");
        assert!(talk(&p, &[op("status")], DEADLINE).is_err());
    }

    fn sock(name: &str) -> PathBuf {
        let d = PathBuf::from(std::env::var("CARGO_TARGET_DIR").unwrap_or_else(|_| "target".into())).join("music-tests");
        std::fs::create_dir_all(&d).unwrap();
        let p = d.join(name);
        let _ = std::fs::remove_file(&p);
        p
    }

    /// A one-connection player: reads the request lines, then writes `reply` (or keeps silent while `hold`).
    fn player(p: &Path, reply: Vec<u8>, hold: Duration) -> std::thread::JoinHandle<()> {
        let l = std::os::unix::net::UnixListener::bind(p).unwrap();
        std::thread::spawn(move || {
            let (mut c, _) = l.accept().unwrap();
            let mut first = String::new();
            let _ = BufReader::new(&c).read_line(&mut first);
            let _ = c.write_all(&reply);
            std::thread::sleep(hold);
        })
    }

    #[test]
    fn a_player_that_answers_gives_the_card() {
        let p = sock("ok.sock");
        let st = r#"{"ok":true,"playing":false,"list":"Abendliste","count":3,"output":"jack"}"#;
        let lib = r#"{"ok":true,"tracks":3,"lists":[{"name":"Abendliste","tracks":3}],"albums":[]}"#;
        let t = player(&p, format!("{st}\n{lib}\n").into_bytes(), Duration::ZERO);
        let m = ask(&p, DEADLINE).unwrap();
        assert_eq!(m.tracks, 3);
        assert_eq!(m.last, Some(("Abendliste".into(), 3, "über Kopfhörer".into())));
        t.join().unwrap();
    }

    #[test]
    fn a_silent_player_ends_at_the_deadline() {
        let p = sock("silent.sock");
        let t = player(&p, Vec::new(), Duration::from_millis(1500));
        let t0 = Instant::now();
        let dl = Duration::from_millis(200);
        assert!(ask(&p, dl).is_err());
        assert!(t0.elapsed() < dl + Duration::from_millis(800), "took {:?}", t0.elapsed());
        t.join().unwrap();
    }

    #[test]
    fn a_trickling_player_cannot_hold_the_thread() {
        let p = sock("trickle.sock");
        let l = std::os::unix::net::UnixListener::bind(&p).unwrap();
        let t = std::thread::spawn(move || {
            let (mut c, _) = l.accept().unwrap();
            for _ in 0..30 {
                if c.write_all(b" ").is_err() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        });
        let t0 = Instant::now();
        let dl = Duration::from_millis(300);
        assert!(ask(&p, dl).is_err());
        assert!(t0.elapsed() < dl + Duration::from_millis(800), "took {:?}", t0.elapsed());
        t.join().unwrap();
    }

    #[test]
    fn an_overlong_answer_is_refused_without_panic() {
        let p = sock("long.sock");
        let t = player(&p, vec![b'x'; MAX_REPLY as usize + 10], Duration::ZERO);
        assert_eq!(ask(&p, Duration::from_secs(5)), Err("answer too long".into()));
        t.join().unwrap();
    }

    #[test]
    fn a_play_behind_an_ask_is_kept() {
        let p = sock("queue.sock");
        let (tx, rx) = mpsc::channel();
        tx.send(Job::Ask).unwrap();
        tx.send(Job::Ask).unwrap();
        tx.send(Job::Play { kind: "list", name: "Abendliste".into() }).unwrap();
        tx.send(Job::Ask).unwrap();
        drop(tx);
        // Without a player every job fails fast; what the worker took is what it logged and asked about.
        let l = std::os::unix::net::UnixListener::bind(&p).unwrap();
        let seen = std::thread::spawn(move || {
            let mut lines = Vec::new();
            l.set_nonblocking(false).unwrap();
            for _ in 0..3 {
                let (c, _) = l.accept().unwrap();
                let mut first = String::new();
                let _ = BufReader::new(&c).read_line(&mut first);
                lines.push(first.trim().to_string());
            }
            lines
        });
        let (ctx, _crx) = channel::channel::<Option<Music>>();
        worker(p, ctx, rx);
        let lines = seen.join().unwrap();
        assert_eq!(lines.len(), 3);
        assert!(lines[0].contains("status"), "{lines:?}");
        assert!(lines[1].contains("play") && lines[1].contains("Abendliste"), "{lines:?}");
        assert!(lines[2].contains("status"), "{lines:?}");
    }
}
