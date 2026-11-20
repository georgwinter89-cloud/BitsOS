//! The player thread: owns the sink and the current decoder, takes commands from the socket threads over a
//! channel and publishes its [`Status`] after every piece it writes, so `status` answers from memory (B4).

use crate::decode::Decoding;
use crate::events::{Events, Level, q};
use crate::last::{self, Last};
use crate::scan::Library;
use crate::sink::{Resampler, Sink};
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, RecvTimeoutError, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum State {
    #[default]
    Stopped,
    Playing,
    Paused,
}

impl State {
    pub fn word(self) -> &'static str {
        match self {
            State::Stopped => "stopped",
            State::Playing => "playing",
            State::Paused => "paused",
        }
    }
}

/// What plays: a list, an album or all tracks, as indices into the library it was picked from.
#[derive(Clone, Debug, Default)]
pub struct Queue {
    pub lib: Arc<Library>,
    pub kind: &'static str,
    pub name: String,
    pub tracks: Vec<usize>,
}

#[derive(Clone, Debug, Default)]
pub struct Status {
    pub state: State,
    pub queue: Option<Queue>,
    pub at: usize,
    pub position: f64,
    pub duration: Option<f64>,
    pub output: String,
    /// Where the music goes: `output` while a stream plays; idle the kernel's set port, else the last output
    /// played to, else `auto` (the start card's "über HDMI" before anything plays).
    pub route: String,
    /// What played last, also from before a restart.
    pub last: Last,
    /// The last error in German words (screen text), cleared by the next play.
    pub error: Option<String>,
}

pub enum Cmd {
    Play(Queue, usize),
    Pause,
    Resume,
    Next,
    Prev,
    Stop,
}

/// `prev` within this many seconds of a track's start goes to the track before, later it starts it again.
const PREV_RESTART: f64 = 3.0;
/// Paused or stopped: the status is published this often (the queued end of a paused track still plays out).
const IDLE_TICK: Duration = Duration::from_millis(100);
/// How long the end of a track may take to play out before the next one opens the device again.
const DRAIN_MAX: Duration = Duration::from_secs(1);

struct Current {
    dec: Decoding,
    /// The source rate and channels the sink was opened for (None until the first piece opened it).
    out: Option<(u32, u32)>,
    resampler: Option<Resampler>,
}

pub struct Player {
    sink: Box<dyn Sink>,
    shared: Arc<Mutex<Status>>,
    ev: Events,
    st: Status,
    cur: Option<Current>,
    output_read: Option<Instant>,
    last_path: Option<PathBuf>,
}

impl Player {
    pub fn new(
        sink: Box<dyn Sink>,
        shared: Arc<Mutex<Status>>,
        ev: Events,
        last_path: Option<PathBuf>,
    ) -> Player {
        let st = Status {
            last: last_path.as_deref().map(last::load).unwrap_or_default(),
            ..Status::default()
        };
        Player {
            sink,
            shared,
            ev,
            st,
            cur: None,
            output_read: None,
            last_path,
        }
    }

    fn remember(&mut self) {
        let Some(p) = &self.last_path else { return };
        if let Err(e) = last::save(p, &self.st.last) {
            self.ev
                .emit(Level::Warn, "last.save_failed", &format!("error={}", q(&e)));
        }
    }

    pub fn run(mut self, rx: Receiver<Cmd>) {
        loop {
            let cmd = if self.st.state == State::Playing {
                match rx.try_recv() {
                    Ok(c) => Some(c),
                    Err(TryRecvError::Empty) => None,
                    Err(TryRecvError::Disconnected) => return,
                }
            } else {
                match rx.recv_timeout(IDLE_TICK) {
                    Ok(c) => Some(c),
                    Err(RecvTimeoutError::Timeout) => None,
                    Err(RecvTimeoutError::Disconnected) => return,
                }
            };
            if let Some(c) = cmd {
                self.command(c);
            }
            if self.st.state == State::Playing {
                self.step();
            }
            self.publish();
        }
    }

    fn title(&self) -> String {
        self.st
            .queue
            .as_ref()
            .and_then(|q| {
                q.tracks
                    .get(self.st.at)
                    .map(|&i| q.lib.tracks[i].title.clone())
            })
            .unwrap_or_default()
    }

    fn command(&mut self, c: Cmd) {
        match c {
            Cmd::Play(queue, at) => {
                self.ev.emit(
                    Level::Info,
                    "play.queue",
                    &format!(
                        "kind={} name={} tracks={} index={at}",
                        queue.kind,
                        q(&queue.name),
                        queue.tracks.len()
                    ),
                );
                self.st.last.kind = queue.kind.to_string();
                self.st.last.name = queue.name.clone();
                self.remember();
                self.st.queue = Some(queue);
                self.st.error = None;
                self.start(at);
            }
            Cmd::Pause if self.st.state == State::Playing => {
                self.st.state = State::Paused;
                self.ev.emit(
                    Level::Info,
                    "play.pause",
                    &format!(
                        "title={} position={:.1}",
                        q(&self.title()),
                        self.st.position
                    ),
                );
            }
            Cmd::Resume if self.st.state == State::Paused => {
                self.st.state = State::Playing;
                self.ev.emit(
                    Level::Info,
                    "play.resume",
                    &format!("title={}", q(&self.title())),
                );
            }
            Cmd::Next if self.st.queue.is_some() => {
                self.sink.drop_queued();
                self.start(self.st.at + 1);
            }
            Cmd::Prev if self.st.queue.is_some() => {
                self.sink.drop_queued();
                let at = if self.st.position > PREV_RESTART {
                    self.st.at
                } else {
                    self.st.at.saturating_sub(1)
                };
                self.start(at);
            }
            Cmd::Stop => self.stop("stop"),
            _ => {}
        }
    }

