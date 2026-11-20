//! The service: the library (scanned on its own thread), the player thread and the socket. Each connection has
//! its own thread; answers come from the cached library and status (B4), commands go to the player by channel.

use crate::events::{Events, Level, q};
use crate::player::{Cmd, Player, Queue, Status};
use crate::proto::{self, Pick, Refusal, Request};
use crate::scan::{self, Library};
use crate::sink::Sink;
use settingsd::json::Json;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// An idle connection ends after this long (a client that never sends does not keep a thread for ever).
const IDLE: Duration = Duration::from_secs(120);
/// `library` starts a new scan in the background when the last one is older (music added from Windows or the
/// network shows up the next time the collection opens; no inotify on FAT).
pub const STALE: Duration = Duration::from_secs(60);
/// Connections served at once (a per-input bound of the one socket, R1): the shell and the quick menu need a
/// few; a client that leaks connections gets a refusal instead of taking every thread.
pub const CLIENTS_MAX: usize = 128;

/// Settings of [`Service::start`] besides the folders and the sink.
#[derive(Clone, Debug)]
pub struct Opts {
    /// The memory of what played last (None: kept in memory only).
    pub last: Option<PathBuf>,
    pub stale: Duration,
}

impl Default for Opts {
    fn default() -> Opts {
        Opts {
            last: None,
            stale: STALE,
        }
    }
}

/// A served connection's place in [`CLIENTS_MAX`], given back however the thread ends.
struct Slot(Arc<Service>);
impl Drop for Slot {
    fn drop(&mut self) {
        self.0.clients.fetch_sub(1, Ordering::SeqCst);
    }
}

pub struct Service {
    roots: Vec<PathBuf>,
    ev: Events,
    lib: Mutex<Arc<Library>>,
    scanning: AtomicBool,
    scanned: AtomicBool,
    last_scan: Mutex<Option<Instant>>,
    stale: Duration,
    clients: AtomicUsize,
    status: Arc<Mutex<Status>>,
    cmd: Mutex<Sender<Cmd>>,
}

/// The music folders of spec/sofa-mode.md 3.5: the data volume's and the stick's (V1's `\BITSOS2\MUSIK\`; FAT
/// is case-blind, so one spelling finds it).
pub fn default_roots(uid: u32) -> Vec<PathBuf> {
    vec![
        PathBuf::from(format!("/data/home/{uid}/Musik")),
        PathBuf::from("/boot/BITSOS2/MUSIK"),
    ]
}

/// `$XDG_RUNTIME_DIR/bitsmusik.sock`, else `/run/user/<uid>/bitsmusik.sock`.
pub fn default_socket(uid: u32) -> PathBuf {
    match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(d) if !d.is_empty() => Path::new(&d).join("bitsmusik.sock"),
        _ => PathBuf::from(format!("/run/user/{uid}/bitsmusik.sock")),
    }
}

impl Service {
    pub fn start(roots: Vec<PathBuf>, sink: Box<dyn Sink>, ev: Events, opts: Opts) -> Arc<Service> {
        let (tx, rx) = channel();
        let status = Arc::new(Mutex::new(Status::default()));
        let player = Player::new(sink, status.clone(), ev.clone(), opts.last);
        std::thread::Builder::new()
            .name("player".into())
            .spawn(move || player.run(rx))
            .ok();
        let s = Arc::new(Service {
            roots,
            ev,
            lib: Mutex::new(Arc::new(Library::default())),
            scanning: AtomicBool::new(false),
            scanned: AtomicBool::new(false),
            last_scan: Mutex::new(None),
            stale: opts.stale,
            clients: AtomicUsize::new(0),
            status,
            cmd: Mutex::new(tx),
        });
        s.rescan();
        s
    }

    /// A scan on its own thread unless one runs; the old library answers meanwhile.
    pub fn rescan(self: &Arc<Self>) {
        if self.scanning.swap(true, Ordering::SeqCst) {
            return;
        }
        if let Ok(mut t) = self.last_scan.lock() {
            *t = Some(Instant::now());
        }
        let me = self.clone();
        let spawned = std::thread::Builder::new()
            .name("scan".into())
            .spawn(move || {
                // A panic in the scan must not leave `scanning` set for good (the shell would wait for ever).
                let lib = catch_unwind(AssertUnwindSafe(|| scan::scan(&me.roots, &me.ev)))
                    .unwrap_or_else(|_| {
                        me.ev
                            .emit(Level::Error, "scan.failed", "the scan stopped on an error");
                        Library::default()
                    });
                me.ev.emit(
                    Level::Info,
                    "scan.done",
                    &format!(
                        "folders={} tracks={} albums={} lists={}",
                        lib.roots.len(),
                        lib.tracks.len(),
                        lib.albums.len(),
                        lib.lists.len()
                    ),
                );
                if let Ok(mut l) = me.lib.lock() {
                    *l = Arc::new(lib);
                }
                me.scanned.store(true, Ordering::SeqCst);
                me.scanning.store(false, Ordering::SeqCst);
            });
        if spawned.is_err() {
            self.ev.emit(Level::Error, "scan.thread_failed", "");
            self.scanning.store(false, Ordering::SeqCst);
        }
    }

