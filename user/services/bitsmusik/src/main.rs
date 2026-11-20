//! `bitsmusik serve [--dir DIR]... [--socket PATH] [--sink alsa|null] [--last PATH] [--stale-secs N]`: the
//! service (started as user 1000 by the desktop's session). `--sink null` plays into a clock-paced nothing (host
//! tests: no /dev/snd); `--last` the memory of what played last (default under /data/home/<uid>/.local/share/
//! bitsos); `--stale-secs` how old a scan may be before `library` starts a new one (default 60).
//! `bitsmusik ctl [--socket PATH] REQUEST`: sends one request line (a bare op word or a JSON object) and prints
//! the answer; exit 0 when it says "ok":true.

use bitsmusik::events::{Events, Level};
use bitsmusik::server::{self, Service};
use bitsmusik::sink::{Null, Sink};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

const USAGE: &str = "usage: bitsmusik serve [--dir DIR]... [--socket PATH] [--sink alsa|null] [--last PATH] [--stale-secs N]\n       bitsmusik ctl [--socket PATH] REQUEST";

fn uid() -> u32 {
    usys::getuid().map(|u| u as u32).unwrap_or(1000)
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (cmd, rest) = match args.split_first() {
        Some((c, r)) => (c.as_str(), r),
        None => {
            eprintln!("{USAGE}");
            return ExitCode::from(2);
        }
    };
    let mut dirs = Vec::new();
    let mut socket = None;
    let mut sink = "alsa".to_string();
    let mut words = Vec::new();
    let mut opts = server::Opts {
        last: Some(bitsmusik::last::default_path(uid())),
        ..server::Opts::default()
    };
    let mut it = rest.iter();
    while let Some(a) = it.next() {
        match (a.as_str(), cmd) {
            ("--dir", "serve") => dirs.extend(it.next().map(PathBuf::from)),
            ("--sink", "serve") => sink = it.next().cloned().unwrap_or_default(),
            ("--socket", _) => socket = it.next().map(PathBuf::from),
            ("--last", "serve") => opts.last = it.next().map(PathBuf::from),
            ("--stale-secs", "serve") => match it.next().and_then(|n| n.parse().ok()) {
                Some(n) => opts.stale = Duration::from_secs(n),
                None => {
                    eprintln!("{USAGE}");
                    return ExitCode::from(2);
                }
            },
            (w, "ctl") => words.push(w.to_string()),
            _ => {
                eprintln!("{USAGE}");
                return ExitCode::from(2);
            }
        }
    }
    let socket = socket.unwrap_or_else(|| server::default_socket(uid()));
    match cmd {
        "serve" => serve(dirs, &socket, &sink, opts),
        "ctl" if !words.is_empty() => ctl(&socket, &words.join(" ")),
        _ => {
            eprintln!("{USAGE}");
            ExitCode::from(2)
        }
    }
}

fn serve(dirs: Vec<PathBuf>, socket: &std::path::Path, sink: &str, opts: server::Opts) -> ExitCode {
    let ev = Events::system();
    let roots = if dirs.is_empty() {
        server::default_roots(uid())
    } else {
        dirs
    };
    let sink: Box<dyn Sink> = match sink {
        "alsa" => Box::new(bitsmusik::pcm::Alsa::new(ev.clone())),
        "null" => Box::new(Null::new()),
        _ => {
            eprintln!("{USAGE}");
            return ExitCode::from(2);
        }
    };
    let s = Service::start(roots, sink, ev.clone(), opts);
    match s.serve(socket) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            ev.emit(Level::Error, "serve.failed", &bitsmusik::events::q(&e));
            ExitCode::from(1)
        }
    }
}

fn ctl(socket: &std::path::Path, line: &str) -> ExitCode {
    let c = match UnixStream::connect(socket) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("bitsmusik: {}: {e}", socket.display());
            return ExitCode::from(1);
        }
    };
    let _ = c.set_read_timeout(Some(Duration::from_secs(5)));
    let _ = c.set_write_timeout(Some(Duration::from_secs(5)));
    let mut w = match c.try_clone() {
        Ok(w) => w,
        Err(e) => {
            eprintln!("bitsmusik: {e}");
            return ExitCode::from(1);
        }
    };
    if writeln!(w, "{line}").is_err() {
        eprintln!("bitsmusik: the request did not go out");
        return ExitCode::from(1);
    }
    let mut answer = String::new();
    match BufReader::new(c).read_line(&mut answer) {
        Ok(n) if n > 0 => {
            print!("{answer}");
            if answer.contains("\"ok\":true") {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(1)
            }
        }
        _ => {
            eprintln!("bitsmusik: no answer within 5 s");
            ExitCode::from(1)
        }
    }
}