    fn stop(&mut self, why: &str) {
        if self.st.state != State::Stopped {
            self.ev.emit(
                Level::Info,
                "play.stop",
                &format!("why={why} title={}", q(&self.title())),
            );
        }
        self.cur = None;
        self.sink.close();
        self.st.state = State::Stopped;
        self.st.position = 0.0;
        self.st.duration = None;
    }

    /// Opens track `at` of the queue, or the next one that opens; stops after the last.
    fn start(&mut self, mut at: usize) {
        self.cur = None;
        let Some(qu) = self.st.queue.clone() else {
            return;
        };
        loop {
            let Some(&i) = qu.tracks.get(at) else {
                self.st.at = at.min(qu.tracks.len().saturating_sub(1));
                self.stop("end");
                return;
            };
            let t = &qu.lib.tracks[i];
            match Decoding::open(&t.path) {
                Ok(dec) => {
                    self.ev.emit(
                        Level::Info,
                        "play.start",
                        &format!(
                            "title={} artist={} index={at} file={}",
                            q(&t.title),
                            q(&t.artist),
                            q(&t.path.to_string_lossy())
                        ),
                    );
                    self.st.at = at;
                    self.st.position = 0.0;
                    self.st.duration = dec.duration;
                    self.st.state = State::Playing;
                    self.cur = Some(Current {
                        dec,
                        out: None,
                        resampler: None,
                    });
                    return;
                }
                Err(e) => {
                    self.ev.emit(
                        Level::Warn,
                        "play.skip",
                        &format!("file={} error={}", q(&t.path.to_string_lossy()), q(&e)),
                    );
                    self.st.error = Some(format!("{} lässt sich nicht abspielen", t.title));
                    at += 1;
                }
            }
        }
    }

    /// One decoded piece to the sink; the end of a track opens the next one.
    fn step(&mut self) {
        let Some(cur) = self.cur.as_mut() else {
            self.stop("nothing open");
            return;
        };
        let chunk = match cur.dec.next() {
            Ok(Some(c)) => c,
            Ok(None) => {
                let bad = cur.dec.bad_packets;
                if bad > 0 {
                    self.ev.emit(
                        Level::Warn,
                        "play.bad_packets",
                        &format!("title={} packets={bad}", q(&self.title())),
                    );
                }
                self.sink.drain(DRAIN_MAX);
                self.start(self.st.at + 1);
                return;
            }
            Err(e) => {
                self.ev.emit(
                    Level::Warn,
                    "play.decode_failed",
                    &format!("title={} error={}", q(&self.title()), q(&e)),
                );
                self.st.error = Some(format!("{} bricht ab", self.title()));
                self.sink.drop_queued();
                self.start(self.st.at + 1);
                return;
            }
        };
        if cur.out != Some((chunk.rate, chunk.channels)) {
            match self.sink.open(chunk.rate, chunk.channels) {
                Ok(r) => {
                    if r != chunk.rate {
                        self.ev.emit(
                            Level::Info,
                            "pcm.resample",
                            &format!("from={} to={r}", chunk.rate),
                        );
                        cur.resampler =
                            Some(Resampler::new(chunk.rate, r, chunk.channels as usize));
                    } else {
                        cur.resampler = None;
                    }
                    cur.out = Some((chunk.rate, chunk.channels));
                    self.ev.emit(
                        Level::Info,
                        "pcm.open",
                        &format!(
                            "rate={r} channels={} output={}",
                            chunk.channels,
                            self.sink.output()
                        ),
                    );
                }
                Err(e) => {
                    self.ev
                        .emit(Level::Error, "pcm.open_failed", &format!("error={}", q(&e)));
                    self.st.error = Some("Die Tonausgabe lässt sich nicht öffnen".into());
                    self.stop("output");
                    return;
                }
            }
        }
        let samples = match cur.resampler.as_mut() {
            Some(r) => {
                let mut v = Vec::with_capacity(chunk.samples.len() * 2);
                r.run(&chunk.samples, &mut v);
                v
            }
            None => chunk.samples,
        };
        if let Err(e) = self.sink.write(&samples) {
            self.ev.emit(
                Level::Error,
                "pcm.write_failed",
                &format!("error={}", q(&e)),
            );
            self.st.error = Some("Die Tonausgabe antwortet nicht".into());
            self.stop("output");
            return;
        }
    }

    fn publish(&mut self) {
        if let Some(Current { out: Some(_), .. }) = &self.cur {
            let rate = self.sink_rate();
            self.st.position = self.sink.played() as f64 / rate as f64;
        }
        if self
            .output_read
            .is_none_or(|t| t.elapsed() > Duration::from_secs(1))
        {
            self.st.output = self.sink.output();
            self.output_read = Some(Instant::now());
            let playing = self.st.state == State::Playing
                && !matches!(self.st.output.as_str(), "" | "unknown");
            if playing && self.st.last.output != self.st.output {
                self.st.last.output = self.st.output.clone();
                self.remember();
            }
            let target = self.sink.target();
            self.st.route = if playing {
                self.st.output.clone()
            } else if target != "auto" || self.st.last.output.is_empty() {
                target
            } else {
                self.st.last.output.clone()
            };
        }
        if let Ok(mut s) = self.shared.lock() {
            *s = self.st.clone();
        }
    }

    fn sink_rate(&self) -> u32 {
        match &self.cur {
            Some(Current {
                resampler: Some(r), ..
            }) => r.to_rate(),
            Some(Current {
                out: Some((r, _)), ..
            }) => *r,
            _ => 48_000,
        }
    }
}