    fn library(&self) -> Arc<Library> {
        self.lib.lock().map(|l| l.clone()).unwrap_or_default()
    }

    fn send(&self, c: Cmd) -> Json {
        match self.cmd.lock().map(|tx| tx.send(c).is_ok()) {
            Ok(true) => Json::obj().with("ok", true),
            _ => proto::refusal(&Refusal {
                error: "player gone",
                text: "Der Musikspieler läuft nicht mehr",
            }),
        }
    }

    pub fn answer(self: &Arc<Self>, line: &str) -> Json {
        let r = match proto::parse(line) {
            Ok(r) => r,
            Err(e) => return proto::refusal(&e),
        };
        match r {
            Request::Status => self.status_json(),
            Request::Library => {
                if self
                    .last_scan
                    .lock()
                    .ok()
                    .and_then(|t| *t)
                    .is_some_and(|t| t.elapsed() >= self.stale)
                {
                    self.rescan();
                }
                self.library_json()
            }
            Request::Rescan => {
                self.rescan();
                Json::obj().with("ok", true)
            }
            Request::Play { pick, index } => match self.queue(&pick) {
                Ok(qu) if index < qu.tracks.len() => self.send(Cmd::Play(qu, index)),
                Ok(_) => proto::refusal(&Refusal {
                    error: "index out of range",
                    text: "Diesen Titel gibt es dort nicht",
                }),
                Err(e) => proto::refusal(&e),
            },
            Request::Pause => self.send(Cmd::Pause),
            Request::Resume => self.send(Cmd::Resume),
            Request::Next => self.send(Cmd::Next),
            Request::Prev => self.send(Cmd::Prev),
            Request::Stop => self.send(Cmd::Stop),
        }
    }

    fn queue(&self, pick: &Pick) -> Result<Queue, Refusal> {
        let lib = self.library();
        let empty = Refusal {
            error: "no music",
            text: "Keine Musik gefunden",
        };
        let qu = match pick {
            Pick::All => Queue {
                lib: lib.clone(),
                kind: "all",
                name: "Alle Titel".into(),
                tracks: (0..lib.tracks.len()).collect(),
            },
            Pick::List(n) | Pick::Album(n) => {
                let (groups, kind, missing) = match pick {
                    Pick::List(_) => (
                        &lib.lists,
                        "list",
                        Refusal {
                            error: "no such list",
                            text: "Diese Liste gibt es nicht",
                        },
                    ),
                    _ => (
                        &lib.albums,
                        "album",
                        Refusal {
                            error: "no such album",
                            text: "Dieses Album gibt es nicht",
                        },
                    ),
                };
                let g = groups
                    .iter()
                    .find(|g| g.name == *n)
                    .or_else(|| {
                        groups
                            .iter()
                            .find(|g| g.name.to_lowercase() == n.to_lowercase())
                    })
                    .ok_or(missing)?;
                Queue {
                    lib: lib.clone(),
                    kind,
                    name: g.name.clone(),
                    tracks: g.tracks.clone(),
                }
            }
            Pick::Track(t) => {
                let tl = t.to_lowercase();
                let i = lib
                    .tracks
                    .iter()
                    .position(|x| x.path.to_string_lossy() == *t)
                    .or_else(|| lib.tracks.iter().position(|x| x.title.to_lowercase() == tl))
                    .ok_or(Refusal {
                        error: "no such track",
                        text: "Diesen Titel gibt es nicht",
                    })?;
                Queue {
                    lib: lib.clone(),
                    kind: "track",
                    name: lib.tracks[i].title.clone(),
                    tracks: vec![i],
                }
            }
        };
        if qu.tracks.is_empty() {
            Err(empty)
        } else {
            Ok(qu)
        }
    }

    fn status_json(&self) -> Json {
        let s = self.status.lock().map(|s| s.clone()).unwrap_or_default();
        let mut j = Json::obj()
            .with("ok", true)
            .with("state", s.state.word())
            .with("playing", s.state == crate::player::State::Playing);
        let track = s
            .queue
            .as_ref()
            .and_then(|qu| qu.tracks.get(s.at).map(|&i| &qu.lib.tracks[i]));
        let text = |o: Option<&str>| o.map_or(Json::Null, Json::from);
        // Nothing queued since the start: the list from before the restart (its count from today's library).
        let remembered = (s.queue.is_none() && !s.last.name.is_empty()).then_some(&s.last);
        let lib = self.library();
        let remembered_count = remembered.map_or(0, |l| {
            let groups = match l.kind.as_str() {
                "list" => &lib.lists,
                "album" => &lib.albums,
                "all" => return lib.tracks.len(),
                _ => return usize::from(lib.tracks.iter().any(|t| t.title == l.name)),
            };
            groups
                .iter()
                .find(|g| g.name.to_lowercase() == l.name.to_lowercase())
                .map_or(0, |g| g.tracks.len())
        });
        j = j
            .with("title", text(track.map(|t| t.title.as_str())))
            .with(
                "artist",
                text(track.map(|t| t.artist.as_str()).filter(|a| !a.is_empty())),
            )
            .with("album", text(track.map(|t| t.album.as_str())))
            .with(
                "list",
                text(
                    s.queue
                        .as_ref()
                        .map(|qu| qu.name.as_str())
                        .or(remembered.map(|l| l.name.as_str())),
                ),
            )
            .with(
                "kind",
                text(
                    s.queue
                        .as_ref()
                        .map(|qu| qu.kind)
                        .or(remembered.map(|l| l.kind.as_str())),
                ),
            )
            .with("index", s.at)
            .with(
                "count",
                s.queue
                    .as_ref()
                    .map_or(remembered_count, |qu| qu.tracks.len()),
            )
            .with("position", Json::Float((s.position * 10.0).round() / 10.0))
            .with(
                "duration",
                s.duration
                    .map_or(Json::Null, |d| Json::Float((d * 10.0).round() / 10.0)),
            )
            .with("output", s.output.as_str())
            .with("route", s.route.as_str());
        if let Some(e) = &s.error {
            j = j.with("error", e.as_str());
        }
        j
    }

    fn library_json(&self) -> Json {
        let lib = self.library();
        let group = |g: &scan::Group| {
            let mut o = Json::obj()
                .with("name", g.name.as_str())
                .with("tracks", g.tracks.len());
            if !g.artist.is_empty() {
                o = o.with("artist", g.artist.as_str());
            }
            o
        };
        Json::obj()
            .with("ok", true)
            .with(
                "scanning",
                self.scanning.load(Ordering::SeqCst) || !self.scanned.load(Ordering::SeqCst),
            )
            .with("tracks", lib.tracks.len())
            .with("lists", lib.lists.iter().map(group).collect::<Vec<_>>())
            .with("albums", lib.albums.iter().map(group).collect::<Vec<_>>())
            .with(
                "folders",
                lib.roots
                    .iter()
                    .map(|r| Json::from(r.to_string_lossy().as_ref()))
                    .collect::<Vec<_>>(),
            )
    }

    /// Serves `path` until the process ends. Refuses to start beside a running player on the same socket.
    pub fn serve(self: &Arc<Self>, path: &Path) -> Result<(), String> {
        if UnixStream::connect(path).is_ok() {
            return Err(format!(
                "{} answers already: another bitsmusik runs",
                path.display()
            ));
        }
        let _ = std::fs::remove_file(path);
        let l = UnixListener::bind(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
        self.ev.emit(
            Level::Info,
            "serve.start",
            &format!(
                "socket={} folders={}",
                q(&path.to_string_lossy()),
                q(&self
                    .roots
                    .iter()
                    .map(|r| r.to_string_lossy())
                    .collect::<Vec<_>>()
                    .join(","))
            ),
        );
        for c in l.incoming() {
            let Ok(mut c) = c else { continue };
            if self.clients.fetch_add(1, Ordering::SeqCst) >= CLIENTS_MAX {
                self.clients.fetch_sub(1, Ordering::SeqCst);
                self.ev
                    .emit(Level::Warn, "serve.too_many", &format!("max={CLIENTS_MAX}"));
                let _ = c.set_write_timeout(Some(Duration::from_secs(1)));
                let _ = writeln!(
                    c,
                    "{}",
                    proto::refusal(&Refusal {
                        error: "too many connections",
                        text: "Zu viele Verbindungen zum Musikspieler",
                    })
                    .to_text()
                );
                continue;
            }
            let me = self.clone();
            if std::thread::Builder::new()
                .name("client".into())
                .spawn(move || {
                    let _slot = Slot(me.clone());
                    me.client(c);
                })
                .is_err()
            {
                self.clients.fetch_sub(1, Ordering::SeqCst);
                self.ev.emit(Level::Warn, "serve.thread_failed", "");
            }
        }
        Ok(())
    }

    fn client(self: Arc<Self>, c: UnixStream) {
        let _ = c.set_read_timeout(Some(IDLE));
        let _ = c.set_write_timeout(Some(Duration::from_secs(5)));
        let Ok(mut w) = c.try_clone() else { return };
        let mut r = BufReader::new(c);
        loop {
            let mut line = Vec::new();
            match (&mut r)
                .take(proto::LINE_MAX as u64 + 1)
                .read_until(b'\n', &mut line)
            {
                Ok(0) | Err(_) => return,
                Ok(_) => {}
            }
            if line.len() > proto::LINE_MAX {
                let _ = writeln!(
                    w,
                    "{}",
                    proto::refusal(&Refusal {
                        error: "line too long",
                        text: "Anfrage zu lang"
                    })
                    .to_text()
                );
                return;
            }
            let text = String::from_utf8_lossy(&line);
            if text.trim().is_empty() {
                continue;
            }
            let a = self.answer(&text);
            if writeln!(w, "{}", a.to_text()).is_err() {
                return;
            }
        }
    }
}
